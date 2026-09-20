# The tiled UI

## What you see

Under `panes = "tiles"` every nvim window gets a frame of its own. The
window you are working in carries the accent colour on its frame, and every
other frame is dimmed halfway toward the background. With `gaps = true`
there is a clear cell between two frames and around the outside of the
screen. With `gaps = false` two neighbouring tiles share one frame line, and
the places where lines meet take a junction glyph.

Under `panes = "nvim"` the screen keeps the shape nvim draws: the `│`
separator column between windows, restyled through `WinSeparator`.

view's own statusline bar keeps the bottom row in both modes.

## Choosing a mode

```toml
[ui]
panes = "auto"   # "auto" | "tiles" | "nvim"
gaps  = true     # false: frames share edges, no outer gap

[ui.tokens]
accent = "#89b4fa"   # the active tile's frame colour
```

`"auto"` asks the environment who is already drawing frames. A tiling
window manager puts a border around the terminal itself, and `"auto"`
answers `nvim` there.

| environment marker | answer |
|---|---|
| `HYPRLAND_INSTANCE_SIGNATURE` set | `nvim` |
| `SWAYSOCK` set | `nvim` |
| `I3SOCK` set | `nvim` |
| `XDG_CURRENT_DESKTOP` names `Hyprland`, `sway`, `i3`, `river`, `niri`, `bspwm`, `dwm`, `awesome`, `qtile`, `xmonad`, `herbstluftwm` or `leftwm` | `nvim` |
| anything else | `tiles` |

The comparison against `XDG_CURRENT_DESKTOP` ignores case and reads every
colon-separated member. An ssh session gets `tiles`.

`[engine] single_grid = true` resolves `panes` to `nvim` whatever the file
or the flag says. Without `ext_multigrid` nvim sends no `win_pos` and no
per-window grid.

The mode follows the same precedence as every other `[ui]` key: the
`--panes` flag, then `VIEW_UI_PANES`, then the file, then the environment.
`:View ui panes` in a running session prints the answer and the marker that
decided it:

```
ui.panes = nvim (HYPRLAND_INSTANCE_SIGNATURE)
```

## Flipping the mode in a running session

```
:View ui panes tiles
:View ui panes nvim
:View ui panes auto
:View ui panes          " reports the mode and its marker
```

A flip re-attaches the outer grid at its new size, sends every window grid
a fresh inner size, and repaints the whole screen.

## How a frame gets its room

nvim owns the window layout. Under `ext_multigrid` it reports each window's
slot as `win_pos grid win startrow startcol width height` and paints the
window's text into a grid of its own.

`nvim_ui_try_resize_grid(grid, w, h)` sets that window's *inner* size. The
slot stays where nvim put it, the text grid becomes smaller than the slot,
and the difference belongs to view. Tiles mode asks each window for a grid
four columns and four rows smaller than its slot and draws the frame and the
gap into what is left. `<C-w>` commands, `:split`, and a plugin that opens
windows all keep working.

Three properties of that request shape the geometry:

- The request stands until it is replaced. After a `:vsplit` halves a slot
  the window keeps the old request. view sends a fresh one on every
  `win_pos` whose slot size changed.
- A request of `0, 0` returns a window to its slot's own size. That is what
  a gapless tile sends, and what every window gets on a flip to `nvim`.
- A `winbar` adds its row on top of the requested height. The row arrives as
  `win_viewport_margins`, and the height request carries it as a term.

A slot narrower than 5 columns or shorter than 5 rows has no room to spare,
so view leaves its grid at the slot size and draws the tile bare.

Gapless tiles need no room at all. The one cell between two windows is the
cell nvim already paints its separator column and status row into, and view
restyles those cells as the frame. The screen's top row and left column come
from the outer grid attaching one row and one column short.

## The accent

The active tile's frame takes the first of these that resolves:

1. `[ui.tokens] accent` from your config
2. the `Function` highlight group's foreground
3. the `Statement` highlight group's foreground
4. view's own emphasis colour

The two highlight groups are read from the engine when a colorscheme
loads.

## Width a surface has to work with

The tree, the agent panel and the other surfaces size themselves as a
percentage of the outer grid. Under gapped tiles the outer grid is already
two columns narrower than the terminal, and a tile spends four more on its
frame and gaps. A windowed surface has up to six columns less text width
than the same percentage of the terminal.
