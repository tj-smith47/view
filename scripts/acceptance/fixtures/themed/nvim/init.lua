-- The visual sweep's engine config: a colorscheme and nothing else, so a
-- box drawn wrong is view's own doing and not a plugin's.
--
-- `cursorline` is on deliberately. The reported defect was an overlay
-- letting the cursor row's full-width highlight read through it, and a
-- fixture with no such highlight cannot show that.
vim.o.termguicolors = true
vim.o.cursorline = true
vim.cmd.colorscheme('view-dracula')
