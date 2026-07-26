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
| 3 | `task perf-audit` reaches a verdict, not a false breach | Blocked on #22 (stale first_paint bars) |
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
| §3.1 budgets are CI gates | **They are not.** Nothing in the codebase compares any measurement to a spec budget. The gate is a pure regression ratchet. |

**Net effect on the product story:** better, not worse. Cold start is a
real and large win on both fixtures. The two genuine losses — typing
~1.2×, scroll ~2× — are unchanged and still unattributed.

## Still genuinely open (not artifacts)

| What | Status |
|---|---|
| Echo ratio 1.2× slower than nvim | Cause unattributed. Three explanations falsified. The `nvim --remote-ui` control settles whether it is protocol-inherent. **#19** |
| Scroll ratio ~2× slower | Unattributed, and §3.1 budgets no ratio for this row at all. Genuine spec gap. **#23** |
| Spec budgets unenforced | **#18** |
| Two spec amendments carry no user sign-off | Both marked provisional in §3.1. Needs a decision after #19 lands. |

## Open tasks, in the order they should be done

Ordered by information yield: each one's failure would invalidate work below it.

1. **#22** — re-record stale first_paint bars + split the shell-visible
   metric. Unblocks a clean `perf-audit` verdict, which gates everything.
2. **#18** — §3.1 budget table in the gate. Will be born red (two cells
   already over budget); that is correct.
3. **#19** — `nvim --remote-ui` control. Settles the echo ratio, which is
   the last claim the README hedges on.
4. **#23** — re-derive headrooms; fix the scroll row's tier mismatch and
   the flood row's cross-class stimulus divergence.
5. **#24** — allowlist the spawn environment. Do before CI runs with any
   secret configured.
6. **#21** — record a quiet-host dev-macos input_path baseline.
7. **#25** — noice's ext_* disable opts are not suppressing its errors.
8. **#20** — P4 plan adversarial review (a fresh session; prompt is at
   `.claude/plans/2026-07-26-p4-review-prompt.md`).

## Deferred with user approval

- Coverage-walk deferrals 1–4 (picker rows → P4, three-state compat → P4,
  compat-page hosting → P6, Windows legs → P6). Re-confirmed 2026-07-26.
- Publisher set / anodizer → P6. The pre-P4 push cuts no release.
