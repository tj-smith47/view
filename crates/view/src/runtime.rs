//! The unified runtime loop: one blocking `recv()` wakes on damage, input,
//! or engine-request tokens, with no timer anywhere in the loop body. There
//! is no fixed post-redraw silence timeout and no input-drain budget:
//! painting fires the instant `update()` marks the model dirty, and a
//! keystroke wakes the loop directly instead of waiting for the next poll.
//!
//! # Ownership chain
//!
//! [`run`] takes ownership of [`Engine`] for the duration of the call: the
//! reader and writer threads spawned by `Engine::spawn` live for exactly as
//! long as `run` is on the stack. `Engine`'s `Drop` (a graceful `qa!`, then a
//! bounded wait, then `SIGKILL`) runs exactly once, whenever `run` returns --
//! a clean quit via `Flow::Quit`, or a terminal I/O error propagated with
//! `?`. The caller in `main.rs` never touches `Engine` again once it has
//! been handed to `run`.

use std::sync::mpsc;
use view_core::model::Model;
use view_core::msg::{Effect, ExitInfo, Msg, ReplyToken, ReplyValue, RpcCall};
use view_core::update::update;
use view_engine::handle::{EngineError, EngineHandle};
use view_engine::process::Engine;
use view_engine::stall::OutboxStallWatch;
use view_tui::terminal::Term;

/// What the user is told while the engine has stopped accepting view's
/// output.
///
/// Fixed text, carrying neither a live duration nor a queue depth: the
/// notice is re-asserted on every loop pass for as long as the stall lasts,
/// and text that changed between passes would repaint the toast on each of
/// them to tell the operator nothing they can act on that this does not.
const ENGINE_STALLED_NOTICE: &str =
    "nvim has stopped reading view's input; keystrokes are queued until it resumes";

/// The notify surface [`Executor`] drives, factored out from [`EngineHandle`]
/// so its effect-to-call mapping is testable against a recording fake
/// instead of a live nvim connection.
pub trait EngineOps {
    /// Forwards one encoded key notation via `nvim_input`.
    fn input(&self, notation: &str) -> Result<(), EngineError>;
    /// Notifies nvim of a terminal resize via `nvim_ui_try_resize`.
    fn try_resize(&self, width: u16, height: u16) -> Result<(), EngineError>;
    /// Streams pasted text via `nvim_paste`.
    fn paste(&self, text: &str) -> Result<(), EngineError>;
    /// Forwards one mouse event via `nvim_input_mouse`.
    fn input_mouse(
        &self,
        button: &str,
        action: &str,
        modifier: &str,
        row: u16,
        col: u16,
    ) -> Result<(), EngineError>;
    /// Answers a request nvim is blocked on.
    fn reply(&self, token: ReplyToken, value: ReplyValue) -> Result<(), EngineError>;
    /// Issues an async `nvim_get_hl(0, {name = "Normal"})` probe tagged
    /// with `generation`; never blocks, and never itself returns the reply
    /// (see `Msg::HlProbeReply`).
    fn probe_default_hl(&self, generation: u64) -> Result<(), EngineError>;
}

impl EngineOps for EngineHandle {
    fn input(&self, notation: &str) -> Result<(), EngineError> {
        self.input(notation)
    }
    fn try_resize(&self, width: u16, height: u16) -> Result<(), EngineError> {
        self.try_resize(width, height)
    }
    fn paste(&self, text: &str) -> Result<(), EngineError> {
        self.paste(text)
    }
    fn input_mouse(
        &self,
        button: &str,
        action: &str,
        modifier: &str,
        row: u16,
        col: u16,
    ) -> Result<(), EngineError> {
        self.input_mouse(button, action, modifier, row, col)
    }
    fn reply(&self, token: ReplyToken, value: ReplyValue) -> Result<(), EngineError> {
        self.reply(token, value)
    }
    fn probe_default_hl(&self, generation: u64) -> Result<(), EngineError> {
        self.probe_default_hl(generation)
    }
}

// blanket impl over `&T`: lets a test hold a `FakeOps` by reference (so it
// can inspect recorded calls after `Executor::run` moves ownership) the same
// way `Executor::new(engine.handle.clone())` holds an owned `EngineHandle` in
// production, without needing two different construction paths.
impl<T: EngineOps + ?Sized> EngineOps for &T {
    fn input(&self, notation: &str) -> Result<(), EngineError> {
        (**self).input(notation)
    }
    fn try_resize(&self, width: u16, height: u16) -> Result<(), EngineError> {
        (**self).try_resize(width, height)
    }
    fn paste(&self, text: &str) -> Result<(), EngineError> {
        (**self).paste(text)
    }
    fn input_mouse(
        &self,
        button: &str,
        action: &str,
        modifier: &str,
        row: u16,
        col: u16,
    ) -> Result<(), EngineError> {
        (**self).input_mouse(button, action, modifier, row, col)
    }
    fn reply(&self, token: ReplyToken, value: ReplyValue) -> Result<(), EngineError> {
        (**self).reply(token, value)
    }
    fn probe_default_hl(&self, generation: u64) -> Result<(), EngineError> {
        (**self).probe_default_hl(generation)
    }
}

/// What the runtime loop does after one effect crosses [`Executor::run`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Keep processing the current effect batch.
    Continue,
    /// Quit with the given exit code; the caller returns immediately.
    Quit(i32),
    /// An engine write failed: the engine connection is gone, not the UI.
    /// The caller resolves the real exit status and requeues
    /// `Msg::EngineDown` rather than aborting.
    EngineLost,
}

/// Carries out [`Effect`]s against an [`EngineOps`] connection. Never
/// blocks: every `Effect::Rpc` maps onto a fire-and-forget notify call, and
/// `run` performs zero `request` calls of its own (startup owns the only
/// requests the process makes).
pub struct Executor<E: EngineOps> {
    ops: E,
}

impl<E: EngineOps> Executor<E> {
    /// Wraps `ops` for the runtime loop to drive.
    pub fn new(ops: E) -> Self {
        Self { ops }
    }

    /// Unwraps back to the owned `ops`, so a test can inspect what a fake
    /// recorded after driving `Executor` through a call it does not
    /// otherwise expose a getter for.
    #[cfg(test)]
    pub(crate) fn into_ops(self) -> E {
        self.ops
    }

    /// Carries out one effect, infallibly by signature: an engine-write
    /// failure never becomes an `Err` that would abort the UI, since the
    /// `Flow::EngineLost` -> `Msg::EngineDown` path exists precisely to
    /// resolve that case through the same loop that handles a clean exit.
    #[must_use]
    pub fn run(&self, eff: Effect) -> Flow {
        match eff {
            Effect::Rpc(call) => {
                let result = match call {
                    RpcCall::Input { notation } => self.ops.input(&notation),
                    RpcCall::TryResize { width, height } => self.ops.try_resize(width, height),
                    RpcCall::Paste { text } => self.ops.paste(&text),
                    RpcCall::InputMouse {
                        button,
                        action,
                        modifier,
                        row,
                        col,
                    } => self.ops.input_mouse(&button, &action, &modifier, row, col),
                    RpcCall::GetDefaultHl { generation } => self.ops.probe_default_hl(generation),
                    // RpcCall is #[non_exhaustive]: a future call kind must
                    // degrade to a no-op here rather than fail to compile
                    _ => return Flow::Continue,
                };
                match result {
                    Ok(()) => Flow::Continue,
                    Err(_) => Flow::EngineLost,
                }
            }
            Effect::Reply { token, value } => match self.ops.reply(token, value) {
                Ok(()) => Flow::Continue,
                Err(_) => Flow::EngineLost,
            },
            Effect::Quit { exit_code } => Flow::Quit(exit_code),
            // Effect is #[non_exhaustive]: same degrade-to-no-op rule
            _ => Flow::Continue,
        }
    }
}

/// Applies `msg` to `model` through the ordinary `update()` -> `Executor`
/// path, stopping early on the first non-`Continue` flow. A pub(crate) seam
/// so `main.rs`'s pre-run replay of the pre-attach buffer (see
/// `startup::drain_pre_attach`) can drive the same dispatch `run()`'s loop
/// uses, instead of hand-rolling a second copy of "call `update`, then run
/// every effect through the executor." Deliberately does not replicate
/// `run()`'s loop machinery for `Quit`, residue draining, or `EngineLost`
/// requeueing: none of it is reachable from the `Msg::Key`/`Msg::Resized`
/// messages replay ever sends (see `view_core::update::update`), so
/// reproducing it here would be dead code, not defensive coverage.
#[must_use]
pub(crate) fn dispatch<E: EngineOps>(model: &mut Model, executor: &Executor<E>, msg: Msg) -> Flow {
    crate::vlog::log_msg(&msg);
    let mut flow = Flow::Continue;
    for eff in update(model, msg) {
        match executor.run(eff) {
            Flow::Continue => {}
            other => {
                flow = other;
                break;
            }
        }
    }
    flow
}

/// Reads the engine's write side once and raises or retracts the stalled-
/// engine notice on `model` to match what it finds. Returns whether the
/// visible message set changed, which is the caller's cue to repaint.
///
/// Costs two relaxed atomic loads plus one walk of the message log per
/// call, and never takes the engine's writer lock --
/// a wedge is precisely the state in which that lock is held by a thread
/// parked inside a write, so a check that wanted it could not report the
/// one condition it exists for.
///
/// Re-asserted rather than edge-triggered: `msg_clear` empties the log
/// wholesale, and a notice raised once on the way in would be gone for good
/// while the condition it describes is still true.
fn note_write_stall(
    model: &mut Model,
    watch: &mut OutboxStallWatch,
    handle: &EngineHandle,
) -> bool {
    let stalled = watch.observe(handle);
    model
        .engine
        .messages
        .set_native_condition(stalled.then_some(ENGINE_STALLED_NOTICE))
}

/// Runs the unified loop until `update()` produces `Effect::Quit` or a
/// terminal I/O error occurs, returning the final `Model` alongside the
/// process exit code on the former (the caller persists the model's
/// last-derived theme for the next startup's cold-start cache; see
/// `theme_cache` in `main.rs`).
///
/// Takes ownership of `engine` for the whole call (see the module docs'
/// ownership chain), plus the already-attached `pump` and the `msg_rx` end
/// of the channel the caller's input thread and `pump`'s sink both already
/// feed. Both are built by `startup` rather than here: the input thread
/// starts (and `msg_tx`/`msg_rx` are created) right after the very first
/// shell frame paints, well before this function is ever called, so a key
/// typed while the engine is still attaching is never lost to a
/// not-yet-existing channel -- see `startup::drain_pre_attach` for the
/// buffering that covers exactly that window. The executor drives
/// `engine.handle` through [`EngineOps`]. There is no timer anywhere in the
/// loop body: painting fires immediately when `update()` marks
/// `model.dirty`, and the loop's only blocking call is `msg_rx.recv()`,
/// which a redraw, a keystroke, or an engine request wakes directly.
///
/// # Errors
///
/// Returns the underlying `std::io::Error` if a terminal paint fails (the
/// `Model` is dropped on this path along with everything else on the
/// stack; an aborted session has no last-good theme worth persisting).
pub fn run(
    mut model: Model,
    mut engine: Engine,
    pump: view_engine::DamagePump,
    msg_rx: mpsc::Receiver<Msg>,
    term_size: view_tui::terminal::TermSizeCell,
    term: &mut Term,
) -> anyhow::Result<(Model, i32)> {
    let executor = Executor::new(engine.handle.clone());
    let mut write_stall = OutboxStallWatch::default();

    loop {
        // a resize the input thread has already seen describes the terminal
        // as it is now, whatever traffic is still queued ahead of its
        // Msg::Resized: folding it in here means no frame is ever painted
        // at a shape the terminal has left. Costs one relaxed load per pass
        // when nothing resized, which is the whole steady state.
        if let Some((width, height)) = term_size.take() {
            match dispatch(&mut model, &executor, Msg::Resized { width, height }) {
                Flow::Continue => {}
                Flow::Quit(code) => return Ok((model, code)),
                Flow::EngineLost => {
                    let info = engine.wait_exit();
                    if let Flow::Quit(code) = dispatch(&mut model, &executor, Msg::EngineDown(info))
                    {
                        return Ok((model, code));
                    }
                }
            }
        }
        // checked here, immediately before the paint that would show it: an
        // engine that has stopped reading view's output also sends no
        // redraws, so nothing else in this loop can notice, and the
        // keystrokes the operator keeps typing at an editor that has gone
        // quiet are the wakeups that bring the check back around
        if note_write_stall(&mut model, &mut write_stall, &engine.handle) {
            model.dirty = true;
        }
        // paint before blocking, not after processing: state mutated ahead
        // of the loop (the startup cutover replays staged messages straight
        // through dispatch) would otherwise sit unpainted until the next
        // message happens to arrive. Steady-state behavior is unchanged --
        // each processed wakeup paints here on the next pass, immediately,
        // with no timer, no recv_timeout, no tick anywhere in this loop.
        if model.dirty {
            let surface = view_surface::render(&model);
            let damage = model.take_paint_damage();
            term.draw_surface(&model, &surface, &damage)?; // terminal I/O errors abort; engine errors never do
            model.dirty = false;
        }
        let received = msg_rx.recv();
        #[cfg(feature = "bench-taps")]
        if received.is_ok() {
            view_tui::tap::tap(view_tui::tap::TAG_LOOP_WAKE);
        }
        let msg = match received {
            Ok(Msg::RedrawReady) => Msg::Redraw(pump.take_damage()),
            Ok(Msg::EngineStopped(reason)) => {
                // stashed on the model rather than reported here: this loop
                // runs behind the terminal's raw-mode alternate screen, so
                // `main` reports it only after `run` returns and the
                // terminal is restored (see Msg::EngineStopped's doc)
                model.fatal_reason = reason;
                Msg::EngineDown(engine.wait_exit())
            }
            Ok(m) => m,
            Err(_) => Msg::EngineDown(ExitInfo {
                code: None,
                by_signal: false,
            }),
        };
        let mut queue = vec![msg];
        let mut drained_residue = false;
        while let Some(msg) = queue.pop() {
            crate::vlog::log_msg(&msg);
            for eff in update(&mut model, msg) {
                match executor.run(eff) {
                    Flow::Continue => {}
                    // run() owns engine: returning here runs Drop (graceful
                    // qa! then kill)
                    Flow::Quit(code) => return Ok((model, code)),
                    // an engine write failed: the engine is gone, not the
                    // UI; resolve the real exit status and let update()
                    // decide
                    // the rest of this batch targets an engine that is
                    // already gone: running it would fail identically and
                    // queue a duplicate EngineDown per remaining effect
                    Flow::EngineLost => {
                        queue.push(Msg::EngineDown(engine.wait_exit()));
                        break;
                    }
                }
            }
            // a RedrawReady is dropped when the shared channel is
            // momentarily full (the pump disarms pending so a later fold
            // retries); this drain makes a stranded batch impossible: a
            // full channel guarantees another queued wakeup, and every
            // wakeup runs this before the loop can sleep. Once per wakeup,
            // so a sustained storm still paints per batch instead of
            // starving the frame.
            if queue.is_empty() && !drained_residue {
                drained_residue = true;
                let residue = pump.take_damage();
                if !residue.is_empty() {
                    queue.push(Msg::Redraw(residue));
                }
            }
        }
    }
}

/// Records every call `Executor::run` makes through [`EngineOps`] instead of
/// touching a real engine connection, so the executor's effect-to-call
/// mapping is provable without a live nvim. `pub(crate)` (not confined to
/// this module's own `mod tests`) so `startup`'s cutover tests can drive the
/// exact same fake through `runtime::dispatch` without a second, duplicate
/// implementation.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct FakeOps {
    pub(crate) calls: std::cell::RefCell<Vec<String>>,
    pub(crate) fail_next: std::cell::RefCell<bool>,
}

#[cfg(test)]
impl FakeOps {
    fn record(&self, call: String) -> Result<(), EngineError> {
        self.calls.borrow_mut().push(call);
        if *self.fail_next.borrow() {
            Err(EngineError::Closed)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
impl EngineOps for FakeOps {
    fn input(&self, notation: &str) -> Result<(), EngineError> {
        self.record(format!("input({notation})"))
    }
    fn try_resize(&self, width: u16, height: u16) -> Result<(), EngineError> {
        self.record(format!("try_resize({width},{height})"))
    }
    fn paste(&self, text: &str) -> Result<(), EngineError> {
        self.record(format!("paste({text})"))
    }
    fn input_mouse(
        &self,
        button: &str,
        action: &str,
        modifier: &str,
        row: u16,
        col: u16,
    ) -> Result<(), EngineError> {
        self.record(format!(
            "input_mouse({button},{action},{modifier},{row},{col})"
        ))
    }
    fn reply(&self, token: ReplyToken, value: ReplyValue) -> Result<(), EngineError> {
        self.record(format!("reply({},{value:?})", token.msgid))
    }
    fn probe_default_hl(&self, generation: u64) -> Result<(), EngineError> {
        self.record(format!("probe_default_hl({generation})"))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn input_effect_maps_to_engine_ops_input() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Rpc(RpcCall::Input {
            notation: "x".into(),
        }));
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(ops.calls.borrow()[0], "input(x)");
    }

    #[test]
    fn try_resize_effect_maps_to_engine_ops_try_resize() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Rpc(RpcCall::TryResize {
            width: 120,
            height: 40,
        }));
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(ops.calls.borrow()[0], "try_resize(120,40)");
    }

    #[test]
    fn paste_effect_maps_to_engine_ops_paste() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Rpc(RpcCall::Paste { text: "hi".into() }));
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(ops.calls.borrow()[0], "paste(hi)");
    }

    #[test]
    fn input_mouse_effect_maps_to_engine_ops_input_mouse() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Rpc(RpcCall::InputMouse {
            button: "left".into(),
            action: "press".into(),
            modifier: "C-".into(),
            row: 3,
            col: 7,
        }));
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(ops.calls.borrow()[0], "input_mouse(left,press,C-,3,7)");
    }

    #[test]
    fn reply_effect_maps_to_engine_ops_reply() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Reply {
            token: ReplyToken { msgid: 9 },
            value: ReplyValue::Nil,
        });
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(ops.calls.borrow()[0], "reply(9,Nil)");
    }

    #[test]
    fn get_default_hl_effect_maps_to_engine_ops_probe_default_hl() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Rpc(RpcCall::GetDefaultHl { generation: 4 }));
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(ops.calls.borrow()[0], "probe_default_hl(4)");
    }

    #[test]
    fn get_default_hl_write_failure_returns_engine_lost() {
        let ops = FakeOps::default();
        *ops.fail_next.borrow_mut() = true;
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Rpc(RpcCall::GetDefaultHl { generation: 1 }));
        assert!(matches!(flow, Flow::EngineLost));
    }

    #[test]
    fn quit_effect_returns_flow_quit_with_exit_code_and_touches_no_engine_op() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Quit { exit_code: 5 });
        assert!(matches!(flow, Flow::Quit(5)));
        assert!(ops.calls.borrow().is_empty());
    }

    #[test]
    fn engine_write_failure_returns_engine_lost_never_an_err() {
        let ops = FakeOps::default();
        *ops.fail_next.borrow_mut() = true;
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Rpc(RpcCall::Input {
            notation: "x".into(),
        }));
        assert!(matches!(flow, Flow::EngineLost));
    }

    #[test]
    fn reply_write_failure_also_returns_engine_lost() {
        let ops = FakeOps::default();
        *ops.fail_next.borrow_mut() = true;
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Reply {
            token: ReplyToken { msgid: 1 },
            value: ReplyValue::Nil,
        });
        assert!(matches!(flow, Flow::EngineLost));
    }

    #[test]
    fn dispatch_a_key_forwards_input_and_returns_continue() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let mut model = Model::with_term_size(80, 24);
        let flow = dispatch(
            &mut model,
            &executor,
            Msg::Key(view_core::msg::Key {
                notation: "x".into(),
            }),
        );
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(ops.calls.borrow()[0], "input(x)");
    }

    #[test]
    fn dispatch_reports_engine_lost_without_a_second_send_when_the_write_fails() {
        let ops = FakeOps::default();
        *ops.fail_next.borrow_mut() = true;
        let executor = Executor::new(&ops);
        let mut model = Model::with_term_size(80, 24);
        let flow = dispatch(
            &mut model,
            &executor,
            Msg::Key(view_core::msg::Key {
                notation: "x".into(),
            }),
        );
        assert!(matches!(flow, Flow::EngineLost));
        assert_eq!(ops.calls.borrow().len(), 1);
    }

    #[test]
    fn dispatch_a_resize_forwards_try_resize() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let mut model = Model::with_term_size(80, 24);
        let flow = dispatch(
            &mut model,
            &executor,
            Msg::Resized {
                width: 100,
                height: 50,
            },
        );
        assert!(matches!(flow, Flow::Continue));
        assert!(ops.calls.borrow()[0].starts_with("try_resize("));
    }

    #[test]
    fn a_resize_already_folded_in_costs_nothing_when_its_message_arrives() {
        // the loop folds a published size in ahead of the paint gate, so
        // the Msg::Resized for that same size reaches dispatch afterwards.
        // Re-running the arm would dirty the model and send nvim a second
        // TryResize for a change that already happened.
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let mut model = Model::with_term_size(80, 24);
        let resize = Msg::Resized {
            width: 100,
            height: 50,
        };
        assert!(matches!(
            dispatch(&mut model, &executor, resize.clone()),
            Flow::Continue
        ));
        model.dirty = false;
        assert!(matches!(
            dispatch(&mut model, &executor, resize),
            Flow::Continue
        ));
        assert_eq!(
            ops.calls.borrow().len(),
            1,
            "the repeated resize must not reach the engine again: {:?}",
            ops.calls.borrow()
        );
        assert!(
            !model.dirty,
            "a resize to the size already applied must not force a repaint"
        );
    }

    /// Recreates re-enqueueing buffered keys onto the same bounded
    /// `sync_channel` `main.rs`'s `msg_tx` is (capacity
    /// `startup::MSG_CHANNEL_CAPACITY`, tied by definition to
    /// `startup::KEY_RING_CAPACITY`) while nothing is consuming it yet
    /// (`runtime::run`'s loop starts only after cutover). 2 keys already
    /// resting in the
    /// channel stand in for whatever the input thread queued in the narrow
    /// gap between attach completing and cutover actually running; 64 more
    /// (the ring's full capacity) is what a maximally-full pre-attach buffer
    /// replays. 66 sends against a capacity-64 channel with zero consumer
    /// must block on the 65th.
    ///
    /// Proven with the literal channel primitive rather than by calling
    /// production code directly (`main.rs`'s cutover no longer re-enqueues
    /// onto `msg_tx` at all, so there is nothing left in that shape to
    /// call): the hazard is a pure channel-capacity/no-consumer property,
    /// independent of which code happens to perform the sends, so
    /// recreating the same capacity and send pattern is a faithful,
    /// deterministic, environment-independent reproduction -- unlike a
    /// live pty race, whose window this crate's own
    /// `view-oracle` pty test found unreliable to hit even at full ring
    /// occupancy (see that test's doc comment).
    #[test]
    fn re_enqueueing_replayed_keys_onto_a_full_bounded_channel_with_no_consumer_blocks_forever() {
        let (tx, rx) = mpsc::sync_channel::<Msg>(crate::startup::MSG_CHANNEL_CAPACITY);
        for _ in 0..2 {
            tx.send(Msg::Key(view_core::msg::Key {
                notation: "leftover".into(),
            }))
            .unwrap();
        }
        let buffered: Vec<Msg> = (0..crate::startup::MSG_CHANNEL_CAPACITY)
            .map(|_| {
                Msg::Key(view_core::msg::Key {
                    notation: "x".into(),
                })
            })
            .collect();

        let handle = std::thread::spawn(move || {
            for msg in buffered {
                // the re-enqueue shape a channel-based replay would use:
                // `msg_tx.send(Msg::Key(key))`
                tx.send(msg).unwrap();
            }
        });

        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            !handle.is_finished(),
            "replay finished instead of blocking -- 2 leftover + 64 \
             buffered is 66 sends against a channel of capacity 64 with \
             zero consumer; if this now finishes on its own, the channel's \
             capacity or this test's premise has changed and the hazard \
             model this test's evidence relies on needs revisiting"
        );
        // unblocks the deliberately-leaked sender thread so it can exit
        // cleanly rather than being abandoned mid-block
        drop(rx);
        let _ = handle.join();
    }

    /// Paired with the blocking test above: `dispatch`-based replay
    /// delivers a flood far larger than `KEY_RING_CAPACITY` (64) directly
    /// through `update()`/`Executor`, touching no channel at all, so there
    /// is no capacity to exceed regardless of flood size. `dispatch` itself
    /// (see its definition above) never references `mpsc` -- this is a
    /// structural guarantee, not a probabilistic one -- and this test backs
    /// that reading with an observed bound: 1000 dispatches (15x
    /// `KEY_RING_CAPACITY`) complete near-instantly rather than blocking.
    #[test]
    fn dispatching_a_flood_of_keys_directly_never_touches_any_channel_and_completes_immediately() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let mut model = Model::with_term_size(80, 24);
        let start = std::time::Instant::now();
        for i in 0..1000 {
            let flow = dispatch(
                &mut model,
                &executor,
                Msg::Key(view_core::msg::Key {
                    notation: i.to_string(),
                }),
            );
            assert!(matches!(flow, Flow::Continue));
        }
        assert!(
            start.elapsed() < std::time::Duration::from_millis(500),
            "1000 direct dispatches took {:?}, unexpectedly slow for a \
             channel-free path",
            start.elapsed()
        );
        assert_eq!(ops.calls.borrow().len(), 1000);
    }

    /// A write sink that reports entering a write and then stays inside it
    /// until released, standing in for a peer that has stopped reading its
    /// stdin.
    struct StuckSink {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    impl std::io::Write for StuckSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _ = self.entered.send(());
            // dropping the release end frees this and every later write, so
            // a run that has taken its reading can let the peer recover
            let _ = self.release.recv();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A read source that never yields and never ends, so the connection's
    /// reader thread cannot close the connection out from under a test that
    /// is only interested in the write side.
    struct IdleSource(mpsc::Receiver<()>);

    impl std::io::Read for IdleSource {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            let _ = self.0.recv();
            Ok(0)
        }
    }

    /// Seconds a run is given to observe the writer thread reach the sink.
    ///
    /// Arming rather than measurement: until the writer is provably inside
    /// a write that cannot finish, there is no stall to detect and a
    /// reading taken early says nothing.
    const STUCK_WRITE_ARM_SECS: u64 = 30;

    /// The stall threshold the tests below run against, in place of the
    /// shipping ten seconds.
    ///
    /// The predicate is the same one either way -- it compares an elapsed
    /// duration against whatever threshold the watch was built with -- so
    /// the only thing a real-length run would add to these assertions is
    /// ten seconds of suite time per test.
    const TEST_STALL_THRESHOLD: std::time::Duration = std::time::Duration::from_millis(50);

    /// Waits for `probe` to hold, failing the test rather than hanging if
    /// it never does.
    fn wait_until(what: &str, mut probe: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !probe() {
            assert!(std::time::Instant::now() < deadline, "timed out: {what}");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Runs `body` against a live connection whose peer never reads: the
    /// writer thread parks inside its first write until `body` drops the
    /// release it is handed, exactly as a wedged nvim would park it.
    ///
    /// Both the notification receiver and the reader's block outlive the
    /// body here rather than inside it: dropping either ends the reader
    /// thread and closes the connection, which would retire the write side
    /// under measurement for a reason that has nothing to do with the peer.
    fn with_wedged_peer(body: impl FnOnce(&EngineHandle, &mpsc::Receiver<()>, mpsc::Sender<()>)) {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (reader_tx, reader_rx) = mpsc::channel();
        let (handle, notifications) = EngineHandle::start(
            IdleSource(reader_rx),
            StuckSink {
                entered: entered_tx,
                release: release_rx,
            },
        );
        body(&handle, &entered_rx, release_tx);
        drop(notifications);
        drop(reader_tx);
    }

    #[test]
    fn a_wedged_engine_raises_the_notice_and_retracts_it_when_the_writer_moves_again() {
        with_wedged_peer(|handle, entered_rx, release_tx| {
            let mut model = Model::with_term_size(80, 24);
            let mut watch = OutboxStallWatch::new(TEST_STALL_THRESHOLD);

            handle.input("a").unwrap();
            entered_rx
                .recv_timeout(std::time::Duration::from_secs(STUCK_WRITE_ARM_SECS))
                .expect("the writer thread reached the sink");
            // queued behind a write that cannot finish, so the backlog
            // outlives the message the writer is holding
            handle.input("b").unwrap();

            // the stall is measured from an observation, never asserted by
            // the first one: nothing has yet been seen to stop moving
            assert!(!note_write_stall(&mut model, &mut watch, handle));
            assert!(model.engine.messages.entries.is_empty());

            std::thread::sleep(TEST_STALL_THRESHOLD * 3);
            assert!(note_write_stall(&mut model, &mut watch, handle));
            assert_eq!(
                model.engine.messages.visible_lines(4),
                vec![ENGINE_STALLED_NOTICE.to_string()]
            );

            // asked again while nothing changed: the notice is re-asserted
            // on every pass, and an unchanged notice must not repaint
            assert!(!note_write_stall(&mut model, &mut watch, handle));

            // a keypress dismisses transient toasts; this one describes a
            // condition that is still true, and the keypress that would
            // drop it is the one it exists to explain
            assert!(!model.engine.messages.dismiss_transient_on_keypress(false));
            assert_eq!(
                model.engine.messages.visible_lines(4),
                vec![ENGINE_STALLED_NOTICE.to_string()]
            );

            drop(release_tx);
            wait_until("the writer drains its backlog", || {
                handle.write_progress().0 == 0
            });
            assert!(note_write_stall(&mut model, &mut watch, handle));
            assert!(model.engine.messages.entries.is_empty());
        });
    }

    #[test]
    fn an_engine_that_keeps_writing_never_raises_the_notice() {
        with_wedged_peer(|handle, entered_rx, release_tx| {
            let mut model = Model::with_term_size(80, 24);
            let mut watch = OutboxStallWatch::new(TEST_STALL_THRESHOLD);
            // released up front: every write returns as soon as it starts,
            // which is a healthy peer expressed through the same sink as
            // the wedged one
            drop(release_tx);

            for i in 0..20 {
                handle.input("a").unwrap();
                entered_rx
                    .recv_timeout(std::time::Duration::from_secs(STUCK_WRITE_ARM_SECS))
                    .expect("the writer thread reached the sink");
                assert!(
                    !note_write_stall(&mut model, &mut watch, handle),
                    "a delivering writer read as stalled on write {i}"
                );
                std::thread::sleep(TEST_STALL_THRESHOLD / 2);
            }
            assert!(model.engine.messages.entries.is_empty());
        });
    }

    #[test]
    fn an_idle_engine_with_nothing_queued_never_raises_the_notice() {
        with_wedged_peer(|handle, _entered_rx, release_tx| {
            let mut model = Model::with_term_size(80, 24);
            let mut watch = OutboxStallWatch::new(TEST_STALL_THRESHOLD);
            drop(release_tx);

            handle.input("a").unwrap();
            wait_until("the writer drains its backlog", || {
                handle.write_progress().0 == 0
            });
            assert!(!note_write_stall(&mut model, &mut watch, handle));
            std::thread::sleep(TEST_STALL_THRESHOLD * 3);
            assert!(!note_write_stall(&mut model, &mut watch, handle));
            assert!(model.engine.messages.entries.is_empty());
        });
    }
}
