-- Intentionally intrusive config: paints a marker into the grid so any
-- spawn path that sources user config becomes visible to the decoded
-- screen. The embedded engine must never source this (it spawns with
-- --clean); a test asserts the marker's absence.
local buf = vim.api.nvim_create_buf(false, true)
vim.api.nvim_buf_set_lines(buf, 0, -1, false, { "CONFIG-LEAKED" })
vim.api.nvim_open_win(buf, false, {
  relative = "editor",
  width = 14,
  height = 1,
  row = 1,
  col = 1,
  style = "minimal",
})
