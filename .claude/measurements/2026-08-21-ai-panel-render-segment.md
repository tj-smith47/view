# What the agent panel costs the paint path, 2026-08-21

The `ai_streaming` spec 3.1 row claims the panel's own render is what it
puts under measurement. This is the reading behind that claim.

## Provenance

One quiet window on dev-linux, the host's peer session stopped for the
duration and nothing else measured, 71G free. Every run is a full protocol
draw -- three trials of 1000 samples over 100 warmup -- taken one cell per
invocation. Host load 0.10 to 1.05 at each cell's start; every null-pair
calibration deviated at most 1.066 against the 1.15 refusal floor. The
recording these runs produced is commit `b898427`.

## draw-start -> flush-start, the segment the panel renders inside

`p50 / p99 / chains`, as the bench binary prints it per run:

| run | class | p50 | p99 | chains |
|---|---|---|---|---|
| `output_path` record | controlled-linux | 20.2 us | 42.5 us | 2999 |
| `output_path` record | dev-linux | 20.6 us | 44.0 us | 2999 |
| `output_path` gate | dev-linux | 17.2 us | 39.6 us | 3000 |
| `ai_streaming` record | controlled-linux | 231.5 us | 346.0 us | 2855 |
| `ai_streaming` gate | controlled-linux | 153.8 us | 317.9 us | 2848 |
| `ai_streaming` record | dev-linux | 229.5 us | 346.9 us | 2887 |

The two groups do not overlap and are an order of magnitude apart, which
is the whole finding: a frame drawn while a turn streams into the panel
costs roughly 8-11x what the same row's frame costs with no session on
screen. The spread inside the streaming group is wide (153.8 to 231.5 us
p50 across three draws), so the honest statement of the cost is that
range and not any single draw -- in particular not the 153.8 low draw,
which flatters the comparison.

## What this is not

Not a paired measurement. The session-absent numbers come from
`output_path` and the session-present ones from `ai_streaming`: two rows
measuring the same segment under different drivers in the same window,
not two arms of one interleaved run. It is enough to say the panel's
render dominates this segment while a turn is in flight; it is not enough
to attribute a percentage to any one part of the panel's render, and no
such attribution is made from it.

The gated statistic each row records is the whole boundary interval, not
this segment. This segment is reported every run as evidence and is
bounded by nothing.

## Attribution alongside it

Frames the panel explains, counted through the tap channel rather than
assumed, over 3300 keystrokes per run: 70 (`ai_streaming` record,
dev-linux), 45 (record, controlled-linux), 43 (gate, controlled-linux).
No paint was left unexplained on any of the three.
