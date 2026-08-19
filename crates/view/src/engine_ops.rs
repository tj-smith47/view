//! The operation surface the runtime calls the engine through.
//!
//! Factored out of the loop that drives it so the effect-to-call mapping is
//! testable against a recording fake instead of a live nvim connection, and
//! so growing the surface never grows the loop's own file.

use view_core::msg::{BufferHandle, OptionValue, ReplyToken, ReplyValue, TextEdit};
use view_core::native::mappings::MappingSpec;
use view_engine::handle::{EngineError, EngineHandle};

/// The notify surface [`crate::runtime::Executor`] drives, factored out
/// from [`EngineHandle`] so it can be faked.
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
    /// Issues an async read of what this engine recovered while starting,
    /// tagged `generation`; never blocks, and never itself returns the
    /// reading (see `Msg::SwapRecovered`).
    fn probe_swap_recovery(&self, generation: u64) -> Result<(), EngineError>;
    /// Asks nvim to repaint from scratch and retract the messages it has
    /// shown (see `RpcCall::Redraw`).
    fn redraw(&self) -> Result<(), EngineError>;
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
    ) -> Result<(), EngineError>;
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
    fn probe_swap_recovery(&self, generation: u64) -> Result<(), EngineError> {
        self.probe_swap_recovery(generation)
    }
    fn redraw(&self) -> Result<(), EngineError> {
        self.redraw()
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
    fn set_buf_text(
        &self,
        buf: BufferHandle,
        edits: &[TextEdit],
        undojoin: bool,
    ) -> Result<(), EngineError> {
        self.set_buf_text(buf, edits, undojoin)
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
    fn probe_swap_recovery(&self, generation: u64) -> Result<(), EngineError> {
        (**self).probe_swap_recovery(generation)
    }
    fn redraw(&self) -> Result<(), EngineError> {
        (**self).redraw()
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
    fn set_buf_text(
        &self,
        buf: BufferHandle,
        edits: &[TextEdit],
        undojoin: bool,
    ) -> Result<(), EngineError> {
        (**self).set_buf_text(buf, edits, undojoin)
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
    fn probe_swap_recovery(&self, generation: u64) -> Result<(), EngineError> {
        self.record(format!("probe_swap_recovery({generation})"))
    }
    fn redraw(&self) -> Result<(), EngineError> {
        self.record("redraw()".to_string())
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
    fn set_buf_text(
        &self,
        buf: BufferHandle,
        edits: &[TextEdit],
        undojoin: bool,
    ) -> Result<(), EngineError> {
        self.record(format!(
            "set_buf_text({},{},{undojoin})",
            buf.0,
            edits.len()
        ))
    }
}
