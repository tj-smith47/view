# Adversarial review: the flood shortfall acceptance (Fable 5, 2026-07-27)

Run under the standing rule that no concession or metric degradation is
accepted without an adversarial review by a Fable 5 subagent. Scope: the
proposed acceptance of `cadence_p99_ms` 16.429 against the spec 3.1 bar of
16.0; the refutation of the prescribed chunk-size pin; the replacement
`cadence_p99_ratio`; the guards around it; and whether the tier fix in
`c819428` launders a concession as a correction.

The reviewer read the raw log, the measurement note, all 516 lines of
`flood.rs`, `rows.rs`, `baselines.rs`, `budgets.toml`, `budgets.rs` and the
`c819428` diff, and re-derived the distribution numbers from the log itself.

**Nothing may be committed from the flood work until findings 1, 2 and 3 are
addressed.** They are pre-commit blockers, not follow-ups.

---

## Verdict on each claim

| Claim | Verdict |
|---|---|
| 16.429 is not a view defect | Survives, but mis-framed. Reframe per finding 1 |
| One run suffices | Survives for the ratio; **fails** for setting the shortfall's accepted value |
| The ratio replaces the cross-class comparison without laundering | Survives, contingent on findings 1 and 2 |
| The chunk-size pin is physically impossible | Survives with the overclaim trimmed (finding 5) |
| Ratio-of-p99s statistic and its naming | Survives; the gate policy keyed on word order is fragile |
| The guards are right and well placed | Mostly survives; three defects and one log gap (finding 3) |
| `c819428` is honesty, not concession | Survives under three conditions (finding 4) |

**Security and safety: nothing material.** The flood producer is a fixed
string with no injection surface, window-bounded, killed in
`BenchSession::Drop` with a bounded reap. The new pty responder pattern-scans
only our own child's output, with a bounded carry tail.

---

## Finding 1 (blocker) — 16.4 ms is a redraw-cadence tail, not a stall

Derived from the log (`FLOOD_WINDOW` in `bench.rs` is 15 s; gap counts from the
per-trial lines of `flood-ratio.log`):

| trial | view mean gap | nvim mean gap |
|---|---:|---:|
| 1 | 12.53 ms | 12.35 ms |
| 2 | 12.30 ms | 12.40 ms |
| 3 | 12.35 ms | 12.46 ms |

p99 16.43 / 16.45 ms against a ~12.4 ms mean is **p99/mean 1.32**. A
stall-shaped distribution detaches its p99 from its bulk; this one hugs it.
Both sides paint at ~80 Hz with a jitter edge near 16 ms, so the p99 is the
upper edge of a regular cadence rather than a stall.

The 60 Hz coincidence is probably not vsync, since the mean 12.4 ms is not
16.7 ms. The likelier shared mechanism is the embedded editor's own
terminal-refresh throttle plus event-loop jitter -- **the reviewer labels that
assumed, having not read nvim's source**; the distribution shape is the
observed part. view's paint stream is downstream of the same nvim the control
measures, so the two sides landing within 0.2% is expected rather than eerie.

Consequences:

- The absolute 16.0 ms bar sits **below bare nvim's own healthy cadence tail**
  on this host. As applied to gap-p99, no implementation downstream of nvim's
  redraw stream can pass it here. "view is 0.43 ms over budget" is a category
  error, not an excused defect: the bar, inherited from scroll's staleness
  row, collides with the intended refresh cadence of the measurand.
- The metric is **not** uninformative. A real coalescing failure still
  detaches p99 from the 12.4 ms bulk and moves the ratio. It gates regressions
  correctly; what it cannot express on this stimulus is "meets 16 ms."
- **The ratchet trap.** `budgets.toml` carries this shortfall at
  `accepted = 20.880463`. Rewriting it to 16.429 from one run makes the
  never-widen ratchet fail the gate on the next 16.8 ms jitter excursion. The
  `why` **must** change, since it cites the now-refuted cross-class rationale,
  but `accepted` must come from a multi-run spread or stay at 20.88.

Cheap disconfirming tests, in order of power:

1. Print the gap **p50/p90** from one run. The vectors already exist in
   `FloodSide.cadence_gaps_ms`; the harness simply does not report them.
   p50 near 12-13 means cadence jitter; p50 near 16.4 means a hard throttle.
   One run decides this outright.
2. Throttle the producer to a trickle that still changes the screen. If p99
   stays near 16 with zero coalescing pressure, the number is the refresh
   cadence, definitively.
3. The mbp pair: different kernel, different chunking. If view and nvim land
   within a few percent again, the shared-upstream story is confirmed
   cross-host.

**Do:** run test 1 before committing; write the shortfall `why` as the
throttle-tail mechanism with the ratio as evidence; do not tighten `accepted`
on n=1.

## Finding 2 (blocker) — the ratio gates on shared classes on transplanted evidence

The measurement note justifies load-robustness by citing echo's `ratio_p50`,
a **median** of **per-sample interleaved** pairs. `cadence_p99_ratio` is a
**tail quotient over two sequential 15 s windows**: a load spike in one window
and not the other does not cancel.

The project's own doctrine says exactly this. The `gate_headroom` doc comment
in `baselines.rs` records that `ratio_p99` carries a ±50% ambient floor and is
**exempted** on shared classes, yet the `headroom_policy_maps_metric_kinds`
test in the same file deliberately gives
`cadence_p99_ratio` a shared-class gate at `RATIO_HEADROOM` 1.25.

The counterargument may well win: these tails are throttle-dominated at 16 ms
scale rather than scheduler-dominated at microsecond scale, so they may be far
stabler than echo's. **That is unmeasured -- one run, one host.** The project
has a protocol for exactly this, the eight-replicate load-regime
characterization behind `2026-07-27-gate-headroom-measured-not-assumed.md`.
This metric skipped it and went straight to gating.

**Do:** keep the metric and record it, but do not let it gate on shared
classes until a load-regime characterization exists -- or run that
characterization first (cheap: unchanged binaries, repeat runs). Otherwise the
failure mode is a flapping gate whose headroom gets hand-widened later, which
is how a real regression hides.

On the statistic itself: a ratio of two p99s is the only honest option, since
there is no per-sample correspondence to pair, and the doc comment at
the `gated_cadence_p99_ratio` doc comment in `flood.rs` plus the name ordering
are adequate. **But** the classification mechanism is a booby trap:
`gate_headroom` keys the exemption on the suffix `ends_with("ratio_p99")`, so
the word order
in a metric name is load-bearing gate policy. A future paired-tail statistic
named `foo_p99_ratio` would silently gate on shared classes, and a consistency
rename flips policy. The test at 1230 documents this; nothing enforces it. An
explicit policy table is worth having -- noted, not a blocker.

## Finding 3 (blocker) — three guard defects and one evidence gap in flood.rs

The shape and placement of the guards are right. The defects:

1. **Wrong value in the nvim-side resolution error.** The nvim-side guard in
   `flood.rs` checks
   `cadence_is_measurable(nvim_cadence_p99_ms, nvim_probe_period_ms)`, which
   is correct, but the `BelowInstrumentResolution` it raises reports
   `resolution: probe_period_ms` -- the **view** side's period. The diagnostic
   lies about the number that tripped it.
2. **Resolution is enforced only on run-level medians**, at the two
   `cadence_is_measurable` calls, while per-trial ratios are formed earlier in
   `cadence_pair`. With three trials, a single floor-contaminated trial
   can be the median ratio while the median-of-p99s still clears the guard.
   Move the measurability check per trial, per side, against that trial's own
   `probe_period_ms`.
3. **`NotEnoughSamples { warmup: min_gap_samples }`**, raised by
   `cadence_pair`, renders through its `thiserror` message in `lib.rs` as
   "only N samples collected with an M-sample **warmup**", but flood has no
   warmup; M is the gap floor. The error
   message misdescribes the run.
4. **Evidence gap:** `probe_period_ms` is never printed (`rows.rs` flood arm,
   lines 275-314). `EXIT=0` proves the guard passed, but the log -- the
   artifact the acceptance cites -- cannot show what the instrument floor was.
   Print it beside the gap counts.

Otherwise the guards survive. The both-sides sample floor is right (a ratio is
only as good as its denominator), the positive-finite denominator check is in
the right place (per trial, before division), and the unit tests
(the `tests` module in `flood.rs`) genuinely exercise the refusal paths,
including the scale-invariance property test, which is the row's whole
argument in one test. Nothing is unreachable.

## Finding 4 — c819428 is a legitimate correction, under three conditions

The spec rows name tier full; the bench was timing a `Basic`-tier child that
skipped the sync bracket every frame. That is a broken instrument producing
false passes, and the recorded baselines were flattering by construction.
Recording worse numbers when re-measured at the named condition is the bar
getting honest. **This claim survives.** Conditions:

1. **The synthetic replies model a terminal state no real terminal starts in.**
   `SYNC` replies `\x1b[?2026;1$y`, where DECRPM 1 means "set" -- the doc
   comment says "permanently set", which is 3, so that is a doc error at
   minimum -- and real terminals answer `2026;2` (reset but recognized) before
   the app enables the mode. `KITTY` replies `\x1b[?1u` (flag already active)
   where real terminals answer `?0u` until the app pushes flags. If view keys
   only on "recognized", tier derivation is fine, and the commit's startup-log
   evidence shows `Full` derived -- but "derives Full" is not "does the same
   per-frame work as under kitty or wezterm." **Missing evidence:** one
   comparison of view's emitted enable and bracket sequences under the bench
   pty against a real modern-terminal transcript. Cheap, and it closes the last
   way this fix could still measure the wrong condition.
2. Re-recorded baselines must carry the instrument change as provenance, in the
   commit message, per the repo's own performance-contract rule -- or the
   history reads as a view regression.
3. Any metric newly over its spec budget after the re-record needs a
   `[[shortfall]]` entry whose `why` names the tier fix. That is the mechanism
   that makes "recorded as-is" honest rather than quiet.

## Finding 5 — the chunk-pin refutation is sound but overclaimed

The six-points-per-host table tests the two knobs the original prescription
implied (producer block size, `-opost`), and the kernel-buffer mechanism
(Linux ~4095 B tty output buffer, macOS 1024 B) is coherent and explains every
row. But "cannot be pinned" from n=6 is overclaimed as written, because one
knob was not tried: a **paced** producer (write a block, wait for drain) *can*
pin delivered chunk size on both kernels.

The reason pacing does not rescue the pin is stronger than the table:
**pacing un-floods the flood.** A rate-limited producer no longer exercises the
unbounded-backpressure invariant the row exists to test, so any producer that
pins the stimulus destroys the stimulus. Putting that sentence in the note
makes the impossibility claim airtight for this row.

Secondary caveat to label: the probe read with a 1 MB buffer flat out, while
nvim's read cadence differs, so the specific byte values (40 B, 194 B and the
rest) are probe-specific. The kernel-owns-the-last-hop conclusion is derived
for nvim's actual reads, not observed.

## On attack 3 — the ratio does not launder the problem, with one dependency

The structure is honest: the absolute `cadence_p99_ms` stays per class, and the
both-sides-stall-catastrophically hole **is** covered on dev-linux. Not by the
baseline gate, since `is_absolute_tail` exempts it on shared classes
inside `gate_headroom`, but by the **shortfall ledger**: a both-sides 50 ms stall
fails "worse than accepted 20.88" regardless of the ratio reading 1.0. That
cover depends entirely on the shortfall entry surviving with a sane `accepted`
value, which is why finding 1's ratchet warning is load-bearing, and why the
entry must never be deleted while the budget is unmet.

---

## Still missing before the acceptance is fully evidenced

1. The gap p50/p90 print -- decides throttle versus stall.
2. The mbp cross-host pair.
3. A load-regime characterization for `cadence_p99_ratio`.
4. The real-terminal transcript comparison for the full-tier replies.
