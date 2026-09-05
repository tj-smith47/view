-- The user's startup shape, reduced to the two things that produce it: a
-- config that draws to the screen while it is still sourcing, and one that
-- opens its windows on VimEnter.
--
-- The redraw is conditional on ext_popupmenu for the same reason the real
-- one is: noice answers a GUI attach with a notification, nvim-notify
-- animates it, and the animation redraws mid-source. A TUI attach sets none
-- of those externalized surfaces, gets no notification, and so redraws
-- nothing before VimEnter -- which is why nvim's own first screen is the
-- post-VimEnter one and view's was not.
-- every line, not the first alone: view's own first-run notices box over
-- the top rows of a small terminal, and a marker only they could cover
-- would read as a frame that was never drawn
local startup = {}
for _ = 1, 40 do
  table.insert(startup, "PREVIMENTERBUFFER")
end
vim.api.nvim_buf_set_lines(0, 0, -1, false, startup)

-- offered over a window rather than once, so no single moment of view's
-- own startup can be the reason a frame did or did not reach the terminal:
-- whatever else it is doing, its paint loop is running for most of this.
local ui = vim.api.nvim_list_uis()[1]
if ui and ui.ext_popupmenu then
  for _ = 1, 60 do
    vim.cmd("redraw")
    vim.uv.sleep(10)
  end
end

vim.api.nvim_create_autocmd("VimEnter", {
  once = true,
  callback = function()
    -- the startup buffer leaves the screen, so its text on the terminal is
    -- proof of a frame nvim's own TUI never wrote
    vim.cmd("enew")
    vim.api.nvim_buf_set_lines(0, 0, -1, false, { "POSTVIMENTERLEFT" })
    vim.cmd("vsplit")
    local scratch = vim.api.nvim_create_buf(false, true)
    vim.api.nvim_buf_set_lines(scratch, 0, -1, false, { "POSTVIMENTERSPLIT" })
    vim.api.nvim_win_set_buf(0, scratch)
  end,
})
