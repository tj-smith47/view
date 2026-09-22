//! The windows view opens for its own surfaces, and the scratch buffers
//! behind them.
//!
//! A windowed surface is an ordinary nvim window: nvim lays it out, `<C-w>`
//! reaches it, and a `:split` beside it makes room the way it would for any
//! buffer. What makes it view's is the buffer inside it, which holds
//! nothing and cannot be typed into, and the pane the compositor paints
//! over it once the handle comes back.

use std::time::Duration;

use rmpv::Value;

use crate::handle::EngineError;
use view_core::msg::WinSplit;

/// How long [`EngineHandle::open_native_window_sync`] waits for nvim to
/// answer. The same five seconds every other blocking call in this crate
/// allows: an engine that has not answered a window open in that long is
/// wedged, not slow.
const OPEN_NATIVE_WINDOW_TIMEOUT: Duration = Duration::from_secs(5);

/// The three things both chunks have to agree on, as the Lua they are
/// built from: whether a window is still the surface's, how the size pins
/// come off, and how the look goes back. Written once and expanded into
/// each chunk, because a rule spelled twice drifts: the close chunk and
/// the open chunk's callback asked the same question in two different
/// ways once, and the weaker spelling let the next open enter a window
/// holding the person's file.
///
/// `is_ours` answers by identity: the buffer the open recorded is still
/// view's own scratch (`is_scratch`) and still the one the window shows.
/// `:edit` inside the window reuses the scratch buffer itself, since it
/// is unnamed and unmodified, which is nvim's own condition for reusing a
/// buffer, so the recorded number alone says nothing.
///
/// `hand_back_look` re-fires the buffer's `FileType` autocommands inside
/// the window after restoring the globals. `FileType` runs during
/// `BufReadPost`, before the window is handed back, so an ftplugin's
/// `setlocal number` had already been written and the restore from the
/// globals wiped it: the person read their file without the look their
/// own config gives that filetype. `modeline = false`, because a file's
/// own modeline has already been applied and applying it twice is not
/// what a hand-back owes.
macro_rules! native_window_helpers {
    () => {
        "\
local function is_scratch(buf)
  return vim.api.nvim_buf_is_valid(buf)
    and vim.api.nvim_get_option_value('buftype', { buf = buf }) == 'nofile'
end
local function is_ours(entry)
  return entry ~= nil
    and vim.api.nvim_win_is_valid(entry.win)
    and is_scratch(entry.buf)
    and vim.api.nvim_win_get_buf(entry.win) == entry.buf
end
local function unpin_size(win)
  for _, opt in ipairs({ 'winfixwidth', 'winfixheight' }) do
    vim.api.nvim_set_option_value(opt, false, { win = win, scope = 'local' })
  end
end
local function hand_back_look(win)
  unpin_size(win)
  local buf = vim.api.nvim_win_get_buf(win)
  for _, opt in ipairs({ 'number', 'relativenumber', 'signcolumn',
    'foldcolumn', 'wrap' }) do
    vim.api.nvim_set_option_value(opt,
      vim.api.nvim_get_option_value(opt, { scope = 'global' }),
      { win = win, scope = 'local' })
  end
  if vim.api.nvim_get_option_value('filetype', { buf = buf }) ~= '' then
    vim.api.nvim_win_call(win, function()
      vim.api.nvim_exec_autocmds('FileType',
        { buffer = buf, modeline = false })
    end)
  end
end
"
    };
}

/// The lua chunk [`EngineHandle::open_native_window`] runs inside nvim,
/// taking the surface id, the split word, the size in percent and view's
/// own channel id.
///
/// One window per surface, kept in a table on the module's own upvalue: a
/// second call for a surface that already has a live window enters that
/// window instead of opening another, which is what makes one message both
/// "open it" and "go to it". The table is written into `vim.g` rather than
/// a Lua local because each `nvim_exec_lua` call is its own chunk.
///
/// Each entry carries the window and the buffer view put in it, and the
/// window counts as the surface's only while it still shows that buffer.
/// A window is a position and a buffer is an identity: anything outside
/// view can put a file in the window (a plugin autocommand, a quickfix
/// jump, `:sbuffer`), and a table that remembered the window alone would
/// have the next open take that file's window for the tree and the close
/// treat the file as view's to throw away.
///
/// The buffer is scratch and stays that way: `buftype = nofile` so nothing
/// writes it, `bufhidden = wipe` so closing the window takes the buffer
/// with it, `buflisted = false` so it never reaches the pill or a picker,
/// and `modifiable = false` with `readonly` set so a keystroke that reaches
/// nvim before view has claimed the window cannot put a character in it.
/// `winfixwidth` and `winfixheight` keep nvim's own layout from re-flowing
/// the sidebar when another window opens beside it.
///
/// Every window option is written with `scope = 'local'`. `vim.wo[win]`
/// and a bare `nvim_set_option_value` write the global value too, the way
/// `:set` does, so opening the tree took the person's own `number`,
/// `signcolumn` and `wrap` away from every window they opened afterwards
/// for the rest of the session.
///
/// The window stops being view's the moment `is_ours` stops answering for
/// it, which the `BufWinEnter` autocommand the open registers is what
/// notices. It reports the surface over the bridge as
/// `native_window_taken`, hands the window's look back, forgets the
/// surface's entry so a later close finds nothing to do, and deletes
/// itself, so one taken window costs one message. Without it view went on
/// painting the surface's rows over the file the person had just opened
/// there, and they could not see what they were editing until the next
/// toggle.
///
/// The callback's first question is whether the entry still names this
/// window, and it deletes itself when the answer is no: the close chunk
/// deletes the augroup, but a firing that reaches a window view has
/// already handed back would write the globals over the person's own
/// window-local look.
///
/// The window is made with `:split` rather than `nvim_open_win`, which
/// allocates a second grid under `ext_multigrid` and leaves it behind: view
/// then holds one more grid than nvim has windows.
///
/// The returned value is the window handle, which the reply decodes into
/// `Msg::NativeWindowOpened`.
///
/// `enter` false is a ring step carrying an already-open surface to
/// `windowed`, a surface nobody asked to visit. The split, and every
/// option write and buffer swap it takes to make the window, run under
/// `eventignore` so none of the four `Win*` events or nvim's own `Buf*`
/// ones reach a user autocommand, and `curwin` and the `wincmd p` target
/// are restored to what they were before the chunk ran, in the same
/// chunk: the two-hop restore (leave through the window that was
/// previous, then arrive at the window that was current) is what makes
/// `nvim_set_current_win` -- which always overwrites the previous-window
/// record with the window it is leaving -- land on the original previous
/// window rather than on the surface's own. Without this a windowed
/// surface carried in by a ring step took `curwin` on the next `:` or
/// `/`: `:q` closed its scratch window instead of the person's, and a
/// search ran against its empty buffer instead of theirs.
///
/// The body runs under one `pcall` from the moment `eventignore` is
/// overridden to the moment it is put back, so a split that fails (no
/// room for the window, or `getcmdwintype()` catches the rarer case of
/// running from inside the command-line window) never leaves the option
/// set for the rest of the session; the scratch buffer it had already
/// created is deleted on that path too, since nothing else will ever
/// pick it up.
///
/// [`EngineHandle::open_native_window`]: super::EngineHandle::open_native_window
pub(crate) const OPEN_NATIVE_WINDOW_CHUNK: &str = concat!(
    "local id, split, pct, channel, enter = ...\n",
    native_window_helpers!(),
    "\
-- the order a shared edge stacks in: named surfaces earlier here draw
-- closer to the edge, whichever one opened second -- design puts the
-- tree above the agent panel, a rule this table states once rather than
-- letting the open order of two `OpenNativeWindow` calls decide it
local stack_order = { tree = 1, agent = 2, notifications = 3 }
local wins = vim.g.view_native_windows or {}
local live = wins[id]
if is_ours(live) then
  if enter then
    vim.api.nvim_set_current_win(live.win)
  end
  return live.win
end
-- a split issued from inside the command-line window raises E11 before
-- it ever reaches `eventignore`; refusing here rather than inside the
-- pcall below is one fewer error a caller has to decode
if vim.fn.getcmdwintype() ~= '' then
  return nil
end
local win_before = vim.api.nvim_get_current_win()
local prev_before = vim.fn.win_getid(vim.fn.winnr('#'))
local ei = vim.o.eventignore
-- `enter == false` is a ring step carrying an already-open surface to
-- `windowed`, a surface nobody asked to visit: the split, and the
-- restore below, must cost the user's own WinEnter/WinLeave/WinNew/
-- WinClosed and Buf* autocmds nothing, since from their side nothing
-- happened
if not enter then
  vim.o.eventignore =
    'WinEnter,WinLeave,WinNew,WinClosed,BufEnter,BufLeave,BufWinEnter'
end
-- the whole split-and-configure step runs under one pcall so a failure
-- partway through (no room for the window is the reachable one) still
-- restores `eventignore` below rather than leaving it set for the rest
-- of the session; `buf` and `win` are declared outside the closure so a
-- buffer or window a failed attempt already created can be cleaned up too
local buf = nil
local win = nil
local ok, result = pcall(function()
  buf = vim.api.nvim_create_buf(false, true)
  vim.bo[buf].buftype = 'nofile'
  vim.bo[buf].bufhidden = 'wipe'
  vim.bo[buf].buflisted = false
  vim.bo[buf].swapfile = false
  vim.bo[buf].filetype = 'view-' .. id
  local vertical = split == 'left' or split == 'right'
  local total = vertical and vim.o.columns or vim.o.lines
  local cells = math.max(1, math.floor(total * pct / 100))
  local commands = {
    left = 'topleft vsplit',
    right = 'botright vsplit',
    above = 'topleft split',
    below = 'botright split',
  }
  -- a second surface anchored at an edge another already holds stacks
  -- under it instead of claiming a further column or row of its own:
  -- split the edge's own window, across the axis a shared column stacks
  -- on (rows for a left/right edge, columns for an above/below one).
  -- `pairs()` visits a table in no defined order, so with three surfaces
  -- already sharing an edge the first one it happens to yield is not
  -- necessarily this one's actual neighbor in `stack_order`; splitting
  -- against the nearest ranked neighbor instead (the closest sibling
  -- already before this one, or failing that the closest already after
  -- it) inserts this window at the right point in the stack regardless of
  -- which order the table gives them in
  local id_rank = stack_order[id] or 99
  local pred_win, pred_rank = nil, -1
  local succ_win, succ_rank = nil, 100
  for other, held in pairs(wins) do
    if other ~= id and held.edge == split and is_ours(held) then
      local rank = stack_order[other] or 99
      if rank < id_rank and rank > pred_rank then
        pred_win, pred_rank = held.win, rank
      end
      if rank > id_rank and rank < succ_rank then
        succ_win, succ_rank = held.win, rank
      end
    end
  end
  local stack_win = pred_win or succ_win
  if stack_win then
    vim.api.nvim_set_current_win(stack_win)
    local before = pred_win == nil
    local after_cmd = vertical and 'belowright split' or 'belowright vsplit'
    local before_cmd = vertical and 'aboveleft split' or 'aboveleft vsplit'
    vim.cmd(before and before_cmd or after_cmd)
  else
    vim.cmd(commands[split] or 'topleft vsplit')
  end
  win = vim.api.nvim_get_current_win()
  vim.api.nvim_win_set_buf(win, buf)
  if vertical then
    vim.api.nvim_win_set_width(win, cells)
  else
    vim.api.nvim_win_set_height(win, cells)
  end
  local look = {
    winfixwidth = true,
    winfixheight = true,
    number = false,
    relativenumber = false,
    signcolumn = 'no',
    foldcolumn = '0',
    wrap = false,
  }
  for opt, value in pairs(look) do
    vim.api.nvim_set_option_value(opt, value, { win = win, scope = 'local' })
  end
  vim.bo[buf].modifiable = false
  vim.bo[buf].readonly = true
  wins[id] = { win = win, buf = buf, edge = split, entered = enter }
  vim.g.view_native_windows = wins
  local group = vim.api.nvim_create_augroup(
    'view_native_' .. id, { clear = true })
  vim.api.nvim_create_autocmd('BufWinEnter', {
    group = group,
    callback = function()
      local held = vim.g.view_native_windows or {}
      local entry = held[id]
      if entry == nil or entry.win ~= win then
        return true
      end
      if vim.api.nvim_get_current_win() ~= win or is_ours(entry) then
        return
      end
      held[id] = nil
      vim.g.view_native_windows = held
      hand_back_look(win)
      if is_scratch(buf) and vim.fn.bufwinid(buf) == -1 then
        pcall(vim.api.nvim_buf_delete, buf, { force = true })
      end
      pcall(vim.rpcnotify, channel, 'view_bridge', 'native_window_taken', id)
      return true
    end,
  })
  if not enter then
    if prev_before > 0 and prev_before ~= win_before
      and vim.api.nvim_win_is_valid(prev_before) then
      vim.api.nvim_set_current_win(prev_before)
    end
    vim.api.nvim_set_current_win(win_before)
  end
  return win
end)
if not ok then
  if win and vim.api.nvim_win_is_valid(win) then
    pcall(vim.api.nvim_win_close, win, true)
  end
  if buf then
    pcall(vim.api.nvim_buf_delete, buf, { force = true })
  end
  if not enter then
    vim.o.eventignore = ei
  end
  error(result)
end
if not enter then
  vim.o.eventignore = ei
end
return result"
);

/// The lua chunk [`EngineHandle::close_native_window`] runs inside nvim,
/// taking the window handle as its single vararg.
///
/// Guarded for the reason
/// [`SELECT_TAB_CHUNK`](super::buffers::SELECT_TAB_CHUNK) is: the window
/// can be gone by the time the key that closes it reaches nvim, and an
/// invalid handle raises rather than doing nothing.
///
/// The chunk touches nothing it did not open. It takes the buffer number
/// the open chunk recorded beside the window, and goes on only while that
/// buffer is still alive, still `buftype = nofile`, and still the one the
/// window shows; otherwise it drops the surface's entry and leaves the
/// window and its buffer where they are, because a window holding
/// somebody else's buffer is the person's, apart from its look, which the
/// tree put there and which goes back whichever way the close runs. A
/// buffer that arrives in a window takes the look the window is wearing,
/// so a file that landed in the tree's window is being read with no line
/// numbers, no sign column, no wrapping and the width the tree pinned;
/// none of that is the person's setting, because the window was the
/// tree's the whole time it was worn. That buffer number is the only
/// thing that says which buffer is view's: the scratch buffer is
/// `bufhidden = wipe`, so a file put into the window takes its place, and
/// a close that read the buffer out of the window would have
/// force-deleted the person's file with its unsaved edits in it. `force`
/// is set on the wipe because the buffer it names is view's own and has
/// nothing to lose.
///
/// The only window of the only tabpage cannot be closed at all -- `:only`
/// from inside the surface leaves the tree's window alone there and nvim
/// answers `nvim_win_close` with E444 -- so the chunk hands that window
/// back to the person: the alternate buffer if one is listed, else the
/// most recently used listed buffer, else an empty listed one, with the
/// scratch buffer wiped and the window options the open chunk set put
/// back. `nvim_create_buf` rather than `:enew`, which reuses an unnamed
/// unmodified buffer and would leave the person typing into the scratch
/// buffer the tree was drawn over. Of the options the open chunk wrote,
/// only `winfixwidth` and `winfixheight` are put back: nvim remembers the
/// rest per buffer, so a buffer this window has not shown takes the
/// person's own value from the switch itself, and a buffer it has shown
/// takes what they left it at. Both counts are read here rather than in
/// the model, whose own reading would be a round trip old by the time the
/// close ran.
///
/// The last window of any other tabpage closes as `:q` would, taking the
/// tabpage with it.
///
/// The surface's entry in `vim.g.view_native_windows` goes whichever way
/// the close runs, and the `view_native_<id>` augroup the open registered
/// goes with it. A window that survives the close is nvim's again: a
/// later open that found the handle still listed would take the person's
/// window for the tree, and a `BufWinEnter` callback left armed would
/// rewrite their window-local look with the globals the next time they
/// opened a file there.
///
/// The whole body runs under one `pcall`, and the chunk runs as a
/// notification, whose error nvim reports to its log and not to the
/// screen. The echo puts a refusal from either arm back on the screen
/// through the same `msg_show` every other engine error reaches the
/// reader by, and the order of the hand-back leaves the scratch buffer
/// standing where the buffer it would be replaced by never arrived. The
/// wipe keeps a `pcall` of its own and no echo: the window already has
/// its buffer back by then, so a refusal there costs the person nothing
/// and a message would arrive after a close they watched succeed.
///
/// [`EngineHandle::close_native_window`]: super::EngineHandle::close_native_window
pub(crate) const CLOSE_NATIVE_WINDOW_CHUNK: &str = concat!(
    "local win = ...\n",
    native_window_helpers!(),
    "\
local ok, err = pcall(function()
  local wins = vim.g.view_native_windows or {}
  local entry = nil
  for id, held in pairs(wins) do
    if held.win == win then
      entry = held
      wins[id] = nil
      pcall(vim.api.nvim_del_augroup_by_name, 'view_native_' .. id)
    end
  end
  vim.g.view_native_windows = wins
  if entry == nil or not vim.api.nvim_win_is_valid(win) then
    return
  end
  -- a window opened with `enter == false` was never a place the user's
  -- own autocmds saw the keyboard arrive, so its close must cost them
  -- nothing either
  local silent = entry.entered == false
  local ei = vim.o.eventignore
  if silent then
    vim.o.eventignore =
      'WinEnter,WinLeave,WinNew,WinClosed,BufEnter,BufLeave,BufWinEnter'
  end
  local function done()
    if silent then
      vim.o.eventignore = ei
    end
  end
  if not is_ours(entry) then
    hand_back_look(win)
    done()
    return
  end
  local scratch = entry.buf
  local tab = vim.api.nvim_win_get_tabpage(win)
  local alone = #vim.api.nvim_tabpage_list_wins(tab) == 1
    and #vim.api.nvim_list_tabpages() == 1
  if not alone then
    vim.api.nvim_win_close(win, true)
    done()
    return
  end
  local target = vim.fn.bufnr('#')
  if target < 1 or target == scratch or vim.fn.buflisted(target) == 0 then
    target = 0
    local newest = -1
    for _, info in ipairs(vim.fn.getbufinfo({ buflisted = 1 })) do
      if info.bufnr ~= scratch and info.lastused > newest then
        newest = info.lastused
        target = info.bufnr
      end
    end
    if target == 0 then
      target = vim.api.nvim_create_buf(true, false)
    end
  end
  unpin_size(win)
  if target ~= 0 then
    vim.api.nvim_win_set_buf(win, target)
  end
  if is_scratch(scratch) then
    pcall(vim.api.nvim_buf_delete, scratch, { force = true })
  end
  done()
end)
if not ok then
  vim.api.nvim_echo({ { tostring(err), 'ErrorMsg' } }, true, {})
end"
);

/// The lua chunk [`EngineHandle::set_window_size`] runs inside nvim,
/// taking the window handle, a width and a height, either of which may be
/// nil.
///
/// The axis a surface does not own is left alone rather than set to what it
/// already is, because nvim re-flows the layout around every set.
///
/// [`EngineHandle::set_window_size`]: super::EngineHandle::set_window_size
pub(crate) const SET_WINDOW_SIZE_CHUNK: &str = "\
local win, width, height = ...
if not vim.api.nvim_win_is_valid(win) then
  return
end
if width then
  vim.api.nvim_win_set_width(win, width)
end
if height then
  vim.api.nvim_win_set_height(win, height)
end";

/// The lua chunk [`EngineHandle::focus_previous_window`] runs inside nvim.
///
/// `wincmd p` and not a stored handle: nvim keeps the previous window per
/// tabpage and updates it on every window change, so a handle view
/// remembered would be stale the moment the user moved on their own.
///
/// [`EngineHandle::focus_previous_window`]: super::EngineHandle::focus_previous_window
pub(crate) const FOCUS_PREVIOUS_WINDOW_CHUNK: &str = "\
pcall(vim.cmd, 'wincmd p')";

/// The lua chunk [`EngineHandle::move_window_to_tabpage`] runs inside nvim,
/// for `:View window to_tabpage <N>`.
///
/// Everything the source window carries -- its buffer, its cursor, its
/// scroll view -- is read off `win` before any navigation runs, because
/// once the destination tabpage takes focus `win` is no longer the current
/// window and nothing about it can be read through a command that acts on
/// "the current window" any more. `tabpagenr('$')` is read in the same
/// breath, ahead of the close that can shrink it: closing `win` may take
/// its own tabpage with it (the last window of a tabpage always does),
/// which renumbers every tabpage after it, and a count read after that
/// would be answering about a layout the destination was never chosen
/// against. The destination window's own width and height, read once it is
/// current, decide the split the way `window new` decides one: a wide
/// window splits beside itself, a tall one below.
///
/// [`EngineHandle::move_window_to_tabpage`]: super::EngineHandle::move_window_to_tabpage
pub(crate) const MOVE_WINDOW_TO_TABPAGE_CHUNK: &str = "\
local win, destination = ...
if not vim.api.nvim_win_is_valid(win) then
  return
end
local ok, err = pcall(function()
  local buf = vim.api.nvim_win_get_buf(win)
  local cursor = vim.api.nvim_win_get_cursor(win)
  local view = vim.api.nvim_win_call(win, vim.fn.winsaveview)
  local last = vim.fn.tabpagenr('$')
  if destination > last then
    vim.cmd('$tabnew')
  else
    vim.cmd(destination .. 'tabnext')
  end
  local base = vim.api.nvim_get_current_win()
  local width = vim.api.nvim_win_get_width(base)
  local height = vim.api.nvim_win_get_height(base)
  vim.cmd(width >= height and 'vsplit' or 'split')
  local placed = vim.api.nvim_get_current_win()
  vim.api.nvim_win_set_buf(placed, buf)
  vim.api.nvim_win_set_cursor(placed, cursor)
  vim.api.nvim_win_call(placed, function()
    vim.fn.winrestview(view)
  end)
  vim.api.nvim_win_close(win, false)
end)
if not ok then
  vim.notify(
    'view: window to_tabpage failed: ' .. tostring(err),
    vim.log.levels.ERROR
  )
end";

impl super::EngineHandle {
    /// Opens a window for `surface`, or enters the one it already has, and
    /// routes the handle back as `Msg::NativeWindowOpened` tagged with
    /// `generation`.
    ///
    /// Async by construction, like
    /// [`list_buffers`](Self::list_buffers): the key that opens a surface
    /// is dispatched from the runtime loop, which never blocks on a reply.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited.
    pub fn open_native_window(
        &self,
        surface: view_core::native::geometry::NativeSurface,
        split: WinSplit,
        size: u16,
        generation: u64,
        enter: bool,
    ) -> Result<(), EngineError> {
        self.request_native_window(
            "nvim_exec_lua",
            vec![
                Value::from(OPEN_NATIVE_WINDOW_CHUNK),
                Value::Array(vec![
                    Value::from(surface.id()),
                    Value::from(split.word()),
                    Value::from(size),
                    Value::from(self.channel_id),
                    Value::from(enter),
                ]),
            ],
            generation,
            surface,
        )
    }

    /// Opens `surface`'s window and waits for the handle, for a caller
    /// with no pump to route the async reply through.
    ///
    /// The differential oracle is that caller: it drives `update()` from a
    /// list of effects rather than from a live loop, so an answer that
    /// comes back through the connection's pump reaches nothing. Every
    /// other caller uses
    /// [`open_native_window`](Self::open_native_window), which never
    /// blocks.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed,
    /// or a timeout error if nvim does not answer within
    /// [`OPEN_NATIVE_WINDOW_TIMEOUT`].
    pub fn open_native_window_sync(
        &self,
        surface: view_core::native::geometry::NativeSurface,
        split: WinSplit,
        size: u16,
        enter: bool,
    ) -> Result<Option<view_core::events::WinHandle>, EngineError> {
        let reply = self.request_timeout(
            "nvim_exec_lua",
            vec![
                Value::from(OPEN_NATIVE_WINDOW_CHUNK),
                Value::Array(vec![
                    Value::from(surface.id()),
                    Value::from(split.word()),
                    Value::from(size),
                    Value::from(self.channel_id),
                    Value::from(enter),
                ]),
            ],
            OPEN_NATIVE_WINDOW_TIMEOUT,
        )?;
        Ok(decode_native_window_reply(&reply))
    }

    /// Closes a window view opened for a surface of its own. A notify on
    /// [`select_tab`](Self::select_tab)'s terms: the answer a caller wants
    /// is the `win_close` nvim sends.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection's writer thread has
    /// already exited.
    pub fn close_native_window(&self, win: u64) -> Result<(), EngineError> {
        self.notify(
            "nvim_exec_lua",
            vec![
                Value::from(CLOSE_NATIVE_WINDOW_CHUNK),
                Value::Array(vec![Value::from(win)]),
            ],
        )
    }

    /// Sets `win`'s width, its height, or both. A notify on
    /// [`close_native_window`](Self::close_native_window)'s terms.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection's writer thread has
    /// already exited.
    pub fn set_window_size(
        &self,
        win: u64,
        width: Option<u16>,
        height: Option<u16>,
    ) -> Result<(), EngineError> {
        self.notify(
            "nvim_exec_lua",
            vec![
                Value::from(SET_WINDOW_SIZE_CHUNK),
                Value::Array(vec![
                    Value::from(win),
                    width.map_or(Value::Nil, Value::from),
                    height.map_or(Value::Nil, Value::from),
                ]),
            ],
        )
    }

    /// Moves the cursor back to the window it was in before this one. A
    /// notify on [`close_native_window`](Self::close_native_window)'s
    /// terms.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection's writer thread has
    /// already exited.
    pub fn focus_previous_window(&self) -> Result<(), EngineError> {
        self.notify(
            "nvim_exec_lua",
            vec![
                Value::from(FOCUS_PREVIOUS_WINDOW_CHUNK),
                Value::Array(Vec::new()),
            ],
        )
    }

    /// Moves `win` to tabpage `destination` (one-based, `tabpagenr('$')`'s
    /// own numbering, past the last tabpage creates a new one at the end),
    /// splitting the destination along its longer side and keeping the
    /// window's buffer, cursor and scroll view. A notify on
    /// [`close_native_window`](Self::close_native_window)'s terms.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection's writer thread has
    /// already exited.
    pub fn move_window_to_tabpage(&self, win: u64, destination: u32) -> Result<(), EngineError> {
        self.notify(
            "nvim_exec_lua",
            vec![
                Value::from(MOVE_WINDOW_TO_TABPAGE_CHUNK),
                Value::Array(vec![Value::from(win), Value::from(destination)]),
            ],
        )
    }
}

/// The window handle the open chunk's reply carries, or `None` for a
/// reply that named none.
///
/// A reply view cannot read degrades to no handle, the same safe default
/// `decode_buffer_list_reply` takes: the surface then draws nothing rather
/// than binding a pane to a window that may not exist.
#[must_use]
pub(crate) fn decode_native_window_reply(result: &Value) -> Option<view_core::events::WinHandle> {
    match result {
        Value::Integer(n) => n.as_u64().map(view_core::events::WinHandle),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The helpers as both chunks carry them, for the walks below.
    const NATIVE_WINDOW_HELPERS: &str = native_window_helpers!();

    /// The buffer behind a windowed surface holds nothing and takes no
    /// text. A keystroke reaches nvim before view has claimed the window,
    /// and a writable scratch buffer would take it.
    #[test]
    fn the_scratch_buffer_is_opened_unwritable() {
        for line in [
            "vim.bo[buf].modifiable = false",
            "vim.bo[buf].readonly = true",
            "vim.bo[buf].buftype = 'nofile'",
            "vim.bo[buf].bufhidden = 'wipe'",
            "vim.bo[buf].buflisted = false",
        ] {
            assert!(
                OPEN_NATIVE_WINDOW_CHUNK.contains(line),
                "the chunk no longer sets {line}"
            );
        }
        let modifiable = OPEN_NATIVE_WINDOW_CHUNK
            .find("vim.bo[buf].modifiable = false")
            .expect("the chunk sets modifiable");
        let open = OPEN_NATIVE_WINDOW_CHUNK
            .find("nvim_win_set_buf")
            .expect("the chunk puts the buffer in a window");
        assert!(
            modifiable > open,
            "modifiable is cleared before the buffer reaches a window, \
             which the window's own buffer setup would undo"
        );
    }

    /// A tree whose window is the last one on its tabpage cannot be
    /// closed, and dropping the surface without doing anything else left
    /// the person looking at an unnamed scratch window. Both arms are
    /// named here because only one of them runs per close, so a chunk
    /// that lost the fallback would still pass every test the close path
    /// has against a tabpage with two windows in it.
    #[test]
    fn the_close_chunk_hands_back_a_window_it_cannot_close() {
        for call in [
            "nvim_tabpage_list_wins",
            "nvim_list_tabpages",
            "nvim_win_close",
            "nvim_win_set_buf",
            "nvim_create_buf",
            "nvim_buf_delete",
            "nvim_buf_is_valid",
        ] {
            assert!(
                CLOSE_NATIVE_WINDOW_CHUNK.contains(call),
                "the chunk no longer calls {call}"
            );
        }
        for line in [
            "for _, opt in ipairs({ 'winfixwidth', 'winfixheight' }) do",
            "vim.api.nvim_set_option_value(opt, false, { win = win, scope = 'local' })",
        ] {
            assert!(
                CLOSE_NATIVE_WINDOW_CHUNK.contains(line),
                "a window handed back keeps the width the tree pinned it \
                 to: {line}"
            );
        }
        assert!(
            !CLOSE_NATIVE_WINDOW_CHUNK.contains("vim.wo["),
            "a window option written through `vim.wo` writes nvim's global \
             value too, so the hand-back would take the person's own look \
             away from every window they open next"
        );
        let opened = CLOSE_NATIVE_WINDOW_CHUNK
            .find("{ win = win, scope = 'local' })")
            .expect("the chunk writes the window's options");
        let switched = CLOSE_NATIVE_WINDOW_CHUNK
            .find("nvim_win_set_buf")
            .expect("the chunk puts a buffer back in the window");
        assert!(
            opened < switched,
            "the options are written after the buffer switch, which is \
             where the autocommands it fires have the last word"
        );
        let body = CLOSE_NATIVE_WINDOW_CHUNK
            .find("local ok, err = pcall(function()")
            .expect("the whole body runs under one pcall");
        let echo = CLOSE_NATIVE_WINDOW_CHUNK
            .find("vim.api.nvim_echo({ { tostring(err), 'ErrorMsg' } }, true, {})")
            .expect("a refusal reaches the screen");
        assert!(
            body < echo && CLOSE_NATIVE_WINDOW_CHUNK.matches("pcall(").count() == 3,
            "an arm outside the body's pcall reports its refusal to nvim's \
             log alone, which is where this chunk's errors are invisible, \
             and the wipe and the augroup delete keep one each so a \
             refusal there says nothing after a close the person watched \
             succeed"
        );
        assert!(
            CLOSE_NATIVE_WINDOW_CHUNK
                .contains("pcall(vim.api.nvim_buf_delete, scratch, { force = true })"),
            "the wipe no longer names the buffer the open chunk recorded"
        );
        for line in [
            "local scratch = entry.buf",
            "if not is_ours(entry) then",
            "if entry == nil or not vim.api.nvim_win_is_valid(win) then",
        ] {
            assert!(
                CLOSE_NATIVE_WINDOW_CHUNK.contains(line),
                "the chunk reaches a buffer it did not open, and a file \
                 somebody else put in the window is force-deleted with its \
                 unsaved edits in it: {line}"
            );
        }
        assert!(
            CLOSE_NATIVE_WINDOW_CHUNK
                .contains("pcall(vim.api.nvim_del_augroup_by_name, 'view_native_' .. id)"),
            "the callback the open armed outlives the close, and the next \
             file the person opens in their own window is read with the \
             globals written over their window-local look"
        );
        let dropped = CLOSE_NATIVE_WINDOW_CHUNK
            .find("pcall(vim.api.nvim_del_augroup_by_name")
            .expect("the chunk deletes the surface's augroup");
        let arms = CLOSE_NATIVE_WINDOW_CHUNK
            .find("if not is_ours(entry) then")
            .expect("the chunk asks whether the window is still view's");
        assert!(
            dropped < arms,
            "an arm that returns before the augroup goes leaves the \
             callback armed on a window view has handed back"
        );
        let listed = CLOSE_NATIVE_WINDOW_CHUNK
            .find("wins[id] = nil")
            .expect("the chunk forgets the surface's window");
        let close = CLOSE_NATIVE_WINDOW_CHUNK
            .find("nvim_tabpage_list_wins")
            .expect("the chunk reads the tabpage's windows");
        assert!(
            listed < close,
            "a window that survives the close stays in the table the open \
             chunk enters, which gives the person's own window to the tree"
        );
    }

    /// The tree's window wears a look of its own, and nvim writes the
    /// global value of a window option alongside the local one unless the
    /// write names `scope = 'local'`. Opening the tree took the person's
    /// `number`, `signcolumn` and `wrap` away from every window they
    /// opened for the rest of the session.
    #[test]
    fn the_open_chunk_writes_the_windows_look_and_no_global() {
        assert!(
            !OPEN_NATIVE_WINDOW_CHUNK.contains("vim.wo["),
            "a window option written through `vim.wo` writes nvim's global \
             value too"
        );
        assert!(
            OPEN_NATIVE_WINDOW_CHUNK.contains(
                "vim.api.nvim_set_option_value(opt, value, { win = win, scope = 'local' })"
            ),
            "the chunk no longer writes the window's look locally"
        );
        for opt in [
            "winfixwidth",
            "winfixheight",
            "number",
            "relativenumber",
            "signcolumn",
            "foldcolumn",
            "wrap",
        ] {
            assert!(
                OPEN_NATIVE_WINDOW_CHUNK.contains(opt),
                "the tree's window no longer takes {opt}"
            );
        }
        for line in [
            "wins[id] = { win = win, buf = buf, edge = split, entered = enter }",
            "if is_ours(live) then",
        ] {
            assert!(
                OPEN_NATIVE_WINDOW_CHUNK.contains(line),
                "a window is the surface's by position alone, so a file \
                 somebody else put in it is entered as the tree: {line}"
            );
        }
    }

    /// The window is the surface's only while it holds the surface's own
    /// buffer, and nvim names no event for the moment that stops being
    /// true. The open registers one, and the report, the hand-back and
    /// the entry it drops all belong to the same firing: a chunk that
    /// reported without dropping the entry would have the next close act
    /// on a window that is the person's.
    #[test]
    fn the_open_chunk_reports_a_window_nvim_gives_to_something_else() {
        for line in [
            "vim.api.nvim_create_autocmd('BufWinEnter', {",
            "if entry == nil or entry.win ~= win then",
            "if vim.api.nvim_get_current_win() ~= win or is_ours(entry) then",
            "held[id] = nil",
            "if is_scratch(buf) and vim.fn.bufwinid(buf) == -1 then",
            "pcall(vim.rpcnotify, channel, 'view_bridge', 'native_window_taken', id)",
            "return true",
        ] {
            assert!(
                OPEN_NATIVE_WINDOW_CHUNK.contains(line),
                "the window nvim gave to a file goes on painting the \
                 surface over it: {line}"
            );
        }
        assert!(
            OPEN_NATIVE_WINDOW_CHUNK.starts_with("local id, split, pct, channel, enter = ..."),
            "the chunk takes no channel to report the window on"
        );
        let dropped = OPEN_NATIVE_WINDOW_CHUNK
            .find("held[id] = nil")
            .expect("the callback drops the surface's entry");
        let reported = OPEN_NATIVE_WINDOW_CHUNK
            .find("'native_window_taken'")
            .expect("the callback reports the window");
        assert!(
            dropped < reported,
            "the entry outlives the report, so a close that follows acts \
             on a window that is the person's"
        );
    }

    /// One spelling of "is this window still the surface's", expanded
    /// into both chunks and read at all three sites. The open chunk's
    /// re-entry guard tested the recorded buffer number alone, which a
    /// `:edit` with autocommands suppressed turns into the person's own
    /// file, and the next open then entered their window and painted the
    /// tree over it.
    #[test]
    fn one_identity_rule_answers_for_every_window_view_opened() {
        for chunk in [OPEN_NATIVE_WINDOW_CHUNK, CLOSE_NATIVE_WINDOW_CHUNK] {
            assert_eq!(
                chunk.matches("local function is_ours(entry)").count(),
                1,
                "a chunk carrying the rule twice can carry two of them"
            );
            assert!(
                chunk.contains(NATIVE_WINDOW_HELPERS),
                "a chunk spelling the rule its own way drifts from the other"
            );
        }
        assert_eq!(
            OPEN_NATIVE_WINDOW_CHUNK.matches("is_ours(").count()
                + CLOSE_NATIVE_WINDOW_CHUNK.matches("is_ours(").count(),
            6,
            "the rule is defined twice and asked at four sites: the \
             re-entry guard, the shared-edge stacking scan, the \
             `BufWinEnter` callback and the close chunk's own arm"
        );
        assert!(
            NATIVE_WINDOW_HELPERS.contains("vim.api.nvim_exec_autocmds('FileType',"),
            "the hand-back writes the globals over an ftplugin's own \
             `setlocal`, and the person reads their file without the look \
             their config gives that filetype"
        );
        let restored = NATIVE_WINDOW_HELPERS
            .find("{ scope = 'global' }")
            .expect("the hand-back restores the globals");
        let refired = NATIVE_WINDOW_HELPERS
            .find("nvim_exec_autocmds('FileType',")
            .expect("the hand-back re-fires the buffer's FileType");
        assert!(
            restored < refired,
            "the globals are written after the filetype's own values, \
             which is the overwrite this re-fire exists to undo"
        );
        for chunk in [OPEN_NATIVE_WINDOW_CHUNK, CLOSE_NATIVE_WINDOW_CHUNK] {
            assert!(
                chunk.contains("hand_back_look(win)"),
                "a hand-back arm that writes the look itself is a second \
                 spelling of the rule"
            );
        }
    }

    /// A split nvim refuses (no room, or an `E11` from inside the
    /// command-line window) used to leave `eventignore` set for the rest
    /// of the session, since the option was written before the `vim.cmd`
    /// that could raise and restored only on the path where it did not.
    /// The whole split-and-configure step now runs under one `pcall`
    /// between the write and the restore, and a refusal deletes the
    /// scratch buffer it had already created rather than leaving it
    /// behind for nothing to ever open. The failure branch's own close and
    /// delete run before its own restore, so the window it undoes still
    /// closes under the same ignore its (never-fired) open would have --
    /// restoring first left a real `WinClosed` autocmd firing for a window
    /// whose `WinNew` had been suppressed.
    #[test]
    fn a_refused_open_restores_eventignore_and_deletes_its_own_buffer() {
        let guard = OPEN_NATIVE_WINDOW_CHUNK
            .find("if vim.fn.getcmdwintype() ~= '' then")
            .expect("the chunk refuses to split from inside the command-line window");
        let written = OPEN_NATIVE_WINDOW_CHUNK
            .find("local ei = vim.o.eventignore")
            .expect("the chunk still saves eventignore before overriding it");
        assert!(
            guard < written,
            "the command-line-window guard runs after eventignore is \
             already overridden, so the one caller it exists for still \
             pays for a value never restored"
        );
        let pcall_open = OPEN_NATIVE_WINDOW_CHUNK
            .find("local ok, result = pcall(function()")
            .expect("the split-and-configure step runs under one pcall");
        assert!(
            written < pcall_open,
            "eventignore is overridden outside the pcall it is meant to \
             be undone by"
        );
        assert!(
            OPEN_NATIVE_WINDOW_CHUNK.contains("if not ok then"),
            "the chunk no longer tells a failed open from a successful one"
        );
        let ok_check = OPEN_NATIVE_WINDOW_CHUNK
            .find("if not ok then")
            .expect("the chunk checks the pcall's own result");
        assert!(
            OPEN_NATIVE_WINDOW_CHUNK
                .contains("pcall(vim.api.nvim_buf_delete, buf, { force = true })")
                && OPEN_NATIVE_WINDOW_CHUNK
                    .matches("nvim_buf_delete, buf, { force = true })")
                    .count()
                    == 2,
            "a failed open no longer deletes the scratch buffer it had \
             already created -- one instance in the BufWinEnter callback's \
             own cleanup and one in the failure branch"
        );
        let win_close = OPEN_NATIVE_WINDOW_CHUNK
            .find("pcall(vim.api.nvim_win_close, win, true)")
            .expect(
                "a failed open no longer closes the split it had already \
                 made before the failure -- an option written on the new \
                 window after the split is the reachable case",
            );
        assert!(
            ok_check < win_close,
            "the window close runs before the chunk even knows the open \
             failed"
        );
        let restore_on_failure = OPEN_NATIVE_WINDOW_CHUNK
            .find("vim.o.eventignore = ei")
            .expect("the failure branch restores eventignore before re-raising");
        assert!(
            pcall_open < restore_on_failure,
            "the restore runs before the pcall it is supposed to follow"
        );
        assert!(
            win_close < restore_on_failure,
            "eventignore is restored before the failure branch closes the \
             window it just opened, so that close fires a WinClosed \
             autocmd for a window whose WinNew was ignored"
        );
        let error_call = OPEN_NATIVE_WINDOW_CHUNK
            .rfind("error(result)")
            .expect("a failed open still re-raises its own error");
        assert!(
            win_close < error_call,
            "the window close runs after the failure is already re-raised, \
             so it never runs at all"
        );
        assert!(
            restore_on_failure < error_call,
            "the failure is re-raised before eventignore is put back, so a \
             caller catching it still finds the override standing"
        );
        let restore_on_success = OPEN_NATIVE_WINDOW_CHUNK
            .rfind("vim.o.eventignore = ei")
            .expect("the success path restores eventignore too");
        assert!(
            error_call < restore_on_success,
            "the success-path restore is not its own statement past the \
             failure branch, so it never runs when the open actually \
             succeeded"
        );
    }

    /// Two surfaces anchored to the same edge split that edge's own window
    /// rather than each claiming a further column or row of the tabpage:
    /// the second one's open scans the table for a live window sharing its
    /// `edge` and, when it finds one, enters it and splits across the axis
    /// that puts the two windows one above the other on a vertical edge
    /// (`belowright split`) or side by side on a horizontal one
    /// (`belowright vsplit`).
    #[test]
    fn two_sidebars_on_one_edge_stack_vertically_when_windowed() {
        assert!(
            OPEN_NATIVE_WINDOW_CHUNK.contains("held.edge == split"),
            "the chunk no longer scans for a window sharing this edge"
        );
        assert!(
            OPEN_NATIVE_WINDOW_CHUNK
                .contains("wins[id] = { win = win, buf = buf, edge = split, entered = enter }"),
            "an opened window forgets which edge it was anchored to"
        );
        for line in ["'belowright split'", "'belowright vsplit'"] {
            assert!(
                OPEN_NATIVE_WINDOW_CHUNK.contains(line),
                "a shared-edge stack no longer splits with {line}"
            );
        }
        let scan = OPEN_NATIVE_WINDOW_CHUNK
            .find("for other, held in pairs(wins) do")
            .expect("the chunk scans the table for a shared edge");
        let split = OPEN_NATIVE_WINDOW_CHUNK
            .find("if stack_win then")
            .expect("the chunk branches on whether it found one");
        assert!(
            scan < split,
            "the split command runs before the scan that decides which one \
             to use, so a shared edge is never stacked"
        );
    }

    /// A third surface sharing an edge with two already-open ones has to
    /// land at its own `stack_order` position among all of them, not
    /// beside whichever one `pairs()` (an unordered table walk) happens to
    /// yield first: the scan keeps the nearest ranked neighbor on each
    /// side rather than breaking on the first match, so the stack order is
    /// the same regardless of Lua's own table iteration order.
    #[test]
    fn a_third_surface_on_one_edge_stacks_by_rank_not_table_order() {
        assert!(
            !OPEN_NATIVE_WINDOW_CHUNK.contains("      break\n"),
            "the scan still stops at the first sibling `pairs()` yields \
             instead of finding the nearest ranked neighbor"
        );
        for needle in ["pred_win", "succ_win", "pred_rank", "succ_rank"] {
            assert!(
                OPEN_NATIVE_WINDOW_CHUNK.contains(needle),
                "the chunk no longer tracks a nearest predecessor/successor \
                 by stack_order ({needle} missing)"
            );
        }
        assert!(
            OPEN_NATIVE_WINDOW_CHUNK.contains("local stack_win = pred_win or succ_win"),
            "the chosen neighbor no longer prefers a nearer-ranked \
             predecessor over an arbitrary sibling"
        );
    }

    #[test]
    fn a_reply_that_names_no_handle_binds_nothing() {
        assert_eq!(
            decode_native_window_reply(&Value::from(7_u64)),
            Some(view_core::events::WinHandle(7))
        );
        assert_eq!(decode_native_window_reply(&Value::Nil), None);
        assert_eq!(
            decode_native_window_reply(&Value::from("not a window")),
            None
        );
    }
}
