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
pending-gha-verification.md); a fresh clone has none of them and starts from the
spec and plans instead.

Spec of record: `.claude/specs/2026-07-17-view-design.md`. Plans:
`.claude/plans/INDEX.md`. On conflict, spec wins.

Hard rules (in addition to global rules):
- **The goal is the bar, never the landing.** Quality, performance and
  reliability held high while working the open tasks toward a one-of-a-kind
  UX — that is the whole goal. Landing the branch on `master` exists only to
  obtain GHA checks; it is implicit in that work and is never a plan step, a
  milestone, an exit item or something "authorized". Mechanics, when a GHA
  run is needed: `master` directly until the first tag (no PRs — a needless
  gate the user has removed more than once), `git push origin <branch>:master`
  as a singular standalone command (the hook's ask-only guard is about
  command shape), and whoever lands it obtains the green run — never handed
  to a later session. **`master` is the only remote branch**: never push
  the local branch ref itself, never leave a second head on origin — the
  remote carries master and tags, nothing else (user ruling 2026-08-26).
- **Work is never kicked into a later phase.** Findings are fixed in the wave
  that found them. The one approved deferral: implement all features first,
  holding existing quality bars (ledgered when close), then one pre-v0.1.0
  performance-and-stability session. It exists so a single item cannot burn a
  week of context: a task that has absorbed ~4 h of tuning goes to that
  session's ledger with the hours spent, and the work moves on. A remark the
  user makes about one phase is scoped to that phase; it never becomes a
  standing rule.
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
- **A measurement needs a quiet host, and a quiet host needs the peer's
  consent.** Before `task bench` / `perf-audit` / `heartbeat-ab`: message the
  other session on this machine (`ListAgents` → `cfgd-*`) to finish its
  cargo work and hold, wait for its "go", `touch ~/.cache/view-quiet-host.lock`,
  measure, `rm` the lock, tell it "released". The hook denies those targets
  without a fresh lock; the lock is the receipt, not the coordination.

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

The tree, the target dir and the process table are shared with peer sessions
and their tests. Never `pkill`/`killall` by name (`nvim --embed`, `view`) —
a peer's live-nvim test fails spuriously and nothing tells it why. Kill
only pids you spawned; a stray from your own test is your harness's bug to
fix (reap on drop), not a sweep to run.

A signature that another crate calls changes by add-beside-then-switch, never
in place: land the new function next to the old one, switch the callers in
the crates you own, and delete the old one only in the commit that switches
the last caller. Files you do not own are being edited and gated by peers
at the same moment; an in-place change leaves their `task ci` unable to
compile until you finish, and a shim you had to restore under pressure is
the same edit done twice.

## Enforcement

- `task ci` = fmt-check, lint, audit, style, loc, test. Commit only via `task commit PATHS="<path> <path>" -- -m "<msg>"`: it runs ci, then commits exactly the named paths from the working tree (`--only`). Never `git add` — the index is shared by every session committing on this tree and a peer's staging can land at any moment, so what is staged is never the set to commit; the named paths are. Non-code changes (docs, plans, README, `.claude/` notes) commit with `task commit:quick PATHS="…" -- -m "<msg>"`, which skips ci and refuses a path under `crates/`, `scripts/`, `.github/`, `Taskfile.yml`, or the Cargo/engine pins.
- `scripts/audit-deps.sh` enforces crate dependency direction; `scripts/check-style.sh` enforces comment/doc style. Both run in CI.
- `.claude/settings.json` hooks block `git push` and plain `git commit`, and check edited Rust files for formatting and comment style.
- Conventions for Rust code: `.claude/rules/rust.md`.
