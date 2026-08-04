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
| P5 AI (ACP client, panel, diff review) | charter: `2026-07-18-p3-p6-charters.md`; full plan at P4 completion (verify ACP spec first via context7/docs) | Chartered |
| P6 Polish, multigrid, doctor, Windows tier-1 | charter: `2026-07-18-p3-p6-charters.md`; full plan at P5 completion | Chartered |

Policy: each pending plan is written when its phase starts, against the real
interfaces produced by the previous phase, using the same task format as P0/P1
(bite-sized TDD tasks, complete code in steps, no placeholders). Writing them
earlier would mean inventing signatures that P1–P3 haven't produced yet.

Every phase exit requires: `task ci` clean, phase budgets from spec §3.1
measured where the harness exists, `.claude/known-bugs.md` drained or
explicitly deferred by the user, and a dogfooding note appended to
`.claude/dogfood-journal.md`.

P1 exit: evaluate a repo-local /phase-exit skill (ci + budgets + known-bugs drain + dogfood note) once the budget harness exists.
