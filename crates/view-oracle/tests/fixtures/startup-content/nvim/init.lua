-- A window with nothing in it, so the chrome frame and the content frame
-- are two different frames and the test can act between them: an empty
-- buffer, the end-of-buffer fill blanked, no status line or command line
-- under it, and nvim's intro screen off. Every cell of the window grid is
-- a blank until the test types into it, so a startup line that fires on
-- the chrome frame fires while the window is provably empty.
vim.o.cmdheight = 0
vim.o.laststatus = 0
vim.o.number = false
vim.o.relativenumber = false
vim.opt.shortmess:append("I")
vim.opt.fillchars = { eob = " " }
vim.api.nvim_buf_set_lines(0, 0, -1, false, { "" })
