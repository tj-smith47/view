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

/// The lua chunk [`EngineHandle::open_native_window`] runs inside nvim,
/// taking the surface id, the split word and the size in percent.
///
/// One window per surface, kept in a table on the module's own upvalue: a
/// second call for a surface that already has a live window enters that
/// window instead of opening another, which is what makes one message both
/// "open it" and "go to it". The table is written into `vim.g` rather than
/// a Lua local because each `nvim_exec_lua` call is its own chunk.
///
/// The buffer is scratch and stays that way: `buftype = nofile` so nothing
/// writes it, `bufhidden = wipe` so closing the window takes the buffer
/// with it, `buflisted = false` so it never reaches the pill or a picker,
/// and `modifiable = false` with `readonly` set so a keystroke that reaches
/// nvim before view has claimed the window cannot put a character in it.
/// `winfixwidth` and `winfixheight` keep nvim's own layout from re-flowing
/// the sidebar when another window opens beside it.
///
/// The window is made with `:split` rather than `nvim_open_win`, which
/// allocates a second grid under `ext_multigrid` and leaves it behind: view
/// then holds one more grid than nvim has windows.
///
/// The returned value is the window handle, which the reply decodes into
/// `Msg::NativeWindowOpened`.
///
/// [`EngineHandle::open_native_window`]: super::EngineHandle::open_native_window
pub(crate) const OPEN_NATIVE_WINDOW_CHUNK: &str = "\
local id, split, pct = ...
local wins = vim.g.view_native_windows or {}
local live = wins[id]
if live and vim.api.nvim_win_is_valid(live) then
  vim.api.nvim_set_current_win(live)
  return live
end
local buf = vim.api.nvim_create_buf(false, true)
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
vim.cmd(commands[split] or 'topleft vsplit')
local win = vim.api.nvim_get_current_win()
vim.api.nvim_win_set_buf(win, buf)
if vertical then
  vim.api.nvim_win_set_width(win, cells)
else
  vim.api.nvim_win_set_height(win, cells)
end
vim.wo[win].winfixwidth = true
vim.wo[win].winfixheight = true
vim.wo[win].number = false
vim.wo[win].relativenumber = false
vim.wo[win].signcolumn = 'no'
vim.wo[win].foldcolumn = '0'
vim.wo[win].wrap = false
vim.bo[buf].modifiable = false
vim.bo[buf].readonly = true
wins[id] = win
vim.g.view_native_windows = wins
return win";

/// The lua chunk [`EngineHandle::close_native_window`] runs inside nvim,
/// taking the window handle as its single vararg.
///
/// Guarded for the reason
/// [`SELECT_TAB_CHUNK`](super::buffers::SELECT_TAB_CHUNK) is: the window
/// can be gone by the time the key that closes it reaches nvim, and an
/// invalid handle raises rather than doing nothing. `force` is set because
/// the buffer is scratch and has nothing to lose.
///
/// The close can still be refused -- `:only` from inside the surface
/// leaves its window the last one and nvim answers E444 -- and this runs
/// as a notification, whose error nvim reports to its log and not to the
/// screen. The `pcall` puts it back on the screen through the same
/// `msg_show` every other engine error reaches the reader by.
///
/// [`EngineHandle::close_native_window`]: super::EngineHandle::close_native_window
pub(crate) const CLOSE_NATIVE_WINDOW_CHUNK: &str = "\
local win = ...
if vim.api.nvim_win_is_valid(win) then
  local ok, err = pcall(vim.api.nvim_win_close, win, true)
  if not ok then
    vim.api.nvim_echo({ { tostring(err), 'ErrorMsg' } }, true, {})
  end
end";

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
    ) -> Result<(), EngineError> {
        self.request_native_window(
            "nvim_exec_lua",
            vec![
                Value::from(OPEN_NATIVE_WINDOW_CHUNK),
                Value::Array(vec![
                    Value::from(surface.id()),
                    Value::from(split.word()),
                    Value::from(size),
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
    ) -> Result<Option<view_core::events::WinHandle>, EngineError> {
        let reply = self.request_timeout(
            "nvim_exec_lua",
            vec![
                Value::from(OPEN_NATIVE_WINDOW_CHUNK),
                Value::Array(vec![
                    Value::from(surface.id()),
                    Value::from(split.word()),
                    Value::from(size),
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
