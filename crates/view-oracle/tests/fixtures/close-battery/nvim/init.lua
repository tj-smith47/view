-- The highlight set the close battery's colour comparison needs to
-- discriminate, shaped after the configuration the stripe was reported
-- from: a side panel that is dark while the cursor is elsewhere and takes
-- the ordinary background the moment the cursor lands in it, with the
-- column between the windows painted in the panel's own colour.
--
-- No colourscheme, no plugins: the battery's text comparison is against the
-- pinned nvim on the same config, so anything here that moved a character
-- would be measuring this file rather than the editor.
vim.o.termguicolors = true

local function apply()
  -- No background of its own, so an ordinary cell is painted in the
  -- terminal's default -- which is what makes a cell that came out wearing
  -- some other layer's background visible at all. A scheme that names a
  -- background for every group hides exactly the defect this discriminates.
  vim.api.nvim_set_hl(0, "Normal", { fg = "#f8f8f2" })

  -- Distinct from Normal, so a window the cursor is not in is a different
  -- colour from the one it is in.
  vim.api.nvim_set_hl(0, "NormalNC", { fg = "#f8f8f2", bg = "#1e1f29" })

  -- What the panel window sets through `winhighlight` for the state it is
  -- not focused in, and the colour a closing neighbour used to leave a
  -- one-column band of down the height of the screen.
  vim.api.nvim_set_hl(0, "PanelNormal", { fg = "#f8f8f2", bg = "#21222c" })

  -- Its own foreground and its own background: the background is what the
  -- window that grows over this column has to replace, and the foreground
  -- is what says whose glyph is standing there.
  vim.api.nvim_set_hl(0, "WinSeparator", { fg = "#ff79c6", bg = "#21222c" })
end

apply()
-- nvim loads its own default colour scheme after this file runs, and every
-- `:colorscheme` clears the highlight table: without this the groups above
-- reach the screen in one of the two sessions and not the other.
vim.api.nvim_create_autocmd("ColorScheme", { callback = apply })
