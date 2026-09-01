//! The operation surface the runtime calls the engine through.
//!
//! Factored out of the loop that drives it so the effect-to-call mapping is
//! testable against a recording fake instead of a live nvim connection, and
//! so growing the surface never grows the loop's own file.

use view_core::msg::{
    BufferHandle, HunkMark, OptionValue, ReplyToken, ReplyValue, ReviewOpenTarget, TextEdit,
};
use view_core::native::ai_context::{
    CurrentBufferRead, CursorRead, DiagnosticEntry, QuickfixEntry, SelectionRead,
};
use view_core::native::mappings::MappingSpec;
use view_engine::handle::{EngineError, EngineHandle};
use view_engine::nvim_api::BufWriteOutcome;

/// The notify surface [`crate::runtime::Executor`] drives, factored out
/// from [`EngineHandle`] so it can be faked. The four `read_*` methods are
/// the one exception to "notify": each is a synchronous, bounded-timeout
/// RPC request (see `EngineHandle::read_current_buffer_text`'s own doc),
/// never fire-and-forget -- callers must issue them only off the loop
/// thread (`crate::ai_context_worker`'s own doc explains why), the same way
/// every other blocking call in this crate stays off it.
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
    /// Re-points `vim.notify` at the engine default and keeps it there for
    /// the session, so a plugin's messages cross as `ext_messages` traffic
    /// (see `RpcCall::HoldNotify`).
    fn hold_notify(&self) -> Result<(), EngineError>;
    /// Answers a request nvim is blocked on.
    fn reply(&self, token: ReplyToken, value: ReplyValue) -> Result<(), EngineError>;
    /// Issues an async `nvim_get_hl(0, {name = "Normal"})` probe tagged
    /// with `generation`; never blocks, and never itself returns the reply
    /// (see `Msg::HlProbeReply`).
    fn probe_default_hl(&self, generation: u64) -> Result<(), EngineError>;
    /// Issues an async read of what this engine recovered while starting,
    /// tagged `generation`; never blocks, and never itself returns the
    /// reading (see `Msg::SwapRecovered`).
    fn probe_swap_recovery(&self, generation: u64) -> Result<(), EngineError>;
    /// Asks nvim to repaint from scratch and retract the messages it has
    /// shown (see `RpcCall::Redraw`).
    fn redraw(&self) -> Result<(), EngineError>;
    /// Declares this UI's stdout a real terminal, starting `ui_send`
    /// delivery (see `RpcCall::ClaimStdoutTty`).
    fn claim_stdout_tty(&self) -> Result<(), EngineError>;
    /// Hands nvim a terminal response sequence as if the host terminal had
    /// sent it, firing `TermResponse` (see
    /// `EngineHandle::ui_term_event`). The channel view's answer to an OSC
    /// 52 read query rides (see `Effect::ClipboardQuery`); never blocks,
    /// and nothing reads a result.
    fn ui_term_event(&self, sequence: &str) -> Result<(), EngineError>;
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
    /// Writes one floating window's own `hide` flag, for the completion
    /// float view absorbs into the palette and hands back;
    /// fire-and-forget, no reply (see `RpcCall::SetFloatHidden`).
    fn set_float_hidden(&self, win: u64, hide: bool) -> Result<(), EngineError>;
    /// Reads an absorbed float's rows and selection; never blocks, and
    /// never itself returns the answer (see `RpcCall::ReadFloatRows`,
    /// `Msg::FloatRows`).
    fn read_float_rows(&self, win: u64) -> Result<(), EngineError>;
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
    /// Applies `edits` to `buf` via `nvim_buf_set_text`, the only path that
    /// ever writes agent-proposed text (see `RpcCall::BufSetText`'s own doc
    /// for the per-hunk undo contract `undojoin` implements). Explicitly
    /// matched in `Executor::run` rather than falling through
    /// `RpcCall`'s `#[non_exhaustive]` catch-all: unlike every other call
    /// here, a silently no-op'd write would drop a buffer edit the user
    /// already accepted, not just skip a read or a prompt.
    fn set_buf_text(
        &self,
        buf: BufferHandle,
        edits: &[TextEdit],
        undojoin: bool,
        expected_changedtick: Option<u64>,
    ) -> Result<BufWriteOutcome, EngineError>;
    /// Subscribes to `buf`'s live edit stream, tagged `generation`; never
    /// blocks, and never itself returns an event (see `RpcCall::BufAttach`,
    /// `Msg::BufTextChanged`).
    fn buf_attach(&self, buf: BufferHandle, generation: u64) -> Result<(), EngineError>;
    /// Unsubscribes from `buf`'s edit stream (see `RpcCall::BufDetach`).
    fn buf_detach(&self, buf: BufferHandle) -> Result<(), EngineError>;
    /// Draws the whole open review inside `buf` -- every open hunk's
    /// deletions, its proposed lines, and the review's buffer-local keys --
    /// taking the cursor there when `focus` asks (see
    /// `RpcCall::ReviewShow`). Fire-and-forget: nothing correlates a reply,
    /// which is what lets the loop emit it on the paint path.
    fn review_show(
        &self,
        buf: BufferHandle,
        marks: &[HunkMark],
        cursor_row: u32,
        focus: bool,
        open_target: ReviewOpenTarget,
    ) -> Result<(), EngineError>;
    /// Takes the review's decoration and its keys back off `buf` (see
    /// `RpcCall::ReviewClear`).
    fn review_clear(&self, buf: BufferHandle) -> Result<(), EngineError>;
    /// Loads `path` into an unlisted hidden buffer (reusing an already-open
    /// one for the same path) and takes a hold on it, tagged `generation`;
    /// never blocks, and answers `Msg::HiddenBufferLoaded` (see
    /// `RpcCall::LoadHidden`).
    ///
    /// A path
    /// [`view_engine::nvim_api::hidden_path_refusal`](view_engine::nvim_api::hidden_path_refusal)
    /// refuses answers `EngineError::UnusablePath` instead, before taking a
    /// hold and before anything reaches the wire -- a refusal of the path,
    /// never a lost connection, which the runtime stands in for with a
    /// buffer-less `Msg::HiddenBufferLoaded`. Every implementation owes
    /// that refusal, fakes included, or the caller cannot be tested against
    /// the contract the real handle enforces.
    fn load_hidden(&self, path: &str, generation: u64) -> Result<(), EngineError>;
    /// Releases one hold taken by `load_hidden` on `path`; deletes the
    /// hidden buffer only when its hold count reaches zero, and never on a
    /// buffer a window still shows or that still has unsaved edits (see
    /// `RpcCall::ReleaseHidden`).
    fn release_hidden(&self, path: &str) -> Result<(), EngineError>;
    /// Reads `buf`'s text -- optionally only the `limit` lines starting at
    /// 1-based `line` -- to answer an agent's `fs/read_text_file`; never
    /// blocks, and answers `Msg::AiFsReadReply` (see `RpcCall::AiFsRead`).
    ///
    /// The buffer, not the file on disk, is the text: an agent reading a
    /// path the user is editing must see the unsaved edits, which is what
    /// the ACP method's own wording requires.
    fn ai_fs_read(
        &self,
        request_id: u64,
        buf: BufferHandle,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<(), EngineError>;
    /// Replaces `buf`'s text with `lines` and saves it to answer an agent's
    /// `fs/write_text_file`, refusing if `buf` has changed past
    /// `expected_changedtick`; never blocks, and answers
    /// `Msg::AiFsWriteReply` (see `RpcCall::AiFsWrite`).
    ///
    /// `eol` carries the one thing a line list cannot: whether the file
    /// ends with a newline, which nvim decides from `endofline` and
    /// `fixendofline` rather than from the lines themselves.
    fn ai_fs_write(
        &self,
        request_id: u64,
        buf: BufferHandle,
        lines: &[String],
        eol: bool,
        expected_changedtick: u64,
    ) -> Result<(), EngineError>;
    /// Drives nvim's own out-of-band-write disposition for every path in
    /// `paths`; never blocks, and answers `Msg::CheckTimeReply` (see
    /// `RpcCall::Checktime`).
    ///
    /// `force: false` is the out-of-band write watcher's own probe;
    /// `force: true` drives the explicit reload behind the conflict
    /// prompt's "reload, discard local edits" answer (see
    /// `view_engine::nvim_api::EngineHandle::checktime`'s own doc for why
    /// the two are not the same request replayed).
    fn checktime(&self, request_id: u64, paths: &[String], force: bool) -> Result<(), EngineError>;
    /// Reads the current buffer's path and nvim-authoritative text (see
    /// `EngineHandle::read_current_buffer_text`). Synchronous and
    /// bounded-timeout, not fire-and-forget -- see this trait's own doc.
    fn read_current_buffer_text(&self) -> Result<CurrentBufferRead, EngineError>;
    /// Reads the buffer-space cursor and, when one is active, the visual
    /// selection (see `EngineHandle::read_cursor_context`). Synchronous and
    /// bounded-timeout -- see this trait's own doc.
    fn read_cursor_context(&self) -> Result<(CursorRead, Option<SelectionRead>), EngineError>;
    /// Reads every current `vim.diagnostic.get(0)` entry (see
    /// `EngineHandle::read_diagnostic_entries`). Synchronous and
    /// bounded-timeout -- see this trait's own doc.
    fn read_diagnostic_entries(&self) -> Result<Vec<DiagnosticEntry>, EngineError>;
    /// Reads every current `getqflist()` entry (see
    /// `EngineHandle::read_quickfix_entries`). Synchronous and
    /// bounded-timeout -- see this trait's own doc.
    fn read_quickfix_entries(&self) -> Result<Vec<QuickfixEntry>, EngineError>;
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
    fn hold_notify(&self) -> Result<(), EngineError> {
        self.hold_notify()
    }
    fn reply(&self, token: ReplyToken, value: ReplyValue) -> Result<(), EngineError> {
        self.reply(token, value)
    }
    fn probe_default_hl(&self, generation: u64) -> Result<(), EngineError> {
        self.probe_default_hl(generation)
    }
    fn probe_swap_recovery(&self, generation: u64) -> Result<(), EngineError> {
        self.probe_swap_recovery(generation)
    }
    fn redraw(&self) -> Result<(), EngineError> {
        self.redraw()
    }
    fn claim_stdout_tty(&self) -> Result<(), EngineError> {
        self.claim_stdout_tty()
    }
    fn ui_term_event(&self, sequence: &str) -> Result<(), EngineError> {
        self.ui_term_event(sequence)
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
    fn set_float_hidden(&self, win: u64, hide: bool) -> Result<(), EngineError> {
        self.set_float_hidden(win, hide)
    }
    fn read_float_rows(&self, win: u64) -> Result<(), EngineError> {
        self.read_float_rows(win)
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
    fn set_buf_text(
        &self,
        buf: BufferHandle,
        edits: &[TextEdit],
        undojoin: bool,
        expected_changedtick: Option<u64>,
    ) -> Result<BufWriteOutcome, EngineError> {
        self.set_buf_text(buf, edits, undojoin, expected_changedtick)
    }
    fn buf_attach(&self, buf: BufferHandle, generation: u64) -> Result<(), EngineError> {
        self.buf_attach(buf, generation)
    }
    fn buf_detach(&self, buf: BufferHandle) -> Result<(), EngineError> {
        self.buf_detach(buf)
    }
    fn review_show(
        &self,
        buf: BufferHandle,
        marks: &[HunkMark],
        cursor_row: u32,
        focus: bool,
        open_target: ReviewOpenTarget,
    ) -> Result<(), EngineError> {
        self.review_show(buf, marks, cursor_row, focus, open_target)
    }
    fn review_clear(&self, buf: BufferHandle) -> Result<(), EngineError> {
        self.review_clear(buf)
    }
    fn load_hidden(&self, path: &str, generation: u64) -> Result<(), EngineError> {
        self.load_hidden(path, generation)
    }
    fn release_hidden(&self, path: &str) -> Result<(), EngineError> {
        self.release_hidden(path)
    }
    fn ai_fs_read(
        &self,
        request_id: u64,
        buf: BufferHandle,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<(), EngineError> {
        self.ai_fs_read(request_id, buf, line, limit)
    }
    fn ai_fs_write(
        &self,
        request_id: u64,
        buf: BufferHandle,
        lines: &[String],
        eol: bool,
        expected_changedtick: u64,
    ) -> Result<(), EngineError> {
        self.ai_fs_write(request_id, buf, lines, eol, expected_changedtick)
    }
    fn checktime(&self, request_id: u64, paths: &[String], force: bool) -> Result<(), EngineError> {
        self.checktime(request_id, paths, force)
    }
    fn read_current_buffer_text(&self) -> Result<CurrentBufferRead, EngineError> {
        self.read_current_buffer_text()
    }
    fn read_cursor_context(&self) -> Result<(CursorRead, Option<SelectionRead>), EngineError> {
        self.read_cursor_context()
    }
    fn read_diagnostic_entries(&self) -> Result<Vec<DiagnosticEntry>, EngineError> {
        self.read_diagnostic_entries()
    }
    fn read_quickfix_entries(&self) -> Result<Vec<QuickfixEntry>, EngineError> {
        self.read_quickfix_entries()
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
    fn hold_notify(&self) -> Result<(), EngineError> {
        (**self).hold_notify()
    }
    fn reply(&self, token: ReplyToken, value: ReplyValue) -> Result<(), EngineError> {
        (**self).reply(token, value)
    }
    fn probe_default_hl(&self, generation: u64) -> Result<(), EngineError> {
        (**self).probe_default_hl(generation)
    }
    fn probe_swap_recovery(&self, generation: u64) -> Result<(), EngineError> {
        (**self).probe_swap_recovery(generation)
    }
    fn redraw(&self) -> Result<(), EngineError> {
        (**self).redraw()
    }
    fn claim_stdout_tty(&self) -> Result<(), EngineError> {
        (**self).claim_stdout_tty()
    }
    fn ui_term_event(&self, sequence: &str) -> Result<(), EngineError> {
        (**self).ui_term_event(sequence)
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
    fn set_float_hidden(&self, win: u64, hide: bool) -> Result<(), EngineError> {
        (**self).set_float_hidden(win, hide)
    }
    fn read_float_rows(&self, win: u64) -> Result<(), EngineError> {
        (**self).read_float_rows(win)
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
    fn set_buf_text(
        &self,
        buf: BufferHandle,
        edits: &[TextEdit],
        undojoin: bool,
        expected_changedtick: Option<u64>,
    ) -> Result<BufWriteOutcome, EngineError> {
        (**self).set_buf_text(buf, edits, undojoin, expected_changedtick)
    }
    fn buf_attach(&self, buf: BufferHandle, generation: u64) -> Result<(), EngineError> {
        (**self).buf_attach(buf, generation)
    }
    fn buf_detach(&self, buf: BufferHandle) -> Result<(), EngineError> {
        (**self).buf_detach(buf)
    }
    fn review_show(
        &self,
        buf: BufferHandle,
        marks: &[HunkMark],
        cursor_row: u32,
        focus: bool,
        open_target: ReviewOpenTarget,
    ) -> Result<(), EngineError> {
        (**self).review_show(buf, marks, cursor_row, focus, open_target)
    }
    fn review_clear(&self, buf: BufferHandle) -> Result<(), EngineError> {
        (**self).review_clear(buf)
    }
    fn load_hidden(&self, path: &str, generation: u64) -> Result<(), EngineError> {
        (**self).load_hidden(path, generation)
    }
    fn release_hidden(&self, path: &str) -> Result<(), EngineError> {
        (**self).release_hidden(path)
    }
    fn ai_fs_read(
        &self,
        request_id: u64,
        buf: BufferHandle,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<(), EngineError> {
        (**self).ai_fs_read(request_id, buf, line, limit)
    }
    fn ai_fs_write(
        &self,
        request_id: u64,
        buf: BufferHandle,
        lines: &[String],
        eol: bool,
        expected_changedtick: u64,
    ) -> Result<(), EngineError> {
        (**self).ai_fs_write(request_id, buf, lines, eol, expected_changedtick)
    }
    fn checktime(&self, request_id: u64, paths: &[String], force: bool) -> Result<(), EngineError> {
        (**self).checktime(request_id, paths, force)
    }
    fn read_current_buffer_text(&self) -> Result<CurrentBufferRead, EngineError> {
        (**self).read_current_buffer_text()
    }
    fn read_cursor_context(&self) -> Result<(CursorRead, Option<SelectionRead>), EngineError> {
        (**self).read_cursor_context()
    }
    fn read_diagnostic_entries(&self) -> Result<Vec<DiagnosticEntry>, EngineError> {
        (**self).read_diagnostic_entries()
    }
    fn read_quickfix_entries(&self) -> Result<Vec<QuickfixEntry>, EngineError> {
        (**self).read_quickfix_entries()
    }
}

/// The same delegation as the `&T` blanket above, over `Rc` instead of a
/// borrow: generic code written against `E: EngineOps + Clone`
/// (`ai_context_worker::OpsRoute`) can wrap any single-owner fake in an
/// `Rc` to get a cheap, state-sharing `Clone` for it, without that fake
/// having to give up its own single-owner `Clone` contract (see
/// `FakeOps`'s own doc) or this trait growing a duplicate implementation
/// for every type that only ever needs sharing in a test.
impl<T: EngineOps + ?Sized> EngineOps for std::rc::Rc<T> {
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
    fn hold_notify(&self) -> Result<(), EngineError> {
        (**self).hold_notify()
    }
    fn reply(&self, token: ReplyToken, value: ReplyValue) -> Result<(), EngineError> {
        (**self).reply(token, value)
    }
    fn probe_default_hl(&self, generation: u64) -> Result<(), EngineError> {
        (**self).probe_default_hl(generation)
    }
    fn probe_swap_recovery(&self, generation: u64) -> Result<(), EngineError> {
        (**self).probe_swap_recovery(generation)
    }
    fn redraw(&self) -> Result<(), EngineError> {
        (**self).redraw()
    }
    fn claim_stdout_tty(&self) -> Result<(), EngineError> {
        (**self).claim_stdout_tty()
    }
    fn ui_term_event(&self, sequence: &str) -> Result<(), EngineError> {
        (**self).ui_term_event(sequence)
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
    fn set_float_hidden(&self, win: u64, hide: bool) -> Result<(), EngineError> {
        (**self).set_float_hidden(win, hide)
    }
    fn read_float_rows(&self, win: u64) -> Result<(), EngineError> {
        (**self).read_float_rows(win)
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
    fn set_buf_text(
        &self,
        buf: BufferHandle,
        edits: &[TextEdit],
        undojoin: bool,
        expected_changedtick: Option<u64>,
    ) -> Result<BufWriteOutcome, EngineError> {
        (**self).set_buf_text(buf, edits, undojoin, expected_changedtick)
    }
    fn buf_attach(&self, buf: BufferHandle, generation: u64) -> Result<(), EngineError> {
        (**self).buf_attach(buf, generation)
    }
    fn buf_detach(&self, buf: BufferHandle) -> Result<(), EngineError> {
        (**self).buf_detach(buf)
    }
    fn review_show(
        &self,
        buf: BufferHandle,
        marks: &[HunkMark],
        cursor_row: u32,
        focus: bool,
        open_target: ReviewOpenTarget,
    ) -> Result<(), EngineError> {
        (**self).review_show(buf, marks, cursor_row, focus, open_target)
    }
    fn review_clear(&self, buf: BufferHandle) -> Result<(), EngineError> {
        (**self).review_clear(buf)
    }
    fn load_hidden(&self, path: &str, generation: u64) -> Result<(), EngineError> {
        (**self).load_hidden(path, generation)
    }
    fn release_hidden(&self, path: &str) -> Result<(), EngineError> {
        (**self).release_hidden(path)
    }
    fn ai_fs_read(
        &self,
        request_id: u64,
        buf: BufferHandle,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<(), EngineError> {
        (**self).ai_fs_read(request_id, buf, line, limit)
    }
    fn ai_fs_write(
        &self,
        request_id: u64,
        buf: BufferHandle,
        lines: &[String],
        eol: bool,
        expected_changedtick: u64,
    ) -> Result<(), EngineError> {
        (**self).ai_fs_write(request_id, buf, lines, eol, expected_changedtick)
    }
    fn checktime(&self, request_id: u64, paths: &[String], force: bool) -> Result<(), EngineError> {
        (**self).checktime(request_id, paths, force)
    }
    fn read_current_buffer_text(&self) -> Result<CurrentBufferRead, EngineError> {
        (**self).read_current_buffer_text()
    }
    fn read_cursor_context(&self) -> Result<(CursorRead, Option<SelectionRead>), EngineError> {
        (**self).read_cursor_context()
    }
    fn read_diagnostic_entries(&self) -> Result<Vec<DiagnosticEntry>, EngineError> {
        (**self).read_diagnostic_entries()
    }
    fn read_quickfix_entries(&self) -> Result<Vec<QuickfixEntry>, EngineError> {
        (**self).read_quickfix_entries()
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
    /// Makes the next `set_buf_text` answer `BufferAdvanced` -- nvim
    /// refusing a write whose named tick the buffer has moved past, which
    /// is a routed message rather than a failure.
    pub(crate) refuse_next_write: std::cell::RefCell<bool>,
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
    fn hold_notify(&self) -> Result<(), EngineError> {
        self.record("hold_notify()".to_string())
    }
    fn reply(&self, token: ReplyToken, value: ReplyValue) -> Result<(), EngineError> {
        self.record(format!("reply({},{value:?})", token.msgid))
    }
    fn probe_default_hl(&self, generation: u64) -> Result<(), EngineError> {
        self.record(format!("probe_default_hl({generation})"))
    }
    fn probe_swap_recovery(&self, generation: u64) -> Result<(), EngineError> {
        self.record(format!("probe_swap_recovery({generation})"))
    }
    fn redraw(&self) -> Result<(), EngineError> {
        self.record("redraw()".to_string())
    }
    fn claim_stdout_tty(&self) -> Result<(), EngineError> {
        self.record("claim_stdout_tty()".to_string())
    }
    fn ui_term_event(&self, sequence: &str) -> Result<(), EngineError> {
        self.record(format!("ui_term_event({sequence:?})"))
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
    fn set_float_hidden(&self, win: u64, hide: bool) -> Result<(), EngineError> {
        self.record(format!("set_float_hidden({win},{hide})"))
    }
    fn read_float_rows(&self, win: u64) -> Result<(), EngineError> {
        self.record(format!("read_float_rows({win})"))
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
    fn set_buf_text(
        &self,
        buf: BufferHandle,
        edits: &[TextEdit],
        undojoin: bool,
        expected_changedtick: Option<u64>,
    ) -> Result<BufWriteOutcome, EngineError> {
        self.record(format!(
            "set_buf_text({},{},{undojoin},{expected_changedtick:?})",
            buf.0,
            edits.len()
        ))?;
        if *self.refuse_next_write.borrow() {
            Ok(BufWriteOutcome::BufferAdvanced)
        } else {
            Ok(BufWriteOutcome::Applied { changedtick: 0 })
        }
    }
    fn buf_attach(&self, buf: BufferHandle, generation: u64) -> Result<(), EngineError> {
        self.record(format!("buf_attach({},{generation})", buf.0))
    }
    fn buf_detach(&self, buf: BufferHandle) -> Result<(), EngineError> {
        self.record(format!("buf_detach({})", buf.0))
    }
    fn review_show(
        &self,
        buf: BufferHandle,
        marks: &[HunkMark],
        cursor_row: u32,
        focus: bool,
        open_target: ReviewOpenTarget,
    ) -> Result<(), EngineError> {
        self.record(format!(
            "review_show({},{},{cursor_row},{focus},{open_target:?})",
            buf.0,
            marks.len()
        ))
    }
    fn review_clear(&self, buf: BufferHandle) -> Result<(), EngineError> {
        self.record(format!("review_clear({})", buf.0))
    }
    fn load_hidden(&self, path: &str, generation: u64) -> Result<(), EngineError> {
        if let Some(reason) = view_engine::nvim_api::hidden_path_refusal(path) {
            return Err(EngineError::UnusablePath {
                path: path.to_owned(),
                reason,
            });
        }
        self.record(format!("load_hidden({path},{generation})"))
    }
    fn release_hidden(&self, path: &str) -> Result<(), EngineError> {
        self.record(format!("release_hidden({path})"))
    }
    fn ai_fs_read(
        &self,
        request_id: u64,
        buf: BufferHandle,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<(), EngineError> {
        self.record(format!(
            "ai_fs_read({request_id},{},{line:?},{limit:?})",
            buf.0
        ))
    }
    fn ai_fs_write(
        &self,
        request_id: u64,
        buf: BufferHandle,
        lines: &[String],
        eol: bool,
        expected_changedtick: u64,
    ) -> Result<(), EngineError> {
        self.record(format!(
            "ai_fs_write({request_id},{},{},{eol},{expected_changedtick})",
            buf.0,
            lines.len()
        ))
    }
    fn checktime(&self, request_id: u64, paths: &[String], force: bool) -> Result<(), EngineError> {
        self.record(format!(
            "checktime({request_id},{},{force})",
            paths.join("|")
        ))
    }
    fn read_current_buffer_text(&self) -> Result<CurrentBufferRead, EngineError> {
        self.record("read_current_buffer_text()".to_string())
            .map(|()| CurrentBufferRead::new(std::path::PathBuf::new(), String::new()))
    }
    fn read_cursor_context(&self) -> Result<(CursorRead, Option<SelectionRead>), EngineError> {
        self.record("read_cursor_context()".to_string())
            .map(|()| (CursorRead::new(0, 0), None))
    }
    fn read_diagnostic_entries(&self) -> Result<Vec<DiagnosticEntry>, EngineError> {
        self.record("read_diagnostic_entries()".to_string())
            .map(|()| Vec::new())
    }
    fn read_quickfix_entries(&self) -> Result<Vec<QuickfixEntry>, EngineError> {
        self.record("read_quickfix_entries()".to_string())
            .map(|()| Vec::new())
    }
}

/// A `Send`-capable [`EngineOps`] fixture whose four `read_*` methods sleep
/// `delay` before returning a trivial default, everything else returning
/// `Ok(())` immediately -- the seam an off-loop-property test blocks on to
/// prove a caller genuinely returns before the read finishes, rather than
/// merely finishing fast because the read itself was fast.
///
/// [`FakeOps`] cannot serve this: its `RefCell` call log makes it neither
/// `Send` nor safely `Clone`-shareable across threads, which
/// `ai_context_worker::spawn`'s `E: EngineOps + Clone + Send + 'static`
/// bound requires. `SlowOps` carries no call log at all -- a plain `Clone`,
/// `Copy`-able `Duration` -- because the property under test is purely
/// timing (does the caller return before the read resolves), not which
/// calls were made.
#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) struct SlowOps {
    delay: std::time::Duration,
}

#[cfg(test)]
impl SlowOps {
    /// A fixture whose reads block for `delay` before returning.
    pub(crate) fn new(delay: std::time::Duration) -> Self {
        Self { delay }
    }
}

#[cfg(test)]
impl EngineOps for SlowOps {
    fn input(&self, _notation: &str) -> Result<(), EngineError> {
        Ok(())
    }
    fn try_resize(&self, _width: u16, _height: u16) -> Result<(), EngineError> {
        Ok(())
    }
    fn paste(&self, _text: &str) -> Result<(), EngineError> {
        Ok(())
    }
    fn input_mouse(
        &self,
        _button: &str,
        _action: &str,
        _modifier: &str,
        _row: u16,
        _col: u16,
    ) -> Result<(), EngineError> {
        Ok(())
    }
    fn set_option(&self, _name: &str, _value: &OptionValue) -> Result<(), EngineError> {
        Ok(())
    }
    fn hold_option(&self, _name: &str, _value: &OptionValue) -> Result<(), EngineError> {
        Ok(())
    }
    fn hold_notify(&self) -> Result<(), EngineError> {
        Ok(())
    }
    fn reply(&self, _token: ReplyToken, _value: ReplyValue) -> Result<(), EngineError> {
        Ok(())
    }
    fn probe_default_hl(&self, _generation: u64) -> Result<(), EngineError> {
        Ok(())
    }
    fn probe_swap_recovery(&self, _generation: u64) -> Result<(), EngineError> {
        Ok(())
    }
    fn redraw(&self) -> Result<(), EngineError> {
        Ok(())
    }
    fn claim_stdout_tty(&self) -> Result<(), EngineError> {
        Ok(())
    }
    fn ui_term_event(&self, _sequence: &str) -> Result<(), EngineError> {
        Ok(())
    }
    fn register_mappings(
        &self,
        _specs: &[MappingSpec],
        _channel_id: u64,
    ) -> Result<(), EngineError> {
        Ok(())
    }
    fn register_bridge(&self, _channel_id: u64) -> Result<(), EngineError> {
        Ok(())
    }
    fn register_clipboard(&self, _channel_id: u64) -> Result<(), EngineError> {
        Ok(())
    }
    fn list_buffers(&self, _generation: u64) -> Result<(), EngineError> {
        Ok(())
    }
    fn preview_buffer(&self, _path: &str, _generation: u64) -> Result<(), EngineError> {
        Ok(())
    }
    fn set_float_hidden(&self, _win: u64, _hide: bool) -> Result<(), EngineError> {
        Ok(())
    }
    fn read_float_rows(&self, _win: u64) -> Result<(), EngineError> {
        Ok(())
    }
    fn open_file(&self, _path: &str) -> Result<(), EngineError> {
        Ok(())
    }
    fn rename_file(
        &self,
        _old_path: &str,
        _new_path: &str,
        _generation: u64,
    ) -> Result<(), EngineError> {
        Ok(())
    }
    fn tree_create_prompt(&self, _generation: u64) -> Result<(), EngineError> {
        Ok(())
    }
    fn tree_rename_prompt(
        &self,
        _old_path: &str,
        _current_name: &str,
        _generation: u64,
    ) -> Result<(), EngineError> {
        Ok(())
    }
    fn tree_delete_confirm(&self, _path: &str, _generation: u64) -> Result<(), EngineError> {
        Ok(())
    }
    fn set_buf_text(
        &self,
        _buf: BufferHandle,
        _edits: &[TextEdit],
        _undojoin: bool,
        _expected_changedtick: Option<u64>,
    ) -> Result<BufWriteOutcome, EngineError> {
        Ok(BufWriteOutcome::Applied { changedtick: 0 })
    }
    fn buf_attach(&self, _buf: BufferHandle, _generation: u64) -> Result<(), EngineError> {
        Ok(())
    }
    fn buf_detach(&self, _buf: BufferHandle) -> Result<(), EngineError> {
        Ok(())
    }
    fn review_show(
        &self,
        _buf: BufferHandle,
        _marks: &[HunkMark],
        _cursor_row: u32,
        _focus: bool,
        _open_target: ReviewOpenTarget,
    ) -> Result<(), EngineError> {
        Ok(())
    }
    fn review_clear(&self, _buf: BufferHandle) -> Result<(), EngineError> {
        Ok(())
    }
    fn load_hidden(&self, _path: &str, _generation: u64) -> Result<(), EngineError> {
        Ok(())
    }
    fn release_hidden(&self, _path: &str) -> Result<(), EngineError> {
        Ok(())
    }
    fn ai_fs_read(
        &self,
        _request_id: u64,
        _buf: BufferHandle,
        _line: Option<u32>,
        _limit: Option<u32>,
    ) -> Result<(), EngineError> {
        Ok(())
    }
    fn ai_fs_write(
        &self,
        _request_id: u64,
        _buf: BufferHandle,
        _lines: &[String],
        _eol: bool,
        _expected_changedtick: u64,
    ) -> Result<(), EngineError> {
        Ok(())
    }
    fn checktime(
        &self,
        _request_id: u64,
        _paths: &[String],
        _force: bool,
    ) -> Result<(), EngineError> {
        Ok(())
    }
    fn read_current_buffer_text(&self) -> Result<CurrentBufferRead, EngineError> {
        std::thread::sleep(self.delay);
        Ok(CurrentBufferRead::new(
            std::path::PathBuf::new(),
            String::new(),
        ))
    }
    fn read_cursor_context(&self) -> Result<(CursorRead, Option<SelectionRead>), EngineError> {
        std::thread::sleep(self.delay);
        Ok((CursorRead::new(0, 0), None))
    }
    fn read_diagnostic_entries(&self) -> Result<Vec<DiagnosticEntry>, EngineError> {
        std::thread::sleep(self.delay);
        Ok(Vec::new())
    }
    fn read_quickfix_entries(&self) -> Result<Vec<QuickfixEntry>, EngineError> {
        std::thread::sleep(self.delay);
        Ok(Vec::new())
    }
}
