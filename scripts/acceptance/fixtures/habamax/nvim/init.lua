-- The second scheme the visual sweep is driven against, and the shape the
-- themed fixture deliberately is not: habamax ships with nvim, so it needs
-- no network and no plugin, and it gives every one of its four diff groups
-- a background and no foreground at all.
--
-- That is the case a hand-written fixture had blinded the sweep to. A
-- review derives its colors from those groups (`REVIEW_SHOW_CHUNK`), and a
-- group that states a background hands it over verbatim while stating no
-- text color -- so a review that painted proposed lines without choosing a
-- legible foreground of its own leaves them in whatever color the row
-- beneath was using, which on this scheme is the background it is now
-- drawn on.
--
-- Nothing else is set: `cursorline` because an overlay letting the cursor
-- row's full-width highlight read through it is the defect the interior
-- check exists for, and `termguicolors` because every assertion in the
-- sweep compares a truecolor triple.
vim.o.termguicolors = true
vim.o.cursorline = true
vim.cmd.colorscheme('habamax')
