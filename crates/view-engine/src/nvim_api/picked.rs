//! Opening a file or a buffer the picker or the file tree chose.

use view_core::msg::OpenIn;
use view_core::native::picker::Picked;

/// Opens a chosen file or buffer, taking `(path, how, line, buffer,
/// previous)` as varargs: `how` is the ex command a file opens with
/// (`edit`, `vsplit`, `split` or `tabedit`), `line` is `0` for none,
/// `buffer` is the handle of a listed buffer to open, `0` for a file, and
/// `previous` enters the window nvim had focused before the current one
/// first. Constant, like every other chunk here: no caller data is
/// interpolated into the source.
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
/// read off disk. The screen is drawn before the reply, so a key replayed
/// on the reply is routed by the window the open left focused.
pub(super) const OPEN_PICKED_CHUNK: &str = "\
local path, how, line, buffer, previous = ...
if previous then
  pcall(vim.cmd, 'wincmd p')
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
    return
  end
  local split = { vsplit = 'vertical sbuffer ', split = 'sbuffer ',
    tabedit = 'tab sbuffer ' }
  ok, err = pcall(vim.cmd, (split[how] or 'buffer ') .. buffer)
elseif not vim.uv.fs_stat(path) then
  say(path .. ' no longer exists')
  return
else
  ok, err = pcall(vim.api.nvim_cmd, {
    cmd = how, args = { path }, magic = { file = false, bar = false },
  }, {})
end
if not ok then
  say(tostring(err):match('E%d+:.*') or tostring(err))
  return
end
if line > 0 then
  local last = vim.api.nvim_buf_line_count(0)
  vim.api.nvim_win_set_cursor(0, { math.min(line, last), 0 })
end
vim.cmd.redraw()";

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
    /// listed buffer by its handle. With `previous_window`, the window nvim
    /// had focused before the current one is entered first.
    ///
    /// Async: nvim's answer is routed as `Msg::PickedOpened` carrying
    /// `generation`, whatever the open did. nvim reads `nvim_input` ahead
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
        previous_window: bool,
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
                    rmpv::Value::from(previous_window),
                ]),
            ],
            crate::handle::Waiter::Opened { generation },
        )
    }
}
