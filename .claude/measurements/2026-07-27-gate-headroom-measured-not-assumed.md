# Gate headroom, measured: 1.25 was 12x looser than this host needs, and 1.5 cannot work at all

Date: 2026-07-27
Class: dev-linux (12-core VM, ambient load uncontrolled by design)
Method: 8 report-only `echo/minimal` runs, one unchanged binary pair,
3 trials of 1000 samples each, host load recorded per run.

## What was assumed

```rust
pub const RATIO_HEADROOM: f64 = 1.25;     // gated ratio metrics
pub const ABSOLUTE_HEADROOM: f64 = 1.5;   // everything else
```

Two round numbers, applied to every metric on every class. The spec already
flagged the first as suspect: 1.25 is roughly 5x the apparatus's own
resolvable effect, so `echo.minimal` could degrade 25 percent without
failing.

## What eight replicates say

Host load ranged 0.44 to 8.53 across the campaign, unforced -- the host
does foreign work and that is the condition a gate must survive.

| metric | median | min | max | half-width | worst / recorded |
|---|---|---|---|---|---|
| `ratio_p50` | 1.1735 | 1.1570 | 1.1970 | **1.70%** | **1.0214** |
| `view_p99_ms` | 1.0000 | 0.9250 | 6.6760 | **287.55%** | **7.38x** |

Two conclusions, opposite in direction.

**The ratio needs far less headroom than it has.** Per-sample interleaving
puts view and nvim under the same ambient shift, so the quotient survives
what the operands do not: across a 19x load range the ratio moved 1.7
percent while the absolute tail from the *same runs* moved 7.4x. The
pre-registered rule for distinguishing conditions from single measurements
is 2x half-width, here 3.4 percent. dev-linux now gates `ratio_p50` at
**1.06**, which clears the pre-registered threshold, clears the worst
observed excursion by 2.8x, and replaces an allowance 12x larger than the
spread it was covering.

**The absolute tail needs headroom no constant can supply.** 0.925 ms to
6.676 ms on unchanged code is not a band, it is the host's mood. No
multiplier distinguishes a 7x regression from a busy afternoon. The three
high readings all came from runs starting above load 3.9, and run 4 shows
the coupling is to *instantaneous* contention rather than the one-minute
average: it started at load 7.74 and measured 1.054 ms.

## What changed

`is_absolute_tail` metrics no longer gate on a shared class, joining
`paired_delta_p99_ms` and `ratio_p99`, which already carried that exemption
for exactly this reason. The protection is not lost: a real slowdown in
view's own tail moves the paired ratio from the same interleaved run, and
that gates, now 4x tighter than before.

Two policy bugs surfaced while enumerating which metrics this touches, both
from matching a statistic by exact name rather than by what it is:

- **`control_delta_p99_ms` was gated as an ordinary absolute metric.** It is
  `paired_delta_p99_ms` for the control row -- the same signed statistic,
  scoped by a prefix. It was gated on shared classes where its twin was
  exempt, and under a proportional bar that inverts once the value goes
  negative, which is the state a paired delta is built to reach when view
  wins. Now matched by suffix, like the `ratio_p99` case the file had
  already learned this lesson for.
- **The shortfall ledger and the ratchet disagreed about what a regression
  is.** Fixed earlier the same day; the ceiling now derives from this
  policy, so a measured headroom moves both gates together.

## The shape the fix takes, and why

Headroom is now a property a class *measures*, not a constant the code
asserts:

```toml
# crates/view-bench/baselines/dev-linux.toml
[headroom]
ratio_p50 = 1.06
```

A metric absent from that table gates on the conservative default. **That
absence is a statement, not a gap to fill with a plausible number.** Only
`echo`'s `ratio_p50` has been characterised on this class; `marker_ratio_p50`,
`pace_ratio` and `control_ratio_p50` have not, so tightening them on this
evidence would be exactly the unverified-constant move this campaign exists
to end. They keep 1.25 until someone measures them.

A key naming no recorded metric is a load error, because that is the one way
the table can lie: the lookup would miss, the default would apply, and the
file would read as though a measured allowance were in force.

## Reproducing

```bash
for i in $(seq 1 8); do
  task bench -- --scenario echo --fixture minimal --class dev-linux
done
```
