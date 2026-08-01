# view — where we are

One page. Updated 2026-07-26. If this disagrees with the task list, this
file is stale and the task list wins — but fix this file in the same turn.

## The one-line answer

P3's deliverables are built and green. What is between us and the first
push is **not features** — it is finishing the honesty pass on the
measurement layer, because this session found that three of the numbers
P3 was going to exit on were measuring the wrong thing.

## Road to first push

The push exists to do exactly two things (`.claude/pending-first-push.md`):
verify the GHA workflow on real runners, and get the CI badge slug. It cuts
no release. The repo now exists: `tj-smith47/view` (public, empty).

| # | Gate | State |
|---|---|---|
| 1 | Measurement layer tells the truth | **4 of 7 done** — see the ledger below |
| 2 | `task ci` green | Green as of the last full run |
| 3 | `task perf-audit` reaches a verdict, not a false breach | **dev-linux unblocked** (#22 re-recorded). dev-macos now fails loudly until re-recorded on mbp (#21) |
| 4 | `known-bugs.md` drained | 0 unchecked |
| 5 | README free of unattainable/unearned claims | **Done** — rewritten against measured evidence |
| 6 | P4 plan authored | **Draft done**, awaiting adversarial review |
| 7 | Dogfood note | Not started |

## The ledger: what changed about our performance story

This is the part that was hardest to track. Left column is what we
believed at the start of the session; right is what is now true.

| Believed | Now |
|---|---|
| first_paint 3.58 ms, 14× under budget | **Void.** It timed view's placeholder chrome against nvim's buffer window — two different events. A view that never attached its engine would have gated green. |
| first_paint.heavy = 7133 ms, 31× slower than nvim | **Artifact, overturned.** nvim-notify popups clipped the marker; view had painted immediately. Real: **104.3 ms, 1.6× faster than nvim** on a 14-plugin stack. |
| first_paint.minimal unmeasurable (socket collision) | **Fixed and measured: 26.5 ms p99 vs nvim 132.2 ms — 5× faster.** |
| input_path 100 µs is "physically inconsistent with the architecture" | **Overstated, withdrawn.** The gated interval is ~87 µs p50 dev-linux, ~75 µs bare metal — both *under* 100 µs. Only the p99 target fails, and only on real hosts. |
| dev-macos input_path floor = 230.0 µs | **Fabricated.** Hand-derived, never measured; the row could not even run on macOS. Withdrawn along with the 350 µs budget it supported. Real capture now exists (235.0 µs) but is not recorded — 2× trial spread, host at load 1.78. |
| input_path row works everywhere | **It did not run on macOS at all.** Fixed with adaptive tap pacing. |
| "All classes gate ratio_p50 measured-or-better" | **False.** The gate enforces `recorded × 1.25`; echo.minimal gates at 1.692 and can degrade 25% silently. |
| §3.1 budgets are CI gates | **They were not** — the gate was a pure regression ratchet, comparing nothing to a spec budget. They are now: `crates/view-bench/budgets.toml` states the table as data, the gate checks every measured cell against it, and `scripts/check-budget-drift.sh` fails CI if a number there stops appearing in the spec row it names. Ten metrics are outside budget today and every one is written down as a `[[shortfall]]` with the value it was accepted at and why. |
| The gate's headroom constants are calibrated | **They were two round numbers applied to everything.** Eight replicates over host loads 0.44–8.53 measured `ratio_p50`'s half-width at **1.70%** against a 1.25 allowance — 12× looser than the spread it covered — while `view_p99_ms` from the *same runs* moved **7.4×** (0.925→6.676 ms on unchanged code), which no constant can bound. Headroom is now a measured per-class `[headroom]` table; dev-linux gates `ratio_p50` at **1.06**, and absolute tails join the existing shared-class exemption. Two bugs surfaced en route: `control_delta_p99_ms` was gated as an ordinary absolute metric though it is a *signed* paired delta (proportional bars invert on negatives), and the shortfall ledger disagreed with the ratchet about what a regression is. |
| Micro-benches measure what a keystroke costs | **They measure the hot-cache state, which the editor never occupies.** The identical paint frame costs 2.94 µs back-to-back and **21.27 µs with a 10 ms keystroke gap before it** — 7.2× from idle alone, with the cold variant timing strictly less work. Every criterion bench here shares the defect: sound as relative regression instruments, ~7× optimistic as absolute costs. `paint_frame` now carries a cold variant; **#29** gives the rest one. |
| The shortfall ledger is a gate | **It was a coin flip, now fixed.** `Widened` compared the next measurement to `accepted` with zero tolerance, while `accepted` is one sample of a noisy statistic — so every listed shortfall had roughly even odds of failing CI. Caught by running the gate against a ledger written minutes earlier: measured 1.176 against accepted 1.172 and failed on a 0.35% difference. The ceiling is now the one the baseline ratchet already grants that metric on that class, so the two gates agree about what a regression is instead of one firing on noise the other absorbs. |
| The input_path budget is a spec bar | **It was a launderer.** 232 µs was the then-recorded 154.749 × `ABSOLUTE_HEADROOM` 1.5 — a bound computed from the measurement it bounds, which no measurement can fail. Withdrawn; the spec's original **100 µs** promise is restored and the gap to it (117.7 µs) is carried as a shortfall like every other. What is left is now sized against something independent: the one hop this path must make costs **80.0 µs p99 by itself**, measured without the editor by `input_handoff`. |
| The first-paint row measures one event with one budget | **Two events, now split and re-recorded** (1000 samples/cell, both null-pair brackets clean). `shell_visible_ms` (view's own chrome, unpaired, the ≤50 ms budget's real subject): **4.14 / 4.30 ms** p99 across both fixtures, 12x under. `marker_cold_ms` (the file on screen, paired): **26.5 / 120.5 ms** against nvim's 131.4 / 199.8, `marker_ratio_p50` 0.135 / 0.460. §3.1 stated no budget for the content metric at all; the user's call landed 2026-07-27 and it is now gated at **p99 ≤ 30 ms, ratio_p50 ≤ 0.30×** — under the ratchet's 39.8 ms so the budget can be the thing that fires. The heavy fixture is outside both and carried as a shortfall. |
| `screen_holds` matches what is on screen | **It could not match any phrase containing a space.** `row_text` concatenated cell contents, and an unwritten cell holds no contents, so every space was deleted: `"view: waiting for nvim..."` read back as `"view:waitingfornvim..."`. Every marker in the tree was a single word, so nothing had ever exercised it. Fixed; no recorded metric was affected (see the measurement note). |

| The typing gap is attributed but untouched | **Partly closed.** The runtime-loop-to-writer-thread hop is now an inline non-blocking write when the queue is empty, which is the first change this project has made that the product is measurably faster for. Segment `rpc-handoff->rpc-written` 42.5 → 14.1 µs p50; `echo.minimal` ratio_p50 1.3538 → **1.1719**, `ratio_p99` 1.3075 → **1.0917**; `input_path` p99 154.7 → **117.7 µs**. Proven by a back-to-back A/B whose bare-nvim arm did not move. |

**Net effect on the product story:** better, not worse. Cold start is a
real and large win on both fixtures. Typing is now attributed to view's
own code rather than to the protocol boundary, and a quarter of the gap
has been removed by acting on that attribution. Scroll (~2×) is unchanged
and still unattributed.

## Still genuinely open (not artifacts)

| What | Status |
|---|---|
| Echo ratio slower than nvim | **Attributed to view, and now being closed.** nvim's own remote UI costs 1.015/1.013 against bare nvim, so the protocol-inherent explanation is falsified alongside the three before it. The residual is per-hop wake cost on view's own path: collapsing the writer hop took `echo.minimal` from 1.354 to **1.172** and `echo.heavy` from 1.244 to **1.184**. The paint path's hops have not been attacked yet. |
| Scroll ratio ~2× slower | Unattributed, and §3.1 budgets no ratio for this row at all. Genuine spec gap. **#23** |
| Seven metrics outside their §3.1 budget | Enforced and ledgered as of #18, not fixed. Five are the echo ratio, now attributed to view's own input and paint paths and awaiting optimization work; one the flood stimulus divergence (**#23**); one a macOS absolute tail that moves with ambient load (**#21**). |
| Two spec amendments carry no user sign-off | Both marked provisional in §3.1. #19 has landed, so the decision is now unblocked and needs the user. |

## Open tasks, in the order they should be done

Ordered by information yield: each one's failure would invalidate work below it.

1. ~~**#22**~~ — done: first_paint split and re-recorded on dev-linux.
2. ~~**#18**~~ — done: §3.1 budget table in the gate, with a shortfall
   ledger for the seven metrics that do not meet it yet.
3. ~~**#19**~~ — done: the `nvim --remote-ui` control ran and refuted the
   protocol-inherent explanation; the README no longer hedges on it.
4. **#23** — re-derive headrooms; fix the scroll row's tier mismatch and
   the flood row's cross-class stimulus divergence.
5. **#24** — allowlist the spawn environment. Do before CI runs with any
   secret configured.
6. **#21** — re-record dev-macos on a quiet mbp: input_path, and
   first_paint's split metrics (its cell gates red until this lands).
7. **#25** — noice's ext_* disable opts are not suppressing its errors.
8. **#20** — P4 plan adversarial review (a fresh session; prompt is at
   `.claude/plans/2026-07-26-p4-review-prompt.md`).

## Deferred with user approval

- Coverage-walk deferrals 1–4 (picker rows → P4, three-state compat → P4,
  compat-page hosting → P6, Windows legs → P6). Re-confirmed 2026-07-26.
- Publisher set / anodizer → P6. The pre-P4 push cuts no release.
