# first_paint's boundary changed; both cells' recorded values are stale

## What changed

The first-frame boundary was `any_visible_cell` — "some cell on screen has
ink". view runs nvim as a child and paints its own placeholder chrome
(`view_surface::LayerKind::Shell`) well before the engine attaches, while
bare nvim's first visible cell is its buffer window. The two sides were
therefore timed to two different events.

Consequence: a view that stopped attaching its engine entirely still paints
chrome, so it would have recorded a healthy `cold_ms` and gated green. The
metric could not catch the regression it exists for.

Both sides now open a scratch file carrying `VIEWBENCHCOLDSTARTMARKER` and
are timed to that marker painting.

## What this invalidates

| Cell | Metric | Recorded (pre-change) | Status |
|---|---|---|---|
| `first_paint.minimal` | `cold_ms` | 3.5832 | stale — measures a different event |
| `first_paint.minimal` | `ratio_vs_nvim` | 0.019123 | stale |
| `first_paint.heavy` | `cold_ms` | 3.5832 (see note) | stale |
| `first_paint.heavy` | `ratio_vs_nvim` | 0.25869 → 0.019123 | stale |

Both cells must be re-recorded on a quiet host before they gate anything.
`cold_ms` will rise on the view side (the marker paints after the
placeholder); the nvim side moves little, since nvim paints its window and
the buffer in the same frame.

## Supersedes the denominator observation

An audit noted that the recorded pairs implied nvim's own `first_paint.heavy`
leg moved 206.24 ms → 187.38 ms (−18.9 ms) while `minimal` moved only
−1.6 ms, and asked whether that asymmetry was plugin-startup variance rather
than a DA1 effect. That question is now moot for the recorded numbers: the
boundary those pairs were measured at no longer exists, so the re-record
replaces both legs. If the asymmetry persists in the re-recorded pairs, it is
variance in the heavy fixture's plugin startup and not attributable to the
DA1 fix — worth re-checking against the new numbers rather than the old ones.
