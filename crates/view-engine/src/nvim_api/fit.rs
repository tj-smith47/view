//! `window fit` and `[ui] fit_active`: a window sized to the longest line it
//! shows.
//!
//! The measuring runs inside nvim because the width of a line lives only
//! there: a line longer than its window reaches view already wrapped across
//! grid rows.

use crate::handle::EngineError;
use rmpv::Value;

/// The lua chunk [`EngineHandle::fit_window`] and
/// [`EngineHandle::set_fit_active`] run inside nvim, taking an operation
/// (`fit`, `on` or `off`) and the columns a gapped frame takes off each side
/// of the slot.
///
/// Constant by construction, for the reason
/// [`REGISTER_BRIDGE_CHUNK`](super::REGISTER_BRIDGE_CHUNK) is: no caller
/// data is interpolated into the Lua source.
///
/// The text width is the widest `strdisplaywidth` over the lines from `w0`
/// to `w$`, plus one column for the cursor past the last character, and
/// never under `winwidth`. It is capped at `textwidth`, or where that is 0
/// at the first absolute `colorcolumn`, so the marker column stays in view.
/// The slot adds the number and sign columns (`textoff`) and the frame on
/// both sides.
///
/// Under `equalalways` the other windows share what the fitted one left,
/// through `horizontal wincmd =` with the fitted one held by `winfixwidth`
/// for that one command; the hold goes on and off under `noautocmd` so no
/// `OptionSet` handler sees a sidebar that never existed. Without it only
/// the neighbour nvim takes the columns from moves.
///
/// Floats, windows already holding `winfixwidth` (view's own sidebars and a
/// plugin's) and terminals are left alone.
///
/// `on` replaces the `view_fit` group with a `WinEnter` hook that fits the
/// entered window at the inset it was handed. It skips a layout a zoom
/// left behind, read the way `window zoom` reads one: every other window at
/// `winminwidth` or `winminheight`. So opening and closing a float over a
/// zoomed tile does not undo the zoom.
///
/// [`EngineHandle::fit_window`]: super::EngineHandle::fit_window
/// [`EngineHandle::set_fit_active`]: super::EngineHandle::set_fit_active
pub(crate) const FIT_CHUNK: &str = "\
local op, inset = ...
local api = vim.api
local function floating(win)
  return api.nvim_win_get_config(win).relative ~= ''
end
local function fit(win)
  local buf = api.nvim_win_get_buf(win)
  if floating(win) or vim.wo[win].winfixwidth
    or vim.bo[buf].buftype == 'terminal' then
    return
  end
  local longest = api.nvim_win_call(win, function()
    local widest = 0
    local first, last = vim.fn.line('w0'), vim.fn.line('w$')
    for _, line in ipairs(api.nvim_buf_get_lines(buf, first - 1, last,
      false)) do
      widest = math.max(widest, vim.fn.strdisplaywidth(line))
    end
    return widest
  end)
  local target = math.max(longest + 1, vim.o.winwidth)
  local cap = vim.bo[buf].textwidth
  if cap <= 0 then
    cap = tonumber(vim.wo[win].colorcolumn:match('^%d+')) or 0
  end
  if cap > 0 then
    target = math.min(target, cap)
  end
  local textoff = vim.fn.getwininfo(win)[1].textoff
  api.nvim_win_set_width(win, target + textoff + 2 * inset)
  if vim.o.equalalways then
    api.nvim_win_call(win, function()
      vim.cmd('noautocmd setlocal winfixwidth')
      local ok, err = pcall(vim.cmd, 'horizontal wincmd =')
      vim.cmd('noautocmd setlocal nowinfixwidth')
      if not ok then
        error(err, 0)
      end
    end)
  end
end
local function zoomed(win)
  local tab = api.nvim_win_get_tabpage(win)
  for _, other in ipairs(api.nvim_tabpage_list_wins(tab)) do
    if other ~= win and not floating(other)
      and api.nvim_win_get_width(other) > vim.o.winminwidth
      and api.nvim_win_get_height(other) > vim.o.winminheight then
      return false
    end
  end
  return true
end
local function run(win)
  local ok, err = pcall(fit, win)
  if not ok then
    vim.notify('view: window fit failed: ' .. tostring(err),
      vim.log.levels.ERROR)
  end
end
if op == 'fit' then
  run(api.nvim_get_current_win())
  return
end
local group = api.nvim_create_augroup('view_fit', { clear = true })
if op == 'on' then
  api.nvim_create_autocmd('WinEnter', {
    group = group,
    callback = function()
      local win = api.nvim_get_current_win()
      if not zoomed(win) then
        run(win)
      end
    end,
  })
end";

impl super::EngineHandle {
    /// Sizes nvim's current window to the longest line it shows, `inset_cols`
    /// being the frame's columns on each side of the slot. A notify on
    /// [`close_native_window`](Self::close_native_window)'s terms: the answer
    /// is the `win_pos` traffic the resize produces.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection's writer thread has
    /// already exited.
    pub fn fit_window(&self, inset_cols: u16) -> Result<(), EngineError> {
        self.fit("fit", inset_cols)
    }

    /// Turns the fit on window entry on or off, at `inset_cols`. Sending it
    /// again replaces the hook, which is how a look change hands over its
    /// new inset.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection's writer thread has
    /// already exited.
    pub fn set_fit_active(&self, on: bool, inset_cols: u16) -> Result<(), EngineError> {
        self.fit(if on { "on" } else { "off" }, inset_cols)
    }

    fn fit(&self, op: &str, inset_cols: u16) -> Result<(), EngineError> {
        self.notify(
            "nvim_exec_lua",
            vec![
                Value::from(FIT_CHUNK),
                Value::Array(vec![Value::from(op), Value::from(inset_cols)]),
            ],
        )
    }
}
