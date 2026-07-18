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
use view_tui::terminal::Term;

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
    term: &mut Term,
) -> anyhow::Result<(Model, i32)> {
    let executor = Executor::new(engine.handle.clone());

    loop {
        // paint before blocking, not after processing: state mutated ahead
        // of the loop (the startup cutover replays staged messages straight
        // through dispatch) would otherwise sit unpainted until the next
        // message happens to arrive. Steady-state behavior is unchanged --
        // each processed wakeup paints here on the next pass, immediately,
        // with no timer, no recv_timeout, no tick anywhere in this loop.
        if model.dirty {
            let surface = view_surface::render(&model);
            term.draw_surface(&model, &surface)?; // terminal I/O errors abort; engine errors never do
            model.dirty = false;
        }
        let msg = match msg_rx.recv() {
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
            for eff in update(&mut model, msg) {
                match executor.run(eff) {
                    Flow::Continue => {}
                    // run() owns engine: returning here runs Drop (graceful
                    // qa! then kill)
                    Flow::Quit(code) => return Ok((model, code)),
                    // an engine write failed: the engine is gone, not the
                    // UI; resolve the real exit status and let update()
                    // decide
                    Flow::EngineLost => queue.push(Msg::EngineDown(engine.wait_exit())),
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
}
