# Surface ownership

A Neovim UI surface is either drawn by view or left to your plugins, and
that answer is given per surface. This page is the whole of it: which
surfaces view owns, what happens when a plugin draws on one of them
anyway, the `view.toml` line that hands it back, and the compat scenario
state for it on the pinned engine.

The table below is generated from `SURFACES` in
`crates/view-core/src/native/surfaces.rs` and `CHANNELS` in
`crates/view-core/src/native/channels.rs`, plus the loaded scenario set in
`compat/scenarios/`.

## Reading a row

- **`ext_*` option** is the `nvim_ui_attach` capability whose attachment
  decides whether view draws the surface at all. `-- none --` is a surface
  no attach carries.
- **policy** is what view does when something else draws there. `Own` means
  view keeps drawing it and tells you once per config, with the line that
  resolves it. `Yield` means view does not draw it, so drawing there takes
  nothing.
- **`[native]` switch** is the `view.toml` line that hands the surface back
  to your plugins. For a surface an attach carries it is the switch that
  attach is gated on. `-- none --` means no switch reaches that surface,
  and a notice about it says what happened and names no setting.
- **channels that draw it** are every way Neovim can put something on that
  surface: the options it evaluates, the capability a UI takes at attach,
  a runtime function a config can replace, and a floating window parked
  over the region the surface occupies. view holds all of them for a
  surface it draws.
- **proving scenario / state** is every compat state whose probes assert
  that surface's `ext_*` attach. The attach is what decides whether view
  draws the surface. `-- none --` is a coverage gap printed where you can
  see it. A state may also run under the tiled layout, and that run keeps
  the state's name.

## The matrix

<!-- generated from SURFACES -->
| surface | `ext_*` option | policy | `[native]` switch that hands it back | channels that draw it | proving scenario / state |
| --- | --- | --- | --- | --- | --- |
| the command line | `ext_cmdline` | `Own` | `native.palette = false` | `ext_cmdline`, `a float over the command line` | `noice`/`superseded`, `noice`/`deferred`, `nvim-notify`/`deferred` |
| the completion menu | `ext_popupmenu` | `Own` | `native.palette = false` | `ext_popupmenu`, `ext_wildmenu`, `a float opened inside the command line` | `noice`/`superseded`, `noice`/`deferred` |
| the message area | `ext_messages` | `Own` | `native.notifications = false` | `ext_messages`, `vim.notify`, `cmdheight`, `showmode`, `showcmd`, `ruler`, `rulerformat`, `a float over the notice column` | `noice`/`superseded`, `noice`/`deferred`, `nvim-notify`/`deferred` |
| the tab line | `ext_tabline` | `Own` | `native.tabline = false` | `ext_tabline`, `winbar`, `tabline`, `showtabline` | `noice`/`deferred`, `smoke-minimal`/`native-only` |
| the status line | -- none -- | `Own` | `native.statusline = false` | `laststatus`, `statusline` | -- none -- |
| the tile status segments | -- none -- | `Own` | `native.statusline = false` | -- none -- | -- none -- |
| the buffer grid | -- none -- | `Yield` | -- none -- | -- none -- | -- none -- |

<!-- generated from SURFACES -->

## Every way a surface can be drawn

Neovim has five ways to put chrome on the screen: a global option, a
window-local option, an `ext_*` capability a UI takes at attach, a runtime
function a config can replace, and a floating window parked over the region
a surface occupies. view holds all five for every surface it draws, and the
lists are in `CHANNELS` in `crates/view-core/src/native/channels.rs`, one
per surface.

The tab line is the row where this shows. `ext_tabline` takes nvim's own
tab row, and a window's `winbar` draws inside that window's grid, so the
capability leaves it standing. view holds `winbar` empty in every window
and in every window that opens afterwards. It names the option once per
config, with the line that hands the tab line back, and the message history
keeps what the option was set to.

Channels view leaves to Neovim are listed beside them in `NOT_CHROME`,
each with what you get instead: the gutter, line numbers, signs and
virtual text belong to the buffer window, and view paints them as the
engine sends them.

## Four switches, seven surfaces

`[native] palette = false` detaches `ext_cmdline` and `ext_popupmenu`
together, so both rows name the same line.

`[native] statusline = false` gives back the status line, which no attach
carries. Under `[ui] panes = "nvim"` view holds `laststatus` at 0 while it
draws the bar, and your own `statusline` is what nvim evaluates again the
moment that switch goes.

Under `[ui] panes = "tiles"` the hold is `laststatus = 2`. Every window then
has a status row and each tile's frame is painted over its own, so the
`statusline` channel covers nothing: nvim evaluates your expression once per
window per status redraw and view draws over the result. The switch empties
the segments in the frame edges and leaves the frames standing, and the
hold stays. Under `[ui] panes = "nvim"` the switch gives `laststatus` back,
and a flip to that look puts your own value back.

`[native] tabline` is the one switch with no fixed default. It follows
`[ui] panes`: on under `"tiles"`, off under `"nvim"`, and a value you write
wins either way. On, the row is detached from nvim and view draws the top
pill in it, under either mode. Under `"tiles"` the row is drawn only while
it names something the tiles' frames do not: a second tabpage, two or more
listed buffers under `tabline_shows = "buffers"`, a `--remote` host, or an
agent that is not idle. Under `"nvim"` it follows `showtabline`. Off, nvim
draws your own `tabline` into grid 1 and the compositor paints that row like
any other.

The buffer grid is the one surface view never draws over. nvim owns it, and
so does anything that wants to float above it, so a picker taking the screen
goes unreported.

## What a takeover actually is

A takeover is measured against the region the engine leaves for the surface.
The measurements behind that are in `docs/surface-float-wire-capture.md`.

Each rule is a conjunction: a rect that lands where a surface lives, and a
state only that surface produces. The command-line rule fires only while a
command line is actually open, so a picker whose lowest chrome window sits
one row above the same band stays silent. It names a float that was
standing when the line opened. A float first placed while the line is open
and not taken as its completion menu is a notification or a progress
message, and it draws over no command line in any corner. The command line
starts at the left edge, so a float standing in the bottom right-hand corner
is a notification stacked there and draws over no command line either.

A completion menu that opens while the palette's command line is open
belongs to that command line, wherever it lands, unless it lands in the
notice column. A float is a completion menu when it stacks at or above
nvim's own completion menu, which is `zindex` 100, and hangs from its left
edge (`anchor` `NW` or `SW`). A notifier stacked as high hangs from a
right-hand corner. A completion menu gets no notice. The palette lists the
tallest such float in its own rows and keeps the others off the screen
until the command line closes. A notification or a progress message that
opens while the line is open stays where it opened.

Such a menu sends no selected index, so the palette reads the selection
from its colours: the one row most of whose cells carry a highlight no
other row carries. A row set apart in a cell or two, as a kind glyph sets
a folder among files, is a row like the others.

## The notice a conflict gets

When your config writes a channel of a surface view draws, view sets the
channel back. It tells you once per config, then the history: the first
launch under a config file shows one box naming each surface and the
channels that reported it, and the keys of yours it maps in one line, with
the `view.toml` lines that hand them back. The features view draws are
named in the same box. A launch where your config wrote no such channel and
mapped no such key shows no box. Every later launch under the same file
records the same finding to the message history and shows no box. The
history entry spells what the channel was set to, which the box leaves out.
A float parked over one of those surfaces gets a notice of its own, named
by what that window calls itself.

The notice stands until you take it down. Any key, click or paste,
`<Esc>` included, clears it once the notice has been on screen for as long
as an ordinary one, so the keystroke you were already typing when it appeared
leaves it alone. Its text stays in the notification history (`<leader>fm`)
either way, which is where the `view.toml` line that resolves the conflict
can be read back. A launch box that names no channel clears on its own, as
an ordinary notice does.
