//! Opening a file or a buffer the picker or the file tree chose.

use view_core::msg::OpenIn;
use view_core::native::picker::Picked;

/// Opens a chosen file or buffer, taking `(path, how, line, buffer,
/// claimed)` as varargs: `how` is the ex command a file opens with
/// (`edit`, `vsplit`, `split` or `tabedit`), `line` is `0` for none,
/// `buffer` is the handle of a listed buffer to open, `0` for a file, and
/// `claimed` lists the windows view paints a sidebar over. Constant, like
/// every other chunk here: no caller data is interpolated into the source.
///
/// A sidebar is a window `claimed` names or one `g:view_native_windows`
/// records. The record holds a sidebar whose window view has not yet
/// claimed, and `claimed` holds one whose record a person cleared. From any
/// other window, a float or a plugin's sidebar included, the open runs
/// there, as `:edit` would. From a sidebar it enters an ordinary window
/// first: one on this tab that is docked, no sidebar, shows a buffer with
/// an empty `buftype` and has neither `winfixbuf` nor `previewwindow` set.
/// The ordinary window the cursor left last is taken, from the list the
/// sidebar open keeps (`_G.view_recent_wins`), then `#`, then the first in
/// the layout. With none left, the file opens in a new window split off
/// beside the sidebar, on the side away from the screen's edge, and a
/// refused open makes no window.
///
/// A file reaches `nvim_cmd` as an argument with filename magic off, so a
/// space, `%`, `#`, `\` or a leading `+` in its name is no command syntax
/// (`docs/tree-open-file-wire-capture.md` measures each half). Escaping
/// the name with `fnameescape` breaks every Windows path, whose separator
/// it escapes.
///
/// A buffer is switched to by its handle, so two buffers whose names read
/// alike (two unnamed scratches) stay apart, and a buffer with no file
/// behind it opens where `:edit` has nothing to open.
///
/// A file deleted since it was listed, a buffer closed since, and an open
/// nvim refuses (`E37` under `nohidden`) each leave a message saying so,
/// since the picker that asked has already closed.
///
/// An operator the person left pending is dropped, so the keys typed
/// behind the open start a command of their own. The line is held to the
/// buffer's last, since the file can have shrunk since a grep match was
/// read off disk. The screen is drawn before the reply. nvim reports the
/// window the cursor moved to only after the reply.
///
/// Returns the handle of the window the cursor is in once the open has
/// run, whether it opened anything or not.
pub(super) const OPEN_PICKED_CHUNK: &str = "\
local path, how, line, buffer, claimed = ...
local here = vim.api.nvim_get_current_win
local sidebar, edge = {}, {}
for _, win in ipairs(claimed) do
  sidebar[win] = true
end
for _, held in pairs(vim.g.view_native_windows or {}) do
  sidebar[held.win] = true
  edge[held.win] = held.edge
end
local function docked(win)
  return vim.api.nvim_win_get_config(win).relative == ''
end
local tab = vim.api.nvim_get_current_tabpage()
local function ordinary(win)
  if sidebar[win] or not vim.api.nvim_win_is_valid(win)
      or vim.api.nvim_win_get_tabpage(win) ~= tab or not docked(win) then
    return false
  end
  local buf = vim.api.nvim_win_get_buf(win)
  return vim.bo[buf].buftype == '' and not vim.wo[win].winfixbuf
    and not vim.wo[win].previewwindow
end
local beside = nil
if sidebar[here()] then
  local into, fallback = nil, nil
  local layout = vim.api.nvim_tabpage_list_wins(0)
  local candidates = vim.list_extend({}, _G.view_recent_wins or {})
  candidates[#candidates + 1] = vim.fn.win_getid(vim.fn.winnr('#'))
  for _, win in ipairs(vim.list_extend(candidates, layout)) do
    if into == nil and win ~= 0 and ordinary(win) then
      into = win
    end
  end
  for _, win in ipairs(layout) do
    fallback = fallback or docked(win) and win or nil
  end
  if into ~= nil then
    vim.api.nvim_set_current_win(into)
  elseif how ~= 'tabedit' then
    if not docked(here()) then
      vim.api.nvim_set_current_win(fallback)
    end
    local across = edge[here()] == 'above' or edge[here()] == 'below'
    local toward = across and 'k' or 'h'
    beside = vim.fn.winnr(toward) == vim.fn.winnr() and 'belowright'
      or 'aboveleft'
    how = across and 'split' or 'vsplit'
  end
end
if vim.api.nvim_get_mode().mode:sub(1, 2) == 'no' then
  vim.api.nvim_feedkeys(vim.keycode('<Esc>'), 'ni', false)
end
local function say(text)
  vim.api.nvim_echo({ { text } }, true, { err = true })
end
local ok, err = true, nil
if buffer > 0 then
  if not vim.api.nvim_buf_is_valid(buffer)
      or not vim.bo[buffer].buflisted then
    say('That buffer has been closed')
    return here()
  end
  local split = { vsplit = 'vertical sbuffer ', split = 'sbuffer ',
    tabedit = 'tab sbuffer ' }
  ok, err = pcall(vim.cmd,
    (beside and beside .. ' ' or '') .. (split[how] or 'buffer ') .. buffer)
elseif not vim.uv.fs_stat(path) then
  say(path .. ' no longer exists')
  return here()
else
  ok, err = pcall(vim.api.nvim_cmd, {
    cmd = how, args = { path }, magic = { file = false, bar = false },
    mods = { split = beside },
  }, {})
end
if not ok then
  say(tostring(err):match('E%d+:.*') or tostring(err))
  return here()
end
if line > 0 then
  local last = vim.api.nvim_buf_line_count(0)
  vim.api.nvim_win_set_cursor(0, { math.min(line, last), 0 })
end
vim.cmd.redraw()
return here()";

/// The window [`OPEN_PICKED_CHUNK`]'s reply names the cursor in, or `None`
/// for an error reply.
pub(crate) fn decode_open_reply(
    error: &rmpv::Value,
    result: &rmpv::Value,
) -> Option<view_core::events::WinHandle> {
    error
        .is_nil()
        .then(|| super::native_window::decode_native_window_reply(result))
        .flatten()
}

/// The ex command [`OPEN_PICKED_CHUNK`] opens a file with for `how`.
fn command(how: OpenIn) -> &'static str {
    match how {
        OpenIn::Vertical => "vsplit",
        OpenIn::Horizontal => "split",
        OpenIn::Tab => "tabedit",
        _ => "edit",
    }
}

impl super::EngineHandle {
    /// Opens `target` in the window `how` names via [`OPEN_PICKED_CHUNK`]:
    /// a file by its path, the cursor on its line when it has one, and a
    /// listed buffer by its handle. A choice made while the cursor is in
    /// one of the `claimed` windows, or in a window nvim's record of view's
    /// own windows names, opens in the ordinary window last edited in.
    ///
    /// Async: nvim's answer is routed as `Msg::PickedOpened` carrying
    /// `generation` and the window the cursor ended in, whatever the open
    /// did. nvim reads `nvim_input` ahead
    /// of a queued request, so input sent before that answer can run
    /// before the open.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited.
    pub fn open_picked(
        &self,
        target: &Picked,
        how: OpenIn,
        claimed: &[view_core::events::WinHandle],
        generation: u64,
    ) -> Result<(), crate::handle::EngineError> {
        let (path, line, buffer) = match target {
            Picked::File { path, line } => (path.as_str(), line.unwrap_or(0), 0),
            Picked::Buffer { handle } => ("", 0, *handle),
            _ => return Ok(()),
        };
        self.request_async(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from(OPEN_PICKED_CHUNK),
                rmpv::Value::Array(vec![
                    rmpv::Value::from(path),
                    rmpv::Value::from(command(how)),
                    rmpv::Value::from(line),
                    rmpv::Value::from(buffer),
                    rmpv::Value::Array(
                        claimed.iter().map(|win| rmpv::Value::from(win.0)).collect(),
                    ),
                ]),
            ],
            crate::handle::Waiter::Opened { generation },
        )
    }
}
