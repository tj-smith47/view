//! Out-of-band write watcher: catches a write that never went through ACP
//! at all -- an agent's own shell tool is the case no MCP/ACP client can
//! see or route -- and turns it into `Msg::ExternalWriteDetected` so
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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use view_core::msg::Msg;

/// Same-path events closer together than this are folded into one
/// `Msg::ExternalWriteDetected`. Atomic-save tooling (temp file plus
/// rename) and a single `write(2)` both raise several raw events for what
/// a user experiences as one save; a checktime round trip per raw event
/// would spam the engine for no correctness gain, since nvim's own
/// mtime-based disposition is already idempotent against redundant probes.
const COALESCE_WINDOW: Duration = Duration::from_millis(50);

/// Everything [`spawn`] can fail with.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    /// The platform watcher backend (inotify/FSEvents/ReadDirectoryChangesW)
    /// could not be created, or could not start watching `root`.
    #[error("could not watch {root} for external writes: {source}")]
    Backend {
        /// The project root the watch was attempted against.
        root: PathBuf,
        /// The underlying platform-watcher error.
        source: notify::Error,
    },
    /// The pump thread that drains the watcher's own event channel could
    /// not be started.
    #[error("could not start the out-of-band write watcher's own thread: {0}")]
    ThreadSpawn(std::io::Error),
}

/// A running out-of-band write watch over one project root.
///
/// Cloneable and cheap: every clone shares the same underlying platform
/// watcher, so an explicit [`Self::stop`] call on any one of them tears
/// down the one real subscription for all of them -- the same
/// shared-teardown shape `AiWorker`'s own clone already has for its
/// session slot. No custom `Drop` impl: once the last clone's `Arc` goes
/// away, the `Mutex<Option<RecommendedWatcher>>` it owns drops along with
/// it, which drops the `RecommendedWatcher` inside on exactly the same
/// terms `stop` unregisters it on -- the cascade already does the right
/// thing without repeating that logic in a second place.
#[derive(Clone)]
pub struct WatchHandle {
    // `Some` for the life of the watch; `stop` takes it out and drops it,
    // which unregisters the platform backend and, in turn, closes the
    // channel the pump thread blocks on -- that closed channel is the pump
    // thread's own exit signal, so no separate shutdown flag exists.
    watcher: Arc<Mutex<Option<RecommendedWatcher>>>,
}

impl WatchHandle {
    /// Stops the watch. Idempotent: a second call, or a call after every
    /// clone has already been dropped, is a no-op.
    pub fn stop(&self) {
        let mut guard = self.watcher.lock().unwrap_or_else(|e| e.into_inner());
        *guard = None;
    }
}

/// Whether `path` (already known to be under `root`) falls under `root`'s
/// own `.git` directory. Filtered at the source rather than left for
/// nvim's own checktime round trip to no-op on: a project's `.git` churns
/// constantly during normal agent activity (index locks, packed-refs,
/// FETCH_HEAD) and virtually never names a loaded buffer, so forwarding it
/// would be a steady stream of round trips nvim always answers `found:
/// false` to -- pure overhead with no conflict a user could ever see.
fn is_git_internal(path: &Path, root: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|rel| {
        rel.components()
            .next()
            .is_some_and(|c| c.as_os_str() == ".git")
    })
}

/// Drains `rx` until the watcher side closes it (see [`WatchHandle::stop`]),
/// forwarding one coalesced [`Msg::ExternalWriteDetected`] per path per
/// [`COALESCE_WINDOW`]. Runs entirely on its own thread: `emit` reaches the
/// loop channel exactly the way `AiSession::spawn`'s own `emit` does, and
/// carries the same never-block requirement, since this thread has nothing
/// else to do but keep draining `rx`.
fn pump(rx: &std_mpsc::Receiver<notify::Result<Event>>, root: &Path, emit: impl Fn(Msg)) {
    let mut last_emit: HashMap<PathBuf, Instant> = HashMap::new();
    while let Ok(result) = rx.recv() {
        let Ok(event) = result else { continue };
        if !(event.kind.is_create() || event.kind.is_modify()) {
            continue;
        }
        for path in event.paths {
            if is_git_internal(&path, root) {
                continue;
            }
            let now = Instant::now();
            if let Some(prev) = last_emit.get(&path) {
                if now.duration_since(*prev) < COALESCE_WINDOW {
                    continue;
                }
            }
            last_emit.insert(path.clone(), now);
            emit(Msg::ExternalWriteDetected { path });
        }
    }
}

/// Starts watching `root` (recursively) for out-of-band writes, calling
/// `emit` once per detected write path (already coalesced and filtered,
/// see [`pump`]'s own doc). `emit` must not block, on the same terms every
/// other cross-crate `Msg` emitter in this crate carries (see
/// `AiSession::spawn`'s own doc): it runs on this watch's own pump thread,
/// which has nothing else to do but keep draining the platform watcher's
/// event channel.
///
/// # Errors
///
/// [`WatchError::Backend`] if the platform watcher cannot be created or
/// cannot start watching `root` (a nonexistent root, or a permissions
/// refusal). [`WatchError::ThreadSpawn`] if the OS refuses the pump
/// thread.
pub fn spawn(root: &Path, emit: impl Fn(Msg) + Send + 'static) -> Result<WatchHandle, WatchError> {
    let (tx, rx) = std_mpsc::channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(move |result| {
        let _ = tx.send(result);
    })
    .map_err(|source| WatchError::Backend {
        root: root.to_path_buf(),
        source,
    })?;
    watcher
        .watch(root, RecursiveMode::Recursive)
        .map_err(|source| WatchError::Backend {
            root: root.to_path_buf(),
            source,
        })?;

    let root_owned = root.to_path_buf();
    std::thread::Builder::new()
        .name("view-ai-watch".to_string())
        .spawn(move || pump(&rx, &root_owned, emit))
        .map_err(WatchError::ThreadSpawn)?;

    Ok(WatchHandle {
        watcher: Arc::new(Mutex::new(Some(watcher))),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::mpsc::{channel, RecvTimeoutError};

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
        dir
    }

    /// Writing a file under the watched root raises exactly one
    /// `Msg::ExternalWriteDetected` for it -- the falsifiable core of
    /// `spawn`'s own contract. `recv_timeout`, never a bare `recv`: a
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

        let target = root.join("touched.rs");
        std::fs::write(&target, b"hello").unwrap();

        let msg = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a write under the watched root must be detected within 5s");
        match msg {
            Msg::ExternalWriteDetected { path } => {
                assert_eq!(
                    std::fs::canonicalize(&path).unwrap(),
                    std::fs::canonicalize(&target).unwrap()
                );
            }
            other => panic!("expected Msg::ExternalWriteDetected, got {other:?}"),
        }

        handle.stop();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A write under the root's own `.git` directory is never forwarded --
    /// the noise filter [`is_git_internal`] exists for. Proves the filter
    /// with a real event, not just the string-matching unit check below,
    /// since a filter that compiles but is wired to the wrong predicate
    /// would still pass a `is_git_internal` unit test alone.
    #[test]
    fn a_write_under_dot_git_is_never_forwarded() {
        let root = tempdir();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let (tx, rx) = channel::<Msg>();
        let handle = spawn(&root, move |msg| {
            let _ = tx.send(msg);
        })
        .expect("watch must start against a real, writable tempdir");

        std::fs::write(root.join(".git").join("index.lock"), b"x").unwrap();
        // a real, non-.git write proves the pump thread is alive and would
        // have forwarded the .git write above had the filter let it
        // through, rather than this test passing merely because nothing
        // was ever observed at all
        std::fs::write(root.join("real.rs"), b"y").unwrap();

        let msg = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the real, non-.git write must still be detected within 5s");
        match msg {
            Msg::ExternalWriteDetected { path } => {
                assert!(
                    !path.starts_with(root.join(".git")),
                    "the .git write must have been filtered, got {path:?} first"
                );
            }
            other => panic!("expected Msg::ExternalWriteDetected, got {other:?}"),
        }

        handle.stop();
        let _ = std::fs::remove_dir_all(&root);
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
}
