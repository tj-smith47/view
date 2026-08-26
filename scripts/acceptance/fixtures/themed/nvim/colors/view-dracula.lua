-- Dracula, cut down to the groups view's own chrome resolves and loaded
-- the way any colorscheme is, so the visual sweep observes the real
-- probe/derive path rather than a hand-written theme cache.
--
-- Three background values carry the whole sweep's meaning and must stay
-- distinct from each other:
--
--   Normal      the buffer layer an overlay is drawn over
--   CursorLine  a second, brighter buffer-layer background, full window
--               width, so an overlay that is not opaque shows the row
--               running through it
--   NormalFloat every overlay's own interior
--
-- No other group may take the Normal or CursorLine value: the sweep reads
-- either of them inside an overlay as the layer beneath bleeding through.
-- The semantic groups are therefore foreground-only, bar the four diff
-- ones -- which is the case that proves a span role with no background of
-- its own keeps the overlay's rather than punching a hole in it.
--
-- The diff groups carry a background each because an agent's proposed edit
-- is drawn in the buffer with nothing but those groups, so a background is
-- the only thing that tells a decorated row from an ordinary one in a
-- capture, which reads no foregrounds. Each is distinct from every other
-- and from the three above, so no leg can confuse them.
--
-- CursorLine is also the one underlined group here, and no chrome group may
-- take an attribute: an overlay cell that comes back underlined can only
-- have inherited it from the row beneath, which is the same defect as a
-- background bleed in the half of it a color cannot show.
vim.cmd('highlight clear')
vim.g.colors_name = 'view-dracula'
local hl = vim.api.nvim_set_hl
hl(0, 'Normal', { fg = '#f8f8f2', bg = '#282a36' })
hl(0, 'CursorLine', { bg = '#44475a', underline = true })
hl(0, 'NormalFloat', { fg = '#f8f8f2', bg = '#21222c' })
hl(0, 'MsgArea', { fg = '#f8f8f2', bg = '#21222c' })
hl(0, 'Pmenu', { fg = '#f8f8f2', bg = '#21222c' })
hl(0, 'PmenuSel', { fg = '#282a36', bg = '#bd93f9' })
hl(0, 'StatusLine', { fg = '#f8f8f2', bg = '#6272a4' })
hl(0, 'TabLine', { fg = '#6272a4', bg = '#21222c' })
hl(0, 'TabLineSel', { fg = '#f8f8f2', bg = '#6272a4' })
hl(0, 'TabLineFill', { fg = '#f8f8f2', bg = '#21222c' })
hl(0, 'FloatTitle', { fg = '#bd93f9' })
hl(0, 'ModeMsg', { fg = '#50fa7b' })
hl(0, 'WarningMsg', { fg = '#ffb86c' })
hl(0, 'ErrorMsg', { fg = '#ff5555' })
hl(0, 'Directory', { fg = '#8be9fd' })
hl(0, 'IncSearch', { fg = '#ffb86c' })
hl(0, 'DiffAdd', { fg = '#50fa7b', bg = '#213d24' })
hl(0, 'DiffChange', { fg = '#ffb86c', bg = '#3d3721' })
hl(0, 'DiffDelete', { fg = '#ff5555', reverse = true })
hl(0, 'DiffText', { fg = '#8be9fd', bg = '#21313d' })
