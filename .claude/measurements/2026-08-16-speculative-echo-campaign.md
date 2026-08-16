# Speculative echo: first recording + replicate campaign (dev-linux)

2026-08-16, tree at `bda9b65`, binaries the `task bench` quartet built from
that tree (plain, taps, nospec, taps-nospec). Host genuinely quiet the whole
window: loads 0.07–0.50 across every draw, zero foreign cargo/rustc/cfgd
processes before and during, null-pair calibration passing on the record
run (ratio_p50 0.9788, deviation 1.0217 against floor 1.15). Log:
`~/.claude/tmp/t6-campaign.log`, `t6-record.log`, `t6-gate.log`.

## Campaign: 8 report-only replicates, echo_speculated/minimal

Gated statistic per replicate = median of 3 trials, same shape as the
flood and input_path campaigns.

| rep | speculated_ratio_p50 | speculated_paint_p99_ms | load at start |
|---|---|---|---|
| 1 | 0.412 | 0.301 | 0.32 |
| 2 | 0.392 | 0.335 | 0.50 |
| 3 | 0.385 | 0.328 | 0.21 |
| 4 | 0.429 | 0.299 | 0.15 |
| 5 | 0.418 | 0.298 | 0.26 |
| 6 | 0.419 | 0.299 | 0.07 |
| 7 | 0.414 | 0.299 | 0.40 |
| 8 | 0.422 | 0.306 | 0.35 |

The `--record` pass that followed (load 0.12) drew ratio 0.3943, paint
0.370 — the ninth draw of each statistic.

- ratio_p50: nine-draw median 0.414, half-width 0.022 (5.3%). Every draw
  beats the spec ceiling of 1.0 with >2.3x headroom; the pitch table's
  "goes below 1.0" claim is now a measured, replicated fact.
- paint_p99_ms: nine-draw median 0.301, half-width 0.036 (12.0%). The
  record draw (0.370) sits above all eight campaign draws — a tail
  excursion, not the center.

## What was recorded, and why

`crates/view-bench/baselines/dev-linux.toml` `[echo_speculated.minimal]`:

- `speculated_ratio_p50 = 0.3943…` — the genuine record draw, kept: it
  sits inside the campaign band (0.385–0.429).
- `speculated_paint_p99_ms = 0.301` — the nine-draw median,
  hand-recorded per the documented path in
  `view_harness::baselines` (RefusedBelowSpread doc): anchoring at the
  0.370 tail draw would put the record floor (recorded ÷ spread) above
  the class's entire honest band, so every later honest record would
  refuse and the tail would be permanent.

Published spreads (`dev-linux.headroom.toml`, scenario-scoped):

- `"echo_speculated.speculated_ratio_p50" = 1.17` — worst excursion above
  the recorded value 1.088x (0.429/0.3943); 2x half-width rule asks
  1.162x (0.414 + 2×0.022 against 0.3943). 1.17 clears both.
- `"echo_speculated.speculated_paint_p99_ms" = 1.25` — worst 1.229x
  (0.370/0.301); 2x half-width rule asks 1.239x. 1.25 clears both.

Both entries arm the ratchet-asymmetry guard in each direction for this
cell's future records.

## Fresh gate draws after recording (Part A exit evidence)

- `echo_speculated/minimal --gate`: **gate OK**, ratio 0.402, paint 0.325
  (loads 0.11–0.15) — both inside the recorded bars.
- `echo/minimal --gate`: **gate OK**, ratio_p50 1.165 against the
  recorded 1.1301 with the accepted 1.1719 shortfall held — the honest
  round-trip row is unmoved by speculation, and the baseline diff touches
  only the four inserted `[echo_speculated.minimal]` lines.

## Attestation scope

dev-linux is a shared class: the `speculated_ratio_p50` max=1.0 budget
verdict attests only on a `controlled-*` class (none published yet); this
class ratchets the recorded bars, which is the standing gate-attestation
split. `speculated_paint_p99_ms` is a tail absolute — recorded here,
gated on controlled classes. The gh-linux/gh-macos cells for this
scenario record post-push (Ruling 17, `pending-first-push.md`).
