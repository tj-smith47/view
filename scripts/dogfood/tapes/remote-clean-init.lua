-- WHY: remote-editing.sh's own fixture, copied to the remote host for the
-- one recording. A daily-driver config on a remote host carries whatever
-- that host's LSP and formatter installs are doing that day, and a Lua
-- traceback and eight "failed to install" toasts once filled the whole
-- recording. This init loads no plugin manager, so nothing on it can fail
-- to install.
--
-- The colorscheme is the one the local config sets, read off it by
-- remote-editing.sh and shipped beside this file, so the far side is
-- themed the way the person's own editor is and view's chrome, which
-- follows the colorscheme, matches it. habamax stands in when that
-- scheme cannot load.
vim.o.termguicolors = true
vim.o.number = true
vim.o.ruler = true
-- the tape types into the file and never saves: no swap file or shada
-- entry is left on the remote host to raise a prompt on the next run
vim.o.swapfile = false
vim.o.shadafile = "NONE"

local dir = vim.env.VIEW_DOGFOOD_THEME_DIR
if dir and dir ~= "" then
  vim.opt.rtp:prepend(dir)
end
local theme = vim.env.VIEW_DOGFOOD_THEME
if not (theme and theme ~= "" and pcall(vim.cmd.colorscheme, theme)) then
  vim.cmd.colorscheme("habamax")
end

-- a local config that clears Normal's background does it in a setup()
-- call this init never runs, so the far side replays the result
if vim.env.VIEW_DOGFOOD_TRANSPARENT == "1" then
  for _, group in ipairs({ "Normal", "NormalNC" }) do
    local hl = vim.api.nvim_get_hl(0, { name = group, link = false })
    hl.bg = nil
    hl.ctermbg = nil
    vim.api.nvim_set_hl(0, group, hl)
  end
end
