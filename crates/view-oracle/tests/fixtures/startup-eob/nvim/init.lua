-- A one-line buffer with nothing drawn under it: `cmdheight` and
-- `laststatus` at 0 and the end-of-buffer fill blanked leave the first row
-- the only text on the screen, with the cursor at its top-left corner --
-- the closest a half-built buffer comes to the shape of a startup prompt.
-- The line is repeated across the row so that a frame view lets through
-- reaches the terminal past anything its own notices draw over the corner.
vim.o.cmdheight = 0
vim.o.laststatus = 0
vim.opt.fillchars = { eob = " " }
vim.api.nvim_buf_set_lines(0, 0, -1, false, { string.rep("ONLYLINE ", 8) })

-- keyed on the attach being a remote one rather than on any externalized
-- surface, so the loop runs under every permutation the editor under test
-- can be configured into and under none of the reference TUI's
local ui = vim.api.nvim_list_uis()[1]
if ui and not ui.stdout_tty then
  for _ = 1, 20 do
    vim.api.nvim__redraw({ valid = false, flush = true })
    vim.uv.sleep(10)
  end
end

vim.api.nvim_create_autocmd("VimEnter", {
  once = true,
  callback = function()
    vim.cmd("enew")
    vim.api.nvim_buf_set_lines(0, 0, -1, false, { "POSTVIMENTERLEFT" })
    vim.cmd("vsplit")
    local scratch = vim.api.nvim_create_buf(false, true)
    vim.api.nvim_buf_set_lines(scratch, 0, -1, false, { "POSTVIMENTERSPLIT" })
    vim.api.nvim_win_set_buf(0, scratch)
  end,
})
