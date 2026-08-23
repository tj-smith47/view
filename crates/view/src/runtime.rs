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
//! long as `run` holds that engine. `Engine`'s `Drop` (a graceful `qa!`,
//! then a bounded wait, then `SIGKILL`) runs exactly once per engine --
//! when `run` returns (a clean quit via `Flow::Quit`, or a terminal I/O
//! error propagated with `?`), or, for an engine being replaced, inside the
//! restart that replaces it ([`crate::recovery::restart_engine`], which
//! never holds two at once). The caller in `main.rs` never touches `Engine`
//! again once it has been handed to `run`.

use crate::bridge::ThemeBridge;
use crate::engine_ops::EngineOps;
use crate::native::NativeSession;
use crate::osc52::{drain_osc52, Osc52Job, Osc52Sink};
use crate::recovery::{
    reconnects, restart_engine, step, EngineSession, LoopChannels, LoopState, ReconnectSchedule,
};
use crate::speculate::{
    expire_speculation, note_engine_call, reconcile_speculation, SpeculationClock,
};
use std::sync::mpsc;
use std::time::Instant;
use view_core::model::Model;
use view_core::msg::{Effect, ExitInfo, Msg, RpcCall};
use view_core::native::supervision::{WedgeKind, READOUT_RESOLUTION};
use view_core::update::update;
use view_engine::handle::EngineHandle;
use view_engine::heartbeat::{wedge_kind, HeartbeatWatch};
use view_engine::nvim_api::BufWriteOutcome;
use view_engine::process::Engine;
use view_engine::stall::OutboxStallWatch;
use view_tui::terminal::Term;

/// What the runtime loop does after one effect crosses [`Executor::run`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Keep processing the current effect batch.
    Continue,
    /// Quit with the given exit code; the caller returns immediately.
    Quit(i32),
    /// An engine write failed: the engine connection is gone, not the UI.
    /// The caller resolves the real exit status and decides between the two
    /// things that can mean -- an engine told to stop ends the session with
    /// its own status, an engine nobody asked to stop is handed to
    /// supervision (see [`crate::recovery::step`]).
    EngineLost,
    /// The user asked the supervision modal to restart the engine. Distinct
    /// from [`EngineLost`](Self::EngineLost) because the two arrive for
    /// opposite reasons -- one is the connection failing, the other is a
    /// deliberate choice made about a connection that may still be open --
    /// and the recovery each is owed differs accordingly.
    RestartEngine,
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
    /// Which engine this executor's connection is, as
    /// [`crate::clipboard::ReplyRoute::epoch`] counts them. Stamped onto
    /// every clipboard job this executor queues, so a job outstanding when
    /// the engine is replaced is recognisable as belonging to the dead
    /// connection -- a restart builds a new executor from the stepped
    /// epoch, and the jobs the old one left in the channel keep the old
    /// number. Read here rather than at reply time because the worker
    /// cannot tell those two apart once they are in the same queue.
    reply_epoch: u64,
    /// The terminal's OSC52 job channel, drained synchronously by `run`'s
    /// loop on the thread that owns `Term` (see `Effect::Osc52Copy`'s doc
    /// for why this cannot be a write from the clipboard worker thread).
    osc52: Option<mpsc::Sender<Osc52Job>>,
    /// The loop's own message channel, cloned into every one-shot toast
    /// timer thread `Effect::ScheduleToastExpiry` spawns (see `run`'s match
    /// arm below). `None` degrades a scheduled expiry to a silent no-op --
    /// the toast then simply outlives its intended timeout instead of the
    /// executor panicking or blocking, matching every other unwired-channel
    /// degrade in this type. A [`crate::wake::LoopSender`] rather than a
    /// bare `SyncSender`: the unix loop sleeps in an fd poll a bare send
    /// cannot interrupt, so every producer here must carry the wake signal
    /// with it.
    toast_timer: Option<crate::wake::LoopSender>,
    /// The matcher worker's query channel (`view_native::picker::matcher::spawn`),
    /// or `None` when no worker is wired -- every test `Executor` built via
    /// plain `new`. `PickerQuery` carries no `ReplyToken` (see that
    /// effect's own doc), so an unwired channel degrades to a silent no-op
    /// the same way `Osc52Copy`/`ScheduleToastExpiry` do below, not the
    /// must-answer-the-token shape `ClipboardRead`/`Write` need.
    picker: Option<mpsc::Sender<view_native::picker::matcher::WorkerRequest>>,
    /// The still-running tree scan's cancel flag, if any -- the executor's
    /// only handle on the worker thread `Effect::TreeScan` spawned, since
    /// `view_native::tree::fs::scan` is one blocking call with no generation
    /// check of its own along the way (see that function's own doc).
    /// `Effect::TreeScan` flips whatever was here before storing its own
    /// fresh flag (a superseding scan cancels the one it replaces, exactly
    /// like `TreeState::request_rescan` already discards a superseded
    /// generation on the `update()` side), and `Effect::TreeClose` flips it
    /// and clears the slot. A `Mutex` rather than a plain field because
    /// `run` takes `&self`: every other piece of executor state that a
    /// worker thread reads back is either `Clone`d out to the thread
    /// (`toast_timer`) or, like this one, mutated from behind `&self` by
    /// more than one effect over the executor's lifetime.
    tree_scan_cancel: std::sync::Mutex<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>>,
    /// The project's agent session worker, or `None` when `[ai]` is
    /// disabled -- the one degrade in this type that is never reachable in
    /// practice rather than merely untested: `update()`'s own
    /// `Msg::FeatureInvoke` gate refuses to ever produce an `Effect::Ai`
    /// while `model.ai_enabled` is false (see `notice_ai_disabled`), so no
    /// command can arrive here with nothing wired to take it.
    ai: Option<crate::ai_worker::AiWorker>,
    /// The messages this type answers an effect with itself, parked when
    /// the loop's channel cannot take one (see
    /// [`LoopMsgOutbox`](crate::loop_msgs::LoopMsgOutbox)).
    loop_msgs: crate::loop_msgs::LoopMsgOutbox,
    /// The context worker's job channel
    /// (`crate::ai_context_worker::spawn`), or `None` when no worker is
    /// wired -- every test `Executor` built via plain `new`.
    /// `Effect::AiPromptSubmit` carries no reply token (assembly happens
    /// entirely off this thread; the worker calls straight into `ai` once
    /// it has read a snapshot), so an unwired channel degrades to a silent
    /// no-op the same way `PickerQuery` does, not the must-answer-the-token
    /// shape `ClipboardRead`/`Write` need.
    ai_context: Option<mpsc::Sender<crate::ai_context_worker::AiContextJob>>,
}

/// One pass's handoffs: the work [`run`]'s loop owes other parties before
/// it can paint or sleep, in one step so a pass cannot perform half of it.
///
/// Both halves are queued by a dispatch this thread ran and can only be
/// carried out by this thread -- the terminal writes because only the loop
/// owns `Term`, the write outcomes because the message channel's consumer
/// is this loop itself. Performed at the top of the pass rather than right
/// after the dispatch that queued them: nothing blocks between here and the
/// bottom of the previous pass's own dispatch loop, so this is effectively
/// immediate, and one site covers every dispatch call the loop makes
/// (resize, supervision, and the main queue) instead of one per call site.
/// On the loop thread, same as `draw_surface` -- see [`run`]'s doc for the
/// latency that costs.
fn drain_pass_handoffs<S: Osc52Sink, E: EngineOps>(
    osc52_rx: &mpsc::Receiver<Osc52Job>,
    sink: &mut S,
    executor: &Executor<E>,
) {
    drain_osc52(osc52_rx, sink);
    executor.flush_loop_msgs();
}

/// Spawns `f` on its own thread, logging rather than panicking if the OS
/// refuses to create one. `std::thread::spawn` panics on that failure
/// internally (it is `Builder::new().spawn(f).expect(...)`), which would
/// crash the whole editor over a resource-exhaustion condition every
/// caller below already treats as an ordinary, recoverable degrade when
/// the reply channel itself is unwired (see each `Effect` arm's own doc).
/// Returns whether the thread was actually spawned, for the one caller
/// (`Effect::TreeGitScan`) whose reply, unlike every other one here,
/// clears a state machine flag with no other clearer.
pub(crate) fn spawn_or_log<F>(label: &'static str, f: F) -> bool
where
    F: FnOnce() + Send + 'static,
{
    match std::thread::Builder::new().spawn(f) {
        Ok(_) => true,
        Err(err) => {
            crate::vlog::log_with(label, || format!("thread spawn failed: {err}"));
            false
        }
    }
}

/// What the user is told when the confirming second look at `path`
/// (`Effect::ReprobeExternalWrite`) could not be scheduled at all.
///
/// Written for that user rather than for a log, and routed through
/// `Msg::ExternalWatchDegraded` because the consequence is that message's
/// own subject: a removal whose confirmation never runs is never announced,
/// so detection is quietly not covering what `docs/ai.md` says it covers.
fn reprobe_unscheduled(path: &std::path::Path) -> String {
    format!(
        "{} could not be re-checked after it stopped being readable",
        path.display()
    )
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
            reply_epoch: 0,
            osc52: None,
            toast_timer: None,
            picker: None,
            tree_scan_cancel: std::sync::Mutex::new(None),
            loop_msgs: crate::loop_msgs::LoopMsgOutbox::default(),
            ai: None,
            ai_context: None,
        }
    }

    /// [`LoopMsgOutbox::route`](crate::loop_msgs::LoopMsgOutbox::route) for
    /// this executor's own outbox and loop channel.
    fn route_loop_msg(&self, msg: Msg) {
        self.loop_msgs.route(self.toast_timer.as_ref(), msg);
    }

    /// [`LoopMsgOutbox::flush`](crate::loop_msgs::LoopMsgOutbox::flush) for
    /// this executor's own outbox and loop channel.
    pub(crate) fn flush_loop_msgs(&self) {
        self.loop_msgs.flush(self.toast_timer.as_ref());
    }

    /// Wires the clipboard worker's job channel; `ClipboardRead`/
    /// `ClipboardWrite` effects forward to it instead of self-answering.
    #[must_use]
    pub fn with_clipboard(mut self, tx: mpsc::Sender<crate::clipboard::ClipboardJob>) -> Self {
        self.clipboard = Some(tx);
        self
    }

    /// Stamps every clipboard job this executor queues as belonging to
    /// engine `epoch` (see [`crate::clipboard::ReplyRoute::epoch`]).
    #[must_use]
    pub fn with_reply_epoch(mut self, epoch: u64) -> Self {
        self.reply_epoch = epoch;
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
    pub fn with_toast_timer(mut self, tx: crate::wake::LoopSender) -> Self {
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

    /// Wires the agent session worker; `Effect::Ai` forwards to it instead
    /// of silently no-oping.
    #[must_use]
    pub(crate) fn with_ai(mut self, worker: crate::ai_worker::AiWorker) -> Self {
        self.ai = Some(worker);
        self
    }

    /// Wires the context worker's job channel; `AiPromptSubmit` effects
    /// forward to it instead of silently no-oping.
    #[must_use]
    pub(crate) fn with_ai_context(
        mut self,
        tx: mpsc::Sender<crate::ai_context_worker::AiContextJob>,
    ) -> Self {
        self.ai_context = Some(tx);
        self
    }

    /// Unwraps back to the owned `ops`, so a test can inspect what a fake
    /// recorded after driving `Executor` through a call it does not
    /// otherwise expose a getter for.
    #[cfg(test)]
    pub(crate) fn into_ops(self) -> E {
        self.ops
    }

    /// The worker `Effect::Ai` forwards to, if wired -- exposed only so a
    /// test can confirm restart survival's identity (see
    /// `crate::ai_worker::AiWorker::is_same_worker_as`'s own doc);
    /// `Effect::Ai`'s own arm never needs a getter, only `dispatch`.
    #[cfg(all(test, unix))]
    pub(crate) fn ai_worker(&self) -> Option<&crate::ai_worker::AiWorker> {
        self.ai.as_ref()
    }

    /// Queues `job` on the wired context worker's one channel -- the single
    /// FIFO both `Effect::Ai` and `Effect::AiPromptSubmit` funnel through
    /// (see [`crate::ai_context_worker::AiContextJob`]'s own doc for why a
    /// shared queue, not two, is what makes a `Cancel` overtaking its own
    /// `Submit` unrepresentable). `mpsc::Sender::send` never blocks the loop
    /// thread here: the channel behind `ai_context` is unbounded (built with
    /// plain `mpsc::channel()`, never a `sync_channel`), so this call
    /// returns as soon as the job is enqueued, with none of the worker's own
    /// (possibly slow) context reads ever running on this thread.
    ///
    /// A send that fails -- `ai_context` unwired, or the worker thread
    /// already gone -- degrades through the same
    /// `Msg::Ai(AiEvent::SessionCrashed)` local-error path a genuine session
    /// crash reports through, injected via `toast_timer`'s non-blocking
    /// `try_send`: the same "recurse a synthesized `Msg` back through the
    /// loop" shape `Effect::AiTrustSet`'s own degrade in `dispatch` uses,
    /// just reachable from inside `run` itself since `try_send` (unlike a
    /// blocking send) needs no `&mut Model` to stay non-blocking. `update()`'s
    /// existing `SessionCrashed` arm is what actually clears
    /// `panel.turn_in_flight` on this path -- see that arm in
    /// `view-core/src/update/ai.rs`. With no `toast_timer` wired either (a
    /// bare test `Executor`, never a real one -- see `LoopChannels::executor`,
    /// which wires `toast_timer` unconditionally ahead of `ai`/`ai_context`),
    /// this is a silent no-op, the same degrade every other unwired-channel
    /// effect in this type falls back to.
    fn queue_ai_job(&self, job: crate::ai_context_worker::AiContextJob) {
        let queued = self
            .ai_context
            .as_ref()
            .is_some_and(|tx| tx.send(job).is_ok());
        if !queued {
            if let Some(toast_timer) = &self.toast_timer {
                let _ = toast_timer.try_send(Msg::Ai(
                    view_core::native::ai_event::AiEvent::SessionCrashed {
                        message: "no AI worker wired for this command".to_string(),
                    },
                ));
            }
        }
    }

    /// Queues one clipboard job on the worker, or discharges the job's own
    /// obligation inline when no worker takes it (see
    /// [`crate::clipboard::dispatch`]).
    fn hand_to_clipboard(&self, kind: crate::clipboard::ClipboardJobKind) -> Flow {
        crate::clipboard::dispatch(self.clipboard.as_ref(), &self.ops, self.reply_epoch, kind);
        Flow::Continue
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
                    RpcCall::ProbeSwapRecovery { generation } => {
                        self.ops.probe_swap_recovery(generation)
                    }
                    RpcCall::Redraw => self.ops.redraw(),
                    RpcCall::ClaimStdoutTty => self.ops.claim_stdout_tty(),
                    RpcCall::RegisterMappings { specs, channel_id } => {
                        self.ops.register_mappings(&specs, channel_id)
                    }
                    RpcCall::RegisterBridge { channel_id } => self.ops.register_bridge(channel_id),
                    RpcCall::RegisterClipboard { channel_id } => {
                        self.ops.register_clipboard(channel_id)
                    }
                    RpcCall::ListBuffers { generation } => self.ops.list_buffers(generation),
                    RpcCall::PreviewBuffer { path, generation } => {
                        self.ops.preview_buffer(&path, generation)
                    }
                    RpcCall::OpenFile { path } => self.ops.open_file(&path),
                    RpcCall::RenameFile {
                        old_path,
                        new_path,
                        generation,
                    } => self.ops.rename_file(&old_path, &new_path, generation),
                    RpcCall::TreeCreatePrompt { generation } => {
                        self.ops.tree_create_prompt(generation)
                    }
                    RpcCall::TreeRenamePrompt {
                        generation,
                        old_path,
                        current_name,
                    } => self
                        .ops
                        .tree_rename_prompt(&old_path, &current_name, generation),
                    RpcCall::TreeDeleteConfirm { generation, path } => {
                        self.ops.tree_delete_confirm(&path, generation)
                    }
                    // The one call whose outcome is not just ok-or-lost: a
                    // buffer that moved past the tick the review named
                    // refuses the write, which is an ordinary race with the
                    // user's own typing, not an engine failure. It is
                    // routed back as `Msg::BufWriteRefused` so the review
                    // puts the hunks back rather than believing they landed.
                    RpcCall::BufSetText {
                        buf,
                        edits,
                        undojoin,
                        expected_changedtick,
                        generation,
                    } => match self
                        .ops
                        .set_buf_text(buf, &edits, undojoin, expected_changedtick)
                    {
                        Ok(outcome) => {
                            self.route_loop_msg(match outcome {
                                BufWriteOutcome::Applied { changedtick } => Msg::BufWriteApplied {
                                    buf,
                                    generation,
                                    changedtick,
                                },
                                BufWriteOutcome::BufferAdvanced => {
                                    Msg::BufWriteRefused { buf, generation }
                                }
                            });
                            Ok(())
                        }
                        Err(err) => Err(err),
                    },
                    RpcCall::BufAttach { buf, generation } => self.ops.buf_attach(buf, generation),
                    RpcCall::BufDetach { buf } => self.ops.buf_detach(buf),
                    // The second call whose outcome is not just ok-or-lost:
                    // a path the engine refuses outright (blank, relative,
                    // or ending in a separator) never reaches the wire, so
                    // no reply is coming to answer the review's bind. It is
                    // stood in for here with the same buffer-less resolve
                    // nvim's own refusal produces, so the review reads as
                    // unbindable instead of waiting for a reply nobody
                    // sent -- and, crucially, so a refusal of one path is
                    // never mistaken for the connection itself being gone.
                    RpcCall::LoadHidden { path, generation } => {
                        match self.ops.load_hidden(&path, generation) {
                            Err(view_engine::handle::EngineError::UnusablePath { .. }) => {
                                self.route_loop_msg(Msg::HiddenBufferLoaded {
                                    generation,
                                    buf: None,
                                    created: false,
                                    changedtick: 0,
                                });
                                Ok(())
                            }
                            other => other,
                        }
                    }
                    RpcCall::ReleaseHidden { path } => self.ops.release_hidden(&path),
                    RpcCall::AiFsRead {
                        request_id,
                        buf,
                        line,
                        limit,
                    } => self.ops.ai_fs_read(request_id, buf, line, limit),
                    RpcCall::AiFsWrite {
                        request_id,
                        buf,
                        lines,
                        eol,
                        expected_changedtick,
                    } => self
                        .ops
                        .ai_fs_write(request_id, buf, &lines, eol, expected_changedtick),
                    RpcCall::Checktime {
                        request_id,
                        paths,
                        force,
                    } => self.ops.checktime(request_id, &paths, force),
                    // RpcCall is #[non_exhaustive]: a future call kind must
                    // degrade to a no-op here rather than fail to compile.
                    // BufSetText, the two AiFs calls, and Checktime are
                    // matched explicitly above rather than falling through
                    // here: unlike every other call this catch-all covers, a
                    // silently no-op'd write would drop a buffer edit the
                    // user already accepted, a silently no-op'd filesystem
                    // answer would leave the agent that asked blocked on a
                    // request nothing else will ever settle, and a silently
                    // no-op'd checktime would leave a watcher-detected write
                    // -- or the user's own "reload, discard local edits"
                    // answer to a conflict prompt -- never carried out.
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
            // the four clipboard effects share one hand-off, because they
            // share the harder half of it: what happens when no worker is
            // reachable. Each carries its own obligation and
            // `clipboard::dispatch` discharges it (see its doc), rather
            // than being silently dropped the way an unmapped
            // fire-and-forget RpcCall may be
            Effect::ClipboardRead { token, register } => {
                self.hand_to_clipboard(crate::clipboard::ClipboardJobKind::Read { token, register })
            }
            Effect::ClipboardWrite {
                token,
                register,
                lines,
                regtype,
            } => self.hand_to_clipboard(crate::clipboard::ClipboardJobKind::Write {
                token,
                register,
                lines,
                regtype,
            }),
            Effect::ClipboardStore { register, text } => {
                self.hand_to_clipboard(crate::clipboard::ClipboardJobKind::Store { register, text })
            }
            Effect::ClipboardQuery { register } => {
                self.hand_to_clipboard(crate::clipboard::ClipboardJobKind::Query { register })
            }
            // carries no ReplyToken (see the effect's own doc): nothing on
            // the wire is blocked on this, so an unwired osc52 channel (or
            // one whose receiver is gone) costs nothing beyond the escape
            // never being written -- an ordinary fire-and-forget degrade,
            // unlike the clipboard effects above, three of which owe an
            // answer whatever happens to their worker
            Effect::Osc52Copy {
                register,
                lines,
                regtype,
            } => {
                if let Some(tx) = &self.osc52 {
                    let _ = tx.send(Osc52Job::Copy {
                        register,
                        lines,
                        regtype,
                    });
                }
                Flow::Continue
            }
            // the same channel and the same fire-and-forget degrade as
            // `Osc52Copy` above; only the encoding side differs (see
            // `Osc52Job`)
            Effect::TermWrite { bytes } => {
                if let Some(tx) = &self.osc52 {
                    let _ = tx.send(Osc52Job::Passthrough(bytes));
                }
                Flow::Continue
            }
            Effect::Quit { exit_code } => Flow::Quit(exit_code),
            // handed straight back to the loop: the engine's lifetime
            // belongs to whoever owns the `Engine` value, and this executor
            // holds only a clone of its RPC handle
            Effect::RestartEngine => Flow::RestartEngine,
            // one-shot: a background thread that owns exactly one send, never
            // a persistent multi-deadline scheduler. The loop has no
            // free-running clock of its own (see this module's own doc), so
            // this thread -- not a paint-time check -- is what wakes it back
            // up on an otherwise-idle editor.
            Effect::ScheduleToastExpiry { id, after } => {
                if let Some(tx) = &self.toast_timer {
                    let tx = tx.clone();
                    spawn_or_log("toast-expiry", move || {
                        std::thread::sleep(after);
                        let _ = tx.send(Msg::ToastExpired { id });
                    });
                }
                Flow::Continue
            }
            // the same one-shot thread `ScheduleToastExpiry` uses, and the
            // same reason for it: `update()` has no clock, and a reply that
            // said a path could not be read has to be re-asked later rather
            // than believed at once. What goes back names the confirming
            // probe as one, since the fold announces on the reply to this
            // look and on no other.
            //
            // The delay is `view_ai`'s, taken from the crate that owns the
            // coalesce window it is keyed to rather than restated here.
            //
            // A spawn that fails says so through the same channel rather
            // than swallowing the confirmation: the removal behind it would
            // otherwise never be announced at all, which is detection
            // quietly not covering what it is documented to cover. The
            // unwired-`toast_timer` half of that degrade needs `update()`
            // itself and lives in `dispatch`, beside `Effect::AiTrustSet`'s.
            Effect::ReprobeExternalWrite { path } => {
                if let Some(tx) = &self.toast_timer {
                    let timer = tx.clone();
                    let reason = reprobe_unscheduled(&path);
                    let spawned = spawn_or_log("external-write-reprobe", move || {
                        std::thread::sleep(view_ai::FILE_GONE_GRACE);
                        let _ = timer.send(Msg::ConfirmExternalRemoval { path });
                    });
                    if !spawned {
                        let _ = tx.try_send(Msg::ExternalWatchDegraded { reason });
                    }
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
            // `Msg::PickerPreviewReply` already reported `loaded: false`:
            // nvim has no buffer open for `path`, so this is a plain
            // `std::fs` read, off the paint loop, reusing the loop's own
            // message channel the same way `ScheduleToastExpiry` does --
            // `view-native` never opens an RPC connection, and this is the
            // one place allowed to depend on both `view-engine` and
            // `view-native` (see `docs/picker-preview-wire-capture.md`).
            Effect::PickerPreviewFallback { generation, path } => {
                if let Some(tx) = &self.toast_timer {
                    let tx = tx.clone();
                    spawn_or_log("picker-preview-fallback", move || {
                        let lines =
                            view_native::picker::preview::read_file(std::path::Path::new(&path));
                        let _ = tx.send(Msg::PickerPreviewFile { generation, lines });
                    });
                }
                Flow::Continue
            }
            // One-shot thread per request, exactly like
            // `PickerPreviewFallback` above: `view_native::tree::fs::scan`
            // is a plain synchronous blocking call, so this is the only
            // place it ever runs off the paint loop, reusing the loop's own
            // message channel to report back. A superseding scan cancels
            // whatever scan preceded it (see `tree_scan_cancel`'s own doc)
            // before installing its own fresh flag, so a burst of rescans
            // never leaves more than one walk running at a time.
            Effect::TreeScan { generation, root } => {
                let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                {
                    let mut slot = self
                        .tree_scan_cancel
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(previous) = slot.replace(std::sync::Arc::clone(&cancel)) {
                        previous.store(true, std::sync::atomic::Ordering::Release);
                    }
                }
                if let Some(tx) = &self.toast_timer {
                    let tx = tx.clone();
                    spawn_or_log("tree-scan", move || {
                        let entries = view_native::tree::fs::scan(&root, &cancel);
                        let _ = tx.send(Msg::TreeScanResult {
                            generation,
                            entries,
                        });
                    });
                }
                Flow::Continue
            }
            // Same one-shot-thread shape as `TreeScan`, calling
            // `view_native::tree::git::status_bounded` instead -- a
            // synchronous, blocking `git status --porcelain=v2` shell-out
            // that never touches RPC (see that module's own doc for why
            // `git` absent from `PATH` degrades there, not here), bounded
            // internally so this spawned thread -- and therefore this
            // `tx.send` -- is guaranteed to complete even against a wedged
            // `git` child. This thread is the whole reason the block above
            // is safe off the paint loop: `Executor::run` itself returns
            // immediately with `Flow::Continue`, so a slow (or now, a
            // bounded-and-killed) `git` only delays this background send,
            // never a frame.
            Effect::TreeGitScan { generation, root } => {
                let delivered = self.toast_timer.as_ref().map(|tx| {
                    let tx = tx.clone();
                    spawn_or_log("tree-git-scan", move || {
                        let (status, timed_out) = view_native::tree::git::status_bounded(&root);
                        let _ = tx.send(Msg::TreeGitResult {
                            generation,
                            status,
                            timed_out,
                        });
                    })
                });
                if delivered != Some(true) {
                    // `TreeState::apply_git` is the only clearer of
                    // `git_refresh_in_flight` (already set `true` by the
                    // `request_git_refresh` call that produced this effect
                    // -- see `view_native::tree::git`'s `GIT_STATUS_TIMEOUT`
                    // doc for why an unanswered generation is a permanent
                    // wedge, not a transient one, and why that call bounds a
                    // wedged `git` child for exactly this reason). An
                    // unwired `toast_timer`, or a `spawn_or_log` that could
                    // not even get the reply thread onto the OS, drops the
                    // reply just as silently, without that bound: the
                    // tree's git decorations freeze for the rest of the
                    // session, or until the tree is closed and reopened.
                    // Every real executor wires `toast_timer`
                    // unconditionally (`main.rs`'s only construction site)
                    // and a thread-spawn failure here is a resource
                    // exhaustion this process is already in serious trouble
                    // from -- reaching here in a debug/test build is a
                    // wiring regression or a test gap worth catching loudly
                    // rather than shipping the silent freeze.
                    debug_assert!(
                        false,
                        "TreeGitScan generation {generation} could not reply; \
                         git_refresh_in_flight can never clear for it"
                    );
                }
                Flow::Continue
            }
            // Flips the still-running scan's cancel flag (if any) and clears
            // the slot: see `tree_scan_cancel`'s own doc for why this, not a
            // generation check inside `tree::fs::scan` itself, is what stops
            // a huge tree's walk once the sidebar that asked for it is
            // already gone.
            Effect::TreeClose => {
                let mut slot = self
                    .tree_scan_cancel
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(cancel) = slot.take() {
                    cancel.store(true, std::sync::atomic::Ordering::Release);
                }
                Flow::Continue
            }
            // A genuine filesystem effect, never RPC (see the effect's own
            // doc): `create_new` refuses to overwrite a destination that
            // already exists rather than truncating it the way a plain
            // `std::fs::write` would -- a destination this create targets
            // may already hold real content a blind truncate would destroy
            // with no way back. `ok` carries through to
            // `Msg::TreeCreateFileResult` unconditionally, so the caller
            // that issued this can rescan on success or notify on refusal.
            Effect::TreeCreateFile { path, generation } => {
                if let Some(tx) = &self.toast_timer {
                    let tx = tx.clone();
                    spawn_or_log("tree-create-file", move || {
                        let ok = std::fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&path)
                            .is_ok();
                        let _ = tx.send(Msg::TreeCreateFileResult { generation, ok });
                    });
                }
                Flow::Continue
            }
            // Symmetric to `TreeCreateFile`.
            Effect::TreeDeleteFile { path, generation } => {
                if let Some(tx) = &self.toast_timer {
                    let tx = tx.clone();
                    spawn_or_log("tree-delete-file", move || {
                        let ok = std::fs::remove_file(&path).is_ok();
                        let _ = tx.send(Msg::TreeDeleteFileResult { generation, ok });
                    });
                }
                Flow::Continue
            }
            // The one caller of `view_ai::TrustStore::set_trusted` (see
            // `Effect::AiTrustSet`'s own doc for why `view-core` cannot call
            // it directly): spawned off the loop thread on the same terms
            // `TreeCreateFile`/`TreeDeleteFile` are, since this is at most
            // one write per session's first `ai` invocation and holding a
            // `TrustStore` open across the executor's own lifetime buys
            // nothing. A write that fails, or finds no state directory to
            // write into, folds to `trusted: false` regardless of what the
            // user answered: the store did not durably record a "yes", so
            // the model must not believe one happened either. No `else`
            // here for an unwired `toast_timer`: unlike every other effect
            // in this match, that degrade needs `update()` itself to
            // self-announce (the model must learn the answer was dropped,
            // not just find nothing persisted later), and `run` has no
            // `&mut Model` of its own to fold a `Msg` through -- see
            // `dispatch`'s own handling of this effect, immediately below
            // its own call to `run`, where a `Model` is in scope.
            Effect::AiTrustSet {
                project_root,
                trusted,
                verb,
            } => {
                if let Some(tx) = &self.toast_timer {
                    let tx = tx.clone();
                    spawn_or_log("ai-trust-set", move || {
                        let resolved = view_ai::TrustStore::load()
                            .and_then(|mut store| {
                                store.set_trusted(&project_root, trusted).map(|()| trusted)
                            })
                            .unwrap_or(false);
                        let _ = tx.send(Msg::AiTrustResolved {
                            trusted: resolved,
                            verb,
                        });
                    });
                }
                Flow::Continue
            }
            // Queued onto the same single worker channel `Effect::AiPromptSubmit`
            // below uses, never dispatched to `self.ai` directly from this
            // thread: a direct dispatch here raced the context worker's
            // (slower, four-read) trip for a `Submit` queued moments
            // earlier, so a `Cancel` following right behind a prompt
            // submission could overtake it -- reaching an idle worker
            // before the prompt it was meant to cancel ever arrived, then
            // spawning a fresh session for a turn nothing was left to
            // cancel by the time the prompt caught up. Routing both through
            // one FIFO channel makes that overtake unrepresentable rather
            // than merely unlikely. See [`crate::ai_context_worker::AiContextJob`]'s
            // own doc.
            Effect::Ai(command) => {
                crate::vlog::log_with("ai", || crate::vlog::ai_command_payload(&command));
                self.queue_ai_job(crate::ai_context_worker::AiContextJob::Direct(command));
                Flow::Continue
            }
            // `update()` stays pure: it hands off the prompt's text, and
            // the context worker (off this thread) performs the four
            // reads, assembles the context, and dispatches the resulting
            // `AiCommand::Prompt` to the agent session itself -- see
            // `Effect::AiPromptSubmit`'s own doc for why `view-core` cannot
            // do this assembly itself, and `ai_context_worker`'s module
            // doc for why the reads cannot run on this thread.
            Effect::AiPromptSubmit { text } => {
                crate::vlog::log_with("ai", || crate::vlog::ai_prompt_submit_payload(&text));
                self.queue_ai_job(crate::ai_context_worker::AiContextJob::Submit { text });
                Flow::Continue
            }
            // Effect is #[non_exhaustive]: same degrade-to-no-op rule.
            // Which means an effect that owes an answer gets no protection
            // from the compiler here -- a missing arm is a silent no-op,
            // not a build failure. `ClipboardRead`/`Write`/`Query` are held
            // to their obligation by their own executor tests instead, and
            // anything added later that owes an answer needs one too.
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
    /// The session's one speculation clock, carried here so the keystroke
    /// that makes a prediction and the loop pass that ages it out read the
    /// same origin. Travels with the reactors because it is attached to the
    /// session in exactly the way they are, and because a second origin
    /// taken anywhere else would produce stamps the first one's readings
    /// cannot be compared against.
    pub speculate: SpeculationClock,
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
/// before the native one because it has nothing to do with the native
/// feature registry: it reads the highlight state `update()` just produced
/// and, at most once per colorscheme change, writes it out -- emitting a
/// native notice of its own only on a write failure.
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
    // the one call site that legitimately needs redraw content: what the
    // engine just said is the only thing a prediction can be judged against
    if let Msg::Redraw(events) = &msg {
        reconcile_speculation(model, events);
    }
    let mut flow = Flow::Continue;
    for eff in update(model, msg) {
        // read off what is actually going to the engine rather than off the
        // message that produced it: a key a native overlay claimed never
        // reaches nvim at all, and a glyph predicted for one would stand
        // over a buffer nobody typed into
        if let Effect::Rpc(call) = &eff {
            note_engine_call(model, call, follow_ups.speculate);
        }
        // `Executor::run` cannot self-announce an unwired `toast_timer`
        // degrade the way its every other effect arm may: these are the
        // effects whose degrade means "tell `update()` its answer was
        // dropped," which needs a `Msg` folded back through `update()`
        // itself, and `run` has no `&mut Model` of its own to do that with
        // -- nor, with no channel wired, anything to send one on. `dispatch`
        // does, so the fold happens here instead of inside `run`'s own match
        // -- recursing into `dispatch` reuses its whole pipeline (follow-ups,
        // speculation, vlog) for the synthesized `Msg` rather than
        // hand-rolling a second copy of it. Never reached outside a bare test
        // `Executor`: every real executor wires `toast_timer` (see `run`'s own
        // comment on that), which is also why the spawn-failure half of the
        // same degrade lives in `run`, where the channel exists to carry it.
        let dropped = executor.toast_timer.is_none().then(|| match &eff {
            Effect::AiTrustSet { verb, .. } => Some(Msg::AiTrustResolved {
                trusted: false,
                verb: verb.clone(),
            }),
            Effect::ReprobeExternalWrite { path } => Some(Msg::ExternalWatchDegraded {
                reason: reprobe_unscheduled(path),
            }),
            _ => None,
        });
        if let Some(Some(answer)) = dropped {
            let sub_flow = dispatch(model, executor, follow_ups, answer);
            if sub_flow != Flow::Continue {
                flow = sub_flow;
                break;
            }
            continue;
        }
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
    for eff in follow_ups.theme.follow_up(model, trigger) {
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

/// The loop's running fold of both sides of the engine connection into the
/// one supervision reading `update()` acts on.
///
/// One fold rather than a watch-shaped notice each, because at most one
/// condition notice is ever shown ([`view_core::model::Messages::set_native_condition`]):
/// two callers raising and retracting that single slot from opposite
/// verdicts would clear each other's text on alternate passes and repaint
/// the frame every time.
#[derive(Debug, Default)]
struct SupervisionFold {
    /// The wedge the previous pass saw, so a healthy pass that follows a
    /// healthy pass can be recognised without a clock read.
    wedge: Option<WedgeKind>,
    /// When the current *episode* was first observed -- the first pass that
    /// saw any wedge at all, not the first that saw this kind. `None` while
    /// there is none.
    since: Option<std::time::Instant>,
}

impl SupervisionFold {
    /// Folds this pass's verdict into the message the loop dispatches, or
    /// `None` when there is nothing new to say.
    ///
    /// The steady state -- no wedge now, none last pass, which is every pass
    /// of a healthy session -- costs two discriminant comparisons and returns
    /// before any clock is read, any message is built, or any dispatch
    /// happens. Everything past that early return is paid for only by a
    /// connection that has actually gone quiet.
    ///
    /// A wedged pass dispatches on every pass rather than only on the
    /// transition. What that buys is a banner whose wording tracks its own
    /// condition -- a reconnect counts attempts, and a wedge that changes
    /// kind re-words itself -- with no transition table to keep in step. The
    /// re-assert itself is idempotent: `Messages::clear` retains view's own
    /// notices, so an engine's `msg_clear` cannot take a live condition down.
    fn note(&mut self, observed: Option<WedgeKind>) -> Option<Msg> {
        if observed.is_none() && self.wedge.is_none() {
            return None;
        }
        let now = std::time::Instant::now();
        if observed != self.wedge {
            self.wedge = observed;
            // the clock belongs to the outage, not to its classification: a
            // write-side stall that later reads as a read-side one is the
            // same connection still quiet, and re-anchoring here would show
            // a user who has waited a minute a readout starting from zero
            self.since = match (observed, self.since) {
                (None, _) => None,
                (Some(_), opened @ Some(_)) => opened,
                (Some(_), None) => Some(now),
            };
        }
        let observed_for = self.since.map_or(std::time::Duration::ZERO, |opened| {
            now.saturating_duration_since(opened)
        });
        Some(Msg::EngineLiveness {
            wedge: observed,
            observed_for,
        })
    }

    /// How long the loop may wait before the visible readout would be
    /// wrong, or `None` while nothing is showing one.
    ///
    /// Costs nothing outside a wedge: the option is `None` and the caller's
    /// existing deadline is returned untouched. Inside one it is the whole
    /// reason the elapsed seconds move at all -- a quiet engine sends
    /// nothing to wake the loop with, so a loop that only woke on traffic
    /// would paint `(31s)` and hold it there until the wedge ended.
    fn readout_deadline(&self) -> Option<std::time::Duration> {
        self.wedge.map(|_| READOUT_RESOLUTION)
    }
}

/// Reads both sides of the engine connection once and folds them into the
/// supervision message the loop dispatches, or `None` when neither side has
/// anything new to say.
///
/// Costs five atomic loads on the steady-state pass: two relaxed loads for
/// the write side's queue reading ([`OutboxStallWatch::observe`], which never
/// takes the engine's writer lock -- a wedge is precisely the state in which
/// that lock is held by a thread parked inside a write), one acquire load of
/// the connection's closed flag ([`EngineHandle::is_closed`]), and the pair
/// [`HeartbeatWatch::observe`] reads to answer whether any probe is
/// outstanding at all. Four more follow in the same pass from
/// [`watch_deadline`], which asks the same watch for a deadline: that same
/// pair again, plus the paused flag and the cadence anchor the prospective
/// deadline is dated from.
///
/// The steady state named here is the exact one all four of those questions
/// short-circuit on: nothing queued for the writer and no probe owed an
/// answer. It costs no lock, no allocation, no walk of the message log and
/// no dispatch. The one reading it does pay for is the clock, once, in
/// [`HeartbeatWatch::poll_deadline`]: a session whose heartbeat cadence is
/// running is always owed another probe, and the deadline that catches an
/// engine wedging between two passes is an instant in the future whether or
/// not a probe happens to be outstanding at this one. The write side still
/// declines the clock on the same evidence its observation does
/// ([`OutboxStallWatch::poll_deadline`]), and pays one `Instant::now()` only
/// on a pass with output pending; a pass with a probe outstanding reads the
/// send-time log in addition to the cadence anchor and takes the same single
/// reading -- both bought by a connection that is demonstrably mid-something
/// rather than by every keystroke.
///
/// Nothing is sent from here at all: the send lives on the engine's own
/// prober thread, so this call cannot await RPC however wedged the engine
/// is, which is the only reason a paint loop can afford to ask on every
/// pass.
fn note_supervision(
    fold: &mut SupervisionFold,
    write: &mut OutboxStallWatch,
    read: &HeartbeatWatch,
    handle: &EngineHandle,
    lost: bool,
    write_lost: bool,
) -> Option<Msg> {
    // a failed write joins the backlog reading rather than replacing it:
    // both describe the same side of the same connection, and the failure
    // is the strongest evidence of it there is -- the outbox watch infers a
    // stall from output that has not moved, while this is the write saying
    // so. It never contributes to `lost`, which is the resolution of a
    // *stop*: a running child that cannot be written to is a wedge with more
    // than one recovery, and `WedgeKind::Dead`'s unattended budget would
    // tear it down with a `qa!` that deletes the swap files the restart is
    // supposed to rehydrate from (see `LoopState::write_lost`)
    let stalled = write.observe(handle) || write_lost;
    // `lost` short-circuits the closed-flag load rather than adding to it:
    // a connection the loop has already resolved as gone is not one this
    // pass has to ask about, and a pass that has not resolved one pays
    // exactly the single acquire load it always did
    fold.note(wedge_kind(
        stalled,
        read.observe(lost || handle.is_closed()),
        lost,
    ))
}

/// The soonest either watch -- or the visible readout -- would have
/// something new to say, or `None` when none of them would.
///
/// `None` from one watch means "as long as you like" and never shortens the
/// other's answer; a caller that took the shorter of `None` and a duration
/// as "no wakeup" would sleep through the one condition that was actually
/// arming a deadline.
fn watch_deadline(wakeups: Wakeups<'_>) -> Option<std::time::Duration> {
    let watches = sooner(wakeups.write.poll_deadline(), wakeups.read.poll_deadline());
    let supervised = sooner(watches, wakeups.supervision.readout_deadline());
    let scheduled = sooner(sooner(supervised, wakeups.speculation), wakeups.reconnect);
    sooner(scheduled, wakeups.spinner)
}

/// The nearer of two deadlines, where `None` is "as long as you like" and
/// so never shortens the other.
fn sooner(
    a: Option<std::time::Duration>,
    b: Option<std::time::Duration>,
) -> Option<std::time::Duration> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (only, None) | (None, only) => only,
    }
}

/// Everything that can ask the loop to wake itself up rather than be woken
/// by traffic. All of them are read together on every pass, so they travel
/// together: a caller holding all but one gets a loop that sleeps through
/// the one condition it was arming for.
///
/// The three watches are borrowed and asked for their own deadlines here;
/// speculation's arrives already resolved because it is the one deadline
/// read off the model rather than off a watch, and the model is borrowed
/// mutably everywhere else on the pass.
#[derive(Clone, Copy)]
struct Wakeups<'a> {
    write: &'a OutboxStallWatch,
    read: &'a HeartbeatWatch,
    supervision: &'a SupervisionFold,
    speculation: Option<std::time::Duration>,
    /// When the next reconnect attempt comes due, resolved for the same
    /// reason speculation's is: it is read off a schedule the loop owns
    /// rather than off a watch, and a dead connection sends nothing that
    /// would otherwise wake the loop to take the attempt.
    reconnect: Option<std::time::Duration>,
    /// When the agent panel's spinner owes its next frame
    /// ([`expire_ai_spinner`]) -- the one wakeup here that exists to move
    /// something on screen rather than to re-read something off the wire.
    spinner: Option<std::time::Duration>,
}

/// Waits for the loop's next message, bounded by whichever watch has a
/// deadline. `None` means the wait expired with nothing delivered and the
/// caller should re-read both sides of the connection.
///
/// Unbounded whenever nothing asks for a wakeup, which is a session whose
/// heartbeat is paused with no prediction pending: with no probe coming and
/// none owed there is no silence either watch could report on, and with
/// nothing painted ahead of the engine there is no age bound to keep, so the
/// loop sleeps until a keystroke, a redraw or an engine request wakes it. A
/// prediction is what turns that same silence into a bounded wait: it is
/// painted over the authoritative grid on a promise about wall-clock time,
/// and the pass that keeps the promise has to be a pass the loop takes. A
/// session whose engine
/// connection is gone is not that case -- its prober retires on the first
/// tick the connection refuses, and the watch it leaves behind keeps asking
/// to be read again one threshold later, which is the cadence a recovery or
/// a resolution is noticed on rather than a wait to be slept through.
/// A live cadence always bounds the wait, and for the wedged case that
/// bound is the entire point: a wedged engine emits no redraws, so an
/// operator who stops typing -- or who never typed at all, since a wedge
/// can open while the session is idle -- would otherwise be told nothing at
/// all. The bound is not a periodic wakeup a healthy session pays: the
/// engine's own answer arrives first on every pass and the next wait is
/// recomputed from the tick that answer belongs to.
#[cfg(any(not(unix), test))]
fn wait_for_msg(
    msg_rx: &mpsc::Receiver<Msg>,
    wakeups: Wakeups<'_>,
) -> Option<Result<Msg, mpsc::RecvError>> {
    let Some(deadline) = watch_deadline(wakeups) else {
        return Some(msg_rx.recv());
    };
    match msg_rx.recv_timeout(deadline) {
        Ok(msg) => Some(Ok(msg)),
        Err(mpsc::RecvTimeoutError::Timeout) => None,
        Err(mpsc::RecvTimeoutError::Disconnected) => Some(Err(mpsc::RecvError)),
    }
}

/// Ends the session's agent when [`run`]'s frame is torn down, whichever
/// way it leaves.
///
/// A guard rather than a call before each `return`: `run` has five exits and
/// an error path through `?`, and a shutdown missing from any one of them is
/// an agent child that outlives the editor with no way for the user to see
/// it, let alone stop it.
struct AgentShutdown(crate::ai_worker::AiWorker);

impl Drop for AgentShutdown {
    fn drop(&mut self) {
        self.0.shutdown();
        crate::vlog::log("exit", "agent stopped");
    }
}

/// The terminal-input handle [`run`] polls and drains inline: the pollable
/// handle `view-tui` exposes on unix, and nothing at all elsewhere (the
/// non-unix loop still receives input from the dedicated thread through
/// the message channel). An alias rather than a `#[cfg]`-gated parameter
/// so `main.rs` has one call shape per function instead of a duplicated
/// call per platform.
#[cfg(unix)]
pub type TermInput<'a> = &'a mut view_tui::input::InputSource;
#[cfg(not(unix))]
pub type TermInput<'a> = &'a mut ();

/// The two input-side handles [`run`] owns alongside the message channel:
/// the resize cell every frame consults and the platform's terminal-input
/// handle. One parameter rather than two because they are two views of the
/// same terminal input stream, and passing them together keeps `run`'s
/// signature at the arity the caller can still read.
pub struct InputHandles<'a> {
    pub term_size: view_tui::terminal::TermSizeCell,
    pub input: TermInput<'a>,
}

/// [`wait_for_msg`]'s unix counterpart: sleeps in the fd readiness poll
/// over the terminal fd, the SIGWINCH pipe, the fatal-signal pipe (whose
/// delivery becomes [`Msg::Terminated`], so an exit view did not choose
/// still leaves through the one teardown), and the wake pipe, so a
/// keystroke wakes this thread directly and is decoded inline in
/// `view-tui` -- the cross-thread hop the input thread used to charge
/// every key is gone by construction. Staged terminal events win ties: a
/// keystroke already drained into `pending` pops before the channel is
/// checked, and each call returns one message, so the loop still paints
/// between a burst's events exactly as it did when each arrived as its
/// own `recv` wakeup.
///
/// The rearm ordering is the lost-wakeup guard: [`LoopWaker::clear`]
/// runs *before* the final queue re-check, so a send consumed by an
/// earlier check can never leave a stale byte, and a send landing after
/// the re-check writes a fresh byte the poll sees. `None` means the stall
/// deadline elapsed, exactly as in [`wait_for_msg`].
///
/// [`LoopWaker::clear`]: crate::wake::LoopWaker::clear
///
/// # Errors
///
/// Returns the underlying `std::io::Error` if the readiness poll itself
/// fails; the caller aborts the session the way it does for any other
/// terminal I/O failure.
#[cfg(unix)]
#[allow(clippy::type_complexity)]
fn wait_for_msg_unified(
    msg_rx: &mpsc::Receiver<Msg>,
    wakeups: Wakeups<'_>,
    input: &mut view_tui::input::InputSource,
    waker: &crate::wake::LoopWaker,
    term_size: &view_tui::terminal::TermSizeCell,
    pending: &mut std::collections::VecDeque<Msg>,
    armed: &mut Option<Option<u128>>,
) -> std::io::Result<Option<Result<Msg, mpsc::RecvError>>> {
    loop {
        if let Some(msg) = pending.pop_front() {
            return Ok(Some(Ok(msg)));
        }
        match msg_rx.try_recv() {
            Ok(msg) => return Ok(Some(Ok(msg))),
            Err(mpsc::TryRecvError::Disconnected) => return Ok(Some(Err(mpsc::RecvError))),
            Err(mpsc::TryRecvError::Empty) => {}
        }
        waker.clear();
        match msg_rx.try_recv() {
            Ok(msg) => return Ok(Some(Ok(msg))),
            Err(mpsc::TryRecvError::Disconnected) => return Ok(Some(Err(mpsc::RecvError))),
            Err(mpsc::TryRecvError::Empty) => {}
        }
        let deadline = watch_deadline(wakeups);
        // logged on change rather than on every block: an idle session
        // re-arms the same engine-liveness deadline twice a second forever,
        // and a log a user is asked to attach to a bug report was 96% that
        // one repeated line. A deadline that moves still logs, so every
        // distinct arming is on the record
        let arming = deadline.map(|d| d.as_millis());
        if *armed != Some(arming) {
            *armed = Some(arming);
            crate::vlog::log_with("sleep", || match arming {
                Some(ms) => format!("armed {ms}ms"),
                None => "unbounded".to_owned(),
            });
        }
        let ready = crate::wake::poll_readiness(input, waker, deadline)?;
        // before the drain and before the timeout verdict: a signal
        // delivered mid-poll is this session's last message, and anything
        // dispatched ahead of it would be work done for a process that is
        // already leaving
        if let Some(signal) = input.take_fatal_signal() {
            crate::vlog::log_with("exit", || format!("signal {signal}"));
            return Ok(Some(Ok(Msg::Terminated { signal })));
        }
        if ready.timed_out {
            crate::vlog::log("sleep", "expired");
            return Ok(None);
        }
        if ready.input {
            input.drain(term_size, |msg| pending.push_back(msg));
        }
    }
}

/// Turns one delivered wakeup into the `Msg` the loop dispatches, taking
/// the connection-level bookkeeping that has to happen exactly once per
/// message and cannot happen inside `update()`: the pure fold has no engine
/// to ask and no clock to read.
///
/// A function rather than a `match` inline in [`run`] so the bookkeeping is
/// reachable without a terminal, an executor or a loop -- every arm here is
/// a place a connection-level fact enters the model, and an arm nothing can
/// call is an arm nothing can prove.
///
/// `None` means the wakeup carried nothing this session should act on: a
/// terminal message belonging to a connection it has already replaced.
/// The whole stop, in the shape [`step`] resolves it from -- or `None` when
/// the child is still running and so there is no stop to resolve.
///
/// Observes the child rather than stopping it
/// ([`Engine::stop_report_if_exited`], never `stop_report`): the only caller
/// is the failed-write arm, which holds evidence about a pipe and none about
/// the process behind it.
///
/// A named function rather than a closure spelled out at each of `run`'s
/// dispatch sites: three copies of "what counts as a stop" is exactly the
/// drift `step` exists to prevent.
fn engine_stop(engine: &mut Engine) -> Option<crate::recovery::EngineStop> {
    let (exit, announced_exit) = engine.stop_report_if_exited()?;
    Some(crate::recovery::EngineStop {
        exit,
        announced_exit,
    })
}

fn intake(
    received: Result<Msg, mpsc::RecvError>,
    engine: &mut Engine,
    pump: &view_engine::DamagePump,
    model: &mut Model,
) -> Option<Msg> {
    Some(match received {
        Ok(Msg::RedrawReady) => Msg::Redraw(pump.take_damage()),
        Ok(Msg::HeartbeatReply { generation }) => {
            // recorded here rather than in `update()`: this is the runtime
            // loop's own thread, the same one that folds the verdict a pass
            // later, so the acknowledgement and the reading of it are
            // ordered by the loop itself rather than by the memory model
            engine.heartbeat.record_ack(generation);
            Msg::HeartbeatReply { generation }
        }
        // a stop belonging to a connection this session has already
        // replaced. One loop channel serves every engine a session opens,
        // and the reader of a replaced engine posts its stop only after the
        // replacement is live -- the restart's own teardown is what
        // produces it. Acted on, it would resolve against the engine now
        // running: `wait_exit` below would kill the healthy replacement,
        // and the session would report the death it had just recovered
        // from. There is nothing left to do about a connection that is
        // gone and already succeeded, so it is dropped outright.
        Ok(Msg::EngineStopped { generation, .. }) if generation != engine.generation() => {
            crate::vlog::log_with("engine", || {
                format!(
                    "dropped a stop from replaced engine generation {generation} (running {})",
                    engine.generation()
                )
            });
            return None;
        }
        Ok(Msg::EngineStopped { generation, reason }) => {
            // Same bounded (up to `shutdown_timeout`) block as the
            // `Flow::EngineLost` arm in `run`, but the reader thread's
            // stream has already ended by the time this fires, so the
            // `qa!` send is a harmless no-op and the first `try_wait`
            // typically finds the child already exited.
            let exit = engine.wait_exit();
            let recoverable = model
                .supervision
                .note_engine_stop(exit, engine.handle.announced_exit());
            // stashed on the model rather than reported here: this loop
            // runs behind the terminal's raw-mode alternate screen, so
            // `main` reports it only after `run` returns and the
            // terminal is restored (see Msg::EngineStopped's doc). A
            // recovery clears it again -- a session that came back has no
            // fatal reason left to print at exit.
            model.fatal_reason = reason.clone();
            if recoverable {
                // passed through rather than resolved into a quit: the
                // supervision fold owns this connection from here, and the
                // stop is a state to report and offer recovery from
                Msg::EngineStopped { generation, reason }
            } else {
                Msg::EngineDown(exit)
            }
        }
        Ok(m) => m,
        Err(_) => Msg::EngineDown(ExitInfo {
            code: None,
            by_signal: false,
        }),
    })
}

/// Runs the unified loop until `update()` produces `Effect::Quit` or a
/// terminal I/O error occurs, returning the final `Model` alongside the
/// process exit code on the former (the caller persists the model's
/// last-derived theme for the next startup's cold-start cache; see
/// `theme_cache` in `main.rs`).
///
/// The message channel's two halves, bundled into one parameter: `rx` is
/// `run()`'s own blocking receive end, while `tx` is cloned once more into
/// the toast-expiry timer thread the same way `start_pump`'s sink already
/// holds its own clone from before `run()` starts.
/// A bare tuple would satisfy the same arg-count constraint but loses the
/// field names at every call site; a struct keeps `tx`/`rx` self-labeling
/// where `run()`'s doc comment already talks about both by name.
pub struct MsgChannel {
    pub tx: crate::wake::LoopSender,
    pub rx: mpsc::Receiver<Msg>,
}

/// Takes ownership of `engine` for the whole call (see the module docs'
/// ownership chain), plus the already-attached `pump` and the `msg_rx` end
/// of the channel `pump`'s sink and every other producer already feed.
/// Both are built by `startup` rather than here: input capture goes live
/// (and `msg_tx`/`msg_rx` are created) right after the very first shell
/// frame paints, well before this function is ever called, so a key typed
/// while the engine is still attaching is never lost -- see
/// `startup::drain_pre_attach` for the buffering that covers exactly that
/// window. The executor drives
/// `engine.handle` through [`EngineOps`]. Painting fires immediately when
/// `update()` marks `model.dirty`, and the loop blocks in
/// [`wait_for_msg`], which a redraw, a keystroke, or an engine request
/// wakes directly.
///
/// The loop body runs no timer of its own, but an idle session is not a
/// silent one: the read side's heartbeat is answered on this same channel
/// every
/// [`HEARTBEAT_PROBE_INTERVAL`](view_engine::heartbeat::HEARTBEAT_PROBE_INTERVAL),
/// so a session with nobody typing wakes at that cadence, records the
/// acknowledgement in [`intake`], marks nothing dirty and paints nothing.
/// Those wakeups are what the read side's deadline is measured against
/// rather than something it sits beside: the block is bounded by whichever
/// watch asks for the sooner look -- engine-bound output pending on the
/// write side, a probe still unanswered on the read side, or, with neither
/// true, the instant the next probe could itself have gone unanswered for a
/// threshold (see [`watch_deadline`]). The last of those is the one an idle
/// user's session runs on, and an engine that keeps answering never reaches
/// it. A pending prediction adds a fourth and much shorter look, because
/// its age bound is a promise about wall-clock time that only a pass can
/// keep (see [`crate::speculate::next_expiry`]); it is armed only while
/// something is painted ahead of the engine, so an idle session's wait is
/// exactly the wait it always was.
///
/// # Latency
///
/// [`drain_osc52`] runs on this loop thread, once per pass, ahead of
/// `draw_surface` -- it is not off the hot path the way the clipboard
/// worker (`crate::clipboard::spawn`, a dedicated thread) is. Its cost is
/// bounded rather than eliminated: the common case (no `Osc52Copy` queued
/// this pass, the overwhelming majority of passes) is one non-blocking
/// `try_recv()` returning empty, no syscall and no allocation. The rare
/// case (a yank just happened) costs exactly one bounded write+flush --
/// [`view_core::msg::OSC52_MAX_PAYLOAD_BYTES`] caps the base64-encoded
/// payload at 100 KiB before it ever reaches the sink, so the worst case
/// this pass can add is one syscall pair on a fixed-size buffer, not an
/// unbounded one. A
/// transient write error or an over-cap payload is logged and skipped
/// rather than retried or escalated, so this never turns into a stall the
/// way an engine-bound write can (see `OutboxStallWatch`).
///
/// A restart is the one call this loop makes that blocks on the engine: it
/// tears the dead one down and brings its replacement up before the pass
/// continues. It runs at most once per engine death, on a transition the
/// user asked for or the auto-recovery rule took, and never on a
/// steady-state pass -- the same terms the bounded `wait_exit` teardown
/// already runs on (see [`crate::recovery::restart_engine`]).
///
/// The read-side liveness watch runs on this thread too, and costs this
/// loop seven atomic loads and one monotonic clock read per steady-state
/// pass: three in [`note_engine_liveness`] (the connection's closed flag,
/// plus the sent and acknowledged generations) and four more when
/// [`watch_deadline`] asks the same watch what deadline to arm -- that
/// generation pair again, the paused flag, and the cadence anchor the
/// reading is dated against. No lock and no send in that state -- it cannot
/// block on the very connection it is asking about. Its recurring cost is
/// the wakeup rather than the fold: one extra pass every probe interval,
/// ending in a dispatch that produces no effect and no paint.
///
/// Speculation's own deadline costs the steady-state pass one length read
/// on an empty list -- [`crate::speculate::next_expiry`] returns before any
/// clock is read, exactly as the per-pass expiry site does. Inside a typing
/// burst it costs one monotonic clock read and one saturating subtraction
/// per pending prediction, and the list is bounded by what a person can
/// type inside [`SPECULATION_MAX_AGE`]: single digits of cells, a walk far
/// shorter than the frame it precedes.
///
/// [`SPECULATION_MAX_AGE`]: view_core::native::speculate::SPECULATION_MAX_AGE
///
/// # Errors
///
/// Returns the underlying `std::io::Error` if a terminal paint fails (the
/// `Model` is dropped on this path along with everything else on the
/// stack; an aborted session has no last-good theme worth persisting).
#[cfg_attr(not(unix), allow(unused_variables))]
pub fn run(
    mut model: Model,
    session: EngineSession<'_>,
    msg_channel: MsgChannel,
    inputs: InputHandles<'_>,
    follow_ups: &mut FollowUps<'_>,
    term: &mut Term,
    ai_agent: view_ai::AgentSpec,
) -> anyhow::Result<(Model, i32)> {
    let EngineSession {
        mut engine,
        mut pump,
        respawn,
    } = session;
    let MsgChannel {
        tx: msg_tx,
        rx: msg_rx,
    } = msg_channel;
    let InputHandles { term_size, input } = inputs;
    // taken before `msg_tx` moves into the executor: the wait below rearms
    // this exact waker, and a loop polling fds with no waker wired would
    // sleep through every channel send
    #[cfg(unix)]
    let waker = msg_tx.waker().cloned().ok_or_else(|| {
        anyhow::anyhow!("the unix runtime loop requires a wake-wired message sender")
    })?;
    #[cfg(unix)]
    let mut pending = std::collections::VecDeque::new();
    // the last deadline this loop logged arming, so the idle cadence is
    // recorded once per value rather than once per block (see the wait's
    // own comment)
    #[cfg(unix)]
    let mut armed: Option<Option<u128>> = None;
    let (clipboard_tx, clipboard_rx) = mpsc::channel();
    let (osc52_tx, osc52_rx) = mpsc::channel();
    // the route, not the handle: the worker answers whichever engine this
    // session currently has, and a restart re-points it rather than
    // starting a second worker (see `ReplyRoute`)
    let clipboard_route = crate::clipboard::ReplyRoute::new(engine.handle.clone());
    // kept alive for the session's duration; the worker exits once every
    // `clipboard_tx` clone (held by `channels` and by `executor`) drops at
    // the end of this function, same lifetime as the engine's own
    // reader/writer threads
    let _clipboard_worker = crate::clipboard::spawn(clipboard_route.clone(), clipboard_rx)?;
    let (picker_tx, picker_rx) = mpsc::channel();
    // kept alive for the process's duration, the same shape as
    // `_clipboard_worker` above: the matcher worker exits once `picker_tx`
    // drops at the end of this function
    let _picker_worker = view_native::picker::matcher::spawn(picker_rx, msg_tx.clone());
    // built from the model's own cwd, resolved once at startup
    // (`main.rs`'s `Model::with_cwd`) and never reassigned afterward, so an
    // engine restart's fresh `Executor` re-wiring this same worker (below)
    // still targets the one project this session was ever asked about
    let ai = crate::ai_worker::AiWorker::new(ai_agent, model.cwd.clone(), msg_tx.clone());
    // declared here so every way out of this function -- the quit returns
    // below and the `?` on a terminal write alike -- signals the agent
    // child, which no refcount can promise (see `AiWorker::shutdown`)
    let _ai_shutdown = AgentShutdown(ai.clone());
    let (ai_context_tx, ai_context_rx) = mpsc::channel();
    // the route, not the handle, for the same reason `clipboard_route` is:
    // a restart re-points this worker at the fresh engine rather than
    // starting a second one (see `ai_context_worker::OpsRoute`)
    let ai_context_route = crate::ai_context_worker::OpsRoute::new(engine.handle.clone());
    // kept alive for the session's duration, the same shape as
    // `_clipboard_worker`: the worker exits once every `ai_context_tx`
    // clone (held by `channels`) drops at the end of this function
    let _ai_context_worker =
        crate::ai_context_worker::spawn(ai_context_route.clone(), ai.clone(), ai_context_rx)?;
    let channels = LoopChannels {
        clipboard: clipboard_tx,
        osc52: osc52_tx,
        picker: picker_tx,
        ai_context: ai_context_tx,
        msg: msg_tx,
        ai,
    };
    let mut executor = channels.executor(engine.handle.clone(), clipboard_route.epoch());
    let mut write_stall = OutboxStallWatch::default();
    let mut supervision = SupervisionFold::default();
    let mut state = LoopState::default();
    let mut reconnect = ReconnectSchedule::default();
    let mut spinner_due: Option<Instant> = None;
    // frame-to-frame surface reuse; the paint site below is this loop's
    // only consumer, so the cache's previous-frame invariant holds by
    // construction (startup's pre-attach paints predate the loop and go
    // through their own full render)
    let mut surface_cache = view_surface::SurfaceCache::new();

    loop {
        // ahead of everything else in the pass: every reading below, and
        // every dispatch, addresses the engine this session has now. A
        // replacement performed here rather than where it was asked for
        // happens once, with nothing borrowed from the engine it replaces.
        if state.restart_requested {
            state.restart_requested = false;
            // a dropped remote connection is the one replacement that waits:
            // the far side is often briefly unreachable, and an immediate
            // retry would spin the ssh client against a host that is still
            // coming back (see `ReconnectSchedule`)
            reconnect.request(
                reconnects(&engine, state.connection_lost),
                std::time::Instant::now(),
            );
        }
        // the clock behind `armed`, never in front of it: a session with
        // nothing scheduled -- every pass of a healthy one -- costs the bool
        if reconnect.armed() && reconnect.take_due(std::time::Instant::now()) {
            match restart_engine(
                &mut engine,
                respawn,
                &model,
                &channels,
                &clipboard_route,
                &ai_context_route,
            ) {
                Ok(fresh) => {
                    reconnect.clear();
                    engine = fresh.engine;
                    pump = fresh.pump;
                    executor = fresh.executor;
                    // the fresh connection answers on its own channel and
                    // carries none of the registrations the dead one was
                    // given; its own `VimEnter` performs the takeover again
                    follow_ups.native.rebind(engine.api_info.channel_id);
                    // both watches read the connection they were built for:
                    // the heartbeat comes fresh with the engine, and the
                    // write side is reset here for the same reason
                    write_stall = OutboxStallWatch::default();
                    state.connection_lost = false;
                    // the replacement's write path is new, so the failure
                    // that classified the old one describes nothing here
                    state.write_lost = false;
                    // answered, not reported: a session that came back has
                    // no fatal reason left to print at exit
                    model.fatal_reason = None;
                    model.dirty = true;
                    if let crate::startup::CutoverOutcome::Quit(code) = crate::startup::run_cutover(
                        &mut model,
                        &executor,
                        follow_ups,
                        fresh.staged,
                        || engine.wait_exit(),
                    ) {
                        return Ok((model, code));
                    }
                }
                // a reconnect sequence absorbs its own failures: the next
                // attempt is scheduled, or the sequence has run out and the
                // choice goes back to the user through the dead-engine
                // modal, and either way the session keeps running against
                // the connection it still holds
                Err(_) if reconnect.note_failure(std::time::Instant::now()) => {}
                // no second engine and no way to ask for one: the modal
                // that offered the restart is gone with the engine it
                // offered it for, so this ends the session with the reason
                // `main` prints once the terminal is restored
                Err(failure) => {
                    model.running = false;
                    model.fatal_reason = Some(match failure {
                        crate::startup::AttachFailure::Spawn(err) => {
                            format!("the engine could not be restarted: {err}")
                        }
                        crate::startup::AttachFailure::Attach(err) => {
                            format!("the restarted engine could not be attached: {err}")
                        }
                    });
                    return Ok((model, 1));
                }
            }
        }
        // what the banner says about that schedule, folded before the
        // reading that raises it: the fold owns the text, this owns the
        // count, and a pass that changed neither writes nothing
        if model.supervision.note_reconnect(reconnect.progress()) {
            model.dirty = true;
        }
        // every pass, whatever the engine has or has not sent: an age bound
        // reachable only when a redraw arrives could never fire during the
        // total redraw stall it exists to bound
        expire_speculation(&mut model, follow_ups.speculate);
        // before the paint below rather than after it, so the frame this
        // pass draws is the one the deadline came due for
        crate::spinner::expire(&mut model, &mut spinner_due, Instant::now());
        drain_pass_handoffs(&osc52_rx, term, &executor);
        // a resize the input reader has already seen describes the terminal
        // as it is now, whatever traffic is still queued ahead of its
        // Msg::Resized: folding it in here means no frame is ever painted
        // at a shape the terminal has left. Costs one relaxed load per pass
        // when nothing resized, which is the whole steady state.
        if let Some((width, height)) = term_size.take() {
            if let Some(code) = step(
                &mut model,
                &executor,
                follow_ups,
                &mut state,
                || engine_stop(&mut engine),
                Msg::Resized { width, height },
            ) {
                return Ok((model, code));
            }
        }
        // both sides read here, immediately before the paint that would show
        // what they found: an engine that has stopped reading view's output
        // -- or stopped answering it -- also sends no redraws, so nothing
        // else in this loop can notice
        if let Some(msg) = note_supervision(
            &mut supervision,
            &mut write_stall,
            &engine.heartbeat,
            &engine.handle,
            state.connection_lost,
            state.write_lost,
        ) {
            if let Some(code) = step(
                &mut model,
                &executor,
                follow_ups,
                &mut state,
                || engine_stop(&mut engine),
                msg,
            ) {
                return Ok((model, code));
            }
            // straight back to the top rather than on through the wait
            // below, matching what the message path does after its own
            // batch: an engine that has stopped sending anything wakes this
            // loop only on the readout timer, so a restart the fold itself
            // asked for would otherwise sit unstarted for up to
            // `READOUT_RESOLUTION` behind a screen the user cannot use.
            if state.restart_requested {
                continue;
            }
        }
        // paint before blocking, not after processing: state mutated ahead
        // of the loop (the startup cutover replays staged messages straight
        // through dispatch) would otherwise sit unpainted until the next
        // message happens to arrive. Steady-state behavior is unchanged --
        // each processed wakeup paints here on the next pass, immediately,
        // with no post-redraw silence timeout and no input-drain budget.
        if model.dirty {
            let surface = surface_cache.render(&model);
            let damage = model.take_paint_damage();
            term.draw_surface(&model, surface, &damage)?; // a frame's own terminal I/O error aborts; engine errors never do, and neither does the OSC52 drain above (fire-and-forget, see its own comment)
            model.dirty = false;
        }
        // resolved here rather than inside the wait because it is the one
        // deadline read off the model, which the wait does not hold
        let speculation = crate::speculate::next_expiry(&model, follow_ups.speculate);
        let due = reconnect
            .armed()
            .then(|| reconnect.poll_deadline(std::time::Instant::now()))
            .flatten();
        let spinner = crate::spinner::next_frame(spinner_due, Instant::now());
        #[cfg(unix)]
        let received = wait_for_msg_unified(
            &msg_rx,
            Wakeups {
                write: &write_stall,
                read: &engine.heartbeat,
                supervision: &supervision,
                speculation,
                spinner,
                reconnect: due,
            },
            input,
            &waker,
            &term_size,
            &mut pending,
            &mut armed,
        )?;
        #[cfg(not(unix))]
        let received = wait_for_msg(
            &msg_rx,
            Wakeups {
                write: &write_stall,
                read: &engine.heartbeat,
                supervision: &supervision,
                speculation,
                spinner,
                reconnect: due,
            },
        );
        let Some(received) = received else {
            // the wait expired against the stall watch's own deadline
            // rather than delivering anything: go around and re-read the
            // write side, which is the whole reason the deadline was armed
            continue;
        };
        #[cfg(all(unix, feature = "bench-taps"))]
        if received.is_ok() {
            view_tui::tap::tap(view_tui::tap::TAG_LOOP_WAKE);
        }
        let Some(msg) = intake(received, &mut engine, &pump, &mut model) else {
            // nothing to dispatch: the intake dropped a message addressed
            // to a connection this session no longer runs
            continue;
        };
        // the intake resolved a stop it judged a death rather than the
        // session ending: from here the supervision fold owns this
        // connection, and `WedgeKind::Dead` is a verdict it may reach
        state.connection_lost |= matches!(msg, Msg::EngineStopped { .. });
        let mut queue = vec![msg];
        let mut drained_residue = false;
        while let Some(msg) = queue.pop() {
            if let Some(code) = step(
                &mut model,
                &executor,
                follow_ups,
                &mut state,
                || engine_stop(&mut engine),
                msg,
            ) {
                return Ok((model, code));
            }
            // the rest of this batch was addressed to an engine that is
            // being replaced, and the replacement happens at the top of the
            // next pass; the residue drain below is what keeps the damage
            // it staged from being stranded
            if state.restart_requested {
                break;
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

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::disallowed_methods
    )]
    use super::*;
    use crate::engine_ops::{FakeOps, SlowOps};
    use crate::osc52::FakeOsc52Sink;
    use view_core::msg::{
        BufferHandle, OptionValue, RegisterType, ReplyToken, ReplyValue, TextEdit,
    };

    /// Serializes every test here that mutates `XDG_STATE_HOME`, the same
    /// reason `view-native::paths`' and `view-ai::trust`'s own suites each
    /// hold a module-local guard: the base directory `view_ai::TrustStore`
    /// resolves is process-global, and two tests racing their own
    /// plant/restore would interleave.
    static ENV_MUTATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_mutation_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_MUTATION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `Messages::visible_lines` returns one span-row per line; these tests
    /// only assert on the text a stall notice carries, so this flattens
    /// each row back to a plain string.
    /// Drives one loop pass's worth of supervision: the same production fold
    /// [`run`] calls, with the message it produces folded through `update()`
    /// the way `dispatch` would. Answers whether that pass changed anything
    /// visible, which is the loop's own cue to repaint.
    ///
    /// Always reads the connection as one whose stop the loop has not
    /// resolved, because every test driving this helper drives a connection
    /// that is still open. The resolved-death path that flag opens is driven
    /// directly against [`note_supervision`] by the tests that own it.
    fn note_supervision_pass(
        model: &mut Model,
        fold: &mut SupervisionFold,
        write: &mut OutboxStallWatch,
        read: &HeartbeatWatch,
        handle: &EngineHandle,
    ) -> bool {
        let Some(msg) = note_supervision(fold, write, read, handle, false, false) else {
            return false;
        };
        model.dirty = false;
        let effects = update(model, msg);
        assert!(
            effects.is_empty(),
            "a liveness reading produced effects the loop would have to run: {effects:?}"
        );
        model.dirty
    }

    fn visible_texts(model: &Model) -> Vec<String> {
        model
            .engine
            .messages
            .visible_lines(4)
            .into_iter()
            .map(|spans| spans.into_iter().map(|s| s.text).collect::<String>())
            .collect()
    }

    /// A path the engine refuses outright is a refusal of that path, never
    /// a lost connection: the loop keeps running, and the review that asked
    /// gets the same buffer-less resolve nvim's own refusal would have
    /// produced, so it reads as unbindable instead of waiting forever for a
    /// reply nothing sent.
    #[test]
    fn a_load_hidden_the_engine_refuses_answers_the_review_instead_of_losing_the_engine() {
        let (msg_tx, msg_rx) = mpsc::sync_channel(4);
        let ops = FakeOps::default();
        let executor =
            Executor::new(&ops).with_toast_timer(crate::wake::LoopSender::new(msg_tx.clone()));

        let flow = executor.run(Effect::Rpc(RpcCall::LoadHidden {
            path: String::new(),
            generation: 9,
        }));

        assert!(
            matches!(flow, Flow::Continue),
            "a refused path must not read as a lost engine: {flow:?}"
        );
        assert!(
            !ops.calls
                .borrow()
                .iter()
                .any(|c| c.starts_with("load_hidden")),
            "the refusal happens before the call reaches the wire"
        );
        let msg = msg_rx.try_recv().ok();
        assert!(
            matches!(
                msg,
                Some(Msg::HiddenBufferLoaded {
                    generation: 9,
                    buf: None,
                    created: false,
                    changedtick: 0,
                })
            ),
            "the review that asked must be told it cannot bind, got {msg:?}"
        );
    }

    /// The agent's filesystem gate in `view-core` spells its own
    /// "absolute?" predicate, because `view-core` cannot name
    /// `view_engine::nvim_api::hidden_path_refusal` -- the authority on
    /// unusable path spellings -- without inverting the dependency
    /// direction. This bin crate can name both, so the seam is pinned here:
    /// whatever core refuses on its own, the engine's set must refuse too.
    ///
    /// The subset direction is the dangerous one. A core predicate that
    /// grew past the engine's would have this client refuse, in its own
    /// words, a path nvim would have opened -- an agent told a file it can
    /// see is unusable, with nothing in either crate's own suite noticing.
    #[test]
    fn the_cores_own_path_refusal_is_a_subset_of_the_engines() {
        let spellings = [
            "",
            "relative.rs",
            "./relative.rs",
            "nested/relative.rs",
            "/absolute/dir/",
            "/absolute/dir\\",
            "/absolute/file.rs",
            "/absolute/no-extension",
        ];

        for path in spellings {
            let mut model = view_core::model::Model::new();
            model.ai_trusted = true;
            let effects = view_core::update::update(
                &mut model,
                Msg::Ai(view_core::native::ai_event::AiEvent::FsReadRequested {
                    request_id: 1,
                    path: std::path::PathBuf::from(path),
                    line: None,
                    limit: None,
                }),
            );
            let core_refused = !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Rpc(RpcCall::LoadHidden { .. })));
            if core_refused {
                assert!(
                    view_engine::nvim_api::hidden_path_refusal(path).is_some(),
                    "core refuses {path:?} on its own, but the engine's \
                     refusal set would have opened it"
                );
            }
        }
    }

    /// The stand-in above is only for the refusal: a usable path still
    /// reaches the wire and answers nothing here, since the reply comes
    /// back through the engine's own pump.
    #[test]
    fn a_usable_load_hidden_path_still_reaches_the_engine_and_answers_nothing_locally() {
        let (msg_tx, msg_rx) = mpsc::sync_channel(4);
        let ops = FakeOps::default();
        let executor =
            Executor::new(&ops).with_toast_timer(crate::wake::LoopSender::new(msg_tx.clone()));

        let path = if cfg!(windows) {
            "C:\\work\\main.rs"
        } else {
            "/work/main.rs"
        };
        let flow = executor.run(Effect::Rpc(RpcCall::LoadHidden {
            path: path.to_owned(),
            generation: 9,
        }));

        assert!(matches!(flow, Flow::Continue));
        assert_eq!(ops.calls.borrow()[0], format!("load_hidden({path},9)"));
        assert!(
            msg_rx.try_recv().is_err(),
            "a call that reached the wire must wait for nvim's own reply"
        );
    }

    /// And the hold that load took has to come back. The engine matches
    /// the two by count and deletes the buffer only when the last one is
    /// given back (`view-engine`'s own `hidden_buffer_live` suite proves
    /// that half against real nvim), so the release is asserted where both
    /// halves are observable at once: a whole agent read driven through
    /// `dispatch`, with the calls that reached the engine read back in
    /// order. An arm wired to anything but `release_hidden` leaks one
    /// buffer per read the agent makes, and leaves every effect-level
    /// assertion in this suite green while it does.
    #[test]
    fn an_agent_read_gives_back_the_hold_it_took_on_the_buffer_it_read() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let mut model = Model::with_term_size(80, 24);
        model.ai_trusted = true;
        let mut native = NativeSession::inert();
        let mut bridge = ThemeBridge::new(None);
        let mut follow_ups = FollowUps {
            native: &mut native,
            theme: &mut bridge,
            speculate: crate::speculate::SpeculationClock::default(),
        };
        let path = if cfg!(windows) {
            "C:\\work\\main.rs"
        } else {
            "/work/main.rs"
        };

        let _ = dispatch(
            &mut model,
            &executor,
            &mut follow_ups,
            Msg::Ai(view_core::native::ai_event::AiEvent::FsReadRequested {
                request_id: 3,
                path: std::path::PathBuf::from(path),
                line: None,
                limit: None,
            }),
        );
        // Read back rather than assumed: the generation is minted by the
        // fold, and a release that carried a different one would be a hold
        // given back for a request nobody made.
        let load = ops.calls.borrow().first().cloned().unwrap_or_default();
        let generation = load
            .strip_prefix(&format!("load_hidden({path},"))
            .and_then(|rest| rest.strip_suffix(')'))
            .and_then(|id| id.parse::<u64>().ok())
            .unwrap_or_else(|| panic!("expected the read to resolve a path, got {load}"));

        let _ = dispatch(
            &mut model,
            &executor,
            &mut follow_ups,
            Msg::HiddenBufferLoaded {
                generation,
                buf: Some(BufferHandle(7)),
                created: true,
                changedtick: 1,
            },
        );
        let _ = dispatch(
            &mut model,
            &executor,
            &mut follow_ups,
            Msg::AiFsReadReply {
                request_id: 3,
                result: Ok("fn main() {}\n".to_string()),
            },
        );

        assert_eq!(
            ops.calls.borrow().as_slice(),
            [
                format!("load_hidden({path},{generation})"),
                "ai_fs_read(3,7,None,None)".to_string(),
                format!("release_hidden({path})"),
            ]
        );
    }

    /// The stand-in above must cover the refused path and nothing else. A
    /// connection that died during a review's bind is a lost engine, and
    /// answering it with a fabricated buffer-less resolve instead would
    /// keep the loop running against a corpse: `Msg::EngineDown` would
    /// never fire, and the user would go on typing into an editor whose
    /// engine is gone.
    #[test]
    fn a_lost_engine_during_a_hidden_buffer_load_still_reads_as_a_lost_engine() {
        let (msg_tx, msg_rx) = mpsc::sync_channel(4);
        let ops = FakeOps::default();
        *ops.fail_next.borrow_mut() = true;
        let executor =
            Executor::new(&ops).with_toast_timer(crate::wake::LoopSender::new(msg_tx.clone()));

        let path = if cfg!(windows) {
            "C:\\work\\main.rs"
        } else {
            "/work/main.rs"
        };
        let flow = executor.run(Effect::Rpc(RpcCall::LoadHidden {
            path: path.to_owned(),
            generation: 9,
        }));

        assert!(
            matches!(flow, Flow::EngineLost),
            "the path was usable, so this Err is the connection dying: {flow:?}"
        );
        let fabricated: Vec<_> = msg_rx
            .try_iter()
            .filter(|msg| matches!(msg, Msg::HiddenBufferLoaded { .. }))
            .collect();
        assert!(
            fabricated.is_empty(),
            "a dead engine must never be answered with a resolve nvim never sent: {fabricated:?}"
        );
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
    fn open_file_effect_maps_to_engine_ops_open_file() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Rpc(RpcCall::OpenFile {
            path: "src/main.rs".into(),
        }));
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(ops.calls.borrow()[0], "open_file(src/main.rs)");
    }

    #[test]
    fn open_file_write_failure_returns_engine_lost() {
        let ops = FakeOps::default();
        *ops.fail_next.borrow_mut() = true;
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Rpc(RpcCall::OpenFile {
            path: "src/main.rs".into(),
        }));
        assert!(matches!(flow, Flow::EngineLost));
    }

    #[test]
    fn preview_buffer_effect_maps_to_engine_ops_preview_buffer() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Rpc(RpcCall::PreviewBuffer {
            path: "src/main.rs".into(),
            generation: 7,
        }));
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(ops.calls.borrow()[0], "preview_buffer(src/main.rs,7)");
    }

    #[test]
    fn preview_buffer_write_failure_returns_engine_lost() {
        let ops = FakeOps::default();
        *ops.fail_next.borrow_mut() = true;
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Rpc(RpcCall::PreviewBuffer {
            path: "src/main.rs".into(),
            generation: 7,
        }));
        assert!(matches!(flow, Flow::EngineLost));
    }

    #[test]
    fn rename_file_effect_maps_to_engine_ops_rename_file() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Rpc(RpcCall::RenameFile {
            old_path: "a.txt".into(),
            new_path: "b.txt".into(),
            generation: 3,
        }));
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(ops.calls.borrow()[0], "rename_file(a.txt,b.txt,3)");
    }

    /// `RpcCall::BufSetText` is matched explicitly in `Executor::run` rather
    /// than falling through the `#[non_exhaustive]` catch-all -- this pins
    /// that the dispatch actually reaches `EngineOps::set_buf_text` (and
    /// with which arguments), so a future refactor that accidentally
    /// deletes the explicit arm regresses back to the silent no-op every
    /// other unmatched call kind gets, instead of losing an accepted buffer
    /// edit unnoticed.
    #[test]
    fn buf_set_text_effect_maps_to_engine_ops_set_buf_text() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Rpc(RpcCall::BufSetText {
            buf: BufferHandle(3),
            edits: vec![TextEdit {
                start_row: 0,
                start_col: 0,
                end_row: 0,
                end_col: 1,
                lines: vec!["x".into()],
            }],
            undojoin: true,
            expected_changedtick: Some(12),
            generation: 4,
        }));
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(ops.calls.borrow()[0], "set_buf_text(3,1,true,Some(12))");
    }

    /// The two agent filesystem calls are matched explicitly for the same
    /// reason `RpcCall::BufSetText` above is, with a sharper consequence:
    /// a silently no-op'd read or write is not a lost edit, it is an agent
    /// blocked forever on a JSON-RPC request nothing else will ever settle.
    #[test]
    fn the_agent_filesystem_effects_map_to_their_engine_ops_calls() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);

        let read = executor.run(Effect::Rpc(RpcCall::AiFsRead {
            request_id: 7,
            buf: BufferHandle(3),
            line: Some(2),
            limit: None,
        }));
        let write = executor.run(Effect::Rpc(RpcCall::AiFsWrite {
            request_id: 8,
            buf: BufferHandle(3),
            lines: vec!["one".into(), "two".into()],
            eol: true,
            expected_changedtick: 12,
        }));

        assert!(matches!(read, Flow::Continue));
        assert!(matches!(write, Flow::Continue));
        assert_eq!(ops.calls.borrow()[0], "ai_fs_read(7,3,Some(2),None)");
        assert_eq!(ops.calls.borrow()[1], "ai_fs_write(8,3,2,true,12)");
    }

    /// The `Checktime` arm is matched explicitly for the same reason the
    /// two above are: a silently no-op'd probe drops an external write the
    /// user never learns about, and a silently no-op'd forced call drops
    /// the reload-and-discard-the-local-edits answer they already gave.
    /// Driven through the real executor, not through `FakeOps` directly --
    /// deleting the arm and letting `RpcCall` fall into the
    /// `#[non_exhaustive]` catch-all is exactly the mutation this exists to
    /// fail on.
    #[test]
    fn the_checktime_effects_map_to_their_engine_ops_calls() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);

        let probe = executor.run(Effect::Rpc(RpcCall::Checktime {
            request_id: 5,
            paths: vec!["a.rs".to_string(), "b.rs".to_string()],
            force: false,
        }));
        let forced = executor.run(Effect::Rpc(RpcCall::Checktime {
            request_id: 6,
            paths: vec!["a.rs".to_string()],
            force: true,
        }));

        assert!(matches!(probe, Flow::Continue));
        assert!(matches!(forced, Flow::Continue));
        assert_eq!(ops.calls.borrow()[0], "checktime(5,a.rs|b.rs,false)");
        assert_eq!(ops.calls.borrow()[1], "checktime(6,a.rs,true)");
    }

    /// A write nvim refuses because the buffer moved is not an engine
    /// failure: the loop carries on and the refusal reaches the review as
    /// `Msg::BufWriteRefused`, which is what puts the hunks it claimed back
    /// on screen as undecided. Reporting `EngineLost` here would tear down
    /// a healthy session over the user typing while a proposal was open.
    #[test]
    fn a_refused_buf_set_text_routes_a_message_and_keeps_the_session() {
        let ops = FakeOps::default();
        *ops.refuse_next_write.borrow_mut() = true;
        let (msg_tx, msg_rx) = mpsc::sync_channel(4);
        let executor = Executor::new(&ops).with_toast_timer(crate::wake::LoopSender::new(msg_tx));

        let flow = executor.run(Effect::Rpc(RpcCall::BufSetText {
            buf: BufferHandle(3),
            edits: vec![TextEdit {
                start_row: 0,
                start_col: 0,
                end_row: 0,
                end_col: 1,
                lines: vec!["x".into()],
            }],
            undojoin: true,
            expected_changedtick: Some(12),
            generation: 4,
        }));

        assert!(matches!(flow, Flow::Continue));
        let msg = msg_rx.try_recv().ok();
        assert!(
            matches!(
                msg,
                Some(Msg::BufWriteRefused {
                    buf: BufferHandle(3),
                    generation: 4,
                })
            ),
            "expected the refusal routed back to the review, got {msg:?}"
        );
    }

    /// Runs `f` on a thread of its own and answers what it produced, failing
    /// the test rather than hanging it if `f` does not return within five
    /// seconds.
    ///
    /// Every property below is about a send that must not wait for room, and
    /// a send that does wait never returns at all -- on the loop thread that
    /// is the editor frozen, and in a test run inline it would be the whole
    /// suite stopped with no failure to read. The receiver stays on this
    /// thread, so the panic that reports the timeout also drops it, which
    /// releases the stuck send instead of leaving a thread wedged behind the
    /// failure.
    fn without_blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let out = f();
            done_tx.send(()).ok();
            out
        });
        done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a write outcome must never wait for room on the channel the loop drains");
        worker.join().unwrap()
    }

    /// The write outcome is produced on the loop thread, which is the
    /// message channel's only consumer: a send that waits for room waits for
    /// a drain only this thread can perform, and the editor is frozen for
    /// good. The channel is at its fullest exactly when this happens -- an
    /// open review holds a buffer subscription, so the keystroke that
    /// accepted a hunk is queueing text-change and redraw traffic of its own
    /// -- so the outcome parks instead, and is delivered once the loop
    /// drains. Bounded rather than merely "does not hang": a blocking send
    /// here never returns at all, so the wait below is what turns that
    /// freeze into a failed test.
    #[test]
    fn a_write_outcome_parks_instead_of_blocking_the_loop_thread() {
        let (msg_tx, msg_rx) = mpsc::sync_channel(1);
        let sender = crate::wake::LoopSender::new(msg_tx);
        sender.try_send(Msg::RedrawReady).unwrap();
        let executor =
            Executor::new(SlowOps::new(std::time::Duration::ZERO)).with_toast_timer(sender);

        let (executor, flow) = without_blocking(move || {
            let flow = executor.run(Effect::Rpc(RpcCall::BufSetText {
                buf: BufferHandle(3),
                edits: vec![TextEdit {
                    start_row: 0,
                    start_col: 0,
                    end_row: 0,
                    end_col: 1,
                    lines: vec!["x".into()],
                }],
                undojoin: true,
                expected_changedtick: Some(12),
                generation: 4,
            }));
            (executor, flow)
        });
        assert!(matches!(flow, Flow::Continue));

        // and it was parked, not dropped: leaving the review with a write
        // it believes is still in flight is what the parking exists to
        // prevent, so the outcome has to arrive once there is room for it
        assert!(matches!(msg_rx.try_recv(), Ok(Msg::RedrawReady)));
        executor.flush_loop_msgs();
        let msg = msg_rx.try_recv().ok();
        assert!(
            matches!(
                msg,
                Some(Msg::BufWriteApplied {
                    buf: BufferHandle(3),
                    generation: 4,
                    ..
                })
            ),
            "expected the parked outcome delivered once the channel drained, got {msg:?}"
        );
    }

    /// Ordering, not just delivery: a second outcome produced while the
    /// first is still parked queues behind it. Sending it straight through
    /// the moment room appears would report the later write to the review
    /// before the earlier one, which reads as a refusal of a write that had
    /// already applied.
    #[test]
    fn a_second_write_outcome_queues_behind_a_parked_one() {
        let (msg_tx, msg_rx) = mpsc::sync_channel(1);
        let sender = crate::wake::LoopSender::new(msg_tx);
        sender.try_send(Msg::RedrawReady).unwrap();
        let executor =
            Executor::new(SlowOps::new(std::time::Duration::ZERO)).with_toast_timer(sender);

        let executor = without_blocking(move || {
            executor.route_loop_msg(Msg::BufWriteApplied {
                buf: BufferHandle(3),
                generation: 4,
                changedtick: 9,
            });
            executor.route_loop_msg(Msg::BufWriteRefused {
                buf: BufferHandle(3),
                generation: 4,
            });
            executor
        });

        assert!(matches!(msg_rx.try_recv(), Ok(Msg::RedrawReady)));
        executor.flush_loop_msgs();
        assert!(matches!(
            msg_rx.try_recv(),
            Ok(Msg::BufWriteApplied {
                generation: 4,
                changedtick: 9,
                ..
            })
        ));
        executor.flush_loop_msgs();
        assert!(matches!(
            msg_rx.try_recv(),
            Ok(Msg::BufWriteRefused { generation: 4, .. })
        ));
    }

    /// The delivery guarantee is the loop's, not the executor's: a parked
    /// outcome reaches the review only because every pass performs its
    /// handoffs before it can paint or sleep. Driven through that step --
    /// the one `run`'s loop calls -- rather than by flushing here, so a
    /// pass that stops carrying write outcomes is a failing test rather
    /// than a review whose every later accept is refused for a race that
    /// never happened.
    #[test]
    fn a_loop_pass_carries_through_a_parked_write_outcome() {
        let (msg_tx, msg_rx) = mpsc::sync_channel(1);
        let sender = crate::wake::LoopSender::new(msg_tx);
        sender.try_send(Msg::RedrawReady).unwrap();
        let executor =
            Executor::new(SlowOps::new(std::time::Duration::ZERO)).with_toast_timer(sender);

        // the write completes with the channel full, which is what an
        // accept does in a buffer whose review is subscribed: the same
        // keystroke has a text-change and a redraw queued ahead of it
        let (executor, flow) = without_blocking(move || {
            let flow = executor.run(Effect::Rpc(RpcCall::BufSetText {
                buf: BufferHandle(3),
                edits: vec![TextEdit {
                    start_row: 0,
                    start_col: 0,
                    end_row: 0,
                    end_col: 1,
                    lines: vec!["x".into()],
                }],
                undojoin: true,
                expected_changedtick: Some(12),
                generation: 4,
            }));
            (executor, flow)
        });
        assert!(matches!(flow, Flow::Continue));
        // and the loop consumes that traffic, as it does on every wakeup
        assert!(matches!(msg_rx.try_recv(), Ok(Msg::RedrawReady)));

        let (_osc52_tx, osc52_rx) = mpsc::channel();
        let mut sink = FakeOsc52Sink::default();
        drain_pass_handoffs(&osc52_rx, &mut sink, &executor);

        let msg = msg_rx.try_recv().ok();
        assert!(
            matches!(
                msg,
                Some(Msg::BufWriteApplied {
                    buf: BufferHandle(3),
                    generation: 4,
                    ..
                })
            ),
            "the pass must carry the parked outcome through, got {msg:?}"
        );
    }

    /// `RpcCall::BufAttach`/`BufDetach` are matched explicitly in
    /// `Executor::run` for the same reason `BufSetText` above is: falling to
    /// the `#[non_exhaustive]` catch-all would silently no-op the request
    /// rather than actually subscribing/unsubscribing, and a caller waiting
    /// on the resulting `Msg::BufTextChanged` stream would simply never see
    /// one, with no error to explain why.
    #[test]
    fn buf_attach_effect_maps_to_engine_ops_buf_attach() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Rpc(RpcCall::BufAttach {
            buf: BufferHandle(5),
            generation: 9,
        }));
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(ops.calls.borrow()[0], "buf_attach(5,9)");
    }

    #[test]
    fn buf_detach_effect_maps_to_engine_ops_buf_detach() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Rpc(RpcCall::BufDetach {
            buf: BufferHandle(5),
        }));
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(ops.calls.borrow()[0], "buf_detach(5)");
    }

    #[test]
    fn rename_file_write_failure_returns_engine_lost() {
        let ops = FakeOps::default();
        *ops.fail_next.borrow_mut() = true;
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::Rpc(RpcCall::RenameFile {
            old_path: "a.txt".into(),
            new_path: "b.txt".into(),
            generation: 3,
        }));
        assert!(matches!(flow, Flow::EngineLost));
    }

    /// A scratch root under `target/tmp`, unique per test process and
    /// nonce, for the tree-effect worker-thread tests below to scan or
    /// git-status for real.
    fn tree_effect_scratch(nonce: &str) -> std::path::PathBuf {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!(
                "runtime-tree-effect-{}-{nonce}",
                std::process::id()
            ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create scratch root");
        root
    }

    /// Proves `Executor::run` actually spawns `view_native::tree::fs::scan`
    /// on a worker thread and reports back over the wired channel -- the
    /// production wiring `FakeOps`-only tests above cannot reach, since
    /// they never install a `toast_timer`.
    #[test]
    fn tree_scan_effect_replies_with_a_real_filesystem_listing() {
        let root = tree_effect_scratch("scan");
        std::fs::write(root.join("a.txt"), "").expect("write a.txt");

        let ops = FakeOps::default();
        let (tx, rx) = mpsc::sync_channel(4);
        let executor = Executor::new(&ops).with_toast_timer(crate::wake::LoopSender::new(tx));
        let flow = executor.run(Effect::TreeScan {
            generation: 9,
            root: root.clone(),
        });
        assert!(matches!(flow, Flow::Continue));

        let msg = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("TreeScanResult arrives from the worker thread");
        match msg {
            Msg::TreeScanResult {
                generation,
                entries,
            } => {
                assert_eq!(generation, 9);
                assert!(
                    entries
                        .iter()
                        .any(|e| e.path == std::path::Path::new("a.txt")),
                    "scan must report the file this test wrote: {entries:?}"
                );
            }
            other => panic!("expected TreeScanResult, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Spawns a real `ai_context_worker` behind `ai` so a runtime-level
    /// Ai-effect test exercises the whole pipeline `Effect::Ai`/
    /// `Effect::AiPromptSubmit` now share: `Executor::run` -> the wired job
    /// channel -> this worker's own thread -> `AiWorker::dispatch`. `SlowOps`
    /// with `delay` stands in for the live engine a `Submit` job would
    /// otherwise read through; `Direct` jobs never touch it at all. Returns
    /// the job sender `Executor::with_ai_context` takes; the worker thread
    /// itself is never joined, the same lifetime the real session's own
    /// `_ai_context_worker` binding has (see `run`'s setup).
    fn spawn_test_ai_context_worker(
        ai: crate::ai_worker::AiWorker,
        delay: std::time::Duration,
    ) -> mpsc::Sender<crate::ai_context_worker::AiContextJob> {
        let route = crate::ai_context_worker::OpsRoute::new(SlowOps::new(delay));
        let (tx, rx) = mpsc::channel();
        let _handle =
            crate::ai_context_worker::spawn(route, ai, rx).expect("spawn ai context worker");
        tx
    }

    /// Proves `Executor::run`'s `Effect::Ai` arm genuinely reaches a wired
    /// [`crate::ai_worker::AiWorker`] through the context worker's shared
    /// queue, not just that the worker itself behaves correctly in
    /// isolation (`ai_worker.rs`'s own suite already covers that): deleting
    /// `queue_ai_job`'s call for this arm, or the arm entirely, leaves
    /// `flow` unaffected (`Effect` is `#[non_exhaustive]` and degrades to a
    /// no-op `Flow::Continue`) but this assertion on `rx` would time out,
    /// since nothing would ever reach the worker to report a crash.
    #[test]
    fn ai_effect_forwards_to_the_wired_worker() {
        let ops = FakeOps::default();
        let (tx, rx) = mpsc::sync_channel(4);
        let ai = crate::ai_worker::AiWorker::new(
            view_ai::AgentSpec::Command(vec![
                "runtime-ai-effect-test-nonexistent-program-xyz".to_string()
            ]),
            std::path::PathBuf::from("."),
            crate::wake::LoopSender::new(tx),
        );
        let job_tx = spawn_test_ai_context_worker(ai, std::time::Duration::ZERO);
        let executor = Executor::new(&ops).with_ai_context(job_tx);

        let flow = executor.run(Effect::Ai(view_core::native::ai_event::AiCommand::Prompt {
            text: "hello".to_string(),
            context: Vec::new(),
        }));
        assert!(matches!(flow, Flow::Continue));

        let msg = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("SessionCrashed arrives from the worker the effect must have queued to");
        assert!(
            matches!(
                &msg,
                Msg::Ai(view_core::native::ai_event::AiEvent::SessionCrashed { message })
                    if message.contains("AI agent failed to start")
            ),
            "expected a spawn-failure SessionCrashed forwarded through the effect, got {msg:?}"
        );
    }

    /// The Cancel-shaped half of `ai_effect_forwards_to_the_wired_worker`:
    /// an `Effect::Ai(AiCommand::Cancel)` reaching an idle worker (`[ai]`
    /// wired but no session ever started) proves the effect really carries
    /// through to `AiWorker::dispatch`'s own I4 handling -- "no active AI
    /// session for this command", never a spawn attempt -- rather than only
    /// exercising the `Prompt` shape the sibling test above already covers.
    #[test]
    fn ai_effect_forwards_a_cancel_to_the_wired_worker_with_no_session_running() {
        let ops = FakeOps::default();
        let (tx, rx) = mpsc::sync_channel(4);
        let ai = crate::ai_worker::AiWorker::new(
            view_ai::AgentSpec::Command(vec![
                "runtime-ai-cancel-effect-test-nonexistent-program-xyz".to_string(),
            ]),
            std::path::PathBuf::from("."),
            crate::wake::LoopSender::new(tx),
        );
        let job_tx = spawn_test_ai_context_worker(ai, std::time::Duration::ZERO);
        let executor = Executor::new(&ops).with_ai_context(job_tx);

        let flow = executor.run(Effect::Ai(view_core::native::ai_event::AiCommand::Cancel));
        assert!(matches!(flow, Flow::Continue));

        let msg = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a Cancel with nothing running must still report something visible");
        assert!(
            matches!(
                &msg,
                Msg::Ai(view_core::native::ai_event::AiEvent::SessionCrashed { message })
                    if message == "no active AI session for this command"
            ),
            "expected the idle-Cancel SessionCrashed forwarded through the effect, got {msg:?}"
        );
    }

    /// A `Cancel` (`Effect::Ai`) issued the instant after a `Submit`
    /// (`Effect::AiPromptSubmit`) must never overtake it: both now funnel
    /// through the one FIFO queue `AiContextJob` describes, so the worker
    /// drains the prompt first regardless of how slow its context reads
    /// are. `SlowOps` delayed 200ms stands in for a live engine's reads,
    /// giving a reverted, direct-dispatch `Effect::Ai` (bypassing the
    /// queue entirely, back on the calling thread) an unmissable head
    /// start -- the shape the mutation check below exercises.
    ///
    /// A direct-dispatch `Effect::Ai` reaches the worker's still-`Idle`
    /// slot before the queued Submit's reads resolve, reporting the
    /// spurious "no active AI session for this command" crash and leaving
    /// the Submit to spawn a session for a turn already cancelled. Queued
    /// through the shared channel instead, the ONLY crash reported is the
    /// prompt's own genuine spawn failure (a nonexistent program), and the
    /// buffered Cancel is dropped harmlessly along with it (see
    /// `AiWorker::spawn_in_background`'s own doc on why a failed spawn
    /// drops what it buffered).
    #[test]
    fn a_cancel_right_after_a_prompt_submit_never_overtakes_it_as_a_spurious_crash() {
        let ops = FakeOps::default();
        let (msg_tx, msg_rx) = mpsc::sync_channel(4);
        let ai = crate::ai_worker::AiWorker::new(
            view_ai::AgentSpec::Command(vec![
                "runtime-ai-fifo-test-nonexistent-program-xyz".to_string()
            ]),
            std::path::PathBuf::from("."),
            crate::wake::LoopSender::new(msg_tx),
        );
        // wired alongside `ai_context`, matching production
        // (`LoopChannels::executor` wires both from the same clone): a
        // mutation reverting `Effect::Ai` to dispatch straight to `self.ai`
        // must find a live worker here to race against the queued Submit,
        // not silently no-op on an unwired field.
        let job_tx =
            spawn_test_ai_context_worker(ai.clone(), std::time::Duration::from_millis(200));
        let executor = Executor::new(&ops).with_ai(ai).with_ai_context(job_tx);

        let flow1 = executor.run(Effect::AiPromptSubmit {
            text: "hello".to_string(),
        });
        let flow2 = executor.run(Effect::Ai(view_core::native::ai_event::AiCommand::Cancel));
        assert!(matches!(flow1, Flow::Continue));
        assert!(matches!(flow2, Flow::Continue));

        let first = msg_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the submitted prompt's spawn failure must still report a crash");
        assert!(
            matches!(
                &first,
                Msg::Ai(view_core::native::ai_event::AiEvent::SessionCrashed { message })
                    if message.contains("AI agent failed to start")
            ),
            "the FIRST crash reported must be the prompt's own genuine spawn failure, \
             never the Cancel's spurious \"no active AI session\" -- got {first:?}"
        );
        assert!(
            msg_rx.try_recv().is_err(),
            "the buffered Cancel must be dropped along with the failed spawn's pending \
             commands, never report a second, spurious crash of its own"
        );
    }

    /// Proves `Executor::run`'s `Effect::AiPromptSubmit` arm genuinely
    /// forwards to the wired context worker's job channel, carrying the
    /// text through unchanged -- `update()`'s own `<CR>` arm never carries
    /// context itself (see `Effect::AiPromptSubmit`'s doc for why that
    /// assembly cannot happen in `view-core`), so this is the one place
    /// that hand-off from the pure model to the executor is provable
    /// without a live engine.
    #[test]
    fn ai_prompt_submit_effect_forwards_to_the_wired_context_worker() {
        let ops = FakeOps::default();
        let (tx, rx) = mpsc::channel();
        let executor = Executor::new(&ops).with_ai_context(tx);

        let flow = executor.run(Effect::AiPromptSubmit {
            text: "hello".to_string(),
        });
        assert!(matches!(flow, Flow::Continue));

        let job = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the job must reach the wired context worker channel");
        match job {
            crate::ai_context_worker::AiContextJob::Submit { text } => {
                assert_eq!(text, "hello");
            }
            crate::ai_context_worker::AiContextJob::Direct(_) => {
                panic!("expected an AiContextJob::Submit, got a Direct job instead")
            }
        }
    }

    /// I4: `Effect::AiPromptSubmit` must return well before the context
    /// worker's four reads resolve, since those reads run on the worker's
    /// own thread, never this one -- `SlowOps` blocked for 2s on every read
    /// is the falsifiable disconfirm, the same "slow resolver" shape
    /// `ai_worker.rs`'s own `dispatch_returns_before_a_genuinely_slow_resolver_finishes`
    /// uses to pin `AiWorker::dispatch`'s off-thread spawn. `SlowOps` with
    /// the same 2s delay backs both the executor's own `ops` (what an
    /// inlined mutation would read through) and the context worker's route
    /// (what the correct, queued path reads through), so a mutation that
    /// inlined the reads into this arm (calling `build_prompt` directly on
    /// the loop thread instead of queuing the job) hits the same 2s delay
    /// and fails this assertion -- reusing `FakeOps` for `ops` here would
    /// let such a mutation's inline read return instantly and pass anyway.
    #[test]
    fn ai_prompt_submit_effect_returns_well_before_the_context_workers_slow_reads_finish() {
        let ops = SlowOps::new(std::time::Duration::from_secs(2));
        let (msg_tx, _msg_rx) = mpsc::sync_channel(4);
        let ai = crate::ai_worker::AiWorker::new(
            view_ai::AgentSpec::Command(vec![
                "runtime-ai-latency-test-nonexistent-program-xyz".to_string()
            ]),
            std::path::PathBuf::from("."),
            crate::wake::LoopSender::new(msg_tx),
        );
        let job_tx = spawn_test_ai_context_worker(ai.clone(), std::time::Duration::from_secs(2));
        let executor = Executor::new(&ops).with_ai(ai).with_ai_context(job_tx);

        let started = std::time::Instant::now();
        let flow = executor.run(Effect::AiPromptSubmit {
            text: "hello".to_string(),
        });
        let elapsed = started.elapsed();

        assert!(matches!(flow, Flow::Continue));
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "Effect::AiPromptSubmit must return well before the 2s-blocked reads finish, \
             took {elapsed:?}"
        );
    }

    /// I7, path one: with no context worker wired at all, `Effect::Ai`'s
    /// failed queue attempt degrades through the same local-error path a
    /// genuine session crash reports through -- and that synthesized
    /// message, fed through `update()` exactly as the real loop would, must
    /// clear `turn_in_flight`, never leaving `<C-c>` as the only way out of
    /// a wedge nothing is actually running for.
    #[test]
    fn ai_effect_with_no_context_worker_wired_surfaces_a_local_error_that_clears_turn_in_flight() {
        let ops = FakeOps::default();
        let (toast_tx, toast_rx) = mpsc::sync_channel(4);
        let executor = Executor::new(&ops).with_toast_timer(crate::wake::LoopSender::new(toast_tx));

        let flow = executor.run(Effect::Ai(view_core::native::ai_event::AiCommand::Cancel));
        assert!(matches!(flow, Flow::Continue));

        let msg = toast_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("an unwired context worker must still surface a local error");
        assert!(
            matches!(
                &msg,
                Msg::Ai(view_core::native::ai_event::AiEvent::SessionCrashed { .. })
            ),
            "expected a synthesized SessionCrashed, got {msg:?}"
        );

        let mut model = Model::with_term_size(80, 24);
        model.ai_panel_mut().turn_in_flight = true;
        let _ = update(&mut model, msg);
        assert!(
            !model.ai_panel().turn_in_flight,
            "the synthesized local error must clear turn_in_flight through update(), \
             the same as any other SessionCrashed"
        );
    }

    /// I7, path two: `ai_context` IS wired, but the worker thread is
    /// already gone (its receiver dropped) -- a real, if rare, shutdown
    /// race, not merely "never configured." The send itself fails, and
    /// must degrade exactly the same way the unwired case above does.
    #[test]
    fn ai_prompt_submit_effect_with_a_dead_context_worker_surfaces_a_local_error_that_clears_turn_in_flight(
    ) {
        let ops = FakeOps::default();
        let (toast_tx, toast_rx) = mpsc::sync_channel(4);
        let (job_tx, job_rx) = mpsc::channel();
        drop(job_rx);
        let executor = Executor::new(&ops)
            .with_toast_timer(crate::wake::LoopSender::new(toast_tx))
            .with_ai_context(job_tx);

        let flow = executor.run(Effect::AiPromptSubmit {
            text: "hello".to_string(),
        });
        assert!(matches!(flow, Flow::Continue));

        let msg = toast_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a dead context worker's failed send must still surface a local error");
        assert!(
            matches!(
                &msg,
                Msg::Ai(view_core::native::ai_event::AiEvent::SessionCrashed { .. })
            ),
            "expected a synthesized SessionCrashed, got {msg:?}"
        );

        let mut model = Model::with_term_size(80, 24);
        model.ai_panel_mut().turn_in_flight = true;
        let _ = update(&mut model, msg);
        assert!(
            !model.ai_panel().turn_in_flight,
            "the synthesized local error must clear turn_in_flight through update()"
        );
    }

    /// The unwired-channel degrade every other effect in this type
    /// documents for itself: with no context worker wired (every bare
    /// `FakeOps`-only `Executor::new`, the shape every test above this one
    /// uses), `Effect::AiPromptSubmit` is a silent no-op rather than a
    /// panic -- there is nothing to forward the job to, and the loop
    /// must keep running regardless.
    #[test]
    fn ai_prompt_submit_effect_with_no_worker_wired_is_a_silent_no_op() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);

        let flow = executor.run(Effect::AiPromptSubmit {
            text: "hello".to_string(),
        });

        assert!(matches!(flow, Flow::Continue));
    }

    /// Same proof as `tree_scan_effect_replies_with_a_real_filesystem_listing`,
    /// for `Effect::PickerPreviewFallback`'s worker: `Msg::PickerPreviewReply`
    /// already told the picker nvim has no buffer open for the path, so this
    /// is the plain `std::fs` read that fills in the preview pane instead --
    /// the production wiring `FakeOps`-only tests above cannot reach, since
    /// they never install a `toast_timer`.
    #[test]
    fn picker_preview_fallback_effect_replies_with_a_real_file_read() {
        let root = tree_effect_scratch("preview-fallback");
        let path = root.join("target.txt");
        std::fs::write(&path, "line one\nline two").expect("write target.txt");

        let ops = FakeOps::default();
        let (tx, rx) = mpsc::sync_channel(4);
        let executor = Executor::new(&ops).with_toast_timer(crate::wake::LoopSender::new(tx));
        let flow = executor.run(Effect::PickerPreviewFallback {
            generation: 4,
            path: path.to_string_lossy().into_owned(),
        });
        assert!(matches!(flow, Flow::Continue));

        let msg = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("PickerPreviewFile arrives from the worker thread");
        match msg {
            Msg::PickerPreviewFile { generation, lines } => {
                assert_eq!(generation, 4);
                assert_eq!(
                    lines,
                    Some(vec!["line one".to_string(), "line two".to_string()]),
                    "the fallback must report the file this test wrote"
                );
            }
            other => panic!("expected PickerPreviewFile, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Same proof as `tree_scan_effect_replies_with_a_real_filesystem_listing`,
    /// for the `git status --porcelain=v2` worker instead.
    #[test]
    fn tree_git_scan_effect_replies_with_a_real_git_status() {
        let root = tree_effect_scratch("git-scan");
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .current_dir(&root)
                .args(args)
                .status()
                .expect("git is on PATH for this test's own setup");
            assert!(status.success(), "git {args:?} failed in {root:?}");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "test"]);
        std::fs::write(root.join("a.txt"), "one\n").expect("write a.txt");
        git(&["add", "a.txt"]);
        git(&["commit", "-q", "-m", "init"]);
        std::fs::write(root.join("a.txt"), "two\n").expect("modify a.txt");

        let ops = FakeOps::default();
        let (tx, rx) = mpsc::sync_channel(4);
        let executor = Executor::new(&ops).with_toast_timer(crate::wake::LoopSender::new(tx));
        let flow = executor.run(Effect::TreeGitScan {
            generation: 5,
            root: root.clone(),
        });
        assert!(matches!(flow, Flow::Continue));

        let msg = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("TreeGitResult arrives from the worker thread");
        match msg {
            Msg::TreeGitResult {
                generation,
                status,
                timed_out,
            } => {
                assert_eq!(generation, 5);
                assert!(
                    status
                        .iter()
                        .any(|e| e.path == std::path::Path::new("a.txt")),
                    "git status must report the file this test modified: {status:?}"
                );
                assert!(
                    !timed_out,
                    "a real, fast git status must not report a timeout"
                );
            }
            other => panic!("expected TreeGitResult, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A `toast_timer`-less `Executor` (every bare `FakeOps`-only test
    /// above it) dropping `TreeGitScan` cannot merely no-op the way
    /// `ScheduleToastExpiry`/`Osc52Copy` safely do: `TreeState::apply_git`
    /// is the only clearer of `git_refresh_in_flight`, already set `true`
    /// by the `request_git_refresh` call that produced this effect, so a
    /// dropped reply here is a permanent wedge for the rest of the tree's
    /// session, not a cosmetic delay. This pins that the debug-build guard
    /// actually fires rather than silently reintroducing the wedge.
    #[test]
    #[should_panic(expected = "git_refresh_in_flight can never clear")]
    fn a_tree_git_scan_without_a_toast_timer_fails_loud_in_debug_builds() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let _ = executor.run(Effect::TreeGitScan {
            generation: 1,
            root: std::path::PathBuf::from("."),
        });
    }

    /// Proves `Executor::run` actually spawns the create worker and reports
    /// `Msg::TreeCreateFileResult` back over the wired channel with `ok:
    /// true`, and that the file it created is genuinely empty.
    #[test]
    fn tree_create_file_effect_writes_an_empty_file_and_reports_ok() {
        let root = tree_effect_scratch("create");
        let path = root.join("new.txt");

        let ops = FakeOps::default();
        let (tx, rx) = mpsc::sync_channel(4);
        let executor = Executor::new(&ops).with_toast_timer(crate::wake::LoopSender::new(tx));
        let flow = executor.run(Effect::TreeCreateFile {
            path: path.clone(),
            generation: 11,
        });
        assert!(matches!(flow, Flow::Continue));

        let msg = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("TreeCreateFileResult arrives from the worker thread");
        assert!(
            matches!(
                msg,
                Msg::TreeCreateFileResult {
                    generation: 11,
                    ok: true
                }
            ),
            "expected TreeCreateFileResult{{generation: 11, ok: true}}, got {msg:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read the created file"),
            ""
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `create_new` must refuse to overwrite a destination that already
    /// holds content, leaving that content untouched and reporting the
    /// refusal rather than truncating it the way a plain `std::fs::write`
    /// would. Dropping `create_new` back to a truncating write is what
    /// this test exists to catch -- the assertion on `existing.txt`'s
    /// content is what fails then, since the create would otherwise wipe
    /// it silently.
    #[test]
    fn tree_create_file_effect_refuses_to_overwrite_an_existing_file() {
        let root = tree_effect_scratch("create-refuse");
        let path = root.join("existing.txt");
        std::fs::write(&path, "keep me\n").expect("write existing.txt");

        let ops = FakeOps::default();
        let (tx, rx) = mpsc::sync_channel(4);
        let executor = Executor::new(&ops).with_toast_timer(crate::wake::LoopSender::new(tx));
        let flow = executor.run(Effect::TreeCreateFile {
            path: path.clone(),
            generation: 12,
        });
        assert!(matches!(flow, Flow::Continue));

        let msg = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("TreeCreateFileResult arrives from the worker thread");
        assert!(
            matches!(
                msg,
                Msg::TreeCreateFileResult {
                    generation: 12,
                    ok: false
                }
            ),
            "expected TreeCreateFileResult{{generation: 12, ok: false}}, got {msg:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read existing.txt"),
            "keep me\n",
            "a refused create must leave the existing file's content exactly as it was"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Symmetric to `tree_create_file_effect_writes_an_empty_file_and_reports_ok`.
    #[test]
    fn tree_delete_file_effect_removes_the_file_and_reports_ok() {
        let root = tree_effect_scratch("delete");
        let path = root.join("gone.txt");
        std::fs::write(&path, "bye\n").expect("write gone.txt");

        let ops = FakeOps::default();
        let (tx, rx) = mpsc::sync_channel(4);
        let executor = Executor::new(&ops).with_toast_timer(crate::wake::LoopSender::new(tx));
        let flow = executor.run(Effect::TreeDeleteFile {
            path: path.clone(),
            generation: 13,
        });
        assert!(matches!(flow, Flow::Continue));

        let msg = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("TreeDeleteFileResult arrives from the worker thread");
        assert!(
            matches!(
                msg,
                Msg::TreeDeleteFileResult {
                    generation: 13,
                    ok: true
                }
            ),
            "expected TreeDeleteFileResult{{generation: 13, ok: true}}, got {msg:?}"
        );
        assert!(!path.exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Restores `XDG_STATE_HOME` to whatever [`with_scratch_state_dir`]
    /// found before it redirected it, on every exit from that function's
    /// closure -- including a panicking assertion, the exact moment a
    /// RED-phase test fails. A straight-line restore after `f()` returns is
    /// skipped entirely by an unwind, leaving the redirection (and the
    /// released mutex, since its guard drops too) in place for every later
    /// test in the same process; a `Drop` impl runs regardless. The same
    /// panic-safety `view-ai::trust`'s own copy of this guard gives its
    /// suite, duplicated rather than shared: `view-core` cannot depend on
    /// `view-ai` and this is test-only code in the bin crate, not a fit for
    /// either crate's own public surface.
    struct EnvRestoreGuard {
        prev: Option<String>,
    }

    impl Drop for EnvRestoreGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("XDG_STATE_HOME", v),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
        }
    }

    /// Redirects `XDG_STATE_HOME` to a nonce-tagged scratch directory under
    /// the guarded lock for the duration of `f` -- the same isolation
    /// `view-ai::trust`'s own suite gives `TrustStore::load`, needed here so
    /// this test never touches the real user's trust store.
    fn with_scratch_state_dir<R>(nonce: &str, f: impl FnOnce() -> R) -> R {
        let _guard = env_mutation_guard();
        let _restore = EnvRestoreGuard {
            prev: std::env::var("XDG_STATE_HOME").ok(),
        };
        let dir = tree_effect_scratch(&format!("ai-trust-state-{nonce}"));
        std::env::set_var("XDG_STATE_HOME", &dir);
        let result = f();
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    /// Proves `Executor::run` is the one caller of
    /// `view_ai::TrustStore::set_trusted` in production: the write actually
    /// lands on disk (a freshly reloaded `TrustStore` sees it), and the
    /// affirmative answer round-trips back as `Msg::AiTrustResolved{trusted:
    /// true}` over the wired channel.
    #[test]
    fn ai_trust_set_effect_persists_and_reports_resolved() {
        with_scratch_state_dir("persist", || {
            let root = tree_effect_scratch("ai-trust-project");

            let ops = FakeOps::default();
            let (tx, rx) = mpsc::sync_channel(4);
            let executor = Executor::new(&ops).with_toast_timer(crate::wake::LoopSender::new(tx));
            let flow = executor.run(Effect::AiTrustSet {
                project_root: root.clone(),
                trusted: true,
                verb: String::new(),
            });
            assert!(matches!(flow, Flow::Continue));

            let msg = rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("AiTrustResolved arrives from the worker thread");
            assert!(
                matches!(msg, Msg::AiTrustResolved { trusted: true, .. }),
                "expected AiTrustResolved{{trusted: true}}, got {msg:?}"
            );

            let reloaded = view_ai::TrustStore::load().expect("reload the trust store");
            assert!(
                reloaded.is_trusted(&root),
                "the executor's write must be visible to a freshly reloaded store"
            );

            let _ = std::fs::remove_dir_all(&root);
        });
    }

    /// Symmetric to the affirmative case: a decline round-trips back as
    /// `Msg::AiTrustResolved{trusted: false}` and leaves the project
    /// unrecorded (see `TrustStore::set_trusted`'s own doc on why a decline
    /// is not durable).
    #[test]
    fn ai_trust_set_effect_declined_reports_resolved_false() {
        with_scratch_state_dir("decline", || {
            let root = tree_effect_scratch("ai-trust-project-declined");

            let ops = FakeOps::default();
            let (tx, rx) = mpsc::sync_channel(4);
            let executor = Executor::new(&ops).with_toast_timer(crate::wake::LoopSender::new(tx));
            let flow = executor.run(Effect::AiTrustSet {
                project_root: root.clone(),
                trusted: false,
                verb: String::new(),
            });
            assert!(matches!(flow, Flow::Continue));

            let msg = rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("AiTrustResolved arrives from the worker thread");
            assert!(
                matches!(msg, Msg::AiTrustResolved { trusted: false, .. }),
                "expected AiTrustResolved{{trusted: false}}, got {msg:?}"
            );

            let reloaded = view_ai::TrustStore::load().expect("reload the trust store");
            assert!(!reloaded.is_trusted(&root));

            let _ = std::fs::remove_dir_all(&root);
        });
    }

    /// The boundary proof this suite actually needs, driven through
    /// `dispatch` -- the real production entry point -- rather than
    /// `Executor::run` in isolation: an executor with no `toast_timer`
    /// wired (every bare `FakeOps`-only `Executor::new`, the shape every
    /// `view-core`-only test builds) neither persists the answer nor drops
    /// it silently.
    /// `dispatch` folds the degrade straight back through `update()` as
    /// `Msg::AiTrustResolved{trusted: false}`, the same notice an explicit
    /// decline produces, so the gate visibly reopens instead of going
    /// quiet. A version of this driving `Executor::run` alone only proves
    /// that code which never runs writes nothing, true of any code and
    /// equally true with the `AiTrustSet` arm deleted outright.
    #[test]
    fn an_unwired_toast_timer_self_announces_the_degrade_instead_of_persisting() {
        with_scratch_state_dir("unwired", || {
            let root = tree_effect_scratch("ai-trust-project-unwired");

            let ops = FakeOps::default();
            let executor = Executor::new(&ops);
            let mut model = Model::with_term_size(80, 24);
            model.cwd = root.clone();
            let mut native = NativeSession::inert();
            let mut bridge = ThemeBridge::new(None);
            let mut follow_ups = FollowUps {
                native: &mut native,
                theme: &mut bridge,
                speculate: crate::speculate::SpeculationClock::default(),
            };

            let _ = dispatch(
                &mut model,
                &executor,
                &mut follow_ups,
                Msg::FeatureInvoke {
                    feature: "ai".to_string(),
                    verb: String::new(),
                },
            );
            let flow = dispatch(
                &mut model,
                &executor,
                &mut follow_ups,
                Msg::Key(view_core::msg::Key {
                    notation: "y".to_string(),
                }),
            );
            assert!(matches!(flow, Flow::Continue));

            assert!(
                !model.ai_trusted,
                "an unwired executor must never leave the model believing trust was granted"
            );
            let entry = model
                .engine
                .messages
                .entries
                .last()
                .expect("the degrade must self-announce a notice");
            let text: String = entry.content.iter().map(|(_, t)| t.as_str()).collect();
            assert!(
                text.contains(":View ai"),
                "the notice must name the way back in, got {text:?}"
            );

            let reloaded = view_ai::TrustStore::load().expect("reload the trust store");
            assert!(
                !reloaded.is_trusted(&root),
                "an unwired executor must not have persisted anything"
            );

            let _ = std::fs::remove_dir_all(&root);
        });
    }

    /// Closing the tree while a scan of a huge directory is still
    /// walking must flip the cancel flag `Effect::TreeScan` installed,
    /// stopping the walk short of the full tree -- the same proof
    /// `tree::fs::scan`'s own `a_cancelled_scan_stops_short_of_the_full_tree`
    /// makes at the walk layer, but exercised here through the executor's
    /// actual `Effect` wiring rather than a bare `AtomicBool` the walk is
    /// handed directly.
    #[test]
    fn tree_close_cancels_an_in_flight_scan_before_it_finishes() {
        let root = tree_effect_scratch("scan-cancel");
        let dirs = 200;
        let files_per_dir = 100;
        for d in 0..dirs {
            let dir = root.join(format!("d{d}"));
            std::fs::create_dir_all(&dir).expect("mkdir");
            for f in 0..files_per_dir {
                std::fs::write(dir.join(format!("f{f}.txt")), "").expect("write file");
            }
        }
        let total = dirs * (files_per_dir + 1);

        let ops = FakeOps::default();
        let (tx, rx) = mpsc::sync_channel(4);
        let executor = Executor::new(&ops).with_toast_timer(crate::wake::LoopSender::new(tx));
        let flow = executor.run(Effect::TreeScan {
            generation: 21,
            root: root.clone(),
        });
        assert!(matches!(flow, Flow::Continue));
        // no sleep, on the same deterministic terms
        // `a_cancelled_scan_stops_short_of_the_full_tree` documents: the
        // walk's very next per-entry check observes whichever of the spawn
        // or this close ran first
        let flow = executor.run(Effect::TreeClose);
        assert!(matches!(flow, Flow::Continue));

        let msg = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("TreeScanResult arrives from the worker thread");
        match msg {
            Msg::TreeScanResult {
                generation,
                entries,
            } => {
                assert_eq!(generation, 21);
                assert!(
                    entries.len() < total,
                    "a scan cancelled by TreeClose must stop short of the full tree \
                     ({} of {total} entries)",
                    entries.len()
                );
            }
            other => panic!("expected TreeScanResult, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
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
        assert!(matches!(
            job.kind,
            crate::clipboard::ClipboardJobKind::Read {
                token: ReplyToken { msgid: 4 },
                register: '*'
            }
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
        let crate::clipboard::ClipboardJobKind::Write {
            token,
            register,
            lines,
            regtype,
        } = job.kind
        else {
            unreachable!("expected a Write job kind");
        };
        assert_eq!(token.msgid, 7);
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

    /// The one effect with no token and no permission to stay silent: nvim
    /// is inside `vim.wait` for this answer and nothing else will ever
    /// unblock it, so an executor with no worker to ask must answer the
    /// empty payload itself rather than degrade the way `Osc52Copy` does.
    #[test]
    fn a_clipboard_query_with_no_worker_wired_answers_empty_inline() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let flow = executor.run(Effect::ClipboardQuery { register: '+' });
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(
            ops.calls.borrow()[0],
            format!("ui_term_event({:?})", "\x1b]52;c;\x1b\\")
        );
    }

    /// The same obligation reached through the other failure: the worker
    /// thread has exited, so the send fails after the channel accepted the
    /// wiring.
    #[test]
    fn a_clipboard_query_with_a_dead_worker_still_answers_exactly_once() {
        let ops = FakeOps::default();
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let executor = Executor::new(&ops).with_clipboard(tx);
        let flow = executor.run(Effect::ClipboardQuery { register: '*' });
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(ops.calls.borrow().len(), 1, "must answer exactly once");
        assert_eq!(
            ops.calls.borrow()[0],
            format!("ui_term_event({:?})", "\x1b]52;p;\x1b\\")
        );
    }

    #[test]
    fn a_clipboard_query_with_a_worker_wired_forwards_the_job_and_answers_nothing_itself() {
        let ops = FakeOps::default();
        let (tx, rx) = mpsc::channel();
        let executor = Executor::new(&ops).with_clipboard(tx);
        let flow = executor.run(Effect::ClipboardQuery { register: '+' });
        assert!(matches!(flow, Flow::Continue));
        assert!(
            ops.calls.borrow().is_empty(),
            "a worker-wired query must not answer twice"
        );
        let job = rx.try_recv().expect("the query job must be forwarded");
        assert!(matches!(
            job.kind,
            crate::clipboard::ClipboardJobKind::Query { register: '+' }
        ));
    }

    /// The mirror of a copy nvim's own provider performed. Nothing waits on
    /// it, so an unwired worker is a silent no-op -- the cost lands on the
    /// next `ClipboardQuery`, not here.
    #[test]
    fn a_clipboard_store_forwards_its_text_and_degrades_silently_unwired() {
        let ops = FakeOps::default();
        let (tx, rx) = mpsc::channel();
        let executor = Executor::new(&ops).with_clipboard(tx);
        let flow = executor.run(Effect::ClipboardStore {
            register: '+',
            text: "yanked\n".to_owned(),
        });
        assert!(matches!(flow, Flow::Continue));
        let job = rx.try_recv().expect("the store job must be forwarded");
        let crate::clipboard::ClipboardJobKind::Store { register, text } = job.kind else {
            unreachable!("expected a Store job kind");
        };
        assert_eq!(register, '+');
        assert_eq!(text, "yanked\n");

        let bare = Executor::new(&ops);
        let flow = bare.run(Effect::ClipboardStore {
            register: '+',
            text: "yanked\n".to_owned(),
        });
        assert!(matches!(flow, Flow::Continue));
        assert!(ops.calls.borrow().is_empty());
    }

    /// `Osc52Copy` carries no `ReplyToken` (see the effect's own doc): an
    /// unwired channel is an ordinary fire-and-forget no-op, unlike three
    /// of the four clipboard effects above, which owe an answer regardless
    /// (`ClipboardStore` is the one that degrades the same way this does).
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
        let Osc52Job::Copy {
            register,
            lines,
            regtype,
        } = job
        else {
            panic!("an Osc52Copy effect must queue an encoding job, not a passthrough one");
        };
        assert_eq!(register, '*');
        assert_eq!(lines, vec!["a", "b"]);
        assert_eq!(regtype, RegisterType::Linewise);
    }

    /// The passthrough twin of the job forwarding above: nvim's own escape
    /// must reach the same channel, unaltered, or the clipboard write a
    /// user's `g:clipboard` provider performs never reaches the terminal.
    #[test]
    fn term_write_forwards_the_bytes_verbatim_on_the_osc52_channel() {
        let ops = FakeOps::default();
        let (tx, rx) = mpsc::channel();
        let executor = Executor::new(&ops).with_osc52(tx);
        const ESCAPE: &[u8] = b"\x1b]52;c;aGk=\x1b\\";
        let flow = executor.run(Effect::TermWrite {
            bytes: ESCAPE.to_vec(),
        });
        assert!(matches!(flow, Flow::Continue));
        let job = rx
            .try_recv()
            .expect("the passthrough job must be forwarded");
        let Osc52Job::Passthrough(bytes) = job else {
            panic!("a TermWrite effect must queue a passthrough job");
        };
        assert_eq!(bytes, ESCAPE);
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
            speculate: crate::speculate::SpeculationClock::default(),
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

    /// One `Msg::CheckTimeReply` naming `path` as unreadable.
    fn gone(request_id: u64, path: &str) -> Msg {
        Msg::CheckTimeReply {
            request_id,
            results: vec![(
                std::path::PathBuf::from(path),
                view_core::msg::CheckTimeOutcome::FileGone { modified: false },
            )],
        }
    }

    /// The frame `run` would put on the terminal for `msg`, or `None` when
    /// nothing asked for one.
    ///
    /// The gate is the loop's own (`run`'s `if model.dirty`), and the
    /// distinction it draws is the whole point: a reply off the RPC pump
    /// has no keystroke behind it, so a change the fold made without asking
    /// for a repaint sits in the model, unseen, until unrelated input
    /// happens along.
    fn painted(
        model: &mut Model,
        executor: &Executor<&FakeOps>,
        follow_ups: &mut FollowUps<'_>,
        msg: Msg,
    ) -> Option<view_surface::Surface> {
        model.dirty = false;
        let _ = dispatch(model, executor, follow_ups, msg);
        model.dirty.then(|| view_surface::render(model))
    }

    /// The shell's own half of the confirmation, driven without waiting out
    /// the real grace: the second look is asked for, and the reply that look
    /// -- and no other -- is allowed to announce is handed back. Its
    /// `request_id` is read off the engine call the fold actually made,
    /// since that is what the fold recorded as the one answer it will
    /// believe.
    fn second_look(
        model: &mut Model,
        executor: &Executor<&FakeOps>,
        follow_ups: &mut FollowUps<'_>,
        ops: &FakeOps,
        path: &str,
    ) -> Msg {
        let _ = dispatch(
            model,
            executor,
            follow_ups,
            Msg::ConfirmExternalRemoval {
                path: std::path::PathBuf::from(path),
            },
        );
        let call = ops
            .calls
            .borrow()
            .last()
            .cloned()
            .expect("the second look has to reach the engine");
        let id = call
            .strip_prefix("checktime(")
            .and_then(|rest| rest.split(',').next())
            .and_then(|id| id.parse::<u64>().ok())
            .unwrap_or_else(|| panic!("expected a checktime call, got {call}"));
        gone(id, path)
    }

    /// The confirming re-probe has to actually be carried out, and not
    /// before the save it exists to wait out could have finished. The fold
    /// stays silent on a first `gone` answer, so an effect that reaches no
    /// arm here (they degrade to no-ops by design, see `run`'s tail) turns
    /// a file an agent deleted into permanent silence rather than a notice
    /// a fraction of a second late -- and one carried out at once answers
    /// from inside an ordinary unlink-then-rewrite save, which is the flash
    /// the whole confirmation exists to remove.
    ///
    /// The floor is an absolute duration, deliberately not a fraction of
    /// the constant under test: an assertion written against
    /// `FILE_GONE_GRACE` itself holds for every value that constant can
    /// take, zero included, and zero is exactly the regression. 60ms is one
    /// coalesce window and change -- above the window a save's two halves
    /// can straddle, below the real grace.
    #[test]
    fn the_re_probe_waits_out_the_save_before_looking_again() {
        let ops = FakeOps::default();
        let (msg_tx, msg_rx) = std::sync::mpsc::sync_channel(8);
        let executor = Executor::new(&ops).with_toast_timer(crate::wake::LoopSender::new(msg_tx));

        let flow = executor.run(Effect::ReprobeExternalWrite {
            path: std::path::PathBuf::from("/proj/src/lib.rs"),
        });

        assert!(matches!(flow, Flow::Continue));
        assert!(
            msg_rx
                .recv_timeout(std::time::Duration::from_millis(60))
                .is_err(),
            "re-asking this soon answers from inside the save it is waiting out"
        );
        match msg_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the grace has to end in a second look, not in silence")
        {
            Msg::ConfirmExternalRemoval { path } => assert_eq!(
                path,
                std::path::PathBuf::from("/proj/src/lib.rs"),
                "the path looked at again is the one that answered gone"
            ),
            other => panic!("expected the path to be looked at again, got {other:?}"),
        }
    }

    /// A second look that could not be scheduled is a removal that is never
    /// announced, so it says so rather than going quiet -- the same degrade
    /// `Effect::AiTrustSet` takes for the same unwired channel, through the
    /// notice `Msg::ExternalWatchDegraded` already exists to raise.
    #[test]
    fn a_second_look_that_cannot_be_scheduled_is_reported_rather_than_swallowed() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let mut model = Model::with_term_size(80, 24);
        let mut native = NativeSession::inert();
        let mut bridge = ThemeBridge::new(None);
        let mut follow_ups = FollowUps {
            native: &mut native,
            theme: &mut bridge,
            speculate: crate::speculate::SpeculationClock::default(),
        };

        let flow = dispatch(
            &mut model,
            &executor,
            &mut follow_ups,
            gone(1, "/proj/src/lib.rs"),
        );
        assert!(matches!(flow, Flow::Continue));
        let entry = model
            .engine
            .messages
            .entries
            .last()
            .expect("an unwired executor must not drop the confirmation in silence");
        let text: String = entry.content.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(
            text,
            "out-of-band write detection is degraded: /proj/src/lib.rs could not \
             be re-checked after it stopped being readable"
        );
    }

    /// A file an agent removed under an open buffer has to reach the
    /// screen by itself. Nothing else will bring it there: the reply that
    /// raises it arrives off the RPC pump, and the frame that would show it
    /// is painted only for a fold that said something changed.
    #[test]
    fn a_confirmed_vanished_file_reaches_the_frame_that_shows_it() {
        let ops = FakeOps::default();
        let (msg_tx, _msg_rx) = std::sync::mpsc::sync_channel(8);
        let executor = Executor::new(&ops).with_toast_timer(crate::wake::LoopSender::new(msg_tx));
        let mut model = Model::with_term_size(80, 24);
        model.engine.apply_grid(view_core::grid::GridOp::Resize {
            width: 80,
            height: 24,
        });
        let mut native = NativeSession::inert();
        let mut bridge = ThemeBridge::new(None);
        let mut follow_ups = FollowUps {
            native: &mut native,
            theme: &mut bridge,
            speculate: crate::speculate::SpeculationClock::default(),
        };

        let before = view_surface::render(&model);
        assert!(
            painted(
                &mut model,
                &executor,
                &mut follow_ups,
                gone(1, "/proj/src/lib.rs")
            )
            .is_none(),
            "the first answer is still being confirmed, so nothing is owed a frame"
        );
        let confirming = second_look(
            &mut model,
            &executor,
            &mut follow_ups,
            &ops,
            "/proj/src/lib.rs",
        );
        let frame = painted(&mut model, &executor, &mut follow_ups, confirming)
            .expect("a notice nobody was told to paint never reaches the terminal");
        assert_ne!(
            frame, before,
            "and the frame it asked for is the one carrying it"
        );
    }

    /// The other screen change the same reply can owe: the conflict prompt
    /// being withdrawn. A user reading "reload and discard the local edits?"
    /// against a path that has stopped being readable has to see the
    /// question go, and the reply that takes it away arrives off the RPC
    /// pump with no keystroke behind it.
    #[test]
    fn a_withdrawn_conflict_prompt_reaches_the_frame_without_it() {
        let ops = FakeOps::default();
        let (msg_tx, _msg_rx) = std::sync::mpsc::sync_channel(8);
        let executor = Executor::new(&ops).with_toast_timer(crate::wake::LoopSender::new(msg_tx));
        let mut model = Model::with_term_size(80, 24);
        model.engine.apply_grid(view_core::grid::GridOp::Resize {
            width: 80,
            height: 24,
        });
        let mut native = NativeSession::inert();
        let mut bridge = ThemeBridge::new(None);
        let mut follow_ups = FollowUps {
            native: &mut native,
            theme: &mut bridge,
            speculate: crate::speculate::SpeculationClock::default(),
        };

        let _ = dispatch(
            &mut model,
            &executor,
            &mut follow_ups,
            Msg::CheckTimeReply {
                request_id: 1,
                results: vec![(
                    std::path::PathBuf::from("/proj/src/lib.rs"),
                    view_core::msg::CheckTimeOutcome::Conflict,
                )],
            },
        );
        assert_eq!(model.overlays().len(), 1, "the prompt is standing");
        let asking = view_surface::render(&model);

        assert!(
            painted(
                &mut model,
                &executor,
                &mut follow_ups,
                gone(2, "/proj/src/lib.rs")
            )
            .is_none(),
            "an unconfirmed answer must not tear down a live question"
        );
        assert_eq!(model.overlays().len(), 1, "the question is still standing");
        let confirming = second_look(
            &mut model,
            &executor,
            &mut follow_ups,
            &ops,
            "/proj/src/lib.rs",
        );
        let frame = painted(&mut model, &executor, &mut follow_ups, confirming)
            .expect("a question leaving the screen has to take the screen with it");
        assert_eq!(model.overlays().len(), 0, "the prompt was withdrawn");
        assert_ne!(
            frame, asking,
            "and the frame painted for it no longer asks the question"
        );
    }

    /// The keystroke path that makes speculation reach the screen at all: a
    /// plain character typed in insert mode is predicted at the cursor the
    /// engine last reported, and the frame is marked so the glyph paints
    /// without waiting for the redraw that confirms it.
    #[cfg(not(feature = "bench-no-speculate"))]
    #[test]
    fn dispatch_predicts_the_glyph_for_the_key_it_sent_to_the_engine() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let mut model = Model::with_term_size(80, 24);
        model.engine.apply_grid(view_core::grid::GridOp::Resize {
            width: 80,
            height: 24,
        });
        model
            .engine
            .apply_grid(view_core::grid::GridOp::CursorGoto { row: 3, col: 5 });
        model.engine.mode.current = "insert".to_string();
        model.dirty = false;
        let mut native = NativeSession::inert();
        let mut bridge = ThemeBridge::new(None);
        let mut follow_ups = FollowUps {
            native: &mut native,
            theme: &mut bridge,
            speculate: crate::speculate::SpeculationClock::default(),
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
        assert_eq!(
            ops.calls.borrow()[0],
            "input(x)",
            "the keystroke still reaches nvim by the path it always did"
        );
        let pending: Vec<(u16, u16, char)> = model
            .speculate
            .pending()
            .iter()
            .map(|cell| (cell.row, cell.col, cell.glyph))
            .collect();
        assert_eq!(pending, vec![(3, 5, 'x')]);
        assert!(
            model.dirty,
            "a prediction nobody paints accelerates nothing"
        );
    }

    /// The other half of the same path: the engine's own answer retires the
    /// prediction it confirms, so nothing keeps painting over a cell the
    /// authoritative grid has already filled.
    #[cfg(not(feature = "bench-no-speculate"))]
    #[test]
    fn dispatch_reconciles_predictions_against_the_redraw_it_folds() {
        let ops = FakeOps::default();
        let executor = Executor::new(&ops);
        let mut model = Model::with_term_size(80, 24);
        model.engine.apply_grid(view_core::grid::GridOp::Resize {
            width: 80,
            height: 24,
        });
        model
            .engine
            .apply_grid(view_core::grid::GridOp::CursorGoto { row: 3, col: 5 });
        model.engine.mode.current = "insert".to_string();
        let mut native = NativeSession::inert();
        let mut bridge = ThemeBridge::new(None);
        let mut follow_ups = FollowUps {
            native: &mut native,
            theme: &mut bridge,
            speculate: crate::speculate::SpeculationClock::default(),
        };
        let _ = dispatch(
            &mut model,
            &executor,
            &mut follow_ups,
            Msg::Key(view_core::msg::Key {
                notation: "x".into(),
            }),
        );
        assert_eq!(model.speculate.pending().len(), 1);

        let _ = dispatch(
            &mut model,
            &executor,
            &mut follow_ups,
            Msg::Redraw(vec![view_core::events::UiEvent::GridLine {
                grid: 1,
                row: 3,
                col_start: 5,
                cells: vec![view_core::events::GridCell {
                    text: "x".to_string(),
                    hl_id: 0,
                    repeat: 1,
                }],
            }]),
        );

        assert!(model.speculate.pending().is_empty());
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
            speculate: crate::speculate::SpeculationClock::default(),
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
            speculate: crate::speculate::SpeculationClock::default(),
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
            speculate: crate::speculate::SpeculationClock::default(),
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
    /// channel stand in for whatever other producers queued in the narrow
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
            speculate: crate::speculate::SpeculationClock::default(),
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

    /// A write that failed outright against a child that is still running
    /// reads as the write side, and must never read as a dead connection.
    ///
    /// The distinction is not cosmetic and it is not about the banner's
    /// wording. `WedgeKind::Dead` is inside the unattended-restart budget,
    /// so on the shipped `auto_restart` default this same pass would return
    /// `Effect::RestartEngine`; the restart tears the old child down, and a
    /// teardown that asks a *live* nvim to quit unloads its buffers normally
    /// and deletes the swap files the restart is supposed to bring the
    /// user's unsaved work back from. `WriteSide` offers the interrupt
    /// first, then a modal, and takes nothing down on its own.
    #[test]
    fn a_failed_write_to_a_live_child_reads_as_the_write_side_and_restarts_nothing() {
        let peer = WedgedPeer::new();
        let mut model = Model::with_term_size(80, 24);
        let mut watch = OutboxStallWatch::new(TEST_STALL_THRESHOLD);
        let mut fold = SupervisionFold::default();
        // paused, and nothing written: neither watch has anything of its own
        // to report, so the verdict below can only have come from the failed
        // write the loop is reporting
        let heartbeat = HeartbeatWatch::new(TEST_STALL_THRESHOLD);

        let msg = note_supervision(&mut fold, &mut watch, &heartbeat, &peer.handle, false, true)
            .expect("a failed write is a condition the fold has something to say about");
        assert_eq!(
            fold.wedge,
            Some(WedgeKind::WriteSide),
            "a running child whose write path broke is a wedge with more than one recovery"
        );

        assert!(
            model.supervision.recovers_unattended(),
            "the shipped default recovers without asking, which is what makes the Dead \
             classification destructive here rather than merely mislabelled"
        );
        assert!(
            update(&mut model, msg).is_empty(),
            "the write side must ask for no unattended restart: the teardown one performs \
             sends `qa!` to the live child and takes its swap files with it"
        );
    }

    /// The combination production actually produces, and the one every
    /// other write-side test here deliberately excludes by leaving the
    /// heartbeat paused: probes leave through the same outbox as everything
    /// else, so a writer parked inside a write stops the read side too and
    /// both verdicts are true at once. The notice must name the writer --
    /// the one of the two that can be the cause rather than the effect --
    /// and a moving writer is what rules it out.
    #[test]
    fn a_connection_wedged_on_both_sides_names_the_writer() {
        let peer = WedgedPeer::new();
        let mut model = Model::with_term_size(80, 24);
        let mut watch = OutboxStallWatch::new(TEST_STALL_THRESHOLD);
        let mut fold = SupervisionFold::default();
        let mut heartbeat = HeartbeatWatch::new(TEST_STALL_THRESHOLD);
        heartbeat.resume();
        let prober = heartbeat.prober();

        peer.handle.input("a").unwrap();
        peer.await_parked_write();
        peer.handle.input("b").unwrap();
        // queued behind the parked write like any other message, which is
        // exactly why a stalled writer wedges the read side as well
        prober.tick(&peer.handle).unwrap();

        // the write side's window opens at the observation that first finds
        // the backlog unmoved, never at the first one -- see
        // `OutboxStallWatch`'s own doc on why it is timed between two
        assert!(!watch.observe(&peer.handle));
        std::thread::sleep(TEST_STALL_THRESHOLD * 3);
        assert_eq!(
            heartbeat.observe(peer.handle.is_closed()),
            view_engine::heartbeat::Liveness::Wedged,
            "the read side must be genuinely wedged or this test pins nothing"
        );
        assert!(
            watch.observe(&peer.handle),
            "the write side must be genuinely stalled or this test pins nothing"
        );

        assert!(note_supervision_pass(
            &mut model,
            &mut fold,
            &mut watch,
            &heartbeat,
            &peer.handle
        ));
        assert_eq!(
            fold.wedge,
            Some(WedgeKind::WriteSide),
            "both sides wedged must read as the write side"
        );
        assert_eq!(
            visible_texts(&model),
            vec![WedgeKind::WriteSide.notice().to_string()],
            "the notice named the consequence instead of the cause"
        );
    }

    /// An outage that changes classification is one outage: the readout a
    /// user has been watching count up must not restart because the fold
    /// changed its mind about which half is at fault.
    #[test]
    fn a_wedge_changing_kind_keeps_the_episodes_own_clock() {
        let mut fold = SupervisionFold::default();
        let Some(Msg::EngineLiveness { observed_for, .. }) = fold.note(Some(WedgeKind::WriteSide))
        else {
            panic!("the first wedge must be reported");
        };
        assert_eq!(observed_for, std::time::Duration::ZERO);
        let opened = fold.since.expect("the episode opened its clock");

        std::thread::sleep(std::time::Duration::from_millis(20));
        let Some(Msg::EngineLiveness {
            wedge,
            observed_for,
        }) = fold.note(Some(WedgeKind::ReadSide))
        else {
            panic!("the reclassified wedge must be reported");
        };
        assert_eq!(wedge, Some(WedgeKind::ReadSide));
        assert_eq!(
            fold.since,
            Some(opened),
            "the reclassification restarted the episode's clock"
        );
        assert!(
            observed_for >= std::time::Duration::from_millis(20),
            "the readout went backwards across the flip: {observed_for:?}"
        );

        // and the clock is released only when the outage itself ends
        assert!(fold.note(None).is_some());
        assert_eq!(fold.since, None);
    }

    /// The readout is the only thing on screen that changes while an engine
    /// is quiet, and nothing quiet wakes the loop, so the fold has to ask.
    #[test]
    fn a_visible_episode_asks_for_the_wakeup_its_readout_needs() {
        let mut fold = SupervisionFold::default();
        assert_eq!(
            fold.readout_deadline(),
            None,
            "a healthy session must not pay for a wakeup it has nothing to paint at"
        );
        let _ = fold.note(Some(WedgeKind::ReadSide));
        assert_eq!(fold.readout_deadline(), Some(READOUT_RESOLUTION));
        let _ = fold.note(None);
        assert_eq!(fold.readout_deadline(), None);
    }

    #[test]
    fn a_wedged_engine_raises_the_notice_and_retracts_it_when_the_writer_moves_again() {
        let mut peer = WedgedPeer::new();
        let mut model = Model::with_term_size(80, 24);
        let mut watch = OutboxStallWatch::new(TEST_STALL_THRESHOLD);
        let mut fold = SupervisionFold::default();
        // paused, so the read side reports Alive and this test's subject
        // stays the write side alone
        let heartbeat = HeartbeatWatch::new(TEST_STALL_THRESHOLD);

        peer.handle.input("a").unwrap();
        peer.await_parked_write();
        // queued behind a write that cannot finish, so the backlog outlives
        // the message the writer is holding
        peer.handle.input("b").unwrap();

        // the stall is measured from an observation, never asserted by the
        // first one: nothing has yet been seen to stop moving
        assert!(
            !note_supervision_pass(&mut model, &mut fold, &mut watch, &heartbeat, &peer.handle),
            "the notice was raised by the observation that first saw the backlog, \
             before any time had passed for the writer to be stalled through"
        );
        assert!(model.engine.messages.entries.is_empty());

        std::thread::sleep(TEST_STALL_THRESHOLD * 3);
        assert!(
            note_supervision_pass(&mut model, &mut fold, &mut watch, &heartbeat, &peer.handle),
            "a writer parked inside a write, with a second message queued behind it \
             and the threshold long past, raised no notice"
        );
        assert_eq!(
            visible_texts(&model),
            vec![WedgeKind::WriteSide.notice().to_string()]
        );

        assert!(
            !note_supervision_pass(&mut model, &mut fold, &mut watch, &heartbeat, &peer.handle),
            "re-asserting an unchanged notice reported a change, which repaints \
             the toast on every loop pass for as long as the stall lasts"
        );

        // a keypress dismisses transient toasts; this one describes a
        // condition that is still true, and the keypress that would drop it
        // is the one it exists to explain
        assert!(!model.engine.messages.dismiss_transient_on_keypress(false));
        assert_eq!(
            visible_texts(&model),
            vec![WedgeKind::WriteSide.notice().to_string()]
        );

        peer.release();
        wait_until("the writer drains its backlog", || {
            peer.handle.write_progress().0 == 0
        });
        assert!(
            note_supervision_pass(&mut model, &mut fold, &mut watch, &heartbeat, &peer.handle),
            "the backlog drained and the notice was not retracted"
        );
        assert!(model.engine.messages.entries.is_empty());
    }

    #[test]
    fn a_wedge_surfaces_without_any_further_input() {
        let mut peer = WedgedPeer::new();
        let mut model = Model::with_term_size(80, 24);
        let mut watch = OutboxStallWatch::new(TEST_STALL_THRESHOLD);
        let mut fold = SupervisionFold::default();
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
        let heartbeat = HeartbeatWatch::new(TEST_STALL_THRESHOLD);
        let start = std::time::Instant::now();
        while !note_supervision_pass(&mut model, &mut fold, &mut watch, &heartbeat, &peer.handle) {
            assert!(
                wait_for_msg(
                    &msg_rx,
                    Wakeups {
                        write: &watch,
                        read: &heartbeat,
                        supervision: &fold,
                        speculation: None,
                        spinner: None,
                        reconnect: None,
                    },
                )
                .is_none(),
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
            vec![WedgeKind::WriteSide.notice().to_string()]
        );
        peer.release();
    }

    /// An idle session's only armed wakeup is the read side's prospective
    /// one -- the instant a probe not yet sent could have gone unanswered
    /// for a threshold -- and nothing shortens it: the write side, with its
    /// backlog drained, asks for nothing at all, and the wait itself
    /// delivers only what is sent to it.
    #[test]
    fn an_idle_session_arms_only_the_wedge_deadline_and_is_never_woken_early() {
        let mut peer = WedgedPeer::new();
        // a peer that reads normally, expressed through the same sink as
        // the wedged one: healthy and wedged differ by this one call
        peer.release();
        let mut model = Model::with_term_size(80, 24);
        let mut watch = OutboxStallWatch::new(TEST_STALL_THRESHOLD);
        let mut fold = SupervisionFold::default();
        // armed, and nothing has probed it: the state a session sits in
        // between one answered probe and the next tick, which is where a
        // wedge that nobody is typing at begins
        let mut heartbeat = HeartbeatWatch::new(TEST_STALL_THRESHOLD);
        heartbeat.resume();

        peer.handle.input("a").unwrap();
        wait_until("the writer drains its backlog", || {
            peer.handle.write_progress().0 == 0
        });
        assert!(!note_supervision_pass(
            &mut model,
            &mut fold,
            &mut watch,
            &heartbeat,
            &peer.handle
        ));
        assert_eq!(
            watch.poll_deadline(),
            None,
            "the write side armed a deadline, so nothing below is about the read side"
        );
        let armed = watch_deadline(Wakeups {
            write: &watch,
            read: &heartbeat,
            supervision: &fold,
            speculation: None,
            spinner: None,
            reconnect: None,
        })
        .expect("an idle session must still arm the wakeup a silent engine needs");
        assert!(
            armed > TEST_STALL_THRESHOLD,
            "the idle wait was cut to {armed:?}, which is sooner than the silence it \
             would take to prove anything"
        );

        // and the wait itself delivers only what is sent, when it is sent
        let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
        let quiet = TEST_STALL_THRESHOLD * 3;
        std::thread::spawn(move || {
            std::thread::sleep(quiet);
            let _ = msg_tx.send(Msg::RedrawReady);
        });
        let start = std::time::Instant::now();
        let received = wait_for_msg(
            &msg_rx,
            Wakeups {
                write: &watch,
                read: &heartbeat,
                supervision: &fold,
                speculation: None,
                spinner: None,
                reconnect: None,
            },
        );
        assert!(
            matches!(received, Some(Ok(Msg::RedrawReady))),
            "an idle wait returned something other than the one message sent to it"
        );
        assert!(
            start.elapsed() >= quiet,
            "the idle wait returned before its only message was sent: the loop was \
             woken by a deadline shorter than the silence that would justify one"
        );
    }

    /// The same silent session, with one prediction painted over the grid:
    /// the wait that was unbounded a line earlier is now bounded by the age
    /// bound itself, and the pass it returns for retires the prediction and
    /// marks the frame that takes the glyph off the terminal.
    ///
    /// This is the reachability the per-pass expiry site cannot supply on
    /// its own. Every other wakeup this loop arms is an order of magnitude
    /// coarser than one second -- and a paused heartbeat arms none -- so
    /// without the speculation deadline in this fold a glyph painted on a
    /// one-second promise would stand until something unrelated happened to
    /// wake the loop.
    #[cfg(not(feature = "bench-no-speculate"))]
    #[test]
    fn a_silent_session_with_a_prediction_pending_cannot_sleep_past_the_age_bound() {
        use view_core::native::speculate::SPECULATION_MAX_AGE;

        let mut model = Model::with_term_size(80, 24);
        model.engine.apply_grid(view_core::grid::GridOp::Resize {
            width: 80,
            height: 24,
        });
        model.engine.mode.current = "insert".to_string();
        // the silent session in full: nothing queued for the writer, a
        // heartbeat that has never been resumed, and no wedge to report on
        let watch = OutboxStallWatch::new(TEST_STALL_THRESHOLD);
        let fold = SupervisionFold::default();
        let heartbeat = HeartbeatWatch::new(TEST_STALL_THRESHOLD);
        assert_eq!(
            watch_deadline(Wakeups {
                write: &watch,
                read: &heartbeat,
                supervision: &fold,
                speculation: None,
                spinner: None,
                reconnect: None,
            }),
            None,
            "the session this test is about must be one nothing else wakes"
        );

        let origin = std::time::Instant::now();
        crate::speculate::note_engine_call(
            &mut model,
            &view_core::msg::RpcCall::Input {
                notation: "x".to_string(),
            },
            crate::speculate::SpeculationClock::started_at(origin),
        );
        assert_eq!(model.speculate.pending().len(), 1);
        model.dirty = false;

        // the same session a hair before the bound, modelled by moving the
        // clock's origin back rather than by sleeping out most of a second
        let grace = std::time::Duration::from_millis(50);
        let nearly_due = crate::speculate::SpeculationClock::started_at(
            origin
                .checked_sub(SPECULATION_MAX_AGE.saturating_sub(grace))
                .unwrap_or(origin),
        );
        let speculation = crate::speculate::next_expiry(&model, nearly_due);
        let armed = watch_deadline(Wakeups {
            write: &watch,
            read: &heartbeat,
            supervision: &fold,
            speculation,
            spinner: None,
            reconnect: None,
        })
        .expect("a pending prediction must bound a wait nothing else bounds");
        assert!(
            armed <= grace,
            "the armed wait was {armed:?}, which is past the bound the glyph was \
             painted on"
        );

        let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(WAIT_WATCHDOG_SECS));
            let _ = msg_tx.send(Msg::RedrawReady);
        });
        let start = std::time::Instant::now();
        assert!(
            wait_for_msg(
                &msg_rx,
                Wakeups {
                    write: &watch,
                    read: &heartbeat,
                    supervision: &fold,
                    speculation,
                    spinner: None,
                    reconnect: None,
                },
            )
            .is_none(),
            "the wait outlasted the age bound and returned the watchdog's message: \
             a prediction made just before the engine went silent would stand"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(WAIT_WATCHDOG_SECS),
            "the wait ran to the watchdog rather than to the prediction's own deadline"
        );

        // and the pass that wait returns for is the one that retires it
        crate::speculate::expire_speculation(
            &mut model,
            crate::speculate::SpeculationClock::started_at(
                origin
                    .checked_sub(SPECULATION_MAX_AGE + grace)
                    .unwrap_or(origin),
            ),
        );
        assert!(
            model.speculate.pending().is_empty(),
            "the pass the deadline bought retired nothing"
        );
        assert!(
            model.dirty,
            "the retirement marked no frame, so the glyph stays on the terminal"
        );
    }

    /// The read side's own version of the wedge nobody types at: the write
    /// side is draining perfectly, so it arms no deadline at all, and the
    /// only thing that could ever wake the loop is the answer that is not
    /// coming. Without the read side's deadline in the same wait, an
    /// operator who typed once and stopped would be told nothing.
    #[test]
    fn an_unanswered_probe_arms_the_deadline_a_silent_read_side_leaves_unarmed() {
        let mut peer = WedgedPeer::new();
        peer.release();
        let mut model = Model::with_term_size(80, 24);
        let mut watch = OutboxStallWatch::new(TEST_STALL_THRESHOLD);
        let mut fold = SupervisionFold::default();
        let mut heartbeat = HeartbeatWatch::new(TEST_STALL_THRESHOLD);
        heartbeat.resume();

        heartbeat.prober().tick(&peer.handle).unwrap();
        wait_until("the writer drains the probe", || {
            peer.handle.write_progress().0 == 0
        });
        assert!(!note_supervision_pass(
            &mut model,
            &mut fold,
            &mut watch,
            &heartbeat,
            &peer.handle
        ));
        assert_eq!(
            watch.poll_deadline(),
            None,
            "the write side armed a deadline, so this proves nothing about the read side"
        );

        let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(WAIT_WATCHDOG_SECS));
            let _ = msg_tx.send(Msg::RedrawReady);
        });
        let start = std::time::Instant::now();
        assert!(
            wait_for_msg(
                &msg_rx,
                Wakeups {
                    write: &watch,
                    read: &heartbeat,
                    supervision: &fold,
                    speculation: None,
                    spinner: None,
                    reconnect: None,
                },
            )
            .is_none(),
            "the wait outlasted the unanswered probe's deadline and returned the \
             watchdog's message: a read-side wedge nobody types at would never surface"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(WAIT_WATCHDOG_SECS),
            "the wait ran to the watchdog rather than to the probe's own deadline"
        );
        // and the fold the loop runs turns that into the read-side wedge,
        // with the write side draining perfectly throughout
        wait_until("the unanswered probe reads as a wedge", || {
            note_supervision(
                &mut fold,
                &mut watch,
                &heartbeat,
                &peer.handle,
                false,
                false,
            );
            fold.wedge == Some(WedgeKind::ReadSide)
        });
    }

    /// The wiring rather than the fold: a real engine, its own prober
    /// thread, the real reader thread, the real sink and the real
    /// [`intake`] arm, with nothing between them stubbed. All three seams
    /// the feature hangs on are load-bearing here -- the prober
    /// `Engine::spawn` starts, the arming `Engine::start_pump` does, and
    /// the acknowledgement `intake` records -- and removing any one of them
    /// leaves this test with nothing to observe: no reply arrives at all,
    /// or replies arrive and the window never closes.
    #[test]
    fn a_live_engine_answers_the_probe_and_the_loops_intake_records_it() {
        let mut engine = Engine::spawn(view_engine::process::EngineConfig::isolated()).unwrap();
        let (tx, rx) = mpsc::sync_channel::<Msg>(64);
        let (pump, _cutover) = engine.start_pump(tx);
        let mut model = Model::with_term_size(80, 24);

        // generous, and hang-safe: the first tick is one interval out, and
        // this budget only ever has to outlast a scheduler, never a wedge
        let deadline =
            std::time::Instant::now() + view_engine::heartbeat::HEARTBEAT_PROBE_INTERVAL * 4;
        let mut replies = 0u32;
        let mut answered_in_full = false;
        while !answered_in_full {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let Ok(msg) = rx.recv_timeout(left) else {
                break;
            };
            if matches!(msg, Msg::HeartbeatReply { .. }) {
                replies += 1;
            }
            let _ = intake(Ok(msg), &mut engine, &pump, &mut model);
            // read a whole wedge threshold into the future: `Alive` there
            // is only possible with nothing outstanding, so it says every
            // probe this engine has been sent was answered *and* folded in
            // through the loop's own intake
            answered_in_full = replies > 0
                && engine.heartbeat.observe_at(
                    false,
                    std::time::Instant::now() + view_engine::heartbeat::HEARTBEAT_WEDGE_THRESHOLD,
                ) == view_engine::heartbeat::Liveness::Alive;
        }
        assert!(
            replies > 0,
            "a live engine answered no heartbeat inside {:?}: nothing is probing it",
            view_engine::heartbeat::HEARTBEAT_PROBE_INTERVAL * 4
        );
        assert!(
            answered_in_full,
            "{replies} heartbeat replies reached the sink and the watch still reads a \
             probe as outstanding: the acknowledgement never made it out of the loop's \
             intake"
        );
        let _ = engine.wait_exit();
    }

    /// Waits for the reader thread's own terminal signal, which is the
    /// message the loop's [`intake`] classifies. Nothing else in the channel
    /// answers the question these tests ask, and a fixed pause would race a
    /// process that is still exiting. The bound is the caller's to pick
    /// because for some callers it is the assertion itself: a quit whose
    /// announcement went unanswered never produces an `EngineStopped` at
    /// all, so arriving inside a short bound is what proves the answer
    /// happened.
    fn await_engine_stopped(rx: &mpsc::Receiver<Msg>, within: std::time::Duration) -> Msg {
        let deadline = std::time::Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let msg = rx.recv_timeout(left).unwrap_or_else(|_| {
                panic!("a stopped engine must signal EngineStopped within {within:?}")
            });
            if matches!(msg, Msg::EngineStopped { .. }) {
                return msg;
            }
        }
    }

    /// The most destructive thing supervision could do, guarded at the seam
    /// that decides it: `:qa!` reaches this loop looking exactly like a
    /// crash -- the process stopped and the connection closed -- and reading
    /// it as one would respawn the editor its user had just closed.
    #[test]
    fn an_engine_a_user_quit_ends_the_session_rather_than_being_supervised() {
        let mut engine = Engine::spawn(view_engine::process::EngineConfig::isolated()).unwrap();
        let (tx, rx) = mpsc::sync_channel::<Msg>(64);
        let (pump, _cutover) = engine.start_pump(tx);
        let mut model = Model::with_term_size(80, 24);
        // the same registration, in the same order, `startup` performs on
        // every real session: it is what installs the `VimLeavePre` relay
        // this seam's answer rests on, so a test that skipped it would be
        // asserting against an engine no user ever runs
        engine
            .handle
            .register_bridge(engine.api_info.channel_id)
            .unwrap();
        // `--embed` holds nvim's startup until a UI attaches, so an engine
        // nobody attached to would never read the quit typed below
        engine.handle.ui_attach(80, 24).unwrap();
        engine.handle.input(":qa!<CR>").unwrap();

        // the announcement is a blocking round trip inside nvim's
        // `VimLeavePre`: answered off the reader thread it costs one pipe
        // hop, but answered by nothing at all it holds the editor open with
        // no `EngineStopped` ever produced -- dropping the reply write
        // wedges this wait to its full bound -- so the short bound IS the
        // discriminator: a stop that arrives at all arrived because the
        // announcement was answered
        let stopped = await_engine_stopped(&rx, std::time::Duration::from_secs(5));
        let resolved = intake(Ok(stopped), &mut engine, &pump, &mut model)
            .expect("a stop from the engine this session runs is never dropped");
        let Msg::EngineDown(exit) = resolved else {
            unreachable!("a quit engine must resolve to EngineDown, got {resolved:?}");
        };
        assert_eq!(
            exit.code,
            Some(0),
            "the session must end with nvim's own status"
        );
        assert!(
            !exit.by_signal,
            "an engine that exited on its own instruction died of no signal"
        );
    }

    /// The counterpart, and the reason `Dead` is reachable at all: an engine
    /// nobody told to stop is a state to report and recover from, not an
    /// exit to take.
    #[cfg(unix)]
    #[test]
    fn an_engine_killed_out_of_band_is_handed_to_supervision_rather_than_quit() {
        let mut engine = Engine::spawn(view_engine::process::EngineConfig::isolated()).unwrap();
        let (tx, rx) = mpsc::sync_channel::<Msg>(64);
        let (pump, _cutover) = engine.start_pump(tx);
        let mut model = Model::with_term_size(80, 24);
        let killed = std::process::Command::new("kill")
            .args(["-KILL", &engine.pid().to_string()])
            .status()
            .expect("kill must run for an out-of-band crash to be simulable");
        assert!(killed.success(), "kill -KILL failed: {killed:?}");

        let stopped = await_engine_stopped(&rx, std::time::Duration::from_secs(30));
        let resolved = intake(Ok(stopped), &mut engine, &pump, &mut model)
            .expect("a stop from the engine this session runs is never dropped");
        assert!(
            matches!(resolved, Msg::EngineStopped { .. }),
            "a killed engine must reach supervision rather than resolve to a \
             quit, got {resolved:?}"
        );
        assert_eq!(
            model.supervision.exit_code(),
            137,
            "the status a user answering with Quit would leave with must be \
             the one nvim's death reported"
        );
        // and the verdict the loop's own fold reaches once that resolution
        // is in hand, which is the whole point of passing the stop through
        assert_eq!(
            wedge_kind(false, view_engine::heartbeat::Liveness::Dead, true),
            Some(WedgeKind::Dead)
        );
    }

    #[test]
    fn an_engine_that_keeps_writing_never_raises_the_notice() {
        let mut peer = WedgedPeer::new();
        let mut model = Model::with_term_size(80, 24);
        let mut watch = OutboxStallWatch::new(TEST_STALL_THRESHOLD);
        let mut fold = SupervisionFold::default();
        // paused, so the read side reports Alive and this test's subject
        // stays the write side alone
        let heartbeat = HeartbeatWatch::new(TEST_STALL_THRESHOLD);
        peer.release();

        for i in 0..20 {
            peer.handle.input("a").unwrap();
            peer.await_parked_write();
            assert!(
                !note_supervision_pass(&mut model, &mut fold, &mut watch, &heartbeat, &peer.handle),
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
        let mut fold = SupervisionFold::default();
        // paused, so the read side reports Alive and this test's subject
        // stays the write side alone
        let heartbeat = HeartbeatWatch::new(TEST_STALL_THRESHOLD);
        peer.release();

        peer.handle.input("a").unwrap();
        wait_until("the writer drains its backlog", || {
            peer.handle.write_progress().0 == 0
        });
        assert!(!note_supervision_pass(
            &mut model,
            &mut fold,
            &mut watch,
            &heartbeat,
            &peer.handle
        ));
        std::thread::sleep(TEST_STALL_THRESHOLD * 3);
        assert!(
            !note_supervision_pass(&mut model, &mut fold, &mut watch, &heartbeat, &peer.handle),
            "an engine with an empty queue read as stalled after three thresholds \
             of doing nothing, which is an idle editor rather than a wedged one"
        );
        assert!(model.engine.messages.entries.is_empty());
    }
}
