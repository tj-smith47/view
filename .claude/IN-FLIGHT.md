# In-flight state, 2026-07-27

Read this immediately after `.claude/HANDOFF.md`. It records work that exists
in the working tree but is **not committed**, and the decisions behind it.
Delete this file once its contents have landed as commits.

Last commit: the flood paired-cadence work (task 30). **Nothing has ever been
pushed.**

## Uncommitted working tree

Nothing of this file's own. The env.rs allowlist that sat here uncommitted
landed in `efb594d`; everything else it recorded landed earlier.

### Task 24, the spawn-environment allowlist -- LANDED in `efb594d`, reviewed in `d32daf0`

`env::hermetic_sweep()` is the single primitive; `make_hermetic` (pty and
plain `Command` alike) and `EngineConfig::env_plan` both consume it, and the
`HOST_REDIRECT_VARS`/`HOST_SEARCH_PATH_VARS` enumerations stay as the second
layer applied after the sweep, since only they bind a caller who sets one of
those names deliberately. `SpawnEnv` gained `value_of` rather than an `is_set`
query: a swept name is dropped only while the builder still holds the host's
own value for it. `d32daf0` closed the review findings: a third host layer,
`HOST_SUBPROCESS_CONFIG_VARS`, points `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM`
at a path that does not exist, closing git's configuration-*file* layers.
The re-review proved `.netrc` credentials still rode the then-allowlisted
`HOME` (libcurl's lookup sits below the config layer); the fix-round on top
re-points a hermetic child's `HOME` at a guarded harness-owned dir
(`HERMETIC_HOME_VAR` -> `env::hermetic_home`), closing `.netrc`, `~/.ssh/*`
and the ignore-file default in one move. Windows OpenSSH resolves the profile rather than
`HOME`; that residual is recorded in `env.rs`, not closed.

What the next session needs from it:

- Baselines recorded before `0299417` measured children with a different
  environment (first a far larger one, then a host-resolved `HOME`) and are
  invalid. Task 32's re-record must run against that revision or later,
  never across it.
- The allowlist keeps neither `SSL_CERT_FILE`/`SSL_CERT_DIR` nor any proxy
  variable, and the compat fixture git-clones plugins from inside a hermetic
  child. Verified fine on dev-linux (`cold-bootstrap` green, real network) and
  unverified on any host whose CA bundle or proxy is non-default; the failure
  there is a loud clone failure, not a silent measurement.

## The adversarial review, and what remains of it

`.claude/reviews/2026-07-27-flood-concession-adversarial-review.md` holds the
full Fable 5 review of the flood acceptance, run under the standing rule that
no concession or metric degradation is accepted without one.

Its three pre-commit blockers (findings 1, 2, 3) and the finding 5 note
corrections are addressed in the flood commit. The decisive measurement it
asked for was taken: gap p50 12.24 ms against a 15.39 ms p99, which is the
pre-registered jitter-tail reading, recorded with its host load in
`.claude/measurements/2026-07-27-the-flood-stimulus-cannot-be-pinned.md`.

What the review left open, none of it blocking the commit:

- Finding 1's ratchet warning: the flood `[[shortfall]]` `why` still cites the
  refuted cross-class rationale (task 31), and `accepted = 20.880463` must not
  be ratcheted down off a single run. That entry is also what covers the
  both-sides-stall hole, so it must not be deleted while the budget is unmet.
- Finding 2's load-regime characterization for `cadence_p99_ratio`, which is
  the only thing that would earn it a shared-class gate.
- The mbp cross-host flood pair.
- Finding 4's real-terminal transcript comparison for the full-tier replies.
- Security and safety: nothing material found.

## Open task list at handoff

The harness task store does not persist across sessions, so this table is the
carrier between them. **The live task list is the harness one**, created from
this table at session start; this table is the handoff copy.

`TaskCreate`/`TaskUpdate`/`TaskList`/`TaskGet` are deferred tools, reached with
`ToolSearch` (the one taking a `query`) via `select:TaskCreate,TaskUpdate,...`.
The regex-only tool-search variant does not index deferred tool names and
returns nothing for them; that is not evidence the store is unreachable.

| # | Task | State |
|---|---|---|
| 30 | Fold the adversarial review's findings into the flood work | Done; landed with the flood commit |
| 23 | Re-derive gate headroom; fix the tier and stimulus mismatches | Sub-problem B committed in `c819428`; sub-problem C landed with the flood commit. Closed |
| 31 | Rewrite the flood `[[shortfall]]` `why` in `crates/view-bench/budgets.toml` | It still blames the measurement, an attribution now refuted. Unblocked |
| 24 | Allowlist the environment at the bench/oracle spawn funnel | Landed in `efb594d`, review findings closed in `d32daf0`. `task ci` green on dev-linux (717 tests) and natively on windows-msvc (654 tests). Closed |
| 32 | Re-record dev-linux baselines after the tier and spawn-env changes | Unblocked by `0299417`, the last revision to change a hermetic child's environment (`HOME` re-pointed); the re-record must run at or after it. Numbers will move worse; record as measured, and name the instrument change in the commit message. `cadence_p99_ratio` is a new recorded metric this record must pick up |
| 21 | Re-record dev-macos on a quiet mbp: `input_path` and `first_paint`'s split metrics | Needs a quiet mbp, not more code. Until it lands, the dev-macos `first_paint` cell gates red on `unmeasured_metrics` |
| 33 | Close the `Verdict::New` budget-check flake risk for absolute tails on shared classes | Same flake shape the ratchet had; untouched |
| 25 | noice's `ext_*` disable opts are not suppressing its startup error notifications | Untouched |
| 28 | Give Windows an inline-write fast path, or record why it cannot have one | Untouched |
| 29 | Give the remaining hot-path micro-benches cold variants | Untouched |
| 10 | Execute the P3 exit checklist, evidence-cited per plan protocol | Gates the phase |

Also carried, from the review's own "still missing" list: a load-regime
characterization for `cadence_p99_ratio`, the mbp cross-host flood pair, and a
comparison of view's emitted enable and bracket sequences under the bench pty
against a real modern-terminal transcript (the synthetic DECRPM and kitty
replies model a state no real terminal starts in).

## Process note for the next session

This session ran far too long and expanded its own task list while reporting
that it was cleaning it up. Keep the queue closing, not growing: the
convergence rule already agreed is that **no new measurement-layer task enters
the queue unless it blocks an optimization.** That rule was not honoured here.
