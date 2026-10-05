//! Opening the result a picker key chose.

use view_core::msg::OpenIn;
use view_core::native::picker::Picked;

/// Opens a picker result, taking `(path, how, line, buffer)` as varargs:
/// `how` is the ex command a file opens with (`edit`, `vsplit`, `split` or
/// `tabedit`), `line` is `0` for none, and `buffer` is the handle of a
/// listed buffer to open, `0` for a file. Constant, like every other chunk
/// here: no caller data is interpolated into the source.
///
/// A file reaches `nvim_cmd` as an argument with filename magic off, the
/// form [`OPEN_FILE_CHUNK`](super::OPEN_FILE_CHUNK) takes, so a space, `%`,
/// `#` or a leading `+` in its name is no command syntax.
///
/// A buffer is switched to by its handle, so two buffers whose names read
/// alike (two unnamed scratches) stay apart, and a buffer with no file
/// behind it opens where `:edit` has nothing to open. The buffer can be
/// gone or unlisted by the time the key lands, and then nothing opens.
///
/// The line is held to the buffer's last, since the file can have shrunk
/// since a grep match was read off disk.
pub(super) const OPEN_PICKED_CHUNK: &str = "\
local path, how, line, buffer = ...
if buffer > 0 then
  if not vim.api.nvim_buf_is_valid(buffer)
      or not vim.bo[buffer].buflisted then
    return
  end
  local split = { vsplit = 'vertical sbuffer ', split = 'sbuffer ',
    tabedit = 'tab sbuffer ' }
  vim.cmd((split[how] or 'buffer ') .. buffer)
else
  vim.api.nvim_cmd({
    cmd = how, args = { path }, magic = { file = false, bar = false },
  }, {})
end
if line > 0 then
  local last = vim.api.nvim_buf_line_count(0)
  vim.api.nvim_win_set_cursor(0, { math.min(line, last), 0 })
end";

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
    /// listed buffer by its handle. A notify, with no reply awaited: the
    /// picker that issued it closes on the same keypress.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection's writer thread has
    /// already exited.
    pub fn open_picked_target(
        &self,
        target: &Picked,
        how: OpenIn,
    ) -> Result<(), crate::handle::EngineError> {
        let (path, line, buffer) = match target {
            Picked::File { path, line } => (path.as_str(), line.unwrap_or(0), 0),
            Picked::Buffer { handle } => ("", 0, *handle),
            _ => return Ok(()),
        };
        self.notify(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from(OPEN_PICKED_CHUNK),
                rmpv::Value::Array(vec![
                    rmpv::Value::from(path),
                    rmpv::Value::from(command(how)),
                    rmpv::Value::from(line),
                    rmpv::Value::from(buffer),
                ]),
            ],
        )
    }
}
