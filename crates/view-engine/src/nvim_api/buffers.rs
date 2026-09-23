//! The bridge's `buffers` trigger: the whole listed-buffer set, whenever it
//! changes.
//!
//! Beside the `view_bridge` chunk for [`super::window_status`]'s reason:
//! this group answers with a list where the session-wide segments there
//! answer with one value each.
//!
//! # The throttle
//!
//! `BufEnter` fires on every window the user steps through, so a `:bufdo`
//! or a quickfix walk fires it as fast as nvim can run. Every trigger arms
//! a `vim.schedule` callback and notifies nothing itself, and the callback
//! sends the list once and disarms, the way the window group collapses a
//! `CursorMoved` burst. Nothing on the key path waits for it.
//!
//! # Why the whole list
//!
//! nvim answers "which buffers are listed" in full or not at all, and the
//! pill names the whole set each time. A per-buffer event would
//! leave view holding a set it had to keep true against every event it
//! might have missed.

/// The lua chunk [`EngineHandle::register_bridge`] runs inside nvim, taking
/// view's channel id as its single vararg.
///
/// Constant by construction, for the same reason
/// [`REGISTER_BRIDGE_CHUNK`](super::REGISTER_BRIDGE_CHUNK) is: no caller
/// data is interpolated into the Lua source.
///
/// The payload is `(list)`, one entry per listed buffer as
/// `(buf, name, modified, current)`, decoded by `handle::decode`'s
/// `"buffers"` arm. `name` is the buffer's tail, the same `:t` modifier the
/// `window` trigger takes, because the row names a file. A whole path
/// would not fit.
///
/// `BufModifiedSet` is in the trigger list beside the three that change the
/// set itself: the row draws an unsaved marker, and without it the marker
/// would stand until the user next changed buffers. `BufFilePost` and
/// `OptionSet buflisted` are there because `:file newname` and
/// `:setlocal nobuflisted` change what the row says without adding or
/// removing a buffer, and the row would stay stale until the next
/// `BufEnter`.
///
/// The report runs under a `pcall` for [`REGISTER_WINDOW_STATUS_CHUNK`]'s
/// reason: it is scheduled, so it can land after a channel teardown.
///
/// `VimEnter` repeats the sweep the registration itself takes, because the
/// buffers a config opens are not open yet when this runs off the spawn's
/// own `--cmd`.
///
/// [`EngineHandle::register_bridge`]: super::EngineHandle::register_bridge
/// [`REGISTER_WINDOW_STATUS_CHUNK`]: super::REGISTER_WINDOW_STATUS_CHUNK
pub(crate) const REGISTER_BUFFERS_CHUNK: &str = "\
local channel = ...
local group = vim.api.nvim_create_augroup('view_buffers', { clear = true })
local armed = false
local function report()
  local current = vim.api.nvim_get_current_buf()
  local listed = {}
  for _, buf in ipairs(vim.api.nvim_list_bufs()) do
    if vim.bo[buf].buflisted then
      listed[#listed + 1] = { buf,
        vim.fn.fnamemodify(vim.api.nvim_buf_get_name(buf), ':t'),
        vim.bo[buf].modified, buf == current }
    end
  end
  vim.rpcnotify(channel, 'view_bridge', 'buffers', listed)
end
local function flush()
  armed = false
  pcall(report)
end
local function arm()
  if armed then
    return
  end
  armed = true
  vim.schedule(flush)
end
vim.api.nvim_create_autocmd({ 'BufAdd', 'BufDelete', 'BufEnter',
  'BufModifiedSet', 'BufFilePost' }, {
  group = group,
  callback = arm,
})
vim.api.nvim_create_autocmd('OptionSet', {
  group = group,
  pattern = 'buflisted',
  callback = arm,
})
vim.api.nvim_create_autocmd('VimEnter', {
  group = group,
  callback = arm,
})
arm()";

/// The lua chunk [`EngineHandle::select_tab`] runs inside nvim, taking the
/// tabpage handle as its single vararg.
///
/// The validity check is the click's own race: the tabpage a row named can
/// be closed between the frame that drew it and the press that reaches
/// nvim, and an invalid handle raises.
///
/// [`EngineHandle::select_tab`]: super::EngineHandle::select_tab
pub(crate) const SELECT_TAB_CHUNK: &str = "\
local tab = ...
if vim.api.nvim_tabpage_is_valid(tab) then
  vim.api.nvim_set_current_tabpage(tab)
end";

/// The lua chunk [`EngineHandle::select_buffer`] runs inside nvim, taking
/// the buffer handle as its single vararg. Guarded for
/// [`SELECT_TAB_CHUNK`]'s reason.
///
/// [`EngineHandle::select_buffer`]: super::EngineHandle::select_buffer
pub(crate) const SELECT_BUFFER_CHUNK: &str = "\
local buf = ...
if vim.api.nvim_buf_is_valid(buf) then
  vim.api.nvim_set_current_buf(buf)
end";

impl super::EngineHandle {
    /// Switches nvim to `tab`, for a click on a pill tab.
    ///
    /// A notify, with no reply awaited, for
    /// [`register_bridge`](Self::register_bridge)'s reason: the answer a
    /// caller wants is the `tabline` event nvim sends when the tabpage
    /// changed, and the paint loop must never wait on a reply.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection's writer thread has
    /// already exited.
    pub fn select_tab(&self, tab: u64) -> Result<(), crate::handle::EngineError> {
        self.notify(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from(SELECT_TAB_CHUNK),
                rmpv::Value::Array(vec![rmpv::Value::from(tab)]),
            ],
        )
    }

    /// Switches nvim to `buf`, for a click on a pill buffer. A notify on
    /// [`select_tab`](Self::select_tab)'s terms.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection's writer thread has
    /// already exited.
    pub fn select_buffer(&self, buf: u64) -> Result<(), crate::handle::EngineError> {
        self.notify(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from(SELECT_BUFFER_CHUNK),
                rmpv::Value::Array(vec![rmpv::Value::from(buf)]),
            ],
        )
    }
}
