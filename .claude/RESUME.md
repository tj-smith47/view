# RESUME — start a fresh session here (written 2026-08-01, by Fable, mid-session stop)

This file is the honest, current carrier. It supersedes `.claude/IN-FLIGHT.md`
and section 3 of `.claude/HANDOFF.md` where they disagree. Read this, then
`.claude/CLAUDE.md`, then `.claude/HANDOFF.md` sections 4–9 (durable judgment).

**Trust nothing here you can re-derive from `git log` — verify.** This session
existed to *land stranded finished work* and stop a re-derivation loop.

---

## 1. What is TRUE about the tree right now

- Branch: **`dev/p3-oracle-bench`**. Integration branch is **`master`** (NOT
  `main` — the harness header lies). Nothing has ever been pushed; that is the
  only one-way door. Push needs an explicit ship instruction.
- Tip: **`bcac248`**. Working tree is **clean**.
- Commit only via `task commit -- -m "<msg>"` (plain `git commit` hook-blocked;
  it runs `git add -A`, so keep the tree stray-free).

### VERIFICATION GAP — do this first
`task ci` was last observed green (exit 0, **724 tests**) at commit `a7d40c0`,
BEFORE the last five commits were cherry-picked onto the branch. Those five
(`5e5e013` F8, and `200c28b`/`ec682d4`/`7b4ef61`/`bcac248` gate portability)
each passed `task ci` **in their own worktree** but the **integrated branch tip
has not been re-verified this session**. First action in the new session:

```
task ci > ~/.claude/tmp/ci-verify.log 2>&1; echo "EXIT=$?"
```

Only after that reads EXIT=0 is `bcac248` trustworthy.

---

## 2. What landed this session (was finished-but-stranded in worktrees)

The loop's real failure was reviewed work never reaching the branch. Landed:

| Commit | What | Was stranded in |
|---|---|---|
| `d378931` `c94f37c` `a7d40c0` | Bench gate transposition rounds 4–6: named-field `CellId`, baseline-lookup and budget-lookup could be called backwards | worktree a455af27 |
| `5e5e013` | **F8**: `paired_summary(view, nvim)` was swappable — a transposed call inverts the core gated ratio. Closed by construction with per-side newtypes `ViewSamples`/`NvimSamples`; transposition now fails to compile | worktree afe3ed63 |
| `200c28b` | Four gate scripts read the host's userland not the tree (bash-3.2/BSD portability) | worktree a2fd9219 |
| `ec682d4` | audit-god-files census skipped an unnamable file; guard refused its own doc sentence | " |
| `7b4ef61` | **collation hole**: `case *[!A-Za-z0-9…]*` uses a bracket RANGE resolved by locale; macOS bash 3.2 folds accents into a-z, so the refusal ACCEPTED the path it exists to reject. Fixed with an enumerated class | " |
| `bcac248` | **push-guard regression fix** + a 61-case matrix committed as a runnable test at `.claude/hooks/tests/validate-commands-cases.sh`. Green on Linux AND macOS bash 3.2, sabotage-verified | " |

The push-guard story, for the record (this is what a safety classifier keeps
false-flagging — it is DEFENSIVE, hardening our own guard):
- An earlier narrowing of `.claude/hooks/validate-commands.sh` (the `-m`/
  `--message` span strip) had a leading boundary class `[^[:alnum:]_-]` that
  matched `=`, so an attached config token like `-c a.b=-m` read its tail as a
  flag and deleted the real subcommand behind it — letting a crafted invocation
  reach `allow`. Verified against real git. Fixed: boundary → `[[:space:]]`, and
  the unquoted value class now stops at `;`/`&`/`|` (a second evasion found
  while fixing). Both directions of the old F5 false-positive stay closed.

---

## 3. OPEN, decided but not yet done (no design question remains)

1. **Gate the push-guard test into ci.** The 61-case matrix at
   `.claude/hooks/tests/validate-commands-cases.sh` is committed but NOT run by
   `task ci` — a decorative moat. The one-line fix (already staged-and-reverted
   this session because it was unverified): add to the `audit` task in
   `Taskfile.yml`:
   ```yaml
   - bash .claude/hooks/tests/validate-commands-cases.sh
   ```
   Then `task ci` to verify, then `task commit`.

2. **The 6 remaining swap-inversion sites (same class as F8).** The F8 fixer's
   sweep found six more transposable view/nvim pairs, all `&SpawnSpec`/`&Path`:
   the five scenario entry points (`echo::run`, `scroll::run`,
   `first_paint::run`, `taps::run_echo_path`, `echo_control::run`) and
   **`crates/view-harness/src/bin/oracle.rs::run_scenario(view_bin, nvim_bin)`**
   — the last one is load-bearing: transposing it inverts the differential
   oracle's reference side, the project's core moat. Close by construction
   (per-side types threaded through the five entry points + oracle + call
   sites), as ONE round. This is the LAST member of the transposition class;
   after it the class is closed and the task is genuinely done. Do NOT spawn six
   rounds — it is one coherent change.

3. **Task 24 fix-round re-review — DONE, verdict CHANGES REQUIRED (1 Critical,
   1 Important, 4 Minor).** Full review: `.superpowers/sdd/task24-fixround-rereview.md`.
   The code it reviews (`efb594d`, `d32daf0`) is already ON the branch. The
   Critical is a REAL, LIVE-PROVEN security finding, not a false positive — do
   NOT drop it:
   - **C1 (Critical, credential leak):** F1 closed only git's config-*file*
     channels. Under the fix's exact overrides, git's http transport still reads
     `$HOME/.netrc` and sends the operator's credentials — proven live against a
     401 server, git 2.53, `Basic netrcuser:netrcpass` observed. `~/.ssh/*` is
     the same construction for ssh remotes. The `HOST_SUBPROCESS_CONFIG_VARS`
     doc and the two docs commits OVERCLAIM "read nothing through HOME."
     Sites: grep `HOST_SUBPROCESS_CONFIG_VARS` in `crates/view-engine/src/env.rs`
     and the hermetic-env builder in `crates/view-oracle/src/pty.rs` (grep
     `GIT_CONFIG_GLOBAL`). Fix direction: the allowlist keeps
     `HOME`, so neutralize the credential channels too (point `HOME` at the
     hardened empty dir for the subprocess layer, or set `GIT_TERMINAL_PROMPT=0`
     + divert `$HOME/.netrc`/`$HOME/.ssh` explicitly) AND correct the docs to
     claim only what is true. Staff-architect call: close the channel, do not
     just soften the doc.
   - **I1 (Important):** deleting `GIT_CONFIG_SYSTEM` from
     `HOST_SUBPROCESS_CONFIG_VARS` turns no test red — the tests iterate the
     const itself and the hostile-home oracle only exercises the global layer.
     Add a system-layer sabotage test. Tests in
     `crates/view-engine/src/process.rs` (grep the env-plan test module) and
     the hostile-home oracle in `crates/view-oracle/src/pty.rs`.
   - **Minors:** `$HOME/.config/git/ignore` still read (core.excludesFile
     default not diverted by `GIT_CONFIG_GLOBAL`); 0o500 plant-hardening is
     `cfg(unix)`-only (Windows gap, test name overclaims); three divergent
     Windows name-fold algorithms (view simple-uppercase vs portable-pty
     `to_lowercase` vs OS); hostile-home probe cwd sits inside the enclosing
     `/opt/repos/view` repo whose local git config no funnel layer diverts.
   - Charters 2–4 otherwise clean; the reviewer confirmed the `cfg(unix)`
     fixes are purely compile-surface and `git clone` does not honor an
     enclosing repo's `insteadOf`.
   Handle as ONE fix dispatch (C1 + I1 + the minors worth closing), then a
   scoped re-review, then adjudicate. This is the only correctness item here
   with an open *design* choice (how far to neutralize HOME), and it is a
   security fix — treat it as the highest priority of the three in §3.

---

## 4. Salvageable UNLANDED work in a worktree

- **worktree `a28475099641cb8d8`** holds the **marker_cold_ms budget** (task
  #14, user-decided 2026-07-27): two `[[budget]]` entries (`marker_cold_ms`
  max 30.0, `marker_ratio_p50` max 0.30) with rationale, two heavy-fixture
  `[[shortfall]]` entries, plus `.claude/STATUS.md` and spec edits. The
  shortfalls are concessions, so per the standing rule this needs a **Fable
  adversarial review before landing**. Work is intact; `git -C
  .claude/worktrees/agent-a28475099641cb8d8 diff` shows it.

- The other stale worktrees (a455af27, a5aa5652, a8dfa70f, ab06dd23) hold ONLY
  commits already landed on the branch and are safe to `git worktree remove`.
  Do NOT remove a2fd9219 (push-guard, may hold the report) or afe3ed63 (F8) or
  a28475 (marker budget) until you've confirmed their contents are landed.

---

## 5. The task list (recreate in the harness store at startup)

| # | Task | State |
|---|---|---|
| 15 | Gate scripts POSIX-portable | Landed (`200c28b`…`bcac248`). Remaining: §3.1 above (wire test into ci). Then CLOSE |
| 1  | Bench transposition class | F8 landed (`5e5e013`). Remaining: §3.2 (6 sites). Then CLOSE |
| 4  | Spawn-env allowlist (task 24) | Code landed + green. Remaining: §3.3 (re-review). Then CLOSE |
| 16 | Outbox backlog under wedged peer | **Design decided** (see below). Implement BEFORE #5 |
| 14 | marker_cold_ms budget | §4 — needs Fable concession review, then land |
| 5  | Re-record dev-linux baselines | Blocked by #4 landing + #16; run at/after `0299417` (the `HOME` re-point, the last revision to change a hermetic child's environment) |
| 6  | Re-record dev-macos baselines | Needs a quiet mbp (Parallels off), not code |
| 7  | `Verdict::New` budget-check flake for absolute tails on shared classes | Untouched |
| 8  | noice `ext_*` startup notifications not suppressed | Untouched |
| 9  | Windows inline-write fast path, or record why not | Untouched |
| 10 | Cold variants for remaining hot-path micro-benches | Untouched |
| 11 | P3 exit checklist (plan `2026-07-18-p3-oracle-bench-gates.md` line 1065) | Gates the phase; blocked by #5 |

**Task 16 decided design** (no open question): no refusal path (a dropped
msgpack Request hangs the peer's loop forever); queue stays unbounded (nothing
droppable without corrupting order); policy is **detect-and-surface** — the
writer thread records last-progress, the runtime tick reads it and raises the
existing notification when stalled past a threshold with a non-empty queue.
Zero cost on the caller send path (verify `Outbox::send` gains no instruction).

---

## 6. The convergence rule that was written and then ignored (do not repeat)

**No new measurement-layer task enters the queue unless it blocks an
optimization.** The loop these past days was: the bench/harness/oracle
measurement layer kept generating tasks, and finished rounds sat unlanded in
worktrees so the work looked incomplete and got re-derived. The CLAUDE.md
warns: *"do not let a long stretch of harness or budget work convince you the
measurement layer is the point."* The critical path to actually EXITING P3 is
short: land the three near-done correctness items (§3) → task 16 → re-record
dev-linux baselines (#5) → run the exit checklist (#11). Resist widening.

## 7. The real remaining performance lever (P4, needs your call)

`.claude/HANDOFF.md` §5.8 is the durable version. In one line: view's own share
of a keystroke is 139 µs p50; the largest item (49 µs) is the hop from the
blocking input thread to the runtime loop. Unifying them (poll terminal fd +
engine stdout on one loop — both hard rules survive, a readiness poll is not an
await) would put typing near the 1.10 spec bar. It is P4-scale, not a P3 patch,
and the P4 plan does not yet contain it. Your decision, not a task in flight.
