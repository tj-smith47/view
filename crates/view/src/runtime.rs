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
    OSC52_MAX_PAYLOAD_BYTES,
};
use view_core::native::mappings::MappingSpec;
use view_core::native::supervision::{WedgeKind, READOUT_RESOLUTION};
use view_core::update::update;
use view_engine::handle::{EngineError, EngineHandle};
use view_engine::heartbeat::{HeartbeatWatch, Liveness};
use view_engine::process::Engine;
use view_engine::stall::OutboxStallWatch;
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
    /// Resolves the picker preview pane's text for `path`, tagged
    /// `generation`; never blocks, and never itself returns the answer (see
    /// `Msg::PickerPreviewReply`).
    fn preview_buffer(&self, path: &str, generation: u64) -> Result<(), EngineError>;
    /// Opens `path` as `:edit` would, reusing an already-loaded buffer
    /// rather than duplicating it; fire-and-forget, no reply (see
    /// `RpcCall::OpenFile`).
    fn open_file(&self, path: &str) -> Result<(), EngineError>;
    /// Renames `old_path` to `new_path`, retargeting any open buffer along
    /// with it, tagged `generation`; never blocks, and never itself returns
    /// the answer (see `RpcCall::RenameFile`, `Msg::TreeRenameReply`).
    fn rename_file(
        &self,
        old_path: &str,
        new_path: &str,
        generation: u64,
    ) -> Result<(), EngineError>;
    /// Asks nvim for a new file's name via a blocked `vim.fn.input()`,
    /// tagged `generation`; never blocks, and never itself returns the
    /// answer (see `RpcCall::TreeCreatePrompt`, `Msg::TreeCreatePromptReply`).
    fn tree_create_prompt(&self, generation: u64) -> Result<(), EngineError>;
    /// Asks nvim for a rename target for `old_path`, pre-filled with
    /// `current_name`, tagged `generation`; never blocks, and never itself
    /// returns the answer (see `RpcCall::TreeRenamePrompt`,
    /// `Msg::TreeRenamePromptReply`).
    fn tree_rename_prompt(
        &self,
        old_path: &str,
        current_name: &str,
        generation: u64,
    ) -> Result<(), EngineError>;
    /// Asks nvim to confirm deleting `path`, tagged `generation`; never
    /// blocks, and never itself returns the answer (see
    /// `RpcCall::TreeDeleteConfirm`, `Msg::TreeDeleteConfirmReply`).
    fn tree_delete_confirm(&self, path: &str, generation: u64) -> Result<(), EngineError>;
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
    fn preview_buffer(&self, path: &str, generation: u64) -> Result<(), EngineError> {
        self.preview_buffer(path, generation)
    }
    fn open_file(&self, path: &str) -> Result<(), EngineError> {
        self.open_file(path)
    }
    fn rename_file(
        &self,
        old_path: &str,
        new_path: &str,
        generation: u64,
    ) -> Result<(), EngineError> {
        self.rename_file(old_path, new_path, generation)
    }
    fn tree_create_prompt(&self, generation: u64) -> Result<(), EngineError> {
        self.tree_create_prompt(generation)
    }
    fn tree_rename_prompt(
        &self,
        old_path: &str,
        current_name: &str,
        generation: u64,
    ) -> Result<(), EngineError> {
        self.tree_rename_prompt(old_path, current_name, generation)
    }
    fn tree_delete_confirm(&self, path: &str, generation: u64) -> Result<(), EngineError> {
        self.tree_delete_confirm(path, generation)
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
    fn preview_buffer(&self, path: &str, generation: u64) -> Result<(), EngineError> {
        (**self).preview_buffer(path, generation)
    }
    fn open_file(&self, path: &str) -> Result<(), EngineError> {
        (**self).open_file(path)
    }
    fn rename_file(
        &self,
        old_path: &str,
        new_path: &str,
        generation: u64,
    ) -> Result<(), EngineError> {
        (**self).rename_file(old_path, new_path, generation)
    }
    fn tree_create_prompt(&self, generation: u64) -> Result<(), EngineError> {
        (**self).tree_create_prompt(generation)
    }
    fn tree_rename_prompt(
        &self,
        old_path: &str,
        current_name: &str,
        generation: u64,
    ) -> Result<(), EngineError> {
        (**self).tree_rename_prompt(old_path, current_name, generation)
    }
    fn tree_delete_confirm(&self, path: &str, generation: u64) -> Result<(), EngineError> {
        (**self).tree_delete_confirm(path, generation)
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
}

/// One OSC52 clipboard-set escape to write, queued by [`Executor::run`] and
/// drained by [`run`]'s loop into [`view_tui::terminal::Term::write_osc52`].
pub struct Osc52Job {
    pub register: char,
    pub lines: Vec<String>,
    pub regtype: RegisterType,
}

/// The one capability [`drain_osc52`] needs of the real terminal, factored
/// out of [`Term`] the same way [`EngineOps`] is factored out of
/// `EngineHandle`: a test drives the cap-before-encode and skip-and-log
/// logic below against a recording fake, with no real terminal and no
/// stdout write in the loop.
trait Osc52Sink {
    fn write_osc52(&mut self, register: char, text: &str) -> std::io::Result<()>;
}

impl Osc52Sink for Term {
    fn write_osc52(&mut self, register: char, text: &str) -> std::io::Result<()> {
        Term::write_osc52(self, register, text)
    }
}

/// Drains every OSC52 job currently queued on `osc52_rx` into `sink`,
/// applying the same cap-before-encode and skip-and-log policy
/// `Effect::Osc52Copy`'s doc states: an over-cap payload is logged and
/// never handed to `sink` at all (no truncated write), and a write error
/// is logged rather than propagated, since nothing on the wire is blocked
/// on this fire-and-forget escape (see [`Osc52Sink`]'s doc for the seam
/// this drives against in tests).
fn drain_osc52<S: Osc52Sink>(osc52_rx: &mpsc::Receiver<Osc52Job>, sink: &mut S) {
    while let Ok(job) = osc52_rx.try_recv() {
        let text = view_native::clipboard::lines_to_text(&job.lines, job.regtype);
        // base64 expands 3 raw bytes to 4 encoded ones; this is the
        // size the terminal actually receives and the bound
        // `OSC52_MAX_PAYLOAD_BYTES` states in `Osc52Copy`'s own doc
        let encoded_len = text.len().div_ceil(3) * 4;
        if encoded_len > OSC52_MAX_PAYLOAD_BYTES {
            crate::vlog::log_with("osc52", || {
                format!(
                    "skipped a {encoded_len}-byte payload over the \
                     {OSC52_MAX_PAYLOAD_BYTES}-byte cap"
                )
            });
            continue;
        }
        // fire-and-forget per `Effect::Osc52Copy`'s doc: nothing on the
        // wire is blocked on this escape, so a transient stdout error
        // here must not tear the session down the way a frame-paint
        // failure does (`draw_surface`'s `?` in `run`, which the terminal's
        // own real content depends on)
        if let Err(err) = sink.write_osc52(job.register, &text) {
            crate::vlog::log_with("osc52", || format!("write failed: {err}"));
        }
    }
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
fn spawn_or_log<F>(label: &'static str, f: F) -> bool
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
            tree_scan_cancel: std::sync::Mutex::new(None),
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

/// Which wedge, if any, the two watches see between them.
///
/// A closed connection outranks every timing question below it, the same
/// ordering [`HeartbeatWatch::observe_at`] applies for the same reason: a
/// probe still waiting on a connection that is gone is waiting on nothing.
///
/// Between the two open-connection failures the write side's verdict wins,
/// because it is the one that can be the root cause of the other. Every
/// probe the read side is waiting on left through the same outbox, so a
/// writer that has stopped delivering is enough on its own to make the read
/// side report a wedge -- while a writer that is still delivering rules the
/// write side out entirely, whatever the read side says. Taking the read
/// side first would report the consequence and hide the cause, and it would
/// do so on the overwhelming majority of real stalls, since the two
/// thresholds are equal and both sides cross them together.
fn wedge_kind(write_stalled: bool, read: Liveness) -> Option<WedgeKind> {
    match read {
        // not reachable from this loop today, and deliberately kept: the
        // intake resolves `Msg::EngineStopped` (and a disconnected channel)
        // into `Msg::EngineDown` and thence `Effect::Quit` before any
        // top-of-pass reading of `is_closed` can find the connection gone,
        // so a closed connection ends the session rather than being
        // supervised through it. The verdict exists for the respawn that
        // will change that -- an engine view can bring back is one whose
        // death is a state to report rather than an exit
        Liveness::Dead => Some(WedgeKind::Dead),
        _ if write_stalled => Some(WedgeKind::WriteSide),
        Liveness::Wedged => Some(WedgeKind::ReadSide),
        // `Liveness` is `#[non_exhaustive]`, so this arm also catches a
        // verdict a later engine build might add: with the write side moving
        // and no verdict this build understands, there is nothing to report
        _ => None,
    }
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
    /// transition, which is what keeps the banner re-asserted against an
    /// `msg_clear` that would otherwise take it down while its condition is
    /// still true.
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
/// outstanding at all. Two more follow in the same pass from
/// [`watch_deadline`], which asks the same watch for a deadline.
///
/// The steady state named here is the exact one all four of those questions
/// short-circuit on: nothing queued for the writer and no probe owed an
/// answer. The whole pass costs no clock read, no lock, no allocation, no
/// walk of the message log and no dispatch -- the deadline questions decline
/// the clock on the same evidence the observations do
/// ([`OutboxStallWatch::poll_deadline`], [`HeartbeatWatch::poll_deadline`]),
/// so a healthy session pays for no reading any of them could use. A pass
/// with output pending pays one `Instant::now()` on the write side, and a
/// pass with a probe outstanding reads the send-time log and takes one of
/// its own -- both bought by a connection that is demonstrably
/// mid-something rather than by every keystroke.
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
) -> Option<Msg> {
    let stalled = write.observe(handle);
    fold.note(wedge_kind(stalled, read.observe(handle.is_closed())))
}

/// The soonest either watch -- or the visible readout -- would have
/// something new to say, or `None` when none of them would.
///
/// `None` from one watch means "as long as you like" and never shortens the
/// other's answer; a caller that took the shorter of `None` and a duration
/// as "no wakeup" would sleep through the one condition that was actually
/// arming a deadline.
fn watch_deadline(wakeups: Wakeups<'_>) -> Option<std::time::Duration> {
    let watches = match (wakeups.write.poll_deadline(), wakeups.read.poll_deadline()) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (only, None) | (None, only) => only,
    };
    match (watches, wakeups.supervision.readout_deadline()) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (only, None) | (None, only) => only,
    }
}

/// Everything that can ask the loop to wake itself up rather than be woken
/// by traffic. All three are read together on every pass, so they travel
/// together: a caller holding two of them and forgetting the third gets a
/// loop that sleeps through the one condition it was arming for.
#[derive(Clone, Copy)]
struct Wakeups<'a> {
    write: &'a OutboxStallWatch,
    read: &'a HeartbeatWatch,
    supervision: &'a SupervisionFold,
}

/// Waits for the loop's next message, bounded by whichever watch has a
/// deadline. `None` means the wait expired with nothing delivered and the
/// caller should re-read both sides of the connection.
///
/// Unbounded whenever neither watch asks for a wakeup, which is the entire
/// idle steady state: an editor with nothing queued and nothing owed an
/// answer sleeps until a keystroke, a redraw or an engine request wakes it,
/// and pays no periodic wakeup for a condition that cannot be true. A
/// deadline exists only while output is pending or a probe is unanswered --
/// moving or wedged, the watches cannot know yet, and for the wedged case
/// the wakeup is the point: a wedged engine emits no redraws, so an
/// operator who types once and then waits would otherwise be told nothing
/// at all.
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
/// over the terminal fd, the SIGWINCH pipe, and the wake pipe, so a
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
        let ready = crate::wake::poll_readiness(input, waker, watch_deadline(wakeups))?;
        if ready.timed_out {
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
fn intake(
    received: Result<Msg, mpsc::RecvError>,
    engine: &mut Engine,
    pump: &view_engine::DamagePump,
    model: &mut Model,
) -> Msg {
    match received {
        Ok(Msg::RedrawReady) => Msg::Redraw(pump.take_damage()),
        Ok(Msg::HeartbeatReply { generation }) => {
            // recorded here rather than in `update()`: this is the runtime
            // loop's own thread, the same one that folds the verdict a pass
            // later, so the acknowledgement and the reading of it are
            // ordered by the loop itself rather than by the memory model
            engine.heartbeat.record_ack(generation);
            Msg::HeartbeatReply { generation }
        }
        Ok(Msg::EngineStopped(reason)) => {
            // stashed on the model rather than reported here: this loop
            // runs behind the terminal's raw-mode alternate screen, so
            // `main` reports it only after `run` returns and the
            // terminal is restored (see Msg::EngineStopped's doc)
            model.fatal_reason = reason;
            // Same bounded (up to `shutdown_timeout`) block as the
            // `Flow::EngineLost` arm in `run`, but the reader thread's
            // stream has already ended by the time this fires, so the
            // `qa!` send is a harmless no-op and the first `try_wait`
            // typically finds the child already exited.
            Msg::EngineDown(engine.wait_exit())
        }
        Ok(m) => m,
        Err(_) => Msg::EngineDown(ExitInfo {
            code: None,
            by_signal: false,
        }),
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
/// Between those wakeups the block is unbounded unless a watch asks
/// otherwise: engine-bound output pending on the write side, or a probe
/// still unanswered on the read side, each bound the sleep by its own
/// deadline (see [`watch_deadline`]).
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
/// [`OSC52_MAX_PAYLOAD_BYTES`] caps the base64-encoded payload at 100 KiB
/// before it ever reaches the sink, so the worst case this pass can add is
/// one syscall pair on a fixed-size buffer, not an unbounded one. A
/// transient write error or an over-cap payload is logged and skipped
/// rather than retried or escalated, so this never turns into a stall the
/// way an engine-bound write can (see `OutboxStallWatch`).
///
/// The read-side liveness watch runs on this thread too, and costs this
/// loop five atomic loads per steady-state pass: three in
/// [`note_engine_liveness`] (the connection's closed flag, plus the sent
/// and acknowledged generations) and two more when [`watch_deadline`] asks
/// the same watch what deadline to arm. No clock read, no lock and no send
/// in that state -- it cannot block on the very connection it is asking
/// about. Its recurring cost is the wakeup rather than the fold: one extra
/// pass every probe interval, ending in a dispatch that produces no effect
/// and no paint.
///
/// # Errors
///
/// Returns the underlying `std::io::Error` if a terminal paint fails (the
/// `Model` is dropped on this path along with everything else on the
/// stack; an aborted session has no last-good theme worth persisting).
#[cfg_attr(not(unix), allow(unused_variables))]
pub fn run(
    mut model: Model,
    mut engine: Engine,
    pump: view_engine::DamagePump,
    msg_channel: MsgChannel,
    inputs: InputHandles<'_>,
    follow_ups: &mut FollowUps<'_>,
    term: &mut Term,
) -> anyhow::Result<(Model, i32)> {
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
    let mut supervision = SupervisionFold::default();
    // frame-to-frame surface reuse; the paint site below is this loop's
    // only consumer, so the cache's previous-frame invariant holds by
    // construction (startup's pre-attach paints predate the loop and go
    // through their own full render)
    let mut surface_cache = view_surface::SurfaceCache::new();

    loop {
        // drained at the top of every pass rather than right after the
        // dispatch that queued it: nothing blocks between here and the
        // bottom of the previous pass's own dispatch loop, so this is
        // effectively immediate, and one drain site covers every dispatch
        // call this loop makes (resize, below, and the main queue) instead
        // of one per call site. On the loop thread, same as `draw_surface`
        // below -- see `run`'s doc for the latency this costs.
        drain_osc52(&osc52_rx, term);
        // a resize the input reader has already seen describes the terminal
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
                Flow::EngineLost | Flow::RestartEngine => {
                    // Blocks this dispatch thread for up to `shutdown_timeout`
                    // (500ms by default -- see `graceful_kill`'s own doc)
                    // sending `qa!` and polling `try_wait`, but only on this
                    // already-rare "the engine connection just failed"
                    // transition, never on the steady-state per-frame path.
                    let info = engine.wait_exit();
                    if let Flow::Quit(code) =
                        dispatch(&mut model, &executor, follow_ups, Msg::EngineDown(info))
                    {
                        return Ok((model, code));
                    }
                }
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
        ) {
            match dispatch(&mut model, &executor, follow_ups, msg) {
                Flow::Continue => {}
                Flow::Quit(code) => return Ok((model, code)),
                // the same bounded teardown the `Msg::Resized` arm above
                // takes, and reached on the same terms: a rare transition,
                // never a per-frame cost
                Flow::EngineLost | Flow::RestartEngine => {
                    let info = engine.wait_exit();
                    if let Flow::Quit(code) =
                        dispatch(&mut model, &executor, follow_ups, Msg::EngineDown(info))
                    {
                        return Ok((model, code));
                    }
                }
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
        #[cfg(unix)]
        let received = wait_for_msg_unified(
            &msg_rx,
            Wakeups {
                write: &write_stall,
                read: &engine.heartbeat,
                supervision: &supervision,
            },
            input,
            &waker,
            &term_size,
            &mut pending,
        )?;
        #[cfg(not(unix))]
        let received = wait_for_msg(
            &msg_rx,
            Wakeups {
                write: &write_stall,
                read: &engine.heartbeat,
                supervision: &supervision,
            },
        );
        let Some(received) = received else {
            // the wait expired against the stall watch's own deadline
            // rather than delivering anything: go around and re-read the
            // write side, which is the whole reason the deadline was armed
            continue;
        };
        #[cfg(feature = "bench-taps")]
        if received.is_ok() {
            view_tui::tap::tap(view_tui::tap::TAG_LOOP_WAKE);
        }
        let msg = intake(received, &mut engine, &pump, &mut model);
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
                // same bounded wait as the `Msg::Resized` arm above (see
                // its comment) -- an already-rare transition, not a
                // per-frame cost
                Flow::EngineLost | Flow::RestartEngine => {
                    queue.push(Msg::EngineDown(engine.wait_exit()));
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
    fn preview_buffer(&self, path: &str, generation: u64) -> Result<(), EngineError> {
        self.record(format!("preview_buffer({path},{generation})"))
    }
    fn open_file(&self, path: &str) -> Result<(), EngineError> {
        self.record(format!("open_file({path})"))
    }
    fn rename_file(
        &self,
        old_path: &str,
        new_path: &str,
        generation: u64,
    ) -> Result<(), EngineError> {
        self.record(format!("rename_file({old_path},{new_path},{generation})"))
    }
    fn tree_create_prompt(&self, generation: u64) -> Result<(), EngineError> {
        self.record(format!("tree_create_prompt({generation})"))
    }
    fn tree_rename_prompt(
        &self,
        old_path: &str,
        current_name: &str,
        generation: u64,
    ) -> Result<(), EngineError> {
        self.record(format!(
            "tree_rename_prompt({old_path},{current_name},{generation})"
        ))
    }
    fn tree_delete_confirm(&self, path: &str, generation: u64) -> Result<(), EngineError> {
        self.record(format!("tree_delete_confirm({path},{generation})"))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// `Messages::visible_lines` returns one span-row per line; these tests
    /// only assert on the text a stall notice carries, so this flattens
    /// each row back to a plain string.
    /// Drives one loop pass's worth of supervision: the same production fold
    /// [`run`] calls, with the message it produces folded through `update()`
    /// the way `dispatch` would. Answers whether that pass changed anything
    /// visible, which is the loop's own cue to repaint.
    fn note_supervision_pass(
        model: &mut Model,
        fold: &mut SupervisionFold,
        write: &mut OutboxStallWatch,
        read: &HeartbeatWatch,
        handle: &EngineHandle,
    ) -> bool {
        let Some(msg) = note_supervision(fold, write, read, handle) else {
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

    /// A recording [`Osc52Sink`] fake, the same shape as `FakeOps` above:
    /// `drain_osc52`'s cap/skip/write logic is driven against this instead
    /// of a real terminal, so the 100 KiB cap and the skip-and-log path are
    /// provable with no stdout write and no sleep.
    #[derive(Default)]
    struct FakeOsc52Sink {
        writes: Vec<(char, String)>,
    }

    impl Osc52Sink for FakeOsc52Sink {
        fn write_osc52(&mut self, register: char, text: &str) -> std::io::Result<()> {
            self.writes.push((register, text.to_owned()));
            Ok(())
        }
    }

    /// One raw byte short of the base64-expanded cap
    /// (`div_ceil(3) * 4 == OSC52_MAX_PAYLOAD_BYTES` exactly at this
    /// length): the boundary case `encoded_len > OSC52_MAX_PAYLOAD_BYTES`
    /// must not reject, since `>` (not `>=`) is the cap's own contract.
    fn at_cap_text() -> String {
        // OSC52_MAX_PAYLOAD_BYTES is a multiple of 4, so 3/4 of it is a
        // whole number of raw bytes whose base64 expansion lands exactly
        // on the cap with no remainder to round up
        "a".repeat(OSC52_MAX_PAYLOAD_BYTES / 4 * 3)
    }

    #[test]
    fn an_at_cap_payload_is_written_whole() {
        let (tx, rx) = mpsc::channel();
        let text = at_cap_text();
        let encoded_len = text.len().div_ceil(3) * 4;
        assert_eq!(
            encoded_len, OSC52_MAX_PAYLOAD_BYTES,
            "fixture must sit exactly at the cap, not merely under it"
        );
        tx.send(Osc52Job {
            register: '+',
            lines: vec![text.clone()],
            regtype: RegisterType::Charwise,
        })
        .unwrap();

        let mut sink = FakeOsc52Sink::default();
        drain_osc52(&rx, &mut sink);

        assert_eq!(sink.writes.len(), 1, "an at-cap payload must be written");
        assert_eq!(sink.writes[0].0, '+');
        assert_eq!(
            sink.writes[0].1, text,
            "the written text must be whole, not truncated"
        );
    }

    #[test]
    fn an_over_cap_payload_is_skipped_with_no_write_attempted() {
        let (tx, rx) = mpsc::channel();
        // one raw byte past `at_cap_text`'s length pushes the base64
        // expansion strictly over the cap
        let text = "a".repeat(OSC52_MAX_PAYLOAD_BYTES / 4 * 3 + 1);
        let encoded_len = text.len().div_ceil(3) * 4;
        assert!(
            encoded_len > OSC52_MAX_PAYLOAD_BYTES,
            "fixture must sit strictly over the cap"
        );
        tx.send(Osc52Job {
            register: '+',
            lines: vec![text],
            regtype: RegisterType::Charwise,
        })
        .unwrap();

        let mut sink = FakeOsc52Sink::default();
        drain_osc52(&rx, &mut sink);

        assert!(
            sink.writes.is_empty(),
            "an over-cap payload must never reach the sink, truncated or otherwise"
        );
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

    /// The write side's precedence is a precedence, not a preference: a
    /// writer that is demonstrably moving cannot be the wedge, so the read
    /// side's verdict stands on its own.
    #[test]
    fn a_moving_writer_leaves_the_read_sides_verdict_alone() {
        assert_eq!(
            wedge_kind(false, view_engine::heartbeat::Liveness::Wedged),
            Some(WedgeKind::ReadSide)
        );
        assert_eq!(
            wedge_kind(true, view_engine::heartbeat::Liveness::Alive),
            Some(WedgeKind::WriteSide)
        );
        assert_eq!(
            wedge_kind(false, view_engine::heartbeat::Liveness::Alive),
            None
        );
        // and a closed connection outranks both, since neither side can
        // recover one
        assert_eq!(
            wedge_kind(true, view_engine::heartbeat::Liveness::Dead),
            Some(WedgeKind::Dead)
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

    #[test]
    fn an_idle_session_arms_no_deadline_and_is_never_woken_early() {
        let mut peer = WedgedPeer::new();
        // a peer that reads normally, expressed through the same sink as
        // the wedged one: healthy and wedged differ by this one call
        peer.release();
        let mut model = Model::with_term_size(80, 24);
        let mut watch = OutboxStallWatch::new(TEST_STALL_THRESHOLD);
        let mut fold = SupervisionFold::default();
        // armed, but nothing has probed it, so neither side of the
        // connection is owed anything that a wakeup could report on
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
            watch_deadline(Wakeups {
                write: &watch,
                read: &heartbeat,
                supervision: &fold,
            }),
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
        let received = wait_for_msg(
            &msg_rx,
            Wakeups {
                write: &watch,
                read: &heartbeat,
                supervision: &fold,
            },
        );
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
            note_supervision(&mut fold, &mut watch, &heartbeat, &peer.handle);
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
                ) == Liveness::Alive;
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
