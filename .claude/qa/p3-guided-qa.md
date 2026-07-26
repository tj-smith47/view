# P3 guided acceptance QA — ~20 minutes

P3 shipped no user-visible editor surface: the oracle, the compat harness
and the bench matrix are all operator tools. The exit checklist allows
recording this step as a no-op for that reason. **It is not recorded as a
no-op.** Those tools are the phase's product, their consumer is an
engineer, and a gate whose output cannot be read is a gate nobody will
trust. So this pass is about the operator surface: does each tool say what
happened, and does a failure name itself.

Everything below runs from the repo root. Each step lists the command and
the exact expected result. Anything that deviates: note the step number and
what you saw — that is a finding, regardless of how minor it feels.

## A. The gate you will actually run

| # | Do | Expect |
|---|---|---|
| 1 | `task ci` | Six stages announce themselves in order (fmt-check, lint, audit, style, loc, test) and the run ends 0. The `loc` stage prints a file census AND a second line `crosscheck: N file(s), both counters agree`. |
| 2 | Edit any `.rs` file to add a comment saying `we should fix this later`, then `task ci` | Fails at `style`, naming the file, the line, and the rule (first-person pronoun). Revert. |
| 3 | `task commit -- -m "test: qa"` on a clean tree | Runs the whole ci chain FIRST, then commits. A failing chain must leave no commit behind. (Undo with `git reset --soft HEAD~1`.) |
| 4 | `git commit -m "x"` directly | Blocked by the hook, with the message telling you to use `task commit`. |

## B. Oracle — the differential moat

| # | Do | Expect |
|---|---|---|
| 5 | `task oracle` | Every corpus entry named with a pass/fail verdict; a summary line with the count. Ends 0. Takes minutes, not seconds — it drives a real pinned nvim per entry. |
| 6 | `task oracle -- corpus/insert-basic.toml` | The same, for one entry only. A single-entry run is the loop you will live in when chasing a divergence. |
| 7 | `cargo run -p view-harness --bin oracle -- fuzz --seed 1 --rounds 20` | Seeded fuzz rounds run and report. `--seed` is REQUIRED: omitting it must be a usage error, never a random default, because a fuzz finding you cannot replay is not a finding. |
| 8 | Repeat step 7 with the identical seed | Byte-identical round scripts. If two runs at one seed differ, stop — every quarantined entry the fuzzer has ever produced is suspect. |
| 9 | `cargo run -p view-harness --bin oracle -- fuzz --seed 7 --rounds 20` | Different scripts from seed 1, same clean verdict. |

## C. Compat — real plugins, real pty

| # | Do | Expect |
|---|---|---|
| 10 | `task compat` | Each scenario named with its verdict; a `compat/results.json` written. First run bootstraps lazy.nvim and is slow; later runs reuse `compat/.cache/`. |
| 11 | `task compat -- compat/scenarios/lualine.toml` | One scenario, same shape. |
| 12 | `cargo run -p view-harness --bin oracle -- page` | Regenerates `docs/compat.md` from `results.json`. Open it: every row carries plugin, version, engine pin, scenario, state, result and date. |
| 13 | Edit `.engine-pin` to a different version, rerun step 12, then revert | REFUSES, naming the pin mismatch, rather than publishing an evidence page describing a run against another engine. |

## D. Bench — the numbers, and the refusals

| # | Do | Expect |
|---|---|---|
| 14 | `task bench -- --scenario echo --fixture minimal --class dev-linux` | Host load printed at start AND end; three trial lines; a `gated <statistic> <value> (median of 3 trials)` trailer naming which number the gate compares. |
| 15 | `task bench -- --scenario input_path --fixture minimal --class dev-linux` | Additionally prints the segment decomposition (`pty->key-read`, `key-read->loop-wake`, `loop-wake->rpc-handoff`, `rpc-handoff->rpc-written`) and a `tap overhead` line with its bar. The first segment is outside the gated boundary by design — the row's doc says so, and the number is reported anyway. |
| 16 | `task bench -- --all --class dev-linux --gate` | Every row runs; a null-pair calibration brackets the run at BOTH ends; the verdict is per cell. |
| 17 | Hand-edit one cell in `crates/view-bench/baselines/dev-linux.toml` to an implausibly small number, rerun step 16, then `git checkout` the file | `GATE BREACH` naming the cell, the measured value, the bar, and the recorded value it came from — enough to act on without rerunning anything. |
| 18 | Hand-add a metric key that no row produces to a cell, rerun step 16, then revert | Refuses: a recorded metric the run never measured is an untested bar, and it must be reported rather than passed over. |
| 19 | `task perf-audit` | Full matrix gated, then every hot-path micro-bench. This is the pre-release gate; it should be the single command a release candidate has to survive. |

## E. Reading a failure

| # | Do | Expect |
|---|---|---|
| 20 | Run step 16 while a `yes > /dev/null` loop burns a core | Either the run completes with the load printed honestly at both brackets, or it REFUSES on the noise — never a quiet pass on a contaminated host. Kill the loop afterwards. |

Result: reply with "QA pass" or the list of step numbers + observations.
Findings feed the phase's known-bugs/fix process like any review finding.
