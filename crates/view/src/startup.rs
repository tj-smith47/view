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
use std::sync::mpsc::Receiver;
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
/// attaches at `width`x`height`, and forwards the capability-probe's
/// leftover `residue` bytes. Deliberately does not call
/// [`Engine::start_pump`]: only `main.rs` does, once the buffered
/// pre-attach window has been fully replayed (see
/// [`attach_in_background`]'s doc comment for why).
fn spawn_and_attach(
    cfg: EngineConfig,
    width: u16,
    height: u16,
    residue: Vec<u8>,
) -> Result<Engine, AttachFailure> {
    // read before `Engine::spawn` consumes `cfg` by value: there is no
    // config left to ask afterward, and the choice below depends on it
    let stdin_relay = cfg.stdin_relay_requested();
    let engine = Engine::spawn(cfg).map_err(AttachFailure::Spawn)?;
    crate::vlog::log_with("engine", || {
        format!("spawned pid={} stdin_relay={stdin_relay}", engine.pid())
    });
    register_and_attach(engine, stdin_relay, width, height, residue)
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
/// the teardown is [`Engine::wait_exit`]'s graceful-then-forced sequence,
/// the same one `Engine::restart` performs by dropping, so no replacement is
/// ever brought up alongside a live connection -- but the caller still holds
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
    // its cached status and this returns at once
    let _ = engine.wait_exit();
    let engine = Engine::spawn_recovering(cfg).map_err(AttachFailure::Spawn)?;
    crate::vlog::log_with("engine", || {
        format!(
            "restarted pid={} stdin_relay={stdin_relay} grid={width}x{height}",
            engine.pid()
        )
    });
    register_and_attach(engine, stdin_relay, width, height, Vec::new())
}

/// The half of [`spawn_and_attach`] that runs against a child that is
/// already up: the `VimEnter` autocmd, the `view_bridge` group, the attach
/// itself, and the terminal handshake's leftover bytes.
fn register_and_attach(
    engine: Engine,
    stdin_relay: bool,
    width: u16,
    height: u16,
    residue: Vec<u8>,
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
    // best-effort, matching this project's original startup ordering: a
    // write failure here means the connection is already gone, which the
    // caller discovers through the engine's own EngineDown path moments
    // later rather than through this loop
    for notation in view_tui::keys::encode_residue_bytes(&residue) {
        let _ = engine.handle.input(&notation);
    }
    Ok(engine)
}

/// Runs [`spawn_and_attach`] on a background thread so a slow-starting
/// nvim can never delay [`paint_shell_frame`], and returns a receiver that
/// yields its result exactly once, success or failure. The same background
/// thread also sends `Msg::EngineReady` down `msg_tx` right after, so
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
pub fn attach_in_background(
    cfg: EngineConfig,
    width: u16,
    height: u16,
    residue: Vec<u8>,
    msg_tx: crate::wake::LoopSender,
) -> Receiver<Result<Engine, AttachFailure>> {
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = spawn_and_attach(cfg, width, height, residue);
        let _ = result_tx.send(result);
        // sent unconditionally, success or failure: drain_pre_attach must
        // wake up either way, so main.rs can move on to read engine_rx and
        // report the failure instead of blocking forever on an attach that
        // will never come
        let _ = msg_tx.send(Msg::EngineReady);
    });
    result_rx
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
/// replay sends.
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

    // ahead of everything staged: this is the one point both the first
    // attach and every replacement pass through, and what it asks the
    // connection must be asked before that connection can park itself at a
    // prompt nobody answers (see `SWAP_RECOVERY_PROBE`)
    match crate::runtime::dispatch(model, executor, follow_ups, Msg::EngineAttached) {
        crate::runtime::Flow::Continue => {}
        crate::runtime::Flow::Quit(code) => return CutoverOutcome::Quit(code),
        crate::runtime::Flow::EngineLost | crate::runtime::Flow::RestartEngine => {
            engine_alive = false;
        }
    }

    for msg in presink {
        if !engine_alive {
            break;
        }
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

        let content = std::env::temp_dir().join(format!(
            "view-startup-spawn-and-attach-stdin-relay-{}.txt",
            std::process::id()
        ));
        std::fs::write(&content, "hello from spawn_and_attach\n").unwrap();
        let source = std::fs::File::open(&content).unwrap();

        let cfg = EngineConfig::isolated()
            .with_arg("-")
            .with_stdin_relay(source.as_fd().try_clone_to_owned().unwrap());
        let mut engine = spawn_and_attach(cfg, 80, 24, Vec::new()).unwrap();

        assert_eq!(
            engine.handle.eval_str("getline(1)").unwrap(),
            "hello from spawn_and_attach",
            "spawn_and_attach must call ui_attach_with_stdin_relay, not \
             plain ui_attach, whenever EngineConfig::stdin_relay_requested() \
             is true, or the fd nvim was told to read from never gets wired \
             up at all"
        );
        let _ = engine.wait_exit();
        std::fs::remove_file(&content).ok();
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
        // the attach probe the cutover opens with, then presink's VimEnter
        // reply and the second probe that reply carries, then the resize,
        // then every buffered key, in that exact order -- the arrival order
        // run_cutover's doc comment claims
        let expected_len = 4 + KEY_RING_CAPACITY;
        assert_eq!(calls.len(), expected_len);
        assert_eq!(calls[0], "probe_swap_recovery(1)");
        assert_eq!(calls[1], "reply(1,Nil)");
        assert_eq!(calls[2], "probe_swap_recovery(2)");
        assert!(calls[3].starts_with("try_resize("));
        assert_eq!(calls[4], "input(0)");
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
        // update(): Quit short-circuits everything after the attach probe
        // the cutover opens with
        assert!(!model.content_painted);
        assert_eq!(
            executor.into_ops().calls.into_inner(),
            vec!["probe_swap_recovery(1)"]
        );
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
        // the failed write is the only call made: the presink reply, pending
        // damage, the resize and the key are all skipped once the engine is
        // lost, whichever of the cutover's writes discovered it
        let calls = executor.into_ops().calls.into_inner();
        assert_eq!(calls, vec!["probe_swap_recovery(1)"]);
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
