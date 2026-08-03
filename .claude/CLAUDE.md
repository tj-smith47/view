# view — session context

## What view is

**view is the first fully native AI-first TUI terminal editor for agentic
development.** It is Neovim (painless migration), written in Rust (objectively
faster), with a modern, cohesive — but still configurable — UI.

That sentence is the differentiator, and it is the thing sessions lose first.
The three user contracts in spec §1 (painless migration, objectively faster,
modern coherent UI) are the **benefits** that follow from it, and the
performance mandate in §3 is a **quality bar**. Do not restate a benefit or a
quality goal when asked what makes this tool unique, and do not let a long
stretch of harness or budget work convince you the measurement layer is the
point.

None of that relaxes the bar. "Objectively faster, smoother UX than nvim" is a
shipping contract with CI-gated budgets, and "we are not done if we cannot
objectively say there is a faster, smoother UX over nvim" still governs every
change. This section exists to stop tunnel vision, never to license a weaker
standard: the goal is both, always. The durable moat is the differential
oracle and the accumulated compat/perf evidence (§1, §13.2) — that is what
makes the strangler roadmap (§15) possible, and it is a different thing from
the differentiator.

**START HERE:** read `.claude/HANDOFF.md` before acting. It carries the working
mode, the open task list, and the judgment that no other file records. Create
tasks from its open-task section at startup — the harness task store does not
persist across sessions, so that file is the only carrier. HANDOFF.md is
machine-local and untracked (as are STATUS.md, known-bugs.md, archive/, and
pending-first-push.md); a fresh clone has none of them and starts from the
spec and plans instead.

Spec of record: `.claude/specs/2026-07-17-view-design.md`. Plans:
`.claude/plans/INDEX.md`. On conflict, spec wins.

Hard rules (in addition to global rules):
- nvim owns all buffer text. No view subsystem holds authoritative text
  state. Buffer mutation happens only through `Effect::Rpc`.
- The paint loop never awaits RPC. The RPC reader thread never blocks.
- No unwrap/expect/panic in lib crates (workspace lints enforce; do not
  weaken them).
- Dependency direction: core ← surface ← {native, ai}; only view-engine
  speaks RPC; only view-tui touches the terminal.
- Performance is a contract: any change touching key dispatch, grid apply,
  or paint states its latency consequence in the PR/commit description.
- Use `task` targets, never raw cargo, for build/fmt/lint/test/commit.

## Subagents: gates run in the FOREGROUND

If you are a subagent (implementer/reviewer/fixer): run `task ci`,
`task compat`, `task commit`, and any other gate in the FOREGROUND with a
generous timeout (10-15 min), redirecting output to a file under
`~/.claude/tmp/` on the first run and reading it selectively. Do NOT use
run_in_background for a gate and then end your turn to "wait for the
notification" — background-child notifications do not resume a subagent
whose turn has ended; they go to the coordinator, and you simply stall.
The global no-polling rule stands (no sleep/until loops, hook-denied);
foreground-with-timeout is the compliant way for a subagent to wait.

## Enforcement

- `task ci` = fmt-check, lint, audit, style, loc, test. Commit only via `task commit -- -m "<msg>"`.
- `scripts/audit-deps.sh` enforces crate dependency direction; `scripts/check-style.sh` enforces comment/doc style. Both run in CI.
- `.claude/settings.json` hooks block `git push` and plain `git commit`, and check edited Rust files for formatting and comment style.
- Conventions for Rust code: `.claude/rules/rust.md`.
