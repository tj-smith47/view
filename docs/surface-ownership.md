# Surface ownership

A Neovim UI surface is either drawn by view or left to your plugins, and
that answer is given per surface. This page is the whole of it: which
surfaces view owns, what happens when a plugin draws on one of them
anyway, the `view.toml` line that hands it back, and the compat scenario
state for it on the pinned engine.

The table below is generated from `SURFACES`, `SURFACE_CLAIMANTS` and
`COMPLETION_MENUS` in `crates/view-core/src/native/surfaces.rs`, plus the
loaded scenario set in `compat/scenarios/`.

## Reading a row

- **`ext_*` option** is the `nvim_ui_attach` capability whose attachment
  decides whether view draws the surface at all. `-- none --` is a surface
  no attach carries.
- **policy** is what view does when something else draws there. `Own` means
  view keeps drawing it and tells you once, with the line that resolves it.
  `Yield` means view does not draw it, so drawing there takes nothing.
  `Absorb` means view takes what the claimant drew into its own chrome, so
  one renderer draws the surface.
- **`[native]` switch** is the `view.toml` line that hands the surface back
  to your plugins, and it is the switch that surface's `ext_*` attach is
  gated on. `-- none --` means no switch reaches that surface, and a
  notice about it says what happened and names no setting.
- **claiming plugin classes** are the plugins whose whole purpose is to
  render a surface view also renders, with the buffer `filetype` their own
  floating windows present. A plugin nobody enumerated reaches the generic
  float detector, which needs no table.
- **proving scenario / state** is every compat state whose probes assert
  that surface's `ext_*` attach. The attach is what decides whether view
  draws the surface. `-- none --` is a coverage gap printed where you can
  see it.

## The matrix

<!-- generated from SURFACES -->
| surface | `ext_*` option | policy | `[native]` switch that hands it back | claiming plugin classes | proving scenario / state |
| --- | --- | --- | --- | --- | --- |
| the command line | `ext_cmdline` | `Own` | `[native] palette = false` | `noice.nvim` (`noice`) | `noice`/`superseded`, `noice`/`deferred`, `nvim-notify`/`deferred` |
| the completion menu | `ext_popupmenu` | `Absorb` | `[native] palette = false` | `noice.nvim` (`noice`) | `noice`/`superseded`, `noice`/`deferred` |
| the message area | `ext_messages` | `Own` | `[native] notifications = false` | `noice.nvim` (`noice`) | `noice`/`superseded`, `noice`/`deferred`, `nvim-notify`/`deferred` |
| the tab line | `ext_tabline` | `Own` | `[native] tabline = false` | -- none -- | `noice`/`deferred`, `smoke-minimal`/`native-only` |
| the buffer grid | -- none -- | `Yield` | -- none -- | -- none -- | -- none -- |

<!-- generated from SURFACES -->
A float whose rows land in the command line's band is taken into the palette
when it presents a completion menu's own filetype (`cmp_menu`). That is the
completion menu's `Absorb` read at the moment the float appears; the command
line's own policy stays `Own`.

## Three switches, five surfaces

`[native] palette = false` detaches `ext_cmdline` and `ext_popupmenu`
together, so both rows name the same line: a session that handed the
command line back absorbs nothing and hides nobody's window.

`[native] tabline` is the one switch that ships off, so the tab line's row
describes what a session running `tabline = true` does: nvim draws your own
`tabline` (and whatever bufferline plugin sets it) into grid 1 by default,
and the compositor paints that row like any other. Turning it on detaches
the row from nvim and hands it to view's own tab renderer, at which point a
bufferline has nothing left to draw into.

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

## The notice a named claimant gets

A plugin the table names gets one notice per launch, and view asks it to
turn itself off first: the ask goes out with the takeover, and again for any
claimant that loads after it, so a plugin lazy.nvim defers is turned off on
the load event.

The notice stands until you take it down. Any key, click or paste,
`<Esc>` included, clears it once the notice has been on screen for as long
as an ordinary one, so the keystroke you were already typing when it appeared
leaves it alone. Its text stays in the notification history (`<leader>fm`)
either way, which is where the `view.toml` line that resolves the conflict
can be read back.
