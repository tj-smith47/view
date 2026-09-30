# The tiled UI

## What you see

Under `panes = "tiles"` every nvim window gets a frame of its own. The
window you are working in carries the accent colour on its frame. Every
other frame takes the colour your colorscheme gives the lines between
windows (`WinSeparator`). With `gaps = true` there is a clear cell between
two frames and around the outside of the screen. With `gaps = false` two
neighbouring tiles share one frame line, and the places where lines meet
take a junction glyph.

An inactive tile shows the cursor line when your config sets `cursorline`
for every window, as nvim does.

Under `panes = "nvim"` the screen keeps the shape nvim draws: the `│`
separator column between windows, restyled through `WinSeparator`.

Under `panes = "nvim"` view's own status bar keeps the bottom row. Under
tiles there is no bar row at all: each tile says its own status in its
frame.

## What a tile's frame says

| where | what it carries |
|---|---|
| gapped, top edge | the tile's title |
| gapped, bottom edge | the segments below |
| gapless, bottom edge | the title, then the segments below |

| segment | shown on |
|---|---|
| the mode, `-- INSERT --` or `recording @q` | the tile you are working in |
| the git branch | every tile of a kind that carries it, once the branch is read |
| the diagnostic counts, `E 2` and `W 1` | a file or scratch tile whose buffer has any |
| the cursor position, `row:col` | a file or help tile, from that window's own cursor |
| the entry you are on, `row/lines` | a quickfix or location list tile |
| the keys you have typed so far | the tile you are working in |

What a tile holds decides its title and which of those segments it
carries. A view panel decides first, then the buffer's type, then whether
the window keeps a fixed width.

| tile | title | segments |
|---|---|---|
| a file | the file name, with `[+]` on unsaved changes | mode, branch, diagnostics, `row:col`, typed keys |
| help | `help: options` | `row:col` |
| the quickfix list | `quickfix` | `row/lines` |
| a location list | `location list` | `row/lines` |
| a terminal | `terminal: zsh`, the program it runs | mode |
| a prompt buffer | its filetype, else `prompt` | mode |
| view's tree | `tree` | branch |
| view's agent panel | `agent` | none |
| view's notifications | `notifications` | none |
| a side panel: a `nofile` buffer in a window with `winfixwidth` set, such as a plugin's file tree, outline or debugger panel | its filetype, else its name, else `scratch` | branch |
| any other buffer with no file behind it, such as a dashboard | its filetype, else its name, else `scratch` | diagnostics |

The mode and the typed keys show only on the tile you are working in. Every
title but a file's is drawn in the colorscheme's float title colour.

A tile titled by its filetype takes the title `tile_titles` gives that
filetype:

```toml
[ui]
tile_titles = { NvimTree = "files", Outline = "symbols", alpha = "dashboard" }
```

A filetype the table leaves out keeps the filetype as its title.

A segment that runs out of room in the edge is dropped whole, with
everything after it.

`[native] statusline = false` empties the segments and leaves the frames
standing. It gives `laststatus` back to your config only under
`panes = "nvim"`. Under tiles view keeps `laststatus = 2`.

## The top row

The top row names what you have open. Under tiles it is drawn only while it
carries something the tiles' own frames do not show:

- a second tabpage
- two or more listed buffers, with `tabline_shows = "buffers"`
- the host of a `--remote` session
- an agent session or its panel, between turns as well as during one

With none of those the row is gone and the tiles take its line. Under
`panes = "nvim"` it follows your `showtabline`: `0` keeps the row off, `1`
brings it up once a second tabpage is open, `2` keeps it up always. It is
the same row in both modes. `:View ui panes` moves the row with the mode it
switches to, and says which of the two now draws it. While view holds the
row with nothing to name, the report says the row is hidden.

Each name is its own pill, centred on the row, and the current one is lit.
The host's pill sits at the left edge and the agent's at the right. Between
the pills the row is your colorscheme's background.

| where | what it carries |
|---|---|
| left edge | the host `--remote` named, on a remote session |
| middle | your tabpages, the current one lit |
| right edge | what the agent is doing |

Clicking a name switches to it, in either mode.

```toml
[native]
# tabline = true          # follows [ui] panes: on under tiles, off under "nvim"
tabline_shows = "tabs"    # "tabs" | "buffers"

[ui]
pill_caps = "auto"        # "auto" | "round" | "flat"
```

`pill_caps` shapes the two ends of every pill. `round` draws the rounded
Nerd Font end glyphs. `flat` draws each end as a blank cell in the pill's
own colour. A pill is the same width in both, so switching moves no name.
`auto` picks `round` on a terminal that draws box glyphs and `flat` on one
that does not.

With `tabline_shows = "buffers"` the middle names your open files instead,
each with a `+` while it has unsaved changes. A second tabpage is named as
tabpages either way.

The agent's word is one of three:

| what is happening | word |
|---|---|
| the agent is asking your permission | `waiting` |
| the agent is working on your prompt | `running` |
| the agent died | `crashed` |

An idle agent draws no pill, and neither does an agent `[ai]` turns off.
An agent is idle between prompts, so under tiles the agent's pill comes
up when you send a prompt and leaves when the answer is done.

`[native] tabline = false` leaves the row to nvim, so your own tabline
plugin keeps it.

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

`[keys] profile` is a choice of its own, alongside `[ui] panes`: it picks
whether a session answers the omarchy desktop's own chords. `<D-Left>`
reaches the same `<C-w>h` under either look mode. See
[keymaps.md](keymaps.md#key-profiles).

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
two columns and two rows smaller than its slot and draws the frame into what
is left. The gap between two frames is the separator column and the status
row nvim paints just past the slot, which view clears. `<C-w>` commands,
`:split`, and a plugin that opens windows all keep working.

Three properties of that request shape the geometry:

- The request stands until it is replaced. After a `:vsplit` halves a slot
  the window keeps the old request. view sends a fresh one on every
  `win_pos` whose slot size changed.
- A request of `0, 0` returns a window to its slot's own size. That is what
  a gapless tile sends, and what every window gets on a flip to `nvim`.
- A `winbar` adds its row on top of the requested height. The row arrives as
  `win_viewport_margins`, and the height request carries it as a term.

A slot narrower than 3 columns or shorter than 3 rows has no room to spare,
so view leaves its grid at the slot size and draws the tile bare.

Gapless tiles need no room at all. The one cell between two windows is the
cell nvim already paints its separator column and status row into, and view
restyles those cells as the frame. The screen's top row and its left and
right columns come from the outer grid attaching one row and two columns
short.

Where view draws the messages nvim keeps `cmdheight` at 0, so the lowest
window's status row is the outer grid's last row. Under gapped tiles that
row is the gap below the lowest frames, and under gapless tiles it is their
bottom frame line. That holds with the palette given back as well, since
nvim draws no command-line row for a session that takes its messages. A
session that gave back the notifications leaves nvim a command-line row at
the foot of the grid, and the frames stop above it.

## Fitting a tile to its text

`<leader>wf` (`SUPER + Home` under the desktop profile, `:View window fit`)
sizes the focused tile to the longest line it shows, with one column spare
for the cursor.

```toml
[ui]
fit_active = false   # true: every window you move into is fitted to its text
```

The width comes from the lines on screen, from the top of the window to the
bottom. It never goes below `winwidth`. It stops at `textwidth`, or where
`textwidth` is 0 at the first `colorcolumn` entry that is a plain column
number, such as `90` in `+1,90`, so the marker column stays in view. The
number and sign columns and the frame are added on top.

With `equalalways` on, the other tiles share the columns that are left.
With `noequalalways`, only the neighbour the columns came from changes.
Floats, terminals and fixed-width sidebars are left as they are, and a
fixed-width sidebar keeps its width when a tile beside it is fitted.

Fitting a zoomed tile changes its width and keeps its height. The next zoom
press zooms it again. Under `fit_active`, a zoomed layout stays zoomed when
the cursor comes back to the zoomed tile from a float.

## The accent

The active tile's frame takes the first of these that resolves:

1. `[ui.tokens] accent` from your config
2. the `Function` highlight group's foreground
3. the `Statement` highlight group's foreground
4. view's own emphasis colour

The two highlight groups are read from the engine when a colorscheme
loads.

## Placing a surface

The file tree, the agent panel, the palette and the notification stream
each have their own `[ui.surfaces.<id>]` table, with a `placement`, an
`anchor` and a `size`. `placement` `"overlay"` floats the surface over
your buffer. `"windowed"` gives it a window of its own in nvim's layout,
so your buffers make room for it and every window command reaches it.
Under tiles, a surface at a side edge frames on the same rows and edge
column as the tiles beside it in both placements. The palette is the one
surface that takes its own band without a window number: it covers the
rows at its anchored edge while `:` is open, and your buffers stay exactly
where they were under it. A window command lands on the buffer you were
already in, and `:` opens the band the same way it opens the centred
palette. `anchor` is the edge, or for the notification stream's toast
stack the corner, it opens at, and its own accepted words and default
change with `placement`: a centred float has no centred window to become,
and a corner toast stack has no corner tile. `size` is its share of the
terminal; whether the same number reaches both placements or windowed alone
depends on the surface, named below.

```toml
[ui.surfaces.tree]
placement = "overlay"      # default: "overlay". "windowed" opens it as a
                            # window of its own
anchor = "left"             # default: "left", both placements. left | right
size = 30                   # percent of the terminal width. Default: 30

[ui.surfaces.agent]
placement = "overlay"
anchor = "right"             # default: "right", both placements. left | right
size = 30

[ui.surfaces.palette]
placement = "overlay"
anchor = "center"            # default: "center" overlay, "bottom" windowed.
                              # overlay: center | top | bottom
                              # windowed: top | bottom
size = 40                    # percent of the terminal height, both
                              # placements. Default: 40

[ui.surfaces.notifications]
placement = "overlay"
anchor = "top-right"         # default: "top-right" overlay, "right" windowed.
                              # overlay: top-left | top-right | bottom-left |
                              # bottom-right
                              # windowed: left | right | top | bottom
size = 30                    # percent of the terminal width (left/right) or
                              # height (top/bottom). Windowed only: the
                              # overlay history opens at a fixed size
```

`[native] tree_width` is `[ui.surfaces.tree] size` under its older name,
and `[ai] panel_width` is `[ui.surfaces.agent] size` under its. Write
either one; the newer key wins when a config writes both.

The file tree lists folders first and then files, each group ordered by
name whatever its case. Dotfiles are listed. Anything `.ignore` names is
left out, and inside a git repository anything `.gitignore` names is left
out as well. Opening `.git` lists what is directly inside it. A linked
folder sorts with the folders and opens to nothing, and a linked row shows
where it points after its name.

```toml
[ui]
tree_icons = "auto"       # "auto" | "nerd" | "none"
```

`tree_icons` picks what each tree row opens with. `nerd` draws an arrow and
a folder glyph on a folder, both in your colorscheme's folder colour, with
a glyph of its own for an empty folder and a linked one. A file gets a
file-type icon in that type's own colour. Git states follow the icon as
glyphs, and a folder shows every state found beneath it. `none` draws a `+`
on a closed folder, a `-` on an open one and a blank on a file, and git
states as letters, a folder showing one for every state found beneath it.
Under `nerd` a folder inside `.git` draws the plain folder glyph. `auto`
picks `nerd` on a terminal that draws box glyphs and `none` on one that
does not.

The notification stream's `anchor` is the toast stack's own corner: the
near box sits flush against it and every later box sits farther away. A
dismissed box leaves by sliding toward that corner, or by shrinking where
a left corner leaves it no column to slide into.

Windowed, the notification stream takes two shapes from the same edge its
`anchor` names. Left or right opens a tall, narrow list: every notice you
have seen, newest at the top. Top or bottom opens a short, wide ticker
instead, `winfixheight`-pinned so a split beside it cannot steal its rows.
Either shape shortens each entry's timestamp to `HH:MM:SS` once its own
tile is too narrow for the full date to fit beside the message.

## Where notices appear

Notices stack in the notice column, in the corner named by
`[ui.surfaces.notifications] anchor`. The column takes the part of the
screen that the tree, the agent panel, the other surfaces and a plugin's
sidebar leave, wherever each is placed. Under tiles it sits inside the
tile in that corner, one cell in from the frame. When a panel fills the
whole screen, the column takes the screen's own corner. It is at most 60
columns wide and at most half as wide as the room the surfaces leave, and
a notice longer than that wraps onto more lines inside its box.

When the boxes would cover the line your cursor is on, or the change you
are reviewing from its header to its last added line, the stack moves to
the column's other end. It stays there until your cursor or the change
reaches it. When the change reaches both ends, the column shrinks to the
larger side of it, and the notices that no longer fit wait in the
history. A plugin's floating window over the column keeps its place, and
the stack starts past it.

## The placement ring

`<leader>uw` (`cycle_surfaces`) steps the tree, the agent panel, the
palette and the notification stream together through one shared,
three-position ring:

```
config -> windowed -> overlay -> config
```

`config` is what `view.toml` gave each surface at startup: `overlay` for
every surface but the ones you placed windowed yourself. The ring's
`windowed` and `overlay` stops move every surface to that placement at
once, whatever `view.toml` said. A third press returns every surface to
its own configured placement, the loop's only three stops. A surface
already open when the ring steps keeps what it was showing: the tree
keeps its cursor row, the agent panel keeps its transcript, and the
notification stream keeps what it was displaying. See
[keymaps.md](keymaps.md#gaps-and-the-placement-ring) for the key itself
and `<leader>ug`, the gaps toggle beside it.

## Width a surface has to work with

The tree, the agent panel and the other surfaces size themselves as a
percentage of the outer grid. Under gapped tiles the outer grid is already
two columns narrower than the terminal, and a tile spends two more on its
frame. A windowed surface has up to four columns less text width
than the same percentage of the terminal.

Inside its own pane, a windowed surface draws unframed: the tile around it
is already a box, so a border of the surface's own would be a second one
where a person sees one. Its content spans the full width nvim gave the
tile. A floating surface draws its own border and, at six columns wide or
more, a one-cell pad inside it on both sides. Its windowed counterpart keeps
those four columns for content.

## Screen regions, by crate

For each region on screen, which crate decides what goes there and which
crate paints it. `view-core` holds the `Model` and the surface tables that
decide content; `view-surface::render` turns that `Model` into an ordered
layer list; `view-tui` is the only crate that turns a layer into terminal
cells. See `docs/surface-ownership.md` for the fuller policy table (what a
plugin drawing over a region gets told, and the `view.toml` line that hands
each one back).

```mermaid
flowchart TB
    nvim["Neovim: owns the buffer grid"]
    core["view-core: Model, Surface table, ui.surfaces config"]
    ai["view-ai: ACP session state"]
    surf["view-surface: render() -> ordered layer list"]
    tui["view-tui: the only crate that writes terminal cells"]
    term["your terminal"]

    nvim -- "ext_* attach, redraw events" --> core
    ai -- "panel content" --> core
    core --> surf
    surf --> tui
    nvim -- "grid cells, under view's frames" --> tui
    tui --> term
```

| region | decided by | painted by |
| --- | --- | --- |
| the buffer grid | Neovim | `view-tui` composites nvim's own cells in, unchanged |
| view frames (tile borders) | `view-core`'s `Surface::Frame` | `view-tui::paint::panes` |
| the pill (top row) | `view-core`'s `Surface::Tabline` | `view-tui::paint::pill` |
| toasts | `view-core`'s `Surface::Messages` | `view-tui::paint::toast` |
| the palette | `view-core`'s `Surface::Cmdline`, `Surface::Popupmenu` | `view-tui::paint` |
| the file tree | `view-core`'s `[ui.surfaces.tree]`, `native::tree` | `view-tui::paint` |
| the agent panel | `view-ai`'s ACP session, held in `view-core`'s `AiPanelView` | `view-tui::paint` |

The buffer grid is the one region nvim keeps for itself: view frames it and
draws every other region around it, but the cells inside stay nvim's.
