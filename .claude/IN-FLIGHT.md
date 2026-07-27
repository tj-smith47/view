# In-flight state, 2026-07-27

Read this immediately after `.claude/HANDOFF.md`. It records work that exists
in the working tree but is **not committed**, and the decisions behind it.
Delete this file once its contents have landed as commits.

Last commit: the flood paired-cadence work (task 30). **Nothing has ever been
pushed.**

## Uncommitted working tree

```
 M crates/view-engine/src/env.rs
```

That one file has not been through `task ci` in its current shape; the rest of
this file's earlier contents landed as the flood commit.

### Task 24, the spawn-environment allowlist -- PARTIAL, NOTHING CONSUMES IT

`crates/view-engine/src/env.rs` only. This is the one piece of the tree that is
**half-landed**: it adds `HERMETIC_PASSTHROUGH_VARS`,
`HERMETIC_PASSTHROUGH_PREFIXES`, `is_hermetic_passthrough` and `env_names_eq`,
with the reasoning for each entry in their doc comments, but **no caller uses
them yet**. It compiles (they are `pub`, so no dead-code warning) and changes
no behavior. Either finish it or revert that file; do not leave unconsumed
public API in the tree.

The design it was heading for, worth not re-deriving:

- The funnel today is a **denylist** (`HOST_REDIRECT_VARS` removed,
  `HOST_SEARCH_PATH_VARS` pointed at an empty dir). A denylist built from a
  documentation sweep can only be complete about the day it ran, and its
  incompleteness is silent: an unenumerated variable reaches a child, changes
  what it loads, and the child still starts and still measures.
- `make_hermetic` should sweep `std::env::vars_os()` and unset every name that
  is not passthrough. Keep the existing `HOST_REDIRECT_VARS` removal *after*
  the sweep as a second layer -- `env_isolation.rs` sets each of those to a
  marker to prove removal wins, so the ordering is load-bearing.
- **Do not add an `is_set` query to `SpawnEnv`.** It cannot mean the same
  thing on both builders: `portable_pty::CommandBuilder::new` pre-populates its
  map with the whole base environment, so `get_env` returns `Some` for every
  host variable and cannot distinguish caller-set from inherited (`EnvEntry`'s
  `is_from_base_env` is private). `std::process::Command::get_envs` returns
  overrides only. A trait method whose semantics differ per implementation is
  exactly the silent drift the `SpawnEnv` doc comment warns about.
- The rule that **does** behave identically on both: skip the sweep for a name
  whose builder value *differs* from the host's, since that difference is what
  a caller override looks like on either builder. Its one hole -- a caller
  setting a name to exactly the host's value -- fails loudly (the child simply
  lacks the variable) rather than silently.
- `EngineConfig::env_plan` in `view-engine/src/process.rs` is a **third** copy
  of the hermetic rule. It should consume the same primitive, or the engine's
  hermetic spawn and the oracle's will disagree across hosts, which is the
  divergence this task exists to kill. Its sweep entries must skip names the
  caller already planned (`plan_set` gives that test).
- Ordering constraint that is not obvious: the allowlist changes the spawn
  environment, so it **must land before any re-record**, or the recording is
  invalidated twice.

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

The harness task store does not persist, and in the 2026-07-27 session it was
not reachable at all (`TaskCreate`/`TaskUpdate`/`TaskList` are advertised as
deferred tools but no `select:` query surfaces them). This table is therefore
the live task list, not just a handoff copy: it is updated in the same turn an
item changes state.

| # | Task | State |
|---|---|---|
| 30 | Fold the adversarial review's findings into the flood work | Done; landed with the flood commit |
| 23 | Re-derive gate headroom; fix the tier and stimulus mismatches | Sub-problem B committed in `c819428`; sub-problem C landed with the flood commit. Closed |
| 31 | Rewrite the flood `[[shortfall]]` `why` in `crates/view-bench/budgets.toml` | It still blames the measurement, an attribution now refuted. Unblocked |
| 24 | Allowlist the environment at the bench/oracle spawn funnel | Partial, see above. Must land before any re-record |
| 32 | Re-record dev-linux baselines after the tier and spawn-env changes | Blocked on 24. Numbers will move worse; record as measured, and name the instrument change in the commit message. `cadence_p99_ratio` is a new recorded metric this record must pick up |
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
