//! The clipboard worker: the one thread that touches the system clipboard,
//! off the paint loop. The loop only ever hands a read or write across a
//! channel and never blocks on the answer itself; this thread owns the
//! reply obligation for both.
//!
//! # The remote-paste contract
//!
//! There is no OSC52 *read* path here, and that is a stated limitation, not
//! an oversight: OSC52 paste-back requires the terminal to answer a query
//! escape sequence, which most terminal emulators refuse by default for
//! security reasons (an arbitrary program reading the system clipboard
//! without a user gesture), so it is not a mechanism this worker can lean
//! on. Instead, every successful [`ClipboardJobKind::Write`] updates an
//! in-memory shadow register alongside the real system-clipboard write, and
//! a [`ClipboardJobKind::Read`] falls back to that shadow whenever
//! `arboard` itself cannot reach a clipboard -- which is exactly the
//! situation an SSH session with no forwarded display is in. That matches
//! the behavior every remote nvim setup already has: `"+p` after `"+yy`
//! works across the same session, and reading a value copied on the far
//! end (something no local backend can do either) simply is not promised.

use std::collections::HashMap;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use view_core::msg::{RegisterType, ReplyToken, ReplyValue};
use view_engine::handle::EngineError;
use view_native::clipboard::{lines_to_text, text_to_lines};

use crate::engine_ops::EngineOps;

/// One clipboard request the loop has handed off, carrying the token its
/// reply must answer (see `Effect::ClipboardRead`/`Effect::ClipboardWrite`'s
/// docs for why the worker, not the loop, owns that obligation).
pub struct ClipboardJob {
    pub token: ReplyToken,
    /// Which engine this job's token belongs to
    /// ([`ReplyRoute::epoch`]). A `ReplyToken` is a bare msgid, and a fresh
    /// nvim starts its own msgids low again, so a job still in flight when
    /// the engine is replaced holds a number that names a live request on
    /// the replacement -- an unrelated one. Stamped by the producer, not
    /// read by the worker at reply time, because the worker cannot tell a
    /// job queued before the swap from one queued after it.
    pub epoch: u64,
    pub kind: ClipboardJobKind,
}

/// The two operations a `g:clipboard` provider call can ask this worker
/// for. `register` is carried on both rather than assumed: `'+'` and `'*'`
/// share one backend (see `Effect::ClipboardRead`'s doc for why), but the
/// shadow fallback keeps them as separate entries, so a design that later
/// gave them distinct backends would not have to replumb this job shape.
/// `Write` carries the copy's [`RegisterType`] alongside its lines: the
/// system clipboard has no field of its own for it, so it must ride here
/// to reach [`lines_to_text`]'s trailing-newline convention (see
/// `view_native::clipboard`'s module doc).
pub enum ClipboardJobKind {
    Read {
        register: char,
    },
    Write {
        register: char,
        lines: Vec<String>,
        regtype: RegisterType,
    },
}

/// The one capability this worker needs of a system clipboard, factored out
/// of `arboard::Clipboard` so a test can drive `run`'s exact logic --
/// including the unreachable-vs-reachable-but-failed distinction
/// `read_lines` branches on -- against a fake that does not depend on a
/// host display, rather than only against the reply-exactly-once contract
/// `spawn`'s own tests can prove without one.
///
/// The error carries `arboard`'s own message rather than collapsing to
/// `()`: both branches that see it (`read_lines`, `write_system`) treat a
/// failure the same way regardless of cause (fall back to the shadow, or
/// drop the write), but a run under `VIEW_LOG` needs the real reason --
/// "no display" and "clipboard holds non-text content" look identical to
/// this worker and identical to `()`, but are two different bugs to a user
/// debugging why `"+p` came back empty.
trait ClipboardBackend {
    fn get_text(&mut self) -> Result<String, String>;
    fn set_text(&mut self, text: String) -> Result<(), String>;
}

impl ClipboardBackend for arboard::Clipboard {
    fn get_text(&mut self) -> Result<String, String> {
        self.get_text().map_err(|err| err.to_string())
    }
    fn set_text(&mut self, text: String) -> Result<(), String> {
        self.set_text(text).map_err(|err| err.to_string())
    }
}

/// The engine connection this worker answers over, re-pointed rather than
/// rebuilt when the engine behind it is replaced.
///
/// The worker is session-scoped and the engine is not. Its shadow registers
/// are the entire clipboard on a host with no reachable display (see this
/// module's own doc), and a restart that started a second worker instead
/// would empty them -- while on a host that does have a display, the very
/// same copy survives a restart untouched, because the OS holds it. So on a
/// restart the handle moves and the thread stays.
///
/// A lock rather than a channel because the worker reads it once per job and
/// the loop writes it once per restart, and because
/// [`EngineHandle::reply`](view_engine::handle::EngineHandle::reply) never
/// blocks (it hands bytes to the outbox and returns), so the loop thread
/// cannot be held here by a wedged engine.
pub struct ReplyRoute<E: EngineOps> {
    ops: std::sync::Arc<std::sync::Mutex<E>>,
    epoch: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl<E: EngineOps> Clone for ReplyRoute<E> {
    fn clone(&self) -> Self {
        Self {
            ops: std::sync::Arc::clone(&self.ops),
            epoch: std::sync::Arc::clone(&self.epoch),
        }
    }
}

#[cfg(all(test, unix))]
impl ReplyRoute<view_engine::handle::EngineHandle> {
    /// Whether this route currently names a connection that is still open.
    ///
    /// The one externally observable fact about which engine a route holds,
    /// and enough to tell a route that followed a restart from one left
    /// pointing at the engine that died: the dead one's handle is closed and
    /// its replacement's is not.
    pub(crate) fn addresses_a_live_connection(&self) -> bool {
        self.ops.lock().is_ok_and(|ops| !ops.is_closed())
    }
}

impl<E: EngineOps> ReplyRoute<E> {
    /// A route answering over `ops`.
    pub fn new(ops: E) -> Self {
        Self {
            ops: std::sync::Arc::new(std::sync::Mutex::new(ops)),
            epoch: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Which engine this route currently answers over, counted from zero and
    /// stepped by every [`rebind`](Self::rebind).
    ///
    /// Stamped onto each [`ClipboardJob`] as it is queued, so a reply can be
    /// checked against the connection its token was issued on rather than
    /// written blind to whichever connection is current when it comes back.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Points every later reply at `ops` instead, for the engine that has
    /// replaced the one this route was built on.
    pub fn rebind(&self, ops: E) {
        match self.ops.lock() {
            Ok(mut held) => {
                *held = ops;
                // published under the same lock the reply path takes, so a
                // worker mid-reply either answers the old engine before the
                // swap or reads the stepped epoch after it, never the new
                // engine with the old epoch's blessing
                self.epoch.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            }
            // a poisoned route means the loop thread panicked mid-swap, and
            // the process is already coming down; the worker's replies fail
            // the same way they would against a closed connection
            Err(_) => crate::vlog::log("clipboard", "reply route poisoned; not rebound"),
        }
    }

    /// Answers `token` over the engine this route names, if `epoch` still
    /// names that engine.
    ///
    /// A stale epoch is dropped rather than written: the request this token
    /// answers died with its connection, and nvim already failed it. Writing
    /// it anyway would answer whatever request the replacement happens to
    /// have open under the same msgid -- a blocked `g:clipboard` call
    /// answered with another register's text, or a `VimEnter` answered with
    /// a list of lines.
    fn reply(&self, epoch: u64, token: ReplyToken, value: ReplyValue) -> Result<(), EngineError> {
        match self.ops.lock() {
            Ok(ops) => {
                if epoch != self.epoch() {
                    crate::vlog::log_with("clipboard", || {
                        format!(
                            "dropped a reply for msgid {} from engine epoch {epoch} (now {})",
                            token.msgid,
                            self.epoch()
                        )
                    });
                    return Ok(());
                }
                ops.reply(token, value)
            }
            Err(_) => Err(EngineError::Closed),
        }
    }
}

/// Spawns the clipboard worker and returns its handle; the caller (`run`'s
/// setup) keeps the `JoinHandle` alive for the session's duration but never
/// joins it -- the thread runs until `jobs`'s sender side (owned by the
/// `Executor`) is dropped at process exit, same lifetime as the writer and
/// reader threads `Engine::spawn` starts.
///
/// Generic over [`EngineOps`] rather than the concrete `EngineHandle`: the
/// only capability this thread needs of its engine connection is `reply`,
/// and taking the same trait `Executor` is generic over lets a test drive
/// the reply-exactly-once contract this function owns against a recording
/// fake instead of a live nvim connection.
///
/// # Errors
///
/// Returns the underlying `std::io::Error` if the OS cannot start the
/// thread.
pub fn spawn<E: EngineOps + Send + 'static>(
    route: ReplyRoute<E>,
    jobs: mpsc::Receiver<ClipboardJob>,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("view-clipboard".to_owned())
        .spawn(move || run(&route, &jobs, || arboard::Clipboard::new().ok()))
}

/// The worker's body: one register-keyed shadow map, one lazily-created
/// backend (see `ensure_clip`'s doc for why it must outlive any single
/// job), both live for the thread's whole lifetime, and one reply per job
/// -- the loop's one-reply-per-token invariant (see `EngineRequest`'s doc)
/// is this function's contract to keep, since `update()` has already
/// handed both obligations here and has no further chance to answer them
/// itself.
///
/// Generic over [`ClipboardBackend`] via an injected `connect` closure
/// rather than calling `arboard::Clipboard::new()` directly: `spawn` is the
/// only caller that needs the real backend, and a test supplying a fake
/// instead is the only way to prove `read_lines`'s unreachable-vs-failed
/// branches without a host display.
fn run<E: EngineOps, C: ClipboardBackend>(
    route: &ReplyRoute<E>,
    jobs: &mpsc::Receiver<ClipboardJob>,
    connect: impl Fn() -> Option<C>,
) {
    let mut shadow: HashMap<char, String> = HashMap::new();
    let mut clip: Option<C> = connect();
    while let Ok(job) = jobs.recv() {
        match job.kind {
            ClipboardJobKind::Read { register } => {
                let (lines, regtype) = read_lines(&mut clip, &connect, register, &shadow);
                // an EngineError here means the engine connection is
                // already gone (the writer thread exited), and the token
                // it answered belonged to that engine: a restart brings up
                // a connection with no memory of this request, so there is
                // nothing to retry against. The paint loop's own
                // EngineLost/supervision path is what notices the
                // connection is down, not this reply
                let _ = route.reply(
                    job.epoch,
                    job.token,
                    ReplyValue::ClipboardLines { lines, regtype },
                );
            }
            ClipboardJobKind::Write {
                register,
                lines,
                regtype,
            } => {
                let text = lines_to_text(&lines, regtype);
                write_system(&mut clip, &connect, &text);
                shadow.insert(register, text);
                let _ = route.reply(job.epoch, job.token, ReplyValue::Nil);
            }
        }
    }
}

/// Returns the live backend, retrying `connect()` if the worker started
/// before a display was reachable and none has been claimed yet. Once a
/// connection exists it is never dropped and re-opened between jobs: on
/// X11, dropping the last non-global `Clipboard` handle (this worker's own
/// instance, once no other thread in the process holds one) tears the
/// whole clipboard connection down -- destroys the selection window and
/// hands the data to a clipboard manager to persist it, which no manager is
/// running to receive under a bare Xvfb/CI/SSH session -- so a fresh
/// instance per call would silently erase whatever it had just written
/// before any reader, including this same thread's own next read, could
/// observe it.
fn ensure_clip<'a, C: ClipboardBackend>(
    clip: &'a mut Option<C>,
    connect: &impl Fn() -> Option<C>,
) -> Option<&'a mut C> {
    if clip.is_none() {
        *clip = connect();
    }
    clip.as_mut()
}

/// Reads the system clipboard, falling back to `shadow`'s entry for
/// `register` only when the clipboard is unreachable at all (`ensure_clip`
/// returns `None`, e.g. no display) -- see the module doc's remote-paste
/// contract for why a fallback, not an error, is correct in exactly that
/// case. A clipboard that *is* reachable but fails the read for any other
/// reason (non-text content, e.g. an image copied in a browser) must not
/// fall back to the shadow: that would silently resurrect this session's
/// own last yank as the answer to a paste of something else entirely,
/// which is a stale read, not a missing one -- it degrades to no lines
/// instead, matching what a real clipboard with nothing pasteable in it
/// looks like.
fn read_lines<C: ClipboardBackend>(
    clip: &mut Option<C>,
    connect: &impl Fn() -> Option<C>,
    register: char,
    shadow: &HashMap<char, String>,
) -> (Vec<String>, RegisterType) {
    match ensure_clip(clip, connect) {
        Some(clip) => match clip.get_text() {
            Ok(text) => text_to_lines(&text),
            Err(err) => {
                crate::vlog::log_with("clipboard", || format!("read failed: {err}"));
                (Vec::new(), RegisterType::Charwise)
            }
        },
        None => shadow
            .get(&register)
            .map(|text| text_to_lines(text))
            .unwrap_or((Vec::new(), RegisterType::Charwise)),
    }
}

/// Writes `text` to the system clipboard. Failure (no reachable backend,
/// e.g. no display) is silent: the shadow register the caller updates
/// regardless is what keeps `"+p` working in exactly that case, and there
/// is nowhere to report a clipboard failure that both this background
/// thread and a headless remote session could reach anyway.
fn write_system<C: ClipboardBackend>(
    clip: &mut Option<C>,
    connect: &impl Fn() -> Option<C>,
    text: &str,
) {
    if let Some(clip) = ensure_clip(clip, connect) {
        if let Err(err) = clip.set_text(text.to_owned()) {
            crate::vlog::log_with("clipboard", || format!("write failed: {err}"));
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// A [`ClipboardBackend`] a test can drive without a host display:
    /// `unreachable()` models `connect()` returning `None` (no display, the
    /// worker's own `ensure_clip` never gets a backend at all); `reachable`
    /// models a backend that exists but whose read can still fail on its
    /// own terms (e.g. non-text content) independently of reachability --
    /// the exact distinction finding 2's bug collapsed.
    #[derive(Clone)]
    struct FakeClipboard {
        text: Option<String>,
        fail_get: bool,
    }

    impl FakeClipboard {
        fn reachable(text: Option<&str>) -> Self {
            Self {
                text: text.map(str::to_owned),
                fail_get: false,
            }
        }

        fn reachable_but_unreadable() -> Self {
            Self {
                text: None,
                fail_get: true,
            }
        }
    }

    impl ClipboardBackend for FakeClipboard {
        fn get_text(&mut self) -> Result<String, String> {
            if self.fail_get {
                Err("fake read failure".to_owned())
            } else {
                self.text
                    .clone()
                    .ok_or_else(|| "fake clipboard is empty".to_owned())
            }
        }
        fn set_text(&mut self, text: String) -> Result<(), String> {
            self.text = Some(text);
            Ok(())
        }
    }

    fn unreachable_connect() -> Option<FakeClipboard> {
        None
    }

    #[test]
    fn ensure_clip_lazily_connects_once_and_reuses_the_same_backend() {
        let calls = std::cell::Cell::new(0);
        let connect = || {
            calls.set(calls.get() + 1);
            Some(FakeClipboard::reachable(Some("first")))
        };
        let mut clip: Option<FakeClipboard> = None;
        assert!(ensure_clip(&mut clip, &connect).is_some());
        assert!(ensure_clip(&mut clip, &connect).is_some());
        assert_eq!(
            calls.get(),
            1,
            "an already-live backend must never be reconnected between calls \
             (see ensure_clip's doc: a fresh instance per call can erase an \
             X11 selection before any reader observes it)"
        );
    }

    #[test]
    fn an_unreachable_clipboard_with_no_shadow_entry_reads_no_lines() {
        let shadow: HashMap<char, String> = HashMap::new();
        let mut clip: Option<FakeClipboard> = None;
        let (lines, regtype) = read_lines(&mut clip, &unreachable_connect, '+', &shadow);
        assert!(lines.is_empty());
        assert_eq!(regtype, RegisterType::Charwise);
    }

    /// The unreachable case (no display, `ensure_clip` returns `None`) is
    /// the one case a shadow fallback is correct for -- the SSH-with-no-
    /// forwarded-display scenario the module doc's remote-paste contract
    /// exists for.
    #[test]
    fn an_unreachable_clipboard_falls_back_to_its_own_shadow_entry() {
        let mut shadow: HashMap<char, String> = HashMap::new();
        shadow.insert(
            '+',
            lines_to_text(&["yanked".to_owned()], RegisterType::Charwise),
        );
        let mut clip: Option<FakeClipboard> = None;
        let (lines, regtype) = read_lines(&mut clip, &unreachable_connect, '+', &shadow);
        assert_eq!(lines, vec!["yanked"]);
        assert_eq!(regtype, RegisterType::Charwise);
    }

    /// A clipboard that *is* reachable but fails the read for its own
    /// reason (e.g. an image copied in a browser, which decodes to no
    /// text) must not fall back to a stale shadow entry from this
    /// session's last yank -- doing so silently answers a paste of
    /// something else entirely with the wrong text instead of reporting
    /// nothing pasteable.
    #[test]
    fn a_reachable_but_unreadable_clipboard_does_not_fall_back_to_a_stale_shadow_entry() {
        let mut shadow: HashMap<char, String> = HashMap::new();
        shadow.insert(
            '+',
            lines_to_text(&["stale session yank".to_owned()], RegisterType::Charwise),
        );
        let mut clip = Some(FakeClipboard::reachable_but_unreadable());
        let connect = || Some(FakeClipboard::reachable_but_unreadable());
        let (lines, regtype) = read_lines(&mut clip, &connect, '+', &shadow);
        assert!(
            lines.is_empty(),
            "a reachable-but-unreadable clipboard must read as empty, not resurrect \
             the shadow's stale entry: got {lines:?}"
        );
        assert_eq!(regtype, RegisterType::Charwise);
    }

    #[test]
    fn a_reachable_clipboard_with_text_ignores_the_shadow_entirely() {
        let mut shadow: HashMap<char, String> = HashMap::new();
        shadow.insert(
            '+',
            lines_to_text(&["stale".to_owned()], RegisterType::Charwise),
        );
        let mut clip = Some(FakeClipboard::reachable(Some("fresh\n")));
        let connect = || Some(FakeClipboard::reachable(Some("fresh\n")));
        let (lines, regtype) = read_lines(&mut clip, &connect, '+', &shadow);
        assert_eq!(lines, vec!["fresh"]);
        assert_eq!(regtype, RegisterType::Linewise);
    }

    #[test]
    fn write_system_writes_through_to_a_reachable_backend() {
        let mut clip = Some(FakeClipboard::reachable(None));
        let connect = || Some(FakeClipboard::reachable(None));
        write_system(&mut clip, &connect, "hello\n");
        assert_eq!(clip.unwrap().text.as_deref(), Some("hello\n"));
    }

    #[test]
    fn write_system_to_an_unreachable_clipboard_is_silent_and_never_panics() {
        let mut clip: Option<FakeClipboard> = None;
        write_system(&mut clip, &unreachable_connect, "hello");
        assert!(clip.is_none());
    }

    #[test]
    fn write_then_shadow_read_round_trips_without_a_display() {
        let mut shadow: HashMap<char, String> = HashMap::new();
        shadow.insert(
            '+',
            lines_to_text(
                &["hello".to_owned(), "world".to_owned()],
                RegisterType::Charwise,
            ),
        );
        let (lines, regtype) = read_lines(&mut None, &unreachable_connect, '+', &shadow);
        assert_eq!(lines, vec!["hello", "world"]);
        assert_eq!(regtype, RegisterType::Charwise);
    }

    /// An [`EngineOps`] whose only live method is `reply`: every other
    /// method is unreachable from this worker's own logic, so this fake
    /// exists to observe the one call the worker thread's reply obligation
    /// actually needs proof of, over a channel a test thread can wait on
    /// with a bound instead of joining a loop that never exits on its own.
    struct ReplyRecorder {
        tx: mpsc::Sender<(u64, ReplyValue)>,
    }

    impl EngineOps for ReplyRecorder {
        fn input(&self, _notation: &str) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn try_resize(
            &self,
            _width: u16,
            _height: u16,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn paste(&self, _text: &str) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn input_mouse(
            &self,
            _button: &str,
            _action: &str,
            _modifier: &str,
            _row: u16,
            _col: u16,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn set_option(
            &self,
            _name: &str,
            _value: &view_core::msg::OptionValue,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn hold_option(
            &self,
            _name: &str,
            _value: &view_core::msg::OptionValue,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn reply(
            &self,
            token: ReplyToken,
            value: ReplyValue,
        ) -> Result<(), view_engine::handle::EngineError> {
            let _ = self.tx.send((token.msgid, value));
            Ok(())
        }
        fn probe_default_hl(
            &self,
            _generation: u64,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn probe_swap_recovery(
            &self,
            _generation: u64,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn redraw(&self) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn register_mappings(
            &self,
            _specs: &[view_core::native::mappings::MappingSpec],
            _channel_id: u64,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn register_bridge(
            &self,
            _channel_id: u64,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn register_clipboard(
            &self,
            _channel_id: u64,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn list_buffers(&self, _generation: u64) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn preview_buffer(
            &self,
            _path: &str,
            _generation: u64,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn open_file(&self, _path: &str) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn rename_file(
            &self,
            _old_path: &str,
            _new_path: &str,
            _generation: u64,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn tree_create_prompt(
            &self,
            _generation: u64,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn tree_rename_prompt(
            &self,
            _old_path: &str,
            _current_name: &str,
            _generation: u64,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn tree_delete_confirm(
            &self,
            _path: &str,
            _generation: u64,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn set_buf_text(
            &self,
            _buf: view_core::msg::BufferHandle,
            _edits: &[view_core::msg::TextEdit],
            _undojoin: bool,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn buf_attach(
            &self,
            _buf: view_core::msg::BufferHandle,
            _generation: u64,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn buf_detach(
            &self,
            _buf: view_core::msg::BufferHandle,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn read_current_buffer_text(
            &self,
        ) -> Result<
            view_core::native::ai_context::CurrentBufferRead,
            view_engine::handle::EngineError,
        > {
            Ok(view_core::native::ai_context::CurrentBufferRead::new(
                std::path::PathBuf::new(),
                String::new(),
            ))
        }
        fn read_cursor_context(
            &self,
        ) -> Result<
            (
                view_core::native::ai_context::CursorRead,
                Option<view_core::native::ai_context::SelectionRead>,
            ),
            view_engine::handle::EngineError,
        > {
            Ok((view_core::native::ai_context::CursorRead::new(0, 0), None))
        }
        fn read_diagnostic_entries(
            &self,
        ) -> Result<
            Vec<view_core::native::ai_context::DiagnosticEntry>,
            view_engine::handle::EngineError,
        > {
            Ok(Vec::new())
        }
        fn read_quickfix_entries(
            &self,
        ) -> Result<
            Vec<view_core::native::ai_context::QuickfixEntry>,
            view_engine::handle::EngineError,
        > {
            Ok(Vec::new())
        }
    }

    #[test]
    fn a_read_job_answers_its_token_exactly_once_within_a_bounded_wait() {
        let (job_tx, job_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        let worker = spawn(ReplyRoute::new(ReplyRecorder { tx: reply_tx }), job_rx).unwrap();
        job_tx
            .send(ClipboardJob {
                token: ReplyToken { msgid: 42 },
                epoch: 0,
                kind: ClipboardJobKind::Read { register: '+' },
            })
            .unwrap();

        // a bounded wait, not a join on a loop that runs until its sender
        // is dropped: a worker that silently swallowed the job must fail
        // this test with a named timeout, never hang it
        let (msgid, value) = reply_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("a read job must answer its token within 2s, not block the engine forever");
        assert_eq!(msgid, 42, "the reply must answer the token the job carried");
        assert!(matches!(value, ReplyValue::ClipboardLines { .. }));
        assert!(
            reply_rx.try_recv().is_err(),
            "a read job must reply exactly once, never twice"
        );

        drop(job_tx);
        let _ = worker.join();
    }

    #[test]
    fn a_write_job_answers_its_token_exactly_once_within_a_bounded_wait() {
        let (job_tx, job_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        let worker = spawn(ReplyRoute::new(ReplyRecorder { tx: reply_tx }), job_rx).unwrap();
        job_tx
            .send(ClipboardJob {
                token: ReplyToken { msgid: 7 },
                epoch: 0,
                kind: ClipboardJobKind::Write {
                    register: '+',
                    lines: vec!["hello".to_owned()],
                    regtype: RegisterType::Charwise,
                },
            })
            .unwrap();

        let (msgid, value) = reply_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("a write job must answer its token within 2s, not block the engine forever");
        assert_eq!(msgid, 7, "the reply must answer the token the job carried");
        assert!(matches!(value, ReplyValue::Nil));
        assert!(
            reply_rx.try_recv().is_err(),
            "a write job must reply exactly once, never twice"
        );

        drop(job_tx);
        let _ = worker.join();
    }

    /// `spawn`'s own tests above only ever exercise the real `arboard`
    /// backend, whose reachability depends on the host's display -- proof
    /// of the reply-exactly-once contract, but not of `run`'s own
    /// unreachable-vs-reachable-but-failed branching (that is what
    /// `read_lines`'s own tests above prove directly). This helper drives
    /// `run` end to end, through the real job channel and `EngineOps`
    /// contract, with a `FakeClipboard` in place of `arboard` -- covering
    /// the seam between the two: that `run` actually calls `read_lines`/
    /// `write_system` with the connect closure it was given, and answers
    /// with the [`ReplyValue::ClipboardLines`] pair form the fake's own
    /// text and regtype produced.
    fn spawn_fake<E: EngineOps + Send + 'static>(
        route: ReplyRoute<E>,
        jobs: mpsc::Receiver<ClipboardJob>,
        connect: impl Fn() -> Option<FakeClipboard> + Send + 'static,
    ) -> JoinHandle<()> {
        thread::spawn(move || run(&route, &jobs, connect))
    }

    #[test]
    fn a_read_job_against_a_reachable_fake_replies_with_its_lines_and_regtype() {
        let (job_tx, job_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        let worker = spawn_fake(
            ReplyRoute::new(ReplyRecorder { tx: reply_tx }),
            job_rx,
            || Some(FakeClipboard::reachable(Some("hi\nthere\n"))),
        );
        job_tx
            .send(ClipboardJob {
                token: ReplyToken { msgid: 1 },
                epoch: 0,
                kind: ClipboardJobKind::Read { register: '+' },
            })
            .unwrap();

        let (_msgid, value) = reply_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("a read job must answer within 2s");
        assert_eq!(
            value,
            ReplyValue::ClipboardLines {
                lines: vec!["hi".to_owned(), "there".to_owned()],
                regtype: RegisterType::Linewise,
            }
        );

        drop(job_tx);
        let _ = worker.join();
    }

    /// The end-to-end version of finding 2's regression test: a write
    /// populates the shadow while the backend is unreachable, and the very
    /// next read on the same register must answer from that shadow -- the
    /// worker's real job-handling loop, not just `read_lines` called
    /// directly.
    #[test]
    fn a_write_then_read_through_an_unreachable_backend_round_trips_via_the_shadow() {
        let (job_tx, job_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        let worker = spawn_fake(
            ReplyRoute::new(ReplyRecorder { tx: reply_tx }),
            job_rx,
            || None,
        );
        job_tx
            .send(ClipboardJob {
                token: ReplyToken { msgid: 1 },
                epoch: 0,
                kind: ClipboardJobKind::Write {
                    register: '+',
                    lines: vec!["shadowed".to_owned()],
                    regtype: RegisterType::Charwise,
                },
            })
            .unwrap();
        reply_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the write must reply within 2s");

        job_tx
            .send(ClipboardJob {
                token: ReplyToken { msgid: 2 },
                epoch: 0,
                kind: ClipboardJobKind::Read { register: '+' },
            })
            .unwrap();
        let (_msgid, value) = reply_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the read must reply within 2s");
        assert_eq!(
            value,
            ReplyValue::ClipboardLines {
                lines: vec!["shadowed".to_owned()],
                regtype: RegisterType::Charwise,
            }
        );

        drop(job_tx);
        let _ = worker.join();
    }

    /// What a restart must not cost a user on a host with no reachable
    /// display: the shadow registers are the whole clipboard there, and
    /// they live in the worker thread. A second worker per engine would
    /// answer the next `"+p` with nothing, while the same copy on a host
    /// that has a display would still be sitting in the OS clipboard.
    #[test]
    fn a_rebound_route_answers_the_new_engine_and_keeps_the_registers_it_held() {
        let (job_tx, job_rx) = mpsc::channel();
        let (first_tx, first_rx) = mpsc::channel();
        let route = ReplyRoute::new(ReplyRecorder { tx: first_tx });
        let worker = spawn_fake(route.clone(), job_rx, || None);
        job_tx
            .send(ClipboardJob {
                token: ReplyToken { msgid: 1 },
                epoch: 0,
                kind: ClipboardJobKind::Write {
                    register: '+',
                    lines: vec!["copied before the crash".to_owned()],
                    regtype: RegisterType::Charwise,
                },
            })
            .unwrap();
        first_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the write must reply within 2s");

        let (second_tx, second_rx) = mpsc::channel();
        route.rebind(ReplyRecorder { tx: second_tx });
        job_tx
            .send(ClipboardJob {
                token: ReplyToken { msgid: 2 },
                epoch: route.epoch(),
                kind: ClipboardJobKind::Read { register: '+' },
            })
            .unwrap();

        let (msgid, value) = second_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the read must answer the engine the route now names, within 2s");
        assert_eq!(msgid, 2);
        assert_eq!(
            value,
            ReplyValue::ClipboardLines {
                lines: vec!["copied before the crash".to_owned()],
                regtype: RegisterType::Charwise,
            },
            "a restart must not empty the registers the worker already held"
        );
        assert!(
            first_rx.try_recv().is_err(),
            "no reply may reach the engine the route was pointed away from"
        );

        drop(job_tx);
        let _ = worker.join();
    }

    /// The hazard a bare msgid cannot see: a job queued against the dead
    /// engine reaches the worker after the swap, and its `ReplyToken` names
    /// a msgid the *replacement* is just as likely to have open -- msgids
    /// start low again on every fresh connection. Answering it would hand
    /// some unrelated blocked request another register's text.
    #[test]
    fn a_job_stamped_by_the_dead_engine_is_never_answered_by_its_replacement() {
        let (job_tx, job_rx) = mpsc::channel();
        let (first_tx, _first_rx) = mpsc::channel();
        let route = ReplyRoute::new(ReplyRecorder { tx: first_tx });
        let worker = spawn_fake(route.clone(), job_rx, || None);

        let stale = ClipboardJob {
            token: ReplyToken { msgid: 4 },
            epoch: route.epoch(),
            kind: ClipboardJobKind::Read { register: '+' },
        };
        let (second_tx, second_rx) = mpsc::channel();
        route.rebind(ReplyRecorder { tx: second_tx });
        job_tx.send(stale).unwrap();
        job_tx
            .send(ClipboardJob {
                token: ReplyToken { msgid: 4 },
                epoch: route.epoch(),
                kind: ClipboardJobKind::Read { register: '*' },
            })
            .unwrap();

        let (msgid, _value) = second_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the replacement's own job must still answer, within 2s");
        assert_eq!(msgid, 4);
        assert!(
            second_rx.try_recv().is_err(),
            "the stale job's reply must be dropped, not written to the \
             connection that inherited its msgid"
        );

        drop(job_tx);
        let _ = worker.join();
    }
}
