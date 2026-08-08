//! The unified runtime loop: one blocking `recv()` wakes on damage, input,
//! or engine-request tokens. There is no fixed post-redraw silence timeout
//! and no input-drain budget: painting fires the instant `update()` marks
//! the model dirty, and a keystroke wakes the loop directly instead of
//! waiting for the next poll.
//!
//! The wait carries a deadline in exactly one state: output queued for the
//! engine and not yet delivered (see [`wait_for_msg`]) -- the one state in
//! which, were that output wedged, no wakeup would otherwise come. An idle
//! editor still sleeps until
//! something happens, and the cost of the exception is bounded by the stall
//! threshold rather than by a frame rate.
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

use crate::bridge::ThemeBridge;
use crate::native::NativeSession;
use std::sync::mpsc;
use view_core::model::Model;
use view_core::msg::{
    Effect, ExitInfo, Msg, OptionValue, RegisterType, ReplyToken, ReplyValue, RpcCall,
};
use view_core::native::mappings::MappingSpec;
use view_core::update::update;
use view_engine::handle::{EngineError, EngineHandle};
use view_engine::process::Engine;
use view_engine::stall::OutboxStallWatch;
use view_tui::terminal::Term;

/// What the user is told while the engine has stopped accepting view's
/// output.
///
/// The consequence leads and the diagnosis follows, because the toast
/// overlay truncates at the tail to fit the grid: on a narrow terminal the
/// operator keeps the half that says their typing is not lost. Fixed text,
/// carrying neither a live duration nor a queue depth, since the notice is
/// re-asserted on every loop pass for as long as the stall lasts and text
/// that changed between passes would repaint the toast on each of them to
/// say nothing more actionable.
const ENGINE_STALLED_NOTICE: &str = "keystrokes queued: nvim has stopped reading view's output";

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
    /// Sets one nvim option via `nvim_set_option_value`, the channel every
    /// non-interactive option change rides (see `RpcCall::SetOption`).
    fn set_option(&self, name: &str, value: &OptionValue) -> Result<(), EngineError>;
    /// Sets one nvim option and keeps it there for the session, the durable
    /// takeover a superseded plugin cannot undo (see `RpcCall::HoldOption`).
    fn hold_option(&self, name: &str, value: &OptionValue) -> Result<(), EngineError>;
    /// Answers a request nvim is blocked on.
    fn reply(&self, token: ReplyToken, value: ReplyValue) -> Result<(), EngineError>;
    /// Issues an async `nvim_get_hl(0, {name = "Normal"})` probe tagged
    /// with `generation`; never blocks, and never itself returns the reply
    /// (see `Msg::HlProbeReply`).
    fn probe_default_hl(&self, generation: u64) -> Result<(), EngineError>;
    /// Registers this session's default keys and the `:View` command in one
    /// chunk; never blocks, and never itself returns the claims (see
    /// `Msg::MappingsClaimed`).
    fn register_mappings(&self, specs: &[MappingSpec], channel_id: u64) -> Result<(), EngineError>;
    /// Registers the one `view_bridge` autocmd group carrying every editor
    /// state change view reacts to; never blocks, and never itself returns an
    /// event (see `RpcCall::RegisterBridge`).
    fn register_bridge(&self, channel_id: u64) -> Result<(), EngineError>;
    /// Injects view's `g:clipboard` provider, conditionally on the user's
    /// own config leaving it unset; never blocks, and never itself answers
    /// a paste or copy request (see `RpcCall::RegisterClipboard`).
    fn register_clipboard(&self, channel_id: u64) -> Result<(), EngineError>;
    /// Enumerates listed, loaded buffers for `Source::Buffers`, tagged
    /// `generation`; never blocks, and never itself returns the list (see
    /// `Msg::PickerBufferList`).
    fn list_buffers(&self, generation: u64) -> Result<(), EngineError>;
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
    fn set_option(&self, name: &str, value: &OptionValue) -> Result<(), EngineError> {
        self.set_option(name, value)
    }
    fn hold_option(&self, name: &str, value: &OptionValue) -> Result<(), EngineError> {
        self.hold_option(name, value)
    }
    fn reply(&self, token: ReplyToken, value: ReplyValue) -> Result<(), EngineError> {
        self.reply(token, value)
    }
    fn probe_default_hl(&self, generation: u64) -> Result<(), EngineError> {
        self.probe_default_hl(generation)
    }
    fn register_mappings(&self, specs: &[MappingSpec], channel_id: u64) -> Result<(), EngineError> {
        self.register_mappings(specs, channel_id)
    }
    fn register_bridge(&self, channel_id: u64) -> Result<(), EngineError> {
        self.register_bridge(channel_id)
    }
    fn register_clipboard(&self, channel_id: u64) -> Result<(), EngineError> {
        self.register_clipboard(channel_id)
    }
    fn list_buffers(&self, generation: u64) -> Result<(), EngineError> {
        self.list_buffers(generation)
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
    fn set_option(&self, name: &str, value: &OptionValue) -> Result<(), EngineError> {
        (**self).set_option(name, value)
    }
    fn hold_option(&self, name: &str, value: &OptionValue) -> Result<(), EngineError> {
        (**self).hold_option(name, value)
    }
    fn reply(&self, token: ReplyToken, value: ReplyValue) -> Result<(), EngineError> {
        (**self).reply(token, value)
    }
    fn probe_default_hl(&self, generation: u64) -> Result<(), EngineError> {
        (**self).probe_default_hl(generation)
    }
    fn register_mappings(&self, specs: &[MappingSpec], channel_id: u64) -> Result<(), EngineError> {
        (**self).register_mappings(specs, channel_id)
    }
    fn register_bridge(&self, channel_id: u64) -> Result<(), EngineError> {
        (**self).register_bridge(channel_id)
    }
    fn register_clipboard(&self, channel_id: u64) -> Result<(), EngineError> {
        (**self).register_clipboard(channel_id)
    }
    fn list_buffers(&self, generation: u64) -> Result<(), EngineError> {
        (**self).list_buffers(generation)
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
    /// The clipboard worker's job channel (`crate::clipboard::spawn`), or
    /// `None` when no worker is wired -- every test `Executor` built via
    /// plain `new`, and the only state a `ClipboardRead`/`ClipboardWrite`
    /// effect has to check before it must degrade to a direct reply rather
    /// than silently drop the token (see `run`'s match arms below and
    /// `EngineRequest`'s one-reply-per-token contract).
    clipboard: Option<mpsc::Sender<crate::clipboard::ClipboardJob>>,
    /// The terminal's OSC52 job channel, drained synchronously by `run`'s
    /// loop on the thread that owns `Term` (see `Effect::Osc52Copy`'s doc
    /// for why this cannot be a write from the clipboard worker thread).
    osc52: Option<mpsc::Sender<Osc52Job>>,
    /// The loop's own message channel, cloned into every one-shot toast
    /// timer thread `Effect::ScheduleToastExpiry` spawns (see `run`'s match
    /// arm below). `None` degrades a scheduled expiry to a silent no-op --
    /// the toast then simply outlives its intended timeout instead of the
    /// executor panicking or blocking, matching every other unwired-channel
    /// degrade in this type.
    toast_timer: Option<mpsc::SyncSender<Msg>>,
    /// The matcher worker's query channel (`view_native::picker::matcher::spawn`),
    /// or `None` when no worker is wired -- every test `Executor` built via
    /// plain `new`. `PickerQuery` carries no `ReplyToken` (see that
    /// effect's own doc), so an unwired channel degrades to a silent no-op
    /// the same way `Osc52Copy`/`ScheduleToastExpiry` do below, not the
    /// must-answer-the-token shape `ClipboardRead`/`Write` need.
    picker: Option<mpsc::Sender<view_native::picker::matcher::WorkerRequest>>,
}

/// One OSC52 clipboard-set escape to write, queued by [`Executor::run`] and
/// drained by [`run`]'s loop into [`view_tui::terminal::Term::write_osc52`].
pub struct Osc52Job {
    pub register: char,
    pub lines: Vec<String>,
    pub regtype: RegisterType,
}

impl<E: EngineOps> Executor<E> {
    /// Wraps `ops` for the runtime loop to drive, with neither the
    /// clipboard worker nor the OSC52 terminal channel wired: every
    /// existing call site stays source-compatible, and a bare `Executor`
    /// (every test today) still answers a clipboard effect safely (see
    /// `run`'s match arms) rather than needing every one of them updated to
    /// wire a channel it does not otherwise exercise.
    pub fn new(ops: E) -> Self {
        Self {
            ops,
            clipboard: None,
            osc52: None,
            toast_timer: None,
            picker: None,
        }
    }

    /// Wires the clipboard worker's job channel; `ClipboardRead`/
    /// `ClipboardWrite` effects forward to it instead of self-answering.
    #[must_use]
    pub fn with_clipboard(mut self, tx: mpsc::Sender<crate::clipboard::ClipboardJob>) -> Self {
        self.clipboard = Some(tx);
        self
    }

    /// Wires the terminal's OSC52 job channel; `Osc52Copy` effects forward
    /// to it instead of silently no-oping.
    #[must_use]
    pub fn with_osc52(mut self, tx: mpsc::Sender<Osc52Job>) -> Self {
        self.osc52 = Some(tx);
        self
    }

    /// Wires the loop's own message channel; `ScheduleToastExpiry` effects
    /// spawn a one-shot timer thread against it instead of silently
    /// no-oping.
    #[must_use]
    pub fn with_toast_timer(mut self, tx: mpsc::SyncSender<Msg>) -> Self {
        self.toast_timer = Some(tx);
        self
    }

    /// Wires the matcher worker's query channel; `PickerQuery`/`PickerClose`
    /// effects forward to it instead of silently no-oping.
    #[must_use]
    pub fn with_picker(
        mut self,
        tx: mpsc::Sender<view_native::picker::matcher::WorkerRequest>,
    ) -> Self {
        self.picker = Some(tx);
        self
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
                    RpcCall::SetOption { name, value } => self.ops.set_option(&name, &value),
                    RpcCall::HoldOption { name, value } => self.ops.hold_option(&name, &value),
                    RpcCall::GetDefaultHl { generation } => self.ops.probe_default_hl(generation),
                    RpcCall::RegisterMappings { specs, channel_id } => {
                        self.ops.register_mappings(&specs, channel_id)
                    }
                    RpcCall::RegisterBridge { channel_id } => self.ops.register_bridge(channel_id),
                    RpcCall::RegisterClipboard { channel_id } => {
                        self.ops.register_clipboard(channel_id)
                    }
                    RpcCall::ListBuffers { generation } => self.ops.list_buffers(generation),
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
            // forwarded to the clipboard worker when one is wired; when
            // none is (every bare test Executor), the token still must be
            // answered exactly once, so this replies here directly with the
            // safest default rather than silently dropping it the way an
            // ordinary unmapped fire-and-forget RpcCall may
            Effect::ClipboardRead { token, register } => {
                match &self.clipboard {
                    Some(tx) => {
                        let job = crate::clipboard::ClipboardJob {
                            token,
                            kind: crate::clipboard::ClipboardJobKind::Read { register },
                        };
                        if tx.send(job).is_err() {
                            // the worker thread is gone; still owe the
                            // token exactly one reply -- charwise-empty is
                            // the safest default a paste of nothing can
                            // report, matching an unreachable clipboard
                            let _ = self.ops.reply(
                                token,
                                ReplyValue::ClipboardLines {
                                    lines: Vec::new(),
                                    regtype: RegisterType::Charwise,
                                },
                            );
                        }
                    }
                    None => {
                        let _ = self.ops.reply(
                            token,
                            ReplyValue::ClipboardLines {
                                lines: Vec::new(),
                                regtype: RegisterType::Charwise,
                            },
                        );
                    }
                }
                Flow::Continue
            }
            Effect::ClipboardWrite {
                token,
                register,
                lines,
                regtype,
            } => {
                match &self.clipboard {
                    Some(tx) => {
                        let job = crate::clipboard::ClipboardJob {
                            token,
                            kind: crate::clipboard::ClipboardJobKind::Write {
                                register,
                                lines,
                                regtype,
                            },
                        };
                        if tx.send(job).is_err() {
                            let _ = self.ops.reply(token, ReplyValue::Nil);
                        }
                    }
                    None => {
                        let _ = self.ops.reply(token, ReplyValue::Nil);
                    }
                }
                Flow::Continue
            }
            // carries no ReplyToken (see the effect's own doc): nothing on
            // the wire is blocked on this, so an unwired osc52 channel (or
            // one whose receiver is gone) costs nothing beyond the escape
            // never being written -- an ordinary fire-and-forget degrade,
            // unlike the two effects above
            Effect::Osc52Copy {
                register,
                lines,
                regtype,
            } => {
                if let Some(tx) = &self.osc52 {
                    let _ = tx.send(Osc52Job {
                        register,
                        lines,
                        regtype,
                    });
                }
                Flow::Continue
            }
            Effect::Quit { exit_code } => Flow::Quit(exit_code),
            // one-shot, mirroring `view_tui::terminal::spawn_input_thread`'s
            // shape: a background thread that owns exactly one send, never
            // a persistent multi-deadline scheduler. The loop has no
            // free-running clock of its own (see this module's own doc), so
            // this thread -- not a paint-time check -- is what wakes it back
            // up on an otherwise-idle editor.
            Effect::ScheduleToastExpiry { id, after } => {
                if let Some(tx) = &self.toast_timer {
                    let tx = tx.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(after);
                        let _ = tx.send(Msg::ToastExpired { id });
                    });
                }
                Flow::Continue
            }
            // carries no ReplyToken (see the effect's own doc): forwarded
            // to the matcher worker when one is wired, silently dropped
            // otherwise -- the worker, not this arm, owns streaming back
            // `Msg::PickerResults`
            Effect::PickerQuery {
                generation,
                needle,
                source,
                resolved,
            } => {
                if let Some(tx) = &self.picker {
                    let _ = tx.send(view_native::picker::matcher::WorkerRequest::Query(
                        view_native::picker::matcher::MatchRequest {
                            generation,
                            needle,
                            source,
                            resolved,
                        },
                    ));
                }
                Flow::Continue
            }
            // carries no ReplyToken (see the effect's own doc): forwarded to
            // the matcher worker when one is wired, silently dropped
            // otherwise, the same unwired-channel degrade every other
            // fire-and-forget effect here uses
            Effect::PickerClose => {
                if let Some(tx) = &self.picker {
                    let _ = tx.send(view_native::picker::matcher::WorkerRequest::Close);
                }
                Flow::Continue
            }
            // Effect is #[non_exhaustive]: same degrade-to-no-op rule
            _ => Flow::Continue,
        }
    }
}

/// The session-scoped reactors [`dispatch`] drives after `update()`'s own
/// effects have run: state that answers a message without being part of the
/// pure model, and that therefore cannot live in `Model`.
///
/// One parameter rather than one per reactor, because every path that
/// dispatches a message carries all of them: they are attached to a session,
/// not to a message, so splitting them across a signature only spreads the
/// same borrow over more call sites.
pub struct FollowUps<'a> {
    /// Native-feature takeover, key claim reporting and the first-run notice.
    pub native: &'a mut NativeSession,
    /// The cold-start theme cache's mid-session writer.
    pub theme: &'a mut ThemeBridge,
}

/// Applies `msg` to `model` through the ordinary `update()` -> `Executor`
/// path, then whatever `native` owes the same message, stopping early on the
/// first non-`Continue` flow. A pub(crate) seam so `main.rs`'s pre-run replay
/// of the pre-attach buffer (see `startup::drain_pre_attach`) can drive the
/// same dispatch `run()`'s loop uses, instead of hand-rolling a second copy
/// of "call `update`, then run every effect through the executor."
/// Deliberately does not replicate `run()`'s loop machinery for `Quit`,
/// residue draining, or `EngineLost` requeueing: none of it is reachable from
/// the `Msg::Key`/`Msg::Resized` messages replay ever sends (see
/// `view_core::update::update`), so reproducing it here would be dead code,
/// not defensive coverage.
///
/// The native follow-up runs here rather than at either loop, because both
/// loops resolve the two messages it hangs off: nvim's `VimEnter` lands in
/// the pump's presink whenever it fires before the sink attaches, and in
/// `msg_rx` whenever it fires after. It runs after `update()`'s own effects
/// so nvim's blocking `VimEnter` request is answered before the takeover it
/// unblocks, and so the first-run notice reads claims `update()` has already
/// recorded.
///
/// The theme-cache follow-up sits at the same seam for the same reason, and
/// before the native one because it emits no effects at all: it reads the
/// highlight state `update()` just produced and, at most once per
/// colorscheme change, writes it out.
#[must_use]
pub(crate) fn dispatch<E: EngineOps>(
    model: &mut Model,
    executor: &Executor<E>,
    follow_ups: &mut FollowUps<'_>,
    msg: Msg,
) -> Flow {
    crate::vlog::log_msg(&msg);
    let stage = crate::native::stage(&msg);
    let trigger = follow_ups.theme.classify(&msg);
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
    if flow != Flow::Continue {
        return flow;
    }
    follow_ups.theme.follow_up(model, trigger);
    for eff in follow_ups.native.follow_up(model, stage) {
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

/// Waits for the loop's next message, bounded by the stall watch's deadline
/// when it has one. `None` means the wait expired with nothing delivered
/// and the caller should re-read the write side.
///
/// Unbounded whenever `watch` asks for no wakeup, which is the entire idle
/// steady state: an editor with nothing queued sleeps until a keystroke, a
/// redraw or an engine request wakes it, exactly as it always has, and pays
/// no periodic wakeup for a condition that cannot be true. A deadline
/// exists only while output is pending -- delivering or wedged, the watch
/// cannot know yet, and for the wedged case the wakeup is the point: a
/// wedged engine emits no redraws, so an operator who types once and then
/// waits would otherwise be told nothing at all.
fn wait_for_msg(
    msg_rx: &mpsc::Receiver<Msg>,
    watch: &OutboxStallWatch,
) -> Option<Result<Msg, mpsc::RecvError>> {
    let Some(deadline) = watch.poll_deadline() else {
        return Some(msg_rx.recv());
    };
    match msg_rx.recv_timeout(deadline) {
        Ok(msg) => Some(Ok(msg)),
        Err(mpsc::RecvTimeoutError::Timeout) => None,
        Err(mpsc::RecvTimeoutError::Disconnected) => Some(Err(mpsc::RecvError)),
    }
}

/// Runs the unified loop until `update()` produces `Effect::Quit` or a
/// terminal I/O error occurs, returning the final `Model` alongside the
/// process exit code on the former (the caller persists the model's
/// last-derived theme for the next startup's cold-start cache; see
/// `theme_cache` in `main.rs`).
///
/// The message channel's two halves, bundled into one parameter: `rx` is
/// `run()`'s own blocking receive end, while `tx` is cloned once more into
/// the toast-expiry timer thread the same way `spawn_input_thread` and
/// `start_pump` already hold their own clones from before `run()` starts.
/// A bare tuple would satisfy the same arg-count constraint but loses the
/// field names at every call site; a struct keeps `tx`/`rx` self-labeling
/// where `run()`'s doc comment already talks about both by name.
pub struct MsgChannel {
    pub tx: mpsc::SyncSender<Msg>,
    pub rx: mpsc::Receiver<Msg>,
}

/// Takes ownership of `engine` for the whole call (see the module docs'
/// ownership chain), plus the already-attached `pump` and the `msg_rx` end
/// of the channel the caller's input thread and `pump`'s sink both already
/// feed. Both are built by `startup` rather than here: the input thread
/// starts (and `msg_tx`/`msg_rx` are created) right after the very first
/// shell frame paints, well before this function is ever called, so a key
/// typed while the engine is still attaching is never lost to a
/// not-yet-existing channel -- see `startup::drain_pre_attach` for the
/// buffering that covers exactly that window. The executor drives
/// `engine.handle` through [`EngineOps`]. There is no periodic timer in the
/// loop body: painting fires immediately when `update()` marks
/// `model.dirty`, and the loop blocks in [`wait_for_msg`], which a redraw,
/// a keystroke, or an engine request wakes directly -- unbounded except
/// while engine-bound output is pending, where the stall deadline bounds
/// the sleep.
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
    msg_channel: MsgChannel,
    term_size: view_tui::terminal::TermSizeCell,
    follow_ups: &mut FollowUps<'_>,
    term: &mut Term,
) -> anyhow::Result<(Model, i32)> {
    let MsgChannel {
        tx: msg_tx,
        rx: msg_rx,
    } = msg_channel;
    let (clipboard_tx, clipboard_rx) = mpsc::channel();
    let (osc52_tx, osc52_rx) = mpsc::channel();
    // kept alive for the session's duration; the worker exits once
    // `clipboard_tx` (held by `executor`) drops at the end of this
    // function, same lifetime as the engine's own reader/writer threads
    let _clipboard_worker = crate::clipboard::spawn(engine.handle.clone(), clipboard_rx)?;
    let (picker_tx, picker_rx) = mpsc::channel();
    // kept alive for the process's duration, the same shape as
    // `_clipboard_worker` above: the matcher worker exits once `picker_tx`
    // (held by `executor`) drops at the end of this function
    let _picker_worker = view_native::picker::matcher::spawn(picker_rx, msg_tx.clone());
    let executor = Executor::new(engine.handle.clone())
        .with_clipboard(clipboard_tx)
        .with_osc52(osc52_tx)
        .with_toast_timer(msg_tx)
        .with_picker(picker_tx);
    let mut write_stall = OutboxStallWatch::default();

    loop {
        // drained at the top of every pass rather than right after the
        // dispatch that queued it: nothing blocks between here and the
        // bottom of the previous pass's own dispatch loop, so this is
        // effectively immediate, and one drain site covers every dispatch
        // call this loop makes (resize, below, and the main queue) instead
        // of one per call site
        while let Ok(job) = osc52_rx.try_recv() {
            term.write_osc52(
                job.register,
                &view_native::clipboard::lines_to_text(&job.lines, job.regtype),
            )?;
        }
        // a resize the input thread has already seen describes the terminal
        // as it is now, whatever traffic is still queued ahead of its
        // Msg::Resized: folding it in here means no frame is ever painted
        // at a shape the terminal has left. Costs one relaxed load per pass
        // when nothing resized, which is the whole steady state.
        if let Some((width, height)) = term_size.take() {
            match dispatch(
                &mut model,
                &executor,
                follow_ups,
                Msg::Resized { width, height },
            ) {
                Flow::Continue => {}
                Flow::Quit(code) => return Ok((model, code)),
                Flow::EngineLost => {
                    let info = engine.wait_exit();
                    if let Flow::Quit(code) =
                        dispatch(&mut model, &executor, follow_ups, Msg::EngineDown(info))
                    {
                        return Ok((model, code));
                    }
                }
            }
        }
        // checked here, immediately before the paint that would show it: an
        // engine that has stopped reading view's output also sends no
        // redraws, so nothing else in this loop can notice
        if note_write_stall(&mut model, &mut write_stall, &engine.handle) {
            model.dirty = true;
        }
        // paint before blocking, not after processing: state mutated ahead
        // of the loop (the startup cutover replays staged messages straight
        // through dispatch) would otherwise sit unpainted until the next
        // message happens to arrive. Steady-state behavior is unchanged --
        // each processed wakeup paints here on the next pass, immediately,
        // with no post-redraw silence timeout and no input-drain budget.
        if model.dirty {
            let surface = view_surface::render(&model);
            let damage = model.take_paint_damage();
            term.draw_surface(&model, &surface, &damage)?; // terminal I/O errors abort; engine errors never do
            model.dirty = false;
        }
        let Some(received) = wait_for_msg(&msg_rx, &write_stall) else {
            // the wait expired against the stall watch's own deadline
            // rather than delivering anything: go around and re-read the
            // write side, which is the whole reason the deadline was armed
            continue;
        };
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
            match dispatch(&mut model, &executor, follow_ups, msg) {
                Flow::Continue => {}
                // run() owns engine: returning here runs Drop (graceful
                // qa! then kill)
                Flow::Quit(code) => return Ok((model, code)),
                // an engine write failed: the engine is gone, not the UI;
                // resolve the real exit status and let update() decide. The
                // rest of that batch targeted an engine that is already
                // gone, which is why `dispatch` stops on the first failure
                // rather than queueing a duplicate EngineDown per remaining
                // effect
                Flow::EngineLost => queue.push(Msg::EngineDown(engine.wait_exit())),
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
    fn set_option(&self, name: &str, value: &OptionValue) -> Result<(), EngineError> {
        self.record(format!("set_option({name},{value:?})"))
    }
    fn hold_option(&self, name: &str, value: &OptionValue) -> Result<(), EngineError> {
        self.record(format!("hold_option({name},{value:?})"))
    }
    fn reply(&self, token: ReplyToken, value: ReplyValue) -> Result<(), EngineError> {
        self.record(format!("reply({},{value:?})", token.msgid))
    }
    fn probe_default_hl(&self, generation: u64) -> Result<(), EngineError> {
        self.record(format!("probe_default_hl({generation})"))
    }
    fn register_mappings(&self, specs: &[MappingSpec], channel_id: u64) -> Result<(), EngineError> {
        let keys: Vec<&str> = specs.iter().map(|s| s.lhs).collect();
        self.record(format!(
            "register_mappings({},{channel_id})",
            keys.join(" ")
        ))
    }
    fn register_bridge(&self, channel_id: u64) -> Result<(), EngineError> {
        self.record(format!("register_bridge({channel_id})"))
    }
    fn register_clipboard(&self, channel_id: u64) -> Result<(), EngineError> {
        self.record(format!("register_clipboard({channel_id})"))
    }
    fn list_buffers(&self, generation: u64) -> Result<(), EngineError> {
        self.record(format!("list_buffers({generation})"))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// `Messages::visible_lines` returns one span-row per line; these tests
    /// only assert on the text a stall notice carries, so this flattens
    /// each row back to a plain string.
    fn visible_texts(model: &Model) -> Vec<String> {
        model
            .engine
            .messages
            .visible_lines(4)
            .into_iter()
            .map(|spans| spans.into_iter().map(|s| s.text).collect::<String>())
            .collect()
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
    fn set_option_effect_maps_to_engine_ops_set_option() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Rpc(RpcCall::SetOption {
            name: "laststatus".into(),
            value: OptionValue::Int(0),
        }));
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(ops.calls.borrow()[0], "set_option(laststatus,Int(0))");
    }

    #[test]
    fn hold_option_effect_maps_to_engine_ops_hold_option() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Rpc(RpcCall::HoldOption {
            name: "laststatus".into(),
            value: OptionValue::Int(0),
        }));
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(ops.calls.borrow()[0], "hold_option(laststatus,Int(0))");
    }

    #[test]
    fn hold_option_write_failure_returns_engine_lost() {
        let ops = FakeOps::default();
        *ops.fail_next.borrow_mut() = true;
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Rpc(RpcCall::HoldOption {
            name: "laststatus".into(),
            value: OptionValue::Int(0),
        }));
        assert!(matches!(flow, Flow::EngineLost));
    }

    /// Every entry a supersession plan produces has to reach the engine.
    /// An effect the executor does not recognize degrades to a silent
    /// no-op by design, which for a takeover means view believing it owns a
    /// surface nvim is still drawing.
    #[test]
    fn every_supersession_entry_reaches_an_engine_op() {
        let plan = view_native::supersede::plan(
            &view_native::config::NativeConfig::all_enabled(),
            view_core::native::registry::features(),
        );
        assert!(!plan.is_empty(), "the all-enabled plan must not be empty");
        for entry in &plan {
            let ops = FakeOps::default();
            let executor = Executor::new(&ops);
            let flow = executor.run(Effect::Rpc(entry.rpc.clone()));
            assert!(matches!(flow, Flow::Continue));
            assert_eq!(
                ops.calls.borrow().len(),
                1,
                "{}'s takeover reached no engine op: {:?}",
                entry.feature,
                entry.rpc
            );
        }
    }

    /// The entry-point plan reaches the engine, on the same terms as a
    /// supersession entry: an effect the executor does not recognize
    /// degrades to a silent no-op, which for a registration means keys the
    /// user is told about in a doctor listing and a `:View` command that
    /// never existed.
    #[test]
    fn register_mappings_effect_maps_to_engine_ops_register_mappings() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let call = view_native::mappings::register_plan(
            &view_native::config::NativeConfig::all_enabled(),
            7,
        );
        let flow = executor.run(Effect::Rpc(call));
        assert!(matches!(flow, Flow::Continue));
        let calls = ops.calls.borrow();
        assert_eq!(
            calls.len(),
            1,
            "the whole plan must ride one call: {calls:?}"
        );
        assert!(
            calls[0].starts_with("register_mappings(<leader>ff") && calls[0].ends_with(",7)"),
            "every enabled key and the channel they answer over must reach the engine, got {:?}",
            calls[0]
        );
    }

    #[test]
    fn register_mappings_write_failure_returns_engine_lost() {
        let ops = FakeOps::default();
        *ops.fail_next.borrow_mut() = true;
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Rpc(RpcCall::RegisterMappings {
            specs: view_core::native::mappings::default_maps().to_vec(),
            channel_id: 1,
        }));
        assert!(matches!(flow, Flow::EngineLost));
    }

    #[test]
    fn register_bridge_effect_maps_to_engine_ops_register_bridge() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Rpc(RpcCall::RegisterBridge { channel_id: 7 }));
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(*ops.calls.borrow(), vec!["register_bridge(7)".to_string()]);
    }

    #[test]
    fn register_bridge_write_failure_returns_engine_lost() {
        let ops = FakeOps::default();
        *ops.fail_next.borrow_mut() = true;
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Rpc(RpcCall::RegisterBridge { channel_id: 1 }));
        assert!(matches!(flow, Flow::EngineLost));
    }

    #[test]
    fn set_option_write_failure_returns_engine_lost() {
        let ops = FakeOps::default();
        *ops.fail_next.borrow_mut() = true;
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Rpc(RpcCall::SetOption {
            name: "laststatus".into(),
            value: OptionValue::Int(0),
        }));
        assert!(matches!(flow, Flow::EngineLost));
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

    /// The no-worker-wired shape (every bare `Executor::new`, per its own
    /// doc): with no clipboard worker to own the reply, the executor must
    /// still answer the token itself, exactly once, rather than silently
    /// dropping it -- a charwise-empty read, the safest default for a
    /// clipboard this build cannot reach.
    #[test]
    fn clipboard_read_with_no_worker_wired_replies_directly_and_charwise_empty() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::ClipboardRead {
            token: ReplyToken { msgid: 3 },
            register: '+',
        });
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(
            ops.calls.borrow()[0],
            format!(
                "reply(3,{:?})",
                ReplyValue::ClipboardLines {
                    lines: Vec::new(),
                    regtype: RegisterType::Charwise,
                }
            )
        );
    }

    /// The worker-delegated shape: with a clipboard channel wired, the
    /// executor must forward the job (token, register) rather than answer
    /// it itself -- answering here too would violate the one-reply-per-
    /// token contract once the worker also replies.
    #[test]
    fn clipboard_read_with_a_worker_wired_forwards_the_job_and_does_not_reply_itself() {
        let ops = FakeOps::default();
        let (tx, rx) = mpsc::channel();
        let executor = Executor::new(&ops).with_clipboard(tx);
        let flow = executor.run(Effect::ClipboardRead {
            token: ReplyToken { msgid: 4 },
            register: '*',
        });
        assert!(matches!(flow, Flow::Continue));
        assert!(
            ops.calls.borrow().is_empty(),
            "a worker-wired read must not self-reply"
        );
        let job = rx.try_recv().expect("the read job must be forwarded");
        assert_eq!(job.token.msgid, 4);
        assert!(matches!(
            job.kind,
            crate::clipboard::ClipboardJobKind::Read { register: '*' }
        ));
    }

    /// A clipboard channel wired but whose receiver has already been
    /// dropped (the worker thread exited) must still answer the token
    /// exactly once, directly -- the same degrade the no-worker-wired case
    /// takes, reached through a different path.
    #[test]
    fn clipboard_read_with_a_dropped_receiver_still_replies_exactly_once() {
        let ops = FakeOps::default();
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let executor = Executor::new(&ops).with_clipboard(tx);
        let flow = executor.run(Effect::ClipboardRead {
            token: ReplyToken { msgid: 5 },
            register: '+',
        });
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(ops.calls.borrow().len(), 1, "must reply exactly once");
        assert_eq!(
            ops.calls.borrow()[0],
            format!(
                "reply(5,{:?})",
                ReplyValue::ClipboardLines {
                    lines: Vec::new(),
                    regtype: RegisterType::Charwise,
                }
            )
        );
    }

    #[test]
    fn clipboard_write_with_no_worker_wired_replies_directly_with_nil() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::ClipboardWrite {
            token: ReplyToken { msgid: 6 },
            register: '+',
            lines: vec!["a".to_owned()],
            regtype: RegisterType::Linewise,
        });
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(ops.calls.borrow()[0], "reply(6,Nil)");
    }

    #[test]
    fn clipboard_write_with_a_worker_wired_forwards_the_job_including_regtype() {
        let ops = FakeOps::default();
        let (tx, rx) = mpsc::channel();
        let executor = Executor::new(&ops).with_clipboard(tx);
        let flow = executor.run(Effect::ClipboardWrite {
            token: ReplyToken { msgid: 7 },
            register: '*',
            lines: vec!["x".to_owned(), "y".to_owned()],
            regtype: RegisterType::Linewise,
        });
        assert!(matches!(flow, Flow::Continue));
        assert!(
            ops.calls.borrow().is_empty(),
            "a worker-wired write must not self-reply"
        );
        let job = rx.try_recv().expect("the write job must be forwarded");
        assert_eq!(job.token.msgid, 7);
        let crate::clipboard::ClipboardJobKind::Write {
            register,
            lines,
            regtype,
        } = job.kind
        else {
            unreachable!("expected a Write job kind");
        };
        assert_eq!(register, '*');
        assert_eq!(lines, vec!["x", "y"]);
        assert_eq!(regtype, RegisterType::Linewise);
    }

    #[test]
    fn clipboard_write_with_a_dropped_receiver_still_replies_exactly_once() {
        let ops = FakeOps::default();
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let executor = Executor::new(&ops).with_clipboard(tx);
        let flow = executor.run(Effect::ClipboardWrite {
            token: ReplyToken { msgid: 8 },
            register: '+',
            lines: vec!["a".to_owned()],
            regtype: RegisterType::Charwise,
        });
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(ops.calls.borrow().len(), 1, "must reply exactly once");
        assert_eq!(ops.calls.borrow()[0], "reply(8,Nil)");
    }

    /// `Osc52Copy` carries no `ReplyToken` (see the effect's own doc): an
    /// unwired channel is an ordinary fire-and-forget no-op, unlike the two
    /// clipboard effects above which owe a reply regardless.
    #[test]
    fn osc52_copy_with_no_channel_wired_is_a_silent_no_op() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Osc52Copy {
            register: '+',
            lines: vec!["a".to_owned()],
            regtype: RegisterType::Charwise,
        });
        assert!(matches!(flow, Flow::Continue));
        assert!(ops.calls.borrow().is_empty());
    }

    #[test]
    fn osc52_copy_with_a_channel_wired_forwards_the_job_including_regtype() {
        let ops = FakeOps::default();
        let (tx, rx) = mpsc::channel();
        let executor = Executor::new(&ops).with_osc52(tx);
        let flow = executor.run(Effect::Osc52Copy {
            register: '*',
            lines: vec!["a".to_owned(), "b".to_owned()],
            regtype: RegisterType::Linewise,
        });
        assert!(matches!(flow, Flow::Continue));
        let job = rx.try_recv().expect("the osc52 job must be forwarded");
        assert_eq!(job.register, '*');
        assert_eq!(job.lines, vec!["a", "b"]);
        assert_eq!(job.regtype, RegisterType::Linewise);
    }

    #[test]
    fn dispatch_a_key_forwards_input_and_returns_continue() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let mut model = Model::with_term_size(80, 24);
        let mut native = NativeSession::inert();
        let mut bridge = ThemeBridge::new(None);
        let mut follow_ups = FollowUps {
            native: &mut native,
            theme: &mut bridge,
        };
        let flow = dispatch(
            &mut model,
            &executor,
            &mut follow_ups,
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
        let mut native = NativeSession::inert();
        let mut bridge = ThemeBridge::new(None);
        let mut follow_ups = FollowUps {
            native: &mut native,
            theme: &mut bridge,
        };
        let flow = dispatch(
            &mut model,
            &executor,
            &mut follow_ups,
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
        let mut native = NativeSession::inert();
        let mut bridge = ThemeBridge::new(None);
        let mut follow_ups = FollowUps {
            native: &mut native,
            theme: &mut bridge,
        };
        let flow = dispatch(
            &mut model,
            &executor,
            &mut follow_ups,
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
        let mut native = NativeSession::inert();
        let mut bridge = ThemeBridge::new(None);
        let mut follow_ups = FollowUps {
            native: &mut native,
            theme: &mut bridge,
        };
        let resize = Msg::Resized {
            width: 100,
            height: 50,
        };
        assert!(matches!(
            dispatch(&mut model, &executor, &mut follow_ups, resize.clone()),
            Flow::Continue
        ));
        model.dirty = false;
        assert!(matches!(
            dispatch(&mut model, &executor, &mut follow_ups, resize),
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
        let mut native = NativeSession::inert();
        let mut bridge = ThemeBridge::new(None);
        let mut follow_ups = FollowUps {
            native: &mut native,
            theme: &mut bridge,
        };
        let start = std::time::Instant::now();
        for i in 0..1000 {
            let flow = dispatch(
                &mut model,
                &executor,
                &mut follow_ups,
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

    /// The stall threshold the tests below run against, in place of the
    /// shipping ten seconds.
    ///
    /// The predicate is the same one either way -- it compares an elapsed
    /// duration against whatever threshold the watch was built with -- so
    /// the only thing a real-length run would add to these assertions is
    /// ten seconds of suite time per test.
    const TEST_STALL_THRESHOLD: std::time::Duration = std::time::Duration::from_millis(50);

    /// How long a watchdog waits before feeding a message to a wait that
    /// should have expired on its own.
    ///
    /// Two orders of magnitude past the test threshold, so it separates
    /// outcomes rather than grading one: a wait bounded by the stall
    /// deadline returns in tens of milliseconds, and an unbounded one
    /// returns never. The watchdog exists so "never" fails the test with a
    /// verdict instead of hanging the suite.
    const WAIT_WATCHDOG_SECS: u64 = 5;

    /// Waits for `probe` to hold, failing the test rather than hanging if
    /// it never does.
    fn wait_until(what: &str, mut probe: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !probe() {
            assert!(std::time::Instant::now() < deadline, "timed out: {what}");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// A live connection whose peer parks inside every write until
    /// released, exactly as a wedged nvim parks the writer thread.
    ///
    /// Every end whose drop would free a parked thread is held here rather
    /// than by the test body, so an assertion that fails mid-wedge still
    /// unparks both threads: the body owns this, and unwinding drops it.
    /// Neither the notification receiver nor the reader's block may be
    /// dropped early, since either would retire the connection for a reason
    /// that has nothing to do with the peer.
    struct WedgedPeer {
        handle: EngineHandle,
        entered: mpsc::Receiver<()>,
        release: Option<mpsc::Sender<()>>,
        _reader: mpsc::Sender<()>,
        _notifications: mpsc::Receiver<view_engine::EngineNotification>,
    }

    impl WedgedPeer {
        fn new() -> Self {
            let (sink, entered, release) = view_engine::test_peer::ParkedSink::new();
            let (source, reader) = view_engine::test_peer::IdleSource::new();
            let (handle, notifications) = EngineHandle::start(source, sink);
            Self {
                handle,
                entered,
                release: Some(release),
                _reader: reader,
                _notifications: notifications,
            }
        }

        /// Blocks until the writer thread is provably inside a write that
        /// cannot finish, so the stall is a fact before anything is timed.
        fn await_parked_write(&self) {
            self.entered
                .recv_timeout(std::time::Duration::from_secs(
                    view_engine::test_peer::PARKED_WRITE_ARM_SECS,
                ))
                .expect("the writer thread reached the sink");
        }

        /// Lets the peer accept writes again, from the one in progress on.
        fn release(&mut self) {
            self.release = None;
        }
    }

    #[test]
    fn a_wedged_engine_raises_the_notice_and_retracts_it_when_the_writer_moves_again() {
        let mut peer = WedgedPeer::new();
        let mut model = Model::with_term_size(80, 24);
        let mut watch = OutboxStallWatch::new(TEST_STALL_THRESHOLD);

        peer.handle.input("a").unwrap();
        peer.await_parked_write();
        // queued behind a write that cannot finish, so the backlog outlives
        // the message the writer is holding
        peer.handle.input("b").unwrap();

        // the stall is measured from an observation, never asserted by the
        // first one: nothing has yet been seen to stop moving
        assert!(
            !note_write_stall(&mut model, &mut watch, &peer.handle),
            "the notice was raised by the observation that first saw the backlog, \
             before any time had passed for the writer to be stalled through"
        );
        assert!(model.engine.messages.entries.is_empty());

        std::thread::sleep(TEST_STALL_THRESHOLD * 3);
        assert!(
            note_write_stall(&mut model, &mut watch, &peer.handle),
            "a writer parked inside a write, with a second message queued behind it \
             and the threshold long past, raised no notice"
        );
        assert_eq!(
            visible_texts(&model),
            vec![ENGINE_STALLED_NOTICE.to_string()]
        );

        assert!(
            !note_write_stall(&mut model, &mut watch, &peer.handle),
            "re-asserting an unchanged notice reported a change, which repaints \
             the toast on every loop pass for as long as the stall lasts"
        );

        // a keypress dismisses transient toasts; this one describes a
        // condition that is still true, and the keypress that would drop it
        // is the one it exists to explain
        assert!(!model.engine.messages.dismiss_transient_on_keypress(false));
        assert_eq!(
            visible_texts(&model),
            vec![ENGINE_STALLED_NOTICE.to_string()]
        );

        peer.release();
        wait_until("the writer drains its backlog", || {
            peer.handle.write_progress().0 == 0
        });
        assert!(
            note_write_stall(&mut model, &mut watch, &peer.handle),
            "the backlog drained and the notice was not retracted"
        );
        assert!(model.engine.messages.entries.is_empty());
    }

    #[test]
    fn a_wedge_surfaces_without_any_further_input() {
        let mut peer = WedgedPeer::new();
        let mut model = Model::with_term_size(80, 24);
        let mut watch = OutboxStallWatch::new(TEST_STALL_THRESHOLD);
        // nothing else will ever arrive: a wedged engine sends no redraws,
        // and this operator typed once and then stopped. The watchdog is
        // not a wakeup the loop may rely on -- it exists so a wait that
        // never expires ends this test with a verdict
        let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(WAIT_WATCHDOG_SECS));
            let _ = msg_tx.send(Msg::RedrawReady);
        });

        peer.handle.input("a").unwrap();
        peer.await_parked_write();
        peer.handle.input("b").unwrap();

        // the loop's own shape: read the write side, wait, repeat
        let start = std::time::Instant::now();
        while !note_write_stall(&mut model, &mut watch, &peer.handle) {
            assert!(
                wait_for_msg(&msg_rx, &watch).is_none(),
                "the wait outlasted the stall deadline and returned the watchdog's \
                 message: a wedge nobody types at would never be surfaced"
            );
            assert!(
                start.elapsed() < std::time::Duration::from_secs(WAIT_WATCHDOG_SECS),
                "the deadline kept expiring without the stall ever being reported"
            );
        }
        assert_eq!(
            visible_texts(&model),
            vec![ENGINE_STALLED_NOTICE.to_string()]
        );
        peer.release();
    }

    #[test]
    fn an_idle_session_arms_no_deadline_and_is_never_woken_early() {
        let mut peer = WedgedPeer::new();
        // a peer that reads normally, expressed through the same sink as
        // the wedged one: healthy and wedged differ by this one call
        peer.release();
        let mut model = Model::with_term_size(80, 24);
        let mut watch = OutboxStallWatch::new(TEST_STALL_THRESHOLD);

        peer.handle.input("a").unwrap();
        wait_until("the writer drains its backlog", || {
            peer.handle.write_progress().0 == 0
        });
        assert!(!note_write_stall(&mut model, &mut watch, &peer.handle));
        assert_eq!(
            watch.poll_deadline(),
            None,
            "an idle session armed a deadline, so the loop would wake on a \
             schedule it has never paid for"
        );

        // and the wait itself delivers only what is sent, when it is sent
        let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
        let quiet = TEST_STALL_THRESHOLD * 3;
        std::thread::spawn(move || {
            std::thread::sleep(quiet);
            let _ = msg_tx.send(Msg::RedrawReady);
        });
        let start = std::time::Instant::now();
        let received = wait_for_msg(&msg_rx, &watch);
        assert!(
            matches!(received, Some(Ok(Msg::RedrawReady))),
            "an idle wait returned something other than the one message sent to it"
        );
        assert!(
            start.elapsed() >= quiet,
            "the idle wait returned before its only message was sent: the loop was \
             woken by a deadline an idle session must not arm"
        );
    }

    #[test]
    fn an_engine_that_keeps_writing_never_raises_the_notice() {
        let mut peer = WedgedPeer::new();
        let mut model = Model::with_term_size(80, 24);
        let mut watch = OutboxStallWatch::new(TEST_STALL_THRESHOLD);
        peer.release();

        for i in 0..20 {
            peer.handle.input("a").unwrap();
            peer.await_parked_write();
            assert!(
                !note_write_stall(&mut model, &mut watch, &peer.handle),
                "a delivering writer read as stalled on write {i}"
            );
            std::thread::sleep(TEST_STALL_THRESHOLD / 2);
        }
        assert!(model.engine.messages.entries.is_empty());
    }

    #[test]
    fn an_idle_engine_with_nothing_queued_never_raises_the_notice() {
        let mut peer = WedgedPeer::new();
        let mut model = Model::with_term_size(80, 24);
        let mut watch = OutboxStallWatch::new(TEST_STALL_THRESHOLD);
        peer.release();

        peer.handle.input("a").unwrap();
        wait_until("the writer drains its backlog", || {
            peer.handle.write_progress().0 == 0
        });
        assert!(!note_write_stall(&mut model, &mut watch, &peer.handle));
        std::thread::sleep(TEST_STALL_THRESHOLD * 3);
        assert!(
            !note_write_stall(&mut model, &mut watch, &peer.handle),
            "an engine with an empty queue read as stalled after three thresholds \
             of doing nothing, which is an idle editor rather than a wedged one"
        );
        assert!(model.engine.messages.entries.is_empty());
    }
}
