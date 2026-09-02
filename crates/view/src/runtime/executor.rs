//! Carrying out one [`Effect`] against the engine connection: the
//! [`Executor`] the loop hands every effect to, and the [`Flow`] its answer
//! is read as.
//!
//! Split out of [`super`] whole: the loop (`run`, `dispatch`, the wait and
//! its wakeups) and the effect table are two concerns of one size each, and
//! the table is the half that grows with every new effect.

use super::spawn_or_log;
use crate::engine_ops::EngineOps;
use crate::osc52::Osc52Job;
use std::sync::mpsc;
use view_core::msg::{Effect, Msg, RpcCall};
use view_engine::nvim_api::BufWriteOutcome;

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
    pub(super) toast_timer: Option<crate::wake::LoopSender>,
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

/// What the user is told when the confirming second look at `path`
/// (`Effect::ReprobeExternalWrite`) could not be scheduled at all.
///
/// Written for that user rather than for a log, and routed through
/// `Msg::ExternalWatchDegraded` because the consequence is that message's
/// own subject: a removal whose confirmation never runs is never announced,
/// so detection is quietly not covering what `docs/ai.md` says it covers.
pub(super) fn reprobe_unscheduled(path: &std::path::Path) -> String {
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
    pub(super) fn route_loop_msg(&self, msg: Msg) {
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
                    RpcCall::HoldNotify => self.ops.hold_notify(),
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
                    RpcCall::SetFloatHidden { win, hide } => self.ops.set_float_hidden(win, hide),
                    RpcCall::ReadFloatRows { win } => self.ops.read_float_rows(win),
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
                    // Neither call correlates a reply, which is what lets
                    // the loop emit them on the path that paints: the marks
                    // are presentation over text nvim owns, so a draw that
                    // fails to land is one repaint away from being right
                    // again and nothing here waits to find out.
                    RpcCall::ReviewShow {
                        buf,
                        marks,
                        cursor_row,
                        focus,
                        open_target,
                    } => self
                        .ops
                        .review_show(buf, &marks, cursor_row, focus, open_target),
                    RpcCall::ReviewClear { buf } => self.ops.review_clear(buf),
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
            // the same one-shot thread again, and one live at a time by
            // construction: `update` returns this only while a motion is
            // running and only once per frame, so the chain is at most the
            // frames one dismissal is quantized into and stops on its own.
            // Unlike every other timer here, a lost wakeup does not merely
            // withhold a later state -- it holds the stack at the frame the
            // motion had reached, since the tick that retires the motion is
            // the same one that would have advanced it. So a refused thread
            // answers `Msg::AnimDropped` on the channel it was going to use,
            // which settles the stack the way a terminal below the full tier
            // paints it. The unwired half of the same degrade is `dispatch`'s
            // to fold, there being no channel here to carry it.
            Effect::ScheduleAnimTick { after } => {
                if let Some(tx) = &self.toast_timer {
                    let ticker = tx.clone();
                    let armed = spawn_or_log("toast-motion", move || {
                        std::thread::sleep(after);
                        let _ = ticker.send(Msg::AnimTick);
                    });
                    if !armed {
                        let _ = tx.send(Msg::AnimDropped);
                    }
                }
                Flow::Continue
            }
            // the same one-shot thread, and the same degrade: an unwired
            // channel leaves the hold to be resolved by the probe's answer
            // or the first keypress, both of which arrive on paths that do
            // not need a clock
            Effect::ScheduleStartupHold { after } => {
                if let Some(tx) = &self.toast_timer {
                    let tx = tx.clone();
                    spawn_or_log("startup-hold", move || {
                        std::thread::sleep(after);
                        let _ = tx.send(Msg::StartupHoldExpired);
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
