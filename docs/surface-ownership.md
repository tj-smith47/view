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
  view keeps drawing it and tells you once, with the line that resolves it.
  `Yield` means view does not draw it, so drawing there takes nothing.
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
  see it.

## The matrix

<!-- generated from SURFACES -->
| surface | `ext_*` option | policy | `[native]` switch that hands it back | channels that draw it | proving scenario / state |
| --- | --- | --- | --- | --- | --- |
| the command line | `ext_cmdline` | `Own` | `[native] palette = false` | `ext_cmdline`, `cmdheight`, `showmode`, `showcmd`, `ruler`, `rulerformat`, `a float over the command line` | `noice`/`superseded`, `noice`/`deferred`, `nvim-notify`/`deferred` |
| the completion menu | `ext_popupmenu` | `Own` | `[native] palette = false` | `ext_popupmenu`, `ext_wildmenu` | `noice`/`superseded`, `noice`/`deferred` |
| the message area | `ext_messages` | `Own` | `[native] notifications = false` | `ext_messages`, `vim.notify`, `cmdheight`, `a float over the message area` | `noice`/`superseded`, `noice`/`deferred`, `nvim-notify`/`deferred` |
| the tab line | `ext_tabline` | `Own` | `[native] tabline = false` | `ext_tabline`, `winbar`, `tabline`, `showtabline` | `noice`/`deferred`, `smoke-minimal`/`native-only` |
| the status line | -- none -- | `Own` | `[native] statusline = false` | `laststatus`, `statusline` | -- none -- |
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
and in every window that opens afterwards, and says once what the option
was set to, with the line that hands the tab line back.

Channels view leaves to Neovim are listed beside them in `NOT_CHROME`,
each with what you get instead: the gutter, line numbers, signs and
virtual text belong to the buffer window, and view paints them as the
engine sends them.

## Four switches, six surfaces

`[native] palette = false` detaches `ext_cmdline` and `ext_popupmenu`
together, so both rows name the same line.

`[native] statusline = false` gives back the status line, which no attach
carries. view holds `laststatus` at 0 while it draws one, and your own
`statusline` is what nvim evaluates again the moment that switch goes.

`[native] tabline` is the one switch that ships off, so the tab line's row
describes what a session running `tabline = true` does: nvim draws your own
`tabline` into grid 1 by default, and the compositor paints that row like
any other. Turning it on detaches the row from nvim and hands it to view's
own tab renderer.

The buffer grid is the one surface view never draws over. nvim owns it, and
so does anything that wants to float above it, so a picker taking the screen
goes unreported.

## What a takeover actually is

A takeover is measured against the region the engine leaves for the surface.
The measurements behind that are in `docs/surface-float-wire-capture.md`.

Each rule is a conjunction: a rect that lands where a surface lives, and a
state only that surface produces. The command-line rule fires only while a
command line is actually open, so a picker whose lowest chrome window sits
one row above the same band stays silent.

## The notice a conflict gets

When something else writes a channel of a surface view draws, view sets the
channel back and tells you once: which surface it was, what the channel was
set to, and the `view.toml` line that hands the surface back. A float parked
over one of those surfaces gets the same notice, named by what that window
calls itself.

The notice stands until you take it down. Any key, click or paste,
`<Esc>` included, clears it once the notice has been on screen for as long
as an ordinary one, so the keystroke you were already typing when it appeared
leaves it alone. Its text stays in the notification history (`<leader>fm`)
either way, which is where the `view.toml` line that resolves the conflict
can be read back.
