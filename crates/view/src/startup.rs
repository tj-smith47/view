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
use std::sync::mpsc::{Receiver, SyncSender};
use std::time::Instant;

use view_core::events::UiEvent;
use view_core::model::Model;
use view_core::msg::{Key, Msg};
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
/// Debug builds log the elapsed time since `process_start` to stderr: the
/// design spec's informal 50ms shell-paint target, measured here but not
/// enforced (the formal budget gate lands with the bench harness).
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
    log_shell_paint_latency(process_start);
    Ok(())
}

#[cfg(debug_assertions)]
fn log_shell_paint_latency(process_start: Instant) {
    eprintln!(
        "view: shell frame painted {:?} after process start\r",
        process_start.elapsed()
    );
}

#[cfg(not(debug_assertions))]
fn log_shell_paint_latency(_process_start: Instant) {}

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
    let engine = Engine::spawn(cfg).map_err(AttachFailure::Spawn)?;
    engine
        .handle
        .register_vim_enter_autocmd(engine.api_info.channel_id)
        .map_err(AttachFailure::Attach)?;
    engine
        .handle
        .register_bridge(engine.api_info.channel_id)
        .map_err(AttachFailure::Attach)?;
    engine
        .handle
        .ui_attach(width, height)
        .map_err(AttachFailure::Attach)?;
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
    msg_tx: SyncSender<Msg>,
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
}

/// Drains `msg_rx` until [`attach_in_background`]'s `Msg::EngineReady`
/// marker arrives, buffering every `Msg::Key` seen in the meantime into a
/// bounded ring of [`KEY_RING_CAPACITY`] (oldest dropped first once full,
/// each drop repainting an updated toast through the normal
/// `view_core::model::Messages` overlay via `Messages::push_native` --
/// never silent), and tracking the latest `Msg::Resized` seen, if any.
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
pub fn drain_pre_attach(
    msg_rx: &Receiver<Msg>,
    model: &mut Model,
    term: &mut Term,
) -> DrainedInput {
    drain_pre_attach_with(msg_rx, model, |model| {
        let surface = view_surface::render(model);
        let _ = term.draw_surface(model, &surface, &view_core::grid::GridDamage::full());
    })
}

/// The accumulation logic behind [`drain_pre_attach`], generic over the
/// overflow repaint so tests can supply a no-op (or call-counting) closure
/// instead of a real [`Term`].
fn drain_pre_attach_with(
    msg_rx: &Receiver<Msg>,
    model: &mut Model,
    mut repaint: impl FnMut(&mut Model),
) -> DrainedInput {
    let mut ring = KeyRing::new(KEY_RING_CAPACITY);
    let mut resize = None;
    let mut dropped: u32 = 0;
    loop {
        match msg_rx.recv() {
            Ok(Msg::Key(key)) => {
                if ring.push(key) {
                    dropped = dropped.saturating_add(1);
                    let plural = if dropped == 1 { "" } else { "s" };
                    model.engine.messages.push_native(
                        format!(
                            "view: startup key buffer full, dropped {dropped} keystroke{plural}"
                        ),
                        dropped > 1,
                    );
                    repaint(model);
                }
            }
            Ok(Msg::Resized { width, height }) => {
                // applied to the model here, not only carried to the
                // cutover: the overflow repaint above paints from the
                // model's terminal size, so a resize that arrives during
                // the attach window would otherwise leave every repaint
                // for the rest of that window addressing rows and columns
                // the terminal no longer has. Still carried, because the
                // cutover owes nvim its own `TryResize` and this loop has
                // no engine to send one to.
                model.term_width = width;
                model.term_height = height;
                resize = Some((width, height));
            }
            Ok(Msg::EngineReady) => break,
            // structurally unreachable before EngineReady (see
            // attach_in_background's doc comment); kept for the same
            // defensive-totality reason update()'s own no-op arms are
            Ok(_) => {}
            // the input thread and the attach thread are both gone: nothing
            // left to wait for
            Err(_) => break,
        }
    }
    DrainedInput {
        resize,
        keys: ring.drain(),
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
/// resolve first; every key still sitting in `msg_tx` after this call
/// returns was typed by the input thread after `drain_pre_attach` observed
/// `Msg::EngineReady`, which is after every key in `keys` was already
/// buffered, so applying `keys` here, before `msg_tx` is read again by
/// `runtime::run`'s loop, reproduces arrival order exactly. The resize (if
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
pub(crate) fn run_cutover<E: crate::runtime::EngineOps>(
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
            Msg::EngineStopped(reason) => {
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
            crate::runtime::Flow::EngineLost => {
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
            let ops = crate::runtime::FakeOps::default();
            let executor = crate::runtime::Executor::new(ops);
            let mut model = Model::with_term_size(80, 24);
            model.content_painted = false;
            let outcome = run_cutover(
                &mut model,
                &executor,
                &mut crate::runtime::FollowUps {
                    native: &mut crate::native::NativeSession::inert(),
                    theme: &mut crate::bridge::ThemeBridge::new(None),
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
        // presink's VimEnter reply, then the resize, then every buffered
        // key, in that exact order -- the arrival order run_cutover's doc
        // comment claims
        let expected_len = 2 + KEY_RING_CAPACITY;
        assert_eq!(calls.len(), expected_len);
        assert_eq!(calls[0], "reply(1,Nil)");
        assert!(calls[1].starts_with("try_resize("));
        assert_eq!(calls[2], "input(0)");
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
        let ops = crate::runtime::FakeOps::default();
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
            },
            CutoverInput {
                presink: vec![Msg::EngineStopped(None)],
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
        // update(): Quit short-circuits everything after it
        assert!(!model.content_painted);
        assert!(executor.into_ops().calls.into_inner().is_empty());
    }

    /// A presink `Msg::EngineStopped(Some(reason))` stashes the reason on
    /// `model.fatal_reason` before translating to `Msg::EngineDown`, the
    /// same as a live one does in `runtime::run`'s own loop: `main.rs`
    /// reads it off the returned model after the terminal is restored,
    /// never from a direct write inside the reader thread itself.
    #[test]
    fn run_cutover_stashes_a_presink_engine_stopped_reason_on_the_model() {
        let ops = crate::runtime::FakeOps::default();
        let executor = crate::runtime::Executor::new(ops);
        let mut model = Model::with_term_size(80, 24);

        let outcome = run_cutover(
            &mut model,
            &executor,
            &mut crate::runtime::FollowUps {
                native: &mut crate::native::NativeSession::inert(),
                theme: &mut crate::bridge::ThemeBridge::new(None),
            },
            CutoverInput {
                presink: vec![Msg::EngineStopped(Some("wedged reader".to_string()))],
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

        let ops = crate::runtime::FakeOps::default();
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
        // the failed reply is the only call made: pending damage, the
        // resize, and the key are all skipped once the engine is lost
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

        let ops = crate::runtime::FakeOps::default();
        let executor = crate::runtime::Executor::new(ops);
        let mut model = Model::with_term_size(80, 24);
        let mut native = crate::native::NativeSession::all_enabled(7, None);

        let outcome = run_cutover(
            &mut model,
            &executor,
            &mut crate::runtime::FollowUps {
                native: &mut native,
                theme: &mut crate::bridge::ThemeBridge::new(None),
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
        assert_eq!(
            calls.first().map(String::as_str),
            Some("reply(1,Nil)"),
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
