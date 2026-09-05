-- A config that asks the user something while it is still sourcing. On a
-- session that left both the cmdline and the messages with nvim, the prompt
-- is drawn into nvim's own message area and nowhere else, so a hold that
-- keeps that area off the terminal is a hang until its cap. `cmdheight` at
-- 2 puts the prompt one row above the bottom of the screen, with the last
-- row blank under it: the shape a reading of the last row alone misses.
vim.o.cmdheight = 2

vim.fn.input("PROMPTHERE: ")

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
