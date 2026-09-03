# The multigrid protocol and the `single_grid` escape hatch

## What `ext_multigrid` is

Plain `nvim_ui_attach` paints one editor-wide grid: every window, float,
and the cmdline share a single coordinate space, and a UI has to work out
which window owns which screen cell from `win_viewport` alone (see
`docs/multigrid-wire-capture.md`, whose `win_viewport` section documents
the trap in that inference). `ext_multigrid` asks nvim to address each
window's own grid separately -- `grid_resize`, `grid_line` and friends
carry a `grid` id, `win_pos`/`win_float_pos` place that grid inside the
editor, and a UI composites the panes itself instead of trusting one
global buffer of cells.

view ships `ext_multigrid` on by default: it is what lets the pane
compositor (T9) paint each window from its own grid rather than parsing
one shared grid back apart, and per-grid input routing (T10) address a
click or a paste at the pane the cursor is actually in.

## What the knob does

```toml
[engine]
single_grid = false   # default: false -- view composites nvim's windows
                      # itself. Set true (or pass --single-grid) to let
                      # nvim draw one grid, which is the fallback when a
                      # plugin misbehaves under the multigrid protocol.
```

`single_grid = true` (or `--single-grid` for one session) drops
`ext_multigrid` from the attach request. nvim then draws the classic
single grid and view renders it exactly as it did before the P6 flip: one
pane, no compositor. Every other `[native]`/`[ui]` switch is unaffected --
this knob only changes how the grid protocol addresses windows, never
which surfaces are externalized (`ext_linegrid`, `ext_cmdline`, etc. still
follow `[native]`, see `crates/view-native/src/config.rs`'s `ext_surfaces`).

## Why the knob exists

`ext_multigrid` is the roughest corner of the UI protocol. A plugin that
computes screen positions itself -- rather than through the window-relative
APIs (`nvim_win_set_config`, `nvim_win_get_position`) -- is assuming the
single-grid coordinate space nvim has shipped since 0.4, and multigrid
changes what those coordinates mean. Upstream is unifying the two attach
modes (nvim PR #32691); until that lands, `single_grid` is the one-line
way out for a session hitting a plugin that assumes the coordinate space
multigrid no longer gives it.

## What a multigrid-shaped failure looks like

The symptom list `view doctor` recognizes and offers the knob for:

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
coordinates itself rather than at view's compositor -- `view doctor`
suggests `single_grid = true` as the workaround while the plugin (or
upstream) catches up, not as a verdict on which side is at fault.

## Re-evaluation

The charter requires this knob be re-evaluated at every engine-pin bump:
whether upstream's unification (PR #32691) has landed, and whether the
knob can be retired.

**Last re-evaluated against engine pin v0.12.4 (2026-09-03):** PR #32691 is
still open upstream. The knob stays.

`scripts/check-engine-pin.sh` enforces that this line's pin matches
`.engine-pin` -- bumping the pin without updating this line fails the same
gate that refuses a hardcoded nvim version.
