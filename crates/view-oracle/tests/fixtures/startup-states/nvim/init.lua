-- The user's startup shape, reduced to the two things that produce it: a
-- config that draws to the screen while it is still sourcing, and one that
-- opens its windows on VimEnter.
--
-- The flush is the call nvim-notify makes to animate the notification noice
-- raises at a GUI attach, and it is conditional on ext_multigrid because
-- every one of view's [native] permutations attaches that and no TUI
-- attaches it: gating on a surface a switch can turn off would make this
-- config draw nothing mid-source for the very permutations the hold has to
-- cover. A TUI attach redraws nothing before VimEnter either way, which is
-- why nvim's own first screen is the post-VimEnter one and view's was not.
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
-- keyed on the attach being a remote one rather than on any externalized
-- surface, so the loop runs under every permutation the editor under test
-- can be configured into -- `single_grid = true` externalizes nothing at
-- all -- and under none of the reference TUI's.
local ui = vim.api.nvim_list_uis()[1]
if ui and not ui.stdout_tty then
  for _ = 1, 60 do
    vim.api.nvim__redraw({ valid = false, flush = true })
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
