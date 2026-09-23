//! The bridge's `window` trigger: one report per window whose own status
//! segments changed.
//!
//! Beside the `view_bridge` chunk, outside it, because this group
//! answers a different question: the session-wide segments there are one
//! value each, and these are one value per window.
//!
//! # The throttle
//!
//! `CursorMoved` fires once per cursor motion, so a held-down `j` fires it
//! as fast as nvim can redraw. Every trigger arms a `vim.schedule`
//! callback and notifies nothing itself, and the callback reports each
//! armed window once and disarms. `vim.schedule` defers to the next turn of
//! nvim's event loop, whatever redraws it holds, so a burst inside one turn
//! collapses to one message per window and the segment lags the cursor by
//! at most a tick. Nothing on the key path waits for it: the trigger runs
//! in nvim, the notification arrives on view's reader thread, and the
//! paint that shows it is the next one view was going to draw anyway.
//!
//! # Which window a trigger names
//!
//! `WinEnter`, `BufEnter`, `BufModifiedSet`, `CursorMoved` and
//! `CursorMovedI` are all about the window the user is in, so they arm the
//! current one. `DiagnosticChanged` is about a buffer, which any number of
//! windows may be showing, so it arms every window showing it -- a split
//! over one file shows the same counts in both tiles, and arming only the
//! current one would leave the other's count stale until its own cursor
//! moved.

/// The lua chunk [`register_window_status`] runs inside nvim, taking view's
/// channel id as its single vararg.
///
/// Constant by construction, for the same reason
/// [`REGISTER_BRIDGE_CHUNK`](super::REGISTER_BRIDGE_CHUNK) is: no caller
/// data is interpolated into the Lua source.
///
/// The payload is `(win, buf, name, modified, row, col, errors, warnings)`,
/// decoded field for field by `handle::decode`'s `"window"` arm. `name` is
/// the buffer's tail, the same `:t` modifier the `buffer` trigger takes, so
/// a tile's edge names a file. A whole path would not fit.
/// `nvim_win_get_cursor` answers a 1-based line and a 0-based column, and
/// the column is put on the wire 1-based because that is the number a user
/// reads off a ruler.
///
/// Every report runs under a `pcall`: a window can close between the tick
/// that armed it and the tick that flushes it, and an error raised on the
/// first of two windows would otherwise lose the second. The `pcall` covers
/// the notify too, which is the one call here that can land after a channel
/// teardown, since it is scheduled.
///
/// The registration reports every window it finds, so a session whose user
/// never moves the cursor still has segments to draw. `VimEnter` repeats
/// that sweep, because the windows a config opens are not open yet when
/// this runs off the spawn's own `--cmd`.
///
/// The counts are read only once a `DiagnosticChanged` has fired, or when
/// `vim.diagnostic` was already reached before this registration ran.
/// Touching `vim.diagnostic` loads the module, which costs about a
/// millisecond, and the startup sweep runs inside nvim's blocked `VimEnter`,
/// so that load landed on every launch's startup clock. Every diagnostic a
/// producer sets or clears fires `DiagnosticChanged`, and this group is
/// registered off the spawn's `--cmd`, ahead of any config, so a session
/// that has not fired it holds no diagnostics to count.
pub(crate) const REGISTER_WINDOW_STATUS_CHUNK: &str = "\
local channel = ...
local group = vim.api.nvim_create_augroup('view_window_status',
  { clear = true })
local pending, armed = {}, false
local counted = rawget(vim, 'diagnostic') ~= nil
local function report(win)
  if not vim.api.nvim_win_is_valid(win) then
    return
  end
  local buf = vim.api.nvim_win_get_buf(win)
  local cursor = vim.api.nvim_win_get_cursor(win)
  local errors, warnings = 0, 0
  if counted then
    local counts = vim.diagnostic.count(buf)
    errors = counts[vim.diagnostic.severity.ERROR] or 0
    warnings = counts[vim.diagnostic.severity.WARN] or 0
  end
  vim.rpcnotify(channel, 'view_bridge', 'window', win, buf,
    vim.fn.fnamemodify(vim.api.nvim_buf_get_name(buf), ':t'),
    vim.bo[buf].modified, cursor[1], cursor[2] + 1, errors, warnings)
end
local function flush()
  armed = false
  local armed_wins = pending
  pending = {}
  for win in pairs(armed_wins) do
    pcall(report, win)
  end
end
local function arm(win)
  pending[win] = true
  if armed then
    return
  end
  armed = true
  vim.schedule(flush)
end
local function arm_current()
  arm(vim.api.nvim_get_current_win())
end
local function arm_all()
  for _, win in ipairs(vim.api.nvim_list_wins()) do
    arm(win)
  end
end
vim.api.nvim_create_autocmd({ 'WinEnter', 'BufEnter', 'BufModifiedSet',
  'CursorMoved', 'CursorMovedI' }, {
  group = group,
  callback = arm_current,
})
vim.api.nvim_create_autocmd('DiagnosticChanged', {
  group = group,
  callback = function(args)
    counted = true
    for _, win in ipairs(vim.api.nvim_list_wins()) do
      if vim.api.nvim_win_get_buf(win) == args.buf then
        arm(win)
      end
    end
  end,
})
vim.api.nvim_create_autocmd('VimEnter', {
  group = group,
  callback = arm_all,
})
arm_all()";
