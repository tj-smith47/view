# Remote-mode footprint: first recording + paired campaign (dev-linux)

2026-08-16, tree at `046d06e`, binaries the `task bench` quartet built from
that tree. User-granted quiet window: loads 1.20–1.55 across every draw
(elevated vs the echo campaign's 0.07–0.50 but flat — the source is this
session's own just-finished CI run draining, no foreign builds), zero
foreign cargo/rustc/cfgd processes. Logs: `~/.claude/tmp/t11-campaign.log`,
`t11-gate.log`.

## Campaign: 8 interleaved replicate pairs, remote_memory vs memory, minimal

Gated statistic per replicate = pss_mb, the same 1000-sample (+100 warmup)
shape both rows share. Rows are a verified minimal pair (T11 fix round 1):
same workload, same positional, same sampling — transport is the only
difference.

| rep | remote_memory pss_mb | memory pss_mb | load at start |
|---|---|---|---|
| 1 | 3.734 | 3.732 | 1.22 |
| 2 | 3.707 | 3.680 | 1.25 |
| 3 | 3.762 | 3.736 | 1.42 |
| 4 | 3.746 | 3.672 | 1.40 |
| 5 | 3.812 | 3.672 | 1.39 |
| 6 | 3.730 | 3.736 | 1.33 |
| 7 | 3.730 | 3.697 | 1.40 |
| 8 | 3.773 | 3.680 | 1.45 |

The `--record` pass that followed (load 1.43) drew 3.7109 — the ninth
remote draw.

## The paired comparison (Task 11 Step 2, the row's falsifiable check)

- remote_memory: nine-draw median 3.734, half-width 0.0525 (1.41%).
- memory: eight-draw median 3.6885, half-width 0.032 (0.87%).
- Delta: **+0.0455 MB (+1.23%)**, bands overlapping (3.707–3.812 vs
  3.672–3.736). Under the pre-registered two-condition rule (2× the larger
  half-width = 2.8%), the conditions are indistinguishable: view's own
  local footprint does not grow when the engine moves behind ssh. That is
  the resource-claim the spec row states, now measured rather than
  asserted.

## What was recorded, and why

`crates/view-bench/baselines/dev-linux.toml` `[remote_memory.minimal]`:

- `pss_mb = 3.7109…` — the genuine record draw, kept: second-lowest of
  nine, inside the campaign band. No hand-edit needed (contrast the
  echo_speculated paint record, where the record draw was a tail
  excursion).

Published spread (`dev-linux.headroom.toml`, scenario-scoped):

- `"remote_memory.pss_mb" = 1.04` — worst excursion above the recorded
  value 1.027× (3.812/3.7109); 2× half-width rule asks 1.035×. 1.04
  clears both.

## Fresh gate draws after recording

- `remote_memory/minimal --gate`: **gate OK**, 3.711 within the recorded
  bar.
- `memory/minimal --gate`: **gate OK**, 3.69 — the pre-existing local row
  is unmoved by this task; the baseline diff is the three inserted
  `[remote_memory.minimal]` lines only.

## Observations for later sessions

- memory.minimal's recorded bar is 4.962 but today's honest draws sit at
  3.67–3.74: the tree has grown ~25% leaner on this metric since that
  record. A deliberate re-record would tighten the ratchet accordingly —
  not done here because the budget row's `max = 6.0` rationale and the
  spec row both cite 4.962, and re-anchoring those is its own decision,
  not a side effect of a different task's window.
- Attestation scope: dev-linux is a shared class — the `max = 6.0` budget
  verdict attests only on a `controlled-*` class; this class ratchets the
  recorded bars (BUDGET SKIP printed, standing gate-attestation split).
- gh-linux/gh-macos cells for this scenario record post-push (Ruling 17,
  `pending-first-push.md`).
