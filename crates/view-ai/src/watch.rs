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

use std::collections::{BTreeMap, BTreeSet};
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

/// How long a path that answered `gone` is given to come back before that
/// answer is believed. What has to be outlasted is the file's own absence
/// from the filesystem, not any message about it: the confirming probe
/// stats the path itself rather than waiting for the batch that would carry
/// the rewrite.
///
/// Two [`COALESCE_WINDOW`]s rather than a number of its own, because the
/// window is what sets how long that absence can last while still being one
/// ordinary save: the unlink is nominated by the window it falls in, and
/// the rewrite is a save that has not finished until the window after that
/// one -- a grace shorter than the two of them re-probes a path whose save
/// is still legitimately in progress, which is the flash the grace exists
/// to prevent. `the_grace_outlasts_the_absence_a_save_can_leave` is what
/// holds the two constants in that relation.
pub const FILE_GONE_GRACE: Duration = Duration::from_millis(2 * COALESCE_WINDOW.as_millis() as u64);

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

/// The per-directory ignore files the registration walk reads, and so the
/// ones the pump has to be able to answer with too (see [`IgnoreRules`]).
/// Ordered by the precedence the walk gives them, highest first.
const IGNORE_FILES: [&str; 2] = [".ignore", ".gitignore"];

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

/// Standing of a source in the order the walk consults them, ascending, so
/// that one reverse iteration over the map answers in precedence order.
///
/// The order is `ignore::dir::Ignore::matched_ignore`'s own `or` chain --
/// `.ignore`, then `.gitignore`, then `.git/info/exclude`, then the global
/// excludes -- and it holds whatever the depths involved are.
const AUTHORITY_GLOBAL: u8 = 0;
const AUTHORITY_EXCLUDE: u8 = 1;

/// Standing of one of the [`IGNORE_FILES`] the walk discovers, by name.
///
/// The two external sources name their own standing at their single call
/// site instead of coming through here: `core.excludesFile` may point at a
/// file called `.gitignore`, and a name lookup would hand it a standing
/// three ranks above the one git gives it.
fn authority(file: &Path) -> u8 {
    match file.file_name().and_then(std::ffi::OsStr::to_str) {
        Some(".ignore") => 3,
        _ => 2,
    }
}

/// The ignore rules the registration walk resolved, kept in a form the pump
/// can hold a single event to.
///
/// Skipping ignored directories during the walk is enough on inotify -- a
/// directory that was never registered raises nothing -- but macOS's
/// FSEvents backend delivers a whole subtree's events to any descriptor
/// placed above it, whatever `RecursiveMode::NonRecursive` asked for. The
/// pump therefore sees writes under directories the walk deliberately left
/// unregistered, which is the build output this watch exists to stay out
/// of, and needs the same answer the walk had. Matching happens off these
/// pre-read rules rather than off the filesystem because the pump may not
/// re-read an ignore file per event.
///
/// One matcher per ignore file, each rooted at the directory that file
/// governs, because `GitignoreBuilder` anchors every line it reads to the
/// builder's own root: a single builder rooted at the project root turns a
/// nested `/out` into the root's `out` and a nested `out/` into `**/out/`
/// across the whole tree, which is both under- and over-ignoring against
/// what the walk computed.
struct IgnoreRules {
    /// The floor of the ancestor walk in [`Self::ignores`]. Rules above the
    /// project root exist (git reads them, so the walk does too) but a
    /// verdict on a directory above the root would put the whole project
    /// inside an ignored tree, which is never what this watch should
    /// conclude.
    root: PathBuf,
    /// Keyed by standing (see [`authority`]), then by the depth of the
    /// directory the matcher governs, then by the file itself.
    ///
    /// Depth is explicit rather than inferred from where the key sorts:
    /// `Path`'s `Ord` is component-wise, so an ancestor's `.gitignore`
    /// sorts *after* a child directory whose name starts below `.` (0x2E)
    /// -- ` `, `!`, `(`, `+`, `-` and the rest -- and a lexical order would
    /// hand `app/(marketing)/.gitignore` a lower standing than the root's,
    /// dropping a real write wherever the nested file re-includes what the
    /// root ignores.
    files: BTreeMap<(u8, usize, PathBuf), ignore::gitignore::Gitignore>,
}

impl IgnoreRules {
    /// Rules for `root` holding nothing yet -- what the walk is about to
    /// fill in.
    fn rooted_at(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            files: BTreeMap::new(),
        }
    }

    /// [`Self::rooted_at`], plus the three sources git resolves from
    /// outside the tree: ignore files in the directories above `root`,
    /// `.git/info/exclude`, and the user's global excludes.
    ///
    /// The walk reads all three -- `require_git(false)` is what makes its
    /// git-owned sources unconditional rather than contingent on a `.git`
    /// being found -- and none of them can be discovered from inside the
    /// tree the walk hands back: `.git` is in [`NEVER_WATCHED`], and the
    /// other two live outside the root entirely.
    fn for_project(root: &Path) -> Self {
        let mut rules = Self::rooted_at(root);
        for dir in root.ancestors().skip(1) {
            for name in IGNORE_FILES {
                rules.add(&dir.join(name));
            }
        }
        // both rooted at the project root rather than at the directory
        // holding them: git anchors `.git/info/exclude` and
        // `core.excludesFile` patterns at the top of the work tree, so
        // `/vendor` in either means the project's own `vendor`.
        //
        // `Gitignore::global()` is not what reads the second one, because it
        // roots the file at `std::env::current_dir()` -- the same rooting
        // the walk uses, and coherent for a search tool answering about the
        // directory it was invoked in, but for a watch it makes the set of
        // ignored paths depend on where the editor was launched from: from a
        // project's parent, a global `/myproject/vendor` would silently
        // swallow every write under a tracked `vendor/`.
        rules.add_rooted(
            root,
            &root.join(".git").join("info").join("exclude"),
            AUTHORITY_EXCLUDE,
        );
        if let Some(global) = ignore::gitignore::gitconfig_excludes_path() {
            rules.add_rooted(root, &global, AUTHORITY_GLOBAL);
        }
        rules
    }

    /// Reads one ignore file, rooted at the directory it sits in.
    fn add(&mut self, file: &Path) {
        let Some(dir) = file.parent() else {
            return;
        };
        self.add_rooted(dir, file, authority(file));
    }

    /// Reads one ignore file whose rules govern `dir`, replacing whatever
    /// that same file contributed before -- so a directory registered a
    /// second time re-reads it rather than stacking a duplicate, and an
    /// edited or deleted file is re-read rather than believed.
    ///
    /// A file holding nothing that matches is dropped rather than kept, and
    /// a file that cannot be read holds nothing: every rule left here is
    /// one the pump consults per event.
    fn add_rooted(&mut self, dir: &Path, file: &Path, authority: u8) {
        let mut builder = ignore::gitignore::GitignoreBuilder::new(dir);
        // a partial error still yields the lines that did parse, exactly
        // what the walk itself matched against
        let _ = builder.add(file);
        let key = (authority, dir.components().count(), file.to_path_buf());
        match builder.build() {
            Ok(matcher) if !matcher.is_empty() => self.files.insert(key, matcher),
            _ => self.files.remove(&key),
        };
    }

    /// Whether the ignore rules cover `path`, itself or through an excluded
    /// directory above it.
    ///
    /// Resolved the way git descends rather than by asking one matcher
    /// about the whole path: every directory between the root and `path` is
    /// judged from the top down, and the first excluded one is final.
    /// `gitignore(5)` is explicit that "it is not possible to re-include a
    /// file if a parent directory of that file is excluded", and the walk
    /// agrees with it by construction -- it never descends into an excluded
    /// directory, so a `!` line further down is never even read.
    ///
    /// `is_dir` is not a hint: a directory-only rule (`generated/`) matches
    /// the directory it names only when the matcher is told the path is
    /// one, and answering `false` for a directory lets the event that
    /// created it through -- which is how a whole ignored tree gets
    /// registered and swept into a batch by the one create event the walk
    /// had already refused.
    fn ignores(&self, path: &Path, is_dir: bool) -> bool {
        // gathered once, in precedence order: a matcher that governs any
        // directory on this path governs the path too, so the walk below
        // reuses this list instead of re-scanning every rule file in the
        // project for each directory it steps through
        let governing: Vec<&ignore::gitignore::Gitignore> = self
            .files
            .values()
            .rev()
            .filter(|matcher| path.starts_with(matcher.path()))
            .collect();
        let above: Vec<&Path> = path
            .ancestors()
            .skip(1)
            .take_while(|dir| *dir != self.root)
            .collect();
        for dir in above.into_iter().rev() {
            if verdict(&governing, dir, true) == Some(true) {
                return true;
            }
        }
        verdict(&governing, path, is_dir) == Some(true)
    }
}

/// `Some(true)` excluded, `Some(false)` re-included, `None` unmatched by
/// any rule -- the three answers git distinguishes, since a `!` line has
/// to stop a lower-standing source from being consulted at all.
fn verdict(governing: &[&ignore::gitignore::Gitignore], path: &Path, is_dir: bool) -> Option<bool> {
    for matcher in governing {
        // re-tested per directory, and not only when the list was
        // built: the crate's own prefix strip is byte-wise, so a
        // directory whose name merely extends this matcher's root
        // would be matched against a mangled tail rather than
        // rejected. `Path::starts_with` is component-aware.
        if !path.starts_with(matcher.path()) {
            continue;
        }
        if let Some(verdict) = decide(matcher, path, is_dir) {
            return Some(verdict);
        }
    }
    None
}

/// One matcher's answer as the three-way verdict [`verdict`] carries.
fn decide(matcher: &ignore::gitignore::Gitignore, path: &Path, is_dir: bool) -> Option<bool> {
    let matched = matcher.matched(path, is_dir);
    if matched.is_whitelist() {
        return Some(false);
    }
    if matched.is_ignore() {
        return Some(true);
    }
    None
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
/// Every [`IGNORE_FILES`] entry the walk passes joins `ignores`, so the pump
/// can hold an event to the same rules this walk held a directory to (see
/// [`IgnoreRules`]).
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
    ignores: &mut IgnoreRules,
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
            if IGNORE_FILES.iter().any(|name| entry.file_name() == *name) {
                ignores.add(entry.path());
            }
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
/// Every path is held to [`is_excluded`] and to `ignores` before it joins a
/// batch, rather than trusted because the walk chose what to register: a
/// backend free to report a subtree it was never asked to watch (FSEvents
/// is) would otherwise put exactly the ignored build output back in front
/// of nvim.
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
    ignores: &mut IgnoreRules,
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
        // a removal is nominated exactly like a write. An agent deleting a
        // file the user has open is the most material shape this watch
        // has, and it is the only one that raises neither a create nor a
        // modify of its own -- filtering it left the user with a buffer
        // whose file was gone and nothing said until something unrelated
        // touched the path. Atomic-save tooling (temp file plus rename)
        // raises no removal at all: a rename over the target arrives as a
        // move-in, which the modify arm above already nominated. The save
        // shape that does raise one is the unlink-then-rewrite a generator
        // clearing its output performs, and it is harmless because
        // nomination is not a verdict: the checktime probe re-stats every
        // path before answering, so a path that is back by then reads as
        // the ordinary reload it is
        if !(event.kind.is_create() || event.kind.is_modify() || event.kind.is_remove()) {
            continue;
        }
        let is_create = event.kind.is_create();
        for path in event.paths {
            if is_excluded(&path, root) {
                continue;
            }
            // an ignore file that changed on disk re-reads here rather than
            // waiting for its directory to be registered again: an agent
            // adding `dist/` to `.gitignore` and then building is a whole
            // session's worth of churn otherwise, and rebuilding one
            // matcher is bounded work the walk would repeat anyway
            if IGNORE_FILES
                .iter()
                .any(|name| path.file_name().is_some_and(|got| got == *name))
            {
                ignores.add(&path);
            }
            if ignores.ignores(&path, false) {
                continue;
            }
            // no event kind every backend raises carries "this is a
            // directory", and a directory-only rule needs that answer (see
            // [`IgnoreRules::ignores`]) -- but only about the directory's
            // own path, since every path under it is already answered by
            // the ancestor walk above. Only a create can register one, so
            // that is the only event that pays the stat, exactly as before
            // the ignore rules existed. The link itself is what is asked
            // about, never its target: the walk does not follow one either
            let is_dir = is_create
                && path
                    .symlink_metadata()
                    .is_ok_and(|meta| meta.file_type().is_dir());
            if is_dir && ignores.ignores(&path, true) {
                continue;
            }
            if is_dir {
                let _ = register(slot, root, &path, emit, Some(&mut pending), ignores);
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
            let mut ignores = IgnoreRules::for_project(&root_owned);
            let _ = register(
                &thread_slot,
                &root_owned,
                &root_owned,
                emit,
                None,
                &mut ignores,
            );
            #[cfg(any(test, feature = "test-support"))]
            {
                let (lock, cv) = &*thread_ready;
                *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
                cv.notify_all();
            }
            pump(&rx, &thread_slot, &root_owned, &mut ignores, emit);
        })
        .map_err(WatchError::ThreadSpawn)?;

    Ok(WatchHandle {
        watcher: slot,
        #[cfg(any(test, feature = "test-support"))]
        ready,
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods
)]
mod tests {
    use super::*;
    use std::sync::mpsc::{channel, RecvTimeoutError};

    /// The grace's magnitude is the feature, not its existence: a grace
    /// shorter than the absence an ordinary save can leave re-probes while
    /// that save is still running, which announces a vanished file over a
    /// write and takes it back a moment later. Two coalesce windows is the
    /// floor because a save whose unlink and rewrite fall in different
    /// windows is not finished until the second window has closed, and the
    /// unlink was nominated out of the first.
    ///
    /// Asserted against `COALESCE_WINDOW` rather than against a number so
    /// the two constants can only move together, and asserted at all
    /// because every other test of this delay is satisfied by any value the
    /// constant can take, zero included.
    #[test]
    fn the_grace_outlasts_the_absence_a_save_can_leave() {
        assert!(
            FILE_GONE_GRACE >= 2 * COALESCE_WINDOW,
            "a grace of {FILE_GONE_GRACE:?} re-probes inside a save whose \
             halves straddle two {COALESCE_WINDOW:?} windows"
        );
    }

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

        let registered = register(
            &slot,
            &root,
            &root,
            &emit,
            None,
            &mut IgnoreRules::rooted_at(&root),
        );

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

        let registered = register(
            &slot,
            &root,
            &root,
            &emit,
            None,
            &mut IgnoreRules::rooted_at(&root),
        );

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
    ///
    /// The counter is what actually separates two callers: macOS reports
    /// `SystemTime` at microsecond granularity, so two of this module's
    /// tests starting in the same microsecond -- which they do, since
    /// `cargo test` starts them all at once -- built the very same name.
    /// `create_dir_all` accepts an existing directory, so that collision was
    /// silent: one test walked another's tree, or read rules a test that
    /// had already finished had deleted underneath it. Creating the
    /// directory non-recursively is what keeps any future collision loud.
    fn tempdir() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "view-ai-watch-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        dir.push(unique);
        std::fs::create_dir(&dir).unwrap();
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

        let registered = register(
            &slot,
            &root,
            &root,
            &emit,
            None,
            &mut IgnoreRules::rooted_at(&root),
        );

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

    /// Removing a file under the watched root raises a batch naming it,
    /// exactly as a write does. The mutation this kills is the filter that
    /// forwarded creates and modifies only: a bare `rm` is neither, so an
    /// agent deleting a file the user had open was noticed by nothing at
    /// all until some later, unrelated write touched the same path.
    #[test]
    fn a_removal_under_the_root_is_detected() {
        let root = tempdir();
        let target = root.join("removed.rs");
        std::fs::write(&target, b"hello").unwrap();
        let (tx, rx) = channel::<Msg>();
        let handle = spawn(&root, move |msg| {
            let _ = tx.send(msg);
        })
        .expect("watch must start against a real, writable tempdir");
        assert!(handle.wait_until_watching(Duration::from_secs(5)));

        std::fs::remove_file(&target).unwrap();

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
        // timed from before the writes, not from the first batch's arrival:
        // the pump emits every batch somewhere inside [here, last arrival],
        // so this envelope can only over-count the windows the pump saw. A
        // receiver-side spread cannot -- when the test thread is descheduled
        // on a loaded host, batches emitted a full window apart queue in the
        // channel and drain in microseconds, compressing the observed spread
        // below the true emission span and failing an honest pump.
        let start = Instant::now();
        let mut wanted: BTreeSet<PathBuf> = BTreeSet::new();
        for i in 0..total {
            let path = root.join(format!("file{i}.rs"));
            std::fs::write(&path, b"x").unwrap();
            wanted.insert(path);
        }

        let mut batches = 0;
        let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
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
                    seen.extend(paths);
                }
                Ok(other) => panic!("unexpected message {other:?}"),
                Err(err) => panic!("channel closed with {} paths seen: {err}", seen.len()),
            }
        }
        // the bound the design actually promises: one probe per coalescing
        // window, not merely "fewer than one per write". A regression to
        // near-per-event probing would still satisfy `batches < total`.
        let spread = start.elapsed();
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
        let _ = register(
            &slot,
            &root,
            &start,
            &emit,
            None,
            &mut IgnoreRules::rooted_at(&root),
        );
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
        pump(&rx, &slot, &root, &mut IgnoreRules::rooted_at(&root), &emit);

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

    /// A modify event naming `paths`, the shape a backend reports a write
    /// to this module with.
    fn modify_event(paths: &[PathBuf]) -> Event {
        naming(
            notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any,
            )),
            paths,
        )
    }

    /// A create event naming `paths` -- the only kind that can register a
    /// directory, so the only one the pump stats for.
    fn create_event(paths: &[PathBuf]) -> Event {
        naming(
            notify::EventKind::Create(notify::event::CreateKind::Any),
            paths,
        )
    }

    fn naming(kind: notify::EventKind, paths: &[PathBuf]) -> Event {
        paths
            .iter()
            .fold(Event::new(kind), |event, path| event.add_path(path.clone()))
    }

    /// A slot holding a backend that accepts every directory, so a walk
    /// through it registers the whole tree rather than stopping at the
    /// first one.
    fn accepting_slot() -> WatcherSlot {
        let watcher = FakeWatcher {
            calls: 0,
            refuse_at: usize::MAX,
            limit_at: usize::MAX,
            watched: Arc::new(Mutex::new(Vec::new())),
        };
        Arc::new(Mutex::new(Some(Box::new(watcher))))
    }

    /// A slot the watch has already been stopped on -- for a case whose
    /// event can never reach the registration arm.
    fn empty_slot() -> WatcherSlot {
        Arc::new(Mutex::new(None))
    }

    /// Seeds `dirs` and `files` under `root`, then registers the tree
    /// exactly as the watch's own thread would, answering the slot and the
    /// rules that walk produced.
    fn walked(root: &Path, files: &[(&str, &str)], dirs: &[&str]) -> (WatcherSlot, IgnoreRules) {
        for dir in dirs {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for (name, body) in files {
            std::fs::write(root.join(name), body.as_bytes()).unwrap();
        }
        registered(root, IgnoreRules::rooted_at(root))
    }

    /// What one registration walk of `root` leaves behind, starting from
    /// rules the caller chose -- [`IgnoreRules::for_project`] for the tests
    /// that care about the sources resolved from outside the tree.
    fn registered(root: &Path, mut ignores: IgnoreRules) -> (WatcherSlot, IgnoreRules) {
        let slot = accepting_slot();
        let quiet = |msg: Msg| panic!("a clean walk must report nothing, got {msg:?}");
        let _ = register(&slot, root, root, &quiet, None, &mut ignores);
        (slot, ignores)
    }

    /// Every path one pre-sent event survives the pump's own filters with.
    ///
    /// The sender is held until the batch has been emitted rather than
    /// dropped up front: the pump exits on disconnect without flushing, so
    /// a caller that closed the channel first would read an empty batch and
    /// see a pump that forwards nothing as one that filtered correctly.
    fn paths_surviving_the_pump(
        root: &Path,
        slot: &WatcherSlot,
        ignores: &mut IgnoreRules,
        event: Event,
    ) -> Vec<PathBuf> {
        let (tx, rx) = channel::<notify::Result<Event>>();
        let (msg_tx, msg_rx) = channel::<Msg>();
        let (emitted_tx, emitted_rx) = channel::<()>();
        tx.send(Ok(event)).unwrap();
        // a pump that emits nothing at all still has to end this test, so
        // the wait is bounded and the empty batch becomes the assertion's
        // failure rather than a hang
        let closer = std::thread::spawn(move || {
            let _ = emitted_rx.recv_timeout(Duration::from_secs(5));
            drop(tx);
        });

        let emit = move |msg: Msg| {
            let _ = msg_tx.send(msg);
            let _ = emitted_tx.send(());
        };
        pump(&rx, slot, root, ignores, &emit);
        closer.join().unwrap();

        let mut seen: Vec<PathBuf> = Vec::new();
        while let Ok(msg) = msg_rx.try_recv() {
            if let Msg::ExternalWritesDetected { paths } = msg {
                seen.extend(paths);
            }
        }
        seen
    }

    /// The pump filters excluded paths itself, not only through what the
    /// walk registered: driven with a synthetic event naming a path under
    /// a nested `.git`, which is exactly what a backend that reports a
    /// directory it was never asked to watch would deliver.
    #[test]
    fn the_pump_drops_an_excluded_path_it_is_handed_directly() {
        let root = PathBuf::from("/proj");
        let slot: WatcherSlot = Arc::new(Mutex::new(None));
        let excluded = root.join("vendor/dep/.git/HEAD");
        let kept = root.join("src/lib.rs");

        let seen = paths_surviving_the_pump(
            &root,
            &slot,
            &mut IgnoreRules::rooted_at(&root),
            modify_event(&[excluded, kept.clone()]),
        );

        assert!(
            !seen.iter().any(|p| p.to_string_lossy().contains(".git")),
            "an excluded path survived the pump: {seen:?}"
        );
        assert!(
            seen.contains(&kept),
            "the pump dropped a path no rule names: {seen:?}"
        );
    }

    /// The pump holds an event to the project's own ignore rules too, not
    /// only to [`NEVER_WATCHED`]. The leak this kills is macOS's: FSEvents
    /// hands a descriptor every event under it whatever
    /// `RecursiveMode::NonRecursive` asked for, so a write inside a
    /// gitignored directory the walk deliberately never registered still
    /// reaches the pump -- and a filter that trusted registration probed
    /// nvim for exactly the build churn this watch promises to skip.
    ///
    /// Driven from a nested `.gitignore` and a synthetic event, so the leak
    /// shape is reproduced on every platform rather than only on the one
    /// that produces it. The non-ignored sibling is what keeps this from
    /// passing on a pump that drops everything.
    ///
    /// The `.ignore` file beside the `.gitignore` is the walk's other
    /// per-directory source, and holding both proves neither displaces the
    /// other -- rules keyed by the directory they govern rather than by the
    /// file they came from would keep only whichever was read last.
    ///
    /// `subsidiary/` is the case the prefix guard in
    /// [`IgnoreRules::verdict`] exists for: the crate's own strip is a
    /// byte-prefix strip, so a matcher rooted at `sub` handed
    /// `subsidiary/out/keep.rs` would match the mangled tail
    /// `sidiary/out/keep.rs` against `out/` and drop a real write in a
    /// directory it has no rules for.
    #[test]
    fn the_pump_drops_a_gitignored_path_it_is_handed_directly() {
        let root = tempdir();
        let (slot, mut ignores) = walked(
            &root,
            &[
                ("sub/.gitignore", "out/\n"),
                ("sub/.ignore", "scratch/\n"),
                // a disjoint sibling, so the map holds matchers that do not
                // govern the paths asserted below
                ("other/.gitignore", "junk/\n"),
            ],
            &["sub/out", "sub/scratch", "other/junk", "subsidiary/out"],
        );

        let ignored = root.join("sub").join("out").join("built.rs");
        let by_dot_ignore = root.join("sub").join("scratch").join("note.txt");
        let by_sibling = root.join("other").join("junk").join("x.o");
        let past_the_prefix = root.join("subsidiary").join("out").join("keep.rs");
        let kept = root.join("sub").join("hand-written.rs");

        let seen = paths_surviving_the_pump(
            &root,
            &slot,
            &mut ignores,
            modify_event(&[
                ignored.clone(),
                by_dot_ignore.clone(),
                by_sibling.clone(),
                past_the_prefix.clone(),
                kept.clone(),
            ]),
        );

        assert!(
            seen.contains(&past_the_prefix),
            "a directory whose name merely extends another's was matched \
             against that other's rules: {seen:?}"
        );

        assert!(
            !seen.contains(&ignored),
            "a gitignored path survived the pump: {seen:?}"
        );
        assert!(
            !seen.contains(&by_dot_ignore),
            "a path the walk's `.ignore` source names survived the pump: {seen:?}"
        );
        assert!(
            !seen.contains(&by_sibling),
            "a sibling directory's own rules were not consulted: {seen:?}"
        );
        assert!(
            seen.contains(&kept),
            "the pump dropped a path no ignore rule names: {seen:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A create event naming an ignored directory registers nothing. This
    /// is the one place the directory's own path has to be recognised as a
    /// directory: a directory-only rule (`out/`) does not match it
    /// otherwise, and the create arm would then walk it and sweep every
    /// file inside into the same batch -- re-registering, from one leaked
    /// event, the whole tree the walk refused.
    #[test]
    fn a_created_gitignored_directory_is_neither_registered_nor_swept() {
        let root = tempdir();
        let (slot, mut ignores) = walked(&root, &[(".gitignore", "out/\n")], &["out"]);
        std::fs::write(root.join("out").join("built.rs"), b"x").unwrap();
        let kept = root.join("hand-written.rs");

        let seen = paths_surviving_the_pump(
            &root,
            &slot,
            &mut ignores,
            create_event(&[root.join("out"), kept.clone()]),
        );

        assert!(
            !seen.iter().any(|p| p.starts_with(root.join("out"))),
            "an ignored directory was registered and swept: {seen:?}"
        );
        assert!(seen.contains(&kept), "got {seen:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Precedence between two ignore files is by the depth of the directory
    /// each governs, taken from the path rather than from where its key
    /// happens to sort. `Path`'s `Ord` is component-wise, so a child
    /// directory whose name starts below `.` (0x2E) -- ` `, `!`, `(`, `+`,
    /// `-` -- sorts *before* its own parent's `.gitignore`, and a lexical
    /// order consults the parent first.
    ///
    /// The consequence is not a spurious probe but a lost write: the nested
    /// file re-includes what the root ignores, the parent's rule is applied
    /// instead, and an agent rewriting that file raises nothing at all --
    /// no batch, no degradation notice, a buffer going stale in silence.
    /// `(marketing)` is the Next.js and Expo route-group convention, not an
    /// exotic name.
    #[test]
    fn a_nested_re_inclusion_wins_whatever_its_directory_is_named() {
        let root = tempdir();
        let groups = ["sub", "(marketing)", "-tmp", "+gen"];
        let files: Vec<(String, String)> =
            std::iter::once((".gitignore".to_string(), "*.log\n".to_string()))
                .chain(
                    groups
                        .iter()
                        .map(|dir| (format!("{dir}/.gitignore"), "!keep.log\n".to_string())),
                )
                .collect();
        let seeded: Vec<(&str, &str)> = files
            .iter()
            .map(|(name, body)| (name.as_str(), body.as_str()))
            .collect();
        let (slot, mut ignores) = walked(&root, &seeded, &groups);

        let re_included: Vec<PathBuf> = groups
            .iter()
            .map(|dir| root.join(dir).join("keep.log"))
            .collect();
        let still_ignored = root.join("noisy.log");

        let seen = paths_surviving_the_pump(
            &root,
            &slot,
            &mut ignores,
            modify_event(
                &re_included
                    .iter()
                    .cloned()
                    .chain(std::iter::once(still_ignored.clone()))
                    .collect::<Vec<_>>(),
            ),
        );

        for path in &re_included {
            assert!(
                seen.contains(path),
                "a nested `!` rule lost to its own parent's: {path:?} missing from {seen:?}"
            );
        }
        assert!(
            !seen.contains(&still_ignored),
            "the root's own rule stopped applying: {seen:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Git resolves a path one directory at a time and never descends into
    /// an excluded one, so a `!` line cannot re-include a file whose parent
    /// directory is already excluded -- and the walk agrees with git by
    /// construction, because it never reads the deeper rule at all. Both
    /// directions, since a matcher asked about the whole path in one call
    /// answers the first match walking upward and gets this backwards.
    #[test]
    fn a_re_inclusion_under_an_excluded_directory_stays_excluded() {
        let root = tempdir();
        let (slot, mut ignores) = walked(
            &root,
            &[(".gitignore", "out/\n!out/keep.rs\ngen/\n!gen/\n")],
            &["out", "gen"],
        );

        let under_excluded = root.join("out").join("keep.rs");
        // the directory itself re-included, which git does allow, so the
        // ancestor walk cannot be a blanket "any rule above wins"
        let under_re_included = root.join("gen").join("made.rs");

        let seen = paths_surviving_the_pump(
            &root,
            &slot,
            &mut ignores,
            modify_event(&[under_excluded.clone(), under_re_included.clone()]),
        );

        assert!(
            !seen.contains(&under_excluded),
            "a `!` rule re-included a file under an excluded directory: {seen:?}"
        );
        assert!(
            seen.contains(&under_re_included),
            "a re-included directory's contents stayed excluded: {seen:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The ancestor walk resolves top down, not bottom up: the shallowest
    /// excluded directory settles the whole subtree, and a rule file inside
    /// it -- one the walk would never have read, because it never descends
    /// there, but a leaked event can still put in front of the pump --
    /// cannot overturn that.
    ///
    /// Asked of [`IgnoreRules`] directly: getting the nested file into the
    /// rules at all takes the mid-session re-read path, and what is under
    /// test here is the resolution order, not how the rules arrived.
    #[test]
    fn the_shallowest_excluded_directory_settles_the_subtree() {
        let root = tempdir();
        std::fs::create_dir_all(root.join("a").join("b")).unwrap();
        std::fs::write(root.join(".gitignore"), b"a/\n").unwrap();
        std::fs::write(root.join("a").join(".gitignore"), b"!b/\n").unwrap();
        let mut rules = IgnoreRules::rooted_at(&root);
        rules.add(&root.join(".gitignore"));
        rules.add(&root.join("a").join(".gitignore"));

        assert!(
            rules.ignores(&root.join("a").join("b").join("c.rs"), false),
            "a `!` rule below an excluded directory resurrected the subtree"
        );
        assert!(
            !rules.ignores(&root.join("kept.rs"), false),
            "the exclusion escaped the directory it names"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// An ignore file edited mid-session is re-read from the event that
    /// changed it, not left until its directory happens to be registered
    /// again: an agent that adds `dist/` and then builds would otherwise
    /// spend the rest of the session probing nvim for every file it
    /// writes there. Deleting the file gives its rules back, on the same
    /// event path.
    #[test]
    fn an_ignore_file_changed_mid_session_is_re_read() {
        let root = tempdir();
        let (slot, mut ignores) = walked(&root, &[], &["dist"]);
        let built = root.join("dist").join("app.js");
        let gitignore = root.join(".gitignore");

        let before = paths_surviving_the_pump(
            &root,
            &slot,
            &mut ignores,
            modify_event(std::slice::from_ref(&built)),
        );
        assert!(before.contains(&built), "got {before:?}");

        std::fs::write(&gitignore, b"dist/\n").unwrap();
        let after = paths_surviving_the_pump(
            &root,
            &slot,
            &mut ignores,
            modify_event(&[gitignore.clone(), built.clone()]),
        );
        assert!(
            !after.contains(&built),
            "a rule added mid-session was not honoured: {after:?}"
        );
        assert!(
            after.contains(&gitignore),
            "the ignore file's own write must still be reported: {after:?}"
        );

        std::fs::remove_file(&gitignore).unwrap();
        let removed = paths_surviving_the_pump(
            &root,
            &slot,
            &mut ignores,
            modify_event(&[gitignore.clone(), built.clone()]),
        );
        assert!(
            removed.contains(&built),
            "a deleted ignore file's rules outlived it: {removed:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Ignore files above the project root are the walk's own behaviour
    /// (`WalkBuilder::parents` defaults on), so a `~/repos/.gitignore`
    /// naming `build/` covers a project checked out beneath it.
    #[test]
    fn an_ignore_file_above_the_root_is_honoured() {
        let outer = tempdir();
        std::fs::write(outer.join(".gitignore"), b"build/\n").unwrap();
        let root = outer.join("project");
        std::fs::create_dir_all(root.join("build")).unwrap();
        let (slot, mut ignores) = (empty_slot(), IgnoreRules::for_project(&root));

        let built = root.join("build").join("out.o");
        let kept = root.join("src.rs");

        let seen = paths_surviving_the_pump(
            &root,
            &slot,
            &mut ignores,
            modify_event(&[built.clone(), kept.clone()]),
        );

        assert!(
            !seen.contains(&built),
            "a rule above the project root was not read: {seen:?}"
        );
        assert!(seen.contains(&kept), "got {seen:?}");

        let _ = std::fs::remove_dir_all(&outer);
    }

    /// `.git/info/exclude` is the repository's own unshared rule file and
    /// the walk reads it, but `.git` is in [`NEVER_WATCHED`], so no walk of
    /// the tree can ever discover it -- registration has to add it by name.
    #[test]
    fn the_repositorys_own_exclude_file_is_honoured() {
        let root = tempdir();
        std::fs::create_dir_all(root.join(".git").join("info")).unwrap();
        std::fs::write(
            root.join(".git").join("info").join("exclude"),
            b"scratch/\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("scratch")).unwrap();
        let (slot, mut ignores) = (empty_slot(), IgnoreRules::for_project(&root));

        let excluded = root.join("scratch").join("note.txt");
        let kept = root.join("src.rs");

        let seen = paths_surviving_the_pump(
            &root,
            &slot,
            &mut ignores,
            modify_event(&[excluded.clone(), kept.clone()]),
        );

        assert!(
            !seen.contains(&excluded),
            ".git/info/exclude was not read: {seen:?}"
        );
        assert!(seen.contains(&kept), "got {seen:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Two sources naming the same file are settled by which source it is,
    /// never by which one is consulted first: `.gitignore` outranks
    /// `.git/info/exclude`, so its re-inclusion is the whole answer and the
    /// write is still probed.
    #[test]
    fn a_re_inclusion_in_the_higher_standing_source_wins() {
        let root = tempdir();
        std::fs::create_dir_all(root.join(".git").join("info")).unwrap();
        std::fs::write(root.join(".git").join("info").join("exclude"), b"*.log\n").unwrap();
        std::fs::write(root.join(".gitignore"), b"!keep.log\n").unwrap();
        let (slot, mut ignores) = registered(&root, IgnoreRules::for_project(&root));

        let kept = root.join("keep.log");
        let dropped = root.join("noise.log");

        let seen = paths_surviving_the_pump(
            &root,
            &slot,
            &mut ignores,
            modify_event(&[kept.clone(), dropped.clone()]),
        );

        assert!(
            seen.contains(&kept),
            ".git/info/exclude outranked .gitignore: {seen:?}"
        );
        assert!(!seen.contains(&dropped), "got {seen:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The same pair the other way up: a re-inclusion in
    /// `.git/info/exclude` cannot survive `.gitignore`'s exclusion, because
    /// the higher-standing source answers first and a lower one is then
    /// never consulted at all.
    #[test]
    fn a_re_inclusion_in_the_lower_standing_source_loses() {
        let root = tempdir();
        std::fs::create_dir_all(root.join(".git").join("info")).unwrap();
        std::fs::write(
            root.join(".git").join("info").join("exclude"),
            b"!keep.log\n",
        )
        .unwrap();
        std::fs::write(root.join(".gitignore"), b"*.log\n").unwrap();
        let (slot, mut ignores) = registered(&root, IgnoreRules::for_project(&root));

        let dropped = root.join("keep.log");
        let kept = root.join("src.rs");

        let seen = paths_surviving_the_pump(
            &root,
            &slot,
            &mut ignores,
            modify_event(&[dropped.clone(), kept.clone()]),
        );

        assert!(
            !seen.contains(&dropped),
            ".git/info/exclude outranked .gitignore: {seen:?}"
        );
        assert!(seen.contains(&kept), "got {seen:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `.ignore` is the top of the same order, above a sibling `.gitignore`
    /// in the very same directory -- where depth cannot be what separates
    /// them.
    #[test]
    fn a_dot_ignore_outranks_its_sibling_gitignore() {
        let root = tempdir();
        std::fs::write(root.join(".gitignore"), b"*.log\n").unwrap();
        std::fs::write(root.join(".ignore"), b"!keep.log\n").unwrap();
        let (slot, mut ignores) = registered(&root, IgnoreRules::for_project(&root));

        let kept = root.join("keep.log");
        let dropped = root.join("noise.log");

        let seen = paths_surviving_the_pump(
            &root,
            &slot,
            &mut ignores,
            modify_event(&[kept.clone(), dropped.clone()]),
        );

        assert!(
            seen.contains(&kept),
            ".gitignore outranked .ignore: {seen:?}"
        );
        assert!(!seen.contains(&dropped), "got {seen:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `GIT_CONFIG_GLOBAL` for as long as it is held, restored on the way
    /// out -- a panicking assertion included, since a leaked plant would
    /// point every later `IgnoreRules::for_project` in this binary at a
    /// config file the test has already deleted.
    ///
    /// Only one test plants it, so nothing here needs serializing against
    /// another writer; the tests that read it concurrently are safe because
    /// the planted rules name paths (`vendor`, `*.swp`) that appear in no
    /// other test's tree.
    struct GitConfigGlobal(Option<std::ffi::OsString>);

    impl GitConfigGlobal {
        fn planted(config: &Path) -> Self {
            let prev = std::env::var_os("GIT_CONFIG_GLOBAL");
            std::env::set_var("GIT_CONFIG_GLOBAL", config);
            Self(prev)
        }
    }

    impl Drop for GitConfigGlobal {
        fn drop(&mut self) {
            match &self.0 {
                Some(prev) => std::env::set_var("GIT_CONFIG_GLOBAL", prev),
                None => std::env::remove_var("GIT_CONFIG_GLOBAL"),
            }
        }
    }

    /// The user's global excludes are read as git reads them: anchored at
    /// the top of the work tree, not at the directory the editor happened to
    /// be launched from. A pattern naming the project directory itself is
    /// therefore inert -- it cannot swallow every write under a tracked
    /// `vendor/` because a session started one level up -- while an
    /// unanchored pattern still matches at any depth.
    #[test]
    fn a_global_exclude_anchors_at_the_project_root() {
        let outer = tempdir();
        let root = outer.join("project");
        std::fs::create_dir_all(root.join("vendor")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let excludes = outer.join("excludes");
        std::fs::write(&excludes, b"/vendor\n/project/tools\n*.swp\n").unwrap();
        let config = outer.join("gitconfig");
        std::fs::write(
            &config,
            format!("[core]\nexcludesFile = {}\n", excludes.display()),
        )
        .unwrap();

        let planted = GitConfigGlobal::planted(&config);
        let (slot, mut ignores) = (empty_slot(), IgnoreRules::for_project(&root));
        drop(planted);

        let vendored = root.join("vendor").join("lib.rs");
        let swap = root.join("src").join(".main.rs.swp");
        let kept = root.join("tools").join("build.rs");

        let seen = paths_surviving_the_pump(
            &root,
            &slot,
            &mut ignores,
            modify_event(&[vendored.clone(), swap.clone(), kept.clone()]),
        );

        assert!(
            !seen.contains(&vendored),
            "an anchored global pattern was not anchored at the root: {seen:?}"
        );
        assert!(
            !seen.contains(&swap),
            "an unanchored global pattern stopped matching: {seen:?}"
        );
        assert!(
            seen.contains(&kept),
            "a global pattern naming the project directory dropped a tracked write: {seen:?}"
        );

        let _ = std::fs::remove_dir_all(&outer);
    }
}
