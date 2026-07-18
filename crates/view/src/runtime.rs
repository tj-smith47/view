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

/// Runs the unified loop until `update()` produces `Effect::Quit` or a
/// terminal I/O error occurs, returning the process exit code on the
/// former.
///
/// Takes ownership of `engine` for the whole call (see the module docs'
/// ownership chain): the reader thread feeds `msg_tx` directly via
/// [`Engine::start_pump`], a dedicated input thread feeds it key and resize
/// events via [`view_tui::terminal::spawn_input_thread`], and the executor
/// drives `engine.handle` through [`EngineOps`]. There is no timer anywhere
/// in the loop body: painting fires immediately when `update()` marks
/// `model.dirty`, and the loop's only blocking call is `msg_rx.recv()`,
/// which a redraw, a keystroke, or an engine request wakes directly.
///
/// # Errors
///
/// Returns the underlying `std::io::Error` if a terminal paint fails.
pub fn run(mut model: Model, mut engine: Engine, term: &mut Term) -> anyhow::Result<i32> {
    let (msg_tx, msg_rx) = mpsc::sync_channel(64);
    let pump = engine.start_pump(msg_tx.clone());
    view_tui::terminal::spawn_input_thread(msg_tx);
    let executor = Executor::new(engine.handle.clone());

    loop {
        let msg = match msg_rx.recv() {
            Ok(Msg::RedrawReady) => Msg::Redraw(pump.take_damage()),
            Ok(Msg::EngineStopped) => Msg::EngineDown(engine.wait_exit()),
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
                    Flow::Quit(code) => return Ok(code),
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
        // paint immediately when update marked dirty: there is no timer, no
        // recv_timeout, no tick anywhere in this loop
        if model.dirty {
            let surface = view_surface::render(&model);
            term.draw_surface(&model, &surface)?; // terminal I/O errors abort; engine errors never do
            model.dirty = false;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::cell::RefCell;
    use view_core::msg::{Effect, ReplyToken, ReplyValue, RpcCall};
    use view_engine::handle::EngineError;

    /// Records every call `Executor::run` makes through [`EngineOps`]
    /// instead of touching a real engine connection, so the executor's
    /// effect-to-call mapping is provable without a live nvim.
    #[derive(Default)]
    struct FakeOps {
        calls: RefCell<Vec<String>>,
        fail_next: RefCell<bool>,
    }

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
}
