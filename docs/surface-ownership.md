# Surface ownership

A Neovim UI surface is either drawn by view or left to your plugins, and
that is a per-surface answer rather than a mode. This page is the whole of
it: which surfaces view claims, what happens when a plugin draws on one of
them anyway, the `view.toml` line that hands it back, and the compat
scenario state that proves the answer on the pinned engine.

The table below is generated from `SURFACES`, `SURFACE_CLAIMANTS` and
`COMPLETION_MENUS` in `crates/view-core/src/native/surfaces.rs`, plus the
loaded scenario set in `compat/scenarios/`; a test fails if this page and
the tables view actually runs disagree.

## Reading a row

- **`ext_*` option** is the `nvim_ui_attach` capability whose attachment
  decides whether view draws the surface at all. `-- none --` is a surface
  no attach carries.
- **policy** is what view does when something else draws there. `Own` means
  view keeps drawing it and tells you once, with the line that resolves it.
  `Yield` means view does not draw it, so drawing there claims nothing.
  `Absorb` means view takes what the claimant drew into its own chrome
  rather than letting two renderers stack.
- **`[native]` switch** is the `view.toml` line that hands the surface back
  to your plugins. `-- none --` is honest rather than missing: no switch
  reaches that surface today, and a notice about it says what happened and
  stops rather than naming a setting that does not exist.
- **claiming plugin classes** are the plugins whose whole purpose is to
  render a surface view also renders, with the buffer `filetype` their own
  floating windows present. A plugin nobody enumerated is not missing from
  here: it reaches the generic float detector instead, which needs no table.
- **proving scenario / state** is every compat state whose probes assert
  that surface's `ext_*` attach. The attach is what decides whether view
  draws the surface, so a state asserting it on proves the policy and one
  asserting it off proves the switch. `-- none --` is a coverage gap you can
  see rather than one you have to go looking for.

## The matrix

<!-- generated from SURFACES -->
| surface | `ext_*` option | policy | `[native]` switch that hands it back | claiming plugin classes | proving scenario / state |
| --- | --- | --- | --- | --- | --- |
| the command line | `ext_cmdline` | `Own` | `[native] palette = false` | `noice.nvim` (`noice`) | `noice`/`superseded`, `noice`/`deferred`, `nvim-notify`/`deferred` |
| the completion menu | `ext_popupmenu` | `Absorb` | `[native] palette = false` | `noice.nvim` (`noice`) | `noice`/`deferred` |
| the message area | `ext_messages` | `Own` | `[native] notifications = false` | `noice.nvim` (`noice`) | `noice`/`superseded`, `noice`/`deferred`, `nvim-notify`/`deferred` |
| the tab line | `ext_tabline` | `Own` | -- none -- | -- none -- | `noice`/`deferred` |
| the buffer grid | -- none -- | `Yield` | -- none -- | -- none -- | -- none -- |

A float whose rows land in the command line's band is taken into the palette instead of being reported, but only when it presents a completion menu's own filetype (`cmp_menu`). That is the completion menu's `Absorb` read at the moment of the claim; the command line's own policy stays `Own`.

## Two switches, five surfaces

`[native] palette = false` detaches `ext_cmdline` and `ext_popupmenu`
together, which is why both rows name the same line: a session that handed
the command line back absorbs nothing and hides nobody's window.

The buffer grid is the one surface view never draws over. nvim owns it, and
so does anything that wants to float above it, so a picker taking the screen
is not a conflict and is never reported as one.

## What a claim actually is

A claim is measured against the region the engine leaves for the surface
view took over, never against the pixels view paints. Rect overlap with
view's own chrome answers backwards: read against the palette box,
nvim-cmp's cmdline menu misses it entirely while a centered telescope picker
covers it whole, so the float that claims a surface looks innocent and the
negative control looks guilty. The measurements behind that are in
`docs/surface-float-wire-capture.md`.

Geometry alone is not enough either, so each rule is a conjunction: a rect
that lands where a surface lives, and a state only that surface produces.
The command-line rule fires only while a command line is actually open,
which is what keeps a picker whose lowest chrome window sits one row above
the same band silent.
