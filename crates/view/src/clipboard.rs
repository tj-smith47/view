//! The clipboard worker: the one thread that touches the system clipboard,
//! off the paint loop. The loop only ever hands a read or write across a
//! channel and never blocks on the answer itself; this thread owns the
//! reply obligation for both.
//!
//! # The remote-paste contract
//!
//! Nothing here ever asks a terminal what is on its clipboard. OSC 52
//! paste-back would require the terminal to answer a query escape, which
//! most emulators refuse by default for the obvious security reason (an
//! arbitrary program reading the system clipboard without a user gesture),
//! so it is not a mechanism this worker can lean on. Instead, every copy --
//! [`ClipboardJobKind::Write`] through view's own provider, or
//! [`ClipboardJobKind::Store`] mirroring one nvim's provider performed --
//! updates an in-memory shadow register alongside the real system-clipboard
//! write, and a read falls back to that shadow whenever `arboard` itself
//! cannot reach a clipboard, which is exactly the situation an SSH session
//! with no forwarded display is in. That matches the behavior every remote
//! nvim setup already has: `"+p` after `"+yy` works across the same
//! session, and reading a value copied on the far end (something no local
//! backend can do either) simply is not promised.
//!
//! Both providers reach the same state, which is the point of the `Store`
//! and `Query` pair: a user whose own `g:clipboard` names nvim's OSC 52
//! provider gets the identical paste semantics as one who left the slot for
//! view, rather than a second set that depends on which provider won.

use std::collections::HashMap;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use view_core::msg::{RegisterType, ReplyToken, ReplyValue};
use view_engine::handle::EngineError;
use view_native::clipboard::{lines_to_text, text_to_lines};

use crate::engine_ops::EngineOps;

/// One clipboard request the loop has handed off. The obligation each kind
/// carries rides inside it: the two that answer a blocked nvim request carry
/// the token they must answer (see
/// `Effect::ClipboardRead`/`Effect::ClipboardWrite`'s docs for why the
/// worker, not the loop, owns that obligation), and the two that serve
/// nvim's own OSC 52 provider carry none, because nothing on the wire is
/// blocked on them.
pub struct ClipboardJob {
    /// Which engine this job belongs to
    /// ([`ReplyRoute::epoch`]). A `ReplyToken` is a bare msgid, and a fresh
    /// nvim starts its own msgids low again, so a job still in flight when
    /// the engine is replaced holds a number that names a live request on
    /// the replacement -- an unrelated one. Stamped by the producer, not
    /// read by the worker at answer time, because the worker cannot tell a
    /// job queued before the swap from one queued after it.
    pub epoch: u64,
    pub kind: ClipboardJobKind,
}

/// What this worker can be asked for. `register` is carried on every kind
/// rather than assumed: `'+'` and `'*'` share one backend (see
/// `Effect::ClipboardRead`'s doc for why), but the shadow fallback keeps
/// them as separate entries, so a design that later gave them distinct
/// backends would not have to replumb this job shape.
///
/// The two halves differ in who is waiting. [`Read`](Self::Read) and
/// [`Write`](Self::Write) answer a `g:clipboard` provider call view itself
/// registered, so each carries the [`ReplyToken`] of the nvim request
/// blocked on it -- inside the variant rather than beside it, so a kind
/// with nobody to answer cannot be handed one, and a kind that owes an
/// answer cannot be built without it. [`Store`](Self::Store) and
/// [`Query`](Self::Query) serve nvim's *own* OSC 52 provider, which blocks
/// on no request at all (see `Effect::ClipboardStore` and
/// `Effect::ClipboardQuery`), and carry text rather than lines because an
/// OSC 52 escape carries no regtype field to split out.
pub enum ClipboardJobKind {
    Read {
        token: ReplyToken,
        register: char,
    },
    /// `Write` carries the copy's [`RegisterType`] alongside its lines: the
    /// system clipboard has no field of its own for it, so it must ride
    /// here to reach [`lines_to_text`]'s trailing-newline convention (see
    /// `view_native::clipboard`'s module doc).
    Write {
        token: ReplyToken,
        register: char,
        lines: Vec<String>,
        regtype: RegisterType,
    },
    Store {
        register: char,
        text: String,
    },
    Query {
        register: char,
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

    /// Delivers `sequence` to the engine this route names as a terminal
    /// response, if `epoch` still names that engine.
    ///
    /// Epoch-checked for a milder version of [`reply`](Self::reply)'s
    /// reason: a `TermResponse` is broadcast to whatever autocommands the
    /// engine has, so an answer arriving at the *replacement* engine would
    /// hand it a clipboard reply nothing there ever asked for. The paste
    /// that did ask died with its connection.
    fn ui_term_event(&self, epoch: u64, sequence: &str) -> Result<(), EngineError> {
        match self.ops.lock() {
            Ok(ops) => {
                if epoch != self.epoch() {
                    crate::vlog::log_with("clipboard", || {
                        format!(
                            "dropped a term event from engine epoch {epoch} (now {})",
                            self.epoch()
                        )
                    });
                    return Ok(());
                }
                ops.ui_term_event(sequence)
            }
            Err(_) => Err(EngineError::Closed),
        }
    }
}

/// Queues `kind` on the worker's channel, or -- when no worker is wired
/// (every bare test `Executor`) or its thread is already gone -- discharges
/// the obligation the job carries inline, on the loop thread.
///
/// The fallbacks are the whole reason this is one function rather than four
/// effect arms. Each is the answer that costs the caller least while still
/// being an answer:
///
/// - [`Read`](ClipboardJobKind::Read) replies charwise-empty, what an
///   unreachable clipboard reads as, because the token must be answered
///   exactly once whatever happens to the worker;
/// - [`Write`](ClipboardJobKind::Write) replies `Nil`, for the same
///   exactly-once reason;
/// - [`Query`](ClipboardJobKind::Query) sends the empty OSC 52 payload,
///   because nvim is sitting in `vim.wait` for it and silence there costs a
///   one-second stall and then a hit-enter prompt that wedges a narrow
///   window (see `Effect::ClipboardQuery`);
/// - [`Store`](ClipboardJobKind::Store) alone degrades silently: nvim
///   already performed that copy and nothing is waiting on the mirror.
pub(crate) fn dispatch<E: EngineOps>(
    jobs: Option<&mpsc::Sender<ClipboardJob>>,
    ops: &E,
    epoch: u64,
    kind: ClipboardJobKind,
) {
    let undelivered = match jobs {
        Some(tx) => tx
            .send(ClipboardJob { epoch, kind })
            .err()
            .map(|err| err.0.kind),
        None => Some(kind),
    };
    let Some(kind) = undelivered else {
        return;
    };
    match kind {
        ClipboardJobKind::Read { token, .. } => {
            let _ = ops.reply(
                token,
                ReplyValue::ClipboardLines {
                    lines: Vec::new(),
                    regtype: RegisterType::Charwise,
                },
            );
        }
        ClipboardJobKind::Write { token, .. } => {
            let _ = ops.reply(token, ReplyValue::Nil);
        }
        ClipboardJobKind::Query { register } => {
            let _ = ops.ui_term_event(&view_core::osc52::clipboard_escape(register, ""));
        }
        ClipboardJobKind::Store { .. } => {}
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
fn run<E: EngineOps, C: ClipboardBackend + Send + 'static>(
    route: &ReplyRoute<E>,
    jobs: &mpsc::Receiver<ClipboardJob>,
    connect: impl Fn() -> Option<C>,
) {
    let mut shadow: HashMap<char, String> = HashMap::new();
    let mut clip: Option<C> = connect();
    while let Ok(job) = jobs.recv() {
        match job.kind {
            ClipboardJobKind::Read { token, register } => {
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
                    token,
                    ReplyValue::ClipboardLines { lines, regtype },
                );
            }
            ClipboardJobKind::Write {
                token,
                register,
                lines,
                regtype,
            } => {
                store(
                    &mut clip,
                    &connect,
                    &mut shadow,
                    register,
                    lines_to_text(&lines, regtype),
                );
                let _ = route.reply(job.epoch, token, ReplyValue::Nil);
            }
            ClipboardJobKind::Store { register, text } => {
                store(&mut clip, &connect, &mut shadow, register, text);
            }
            ClipboardJobKind::Query { register } => {
                // answered on every path and inside `READ_BUDGET` on every
                // path, which for this job are the same requirement: nvim's
                // provider is inside `vim.wait` for one second, so a late
                // answer costs exactly what silence does
                let text = read_text(&mut clip, &connect, register, &shadow);
                let _ = route.ui_term_event(
                    job.epoch,
                    &view_core::osc52::clipboard_escape(register, &text),
                );
            }
        }
    }
}

/// Puts `text` on the system clipboard and in `shadow`'s entry for
/// `register`, the pair every copy leaves behind whichever provider
/// performed it -- so a paste answers the same text back whether the host
/// has a reachable display or only this worker's own memory of the session.
fn store<C: ClipboardBackend>(
    clip: &mut Option<C>,
    connect: &impl Fn() -> Option<C>,
    shadow: &mut HashMap<char, String>,
    register: char,
    text: String,
) {
    write_system(clip, connect, &text);
    shadow.insert(register, text);
}

/// How long a system-clipboard read may take before this worker stops
/// waiting for it and answers from what it already has.
///
/// The number is set by the consumer with the tightest deadline, nvim's own
/// OSC 52 provider: it waits one second for a paste answer, and past that
/// echoes a notice that raises a hit-enter prompt -- an editor wedged until
/// a keystroke on any window under 100 columns (see
/// `Effect::ClipboardQuery`). So an answer is only an answer if it is back
/// well inside that second, and 250 ms leaves room for the RPC hop and the
/// provider's own polling granularity on top.
///
/// The bound is not theoretical: `arboard`'s X11 backend waits up to four
/// seconds for a slow or wedged selection owner, and its Wayland backend
/// reads the owner's pipe to EOF with no timeout at all. Both are ordinary
/// desktop configurations, and both are longer than the provider will wait.
const READ_BUDGET: std::time::Duration = std::time::Duration::from_millis(250);

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

/// [`read_text`] in the line-list-plus-regtype shape a `g:clipboard.paste`
/// reply takes, the trailing newline decoded back into a
/// [`RegisterType`] by [`text_to_lines`].
fn read_lines<C: ClipboardBackend + Send + 'static>(
    clip: &mut Option<C>,
    connect: &impl Fn() -> Option<C>,
    register: char,
    shadow: &HashMap<char, String>,
) -> (Vec<String>, RegisterType) {
    text_to_lines(&read_text(clip, connect, register, shadow))
}

/// Reads the system clipboard, falling back to `shadow`'s entry for
/// `register` only when the clipboard is unreachable at all (`ensure_clip`
/// returns `None`, e.g. no display) -- see the module doc's remote-paste
/// contract for why a fallback, not an error, is correct in exactly that
/// case. A clipboard that *is* reachable but fails the read for any other
/// reason (non-text content, e.g. an image copied in a browser) must not
/// fall back to the shadow: that would silently resurrect this session's
/// own last yank as the answer to a paste of something else entirely,
/// which is a stale read, not a missing one -- it degrades to the empty
/// string instead, matching what a real clipboard with nothing pasteable
/// in it looks like.
///
/// Text rather than lines because both callers want it that way in the
/// end: an OSC 52 answer carries text and has no regtype field to split
/// one out for (see `Effect::ClipboardQuery`), and [`read_lines`] recovers
/// the pair from the same string.
///
/// # The [`READ_BUDGET`] and what it costs
///
/// The backend call is made on a scratch thread and collected with a
/// timeout, because two of `arboard`'s three backends can outlast the
/// deadline the answer has to meet (see [`READ_BUDGET`]). Every caller
/// therefore returns inside the budget, whatever the host's clipboard
/// owner is doing.
///
/// A read that overruns is treated as an unreachable clipboard: the answer
/// falls back to `shadow`, which is the same degrade a host with no
/// display already takes, and `clip` is left `None` so the next job
/// reconnects. The backend goes with the stranded thread and is not
/// waited on -- the alternative is holding the worker's single job queue
/// open behind it, which would strand the `Read` and `Write` jobs behind
/// it too, each owing a reply token to an nvim blocked on `rpcrequest`. A
/// leaked thread on a connection that is already wedged is the cheaper
/// half of that trade.
///
/// ponytail: one orphaned thread and one dropped connection per overrun.
/// Fine while an overrun means a wedged clipboard owner, which is rare and
/// not self-inflicted; if a backend is ever found that overruns routinely,
/// the upgrade is one long-lived reader thread the worker hands requests
/// to, so a wedge costs one thread for the session rather than one per
/// read.
fn read_text<C: ClipboardBackend + Send + 'static>(
    clip: &mut Option<C>,
    connect: &impl Fn() -> Option<C>,
    register: char,
    shadow: &HashMap<char, String>,
) -> String {
    let from_shadow = || shadow.get(&register).cloned().unwrap_or_default();
    ensure_clip(clip, connect);
    let Some(mut backend) = clip.take() else {
        return from_shadow();
    };
    let (tx, rx) = mpsc::channel();
    if thread::Builder::new()
        .name("view-clipboard-read".to_owned())
        .spawn(move || {
            let read = backend.get_text();
            // the backend rides back with its answer so an in-budget read
            // keeps the one connection alive: reopening per read can erase
            // an X11 selection this worker itself just wrote (see
            // `ensure_clip`)
            let _ = tx.send((backend, read));
        })
        .is_err()
    {
        crate::vlog::log("clipboard", "could not start a read thread");
        return from_shadow();
    }
    match rx.recv_timeout(READ_BUDGET) {
        Ok((backend, read)) => {
            *clip = Some(backend);
            read.unwrap_or_else(|err| {
                crate::vlog::log_with("clipboard", || format!("read failed: {err}"));
                String::new()
            })
        }
        Err(_) => {
            crate::vlog::log_with("clipboard", || {
                format!("read exceeded {READ_BUDGET:?}; answering from the shadow register")
            });
            from_shadow()
        }
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
        /// How long `get_text` blocks before answering, standing in for
        /// `arboard`'s X11 four-second ceiling and its Wayland backend's
        /// unbounded pipe read -- the two configurations that can outlast
        /// the deadline a paste answer has to meet.
        get_delay: std::time::Duration,
    }

    impl FakeClipboard {
        fn reachable(text: Option<&str>) -> Self {
            Self {
                text: text.map(str::to_owned),
                fail_get: false,
                get_delay: std::time::Duration::ZERO,
            }
        }

        fn reachable_but_unreadable() -> Self {
            Self {
                text: None,
                fail_get: true,
                get_delay: std::time::Duration::ZERO,
            }
        }

        fn reachable_but_slow(delay: std::time::Duration) -> Self {
            Self {
                text: Some("the wedged owner finally answered".to_owned()),
                fail_get: false,
                get_delay: delay,
            }
        }
    }

    impl ClipboardBackend for FakeClipboard {
        fn get_text(&mut self) -> Result<String, String> {
            thread::sleep(self.get_delay);
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

    /// An [`EngineOps`] whose only live methods are `reply` and
    /// `ui_term_event`: every other method is unreachable from this
    /// worker's own logic, so this fake exists to observe the two calls the
    /// worker thread's answer obligation actually needs proof of, over
    /// channels a test thread can wait on with a bound instead of joining a
    /// loop that never exits on its own.
    struct ReplyRecorder {
        tx: mpsc::Sender<(u64, ReplyValue)>,
        terms: mpsc::Sender<String>,
    }

    impl ReplyRecorder {
        /// A recorder for a test that only watches replies. Its term-event
        /// channel has no receiver, so an unexpected answer there is
        /// dropped exactly as a closed engine connection would drop it,
        /// rather than deadlocking the worker.
        fn replies_only(tx: mpsc::Sender<(u64, ReplyValue)>) -> Self {
            Self {
                tx,
                terms: mpsc::channel().0,
            }
        }
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
        fn claim_stdout_tty(&self) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn ui_term_event(&self, sequence: &str) -> Result<(), view_engine::handle::EngineError> {
            let _ = self.terms.send(sequence.to_owned());
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
            _expected_changedtick: Option<u64>,
        ) -> Result<view_engine::nvim_api::BufWriteOutcome, view_engine::handle::EngineError>
        {
            Ok(view_engine::nvim_api::BufWriteOutcome::Applied { changedtick: 0 })
        }
        fn buf_attach(
            &self,
            _buf: view_core::msg::BufferHandle,
            _generation: u64,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn load_hidden(
            &self,
            _path: &str,
            _generation: u64,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn release_hidden(&self, _path: &str) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn ai_fs_read(
            &self,
            _request_id: u64,
            _buf: view_core::msg::BufferHandle,
            _line: Option<u32>,
            _limit: Option<u32>,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn ai_fs_write(
            &self,
            _request_id: u64,
            _buf: view_core::msg::BufferHandle,
            _lines: &[String],
            _eol: bool,
            _expected_changedtick: u64,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn buf_detach(
            &self,
            _buf: view_core::msg::BufferHandle,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn checktime(
            &self,
            _request_id: u64,
            _paths: &[String],
            _force: bool,
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
        let worker = spawn(
            ReplyRoute::new(ReplyRecorder::replies_only(reply_tx)),
            job_rx,
        )
        .unwrap();
        job_tx
            .send(ClipboardJob {
                epoch: 0,
                kind: ClipboardJobKind::Read {
                    token: ReplyToken { msgid: 42 },
                    register: '+',
                },
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
        let worker = spawn(
            ReplyRoute::new(ReplyRecorder::replies_only(reply_tx)),
            job_rx,
        )
        .unwrap();
        job_tx
            .send(ClipboardJob {
                epoch: 0,
                kind: ClipboardJobKind::Write {
                    token: ReplyToken { msgid: 7 },
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
            ReplyRoute::new(ReplyRecorder::replies_only(reply_tx)),
            job_rx,
            || Some(FakeClipboard::reachable(Some("hi\nthere\n"))),
        );
        job_tx
            .send(ClipboardJob {
                epoch: 0,
                kind: ClipboardJobKind::Read {
                    token: ReplyToken { msgid: 1 },
                    register: '+',
                },
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
            ReplyRoute::new(ReplyRecorder::replies_only(reply_tx)),
            job_rx,
            || None,
        );
        job_tx
            .send(ClipboardJob {
                epoch: 0,
                kind: ClipboardJobKind::Write {
                    token: ReplyToken { msgid: 1 },
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
                epoch: 0,
                kind: ClipboardJobKind::Read {
                    token: ReplyToken { msgid: 2 },
                    register: '+',
                },
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

    /// The whole `"+y` then `"+p` round trip through nvim's *own* OSC 52
    /// provider, at the layer where both halves meet.
    ///
    /// The store is not decoration: with a user's `g:clipboard` installed
    /// view's provider stands down, so `Write` never runs and this shadow
    /// would otherwise be empty for the entire session -- the query would
    /// answer the user's own yank with nothing. The answer's exact bytes
    /// matter too, because nvim's provider matches them with a Lua pattern
    /// and decodes the capture as base64.
    #[test]
    fn a_store_then_query_answers_the_copy_back_as_an_osc52_escape() {
        let (job_tx, job_rx) = mpsc::channel();
        let (reply_tx, _reply_rx) = mpsc::channel();
        let (term_tx, term_rx) = mpsc::channel();
        let worker = spawn_fake(
            ReplyRoute::new(ReplyRecorder {
                tx: reply_tx,
                terms: term_tx,
            }),
            job_rx,
            || None,
        );
        job_tx
            .send(ClipboardJob {
                epoch: 0,
                kind: ClipboardJobKind::Store {
                    register: '+',
                    text: "yanked\n".to_owned(),
                },
            })
            .unwrap();
        job_tx
            .send(ClipboardJob {
                epoch: 0,
                kind: ClipboardJobKind::Query { register: '+' },
            })
            .unwrap();

        let answer = term_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("a query must be answered within 2s, never left to nvim's own timeout");
        assert_eq!(answer, "\x1b]52;c;eWFua2VkCg==\x1b\\");
        assert!(
            term_rx.try_recv().is_err(),
            "a store must answer nothing of its own"
        );

        drop(job_tx);
        let _ = worker.join();
    }

    /// A backend slower than the provider will wait is answered *around*,
    /// not waited on. `arboard` gives X11 a four-second ceiling and Wayland
    /// none at all, both of which outlast the one second nvim's provider
    /// waits before echoing the notice that wedges a narrow window -- so an
    /// answer that arrives late is the very bug this job exists to prevent,
    /// and the shadow's copy delivered on time beats the real clipboard's
    /// delivered after the editor has already stalled.
    ///
    /// The second query is the other half of the finding: the worker is a
    /// serial queue, so a read that blocked it would strand every job
    /// behind it, `Read` and `Write` included -- and those owe reply tokens
    /// to an nvim blocked on `rpcrequest`.
    #[test]
    fn a_backend_slower_than_the_budget_is_answered_around_and_blocks_no_later_job() {
        let (job_tx, job_rx) = mpsc::channel();
        let (reply_tx, _reply_rx) = mpsc::channel();
        let (term_tx, term_rx) = mpsc::channel();
        let worker = spawn_fake(
            ReplyRoute::new(ReplyRecorder {
                tx: reply_tx,
                terms: term_tx,
            }),
            job_rx,
            || {
                Some(FakeClipboard::reachable_but_slow(
                    std::time::Duration::from_secs(4),
                ))
            },
        );
        job_tx
            .send(ClipboardJob {
                epoch: 0,
                kind: ClipboardJobKind::Store {
                    register: '+',
                    text: "yanked\n".to_owned(),
                },
            })
            .unwrap();
        for _ in 0..2 {
            job_tx
                .send(ClipboardJob {
                    epoch: 0,
                    kind: ClipboardJobKind::Query { register: '+' },
                })
                .unwrap();
        }

        let started = std::time::Instant::now();
        let first = term_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("a slow backend must not hold the answer past the provider's own wait");
        let second = term_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("a timed-out read must not strand the jobs queued behind it");
        let elapsed = started.elapsed();

        let from_shadow = view_core::osc52::clipboard_escape('+', "yanked\n");
        assert_eq!(first, from_shadow);
        assert_eq!(second, from_shadow);
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "two answers took {elapsed:?}; each read must cost at most READ_BUDGET"
        );

        drop(job_tx);
        let _ = worker.join();
    }

    /// Nothing to report is still an answer. Silence here is what the
    /// investigation measured as a one-second stall followed by a hit-enter
    /// prompt that wedges any window under 100 columns until a key is
    /// pressed, then nine seconds more; the empty payload satisfies the
    /// provider's own capture and returns at once.
    #[test]
    fn a_query_with_nothing_to_report_answers_an_empty_payload() {
        let (job_tx, job_rx) = mpsc::channel();
        let (reply_tx, _reply_rx) = mpsc::channel();
        let (term_tx, term_rx) = mpsc::channel();
        let worker = spawn_fake(
            ReplyRoute::new(ReplyRecorder {
                tx: reply_tx,
                terms: term_tx,
            }),
            job_rx,
            || None,
        );
        job_tx
            .send(ClipboardJob {
                epoch: 0,
                kind: ClipboardJobKind::Query { register: '*' },
            })
            .unwrap();

        let answer = term_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("an empty clipboard must still answer within 2s");
        assert_eq!(answer, "\x1b]52;p;\x1b\\");

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
        let route = ReplyRoute::new(ReplyRecorder::replies_only(first_tx));
        let worker = spawn_fake(route.clone(), job_rx, || None);
        job_tx
            .send(ClipboardJob {
                epoch: 0,
                kind: ClipboardJobKind::Write {
                    token: ReplyToken { msgid: 1 },
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
        route.rebind(ReplyRecorder::replies_only(second_tx));
        job_tx
            .send(ClipboardJob {
                epoch: route.epoch(),
                kind: ClipboardJobKind::Read {
                    token: ReplyToken { msgid: 2 },
                    register: '+',
                },
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
        let route = ReplyRoute::new(ReplyRecorder::replies_only(first_tx));
        let worker = spawn_fake(route.clone(), job_rx, || None);

        let stale = ClipboardJob {
            epoch: route.epoch(),
            kind: ClipboardJobKind::Read {
                token: ReplyToken { msgid: 4 },
                register: '+',
            },
        };
        let (second_tx, second_rx) = mpsc::channel();
        route.rebind(ReplyRecorder::replies_only(second_tx));
        job_tx.send(stale).unwrap();
        job_tx
            .send(ClipboardJob {
                epoch: route.epoch(),
                kind: ClipboardJobKind::Read {
                    token: ReplyToken { msgid: 4 },
                    register: '*',
                },
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
