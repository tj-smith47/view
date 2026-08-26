//! The startup sequence (see the design spec's startup-sequence section):
//! paint a themed placeholder shell
//! immediately from the cached theme, spawn the engine and attach on a
//! background thread so a slow-starting nvim can never delay that first
//! paint, buffer keys (and the latest resize) seen in the gap, and hand
//! everything back to `main.rs` once attach completes.
//!
//! [`paint_shell_frame`] happens entirely outside `runtime::run`'s
//! steady-state loop: nothing has set `Model::dirty` yet at this point,
//! since no `Flush` has ever arrived, so the loop's own `if model.dirty`
//! paint gate would never fire for this very first frame without an
//! explicit call here.
//!
//! [`attach_in_background`] deliberately never calls
//! [`Engine::start_pump`]: only `main.rs` does, strictly after
//! [`drain_pre_attach`] has already observed the [`Msg::EngineReady`]
//! marker this module sends. See `attach_in_background`'s doc comment for
//! the full ordering argument this depends on, and [`run_cutover`]'s doc
//! comment for how everything `Engine::start_pump` returns (rather than
//! sends) is resolved once a consumer of `msg_tx` is guaranteed to exist.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvError, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Instant;

use view_core::events::UiEvent;
use view_core::model::Model;
use view_core::msg::{Effect, Key, Msg};
use view_engine::handle::EngineError;
use view_engine::process::{Engine, EngineConfig};
use view_tui::terminal::Term;

/// How many pre-attach keystrokes [`drain_pre_attach`] holds before it
/// starts dropping the oldest to make room for the newest -- the design
/// spec's "bounded ring of 64".
const KEY_RING_CAPACITY: usize = 64;

/// `main.rs`'s `msg_tx`/`msg_rx` channel capacity. Deliberately tied to
/// [`KEY_RING_CAPACITY`] rather than stated as its own literal: a
/// maximally-full pre-attach key ring replays exactly `KEY_RING_CAPACITY`
/// messages onto this channel during cutover, and the two numbers drifting
/// apart would silently change the replay-hazard model
/// `runtime`'s `re_enqueueing_replayed_keys_onto_a_full_bounded_channel_with_no_consumer_blocks_forever`
/// test pins, with no compile error to catch it.
pub(crate) const MSG_CHANNEL_CAPACITY: usize = KEY_RING_CAPACITY;

/// A fixed-capacity FIFO of pre-attach keystrokes. [`push`](Self::push)
/// evicts the oldest entry once full rather than rejecting the newest: a
/// keystroke the user just typed is more likely to still matter than one
/// typed a while ago, and reports whether an eviction happened so the
/// caller can surface exactly one updated toast per drop instead of
/// staying silent about lost input.
struct KeyRing {
    buf: VecDeque<Key>,
    cap: usize,
}

impl KeyRing {
    fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap),
            cap,
        }
    }

    /// Returns `true` if an older entry was evicted to make room for `key`.
    fn push(&mut self, key: Key) -> bool {
        let evicted = if self.buf.len() >= self.cap {
            self.buf.pop_front();
            true
        } else {
            false
        };
        self.buf.push_back(key);
        evicted
    }

    /// Drains every buffered key, oldest first, for replay through the
    /// normal `Msg::Key` path once attach completes.
    fn drain(&mut self) -> Vec<Key> {
        self.buf.drain(..).collect()
    }
}

/// Paints the very first frame directly, bypassing `runtime::run`'s own
/// `if model.dirty` gate: nothing has set `dirty` yet this early in startup
/// (no `Flush` has ever arrived), so without this explicit call the
/// terminal would show whatever `Term::init` last left on screen until the
/// engine attaches and streams real content -- a blank screen for however
/// long attach takes, rather than an immediate themed placeholder. The caller must have already set
/// `model.content_painted = false` (`Model`'s default is `true`, the
/// ordinary steady state; startup is the one caller that opts into the
/// placeholder) for this frame to show the shell rather than an empty grid.
///
/// Logs the elapsed time since `process_start` under the `"startup"`
/// `VIEW_LOG` topic: the design spec's informal 50ms shell-paint target,
/// measured here but not enforced (the formal budget gate lands with the
/// bench harness). Routed through [`crate::vlog::log_with`] rather than a
/// bare stderr write, unlike `view-tui`'s own `tiers::log_caps`: that log
/// line runs before `Term::init` ever enters the alternate screen (raw mode
/// only, still the visible screen -- see `terminal.rs`'s own doc comment on
/// why its `\r\n` has to be explicit), while this one fires from inside
/// `paint_shell_frame`, called only after `Term::init` has entered both raw
/// mode and the alternate screen (see `main.rs`'s own call ordering); a
/// bare stderr write at this point lands inside the frame this call just
/// painted, not on any screen a developer could read it from. `vlog` is the
/// zero-overhead-when-unset channel every other startup measurement in
/// `main.rs` already uses for the same reason.
///
/// # Errors
///
/// Returns the underlying `std::io::Error` if the terminal write fails.
pub fn paint_shell_frame(
    term: &mut Term,
    model: &Model,
    process_start: Instant,
) -> std::io::Result<()> {
    let surface = view_surface::render(model);
    // the placeholder shell is always a whole-frame paint
    term.draw_surface(model, &surface, &view_core::grid::GridDamage::full())?;
    crate::vlog::log_with("startup", || {
        format!(
            "shell frame painted {:?} after process start",
            process_start.elapsed()
        )
    });
    Ok(())
}

/// Distinguishes a failure launching the `nvim` process at all (a missing
/// or broken `--nvim-bin`/`PATH` binary, most commonly) from a failure
/// during the post-spawn handshake/attach sequence against a process that
/// did launch, so `main.rs` can report each with its own actionable
/// context instead of one blanket "attach failed" message that could mean
/// either.
#[derive(Debug)]
pub enum AttachFailure {
    /// `Engine::spawn` itself failed: the process never started.
    Spawn(EngineError),
    /// The process started, but registering the `VimEnter` autocmd or
    /// `ui_attach` failed or timed out.
    Attach(EngineError),
}

/// Spawns `nvim --embed`, registers the `VimEnter` autocmd and the
/// `view_bridge` autocmd group BEFORE `ui_attach` (see
/// [`EngineHandle::register_vim_enter_autocmd`](view_engine::handle::EngineHandle::register_vim_enter_autocmd)'s
/// doc comment for why that ordering is load-bearing, not incidental, and
/// [`EngineHandle::register_bridge`](view_engine::handle::EngineHandle::register_bridge)'s
/// for the one difference between the two),
/// attaches at the size `size` reports, and forwards the
/// capability-probe's leftover `residue` bytes. Deliberately does not call
/// [`Engine::start_pump`]: only `main.rs` does, once the buffered
/// pre-attach window has been fully replayed (see
/// [`attach_in_background`]'s doc comment for why).
///
/// The size arrives through a callback rather than as two arguments
/// because the caller does not have it yet when the child should start:
/// `nvim --embed` performs no startup at all until a UI attaches, so the
/// terminal handshake that resolves the size
/// is work the child's own startup can run underneath instead of behind
/// (see `main.rs`'s call ordering). Only the attach needs a terminal.
///
/// A `None` size means that handshake failed and no attach will ever
/// happen. The child is killed and reaped here, before returning, so a
/// process that could not take the terminal cannot leave an nvim running
/// behind it; the failure is reported as [`AttachFailure::Attach`], which
/// is what it is -- a child that started and never got attached -- though
/// the one caller that can reach it is already returning the terminal's
/// own error instead.
fn spawn_and_attach(
    cfg: EngineConfig,
    spawned: &AtomicU32,
    size: impl FnOnce() -> Option<(u16, u16)>,
    residue: impl FnOnce() -> Vec<u8>,
) -> Result<Engine, AttachFailure> {
    // read before `Engine::spawn` consumes `cfg` by value: there is no
    // config left to ask afterward, and the choice below depends on it
    let stdin_relay = cfg.stdin_relay_requested();
    let engine = Engine::spawn(cfg).map_err(AttachFailure::Spawn)?;
    // published before the handshake and the registrations below, not
    // after them: the window this pid identifies a child through is
    // exactly the window in which the child can stop answering, and a
    // diagnostic that names it only once the attach is nearly done cannot
    // describe the failure anybody would be hunting
    spawned.store(engine.pid(), Ordering::SeqCst);
    crate::vlog::log_with("engine", || {
        format!("spawned pid={} stdin_relay={stdin_relay}", engine.pid())
    });
    register_and_attach(engine, stdin_relay, size, residue)
}

/// Replaces a failed engine with a fresh one and brings it through the same
/// registration and attach sequence [`spawn_and_attach`] performs, so a
/// restarted session is registered and attached in the one order that has
/// ever been correct rather than in a second copy of it.
///
/// `width`/`height` are the grid's own target size, not the raw terminal's:
/// the chrome the session already reserved (the statusline row, most
/// notably) was reserved by a resize the dead engine was told about and the
/// fresh one has never heard of, so attaching at the terminal's full height
/// would put the statusline one row below the screen -- the same failure
/// `NativeSession::load`'s own resize exists to prevent at startup.
///
/// No `residue`: the capability probe's leftover bytes belong to the
/// terminal handshake this process performed once, long before any restart.
///
/// The engine being replaced is torn down here, in place, and then kept:
/// the teardown is [`Engine::kill_exit`], so no replacement is ever brought
/// up alongside a live connection -- but the caller still holds
/// the corpse when this returns, whichever way it returned. That is what
/// lets a failed attempt be retried: a session whose replacement could not
/// be started has no second engine to report through, and one that had
/// dropped its first has nothing to keep painting with either.
pub(crate) fn restart_and_attach(
    engine: &mut Engine,
    cfg: EngineConfig,
    width: u16,
    height: u16,
) -> Result<Engine, AttachFailure> {
    let stdin_relay = cfg.stdin_relay_requested();
    // on every attempt, not only the first: a child already reaped reports
    // its cached status and this returns at once. `kill_exit`, never
    // `wait_exit`: a restart reached with the child still alive is one the
    // user's unsaved work depends on getting back off the swap file, and
    // `wait_exit`'s opening `qa!` is what deletes it (see `kill_exit`)
    let _ = engine.kill_exit();
    let engine = Engine::spawn_recovering(cfg).map_err(AttachFailure::Spawn)?;
    crate::vlog::log_with("engine", || {
        format!(
            "restarted pid={} stdin_relay={stdin_relay} grid={width}x{height}",
            engine.pid()
        )
    });
    register_and_attach(engine, stdin_relay, || Some((width, height)), Vec::new)
}

/// The half of [`spawn_and_attach`] that runs against a child that is
/// already up: the `VimEnter` autocmd, the `view_bridge` group, the attach
/// itself, and the terminal handshake's leftover bytes.
///
/// `size` is asked as late as it can be -- after both registrations, which
/// need a channel id and no terminal at all, and immediately before the
/// one call that does need it. On the startup path that callback is a wait
/// on the terminal handshake, so everything above it is work nvim performs
/// while the frontend is still resolving what kind of terminal it is
/// talking to.
fn register_and_attach(
    engine: Engine,
    stdin_relay: bool,
    size: impl FnOnce() -> Option<(u16, u16)>,
    residue: impl FnOnce() -> Vec<u8>,
) -> Result<Engine, AttachFailure> {
    engine
        .handle
        .register_vim_enter_autocmd(engine.api_info.channel_id)
        .map_err(AttachFailure::Attach)?;
    crate::vlog::log("engine", "registered VimEnter autocmd");
    engine
        .handle
        .register_bridge(engine.api_info.channel_id)
        .map_err(AttachFailure::Attach)?;
    crate::vlog::log("engine", "registered view_bridge autocmd group");
    let Some((width, height)) = size() else {
        crate::vlog::log("engine", "no terminal size ever came; killing the child");
        // `Engine`'s own `Drop` is the kill and the reap (see its impl):
        // dropping it here is what performs them, and returning without it
        // is what leaves the stray nvim behind
        drop(engine);
        return Err(AttachFailure::Attach(EngineError::Closed));
    };
    if stdin_relay {
        engine
            .handle
            .ui_attach_with_stdin_relay(width, height)
            .map_err(AttachFailure::Attach)?;
        crate::vlog::log("engine", "ui_attach_with_stdin_relay returned ok");
    } else {
        engine
            .handle
            .ui_attach(width, height)
            .map_err(AttachFailure::Attach)?;
        crate::vlog::log("engine", "ui_attach returned ok");
    }
    // resolved here rather than at the call site, and here rather than
    // anywhere earlier in this function: on the startup path this waits on
    // a channel the capability probe fills, and every line above is work
    // that must not be held up for a terminal's reply (see
    // `attach_in_background`)
    let residue = residue();
    // best-effort, matching this project's original startup ordering: a
    // write failure here means the connection is already gone, which the
    // caller discovers through the engine's own EngineDown path moments
    // later rather than through this loop
    for notation in view_tui::keys::encode_residue_bytes(&residue) {
        let _ = engine.handle.input(&notation);
    }
    Ok(engine)
}

/// What [`attach_in_background`]'s thread waits for before it attaches:
/// the runtime loop's sender (its one use of it is the `EngineReady`
/// marker) and the terminal size `ui_attach` needs. Neither exists until
/// `Term::init` has returned, and the child's own startup depends on
/// neither, which is why the thread starts without them.
type AttachStart = (crate::wake::LoopSender, u16, u16);

/// Runs [`spawn_and_attach`] on a background thread so a slow-starting
/// nvim can never delay [`paint_shell_frame`], and returns the
/// [`AttachGuard`] that owns it: the child starts immediately, the
/// terminal size reaches it later through [`AttachGuard::attach_at`], and
/// its result is read exactly once, success or failure, through
/// [`AttachGuard::engine_result`]. The same background thread then sends
/// `Msg::EngineReady` down the sender `attach_at` handed it, so
/// [`drain_pre_attach`]'s blocking loop wakes deterministically instead of
/// polling, with no timer and no poll.
///
/// # Ordering: no pump message can ever precede `EngineReady` in `msg_tx`
///
/// This function never calls [`Engine::start_pump`] -- `main.rs` is the
/// only caller of `start_pump` in the whole process, and it only calls it
/// after `drain_pre_attach` has already returned, having observed this
/// exact `EngineReady` send or a channel disconnect. `Engine::start_pump`
/// -> `attach_sink` (`view-engine`'s `damage` module) is the *only* code
/// path that connects the engine's pump (and therefore any
/// `Msg::RedrawReady`, `Msg::EngineRequest`, or `Msg::EngineStopped`) to
/// `msg_tx` at all -- before it runs, those messages stay staged in the
/// pump's own presink, untouched by anything reading `msg_tx`; once it
/// runs, it returns what was staged instead of sending it (see
/// `attach_sink`'s doc comment), for [`run_cutover`] to resolve directly.
/// Since `start_pump` cannot be called until strictly after this
/// `EngineReady` send has already been consumed by `drain_pre_attach`, it
/// is structurally impossible -- not merely unobserved in testing -- for a
/// pump-routed message to reach `msg_tx` (by send or by being handed back
/// through `SinkCutover`) ahead of `EngineReady`. This is why
/// `drain_pre_attach`'s catch-all match arm for "any other message kind" is
/// correct rather than merely lucky: there is no other kind of message this
/// loop could ever see before `EngineReady`, besides `Msg::Key` and
/// `Msg::Resized` from the input thread.
pub fn attach_in_background(cfg: EngineConfig) -> AttachGuard {
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
    let (start_tx, start_rx) = std::sync::mpsc::sync_channel(1);
    let (residue_tx, residue_rx) = std::sync::mpsc::sync_channel(1);
    let pid = Arc::new(AtomicU32::new(0));
    let spawned = Arc::clone(&pid);
    let attach = std::thread::spawn(move || {
        let mut started: Option<crate::wake::LoopSender> = None;
        let result = spawn_and_attach(
            cfg,
            &spawned,
            || {
                let (msg_tx, width, height) = start_rx.recv().ok()?;
                started = Some(msg_tx);
                Some((width, height))
            },
            // a sender dropped without ever sending (a caller that failed
            // between here and its own probe) reads as "nothing was typed",
            // which is what an empty residue already means
            || residue_rx.recv().unwrap_or_default(),
        );
        // a spawn that failed before the size was ever asked for still owes
        // `main.rs` its report, and the sender it is reported over is the
        // one that arrives with that size: this waits for it rather than
        // dropping the failure. Nothing arrives at all only when the
        // terminal itself never came up -- and then `main.rs` is already
        // returning that error, with no loop left to wake.
        let Some(msg_tx) = started.or_else(|| start_rx.recv().ok().map(|(msg_tx, ..)| msg_tx))
        else {
            return;
        };
        let _ = result_tx.send(result);
        // sent unconditionally, success or failure: drain_pre_attach must
        // wake up either way, so main.rs can move on to read the result and
        // report the failure instead of blocking forever on an attach that
        // will never come
        let _ = msg_tx.send(Msg::EngineReady);
    });
    AttachGuard {
        start_tx: Some(start_tx),
        residue_tx: Some(residue_tx),
        attach: Some(attach),
        engine_rx: result_rx,
        pid,
        armed: true,
    }
}

/// Owns the background attach, and the nvim it spawned, until the caller
/// has the attach's result in hand.
///
/// Every `?` between the spawn and that result -- `Term::init` itself, the
/// shell frame, the probe's second window, opening the input handle -- is
/// an exit from a process that has a live `nvim --embed` child and no
/// remaining path to it: nothing downstream ever attaches, and nothing
/// left in the process knows the child exists. This guard is what makes
/// that unrepresentable rather than a list of call sites to remember: its
/// [`Drop`] kills and reaps whatever the attach produced, so a new early
/// return added between those two points is safe by construction.
///
/// It owns *both* channels the attach thread can block on, and that is
/// load-bearing rather than tidy: a sender left in `main` outlives the
/// guard (locals drop in reverse declaration order), so `Drop` would join
/// a thread parked on a channel nothing can ever disconnect, and an
/// editor that failed to paint its first frame would hang holding the very
/// child this exists to kill.
pub struct AttachGuard {
    /// Dropping this is what tells the attach thread no size is coming.
    start_tx: Option<SyncSender<AttachStart>>,
    /// Dropping this is what tells it no probe residue is coming, which is
    /// the last thing an attach that already succeeded waits for.
    residue_tx: Option<SyncSender<Vec<u8>>>,
    attach: Option<JoinHandle<()>>,
    engine_rx: Receiver<Result<Engine, AttachFailure>>,
    /// The spawned child, `0` until it exists; see [`Self::pid`].
    pid: Arc<AtomicU32>,
    armed: bool,
}

impl AttachGuard {
    /// Hands the attach the terminal size it has been waiting on, and the
    /// loop sender it announces itself over, releasing it to run
    /// `ui_attach`.
    pub fn attach_at(&self, msg_tx: crate::wake::LoopSender, width: u16, height: u16) {
        // a receiver already gone means the attach thread ended early, and
        // the failure it ended with is already in `engine_rx` for
        // `engine_result` to report
        let _ = self
            .start_tx
            .as_ref()
            .map(|start_tx| start_tx.send((msg_tx, width, height)));
    }

    /// Hands the attach the capability probe's leftover bytes, the last
    /// thing it waits for (see `register_and_attach`).
    pub fn send_residue(&self, residue: Vec<u8>) {
        let _ = self
            .residue_tx
            .as_ref()
            .map(|residue_tx| residue_tx.send(residue));
    }

    /// Blocks for the attach's result and disarms: from here the caller
    /// owns the engine, and dropping this guard does nothing.
    ///
    /// Disarming only on `Ok` is the point of the `if let`: an `Err` means
    /// the caller was handed no engine at all, so the guard is still the
    /// only thing that can kill one.
    ///
    /// # Errors
    ///
    /// Returns [`RecvError`] if the attach thread ended without producing
    /// a result at all.
    pub fn engine_result(&mut self) -> Result<Result<Engine, AttachFailure>, RecvError> {
        let result = self.engine_rx.recv();
        if result.is_ok() {
            self.armed = false;
        }
        result
    }

    /// The `nvim` this attach spawned, once it exists -- `None` before the
    /// spawn, and for a spawn that never got a process at all.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        match self.pid.load(Ordering::SeqCst) {
            0 => None,
            pid => Some(pid),
        }
    }
}

impl Drop for AttachGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // both, and both before the join: whichever of the two the thread
        // is parked on, a disconnected channel is what returns it
        self.start_tx.take();
        self.residue_tx.take();
        // the thread is what owns the engine until it sends it, so the join
        // is what makes "there is nothing left running" true rather than
        // eventually true
        if let Some(attach) = self.attach.take() {
            let _ = attach.join();
        }
        // `Engine`'s own `Drop` is the kill and the reap: taking the result
        // out of the channel and letting it fall here is what runs them
        let killed = self.engine_rx.try_recv().is_ok();
        crate::vlog::log_with("engine", || {
            format!(
                "attach abandoned before its result was read (pid={:?} attached={killed})",
                self.pid()
            )
        });
    }
}

/// The pre-attach window's buffered input, for [`run_cutover`] to replay
/// directly through `update()`/`Executor` (never by re-enqueuing onto the
/// bounded channel `msg_tx` -- see `run_cutover`'s doc comment for why)
/// once attach completes.
pub struct DrainedInput {
    /// The most recent terminal size observed during the window, if it was
    /// resized at all. Only the latest matters at cutover: an intermediate
    /// size mid-resize is already stale by the time attach completes, and
    /// there is no engine yet during the window to notify of any of them.
    pub resize: Option<(u16, u16)>,
    /// Buffered keystrokes, oldest first.
    pub keys: Vec<Key>,
    /// `Effect::ScheduleToastExpiry` owed by every native notice this
    /// window's key-ring overflow pushed (via
    /// `EngineModel::record_native_notice`), buffered rather than run: no
    /// executor exists yet at the point this window drains (`main.rs`
    /// builds one strictly after `drain_pre_attach` returns), so there is
    /// nothing to hand the effect to. The caller runs these once its own
    /// executor exists -- see `main.rs`'s cutover setup -- which still
    /// starts the toast's clock before `runtime::run`'s loop does anything
    /// else, never silently drops it.
    pub toast_effects: Vec<Effect>,
}

/// Drains `msg_rx` until [`attach_in_background`]'s `Msg::EngineReady`
/// marker arrives, buffering every `Msg::Key` seen in the meantime into a
/// bounded ring of [`KEY_RING_CAPACITY`] (oldest dropped first once full,
/// each drop repainting an updated toast through
/// `EngineModel::record_native_notice` -- the same classify/history/expiry
/// choke point every other locally-synthesized notice goes through, never
/// silent), and tracking the latest `Msg::Resized` seen, if any.
///
/// No other message kind can reach `msg_rx` before `EngineReady` -- see
/// [`attach_in_background`]'s doc comment for the ordering argument. A
/// paste or mouse event is still out of this window's scope even though it
/// is unreachable here in practice: there is no engine yet to forward
/// either to, and only keystrokes and the terminal size are retained
/// across the window.
///
/// Delegates the drop-overflow repaint to [`drain_pre_attach_with`] so the
/// accumulation logic itself (the part [`Msg`] kind does what, and which
/// one wins on conflict) is testable without a live [`Term`], which
/// `cargo test` cannot construct outside a real tty.
#[cfg_attr(not(unix), allow(unused_variables))]
pub fn drain_pre_attach(
    msg_rx: &Receiver<Msg>,
    msg_tx: &crate::wake::LoopSender,
    model: &mut Model,
    term: &mut Term,
    input: crate::runtime::TermInput<'_>,
    term_size: &view_tui::terminal::TermSizeCell,
) -> DrainedInput {
    let repaint = |model: &mut Model| {
        let surface = view_surface::render(model);
        let _ = term.draw_surface(model, &surface, &view_core::grid::GridDamage::full());
    };
    #[cfg(unix)]
    {
        drain_pre_attach_polled(msg_rx, msg_tx, model, input, term_size, repaint)
    }
    #[cfg(not(unix))]
    {
        drain_pre_attach_with(msg_rx, model, repaint)
    }
}

/// The unix pre-attach wait: the same fd readiness poll the runtime loop
/// sleeps in, over the terminal handle and the wake pipe, with every ready
/// terminal event decoded inline (`view-tui`'s drain) and absorbed through
/// the same [`PreAttach`] accumulator the channel-driven wait uses --
/// pre-attach keys still land in the bounded ring, oldest evicted first,
/// and are replayed in order at cutover. `Msg::EngineReady` still arrives
/// through the channel from the attach thread, whose wake-wired send is
/// what interrupts the poll.
///
/// A failing poll degrades to the channel-only blocking wait: attach
/// completion still terminates the window (the attach thread's send is
/// unconditional), at the cost of any keys typed during it -- the same
/// loss profile the input-thread design had when its thread died.
#[cfg(unix)]
fn drain_pre_attach_polled(
    msg_rx: &Receiver<Msg>,
    msg_tx: &crate::wake::LoopSender,
    model: &mut Model,
    input: &mut view_tui::input::InputSource,
    term_size: &view_tui::terminal::TermSizeCell,
    mut repaint: impl FnMut(&mut Model),
) -> DrainedInput {
    use std::sync::mpsc::TryRecvError;

    let Some(waker) = msg_tx.waker() else {
        // no waker wired means no poll to interrupt: the blocking
        // channel wait is the only correct wait left
        return drain_pre_attach_with(msg_rx, model, repaint);
    };
    let mut state = PreAttach::new();
    'window: loop {
        loop {
            match msg_rx.try_recv() {
                Ok(msg) => {
                    if state.absorb(msg, model, &mut repaint) {
                        break 'window;
                    }
                }
                Err(TryRecvError::Disconnected) => break 'window,
                Err(TryRecvError::Empty) => break,
            }
        }
        waker.clear();
        // re-checked after the rearm, mirroring the runtime loop's own
        // lost-wakeup guard (see `runtime`'s unified wait)
        match msg_rx.try_recv() {
            Ok(msg) => {
                if state.absorb(msg, model, &mut repaint) {
                    break 'window;
                }
                continue 'window;
            }
            Err(TryRecvError::Disconnected) => break 'window,
            Err(TryRecvError::Empty) => {}
        }
        match crate::wake::poll_readiness(input, waker, None) {
            Ok(ready) => {
                if ready.input {
                    let mut events = Vec::new();
                    input.drain(term_size, |msg| events.push(msg));
                    for msg in events {
                        if state.absorb(msg, model, &mut repaint) {
                            break 'window;
                        }
                    }
                }
            }
            Err(_) => loop {
                match msg_rx.recv() {
                    Ok(msg) => {
                        if state.absorb(msg, model, &mut repaint) {
                            break 'window;
                        }
                    }
                    Err(_) => break 'window,
                }
            },
        }
    }
    state.finish()
}

/// The accumulation logic behind [`drain_pre_attach`], generic over the
/// overflow repaint so tests can supply a no-op (or call-counting) closure
/// instead of a real [`Term`]. Also the whole wait off unix, where input
/// arrives through the channel from the dedicated input thread.
fn drain_pre_attach_with(
    msg_rx: &Receiver<Msg>,
    model: &mut Model,
    mut repaint: impl FnMut(&mut Model),
) -> DrainedInput {
    let mut state = PreAttach::new();
    // a recv error means every producer is gone: nothing left to wait for
    while let Ok(msg) = msg_rx.recv() {
        if state.absorb(msg, model, &mut repaint) {
            break;
        }
    }
    state.finish()
}

/// The pre-attach window's accumulator, shared by the channel-driven and
/// poll-driven waits so the two cannot drift in what a message does to the
/// buffered state.
struct PreAttach {
    ring: KeyRing,
    resize: Option<(u16, u16)>,
    dropped: u32,
    toast_effects: Vec<Effect>,
}

impl PreAttach {
    fn new() -> Self {
        Self {
            ring: KeyRing::new(KEY_RING_CAPACITY),
            resize: None,
            dropped: 0,
            toast_effects: Vec::new(),
        }
    }

    /// Folds one message into the window's state, returning `true` when
    /// the window is over (`Msg::EngineReady` observed).
    fn absorb(&mut self, msg: Msg, model: &mut Model, mut repaint: impl FnMut(&mut Model)) -> bool {
        match msg {
            Msg::Key(key) => {
                if self.ring.push(key) {
                    self.dropped = self.dropped.saturating_add(1);
                    let dropped = self.dropped;
                    let plural = if dropped == 1 { "" } else { "s" };
                    self.toast_effects.extend(model.engine.record_native_notice(
                        format!(
                            "view: startup key buffer full, dropped {dropped} keystroke{plural}"
                        ),
                        dropped > 1,
                    ));
                    repaint(model);
                }
                false
            }
            Msg::Resized { width, height } => {
                // applied to the model here, not only carried to the
                // cutover: the overflow repaint above paints from the
                // model's terminal size, so a resize that arrives during
                // the attach window would otherwise leave every repaint
                // for the rest of that window addressing rows and columns
                // the terminal no longer has. Still carried, because the
                // cutover owes nvim its own `TryResize` and this window has
                // no engine to send one to.
                model.term_width = width;
                model.term_height = height;
                self.resize = Some((width, height));
                false
            }
            // applied to the model here for the same reason a resize is:
            // the first content frame paints at the model's capabilities,
            // and this window is the one place a late answer has no loop to
            // reach. No repaint of its own -- the shell frame carries no
            // cell a tier changes, so the cutover is early enough.
            //
            // Logged through the loop's own path rather than beside it: the
            // last `caps tier=` line is the session's record of what it
            // settled at, and a reader cannot be asked to know which side
            // of the attach the answer happened to land on.
            Msg::CapsUpgraded(caps) => {
                crate::vlog::log_msg(&msg);
                model.caps = caps;
                false
            }
            Msg::EngineReady => true,
            // structurally unreachable before EngineReady (see
            // attach_in_background's doc comment); kept for the same
            // defensive-totality reason update()'s own no-op arms are
            _ => false,
        }
    }

    fn finish(mut self) -> DrainedInput {
        DrainedInput {
            resize: self.resize,
            keys: self.ring.drain(),
            toast_effects: self.toast_effects,
        }
    }
}

/// Everything [`run_cutover`] needs to resolve, gathered by the caller from
/// `Engine::start_pump`'s `SinkCutover` (`presink`, and `pending_redraw`
/// once resolved via `DamagePump::take_damage`) and [`drain_pre_attach`]'s
/// [`DrainedInput`] (`resize`, `keys`). A plain data bundle rather than
/// separate parameters: `run_cutover` already takes the channel/executor
/// seam alongside it, and folding the four staged-state fields into one
/// struct keeps the call site self-describing instead of four
/// same-shaped-looking positional arguments.
pub(crate) struct CutoverInput {
    pub presink: Vec<Msg>,
    pub pending_redraw: Vec<UiEvent>,
    pub resize: Option<(u16, u16)>,
    pub keys: Vec<Key>,
}

/// What [`run_cutover`] decides once every staged message and buffered
/// input has been resolved.
pub(crate) enum CutoverOutcome {
    /// Every stage resolved with `Flow::Continue`, or the engine connection
    /// was lost partway through -- discovered cleanly by `runtime::run`'s
    /// own loop once it starts, the same way a mid-loop write failure
    /// already is. The caller proceeds to `runtime::run`.
    Continue,
    /// `update()` produced `Effect::Quit` while resolving a presink message
    /// (nvim exited before the runtime loop ever started running). The
    /// caller exits with this code without calling `runtime::run`.
    Quit(i32),
}

/// Resolves everything staged at cutover -- presink messages, pending
/// damage, then the pre-attach input buffer (latest resize, then every
/// key, oldest first) -- directly through `runtime::dispatch`, in that
/// order, then returns so the caller can start `runtime::run`. Never sends
/// into `msg_tx`; see the "why nothing here can block" section below.
///
/// Order matches arrival: presink messages and pending damage were both
/// staged before this call ever ran (see
/// `view_engine::damage::PumpShared::attach_sink`'s doc comment), so they
/// resolve first; every key not yet delivered when this call runs (queued
/// in `msg_tx` by the non-unix input thread, or still in the kernel's tty
/// queue for the unix inline drain) was typed after `drain_pre_attach`
/// observed `Msg::EngineReady`, which is after every key in `keys` was
/// already buffered, so applying `keys` here, before either source is
/// read again by `runtime::run`'s loop, reproduces arrival order
/// exactly. The resize (if
/// any) is applied before the keys typed at that size, so nvim sees the
/// final pre-attach terminal size first. A presink `Msg::EngineStopped` is
/// translated to `Msg::EngineDown` exactly like `runtime::run`'s own loop
/// translates a live one (see `view_core::msg`'s module doc comment):
/// `dispatch` does not replicate that loop-specific mapping itself, since
/// it is otherwise unreachable from the `Msg::Key`/`Msg::Resized` messages
/// replay sends. Between the staged traffic and the replayed input sits
/// `Msg::EngineAttached`, the one announcement this call makes rather than
/// replays: it comes after the presink because a connection that died on the
/// way up says so there, and asking a dead connection anything ahead of that
/// would lose the exit that saying carries.
///
/// # Why nothing here can ever block on `msg_tx`
///
/// Every stage resolves through `runtime::dispatch`, which drives
/// `update()` -> `Executor` directly and never references `mpsc` at all
/// (see `dispatch`'s own doc comment). No channel end appears in this
/// signature, so reintroducing a send here is a compile error, not a
/// runtime hazard a test has to catch. `main.rs` calls this strictly
/// after `Engine::start_pump` has already returned its `SinkCutover`
/// (never sent into `msg_tx`, see `attach_sink`'s doc comment) and
/// strictly before `runtime::run`'s loop starts consuming `msg_tx`, so
/// this is the one place in the whole process that resolves messages
/// staged before a consumer of `msg_tx` existed.
///
/// That includes nvim's `VimEnter`, which is why `native` is threaded here
/// rather than only into `runtime::run`: a config that sources quickly fires
/// `VimEnter` before the sink attaches, so the presink is the ordinary place
/// this session's takeover and key registration are triggered from.
pub(crate) fn run_cutover<E: crate::engine_ops::EngineOps>(
    model: &mut Model,
    executor: &crate::runtime::Executor<E>,
    follow_ups: &mut crate::runtime::FollowUps<'_>,
    input: CutoverInput,
    engine_stopped_exit: impl FnOnce() -> view_core::msg::ExitInfo,
) -> CutoverOutcome {
    let CutoverInput {
        presink,
        pending_redraw,
        resize,
        keys,
    } = input;
    let mut engine_alive = true;
    let mut engine_stopped_exit = Some(engine_stopped_exit);

    for msg in presink {
        let msg = match msg {
            Msg::EngineStopped { reason, .. } => {
                model.fatal_reason = reason;
                let exit = engine_stopped_exit.take().map_or(
                    view_core::msg::ExitInfo {
                        code: None,
                        by_signal: false,
                    },
                    |f| f(),
                );
                Msg::EngineDown(exit)
            }
            other => other,
        };
        match crate::runtime::dispatch(model, executor, follow_ups, msg) {
            crate::runtime::Flow::Continue => {}
            crate::runtime::Flow::Quit(code) => return CutoverOutcome::Quit(code),
            // a restart chosen during the cutover is the same teardown here
            // as a lost engine: the fresh connection this window is still
            // establishing is the one that would be torn down, and startup
            // has no steady-state loop to bring a replacement back into
            crate::runtime::Flow::EngineLost | crate::runtime::Flow::RestartEngine => {
                engine_alive = false;
                break;
            }
        }
    }

    // after the connection's own staged traffic and before anything replayed
    // from the terminal: the staged messages are the only place a connection
    // that died on the way up says so, and that saying has to be translated
    // into an exit before anything else is asked of it. Still ahead of the
    // steady-state loop, which is what the reading needs -- the connection
    // this asks about can park itself at a prompt nobody answers, so waiting
    // for it to announce it finished starting would wait forever (see
    // `SWAP_RECOVERY_PROBE`)
    if engine_alive {
        match crate::runtime::dispatch(model, executor, follow_ups, Msg::EngineAttached) {
            crate::runtime::Flow::Continue => {}
            crate::runtime::Flow::Quit(code) => return CutoverOutcome::Quit(code),
            crate::runtime::Flow::EngineLost | crate::runtime::Flow::RestartEngine => {
                engine_alive = false;
            }
        }
    }

    if engine_alive && !pending_redraw.is_empty() {
        engine_alive =
            crate::runtime::dispatch(model, executor, follow_ups, Msg::Redraw(pending_redraw))
                == crate::runtime::Flow::Continue;
    }
    if engine_alive {
        if let Some((width, height)) = resize {
            engine_alive = crate::runtime::dispatch(
                model,
                executor,
                follow_ups,
                Msg::Resized { width, height },
            ) == crate::runtime::Flow::Continue;
        }
    }
    // skipped entirely once an earlier write already reported the engine
    // gone: further replayed input would fail the same way, and
    // runtime::run's own loop discovers the same failure cleanly once its
    // pump is attached
    if engine_alive {
        for key in keys {
            if crate::runtime::dispatch(model, executor, follow_ups, Msg::Key(key))
                != crate::runtime::Flow::Continue
            {
                break;
            }
        }
    }
    CutoverOutcome::Continue
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    #[cfg(unix)]
    use view_test_support::ScratchDir;

    fn key(notation: &str) -> Key {
        Key {
            notation: notation.to_string(),
        }
    }

    // `spawn_and_attach` is private to this crate, and `view` ships no lib
    // target for an integration test under tests/ to link against, so this
    // is the only place its stdin-relay branch (the `if stdin_relay`
    // dispatch to `ui_attach_with_stdin_relay` rather than plain
    // `ui_attach`) can be exercised at all. Spawns a real nvim, matching
    // `cli_live.rs`'s own live-relay test and this crate's `task test`
    // target, which already documents "requires nvim >= 0.11 on PATH".
    #[cfg(unix)]
    #[test]
    fn spawn_and_attach_takes_the_stdin_relay_branch_when_armed() {
        use std::os::fd::AsFd;

        let scratch = ScratchDir::new("startup-spawn-and-attach-stdin-relay").unwrap();
        let content = scratch.join("source.txt");
        std::fs::write(&content, "hello from spawn_and_attach\n").unwrap();
        let source = std::fs::File::open(&content).unwrap();

        let cfg = EngineConfig::isolated()
            .with_arg("-")
            .with_stdin_relay(source.as_fd().try_clone_to_owned().unwrap());
        let mut engine =
            spawn_and_attach(cfg, &AtomicU32::new(0), || Some((80, 24)), Vec::new).unwrap();

        assert_eq!(
            engine.handle.eval_str("getline(1)").unwrap(),
            "hello from spawn_and_attach",
            "spawn_and_attach must call ui_attach_with_stdin_relay, not \
             plain ui_attach, whenever EngineConfig::stdin_relay_requested() \
             is true, or the fd nvim was told to read from never gets wired \
             up at all"
        );
        let _ = engine.wait_exit();
    }

    /// A process that cannot bring its terminal up still spawned an nvim
    /// first, and the moment it returns that error there is nothing left
    /// holding the child: no attach will ever happen, no handle survives
    /// the return, and the editor stays resident until the user finds it.
    /// This is the half of that the attach thread owns -- a size that never
    /// arrives at all.
    #[cfg(unix)]
    #[test]
    fn a_size_that_never_arrives_leaves_no_nvim_behind() {
        let spawned = AtomicU32::new(0);
        let failed = spawn_and_attach(EngineConfig::isolated(), &spawned, || None, Vec::new);

        assert!(
            failed.is_err(),
            "an attach that never got a terminal size cannot report success"
        );
        let pid = spawned.load(Ordering::SeqCst);
        assert!(pid != 0, "the child was never spawned at all");
        assert_reaped(pid);
    }

    /// The other half, at the point it is most dangerous: the size did
    /// arrive, `ui_attach` succeeded, and the caller failed before it could
    /// hand over the probe's residue -- `paint_shell_frame` writing into a
    /// pty that has gone away, or `settle_probe` itself. The attach thread
    /// is parked in the residue wait at that moment, so the guard's `Drop`
    /// returns only if it disconnects that channel too: a residue sender
    /// living anywhere else outlives the guard, and this hangs forever
    /// holding the child it exists to kill.
    #[cfg(unix)]
    #[test]
    fn a_failure_before_the_residue_is_handed_over_leaves_no_nvim_behind() {
        let (raw_tx, _msg_rx) = std::sync::mpsc::sync_channel(MSG_CHANNEL_CAPACITY);
        let msg_tx =
            crate::wake::LoopSender::with_waker(raw_tx, crate::wake::LoopWaker::new().unwrap());

        let guard = attach_in_background(EngineConfig::isolated());
        guard.attach_at(msg_tx, 80, 24);
        let pid = wait_for_spawn(&guard);

        drop(guard);

        assert_reaped(pid);
    }

    /// And the ordinary early return, after everything the attach waits on
    /// has been handed over: the engine is sitting in the result channel,
    /// owned by nobody the caller can still reach.
    #[cfg(unix)]
    #[test]
    fn dropping_the_guard_before_its_result_leaves_no_nvim_behind() {
        let (raw_tx, msg_rx) = std::sync::mpsc::sync_channel(MSG_CHANNEL_CAPACITY);
        let msg_tx =
            crate::wake::LoopSender::with_waker(raw_tx, crate::wake::LoopWaker::new().unwrap());

        let guard = attach_in_background(EngineConfig::isolated());
        guard.attach_at(msg_tx, 80, 24);
        guard.send_residue(Vec::new());
        assert!(
            matches!(msg_rx.recv().unwrap(), Msg::EngineReady),
            "the attach thread announces itself once, with EngineReady"
        );
        let pid = guard.pid().expect("the attach spawned a child");

        drop(guard);

        assert_reaped(pid);
    }

    /// The pid is published the moment `Engine::spawn` returns, so this
    /// resolves during the handshake rather than at the end of the attach.
    ///
    /// The bound is a wedge, not a measurement: a fork that has not
    /// happened within it did not happen at all, so it is generous and
    /// scaled for the host rather than picked to fit one.
    #[cfg(unix)]
    fn wait_for_spawn(guard: &AttachGuard) -> u32 {
        let deadline =
            Instant::now() + view_test_support::host_deadline(std::time::Duration::from_secs(5));
        std::iter::repeat_with(|| guard.pid())
            .take_while(|_| Instant::now() < deadline)
            .find_map(|pid| {
                if pid.is_none() {
                    std::thread::yield_now();
                }
                pid
            })
            .expect("the attach never reported a spawned child")
    }

    /// Killed *and* reaped: a zombie is still a child this process left
    /// behind, and both checks below report one.
    #[cfg(unix)]
    fn assert_reaped(pid: u32) {
        let gone = if cfg!(target_os = "linux") {
            !std::path::Path::new(&format!("/proc/{pid}")).exists()
        } else {
            // `.output()`, and an unrunnable `ps` failing the test rather
            // than reading as an absent child: a check that cannot run is
            // the one result this assertion must never accept quietly
            !std::process::Command::new("ps")
                .args(["-p", &pid.to_string()])
                .output()
                .expect("`ps` has to run for this assertion to mean anything")
                .status
                .success()
        };
        assert!(
            gone,
            "pid {pid} is still around: startup left an nvim running with \
             nothing in the process able to reach it"
        );
    }

    /// Pins what makes the capability probe's second window free: the
    /// attach asks for the probe's residue only once its own work is done.
    ///
    /// `main` hands this thread a channel and then spends the wait for a
    /// slow terminal's replies on its own thread, so the two run at the
    /// same time. That only holds while the residue is read at the very end
    /// -- move the read up to the top of `spawn_and_attach` and the engine
    /// spawn stops overlapping the wait and starts queueing behind it, with
    /// nothing failing to say so.
    #[cfg(unix)]
    #[test]
    fn the_attach_asks_for_the_probe_residue_only_after_its_own_work() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;

        let start = Instant::now();
        let asked_after = Arc::new(AtomicU64::new(0));
        let recorder = Arc::clone(&asked_after);
        let mut engine = spawn_and_attach(
            EngineConfig::isolated(),
            &AtomicU32::new(0),
            || Some((80, 24)),
            move || {
                let elapsed = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
                recorder.store(elapsed, Ordering::SeqCst);
                Vec::new()
            },
        )
        .unwrap();
        let attached_after = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
        let asked_after = asked_after.load(Ordering::SeqCst);

        assert!(
            asked_after * 2 >= attached_after,
            "the residue was asked for {asked_after}us into an attach that \
             took {attached_after}us: everything before that point is time \
             the caller's own probe wait no longer overlaps"
        );
        let _ = engine.wait_exit();
    }

    #[test]
    fn ring_under_capacity_never_evicts_and_drains_in_order() {
        let mut ring = KeyRing::new(3);
        assert!(!ring.push(key("a")));
        assert!(!ring.push(key("b")));
        let drained = ring.drain();
        assert_eq!(
            drained
                .iter()
                .map(|k| k.notation.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn ring_at_capacity_evicts_oldest_and_keeps_arrival_order() {
        let mut ring = KeyRing::new(2);
        assert!(!ring.push(key("a")));
        assert!(!ring.push(key("b")));
        // over capacity: "a" (oldest) is evicted to make room for "c"
        assert!(ring.push(key("c")));
        let drained = ring.drain();
        assert_eq!(
            drained
                .iter()
                .map(|k| k.notation.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "c"]
        );
    }

    #[test]
    fn drain_empties_the_ring_so_a_second_drain_yields_nothing() {
        let mut ring = KeyRing::new(4);
        ring.push(key("x"));
        assert_eq!(ring.drain().len(), 1);
        assert!(ring.drain().is_empty());
    }

    /// A terminal that answers while nvim is still attaching answers into
    /// this window, which has no loop to hand the upgrade to: the tier has
    /// to land on the model here or the session paints the rest of its life
    /// at the one the probe settled for.
    #[test]
    fn a_capability_answer_inside_the_attach_window_still_reaches_the_model() {
        let (tx, rx) = std::sync::mpsc::sync_channel(MSG_CHANNEL_CAPACITY);
        let full = view_core::model::TermCaps::from_probe(true, true, true);
        tx.send(Msg::CapsUpgraded(full)).unwrap();
        tx.send(Msg::EngineReady).unwrap();

        let mut model = Model::with_term_size(80, 24);
        let _ = drain_pre_attach_with(&rx, &mut model, |_| {});

        assert_eq!(
            model.caps, full,
            "the answer arrived before the loop existed and was dropped with \
             the window, so every frame after the cutover paints at the tier \
             the probe gave up on"
        );
    }

    #[test]
    fn drain_pre_attach_keeps_only_the_latest_resize_and_every_key_in_order() {
        let (tx, rx) = std::sync::mpsc::sync_channel(16);
        tx.send(Msg::Resized {
            width: 80,
            height: 24,
        })
        .unwrap();
        tx.send(Msg::Key(key("a"))).unwrap();
        // a later resize supersedes the first: only the final size at
        // cutover matters
        tx.send(Msg::Resized {
            width: 120,
            height: 40,
        })
        .unwrap();
        tx.send(Msg::Key(key("b"))).unwrap();
        tx.send(Msg::EngineReady).unwrap();

        let mut model = Model::with_term_size(80, 24);
        let drained = drain_pre_attach_with(&rx, &mut model, |_| {});

        assert_eq!(drained.resize, Some((120, 40)));
        assert_eq!(
            drained
                .keys
                .iter()
                .map(|k| k.notation.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(
            (model.term_width, model.term_height),
            (120, 40),
            "the model must follow the terminal during the attach window too: \
             an overflow repaint here paints from these fields"
        );
    }

    #[test]
    fn an_overflow_repaint_during_the_attach_window_sees_the_resized_terminal() {
        // the pre-attach hole this closes: a resize arriving before the
        // key-ring overflow toast leaves every repaint for the rest of the
        // attach window painting at the startup size -- on a shrink, at
        // rows the terminal no longer has
        let (tx, rx) = std::sync::mpsc::sync_channel(256);
        tx.send(Msg::Resized {
            width: 40,
            height: 12,
        })
        .unwrap();
        for _ in 0..=KEY_RING_CAPACITY {
            tx.send(Msg::Key(key("a"))).unwrap();
        }
        tx.send(Msg::EngineReady).unwrap();

        let mut model = Model::with_term_size(80, 24);
        let mut painted_sizes = Vec::new();
        let _ = drain_pre_attach_with(&rx, &mut model, |model| {
            painted_sizes.push((model.term_width, model.term_height));
        });

        assert!(
            !painted_sizes.is_empty(),
            "the overflow repaint must have run for this test to say anything"
        );
        assert!(
            painted_sizes.iter().all(|&size| size == (40, 12)),
            "every repaint after the resize must paint at the new size, got {painted_sizes:?}"
        );
    }

    #[test]
    fn drain_pre_attach_returns_on_channel_disconnect_with_no_engine_ready() {
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        tx.send(Msg::Key(key("a"))).unwrap();
        drop(tx);

        let mut model = Model::with_term_size(80, 24);
        let drained = drain_pre_attach_with(&rx, &mut model, |_| {});

        assert_eq!(
            drained
                .keys
                .iter()
                .map(|k| k.notation.as_str())
                .collect::<Vec<_>>(),
            vec!["a"]
        );
        assert_eq!(drained.resize, None);
    }

    #[test]
    fn drain_pre_attach_repaints_once_per_eviction_and_keeps_the_ring_at_capacity() {
        let (tx, rx) = std::sync::mpsc::sync_channel(KEY_RING_CAPACITY + 2);
        for i in 0..KEY_RING_CAPACITY + 1 {
            tx.send(Msg::Key(key(&i.to_string()))).unwrap();
        }
        tx.send(Msg::EngineReady).unwrap();

        let mut model = Model::with_term_size(80, 24);
        let repaints = std::cell::RefCell::new(0u32);
        let drained = drain_pre_attach_with(&rx, &mut model, |_| {
            *repaints.borrow_mut() += 1;
        });

        assert_eq!(drained.keys.len(), KEY_RING_CAPACITY);
        // the oldest ("0") was evicted to make room for the (KEY_RING_CAPACITY)-th key
        assert_eq!(drained.keys[0].notation, "1");
        assert_eq!(*repaints.borrow(), 1);

        // the overflow notice is pushed through EngineModel::record_native_notice
        // (never a raw Messages::push), so it must carry the same
        // ScheduleToastExpiry and scrollback-history treatment any other
        // locally-synthesized notice gets -- the drain runs before any
        // Executor exists to run that effect against, so it must come back
        // to the caller rather than being silently dropped
        let entry = model
            .engine
            .messages
            .entries
            .last()
            .expect("the overflow notice must be pushed to the message surface");
        assert!(
            matches!(
                drained.toast_effects.as_slice(),
                [Effect::ScheduleToastExpiry { id, after }]
                    if *id == entry.id() && *after == view_core::native::toast::TRANSIENT_TOAST_TIMEOUT
            ),
            "the drain must hand back exactly one ScheduleToastExpiry for the overflow \
             notice, to be run once the caller's own executor exists: {:?}",
            drained.toast_effects
        );
        assert_eq!(
            model.engine.toast_history.entries().next().map(|e| e.id()),
            Some(entry.id()),
            "the overflow notice must land in scrollback history too, not just on screen"
        );
    }

    /// Drives the literal production `run_cutover` -- not a hand-recreated
    /// shape of it -- against a `msg_tx` pre-filled to its full 64-slot
    /// capacity with no consumer draining it, proving the whole cutover
    /// (presink dispatch, pending-damage dispatch, resize replay, key
    /// replay) completes without ever touching that channel. A version that
    /// writes into `msg_tx` anywhere in this path deadlocks against this
    /// setup instead of passing.
    #[test]
    fn run_cutover_against_a_pre_filled_channel_replays_everything_without_blocking() {
        use view_core::msg::{EngineRequest, ReplyToken};

        let (tx, _rx) = std::sync::mpsc::sync_channel::<Msg>(KEY_RING_CAPACITY);
        for _ in 0..KEY_RING_CAPACITY {
            tx.send(Msg::Key(key("filler"))).unwrap();
        }
        // the channel is now completely full with no consumer draining it --
        // the exact state main.rs's real msg_tx can be in by the time
        // cutover runs: the input thread's own blocking sends can fill it
        // during attach, and runtime::run's loop has not started consuming
        // yet

        let presink = vec![Msg::EngineRequest(EngineRequest::VimEnter {
            token: ReplyToken { msgid: 1 },
        })];
        let keys: Vec<Key> = (0..KEY_RING_CAPACITY)
            .map(|i| key(&i.to_string()))
            .collect();

        let handle = std::thread::spawn(move || {
            let ops = crate::engine_ops::FakeOps::default();
            let executor = crate::runtime::Executor::new(ops);
            let mut model = Model::with_term_size(80, 24);
            model.content_painted = false;
            let outcome = run_cutover(
                &mut model,
                &executor,
                &mut crate::runtime::FollowUps {
                    native: &mut crate::native::NativeSession::inert(),
                    theme: &mut crate::bridge::ThemeBridge::new(None),
                    speculate: crate::speculate::SpeculationClock::default(),
                },
                CutoverInput {
                    presink,
                    pending_redraw: vec![UiEvent::Flush],
                    resize: Some((100, 40)),
                    keys,
                },
                || view_core::msg::ExitInfo {
                    code: None,
                    by_signal: false,
                },
            );
            (
                outcome,
                model.content_painted,
                executor.into_ops().calls.into_inner(),
            )
        });

        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            handle.is_finished(),
            "run_cutover blocked against a fully pre-filled channel with no \
             consumer -- a channel send was likely reintroduced into the \
             cutover path"
        );
        let (outcome, content_painted, calls) = handle.join().unwrap();
        assert!(matches!(outcome, CutoverOutcome::Continue));
        assert!(
            content_painted,
            "the pending-damage Flush was not dispatched"
        );
        // presink's VimEnter reply and the two calls that reply carries, then
        // the attach probe the cutover closes the staged traffic with, then
        // the resize, then every buffered key, in that exact order -- the
        // arrival order run_cutover's doc comment claims
        let expected_len = 5 + KEY_RING_CAPACITY;
        assert_eq!(calls.len(), expected_len);
        assert_eq!(calls[0], "reply(1,Nil)");
        assert_eq!(calls[1], "probe_swap_recovery(1)");
        assert_eq!(calls[2], "claim_stdout_tty()");
        assert_eq!(calls[3], "probe_swap_recovery(2)");
        assert!(calls[4].starts_with("try_resize("));
        assert_eq!(calls[5], "input(0)");
        assert_eq!(
            calls[expected_len - 1],
            format!("input({})", KEY_RING_CAPACITY - 1)
        );
    }

    /// A presink `Msg::EngineStopped` (nvim's reader thread detected the
    /// connection close before `main.rs` ever called `start_pump`) is
    /// translated to `Msg::EngineDown` and produces `Effect::Quit`, exactly
    /// like a live `Msg::EngineStopped` does in `runtime::run`'s own loop.
    /// Any later presink entries, the pending redraw, and the input replay
    /// are all skipped once that happens: there is no steady-state loop
    /// left to hand them to.
    #[test]
    fn run_cutover_translates_a_presink_engine_stopped_into_quit_and_skips_the_rest() {
        let ops = crate::engine_ops::FakeOps::default();
        let executor = crate::runtime::Executor::new(ops);
        let mut model = Model::with_term_size(80, 24);
        model.content_painted = false;

        let exit_called = std::cell::Cell::new(false);
        let outcome = run_cutover(
            &mut model,
            &executor,
            &mut crate::runtime::FollowUps {
                native: &mut crate::native::NativeSession::inert(),
                theme: &mut crate::bridge::ThemeBridge::new(None),
                speculate: crate::speculate::SpeculationClock::default(),
            },
            CutoverInput {
                presink: vec![Msg::EngineStopped {
                    generation: 1,
                    reason: None,
                }],
                pending_redraw: vec![UiEvent::Flush],
                resize: Some((100, 40)),
                keys: vec![key("should-not-be-sent")],
            },
            || {
                exit_called.set(true);
                view_core::msg::ExitInfo {
                    code: Some(3),
                    by_signal: false,
                }
            },
        );

        assert!(matches!(outcome, CutoverOutcome::Quit(3)));
        assert!(exit_called.get());
        // neither the pending Flush nor the replayed key ever reached
        // update(): Quit short-circuits everything, the attach probe
        // included
        assert!(!model.content_painted);
        assert!(executor.into_ops().calls.into_inner().is_empty());
    }

    /// A presink `Msg::EngineStopped` carrying a reason stashes it on
    /// `model.fatal_reason` before translating to `Msg::EngineDown`, the
    /// same as a live one does in `runtime::run`'s own loop: `main.rs`
    /// reads it off the returned model after the terminal is restored,
    /// never from a direct write inside the reader thread itself.
    #[test]
    fn run_cutover_stashes_a_presink_engine_stopped_reason_on_the_model() {
        let ops = crate::engine_ops::FakeOps::default();
        let executor = crate::runtime::Executor::new(ops);
        let mut model = Model::with_term_size(80, 24);

        let outcome = run_cutover(
            &mut model,
            &executor,
            &mut crate::runtime::FollowUps {
                native: &mut crate::native::NativeSession::inert(),
                theme: &mut crate::bridge::ThemeBridge::new(None),
                speculate: crate::speculate::SpeculationClock::default(),
            },
            CutoverInput {
                presink: vec![Msg::EngineStopped {
                    generation: 1,
                    reason: Some("wedged reader".to_string()),
                }],
                pending_redraw: vec![],
                resize: None,
                keys: vec![],
            },
            || view_core::msg::ExitInfo {
                code: Some(1),
                by_signal: false,
            },
        );

        assert!(matches!(outcome, CutoverOutcome::Quit(1)));
        assert_eq!(model.fatal_reason.as_deref(), Some("wedged reader"));
    }

    /// A connection that died while starting still exits with its own status
    /// and its own reason, even though nothing it is asked can be written to
    /// it. The staged `Msg::EngineStopped` is what carries both, so it is
    /// translated before the cutover asks the connection anything: an
    /// unanswerable question asked first would take the exit down with it and
    /// leave the session running against a dead engine.
    #[test]
    fn run_cutover_exits_with_the_status_of_an_engine_that_died_while_starting() {
        let ops = crate::engine_ops::FakeOps::default();
        *ops.fail_next.borrow_mut() = true;
        let executor = crate::runtime::Executor::new(ops);
        let mut model = Model::with_term_size(80, 24);

        let exit_called = std::cell::Cell::new(false);
        let outcome = run_cutover(
            &mut model,
            &executor,
            &mut crate::runtime::FollowUps {
                native: &mut crate::native::NativeSession::inert(),
                theme: &mut crate::bridge::ThemeBridge::new(None),
                speculate: crate::speculate::SpeculationClock::default(),
            },
            CutoverInput {
                presink: vec![Msg::EngineStopped {
                    generation: 1,
                    reason: Some("nvim: E492: Not an editor command".to_string()),
                }],
                pending_redraw: vec![],
                resize: None,
                keys: vec![],
            },
            || {
                exit_called.set(true);
                view_core::msg::ExitInfo {
                    code: Some(3),
                    by_signal: false,
                }
            },
        );

        assert!(matches!(outcome, CutoverOutcome::Quit(3)));
        assert!(exit_called.get());
        assert_eq!(
            model.fatal_reason.as_deref(),
            Some("nvim: E492: Not an editor command")
        );
    }

    /// Once a dispatch reports the engine connection lost, every later
    /// stage (pending damage, resize, keys) is skipped rather than
    /// attempted: a further write would fail the same way, and
    /// `runtime::run`'s own loop discovers the same failure cleanly once
    /// its pump is attached.
    #[test]
    fn run_cutover_stops_replaying_once_a_write_reports_the_engine_lost() {
        use view_core::msg::{EngineRequest, ReplyToken};

        let ops = crate::engine_ops::FakeOps::default();
        *ops.fail_next.borrow_mut() = true;
        let executor = crate::runtime::Executor::new(ops);
        let mut model = Model::with_term_size(80, 24);
        model.content_painted = false;

        let outcome = run_cutover(
            &mut model,
            &executor,
            &mut crate::runtime::FollowUps {
                native: &mut crate::native::NativeSession::inert(),
                theme: &mut crate::bridge::ThemeBridge::new(None),
                speculate: crate::speculate::SpeculationClock::default(),
            },
            CutoverInput {
                presink: vec![Msg::EngineRequest(EngineRequest::VimEnter {
                    token: ReplyToken { msgid: 1 },
                })],
                pending_redraw: vec![UiEvent::Flush],
                resize: Some((100, 40)),
                keys: vec![key("a")],
            },
            || view_core::msg::ExitInfo {
                code: None,
                by_signal: false,
            },
        );

        assert!(matches!(outcome, CutoverOutcome::Continue));
        // the failed reply is the only call made: pending damage, the attach
        // probe, the resize and the key are all skipped once the engine is
        // lost
        let calls = executor.into_ops().calls.into_inner();
        assert_eq!(calls, vec!["reply(1,Nil)"]);
        assert!(!model.content_painted);
    }

    /// The takeover a real session performs is triggered here, not by
    /// `runtime::run`'s loop: a config that sources quickly fires `VimEnter`
    /// into the presink, and nothing else in the process resolves that.
    #[test]
    fn a_presink_vim_enter_hands_the_surfaces_over_after_answering_nvim() {
        use view_core::msg::{EngineRequest, ReplyToken};

        let ops = crate::engine_ops::FakeOps::default();
        let executor = crate::runtime::Executor::new(ops);
        let mut model = Model::with_term_size(80, 24);
        let mut native = crate::native::NativeSession::all_enabled(7, None);

        let outcome = run_cutover(
            &mut model,
            &executor,
            &mut crate::runtime::FollowUps {
                native: &mut native,
                theme: &mut crate::bridge::ThemeBridge::new(None),
                speculate: crate::speculate::SpeculationClock::default(),
            },
            CutoverInput {
                presink: vec![Msg::EngineRequest(EngineRequest::VimEnter {
                    token: ReplyToken { msgid: 1 },
                })],
                pending_redraw: vec![],
                resize: None,
                keys: vec![],
            },
            || view_core::msg::ExitInfo {
                code: None,
                by_signal: false,
            },
        );

        assert!(matches!(outcome, CutoverOutcome::Continue));
        let calls = executor.into_ops().calls.into_inner();
        let answered = calls.iter().position(|c| c == "reply(1,Nil)");
        let took_over = calls
            .iter()
            .position(|c| c.starts_with("hold_option(laststatus"));
        assert!(
            answered < took_over && answered.is_some(),
            "nvim's blocking request is answered before the takeover it \
             unblocks: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c.starts_with("hold_option(laststatus")),
            "the planned option takeover must reach the engine: {calls:?}"
        );
        let registrations: Vec<&String> = calls
            .iter()
            .filter(|c| c.starts_with("register_mappings("))
            .collect();
        assert_eq!(
            registrations.len(),
            1,
            "the keys register exactly once per session: {calls:?}"
        );
        assert!(
            registrations[0].contains("ff") && registrations[0].ends_with(",7)"),
            "the registration carries this session's keys and channel: {registrations:?}"
        );
    }
}
