# Measurement-layer rulings (2026-08-08)

The five open harness-semantics questions the 2026-08-03 dev-macos campaign
left behind, each either ruled here with the evidence that decided it, or
explicitly surfaced for the user where the answer changes gating semantics.
Question numbering follows the P4 plan's "Measurement-layer carry-ins"
section.

## 1. `--record` can pin a wide-spread cell to a below-median draw — SURFACED

`echo.heavy ratio_p50` on dev-macos spans 0.974–1.218 across 8 quiet-host
replicates (within-replicate trial spread 2–8% against 25% between
replicates; both processes' absolute p50 redraw in discrete tiers per
spawned pair — core placement, not load). Ratchet-only-tightens means a
0.974 draw recorded becomes a bar that fails a large fraction of honest
draws forever.

What already stands: the 2026-08-03 ratification recorded that cell at the
replicate median (1.114) by hand, and `budgets.toml`'s shortfall entry
carries the same value, so the shipped bars are not currently pinned to a
lucky draw.

Options measured, for user adjudication (any of these changes what
`--record` writes, which is gating semantics):

- **(a) Status quo + documented practice** — wide cells are recorded from a
  replicate-campaign median by hand, as done 2026-08-03. Cost: discipline
  only; risk: nothing enforces it on the next record.
- **(b) Replicate-median record mode** (`--record` runs N full replicates
  for cells with a published per-scenario spread, records the median).
  Cost: ~N× record time on those cells; removes the hand step.
- **(c) Ratchet asymmetry guard** — `--record` refuses to move a cell with
  a published per-scenario spread further below the current recorded value
  than that spread, forcing the operator to state a campaign when a large
  "improvement" appears. Cost: one refusal message; catches the lucky-draw
  case without new measurement.

## 2. Single-shot gating cannot resolve <25% regressions on that cell — SURFACED

Same evidence base as 1: on dev-macos the between-replicate spread of
`echo.heavy ratio_p50` (25%) dwarfs the within-replicate spread (2–8%), so
one gate invocation cannot tell a 20% regression from placement luck.
Candidates, for user adjudication (each changes what a gate verdict means
on that cell):

- **(a) Replicate-median gating** for cells with a published wide spread
  (gate = median of N spawns' gated statistics). Cost: ~N× gate time on
  that cell; resolution improves toward the within-replicate 2–8%.
- **(b) Placement-robust pairing** — respawn both sides per trial and pool,
  so per-spawn core placement averages out inside one invocation. Cost:
  scenario protocol change (echo currently spawns once per side per run);
  resolution gain unmeasured until implemented.
- **(c) Accept 25% resolution on shared dev-macos** and leave finer
  resolution to a controlled class when one exists. Cost: none now; a
  <25% regression on this cell stays invisible on this class.

## 3. `scroll.minimal` dev-macos knife-edge headroom — RULED

The sidecar factor 1.024 puts the ceiling at 2.3816 × 1.024 = 2.4387 while
the worst included quiet draw was 2.438 — 0.03% of margin. Ruling: the
factor stands as characterized; it was sized by the pre-registered rule
(worst reading over recorded value, worse fixture governing) and the
knife-edge is a property of the recorded value being itself a high-band
draw, not of the factor being wrong. A gate draw above 2.4387 is new
evidence to adjudicate under the campaign protocol — rerun quiet; if quiet
draws repeat above the ceiling, re-characterize the factor from the new
campaign — never a license to nudge the factor without replicates, and not
automatically a regression verdict against view. Recorded as a comment on
the sidecar entry. No semantics change: the gate already fails loud and the
operator adjudicates.

## 4. dev-macos echo routes to the compiled 1.25 default — RULED

After the falsified host-wide `ratio_p50 = 1.02` key was removed, echo on
dev-macos gates its shortfall ceilings under the compiled `RATIO_HEADROOM`
1.25. Ruling: absence stands as correct, not merely unfinished. Evidence:
the observed clean spread on this class's echo cells spans 0.974–1.2513
against a replicate median of 1.114 — worst clean draw / recorded ≈ 1.123
— so 1.25 covers the class's own honest draws with margin, while any
honestly-sized per-scenario factor (≥ the observed ~1.12) would buy at
most a 10% tighter ceiling on a statistic whose draws span nearly the
difference. On this shared class the only consumer of an echo factor is
the shortfall ceiling (tails do not ratchet here; spec budgets attest on
controlled classes only), so a tighter factor's only effect would be
false-failing honest draws. Characterizing the remaining scenarios stays
available to a future campaign, but nothing gates wrongly today for the
lack of it.

## 5. Auto-staleness dormancy on dev-macos — RULED

A shortfall entry is auto-reported stale only when a run measures it
inside its budget by more than the class's spread for the statistic; under
the compiled 1.25 default that means ~20% inside the bar, and dev-macos's
entries sit 0.6%–29% outside, so the mechanism is dormant there. Ruling:
the dormancy is the mechanism working, not failing. Loosening the
provably-inside rule so entries spend on smaller margins would spend them
on draws the class's own spread accounts for — the false-spend the rule
exists to prevent (`an_inside_reading_within_the_spread_does_not_spend_the_shortfall`
pins this). The documented path to spend a dev-macos entry is a replicate
campaign adjudicated by a person, exactly as `echo.heavy ratio_p50` on
dev-linux was spent this phase (two same-session paired rounds, both
inside the bar, consistent paired delta). No change recorded beyond this
ruling; ledger cleanup on that class stays human judgment.
