//! The bridge's `window` trigger: one report per window whose own status
//! segments changed.
//!
//! A group of its own beside the `view_bridge` chunk, because it answers a
//! different question: the session-wide segments there are one value each,
//! and these are one value per window.
//!
//! # No cursor event
//!
//! The group registers no `CursorMoved` or `CursorMovedI`. Either would run
//! a Lua callback and a notification on every keystroke under every look,
//! and nvim already sends the cursor's position with each redraw as
//! `win_viewport`, which the model folds into the same record.
//!
//! # The throttle
//!
//! Every trigger arms a `vim.schedule` callback and notifies nothing
//! itself, and the callback reports each armed window once and disarms.
//! `vim.schedule` defers to the next turn of nvim's event loop, whatever
//! redraws it holds, so a burst inside one turn (a `:bufdo`, a `:windo`, a
//! diagnostic producer setting a namespace per buffer) collapses to one
//! message per window.
//!
//! # Which window a trigger names
//!
//! `WinEnter`, `BufEnter` and `BufModifiedSet` are all about the window the
//! user is in, so they arm the current one. `DiagnosticChanged` is about a
//! buffer, which any number of windows may be showing, so it arms every
//! window showing it: a split over one file shows the same counts in both
//! tiles. `FileType` and `TermOpen` are about a buffer as well and arm the
//! same way: a plugin sets its filetype after `BufEnter` has reported the
//! window, often from outside it, and the tile's kind is read off that
//! filetype.

/// The lua chunk [`register_window_status`] runs inside nvim, taking view's
/// channel id as its single vararg.
///
/// Constant by construction, for the same reason
/// [`REGISTER_BRIDGE_CHUNK`](super::REGISTER_BRIDGE_CHUNK) is: no caller
/// data is interpolated into the Lua source.
///
/// The payload is `(win, buf, name, modified, row, col, errors, warnings,
/// buftype, filetype, loclist, lines)`, decoded field for field by
/// `handle::decode`'s `"window"` arm. `name` is the buffer's tail, the same
/// `:t` modifier the `buffer` trigger takes, so a tile's edge names a file.
/// A whole path would not fit. A terminal's buffer is named
/// `term://{cwd}//{pid}:{cmd}`, so its name is the tail of the command's
/// first word, the program the job runs. The word stops at a `;` as well,
/// since a terminal manager appends its own `;#tag` to the command.
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
  local buftype = vim.bo[buf].buftype
  local name = vim.api.nvim_buf_get_name(buf)
  if buftype == 'terminal' then
    name = name:match('^term://.-//%d+:(.*)$') or name
    name = name:match('^[^%s;]+') or name
  end
  vim.rpcnotify(channel, 'view_bridge', 'window', win, buf,
    vim.fn.fnamemodify(name, ':t'),
    vim.bo[buf].modified, cursor[1], cursor[2] + 1, errors, warnings,
    buftype, vim.bo[buf].filetype, vim.fn.getwininfo(win)[1].loclist,
    vim.api.nvim_buf_line_count(buf))
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
vim.api.nvim_create_autocmd({ 'WinEnter', 'BufEnter', 'BufModifiedSet' }, {
  group = group,
  callback = arm_current,
})
local function arm_showing(args)
  for _, win in ipairs(vim.api.nvim_list_wins()) do
    if vim.api.nvim_win_get_buf(win) == args.buf then
      arm(win)
    end
  end
end
vim.api.nvim_create_autocmd('DiagnosticChanged', {
  group = group,
  callback = function(args)
    counted = true
    arm_showing(args)
  end,
})
vim.api.nvim_create_autocmd({ 'FileType', 'TermOpen' }, {
  group = group,
  callback = arm_showing,
})
vim.api.nvim_create_autocmd('VimEnter', {
  group = group,
  callback = arm_all,
})
arm_all()";
