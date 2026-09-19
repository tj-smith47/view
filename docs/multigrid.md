# The multigrid protocol and the `single_grid` escape hatch

## What `ext_multigrid` is

Plain `nvim_ui_attach` paints one editor-wide grid: every window, float,
and the cmdline share a single coordinate space, and a UI has to work out
which window owns which screen cell from `win_viewport` alone (see
`docs/multigrid-wire-capture.md`, whose `win_viewport` section documents
the trap in that inference). `ext_multigrid` asks nvim to address each
window's own grid separately. `grid_resize`, `grid_line` and friends
carry a `grid` id, `win_pos`/`win_float_pos` place that grid inside the
editor, and a UI composites the panes itself.

view ships `ext_multigrid` on by default. The pane compositor paints each
window from its own grid, and input routing addresses a click or a paste at
the pane the cursor is actually in.

## What the knob does

```toml
[engine]
single_grid = false   # default: false -- view composites nvim's windows
                      # itself. Set true (or pass --single-grid) to let
                      # nvim draw one grid, which is the fallback when a
                      # plugin misbehaves under the multigrid protocol.
```

`single_grid = true` (or `--single-grid` for one session) drops
`ext_multigrid` from the attach request. nvim then draws the classic single
grid and view renders it the way it did before multigrid became the default:
one pane, no compositor. Every other `[native]`/`[ui]` switch is
unaffected. This knob changes how the grid protocol addresses windows,
and which surfaces are externalized still follows `[native]`
(`ext_linegrid`, `ext_cmdline`, etc., see `ext_surfaces` in
`crates/view-native/src/config.rs`).

## When to reach for it

A plugin that computes screen positions itself, bypassing the
window-relative APIs (`nvim_win_set_config`, `nvim_win_get_position`), is
assuming the single-grid coordinate space nvim has shipped since 0.4, and
multigrid changes what those coordinates mean. Upstream is unifying the two
attach modes (nvim PR #32691); until that lands, `single_grid` is the
one-line way out for a session hitting such a plugin.

## What a multigrid-shaped failure looks like

The symptoms below are what a plugin fighting the multigrid coordinate
space produces. Recognizing them and offering the knob is `view doctor`'s
job; the shipped doctor does not read this list, so a report is checked
against it by hand:

- A floating window's content renders in the wrong pane, or at a screen
  position that does not track the window it belongs to when panes are
  resized or reordered.
- A window border, separator, or highlight bleeds across a pane boundary,
  or is duplicated between two panes.
- The cursor is drawn in a pane other than the one nvim reports as
  current (`nvim_get_current_win`).
- A completion popup, hover window, or signature-help float is positioned
  as though the editor were one grid: offset by another pane's origin, or
  clipped at the wrong edge.
- Mouse clicks or scroll land in the wrong window relative to where the
  content is actually drawn.

Any one of these, reproducible with `nvim -u NORC` against the same
plugin (bare nvim, no view), points at a plugin computing screen
coordinates itself. `single_grid = true` is the workaround while the
plugin (or upstream) catches up.

## Re-evaluation

This knob is re-evaluated at every engine-pin bump: whether upstream's
unification (PR #32691) has landed, and whether the knob can be retired.

**Last re-evaluated against engine pin v0.12.4 (2026-09-03):** PR #32691 is
still open upstream. The knob stays.

`scripts/check-engine-pin.sh` enforces that this line's pin matches
`.engine-pin`. Bumping the pin without updating this line fails the same
gate that refuses a hardcoded nvim version.
