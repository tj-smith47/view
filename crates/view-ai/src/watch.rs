//! Out-of-band write watcher: catches a write that never went through ACP
//! at all -- an agent's own shell tool is the case no MCP/ACP client can
//! see or route -- and turns it into `Msg::ExternalWritesDetected` so
//! `update/watch.rs` (in `view-core`) can drive nvim's own `checktime`
//! path for it. Never reads a byte of file content: nvim is the sole
//! owner of buffer text (see this crate's root doc), so this module's
//! whole job stops at "something changed at this path."
//!
//! No self-write suppression happens here. Every candidate write this
//! module sees -- the watcher's own probe, a hidden-buffer `:w`, an
//! agent's shell write, the user's own save -- is forwarded unfiltered;
//! `docs/checktime-wire-capture.md` cases 4 and 5 proved nvim's own
//! `FileChangedShell`-fired flag already tells a genuine external change
//! apart from a write view itself just issued, so a second, independent
//! filter here would only risk disagreeing with the answer nvim already
//! computes correctly.
//!
//! Two costs shape everything below, both paid on nvim's single-threaded
//! main loop rather than here:
//!
//! - **What is registered.** A recursive registration over a project root
//!   costs one platform watch descriptor per directory and walks the whole
//!   tree to place them. On this repository that is 30k directories, 26k of
//!   them build output nothing will ever have a buffer for, and the walk
//!   takes seconds. [`register`] therefore walks with the project's own
//!   ignore rules plus [`NEVER_WATCHED`], and does it on the watch's own
//!   thread so no caller ever waits for it.
//! - **How often nvim is asked.** Every detection costs one
//!   `nvim_exec_lua` that resolves the loaded-buffer set. [`pump`]
//!   therefore batches: paths seen inside one [`COALESCE_WINDOW`] leave as
//!   a single `Msg`, which becomes a single chunk execution answering all
//!   of them (`docs/checktime-wire-capture.md` case 8).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify::{Event, RecursiveMode, Watcher};
use view_core::msg::Msg;

/// Paths seen closer together than this leave as one batched
/// `Msg::ExternalWritesDetected`. Atomic-save tooling (temp file plus
/// rename) and a single `write(2)` both raise several raw events for what a
/// user experiences as one save, and an agent's shell tool touching a tree
/// raises one per file; a checktime round trip per raw event would spend
/// nvim's main loop on a buffer scan per event, while nvim's own
/// mtime-based disposition is idempotent against redundant probes anyway.
///
/// The window is a deferral, never a filter: every path seen inside it is
/// carried into the batch that closes it, so a second write to the same
/// path during the window still reaches nvim afterward rather than being
/// dropped for looking like the first one.
const COALESCE_WINDOW: Duration = Duration::from_millis(50);

/// The most paths one batch carries. Whatever a window collects beyond this
/// stays pending for the next one, so a single chunk execution on nvim's
/// main loop is bounded no matter how many files a `git checkout` or a
/// formatter sweep rewrites at once.
const MAX_BATCH: usize = 256;

/// The most paths held pending at once. A burst larger than this is
/// reported (see [`Msg::ExternalWatchDegraded`]) rather than silently
/// buffered without limit -- the watch runs for a whole session, so an
/// unbounded queue is a leak with a slow fuse.
const MAX_PENDING: usize = 65_536;

/// Directory names never watched, at any depth, whatever the project's own
/// ignore rules say. `.git` churns constantly during ordinary agent
/// activity (index locks, packed-refs, FETCH_HEAD) and virtually never
/// names a loaded buffer; the other three are build and dependency output
/// with the same property, and are the bulk of the directories a project
/// root contains. Matching at any depth rather than only at the root is
/// deliberate: a submodule's or nested worktree's `.git`, and a nested
/// `node_modules`, produce exactly the storm this list exists to prevent.
const NEVER_WATCHED: [&str; 4] = [".git", "target", "node_modules", ".venv"];

/// Everything [`spawn`] can fail with.
///
/// Registering the tree is deliberately absent: it happens on the watch's
/// own thread, after this returns, and reports through
/// [`Msg::ExternalWatchDegraded`] instead (see [`register`]).
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    /// The platform watcher backend (inotify/FSEvents/ReadDirectoryChangesW)
    /// could not be created.
    #[error("could not watch {root} for external writes: {source}")]
    Backend {
        /// The project root the watch was attempted against.
        root: PathBuf,
        /// The underlying platform-watcher error.
        source: notify::Error,
    },
    /// The pump thread that registers the tree and drains the watcher's own
    /// event channel could not be started.
    #[error("could not start the out-of-band write watcher's own thread: {0}")]
    ThreadSpawn(std::io::Error),
}

/// The one real platform watcher, shared by every [`WatchHandle`] clone and
/// by the watch's own thread (which keeps registering directories into it
/// as they appear). `None` once [`WatchHandle::stop`] has taken it out.
///
/// Boxed behind `notify`'s own trait rather than held as the concrete
/// `RecommendedWatcher`: a backend that refuses one directory and a backend
/// that has hit the host's watch limit take [`register`] down two very
/// different paths, and neither can be provoked from outside without
/// either racing the walk or changing a global sysctl on the machine the
/// test runs on. One indirection per directory, paid once at registration,
/// buys both branches a real test.
type WatcherSlot = Arc<Mutex<Option<Box<dyn Watcher + Send>>>>;

/// Set once the initial registration walk has finished, so a caller that
/// needs to know detection is actually live can wait for it instead of
/// guessing at a duration (see [`WatchHandle::wait_until_watching`]).
#[cfg(any(test, feature = "test-support"))]
type Ready = Arc<(Mutex<bool>, std::sync::Condvar)>;

/// A running out-of-band write watch over one project root.
///
/// Cloneable and cheap: every clone shares the same underlying platform
/// watcher, so an explicit [`Self::stop`] call on any one of them tears
/// down the one real subscription for all of them -- the same
/// shared-teardown shape `AiWorker`'s own clone already has for its
/// session slot. No custom `Drop` impl: once the last clone's `Arc` goes
/// away, the `Mutex<Option<_>>` it owns drops along with
/// it, which drops the watcher inside on exactly the same
/// terms `stop` unregisters it on -- the cascade already does the right
/// thing without repeating that logic in a second place.
#[derive(Clone)]
pub struct WatchHandle {
    watcher: WatcherSlot,
    #[cfg(any(test, feature = "test-support"))]
    ready: Ready,
}

impl WatchHandle {
    /// Stops the watch. Idempotent: a second call, or a call after every
    /// clone has already been dropped, is a no-op.
    ///
    /// Taking the watcher out closes the channel the watch's own thread
    /// blocks on, which is that thread's exit signal -- so no separate
    /// shutdown flag exists. A registration walk still in progress sees the
    /// empty slot on its next directory and stops there, which is why it
    /// takes the lock per directory rather than for the whole walk.
    pub fn stop(&self) {
        let mut guard = self.watcher.lock().unwrap_or_else(|e| e.into_inner());
        *guard = None;
    }

    /// Blocks until the initial registration walk has finished, or
    /// `timeout` elapses; answers whether it finished.
    ///
    /// Nothing in a running editor calls this -- [`spawn`] returns before
    /// the walk starts precisely so the launch path never waits on it, and
    /// a session's first write is seconds of agent handshake away. It
    /// exists for the tests that must observe a write the watch was
    /// already covering, which would otherwise be racing the walk and
    /// papering over it with a sleep.
    #[cfg(any(test, feature = "test-support"))]
    pub fn wait_until_watching(&self, timeout: Duration) -> bool {
        let (lock, cv) = &*self.ready;
        let mut done = lock.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = Instant::now() + timeout;
        while !*done {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, _) = cv
                .wait_timeout(done, remaining)
                .unwrap_or_else(|e| e.into_inner());
            done = next;
        }
        true
    }
}

/// Whether `path` lies under a directory name no watch ever covers (see
/// [`NEVER_WATCHED`]), or outside `root` altogether.
///
/// Outside-the-root counts as excluded: the watch exists for the trusted
/// project root and nothing else, and a path that does not resolve under it
/// is one this watch has no business reporting.
fn is_excluded(path: &Path, root: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return true;
    };
    rel.components()
        .any(|c| NEVER_WATCHED.iter().any(|name| c.as_os_str() == *name))
}

/// The message a degraded watch reports itself with. Never silent: after
/// either degradation the session goes on running with out-of-band
/// detection partly or wholly off, and `docs/ai.md` tells the user it is
/// on.
fn degraded(reason: String) -> Msg {
    Msg::ExternalWatchDegraded { reason }
}

/// Registers every directory at or under `start` that survives
/// [`is_excluded`] and the project's own ignore rules (`.gitignore`,
/// `.git/info/exclude`, and the global ignore file, exactly as the picker's
/// Files source honours them).
///
/// Directories are registered one at a time and non-recursively, rather
/// than handing `root` to the backend with `RecursiveMode::Recursive`: the
/// recursive mode places a descriptor per directory with no filter hook at
/// all, which on a Rust project means tens of thousands of descriptors on
/// build output that can never name a loaded buffer -- and on a host with
/// the common `max_user_watches = 8192` it means the whole watch failing.
///
/// `found_files`, when given, collects every file the walk passed. The
/// initial registration passes `None` (a project's whole file list is not a
/// list of writes); a directory that appears later passes `Some`, so files
/// created inside it before its own registration landed are still probed
/// rather than missed.
///
/// Answers how many directories were registered -- the platform descriptors
/// this walk actually cost, which is the number the filtering above exists
/// to hold down.
fn register(
    slot: &WatcherSlot,
    root: &Path,
    start: &Path,
    emit: &dyn Fn(Msg),
    mut found_files: Option<&mut BTreeSet<PathBuf>>,
) -> usize {
    let root_owned = root.to_path_buf();
    let mut builder = ignore::WalkBuilder::new(start);
    builder
        // dotfiles are ordinary editable project files (`.github/`,
        // `.cargo/config.toml`); `.git` itself is excluded by name below,
        // which is the only hidden directory this watch has a reason to skip
        .hidden(false)
        .follow_links(false)
        // honour the project's ignore rules even when the root is not a git
        // checkout: a vendored tree or a worktree export still carries a
        // `.gitignore` describing exactly the output no buffer will name
        .require_git(false)
        .filter_entry(move |entry| !is_excluded(entry.path(), &root_owned));
    // each degradation class says its piece once per registration -- a tree
    // whose permissions refuse a whole subdirectory would otherwise repeat
    // the same notice for every entry under it -- and the two are counted
    // apart so the quieter one can never silence the other
    let mut walk_error_reported = false;
    let mut watch_error_reported = false;
    let mut registered = 0;
    for entry in builder.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                if !walk_error_reported {
                    walk_error_reported = true;
                    emit(degraded(format!(
                        "part of {} could not be listed ({err}); \
                         writes under it will not be noticed",
                        start.display()
                    )));
                }
                continue;
            }
        };
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        if !is_dir {
            if let Some(files) = found_files.as_deref_mut() {
                files.insert(entry.path().to_path_buf());
            }
            continue;
        }
        let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
        let Some(watcher) = guard.as_mut() else {
            return registered;
        };
        let Err(err) = watcher.watch(entry.path(), RecursiveMode::NonRecursive) else {
            registered += 1;
            continue;
        };
        drop(guard);
        // the limit is the host's, not this directory's, so every later
        // registration would fail identically: stopping is the honest
        // answer, and the notice has to name the whole remainder rather
        // than the one path that happened to hit it first
        if matches!(err.kind, notify::ErrorKind::MaxFilesWatch) {
            emit(degraded(format!(
                "the platform's watch limit was reached while registering {} \
                 (raise fs.inotify.max_user_watches); writes under it, and under \
                 everything not yet registered, will not be noticed",
                entry.path().display()
            )));
            return registered;
        }
        // one directory refusing says nothing about the rest of the tree --
        // a directory vanishing mid-walk is ordinary under a root an agent
        // runs builds in, which is this whole feature's premise -- so the
        // walk goes on and the coverage lost is that one directory
        if !watch_error_reported {
            watch_error_reported = true;
            emit(degraded(format!(
                "{} could not be watched ({err}); writes under it will not be noticed",
                entry.path().display()
            )));
        }
    }
    registered
}

/// Drains `rx` until the watcher side closes it (see [`WatchHandle::stop`]),
/// batching paths into one [`Msg::ExternalWritesDetected`] per
/// [`COALESCE_WINDOW`]. Runs entirely on its own thread: `emit` reaches the
/// loop channel exactly the way `AiSession::spawn`'s own `emit` does, and
/// carries the same never-block requirement, since this thread has nothing
/// else to do but keep draining `rx`.
///
/// A directory that appears while the watch runs is registered here rather
/// than left uncovered, and the files already inside it join the same batch
/// -- a `git checkout` that recreates a directory holding an open file
/// would otherwise land entirely inside the window between the directory's
/// creation and its registration.
fn pump(
    rx: &std_mpsc::Receiver<notify::Result<Event>>,
    slot: &WatcherSlot,
    root: &Path,
    emit: &dyn Fn(Msg),
) {
    let mut pending: BTreeSet<PathBuf> = BTreeSet::new();
    let mut window_closes: Option<Instant> = None;
    let mut backend_error_reported = false;
    let mut overflow_reported = false;
    loop {
        let received = match window_closes {
            Some(closes) => {
                let now = Instant::now();
                if closes <= now {
                    flush(&mut pending, &mut window_closes, emit);
                    continue;
                }
                rx.recv_timeout(closes - now)
            }
            None => rx
                .recv()
                .map_err(|_| std_mpsc::RecvTimeoutError::Disconnected),
        };
        let result = match received {
            Ok(result) => result,
            // the window closed with nothing new arriving: emit what it
            // collected rather than waiting for a next event that may never
            // come, which is what makes the window a deferral and not a
            // filter
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                flush(&mut pending, &mut window_closes, emit);
                continue;
            }
            // the watcher was dropped (`stop`, or the last handle clone
            // going away): the session this watch belonged to is over, so a
            // final flush would raise conflict prompts for a session that no
            // longer exists
            Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
        };
        let event = match result {
            Ok(event) => event,
            // a backend error is not "no event": inotify reports queue
            // overflow this way, and an overflow means events were LOST --
            // the one degradation the user must not discover by finding a
            // stale buffer
            Err(err) => {
                if !backend_error_reported {
                    backend_error_reported = true;
                    emit(degraded(format!(
                        "the filesystem watcher reported an error ({err}); \
                         some external writes may go unnoticed"
                    )));
                }
                continue;
            }
        };
        if !(event.kind.is_create() || event.kind.is_modify()) {
            continue;
        }
        let is_create = event.kind.is_create();
        for path in event.paths {
            if is_excluded(&path, root) {
                continue;
            }
            if is_create && path.is_dir() {
                let _ = register(slot, root, &path, emit, Some(&mut pending));
                continue;
            }
            pending.insert(path);
        }
        if pending.len() > MAX_PENDING {
            if !overflow_reported {
                overflow_reported = true;
                emit(degraded(format!(
                    "more than {MAX_PENDING} external writes are waiting to be checked; \
                     the ones past that are not being tracked"
                )));
            }
            while pending.len() > MAX_PENDING {
                pending.pop_last();
            }
        }
        if window_closes.is_none() {
            window_closes = Some(Instant::now() + COALESCE_WINDOW);
        }
    }
}

/// Emits up to [`MAX_BATCH`] pending paths as one message. Whatever is left
/// over opens a fresh window rather than riding along, which is what bounds
/// a single chunk execution on nvim's main loop.
fn flush(pending: &mut BTreeSet<PathBuf>, window_closes: &mut Option<Instant>, emit: &dyn Fn(Msg)) {
    let mut paths: Vec<PathBuf> = Vec::with_capacity(pending.len().min(MAX_BATCH));
    while paths.len() < MAX_BATCH {
        let Some(path) = pending.pop_first() else {
            break;
        };
        paths.push(path);
    }
    *window_closes = if pending.is_empty() {
        None
    } else {
        Some(Instant::now() + COALESCE_WINDOW)
    };
    if !paths.is_empty() {
        emit(Msg::ExternalWritesDetected { paths });
    }
}

/// Starts watching `root` for out-of-band writes, calling `emit` once per
/// batch of detected write paths (already coalesced and filtered, see
/// [`pump`]'s own doc). `emit` must not block, on the same terms every
/// other cross-crate `Msg` emitter in this crate carries (see
/// `AiSession::spawn`'s own doc): it runs on this watch's own thread, which
/// has nothing else to do but keep draining the platform watcher's event
/// channel.
///
/// Returns as soon as the backend and the thread exist -- registering the
/// tree happens on that thread (see [`register`]), so this never sits on
/// the caller's own path. A caller that dispatches a user-visible action
/// right after this call therefore pays microseconds for it, not the
/// seconds a full-tree registration takes.
///
/// # Errors
///
/// [`WatchError::Backend`] if the platform watcher cannot be created at
/// all. [`WatchError::ThreadSpawn`] if the OS refuses the watch's thread.
/// Everything that can go wrong afterward -- a watch limit reached part way
/// through registration, a backend error losing events -- is reported
/// through [`Msg::ExternalWatchDegraded`] on `emit` instead, because by then
/// there is no call left to fail.
pub fn spawn(root: &Path, emit: impl Fn(Msg) + Send + 'static) -> Result<WatchHandle, WatchError> {
    let (tx, rx) = std_mpsc::channel::<notify::Result<Event>>();
    let watcher = notify::recommended_watcher(move |result| {
        let _ = tx.send(result);
    })
    .map_err(|source| WatchError::Backend {
        root: root.to_path_buf(),
        source,
    })?;

    // the backend reports realpath'd event paths, so every comparison this
    // module makes (`is_excluded`, and the walk's own filter) has to be
    // against the realpath'd root or a symlinked project root would leave
    // every event looking like it fell outside the tree
    let root_owned = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let slot: WatcherSlot = Arc::new(Mutex::new(Some(Box::new(watcher))));
    #[cfg(any(test, feature = "test-support"))]
    let ready: Ready = Ready::default();
    let thread_slot = Arc::clone(&slot);
    #[cfg(any(test, feature = "test-support"))]
    let thread_ready = Arc::clone(&ready);
    std::thread::Builder::new()
        .name("view-ai-watch".to_string())
        .spawn(move || {
            let emit: &dyn Fn(Msg) = &emit;
            let _ = register(&thread_slot, &root_owned, &root_owned, emit, None);
            #[cfg(any(test, feature = "test-support"))]
            {
                let (lock, cv) = &*thread_ready;
                *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
                cv.notify_all();
            }
            pump(&rx, &thread_slot, &root_owned, emit);
        })
        .map_err(WatchError::ThreadSpawn)?;

    Ok(WatchHandle {
        watcher: slot,
        #[cfg(any(test, feature = "test-support"))]
        ready,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::mpsc::{channel, RecvTimeoutError};

    /// A backend that answers by call index rather than by path, so
    /// [`register`]'s two failure paths can be driven without racing a real
    /// walk or lowering the host's own `max_user_watches`. Index-based on
    /// purpose: directory order inside a walk is the filesystem's business,
    /// so a test that named paths would be asserting on readdir order.
    struct FakeWatcher {
        calls: usize,
        /// The call index that answers with an ordinary refusal (the shape a
        /// directory vanishing mid-walk has). `usize::MAX` for none.
        refuse_at: usize,
        /// The first call index that answers with the host's watch limit.
        limit_at: usize,
        watched: Arc<Mutex<Vec<PathBuf>>>,
    }

    impl notify::Watcher for FakeWatcher {
        fn new<F: notify::EventHandler>(_: F, _: notify::Config) -> notify::Result<Self> {
            Err(notify::Error::generic(
                "this double is constructed directly",
            ))
        }

        fn watch(&mut self, path: &Path, _: RecursiveMode) -> notify::Result<()> {
            let call = self.calls;
            self.calls += 1;
            if call >= self.limit_at {
                return Err(notify::Error::new(notify::ErrorKind::MaxFilesWatch));
            }
            if call == self.refuse_at {
                return Err(notify::Error::generic("no such directory"));
            }
            self.watched
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(path.to_path_buf());
            Ok(())
        }

        fn unwatch(&mut self, _: &Path) -> notify::Result<()> {
            Ok(())
        }

        fn kind() -> notify::WatcherKind {
            notify::WatcherKind::NullWatcher
        }
    }

    /// Builds a root with `root`, `a`, `b`, `c` as its four directories and
    /// a slot holding a [`FakeWatcher`] with the given answers.
    fn fake_backend(
        refuse_at: usize,
        limit_at: usize,
    ) -> (PathBuf, WatcherSlot, Arc<Mutex<Vec<PathBuf>>>) {
        let root = tempdir();
        for dir in ["a", "b", "c"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let watched = Arc::new(Mutex::new(Vec::new()));
        let watcher = FakeWatcher {
            calls: 0,
            refuse_at,
            limit_at,
            watched: Arc::clone(&watched),
        };
        let slot: WatcherSlot = Arc::new(Mutex::new(Some(Box::new(watcher))));
        (root, slot, watched)
    }

    /// One directory the backend refuses costs that directory's coverage and
    /// nothing else. A directory vanishing part way through a walk is
    /// ordinary under a root an agent runs builds in -- the case this whole
    /// feature exists for -- so abandoning every directory the walk had not
    /// yet reached would silently gut coverage of the tree while reporting
    /// only the one path that failed.
    #[test]
    fn one_refused_directory_does_not_truncate_the_rest_of_the_walk() {
        let (root, slot, watched) = fake_backend(1, usize::MAX);
        let (msg_tx, msg_rx) = channel::<Msg>();
        let emit = move |msg: Msg| {
            let _ = msg_tx.send(msg);
        };

        let registered = register(&slot, &root, &root, &emit, None);

        assert_eq!(registered, 3, "the walk stopped at the refusal");
        assert_eq!(
            watched.lock().unwrap().len(),
            3,
            "three of the four directories must still be watched"
        );
        match msg_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Msg::ExternalWatchDegraded { reason }) => {
                assert!(reason.contains("will not be noticed"), "got {reason:?}")
            }
            other => panic!("a refusal must report itself, got {other:?}"),
        }
        assert!(
            matches!(
                msg_rx.recv_timeout(Duration::from_millis(100)),
                Err(RecvTimeoutError::Timeout)
            ),
            "one refusal must not become a storm of notices"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The host's watch limit is the one refusal that does stop the walk --
    /// every later registration would fail identically -- so its notice has
    /// to name the whole remainder rather than the one path that hit it
    /// first, and it can never be suppressed by a degradation reported
    /// earlier: the loudest failure must not be silenced by the quietest.
    /// The string asserted here is the one `docs/ai.md` quotes to the user.
    #[test]
    fn the_watch_limit_stops_the_walk_and_still_says_so_after_another_refusal() {
        let (root, slot, watched) = fake_backend(0, 1);
        let (msg_tx, msg_rx) = channel::<Msg>();
        let emit = move |msg: Msg| {
            let _ = msg_tx.send(msg);
        };

        let registered = register(&slot, &root, &root, &emit, None);

        assert_eq!(registered, 0);
        assert!(watched.lock().unwrap().is_empty());
        let mut reasons = Vec::new();
        while let Ok(Msg::ExternalWatchDegraded { reason }) =
            msg_rx.recv_timeout(Duration::from_millis(500))
        {
            reasons.push(reason);
        }
        assert_eq!(reasons.len(), 2, "got {reasons:?}");
        let limit = &reasons[1];
        assert!(
            limit.contains("fs.inotify.max_user_watches"),
            "the limit notice must name the knob that fixes it, got {limit:?}"
        );
        assert!(
            limit.contains("everything not yet registered"),
            "the limit notice must name the coverage it gave up, got {limit:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A tempdir this crate's own `view-test-support` fixture would
    /// normally supply, but that crate is a dev-only leaf with no reach
    /// into `view-ai` (`scripts/audit-deps.sh`'s `check_dev_only` row) --
    /// so a real filesystem event needs a real directory here instead.
    fn tempdir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "view-ai-watch-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        dir.push(unique);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    /// The walk registers one platform descriptor per directory, so what it
    /// skips is what keeps starting a session off the host's watch limit --
    /// and off seconds of walking build output. Counted directly from
    /// [`register`]'s own answer rather than inferred from what the pump
    /// forwards: the pump filters excluded paths a second time, so a walk
    /// that had stopped filtering entirely would still look correct from
    /// the outside while costing every descriptor it was meant to save.
    #[test]
    fn the_registration_walk_skips_build_output_and_ignored_trees() {
        let root = tempdir();
        std::fs::write(root.join(".gitignore"), b"generated/\n").unwrap();
        for dir in [
            "src/inner",
            "target/debug/deps",
            "node_modules/pkg/dist",
            ".venv/lib",
            "sub/.git/objects",
            "generated/out",
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let (tx, rx) = channel::<notify::Result<Event>>();
        let watcher = notify::recommended_watcher(move |result| {
            let _ = tx.send(result);
        })
        .expect("backend must be creatable");
        let slot: WatcherSlot = Arc::new(Mutex::new(Some(Box::new(watcher))));
        let emit = |msg: Msg| panic!("a clean walk must report nothing, got {msg:?}");

        let registered = register(&slot, &root, &root, &emit, None);

        // root, src, src/inner, sub -- and nothing under target,
        // node_modules, .venv, sub/.git, or the gitignored generated/
        assert_eq!(
            registered, 4,
            "the walk registered directories it must skip"
        );

        drop(rx);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// One window's batch is bounded no matter how much the window
    /// collected, which is what keeps a `git checkout` of a large tree from
    /// handing nvim's main loop one buffer scan over thousands of paths.
    /// Driven against [`flush`] directly: making a real watcher observe
    /// more than [`MAX_BATCH`] writes inside one window would be a race
    /// dressed up as a bound.
    #[test]
    fn one_batch_never_exceeds_the_bound() {
        let mut pending: BTreeSet<PathBuf> = (0..MAX_BATCH * 2 + 1)
            .map(|i| PathBuf::from(format!("/p/{i:06}.rs")))
            .collect();
        let mut window_closes = Some(Instant::now());
        let (tx, rx) = channel::<Msg>();
        let emit = move |msg: Msg| {
            let _ = tx.send(msg);
        };

        flush(&mut pending, &mut window_closes, &emit);

        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Msg::ExternalWritesDetected { paths }) => assert_eq!(paths.len(), MAX_BATCH),
            other => panic!("expected one bounded batch, got {other:?}"),
        }
        assert_eq!(pending.len(), MAX_BATCH + 1, "the rest must stay pending");
        assert!(
            window_closes.is_some(),
            "a leftover must open its own window rather than being dropped"
        );
    }

    /// The first batch naming `wanted`, or a panic once the deadline passes.
    fn wait_for_path(rx: &std_mpsc::Receiver<Msg>, wanted: &Path) -> Vec<PathBuf> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                remaining > Duration::ZERO,
                "no batch named {wanted:?} in 5s"
            );
            match rx.recv_timeout(remaining) {
                Ok(Msg::ExternalWritesDetected { paths }) => {
                    if paths.iter().any(|p| p == wanted) {
                        return paths;
                    }
                }
                Ok(other) => panic!("unexpected message {other:?}"),
                Err(err) => panic!("channel closed before {wanted:?} arrived: {err}"),
            }
        }
    }

    /// Writing a file under the watched root raises exactly one
    /// `Msg::ExternalWritesDetected` naming it -- the falsifiable core of
    /// [`spawn`]'s own contract. `recv_timeout`, never a bare `recv`: a
    /// regression that stops forwarding events must fail this test in a
    /// few seconds, not hang the suite.
    #[test]
    fn a_write_under_the_root_is_detected() {
        let root = tempdir();
        let (tx, rx) = channel::<Msg>();
        let handle = spawn(&root, move |msg| {
            let _ = tx.send(msg);
        })
        .expect("watch must start against a real, writable tempdir");
        assert!(handle.wait_until_watching(Duration::from_secs(5)));

        let target = root.join("touched.rs");
        std::fs::write(&target, b"hello").unwrap();

        let paths = wait_for_path(&rx, &target);
        assert!(paths.contains(&target), "got {paths:?}");

        handle.stop();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Two writes to the SAME path inside one coalesce window both reach
    /// nvim: the window defers the second one into the batch that closes
    /// it, never drops it. The mutation this kills is a leading-edge-only
    /// window (emit the first, `continue` past everything inside it), under
    /// which the second write's content is never probed at all and a user
    /// reads a stale buffer believing it is current.
    #[test]
    fn a_second_write_inside_the_window_is_still_probed() {
        let root = tempdir();
        let target = root.join("twice.rs");
        std::fs::write(&target, b"first").unwrap();
        let (tx, rx) = channel::<Msg>();
        let handle = spawn(&root, move |msg| {
            let _ = tx.send(msg);
        })
        .expect("watch must start");
        assert!(handle.wait_until_watching(Duration::from_secs(5)));

        // let the first write's own batch drain, so the second write below
        // is unambiguously a separate detection rather than the same one
        std::fs::write(&target, b"second").unwrap();
        let _ = wait_for_path(&rx, &target);
        std::fs::write(&target, b"third").unwrap();

        let paths = wait_for_path(&rx, &target);
        assert!(
            paths.contains(&target),
            "the second write must produce its own detection, got {paths:?}"
        );

        handle.stop();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Many paths written at once arrive as batches, never one message per
    /// path: each message costs nvim's main loop one buffer scan, so the
    /// count of messages -- not the count of writes -- is what the design
    /// bounds. Also the falsifiable half of "nothing is dropped": every
    /// path written appears in some batch.
    #[test]
    fn a_burst_of_writes_arrives_batched_and_complete() {
        let root = tempdir();
        let (tx, rx) = channel::<Msg>();
        let handle = spawn(&root, move |msg| {
            let _ = tx.send(msg);
        })
        .expect("watch must start");
        assert!(handle.wait_until_watching(Duration::from_secs(5)));

        let total = 200;
        let mut wanted: BTreeSet<PathBuf> = BTreeSet::new();
        for i in 0..total {
            let path = root.join(format!("file{i}.rs"));
            std::fs::write(&path, b"x").unwrap();
            wanted.insert(path);
        }

        let mut batches = 0;
        let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
        // timed from the first batch, not from the write loop: how long the
        // writer took says nothing about how long the backend took to
        // deliver, and a loaded host can spread delivery over far more wall
        // time than the writes themselves spanned. The pump's own windows
        // are what the bound below is about, so the span the pump saw is
        // what has to measure them.
        let mut delivery: Option<Instant> = None;
        let deadline = Instant::now() + Duration::from_secs(20);
        while !wanted.is_subset(&seen) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                remaining > Duration::ZERO,
                "only {} of {total} paths arrived in {batches} batches",
                seen.len()
            );
            match rx.recv_timeout(remaining) {
                Ok(Msg::ExternalWritesDetected { paths }) => {
                    batches += 1;
                    delivery.get_or_insert_with(Instant::now);
                    seen.extend(paths);
                }
                Ok(other) => panic!("unexpected message {other:?}"),
                Err(err) => panic!("channel closed with {} paths seen: {err}", seen.len()),
            }
        }
        // the bound the design actually promises: one probe per coalescing
        // window, not merely "fewer than one per write". A regression to
        // near-per-event probing would still satisfy `batches < total`.
        let spread = delivery.map(|first| first.elapsed()).unwrap_or_default();
        let windows = spread.as_millis() / COALESCE_WINDOW.as_millis() + 2;
        assert!(
            u128::try_from(batches).unwrap_or(u128::MAX) <= windows,
            "{total} writes delivered over {spread:?} cost {batches} probes of nvim's \
             main loop, more than the {windows} windows they crossed"
        );

        handle.stop();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A directory created while the watch runs is registered, and the file
    /// written inside it is detected -- the `git checkout` shape, where a
    /// directory holding an open file is removed and recreated.
    #[test]
    fn a_directory_created_after_the_walk_is_still_watched() {
        let root = tempdir();
        let (tx, rx) = channel::<Msg>();
        let handle = spawn(&root, move |msg| {
            let _ = tx.send(msg);
        })
        .expect("watch must start");
        assert!(handle.wait_until_watching(Duration::from_secs(5)));

        let dir = root.join("appeared");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("inside.rs");
        std::fs::write(&target, b"x").unwrap();

        let paths = wait_for_path(&rx, &target);
        assert!(paths.contains(&target), "got {paths:?}");

        handle.stop();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Build output and VCS internals are never registered, at any depth:
    /// the walk skips them, so the platform never places a descriptor on
    /// the 26k directories a Rust `target/` holds and a submodule's own
    /// `.git` never produces the round-trip storm the root's does. Proved
    /// by a real event that must NOT arrive, alongside one that must.
    #[test]
    fn excluded_directories_are_never_watched() {
        let root = tempdir();
        for dir in ["target", "node_modules", ".venv"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::create_dir_all(root.join("sub").join(".git")).unwrap();
        let (tx, rx) = channel::<Msg>();
        let handle = spawn(&root, move |msg| {
            let _ = tx.send(msg);
        })
        .expect("watch must start");
        assert!(handle.wait_until_watching(Duration::from_secs(5)));

        std::fs::write(root.join("target").join("built.rlib"), b"x").unwrap();
        std::fs::write(root.join("node_modules").join("pkg.js"), b"x").unwrap();
        std::fs::write(root.join(".venv").join("pyvenv.cfg"), b"x").unwrap();
        std::fs::write(root.join("sub").join(".git").join("index.lock"), b"x").unwrap();
        // a real, non-excluded write proves the watch is alive and would
        // have forwarded the writes above had the filter let them through,
        // rather than this test passing merely because nothing was ever
        // observed at all
        let real = root.join("sub").join("real.rs");
        std::fs::write(&real, b"y").unwrap();

        let paths = wait_for_path(&rx, &real);
        for path in &paths {
            assert!(
                !is_excluded(path, &root),
                "an excluded path reached the batch: {path:?}"
            );
        }

        handle.stop();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A gitignored directory is never registered either: the project's own
    /// ignore rules are honoured, so a build directory this list does not
    /// name by hand still costs nothing.
    #[test]
    fn a_gitignored_directory_is_never_watched() {
        let root = tempdir();
        std::fs::write(root.join(".gitignore"), b"generated/\n").unwrap();
        std::fs::create_dir_all(root.join("generated")).unwrap();
        let (tx, rx) = channel::<Msg>();
        let handle = spawn(&root, move |msg| {
            let _ = tx.send(msg);
        })
        .expect("watch must start");
        assert!(handle.wait_until_watching(Duration::from_secs(5)));

        std::fs::write(root.join("generated").join("out.rs"), b"x").unwrap();
        let real = root.join("hand-written.rs");
        std::fs::write(&real, b"y").unwrap();

        let paths = wait_for_path(&rx, &real);
        assert!(
            !paths.iter().any(|p| p.starts_with(root.join("generated"))),
            "a gitignored path reached the batch: {paths:?}"
        );

        handle.stop();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The exclusion predicate itself, at every depth and in both
    /// directions. A unit check alongside the event-level tests above:
    /// those prove the predicate is wired into the walk, this proves what
    /// the predicate itself answers, including the nested-`.git` case a
    /// root-only prefix check gets wrong.
    #[test]
    fn is_excluded_matches_at_any_depth() {
        let root = Path::new("/proj");
        assert!(is_excluded(Path::new("/proj/.git/index"), root));
        assert!(is_excluded(Path::new("/proj/vendor/dep/.git/HEAD"), root));
        assert!(is_excluded(Path::new("/proj/target/debug/x"), root));
        assert!(is_excluded(
            Path::new("/proj/a/b/node_modules/p/i.js"),
            root
        ));
        assert!(is_excluded(Path::new("/proj/.venv/pyvenv.cfg"), root));
        assert!(is_excluded(Path::new("/elsewhere/src/lib.rs"), root));
        assert!(!is_excluded(Path::new("/proj/src/lib.rs"), root));
        assert!(!is_excluded(Path::new("/proj/.github/ci.yml"), root));
        assert!(!is_excluded(root, root));
    }

    /// `stop` is idempotent -- a second call on the same handle never
    /// panics -- the mutation this guards against is a `stop` that unwraps
    /// an already-`None` slot instead of matching on it.
    #[test]
    fn stop_is_idempotent() {
        let root = tempdir();
        let handle = spawn(&root, |_| {}).expect("watch must start");
        handle.stop();
        handle.stop();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// After `stop`, a subsequent write under the (still-existing) root is
    /// never forwarded -- proof that `stop` genuinely unregisters the
    /// platform watcher rather than merely dropping this handle's own
    /// reference while a background subscription lives on.
    #[test]
    fn stop_ends_the_watch() {
        let root = tempdir();
        let (tx, rx) = channel::<Msg>();
        let handle = spawn(&root, move |msg| {
            let _ = tx.send(msg);
        })
        .expect("watch must start");
        handle.stop();

        std::fs::write(root.join("after-stop.rs"), b"z").unwrap();

        match rx.recv_timeout(Duration::from_millis(500)) {
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {}
            Ok(msg) => panic!("expected no event after stop, got {msg:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A watch that cannot cover a directory says so rather than leaving
    /// the session believing detection is on. Driven through [`register`]
    /// itself against a start path that disappears before the walk reaches
    /// it, since exhausting the host's real watch limit -- the other way
    /// registration degrades -- is not something a test may do to the
    /// machine it runs on.
    #[test]
    fn a_registration_failure_is_reported_rather_than_swallowed() {
        let root = tempdir();
        let (tx, rx) = channel::<notify::Result<Event>>();
        let watcher = notify::recommended_watcher(move |result| {
            let _ = tx.send(result);
        })
        .expect("backend must be creatable");
        let slot: WatcherSlot = Arc::new(Mutex::new(Some(Box::new(watcher))));
        let (msg_tx, msg_rx) = channel::<Msg>();

        std::fs::create_dir_all(root.join("vanishing")).unwrap();
        // registered from a start path the walk still lists but the backend
        // can no longer watch: the same shape a watch-limit refusal has
        let start = root.join("vanishing");
        let emit = move |msg: Msg| {
            let _ = msg_tx.send(msg);
        };
        std::fs::remove_dir_all(&start).unwrap();
        let _ = register(&slot, &root, &start, &emit, None);
        drop(rx);

        match msg_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Msg::ExternalWatchDegraded { reason }) => {
                assert!(
                    reason.contains("will not be noticed"),
                    "a degraded watch must say what stopped working, got {reason:?}"
                );
            }
            Ok(other) => panic!("expected ExternalWatchDegraded, got {other:?}"),
            Err(err) => panic!("a failed registration must report itself: {err}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A backend error result carries "events were lost" (inotify's own
    /// queue overflow arrives this way), so the pump reports it instead of
    /// dropping it on the floor -- and reports it once, not once per error,
    /// since a storm of notices is its own denial of service.
    #[test]
    fn a_backend_error_is_reported_once() {
        let root = tempdir();
        let (tx, rx) = channel::<notify::Result<Event>>();
        let (msg_tx, msg_rx) = channel::<Msg>();
        let slot: WatcherSlot = Arc::new(Mutex::new(None));
        for _ in 0..3 {
            tx.send(Err(notify::Error::generic("queue overflow")))
                .unwrap();
        }
        drop(tx);

        let emit = move |msg: Msg| {
            let _ = msg_tx.send(msg);
        };
        pump(&rx, &slot, &root, &emit);

        match msg_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Msg::ExternalWatchDegraded { reason }) => {
                assert!(reason.contains("may go unnoticed"), "got {reason:?}");
            }
            Ok(other) => panic!("expected ExternalWatchDegraded, got {other:?}"),
            Err(err) => panic!("a backend error must report itself: {err}"),
        }
        assert!(
            msg_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "three backend errors must not produce three notices"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The pump filters excluded paths itself, not only through what the
    /// walk registered: driven with a synthetic event naming a path under
    /// a nested `.git`, which is exactly what a backend that reports a
    /// directory it was never asked to watch would deliver.
    #[test]
    fn the_pump_drops_an_excluded_path_it_is_handed_directly() {
        let root = PathBuf::from("/proj");
        let (tx, rx) = channel::<notify::Result<Event>>();
        let (msg_tx, msg_rx) = channel::<Msg>();
        let slot: WatcherSlot = Arc::new(Mutex::new(None));
        tx.send(Ok(Event::new(notify::EventKind::Modify(
            notify::event::ModifyKind::Data(notify::event::DataChange::Any),
        ))
        .add_path(root.join("vendor/dep/.git/HEAD"))
        .add_path(root.join("src/lib.rs"))))
            .unwrap();
        drop(tx);

        let emit = move |msg: Msg| {
            let _ = msg_tx.send(msg);
        };
        pump(&rx, &slot, &root, &emit);

        // the pump returns on disconnect without flushing, so the batch is
        // observed by draining whatever it emitted before that
        let mut seen: Vec<PathBuf> = Vec::new();
        while let Ok(msg) = msg_rx.try_recv() {
            if let Msg::ExternalWritesDetected { paths } = msg {
                seen.extend(paths);
            }
        }
        assert!(
            !seen.iter().any(|p| p.to_string_lossy().contains(".git")),
            "an excluded path survived the pump: {seen:?}"
        );
    }
}
