# view — session handoff

Written at the end of a long P3 execution session, for the session that
replaces it. Everything the repo already records is in the spec, the plans, the
ledger, and `known-bugs.md` — this file carries only what those files do not:
judgment, defect classes, measurement discipline, and the working mode.

Read this file, then `.claude/CLAUDE.md`, then the ledger tail
(`.superpowers/sdd/progress.md`). Do not read the spec or plans end-to-end at
startup; grep them when a question arises.

> **Durability, corrected 2026-07-26:** `.claude/` is now **tracked** (commit
> `f52d344`); this file, the spec, the plans, the measurement notes and
> `known-bugs.md` are in git and a commit restores them. Only machine-local
> state stays ignored (`tmp/`, `scratch/`, `bench-baselines/`,
> `settings.local.json`, the scheduler lock). Do not re-add `.claude/` to
> `.gitignore`.
>
> `.superpowers/` **is still gitignored**, so the ledger
> (`.superpowers/sdd/progress.md`) exists only on this machine's disk. It is
> reconstructible from `git log`. `git clean` is denied by the global pre-bash
> policy (verified firing) — but that stops the assistant, not a human at a
> terminal.
>
> **Memory namespace warning:** the prior session's auto-memory lives under a
> different project key than a session started from `/opt/repos/view`. Assume
> NOTHING from that memory follows you. This file is the whole handoff.

---

## 1. First actions in the new session

1. Create tasks from section 3 below, verbatim, in the order listed. The
   harness task store does not persist across sessions and is not
   shell-accessible, so this file is the only carrier.
   `TaskCreate`/`TaskUpdate`/`TaskList`/`TaskGet` are deferred tools, reached
   via `ToolSearch` (the one taking a `query`) with
   `select:TaskCreate,TaskUpdate,...` — the regex-only tool-search variant
   does not index deferred names and returns nothing for them; that is not
   evidence the store is unreachable.
2. `git log --oneline -5` and `git status --short` to confirm section 3.1's
   tree truth still holds.
3. Read `.superpowers/sdd/task6-devmacos-rerecord-report.md` if it exists —
   the in-flight task's outcome (section 3.2). If it does not exist, that
   task did not finish cleanly.
4. Tail the SDD ledger for plan `2026-07-18-p3-oracle-bench-gates.md` under
   `.superpowers/sdd/` for the last few entries.
5. `task --list` to index the project's task targets before running anything.

Then resume at the first unfinished task. Do not re-derive the state; the
ledger and `git log` outrank your recollection.

---

## 2. State at handoff

- **Branch:** `dev/p3-oracle-bench`. Branches are per phase; `master` and
  `dev/p2` also exist. No worktrees (a deliberate resource-only decision, not
  an architectural one). Merge a phase branch to master locally at phase exit.
- **Nothing has ever been pushed.** The user's standing instruction: "We can
  push when we're done." Push is the only one-way door in this project.
  Commits and local tags are two-way and need no permission.
- **P3 is nearly complete.** Everything through the plan's twelve tasks is done
  and reviewed. What remains is the follow-on task list in section 3 plus the
  P3 exit checklist.
- Commits are made ONLY via `task commit -- -m "<msg>"`. Plain `git commit` is
  hook-blocked. It runs `git add -A`, so the tree must be stray-free first.

---

## 3. Open tasks — recreate these at startup

Ordered by the sequencing decisions already made. The dependencies in the
"why here" column are real; do not reorder without re-deriving them.

`.claude/STATUS.md` is the one-page current view of this list (road-to-push
gates, the believed-vs-now ledger, and the ordering below). Update both in
the same turn or they drift.

All P3 code tasks are closed. What remains is the exit battery: one
measurement leg per row, every leg run at the same frozen tip, then
citations, then the user's adjudication.

| # | Task | State / what remains |
|---|------|----------------------|
| 6 | dev-macos re-record | DONE — harness fix `d6ac7af`, bench TOMLs `f4521ab` (see 3.2). `f4521ab` was the frozen tip until #17's fixes moved it; the final tip is now `bb139c5` (+ docs `6043585`). Bench/gate evidence at `f4521ab` stays valid: the #17 commits touch only compat-runner/env-reset paths, no bench path |
| 15 | `task ci` at final tip | re-run (green at `cdf5f26`: 796 passed, 32 suites) |
| 16 | oracle corpus + fuzz seeds 1,2 x200 at final tip | re-run (at `cdf5f26`: 24/24 PARITY, both seeds 0 divergences) |
| 19 | zero-clock greps at final tip | cheap re-observe |
| 17 | compat at final tip | DONE — daily-config exposed 3 real harness defects, fixed at `58006ef` + `bb139c5` (= the NEW FINAL TIP); page committed `6043585`. Linux: plain 0, daily 15/15 OK. mbp at `bb139c5`: env tests 0 (non-root proof of the read-only reset), plain 0, daily x2 back-to-back 15/15 OK (reset proven across runs), hermetic home left empty. Logs: local `~/.claude/tmp/compat-{plain,daily}-v3.log`, mbp `~/compat-mbp-{plain,daily1,daily2}-bb139c5.log`, `~/envtests-bb139c5.log` |
| 18 | `task perf-audit CLASS=dev-linux` at final tip | quiet host, no overlapping loads |
| 26 | dogfood refresh at final tip | pending |
| 20 | citations into the plan's exit checklist | a draft exists but cites `cdf5f26`, which is stale — re-cite everything at the final tip |
| 21 | surface the adjudication list to the user | LAST — see 3.4 |

Sequence: #6 commit -> #15/#16/#19 -> #17 -> #18 -> #26 -> #20 -> #21.
Every measurement leg runs at the frozen final tip on a quiet host.

### 3.1 Tree truth at handoff (verify, do not trust)

Branch `dev/p3-oracle-bench`, tip `7a9f2e9`. The integration branch is
**master**, not main.

Uncommitted #6 results:

- `crates/view-bench/baselines/dev-macos.toml` (M) — the record wrote 12
  cells including this class's first `input_path` and `first_paint` rows.
- `crates/view-bench/baselines/dev-macos.headroom.toml` (untracked) — the
  hand-curated sidecar. `--record` never touches sidecars; an absent entry
  is a statement, not a gap to fill with a plausible number.
- `crates/view-bench/budgets.toml` (M) — `echo.minimal` `ratio_p50`
  re-accepted at 1.1125 (was 1.3436; clears the 1.10 bar by 1.14%);
  `echo.heavy` `ratio_p50` shortfall deleted as spent (1.0667, inside the
  bar); three new `first_paint` shortfalls — minimal `marker_cold_ms`
  42.542, heavy `marker_cold_ms` 219.997, heavy `marker_ratio_p50` 0.382.

Another session owns `.claude/known-bugs.md` (M),
`.claude/archive/known-bugs-2026-08.md` (staged) and
`SECURITY-POSTURE-HANDOFF.md`. Never commit them and never tidy them.
`task commit` runs `git add -A`, so every commit takes a trailing
pathspec; verify with `git show --stat HEAD` afterward.

### 3.2 Task #6 state — the validating gate is still owed

**Superseded 2026-08-02 by gate-5: the record itself is contaminated.**
The 12-cell record was taken on the pre-cleanup mbp — a host later found
running Stocks.app pegging a core, CleanMyMac agents, php-fpm pools, a
root mysqld, and Parallels helpers (all removed 2026-08-02, machine
rebooted). gate-5 ran on the cleaned host at load 1.04 with null-pair
calibration exactly 1.0000 and diverged from the record in BOTH
directions, which is the signature of a contaminated baseline, not of an
excursion:

- `echo.heavy ratio_p50` measured 1.2513 vs recorded 1.0666 — the clean
  number agrees with dev-linux (1.24-1.35) and the settled ~22% own-path
  attribution (5.7); the 1.0666 record now reads as a load artifact that
  slowed the nvim side of the pair.
- `first_paint` far BETTER clean: minimal `marker_cold_ms` 33.036 vs
  accepted 42.542; heavy 85.383 vs accepted 219.997.
- `echo.minimal view_p99_ms` shortfall reported STALE (inside budget).
- `echo.heavy ratio_p50` 1.251 vs spec 1.100 has NO shortfall entry
  (it was deleted as spent at 1.0667) — BUDGET FAIL.

Plan of record for #6 now: (1) full `--record` for dev-macos on the clean
quiet host; (2) re-derive `budgets.toml` shortfalls from the clean numbers
— stale ones deleted, `echo.heavy ratio_p50` re-enters vs the 1.10 spec
bar, `first_paint` accepted values shrink to the clean measurements;
(3) adversarial Fable 5 review of the baseline/shortfall diff (standing
rule: no concession or metric degradation without it — the echo.heavy
re-entry is concession-shaped even though it is an honesty correction);
(4) a fresh `task bench -- --all --gate --class dev-macos` at EXIT:0 on
the same quiet host; (5) only then the frozen-tip commit of exactly the
three files. Do not commit #6 before the EXIT:0 run exists. gate-5 log:
mbp `~/bench-gate-20260802-1621.log` (also in the dev-linux rtk tee dir).

**2026-08-03 state:** steps 1-2 DONE (clean record EXIT:0, log mbp
`~/bench-record-fresh2-20260802.log`; baseline pulled back, budgets
re-derived). Step 3 Fable review returned CHANGES REQUIRED, 8 findings.
Fixed so far: blocker 1 (marker_ratio_p50 why rewritten — clean spread
0.3785-0.3956 straddles the dirty 0.3818, the old "load flattered the
ratio" mechanism was false), warn 4 (echo.heavy view_p99_ms why: gated
ratio_p99 is 1.1481 not 1.020; spread not contention), warn 6 (mbp file
renamed `dev-macos.toml.ratchet-output-aborted-20260802`; true dirty
record preserved as mbp `~/dev-macos.toml.dirty-record-fb53ab6`),
suggestion 7 (percentiles labeled). IN FLIGHT: pre-registered spread
campaign on mbp (`~/bench-campaign-20260803/`, protocol
`~/campaign-20260803-protocol.md`, also local `~/.claude/tmp/`): 8
report-only replicates each of echo.heavy / scroll.heavy / scroll.minimal,
interleaved, fixed exclusion criteria (pre-load>2.0, calibration off >2%)
and a fixed decision rule — resolves blocker 2 (echo.heavy clean runs
disagree 1.0782 vs 1.2513; current diff would predictably gate-FAIL New)
and warn 5 (scroll ×1.2 sidecar factor was characterized under load
2.60-8.89; tightening owed). After campaign: apply decision rule
(possible baseline re-base + shortfall re-entry for echo.heavy; re-derived
scroll factor), rewrite the headroom sidecar provenance (blocker 3: its
comments still cite superseded baseline arithmetic; suggestion 8: replace
the "gate-3" label with the condition it names), then Fable RE-review
(resume the same reviewer via SendMessage), then steps 4-5.

**Campaign result + resolution (2026-08-03):** all 24 replicates EXIT:0,
orphan-clean. echo.heavy gated ratio_p50 over 8 replicates: 0.974, 1.053,
1.090, 1.104, 1.114, 1.177, 1.215, 1.218 — FAILED the pre-registered
unimodality check, so the unimodal factor formula (W/M) does NOT apply;
STOP-branch investigation found the mechanism: trials within a replicate
agree <3% while both processes' absolute p50 redraw in discrete tiers
between replicates (view ~3.2/3.5/3.9 ms, nvim ~2.9/3.3/3.6 ms) with NO
load correlation — per-spawn core placement. 1.0782 (record) and 1.2513
(gate-5) are ordinary draws. Resolution: baseline echo.heavy ratio_p50
re-based to campaign median 1.114 (included-set median; all-8 median 1.109
agrees within 0.5%); budgets.toml shortfall entry accepted=1.114 (median
honestly exceeds the 1.1 bar); bare `ratio_p50 = 1.02` REMOVED from the
sidecar (host-wide claim falsified — it was generalized from loaded-host
echo.minimal, and would false-fail honest echo.heavy draws), routing echo
to the compiled 1.25 default. scroll passed unimodality on the included
set; pre-registered formula applied verbatim: `"scroll.ratio_p50" = 1.024`
= max(2.402/2.3932, 2.438/2.3816) rounded up (excluded loaded replicates
all sit LOWER — contention compresses this ratio — so exclusion discarded
flattery). Also fixed the now-falsified `headroom_for` doc example in
`view-harness/src/baselines.rs` (echo↔scroll spread roles inverted);
commit that separately AFTER the three-file bench commit. Load-based
exclusion cut 17/24 replicates (the campaign self-loads the 1-min avg);
shown uncorrelated for echo, conservative-direction for scroll. NEW
adjudication items for #21: (a) `--record` ratchet-only-tightens semantics
re-tighten a wide-spread cell to any below-median draw — harness-level
question; (b) single-shot gating cannot resolve <~25% regressions on
echo.heavy ratio_p50 on dev-macos (candidates: gate on replicate median,
placement-robust pairing); (c) scroll.minimal factor rests on high-band quiet
draws max 2.438 — bar 2.4387 is knife-edge, a gate draw above it
is new evidence per protocol, not a regression; (d) the bare-key removal +
default-1.25 rationale itself. Campaign data: mbp
`~/bench-campaign-20260803/`, extract `~/.claude/tmp/campaign-extract.txt`.

**Re-review round (2026-08-03):** Fable re-review verified all 8 originals
fixed but returned CHANGES REQUIRED: 1 new blocker (my "trials agree <3%"
claim false — logs show 1.9-7.8% within-replicate; the honest contrast is
2-8% within vs 25% between), 2 warns (load claims overreached — the
high-load echo draws were the excluded ones and gate-5's 1.2513@load 1.1
is the decisive low-load anchor; "every excluded scroll replicate sits
lower" false for heavy r3 2.364 > included r2 2.362 — the load-bearing
fact is no excluded draw exceeds the included worst), 1 suggestion (run
more quiet scroll.minimal replicates BEFORE spending the 55-min gate).
All four fixed: both comment blocks + the shortfall why reworded to the
verified numbers; extension replicates r9-r12 run quiet (2.136@1.39,
2.349@2.42 excl, 2.380, 2.398): W stays 2.438, factor stays 1.024, and r9
falsified "contention compresses" for minimal too — two bands
(2.136-2.161 / 2.349-2.438), band membership load-independent, low band =
flattering scheduling luck, recorded 2.3816 is a high-band draw. Sidecar
scroll block rewritten accordingly (n=12 minimal provenance).
NEXT: Fable final verify, then hygiene check + validating gate, then commit.

**Gate attempt 6 (2026-08-03, post-approval):** EXIT:201 — NOT a TOML
defect: echo.heavy ratio_p50 drew 0.976 (low placement tier), inside the
1.1 budget, and `unreached_shortfalls` declared the shortfall entry STALE
and failed the run. Combined with gate-5's 1.2513-as-New, the wide cell
fails when it draws high AND when it draws low: the STALE check's
one-inside-draw-proves-spent premise is the same one-sample fallacy the
module's docs correct for accepted values. Calibration clean (1.0039 /
1.0073); two orphans leaked and were SIGKILLed post-run. HARNESS FIX
implemented: `provably_inside` mirrors `shortfall_ceiling` — stale only
if `headroom.bar(measured) <= budget.max`; no published spread → entry
stands (fails open, deletion stays human judgment).
`unreached_shortfalls` takes the headroom table; STALE message updated;
existing test moved to controlled-linux (view_p99_ms is a tail statistic
— uncontrolled classes never had a spread for it), new straddle +
no-spread tests added. task ci green. Fable review of the semantics
change IN FLIGHT (gate log mbp `~/bench-gate-20260803-validating.log`,
diff `~/.claude/tmp/stale-shortfall-fix.diff`). On approval: sync
budgets.rs+bench.rs to mbp, rebuild bench there, hygiene check, gate
attempt 7. Commit order at EXIT:0 now: (1) harness fix commit
(view-harness files), (2) three-file bench commit — the bench TOMLs
depend on the fixed gate semantics, so the harness commit goes FIRST.

**Skip-fix + gate attempt 7 (2026-08-03):** Fable APPROVED the stale-shortfall
fix on all 4 attack surfaces, with two non-blocking notes. Note 1 (a
platform-skipped cell yields zero findings, and the old all-quantifier filter
would report its entry stale) FIXED: `unreached_shortfalls` restructured to
visited/live semantics — stale iff at least one matching finding exists AND
none is live; zero matching findings = unvisited, never stale. New test
`a_shortfall_whose_cell_never_ran_is_not_reported_stale` (empty findings +
different-fixture findings both leave the entry standing); doc comment gained
the unvisited paragraph; bench.rs keeps the `cli.all` guard with the comment
reworded (full-sweep adjudication rationale, scope-safety no longer the
reason). task fmt + task ci EXIT:0, all three staleness tests pass by name.
Files re-synced to mbp with md5 equality (budgets.rs 919b85fd, baselines.rs
b38951aa, bench.rs 605b03cb). Skip-fix increment sent to the Fable reviewer
for confirmation (resumed agent a3da59943c66c1ec1). Gate attempt 7 launched
(mbp `~/bench-gate-20260803-attempt7.log`, rebuild + full `--all --gate
--class dev-macos`); pre-run hygiene clean (load 1.16, no orphans).
Reviewer CONFIRMED on all 3 surfaces (truth-table verify of the visited
capture, evidence-granularity match with find_shortfall, guard-as-policy).
Its one optional nit fixed: test renamed to
`a_shortfall_measured_provably_inside_is_reported_stale` (old
"no_longer_reaches...unreached" name collided with the new unvisited
semantics). Rename is #[cfg(test)]-only, done AFTER attempt 7 launched, so
the mbp gate binary is behavior-identical; budgets.rs re-synced to mbp
post-gate (md5 6c7f814f both sides).

**Gate attempt 7 result: EXIT:0.** gate OK, 12 cells within recorded bars,
15 metrics checked, 5 accepted shortfalls held (echo.minimal ratio_p50
1.104, echo.heavy view_p99_ms 9.807, first_paint.minimal marker_cold_ms
33.141, first_paint.heavy marker_cold_ms 83.242 + marker_ratio_p50 0.393).
Calibration 1.0036, end load 2.04. The fix's exact target replayed:
echo.heavy ratio_p50 drew 0.990 (same low placement tier as attempt 6's
0.976 that false-STALEd at EXIT:201) — zero STALE lines this time.
scroll heavy 2.384 / minimal 2.371, both inside their 1.024 bars.
Post-run sweep killed two PPID-1 `nvim --embed` orphans (the known leak,
already an adjudication carry); pgrep clean after. COMMITTED: `d6ac7af`
fix(harness) with exactly the 3 view-harness files, then `f4521ab`
perf(bench) with exactly the 3 bench TOMLs — `f4521ab` is the frozen tip
for #15-#21. Log: mbp `~/bench-gate-20260803-attempt7.log`. Note 2
becomes adjudication item (g) for #21: auto-staleness is dormant on
dev-macos under the compiled defaults — an entry is declared spent only
below measured 0.88 (ratio, 1.1/1.25), ~20ms-under (absolute p99, x1.5), so
in practice ledger cleanup on this class stays human judgment.

Four gate attempts, in true chronological order (mbp local time; the copies
under `~/.claude/tmp/` all share one scp mtime and cannot be ordered by it):

| run | host load | outcome |
|---|---|---|
| gate-1 01:52 | 1.58 | EXIT:0 — but under the *pre-tightening* sidecar |
| gate-2 02:52 | 4.37 | EXIT:201 — tap overhead p99 7.125/6.833 us over the 5 us pre-gate |
| gate-3 07:15 | 9.21 | EXIT:201 — `scroll.heavy ratio_p50` 2.2269 > bar 2.0040 |
| gate-4 | 3.68 | killed mid-run, no verdict |
| gate-5 2026-08-02 16:21 | 1.12 start / 1.04 end | EXIT:201 — `echo.heavy ratio_p50` 1.2513 > 1.0879; `scroll.heavy ratio_p50` 2.4075 > 2.3577; null-pair calibration 1.0000 |

A later attempt ran on past those, orphaned to `PPID=1`, and was killed at
1:05 elapsed. mbp was then verified to have zero surviving `bench`,
`nvim` or `bench-scratch` processes. Note that the bench binary is
`target/release/bench`, **not** `view-bench` — a `pkill -f view-bench`
matches only the scratch paths in its children and leaves the parent
running while it respawns them.

The sidecar was last edited at 10:00, after all three completed runs, so
gate-1's pass does not cover the current configuration. The sidecar
*tightens* against the compiled 1.25 default (host-wide `ratio_p50 = 1.02`,
`"scroll.ratio_p50" = 1.2`), so a looser run passing proves nothing about
the tighter one.

Recomputing the current bars against both completed runs' own
`gated ratio_p50` lines, every ratio cell clears — `echo.minimal` 1.115 vs
1.1348, `echo.heavy` 1.069 vs 1.0879, `scroll.minimal` 2.274 vs 2.8119,
`scroll.heavy` 2.227 vs 2.3577 — and gate-3's sole breach is closed. That is
arithmetic on prior runs, **not** an observed gate pass, and it does not
substitute for one. Note how thin the echo margins are (1.7% and 1.8%): the
1.02 host-wide key clears its own 2x-half-width rule (1.37%) by little, so
modest ambient drift flips echo red.

The tap-overhead pre-gate is the other load-sensitive failure mode: it
tripped at load 4.37 and was comfortably clear (1.1-1.4 us) at load 1.58.
Run the validating gate on a genuinely quiet mbp — historically that means
overnight local time — not on a mid-day host.

Two open items the #6 campaign surfaced:

- The bare `ratio_p50 = 1.02` key was characterized from **echo replicates
  alone** yet applies host-wide, and that scope is what put `scroll.heavy`
  over its bar in gate-3. `"scroll.ratio_p50" = 1.2` covers it, but
  `scroll.minimal` inherits that factor without its own campaign
  (`headroom_for` in `baselines.rs` looks up the bare scenario name).
  Whether an echo-derived factor should govern uncharacterized scenarios is
  adjudication material, not something to settle silently.
- **Bench process leak:** orphaned `PPID=1` `nvim --embed` children survive
  under load and required an explicit pkill-and-verify before every
  replicate. This is a live harness defect, unfixed.

### 3.3 mbp operational traps (each has cost hours)

- Invoke as `ssh mbp 'zsh -lc "..."'` and backslash-escape remote variables
  (`\$HOME`, `\$PATH`), or the outer shell expands them and bakes a minimal
  PATH where `nvim` and `task` vanish.
- Stale note removed 2026-08-02: homebrew git on mbp works (`git 2.55.0`
  at `/opt/homebrew/bin/git`, verified via `zsh -lc`); no `bin-gitfix`
  PATH prefix is needed.
- Never leave an unquoted `===` inside `zsh -lc`: equals-expansion aborts
  the whole line (`zsh:1: == not found`) and everything after it silently
  never runs.
- Never `tail -n0 -f` to wait on a log — it attaches at EOF and misses a
  line already written. Race-free:
  `grep -m1 '^EXIT:' LOG || timeout <N>s tail -c +1 -f LOG | grep -m1 '^EXIT:'`
- A Parallels "Windows 11" VM can auto-start and contaminate a campaign;
  check `prlctl list` before and after, and record `uptime` load ranges in
  the provenance comments.
- A full `--all --gate` run is 55-60 minutes (`first_paint` alone does 1000
  cold nvim spawns; that self-induced load is normal, not contamination).
- The repo is at `~/repos/view`, detached at `7a9f2e9` — advance it to the
  final tip before any post-#6 mbp run.

### 3.4 Adjudication list for #21

No other file carries this list; it is the reason #21 runs last.

1. **Echo escalation door.** dev-linux echo `ratio_p50` 1.172/1.181 against
   the ≤1.10 contract, plus the new dev-macos `echo.minimal` shortfall at
   1.1125. Attribution is settled (see section 5.7): `echo_control` shows
   out-of-process costs ~1-2%, view's own path ~22%, so the
   protocol-inherent hypothesis is refuted and there is no permanent
   limitation to publish.
2. Deferrals 1-4 re-confirmation (the plan's exit checklist).
3. noice residual (folke/noice.nvim#1137).
4. `first_paint.minimal` `marker_cold_ms` budget 30 -> 50.302 — a
   strictness change, already settled and not to be relitigated by an
   implementer, but the user has not ruled on it.
5. flood contended-host residual.
6. Tracked `.claude/settings.json` auto-executing hooks — the one unchecked
   `known-bugs.md` item.
7. Go-ahead for the classifier-blocked `known-bugs.md` repair script.
8. daily-config E216 residual (cfgd-config `hijack_netrw`).
9. Resolved 2026-08-02: mbp homebrew git verified working (`git 2.55.0`,
   `/opt/homebrew/bin/git`) — the "broken, needs bin-gitfix shim" note was
   stale; nothing to adjudicate.
10. **Shortfall-ledger ratification.** Every `[[shortfall]]` in
    `crates/view-bench/budgets.toml` (six dev-linux + four dev-macos once #6
    commits) was accepted by the assistant with engineering rationale; none
    carries user sign-off. Present the complete table — scenario, bar,
    accepted value, why — for a single up-or-down ratification. Same pass
    ratifies the spec §3.1 echo-row amendment, which the spec itself flags
    "This amendment has NO user sign-off; it is provisional." (The
    input-path 232µs self-referential bar was already withdrawn and the
    original 100µs bar restored and met at 88.965µs; the first-paint
    30ms/0.30 bar is user-stated 2026-07-27 — those two need nothing.)

Carried from #6 (see 3.2): the host-wide `ratio_p50 = 1.02` headroom key is
echo-derived but governs scenarios whose spread was never measured, and the
bench process leak (orphaned `PPID=1` `nvim --embed` children). Also carried
from #6: items (a)-(d) in the campaign paragraph of 3.2, and (g) dormant
auto-staleness on dev-macos under compiled defaults (see the skip-fix
paragraph of 3.2).

Separately, not a view matter: the sir/proxmox storage-incident follow-ups
(retention does not fit the pool, backup-script defects, the next run does
~1T of FULL sends, the health check alerted nobody, host `bud` still down,
sir RAM tight).

### 3.5 Constraints sessions lose first

- `--record` rewrites `<class>.toml` from scratch but never the
  `.headroom.toml` sidecar. Shortfall shape: `struct Shortfall` in
  `crates/view-harness/src/budgets.rs`. Outside-budget with no entry is
  `Verdict::New` and fails; widening an existing shortfall fails; honest
  entries carry `accepted` and `why`. Never widen a shortfall — or a
  headroom entry — to clear a loaded-host excursion.
- Subagents run gates in the FOREGROUND with 10-15 minute timeouts, output
  to a file on the first run, read selectively. Background-child
  notifications do not resume a subagent whose turn has ended.
- **`SendMessage` to a finished agent RESUMES it.** There is no "stop" you
  can send: a message telling an agent to stand down restarts it, and it
  then spends a fresh turn deciding what to do about the instruction. A
  long-running subagent cost most of a day this way, re-ignited twice by
  messages whose entire content was an order to stop. To actually end one,
  use the harness kill, and kill the work it is blocked on (see 3.3) —
  never a message.
- Gate logs copied off a remote host all share one scp mtime. They cannot
  be ordered by it, and reading them as run times inverts the true order.
  Get run times from the remote host, or from a timestamp inside the log.
- No polling loops anywhere; they are hook-denied. A denial means switch
  mechanism, never reword.
- Name the model explicitly on every subagent dispatch (an omitted model
  silently inherits the session model). Model policy (user, 2026-08-02):
  Fable reviewers are fine where the review gates a concession or a phase
  exit; implementers default opus 5; sonnet 5 / opus 4.8 for mechanical
  work; if a fix loop reaches round 3, escalate the model rather than
  re-running the same tier — 5+ cheap rounds cost more than one strong one.
- Comments are WHY-only. Banned: session-narrative markers, assistant
  references, finding tags. Fix on sight, before committing.
- The bench/oracle spawn allowlist keeps neither `SSL_CERT_FILE` /
  `SSL_CERT_DIR` nor any proxy variable. On a host behind a proxy or using
  a non-default CA bundle, the cold-bootstrap clone therefore fails loudly
  rather than measuring something wrong quietly — that is the intended
  trade, so read such a failure as environment, not as a regression.

**The publisher-set question moved to P6 and is no longer an open task.**
The user decided 2026-07-26 that the pre-P4 push cuts no release: its only
purpose is the two items in `.claude/pending-first-push.md` (the CI
status-badge slug, and GHA workflow verification on real runners including
the bench-baseline recording flow). Neither needs anodizer, and spec §16
already fixes anodizer's arrival at P6. What P6 inherits: the standard is
match-or-exceed any publisher comparative tooling offers; the crates.io
closure for the `view` binary is view-core, view-surface, view-engine,
view-tui, view (five today, seven once view-native and view-ai join at
P4/P5), while view-oracle, view-bench and view-harness are tooling and never
publish; and one open sub-question the user is open to pushback on — whether
per-language ports (the neovim ruby/python package pattern) belong in this
repo at all.

**Everything else that stood here is done.** 62, 60, 61, 58, 49, 50, 51, 59
and 52 all closed, each with its evidence in `.claude/measurements/` and its
`known-bugs.md` entry flipped. Two of them changed a spec budget rather than
the code, and both amendments are written into §3.1 with the measurement that
forced them:

- **49 (first_paint)** was not a view defect at all: the recorded 54.7 ms was
  the capability probe waiting out its 50 ms fallback against a bench pty that
  never answered the DA1 fence. With the pty answering like a real terminal,
  the number fell to 3.6 ms.
  **Corrected 2026-07-26 (task 22):** that 3.6 ms was never "view's cold
  path". It was view's *own shell frame*, timed under an "any cell has ink"
  boundary and then divided by bare nvim's *buffer window* to produce a
  `ratio_vs_nvim` comparing two different events. The row now measures both
  separately: `shell_visible_ms` (view's chrome, unpaired, the ≤50 ms budget's
  real subject) and `marker_cold_ms` / `marker_ratio_p50` / `marker_ratio_p99`
  (the opened file on screen, paired). §3.1 carries the amendment.
- **50 (input_path)** was two defects, neither of them the 100 µs. The gated
  boundary opened at the harness's write to the pty master, so the biggest
  segment of it was the OS pty transport; and the tap-overhead gate that
  decides whether the row may run was comparing its bar against writes that
  never landed (39398 of 100000). Both fixed; the budget is now per class off
  the measured floor.

**Task 57 (attribute the echo gap) is COMPLETE.** Its findings matter to
several of the above, and the durable ones are in section 5.7 below rather
than here.

**`known-bugs.md` has exactly one unchecked item** as of 2026-08-02: the
tracked `.claude/settings.json` auto-executes machine-local hooks on any
checkout, a supply-chain and portability exposure that needs a decision
before the first public push. It is item 6 of the adjudication list in 3.4,
which is its approval path.

The file *reads* as roughly seven open entries, but the rest are orphaned
body fragments whose `- [x]` headers were archived into
`.claude/archive/known-bugs-2026-08.md` — the archive's "No god-file audit"
entry visibly ends mid-sentence, which is where the split happened. A repair
script exists in a session scratchpad but is classifier-blocked and needs
explicit user go-ahead (item 7 of 3.4). Do not hand-edit that file while
another session holds it uncommitted.

It must carry only explicit user-approved deferrals before declaring P3 done.
The gate is at the "I'm finished" moment, not at push. The message-toast
direction that stood here as an open user decision was decided and shipped:
see `known-bugs.md` and
`.claude/measurements/2026-07-26-msg-show-kind-lifetimes.md`.

**Coverage-walk deferrals 1-4 were re-confirmed by the user on 2026-07-26**
and carry forward unchanged: picker rows gate at P4, three-state compat
assertions land at P4, compat-page hosting wires up with P6's docs
deliverable, and the Windows oracle/compat/bench legs are P6 exit criteria.
Exit-checklist item 9 is closed on that approval.

The echo-ratio escalation door (checklist item 5) is still open, but it needs
less from the user than previously recorded. The user's standing rule as of
2026-07-26: **no concession or metric degradation is accepted without an
adversarial review by a Fable 5 subagent**, which also watches for safety and
security concerns as it reads. Every amendment in section 5.6's ledger is
subject to that gate retroactively. The attribution itself is a measurement,
not a judgment call, and it must run before any further amendment is proposed.

**The attribution ran and is settled (2026-07-26).** Two earlier control
designs are recorded here because both were wrong and both cost time.

The *null-frontend* control (attach over RPC with the identical ext-option
set, drain the grid, paint nothing) cannot work at all: a frontend that
paints nothing produces no pty output, so the harness boundary (the first
vt100 frame where the target cell differs) never fires and the control has
no measurable event. Do not implement it.

The replacement, `nvim --remote-ui` as an external RPC client containing
none of our code, does work and has run. It is the permanent `echo_control`
matrix row (`crates/view-bench/src/scenarios/echo_control.rs`), and it
**refuted** the protocol-inherent hypothesis rather than confirming it:
nvim's own remote UI costs 1.015 (minimal) and 1.013 (heavy) against bare
nvim on dev-linux, where view costs 1.354 and 1.244. Being out of process
costs ~2%; view costs ~22%. There is no permanent limitation to publish.

The second arm, a passthrough frontend painting at the flush boundary, is
**not needed and should not be built**: the `echo_path` tapped
decomposition already separates view's compositing from everything else,
and it resolved every one of 3000 samples with a 0.2% residual on the
minimal fixture. Building a second frontend would re-derive what the taps
already measure. The attribution it produced is task #26.

---

## 4. How we have been working

**Mode: subagent-driven development (SDD), continuously, without check-ins.**
The user expects autonomous execution across a whole task list. Stopping to ask
"shall I continue?" wastes their time. Stop only for a genuine architectural
fork or a decision needing authority only they have.

Per task, the loop is:

```
task-brief script  ->  fresh implementer (Opus 5)
                   ->  review-package script
                   ->  adversarial reviewer (Opus 5, FRESH context)
                   ->  ONE fixer with the COMPLETE findings list
                   ->  re-review
                   ->  ledger line in .superpowers/sdd/progress.md
```

Scripts live under
`/root/.claude/plugins/cache/claude-plugins-official/superpowers/6.1.1/skills/subagent-driven-development/scripts/`.

**Model selection as actually practised here:** Opus 5 for implementers AND
reviewers, essentially always. The published SDD guidance suggests cheaper
models for mechanical tasks; on this codebase that has been the wrong trade —
the work is subtle enough that weaker models take 2-3x the turns and cost more
overall, and the review findings that mattered most came from reasoning depth.
Fable 5 only as a deliberate pick. Always name the model explicitly; omitting
it silently inherits.

**One fixer per review round, never one fixer per finding.** Per-finding fixers
each rebuild context and re-run suites; that pattern once cost more than every
task in the phase combined.

**Fresh task list per phase.** Phase tasks plus the follow-ons that phase
generated. Do not carry a stale list forward.

**While subagents run, keep your own operations refs-only** (`git log`,
`git status`, reads). Do not edit files a running agent is working in.

**Adversarial review means fresh context, always.** Self-review is biased and
the user has said so explicitly. This applies to specs as much as to code.

---

## 5. The judgment that is not in any file

### 5.1 A relayed correction is not evidence

**Three of my own instructions to fixers were wrong this phase.** All three
were refused, with argument, by the subagent. All three were confirmed wrong
when I re-derived them. Applying any of them would have written a falsehood
into the tree.

1. Told a fixer `nvim_get_mode` is not a fast call, sourced from `fast=nil` in
   API metadata. Wrong: the pinned `api.txt` `*api-fast*` section names it, and
   `vim.fn.api_info()` reports 261 functions of which **zero** carry a `fast`
   key — so `fast=nil` is an absent field, not a negative answer.
2. Told a fixer that `set_var` racing `Command::spawn` is a data race. Wrong on
   this toolchain: std's own `ENV_LOCK` already mutually excludes `set_var`,
   `vars_os`, and the environment copy inside `Command::spawn`. The fixer ran
   three reproducers across 20 runs, found no fault, declined to claim an
   observed race, and justified the fix on what it could actually prove.
3. Told a fixer a repaint pessimism could be narrowed. Wrong: a cell painted
   against a never-defined highlight id resolves through Normal, so the FIRST
   definition restyles it. The pessimism was correctly kept.

**Carry this forward:** when you are about to instruct a fixer to rewrite a
statement that is currently TRUE, re-derive it from primary evidence first. And
when a subagent pushes back with an argument, that is the process working —
read the argument, do not force compliance. Those three refusals protected the
tree.

Related, five times this phase: **stale compiler diagnostics contradicted
subagent reports.** Before believing "X is broken," run
`cargo build --workspace --all-targets` yourself.

### 5.2 The defect class that matters most here: right code, wrong moat

The highest-value findings in P3 were never bugs in the code. They were tests
that claimed to hold an invariant and did not:

- Dropping `XDG_DATA_DIRS` from the hermetic list — full suite green.
- Flipping the editor's own spawn from `default()` to `isolated()` — full suite
  green at 539 passing.
- A rustdoc claiming an invariant was "pinned by test" when nothing pinned it.

**The detection method is sabotage.** Break the thing on purpose; the suite must
go red. If it stays green, the moat is decorative. Apply this to every claim of
the form "this is covered." It is now the standard review technique on this
project, and reviewers should be told to use it.

A fixer this phase caught its own test failing this check — it wrote a
combined test, ran the sabotage, watched the test PASS anyway, and split it in
two. That is the standard.

### 5.3 What green does NOT prove — structural coverage boundaries

Each of these was learned by a real bug slipping through. A new session that
does not know them will trust a green suite it should not:

| Moat | What it structurally CANNOT see |
|---|---|
| Differential oracle (parity) | Compares MODEL-level state. Paint-output bugs are invisible to it by construction |
| `assert_clip_matches_full` | Compares clipped vs full over the SAME model state — a dropped state mutation is invisible BY CONSTRUCTION. **Clip moats are not state moats**; state assertions must live in view-core |
| `--embed` tests without a UI attach | Never source `plugin/` scripts at all, so they cannot demonstrate a search-path leak |
| Harness and oracle | Neither links view-tui. The paint property tests are the ENTIRE moat for paint output |
| `corpus/wide-graphemes` | Model/raster wide-glyph handling only. Blanking every multi-column glyph in the painter leaves it at PARITY |

When you extend a moat, write its boundary into the artifact itself, next to
the thing it guards. Several of these are already documented that way; keep the
habit.

### 5.4 Measurement discipline — the specific numbers

Two noise models, distinct, and conflating them is an error I made and had to
retract:

- **~3%** is the codegen-layout floor **across separately-compiled binaries**
  (the default profile is 16 codegen units). Any cross-binary delta under this
  is layout, not work.
- **Trial-to-trial variance on the SAME binary** is a separate and larger
  figure — one row spread ~5% across its own three trials.
- Below either threshold, the reliable method is a **single-binary
  atomic-dispatch control**.

Also binding:

- **Record host load and the null-pair calibration with every number.** A
  figure of 1.576 got written into the spec and had to be retracted purely
  because nobody wrote down that the host was at load ~1.8-2.0.
- The null-pair calibration guards the run's **start** only. Mid-run bursts are
  a known live exposure (task 51).
- **Measure before optimizing.** In one task, measurement found 75% of the cost
  sat inside a dependency's own equality implementation — a type view does not
  own — so the obvious optimization route did not exist. Measure-first turned a
  wasted task into a 91% win on the right thing.
- A bench delta that "improves" is not automatically re-recorded. Until task 60
  lands there is **no mechanism** stopping `--record` from installing a worse
  bar. Treat re-recording as a deliberate, justified act.

### 5.5 Architectural instinct: make the wrong state unrepresentable

When a highlight-table mutation was found producing no paint damage, the fix
deliberately rejected the "patch the three call sites" shape in favour of
**self-reporting sources**: fields private behind accessors, every mutator marks
dirty, one fold that cannot be bypassed or swallowed. That construction
immediately caught a fourth mutation site the review had not listed.

This is the preference to carry: a validator is an apology for an incoherence
the types allowed. Prefer the shape where forgetting is impossible over the
shape that relies on the next author remembering.

### 5.6 When a spec target turns out to be unreachable

There is a precedent and it is NOT silent de-scoping. The sequence used:

1. Prove unreachability with numbers, including what the floor actually is.
2. Present the evidence and the options to the user.
3. The user amends the spec (in that case, per-class).

Scope reduction is the user's call alone. "Open question / decide whether to
add X" is a trigger to research and pitch in 3-5 lines, never permission to
decline. Never write "out of scope," "not worth it," or "skipped" about
anything the user named.

And note the sequel: that same escalation produced a conclusion — a thread-hop
cost floor — that a later bare-metal measurement **falsified**. Even a
carefully escalated, fully reviewed conclusion can be wrong. The spec now
records it as falsified. Do not resurrect the 1.19 hop floor or the 1.576 mbp
figure; both are withdrawn.

### 5.7 What the echo-gap attribution established

That row produced three wrong answers before a right one. The durable outputs:

**Bare nvim v0.12.4 is itself a two-process msgpack-RPC architecture** — a UI
client and an editor server exchanging `nvim_input` and `redraw` over **two
unidirectional pipes** (not a socketpair; `/proc` shows two distinct `pipe:`
inodes, one per direction), one frame at a time, even under `-u NONE`.
Reproduced independently on both hosts. **This reframes the product's central
comparison: view is not paying an `--embed` tax that nvim avoids.** Any future
reasoning that treats the RPC seam as view's unique overhead is starting from a
false premise.

**The inversion everyone chased is p50-only.** dev-macos `ratio_p99` is 0.943 —
view is *already better* than bare nvim at the tail on bare metal. And
`first_paint` already carries the "faster than nvim" claim at ~3x (52 ms vs
158 ms linux, 66 ms vs 215 ms mbp). Whether `echo` is the right hill for that
product claim is a live spec-priority question for the user, raised and not yet
answered.

Three measurement lessons, each paid for with a wrong conclusion:

- **A reconciliation that closes by construction is not evidence.** The
  attribution's residual identity held for *any* assignment of stages. State
  which of your checks *can* fail.
- **A single-run delta is not a result.** Report every delta beside the
  replicate spread of the *same* measurement. Two identical-source baseline runs
  differed by more than the effect being claimed.
- **Have the reviewer pre-register the passing condition.** The one claim that
  survived did so because an adversarial party named the success criterion
  before the measurement existed. Make that the default for contested numbers.

Two process lessons:

- **Superseded claims need markers in the *report*, not just the deliverable.**
  A killed claim reached the spec once because the summary a coordinator opens
  first still asserted it. Mark every assertion site, leave the original wording
  visible, name what replaced it and where.
- **Over-hedging is its own inaccuracy.** A reviewer had to correct an
  *apologetic* framing: the load figures actually ruled contention out. State
  what the evidence supports, in both directions.

**A fourth measurement lesson, added 2026-07-27.** *"It is a wake, and the
rules mandate a wake"* was treated as *"the cost is fixed"*, and that closed
the attribution one step early. It is wrong twice over: a hop's price is set
by how deeply the receiving core has parked (7.5 us at 50 us idle, 36 us at
10 ms, same primitive), and *how many hops the path takes* is a design
variable, not a rule. Sizing the primitive directly with a microbench is what
reopened it, and that is now `cargo bench -p view-core --bench input_handoff`,
wired into `task bench-micro` so it cannot rot. **When a cost is attributed to
something mandated, measure the mandated thing in isolation before accepting
the attribution.**

### 5.8 PITCH, needs the user's call: unify the input thread and the runtime loop

This is the last lever on typing, and it is architectural, so it is a pitch
rather than a task already in flight.

**Where the remaining gap is.** view's own share of a keystroke round trip is
139 us p50 (71 input, 68 paint). The largest single item is
`key-decoded->loop-wake` at **49.1 us p50 / 91.0 p99** -- the hop from the
thread that blocks on `crossterm::event::read()` to the runtime loop that owns
the model. It is expensive because a human pauses between keystrokes, so the
loop is always deeply parked when the next key lands. Nothing else on either
path exceeds 21 us p50.

**Why nvim's own remote UI does not pay it.** That client reads its pty and
writes its socket *on one thread*, and measures 1.015 against bare nvim where
view measures 1.172. The structural difference is the hop, not the protocol
(that was already falsified).

**The change.** Replace the blocking-read input thread with one loop that
polls the terminal fd and the engine's stdout together. Both hard rules
survive: the loop still never *awaits* RPC (a readiness poll is not an await),
and only view-tui still touches the terminal. Expected: ~49 us off 139, which
would put `echo.minimal` near 1.10 -- the spec bar -- and `input_path` near the
restored 100 us bar.

**The cost, honestly.** It rewrites the runtime loop's driver, which is the
most load-bearing code in the project, for a win that is real but bounded. It
is P4-scale work, not a P3 patch, and the P4 plan does not currently contain
it.

**The second lever, on the paint side, is the same shape.** view's 68 us of
paint path is not instruction count -- it is cache residency. The identical
frame costs 2.94 us back to back and **21.27 us with a 10 ms keystroke gap
before it**, a 7.2x difference from idle alone
(`.claude/measurements/2026-07-27-the-paint-path-is-cold-cache-not-instructions.md`).
Cost is therefore proportional to memory touched per frame, and
`view_surface::render` builds a fresh full-screen `Surface` every frame even
when one cell changed. Trading that for incremental rendering would cut it,
and it is a deliberate property of the Elm-style runtime, not an oversight.

**The call the user has to make:** do either of these go into P4's scope --
unifying the input thread with the runtime loop, incremental rendering, or
both -- or does the project accept ~1.17x typing through P4 and revisit
later? All are defensible; the evidence sizes them but does not decide it.

**A caveat that outlives this pitch, now resolved (2026-08-01,
`c215405`+`fc8186c`):** every criterion micro-bench carries a hot/cold
pair — paint_frame, render_frame, damage_fold (2.48x residency factor),
grid_apply, update_key — hot as the relative instrument, cold (10ms idle
gap, only the work timed) as the absolute cost a keystroke pays.
input_handoff is differently-shaped evidence: not a criterion bench, it
sweeps idle gaps 50µs–10ms directly, and its published factor compares a
50µs proxy row to the 10ms row, not a true 0-gap hot. Numbers:
`.superpowers/sdd/task10-cold-bench-report.md`. Quote cold numbers as
absolute costs, hot only for deltas.

---

## 6. Operational gotchas learned the hard way

| Gotcha | Rule |
|---|---|
| A backgrounded gate ending in `tail` reports `exit 0` while the gate failed | End every backgrounded gate with the gate's own exit code as the last thing written: `echo "EXIT=$?" \| tee -a` |
| rsync of working files to mbp leaves git metadata stale | A subagent reporting mbp "clean" is not evidence. Verify git state on the host directly |
| Subagents that background a gate then end their turn simply stall | Background-child notifications go to the COORDINATOR, not an ended subagent. Subagents run gates FOREGROUND with a 10-15 min timeout. This is in `.claude/CLAUDE.md` — keep telling them anyway |
| `cargo test -p a -p b` fail-fasts across packages | Use `--no-fail-fast` |
| `task commit` runs `git add -A` | Tree must be stray-free before committing |
| Scratch in `/tmp` is hook-blocked | tmpfs is RAM-backed and OOMs cargo's linker. Use `~/.claude/tmp/`; on mbp `~/view-bench-tools/scratch/` |
| Line numbers in `.claude/**/*.md` are hook-blocked | Use grep targets instead |
| Poll loops and `git reset --hard` are hook-denied | A denial is the signal to switch approach, not to reword |
| Re-running a slow command for a different view of its output | Redirect to a file on the FIRST run, then read selectively |

Hosts: `ssh mbp` (macOS M1 Max, bare metal, `export PATH="/opt/homebrew/bin:$PATH"`,
engine pin side-loaded under `~/view-bench-tools/`, clone at `~/repos/view`);
`ssh winserver` (Windows, for tier-2 evidence).

---

## 7. The vision — what this tool has to be

This is the part that must not drift.

**view is not a Neovim clone and not a Neovim wrapper.** It embeds a pinned
Neovim as its engine, and that choice is the entire strategic bet: it buys
**total plugin compatibility by construction** — the moat no reimplementation
can cross, because they must chase an ecosystem forever and view simply hosts
it. Everything in the architecture protects that: nvim owns all buffer text, no
view subsystem holds authoritative text state, mutation only through RPC.

**The hard half, and the reason the project is worth doing:** view must be
**measurably faster than the thing it embeds**. Hosting nvim and being slower
than nvim is a product with no reason to exist. That is why the bench harness,
the paired measurement, the recorded baselines, and the gates exist, and why
perf work gets the same rigor as correctness work. The echo row being above 1.0
is not a cosmetic blemish; it is the central open question of the product.

Beyond that: native features win by default, AI arrives via ACP, and releases
go out through anodizer.

**The bar for "done" is evidence, not shipping.** "Tests pass / CI green /
released" is the START of evidence, never proof. Prefer deferring a ship to
obtain more proof across more permutations. Local validation that structurally
cannot catch a broken release is itself a defect to fix, not to work around.
Reviews are evidence audits — which permutations are proven, by which test, and
which remain untested — never skims.

**And the cardinal rule:** never report an action, verification, or result you
did not actually perform and observe. "Verified" without having run it this
session and seen the output is fabrication, and strictly worse than admitting
the work is unfinished. Cite the command and its real output, or say plainly "I
have not verified this."

---

## 8. Cadence for the phases after P3

`.claude/plans/INDEX.md` holds the phase table and the exit policy. The part
that is not written down:

**One phase at a time, specced when its predecessor completes, against the real
interfaces that predecessor produced.** Writing P4's plan today would mean
inventing signatures P3 has not produced. The charters exist
(`2026-07-18-p3-p6-charters.md`); the full plan does not, deliberately.

The cadence per phase:

```
charter  ->  full plan written against real interfaces
         ->  adversarial fresh-context review (P2 took 3 rounds, P3 took 2)
         ->  expand on every finding in excruciating detail
         ->  user approves
         ->  SDD execution on a new dev/<phase> branch, fresh task list
         ->  phase exit: task ci clean + spec budgets measured
                       + known-bugs drained or user-deferred
                       + dogfood note appended
```

Quality, precision, and accuracy over speed — at every one of those arrows.
Multiple review rounds on a plan is the expected outcome, not a failure.

Next up after the section 3 tasks: P4 (native features + theming), which starts
by writing its full plan from the charter and putting it through adversarial
review before a line of code.

---

## 9. Retiring this file

**Trigger: P3 exit (task 53). Do this as part of that task, not later.**

This file mixes two lifetimes and only one of them is perishable. Do not delete
it wholesale — that would throw away the expensive half. Split it:

| Section | Lifetime | Action at P3 exit |
|---|---|---|
| 1-3 startup, state, task list | Stale the moment work resumes | **Delete and rewrite** as a fresh P4 snapshot |
| 4 SDD working mode | Durable | Promote into `.claude/CLAUDE.md` |
| 5 judgment, defect classes, coverage boundaries, measurement discipline | Durable, most expensive to relearn | Promote into a new `.claude/rules/discipline.md`, beside `rules/rust.md` |
| 6 operational gotchas | Durable | Same rules file |
| 7 vision | Durable | Promote into `.claude/CLAUDE.md`, above the hard rules |
| 8 phase cadence | Durable | Promote into `.claude/plans/INDEX.md`, which already holds the exit policy |

The `START HERE` pointer in `.claude/CLAUDE.md` stays permanently. It points at
a file whose sections 1-3 are rewritten at every phase transition.

The steady-state pattern, once the promotions above have happened: **the
handoff is a per-phase snapshot, and each phase promotes what it learned into
the durable rules before opening the next one.** Section 5 is the part that
compounds — every entry in it was paid for with a real bug or a wrong
conclusion, so it should grow at each phase boundary, never be summarized down.

Nothing enforces this. It is a judgment call at the phase boundary, and the
only thing carrying it is this section plus the checklist in task 53.
