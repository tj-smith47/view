# view — Implementation Plan Index

Spec of record: `.claude/specs/2026-07-17-view-design.md`. Plans derive from the
spec; on conflict the spec wins and the plan gets fixed.

| Phase | Plan file | Status |
|---|---|---|
| P0 Repo bootstrap | `2026-07-17-p0-bootstrap.md` | Complete |
| P1 Engine seam | `2026-07-17-p1-engine-seam.md` | Complete (final review approved for merge) |
| P2 Elm core + Surface + tiers + ext layers | `2026-07-18-p2-elm-runtime.md` | Complete (adversarially reviewed, 3 rounds) |
| P3 Oracle + compat suite + bench CI gates | `2026-07-18-p3-oracle-bench-gates.md` (charter: `2026-07-18-p3-p6-charters.md`) | Complete (merged at `4b44791`) |
| P4 Native features + theming | `2026-07-26-p4-native-features.md` (charter: `2026-07-18-p3-p6-charters.md`) | Reviewed (both adversarial rounds folded at `47e14bf`) |
| P5 AI (ACP client, panel, diff review) | `2026-08-09-p5-ai.md` (charter: `2026-07-18-p3-p6-charters.md`; ACP v1 verified against live spec at plan time) | In progress (5 review rounds; execution started 2026-08-18) |
| P5.5 Invented capabilities — engine supervision | `2026-08-09-p5_5-supervision.md` (charter: spec §9, §17 phase table) | Complete (T1–T7 landed through `ea81e66`, each adversarially reviewed; exit checklist closed 2026-08-14 — ci, oracle + hang schedules, acceptance script, and pristine perf-audit all green, evidence cited per checkbox in the plan) |
| P5.5 Invented capabilities — remote editing (incl. speculative echo) | `2026-08-09-p5_5-remote.md` (charter: spec §9, §5.6, §17 phase table) | Complete (every task through its SDD review loop; fable whole-branch final review 2026-08-17, all findings ruled and closed; F-C re-shape landed `d051993`..`b1a0c55` and the ratio gate recorded/validated in a 2026-08-18 quiet window (`a5be001`); exit checklist closed per-box with evidence in the plan) |
| P5.5 Invented capabilities — session DVR | `2026-08-09-p5_5-dvr.md` (charter: spec §9, §17 phase table) | Drafted (adversarially reviewed, 4 rounds + coordinator round 5) |
| P5.5 Invented capabilities — key introspector | `2026-08-09-p5_5-introspector.md` (charter: spec §9, §17 phase table) | Drafted (adversarially reviewed, 1 round) |
| P5.5 Invented capabilities — image viewing | `2026-08-09-p5_5-image.md` (charter: spec §9, §17 phase table) | Drafted (adversarially reviewed, 2 rounds; start-gated on P5.5-media's shared open-dispatch mechanism) |
| P5.5 Invented capabilities — media playback handoff | `2026-08-09-p5_5-media.md` (charter: spec §9, §17 phase table) | Drafted (adversarially reviewed, 4 rounds + coordinator round 5; Task 3 landing-order-gated on P5.5-supervision's `pause()`/`resume()` amendment) |
| P6 Polish, multigrid, doctor, Windows tier-1 | charter: `2026-07-18-p3-p6-charters.md`; full plan at P5 completion | Chartered |
| Migration integrity (capability probing, surface ownership, compat evidence, notification surface) | `2026-08-21-migration-integrity.md` (18 tasks + spec amendments in `2026-08-21-migration-integrity-spec-amendments.md`, landing as their own commits ahead of the tasks that rely on them; open question in `…-open-questions.md`; charters: `2026-08-20-migration-integrity-charters.md`; origin: first release-binary dogfood over SSH, 2026-08-20) | Planned (2026-08-21; fable-reviewed, 4 rounds to SHIP; start-gated on P5 completion, user-ruled; fresh session) |
| Post-v0.1 charters (reattach persistence, agent-fleet attention, theme-switcher interop) | `2026-08-14-post-v01-charters.md` (charter format per `2026-07-18-p3-p6-charters.md`; origin: tmux/herdr/omarchy gap analysis) | Chartered (2026-08-14; adversarially reviewed, 1 round, findings folded) |

Policy: each pending plan is written when its phase starts, against the real
interfaces produced by the previous phase, using the same task format as P0/P1
(bite-sized TDD tasks, complete code in steps, no placeholders). Writing them
earlier would mean inventing signatures that P1–P3 haven't produced yet.

Every phase exit requires: `task ci` clean, phase budgets from spec §3.1
measured where the harness exists, `.claude/known-bugs.md` drained or
explicitly deferred by the user, and a dogfooding note appended to
`.claude/dogfood-journal.md`.

P1 exit: evaluate a repo-local /phase-exit skill (ci + budgets + known-bugs drain + dogfood note) once the budget harness exists.
