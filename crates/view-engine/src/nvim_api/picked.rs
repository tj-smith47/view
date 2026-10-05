//! Opening the result a picker key chose.

use view_core::msg::OpenIn;

/// Opens a picker result, taking `(name, how, line, buffer)` as varargs:
/// `how` is the ex command a file opens with (`edit`, `vsplit`, `split` or
/// `tabedit`), `line` is `0` for none, and `buffer` says `name` names a
/// listed buffer. Constant, like every other chunk here: no caller data is
/// interpolated into the source.
///
/// A file reaches `nvim_cmd` as an argument with filename magic off, the
/// form [`OPEN_FILE_CHUNK`](super::OPEN_FILE_CHUNK) takes, so a space, `%`,
/// `#` or a leading `+` in its name is no command syntax.
///
/// A buffer is found by its exact name among the loaded, listed buffers the
/// picker's list was read from, and switched to by handle: a buffer with no
/// file behind it (a terminal, an unnamed scratch) has nothing `:edit` could
/// open. The buffer can be gone by the time the key lands, and then nothing
/// opens.
///
/// The line is held to the buffer's last, since the file can have shrunk
/// since a grep match was read off disk.
pub(super) const OPEN_PICKED_CHUNK: &str = "\
local name, how, line, buffer = ...
if buffer then
  local target
  for _, buf in ipairs(vim.api.nvim_list_bufs()) do
    if vim.api.nvim_buf_is_loaded(buf) and vim.bo[buf].buflisted
        and vim.api.nvim_buf_get_name(buf) == name then
      target = buf
      break
    end
  end
  if target == nil then
    return
  end
  local split = { vsplit = 'vertical sbuffer ', split = 'sbuffer ',
    tabedit = 'tab sbuffer ' }
  vim.cmd((split[how] or 'buffer ') .. target)
else
  vim.api.nvim_cmd({
    cmd = how, args = { name }, magic = { file = false, bar = false },
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
    /// Opens `name` in the window `how` names via [`OPEN_PICKED_CHUNK`],
    /// the cursor on `line` when there is one. A listed buffer's name when
    /// `buffer`, a file's path otherwise. A notify, with no reply awaited:
    /// the picker that issued it closes on the same keypress.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection's writer thread has
    /// already exited.
    pub fn open_picked(
        &self,
        name: &str,
        line: Option<u64>,
        buffer: bool,
        how: OpenIn,
    ) -> Result<(), crate::handle::EngineError> {
        self.notify(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from(OPEN_PICKED_CHUNK),
                rmpv::Value::Array(vec![
                    rmpv::Value::from(name),
                    rmpv::Value::from(command(how)),
                    rmpv::Value::from(line.unwrap_or(0)),
                    rmpv::Value::from(buffer),
                ]),
            ],
        )
    }
}
