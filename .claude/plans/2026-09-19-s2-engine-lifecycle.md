# S2 — Engine lifecycle: it starts, stops and dies exactly once

Roadmap section: `.claude/plans/2026-09-04-roadmap-restructure.md` "S2 —
Engine lifecycle". Spec of record: `.claude/specs/2026-07-17-view-design.md`
(§9 engine supervision, §3 performance mandate). Branch `dev/p6-polish`,
entry HEAD `793197e` (S1 landed on master).

## Global constraints (binding on every task)

- Read `.claude/CLAUDE.md`, `.claude/rules/rust.md` and `.claude/rules/shell.md`
  before editing. Hard rules: nvim owns buffer text; the paint loop never
  awaits RPC; the RPC reader thread never blocks; no unwrap/expect/panic in
  lib crates; dependency direction core ← surface ← {native, ai}; only
  view-engine speaks RPC; only view-tui touches the terminal.
- Any change on key dispatch, input drain, grid apply or paint states its
  latency consequence in the commit body.
- A signature another crate calls changes by add-beside-then-switch.
- Tests kill only pids they spawned, never by name (`pkill`/`killall` are
  forbidden: peer sessions run live nvims on this host). A stray from your
  own test is your harness's bug (reap on drop).
- Live tests pin their timeouts through `view_test_support::host_deadline`.
- Every task: root cause first, written in the report with the evidence
  (log lines, strace/perf, the 30-run loop's tally) BEFORE the fix. A fix
  whose report cannot say why the old code failed is not done.
- Every task ends with a 30-run loop of its scenario on dev-linux with a
  zero-stray tally (`ps -o pid,ppid,stat,etime,args` filtered to the pids
  the loop spawned), recorded in the report; S2.2 and S2.3 also on mbp
  (`ssh mbp 'zsh -lc "cd ~/repos/view && …"'`, a detached checkout the
  streak advances with `git fetch` from a bundle — never `rm -rf` it;
  `~/repos/view` on mbp is at 8aa2600, ahead by nothing that matters here).
- Gate: exactly one full run per commit, inside
  `nice -n 15 task commit PATHS="…" -- -m "…"`, `run_in_background: true`
  from the start; no `task ci`/`task test`/workspace `cargo test` outside
  it. Focused tests run in the foreground to a file under `~/.claude/tmp/`.
  An agent whose gate went to the background is not finished until the
  notification returns; do not hand back before it.
- Commit subjects are changelog lines: `fix(engine): …`, `fix(tui): …`,
  `test(…): …`, release-notes-ready, one sentence.
- After every landed task: `task install` (release binary to
  `~/.local/bin/view`), done by the coordinator.
- Comments: WHY only, no session narrative, no audit tags, no "we".

## Tasks

### Task 1: `:qa!` respawns the engine (S2.1, audit #30)

**Symptom.** Under the user's config, one `:qa!` in six restarted the engine
instead of quitting: `engine restarted pid=…` with no `engine down` line,
`view: swap recovery failed … E305: No swap file found` painted, nvim's
`Press ENTER` left on the terminal, the session still running
(promise-audit 2026-09-03, `pa-view.log` 8508→9040, `pa-after-quit.txt`).
A second exhibit (ledger 2026-09-04): `view README.md` under a driver whose
probe reply was delayed 4 s never exited on `:qa!` at all — the driver sat
3 h in waitpid, the nvim child reparented to init.

**What is already there (and did not close it).** The supervision predicate
is `by_signal || !announced_exit`
(`crates/view-core/src/native/supervision.rs::note_engine_stop`).
`announced_exit` is set on the reader thread when nvim's `VimLeavePre`
bridge sends the `view_leaving` request (`crates/view-engine/src/handle.rs`
~1093, landed b2b76df 2026-08-10); `Engine::settled_report` waits
`READER_SETTLE` for the reader to close before reading it
(`crates/view-engine/src/process.rs` ~1469, landed 6114fa4 2026-08-23). The
audit reproduced the respawn after both. The race is therefore somewhere
else: candidates to verify, not assume — the runtime's `Flow::EngineLost` /
failed-write arm reading `announced_exit` before the reader decoded the
request; the liveness probe (`view_engine::heartbeat`) timing out while nvim
runs `VimLeavePre` under a slow plugin config and supervision restarting on
the wedge verdict before the exit lands; the `view_leaving` request being
sent by a bridge that a plugin's own `VimLeavePre` ordering or an `autocmd!`
removes; `:qa!` under a modified buffer taking a path that never fires the
bridge.

**Do, in order.**
1. Build the repro loop first: a script under `scripts/` or a test driver
   (your call; state it) that spawns `view <file>` on a pty under the
   user's real config (`~/.config/view/view.toml`, `~/.config/nvim`), waits
   for the settled screen, types `:qa!<CR>`, and tallies per run: exited /
   respawned / still running after a bound, plus every pid the run spawned
   that outlives it. `VIEW_LOG` on. 30 runs. Report the tally; if it is
   0/30 on this host, drive the second exhibit (delay the probe reply) and
   a slow-`VimLeavePre` config (a `vim.uv.sleep`-free `VimLeavePre` that
   burns ~1 s in Lua) until the respawn reproduces. No fix before a repro.
2. Root cause from the log of a failing run: which thread observed the stop,
   what `announced_exit` read, whether `view_leaving` ever arrived and when
   relative to EOF/`try_wait`/the probe deadline.
3. Fix at the root. Whatever the shape, the invariant to hold is: an exit
   nvim announced is never a fault, and a stop observed by any path (reader
   EOF, failed write, probe verdict, `try_wait`) consults the announcement
   only after the reader has drained the bytes nvim wrote before exiting.
   If the probe is the cause, an in-flight `VimLeavePre` suspends the wedge
   verdict (the engine is leaving, not stuck), bounded by the existing
   shutdown timeout.
4. Pin: one test per path you found racing, in the crate that owns the path
   (`view-engine/tests/shutdown.rs` or `restart.rs` style for the engine,
   `view/tests/supervision_live.rs` for the session), each RED on the old
   code — say how in the report (a delay injected where the race window
   is, never a sleep race).
5. 30-run loop, zero respawns, zero strays. Under the user's config.

**Files you may touch:** `crates/view-engine/src/{handle.rs,process.rs,heartbeat.rs}`,
`crates/view/src/{runtime.rs,recovery.rs}`,
`crates/view-core/src/native/supervision.rs`, the tests named above, one
new script under `scripts/` if you chose a script (follow
`.claude/rules/shell.md`). Anything else: stop and report NEEDS_CONTEXT.

### Task 2: strays after the parent dies (S2.2, task #18 residue)

**State.** Four commits landed 2026-09-03 (`cb1c7de` PDEATHSIG for engine
children, `9259a6a` terminal EOF/EIO is a hangup, `308b1ac`, `e060cf6`),
report `.superpowers/sdd/2026-08-21-p6-polish/stray-reaping-report.md`
(read it, and its three re-reviews, first). Open residue from that report
and the ledger:
- **Concern 1 (report):** macOS and Windows carry no armed tie for the
  engine child, the remote `ssh` leg, the ACP adapter child and the bench
  control server (population rows 1, 2, 5, 9): an engine wedged in Lua
  outlives a SIGKILLed view there.
- **Ledger 2026-09-04 stray #2, dev-linux:** a `view README.md` killed by
  SIGTERM left its `nvim --embed` alive, reparented to init, after
  `cb1c7de` was in the tree. Either that binary predated the fix (check:
  the driver's build), or the SIGTERM path (`FATAL_SIGNALS` teardown in
  `crates/view-tui/src/input.rs` → the runtime's exit) does not kill the
  engine and PDEATHSIG did not fire because the forking *thread* (see the
  report's "the signal names the thread") outlived the signal. Establish
  which with a loop: spawn view on a pty, SIGTERM it at three points
  (pre-attach, settled, mid-`:e`), assert the engine child is gone within a
  bound. 30 runs. Then SIGKILL, same.
- **macOS tie.** Design and implement one mechanism; state the alternatives
  you rejected in the report. Options known: a kqueue `EVFILT_PROC`
  `NOTE_EXIT` watcher on the parent from a process that survives the
  parent's SIGKILL (a tiny helper the spawn forks, `view --reaper`-style,
  holding only the pids; or an `nvim --embed` `--cmd` hook that polls
  `getppid()` — rejected on sight if the wedge case is a Lua busy loop that
  never yields, which it is); the parent-death pipe (child holds a pipe
  end, but nvim runs no code of ours). Whatever you choose, every
  population row that has no tie on macOS gets it or the report says, per
  row, why it cannot orphan. Windows: a job object with
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` if the spawn already goes through a
  place that can own the handle; otherwise the honest "not covered" with
  the reason, cased in the platform-gate register the way
  `a_session_whose_pty_master_closed_ends_instead_of_spinning` is
  (`.claude/plans/2026-08-21-p6-polish.md` ~2432).
- Pins: `crates/view-engine/tests/orphan_reaping.rs` gains the macOS leg
  (`cfg(target_os = "macos")`, run on mbp — report the run), the SIGTERM
  loop becomes a test in the same file or `view/tests/supervision_live.rs`.
- `lint:cross` must stay green (every platform fence both ways).

**Files you may touch:** `crates/view-engine/src/process.rs` (spawn path),
a new module under `crates/view-engine/src/` for the macOS tie if it needs
one, `crates/view-tui/src/input.rs` (signal teardown), `crates/view/src/runtime.rs`
(exit path), `crates/view-ai` and `crates/view-bench/src/session.rs` only
for the same tie on rows 5 and 9, the tests above, `Cargo.toml` of a crate
only for a platform-gated dependency already in the lockfile family
(`rustix`/`libc`). Anything else: NEEDS_CONTEXT.

### Task 3: macOS session outlives a closed pty master (S2.3, audit #38)

**Symptom.** `crates/view-oracle/tests/terminal_hangup.rs` fails on mbp:
`the session outlived the terminal it was painting to` (ledger 2026-09-04,
mbp r3 on 8a7c3aa). Round 1 of #18 added `POLLNVAL` to `terminal_hungup`
(`crates/view-tui/src/input.rs` ~167) on the `/dev/tty` reading; the macOS
leg was never verified (report fix round 2: "macOS unverified, host
unreachable"). mbp is reachable now.

**Do.** Run the test on mbp first, as it stands, from the bundle-advanced
checkout: record the failure. Root cause on macOS with evidence (what
`poll` answers on a pty slave whose master closed on Darwin, what `read`
answers — EOF `Ok(0)` rather than Linux's `EIO` is the first thing to
check; and whether the drain loop's `Ok(0)` arm reaches `SourceLost`).
Fix in `input.rs` under the narrowest correct cfg, no behaviour change on
Linux (say so with the reasoning in the commit body, and the latency
statement: the drain path is key dispatch). Pin: the existing test passes
on mbp 30/30 and on dev-linux 30/30; the process table on mbp carries no
view/nvim from the loop after.

**Files you may touch:** `crates/view-tui/src/input.rs`,
`crates/view-oracle/tests/terminal_hangup.rs`. Anything else: NEEDS_CONTEXT.

### Task 4: key-backlog spin (S2.4, T26, known-bugs #15)

**Symptom** (`.claude/known-bugs.md` ~133-150): `tmux paste-buffer` without
`-p` into a view pane delivers 2-4k discrete keys; view's main thread holds
100 % CPU for 20 s+ against a healthy engine; `perf` showed a
`clock_gettime`+`poll` hot loop on an unsymbolised release binary.
Bracketed paste is immune (one `nvim_paste`). Not a supervision defect: a
key-dispatch throughput cost.

**Do.**
1. Repro under tmux on dev-linux with a symbolised release build
   (`CARGO_PROFILE_RELEASE_DEBUG=true` or the profile the Taskfile already
   carries — check `task --list`), `perf record -g` on view's pid for the
   20 s, `perf report` in the report file. Also `VIEW_LOG` for the same run:
   how many `Msg`s per key, whether each key round-trips a redraw before
   the next is drained, whether the paint loop's poll timeout collapses to
   zero while input is pending.
2. Root cause from the profile. Candidates to verify, not assume: one
   `nvim_input` write + one full redraw apply + one paint per key with a
   zero-timeout poll between; the input drain taking one event per poll
   wake; the speculative-echo path speculating per key on a paste; the
   heartbeat re-arming per key.
3. Fix at the root, holding the hard rules (paint loop never awaits RPC;
   reader never blocks). The likely shape is batching: drain every pending
   key in one wake into one `nvim_input` call (nvim accepts a string of
   keys), and paint once per drained batch. Whatever it is, the commit body
   states the latency consequence for a single keystroke (the
   `key_to_rpc_p99_us` budget in `crates/view-bench/budgets.toml` must not
   move; cite the bench cell you would re-run, do not run `task bench` —
   it needs the quiet-host lock the coordinator holds).
4. Pin: a test in `crates/view-engine/tests/flood.rs` style or
   `crates/view/tests/` that delivers 3000 keys as separate reads and
   asserts (a) all reach the buffer, (b) the session's CPU time over the
   run stays under a bound scaled by `host_deadline`, (c) the run ends
   within a bound. RED on the old code (the old code's tally in the
   report).
5. 30-run loop of the tmux paste, zero strays, CPU tally in the report.
   Then check off the known-bugs entry in the same commit
   (`.claude/known-bugs.md` is a non-code path: commit it with the fix,
   not `commit:quick`).

**Files you may touch:** `crates/view-tui/src/input.rs`,
`crates/view/src/runtime.rs`, `crates/view-engine/src/handle.rs` (the
input send path only), `crates/view-core/src/msg.rs` if a batch message is
needed, the tests above, `.claude/known-bugs.md`. Anything else:
NEEDS_CONTEXT.

### Task 5: S2 exit

30-run loops of Tasks 1-4's scenarios on dev-linux and Tasks 2-3's on mbp,
run back-to-back from the installed binary, zero strays in the process
table after (tally in the report, pids only from the loops). `task install`
after. Roadmap section S2 marked landed with the commit range; ledger
entry; HANDOFF.md section 0e written for the S3 session.

### Task 6: CI is red on master at 793197e (run 35421882065)

The handoff recorded that run as green; it failed in five jobs. The saved
failure log is `~/.claude/tmp/ci-35421882065.log` (6057 lines; grep it,
never cat it). Last green CI: 81aa630 (2026-09-02). Every fix here is a
root-cause fix with its own case; a fix that only silences a job is
refused.

1. **ci (windows-latest), dead code.** `crates/view-engine/tests/restart.rs`:
   `RemoteSpec` import unused, `engine_pid`, `spawned_recovering`,
   `short_command_line` never used on Windows (their only callers sit under
   `#[cfg(unix)]`). `crates/view-tui/src/terminal.rs` ~1294: test struct
   `SilentTerminal` never constructed on Windows. Fence each item exactly
   as narrowly as its consumers (`.claude/rules/rust.md`: a platform-gated
   test gates every item only it consumes). Verify BOTH views: `task
   lint:cross` (freebsd stands in for macOS) and the Windows view on
   winserver (`ssh winserver`; the mirror recipe: `git archive` + scp +
   `cargo clippy --tests -p view-engine -p view-tui -- -D warnings` on the
   native host — see `~/.claude/projects/-opt-repos-view/memory/winserver-windows-ci-mirror.md`
   for the recipe). Report both outputs.
2. **ci (macos-latest), `restart.rs:332`.** Two remote-restart tests fail:
   `pid … still in the process table 5s after SIGKILL`
   (`a_remote_restart_recovers_the_far_sides_swap_and_never_guesses_at_it`,
   `a_remote_restart_with_no_swap_left_comes_up_on_the_file_and_takes_the_users_keys`).
   Root cause on mbp (`ssh mbp 'zsh -lc "cd ~/repos/view && cargo test -p
   view-engine --test restart -- a_remote_restart"'`; advance that checkout
   to the commit under test with a bundle first: `git bundle create` here,
   scp, `git fetch` there, `git checkout --detach`). First candidates:
   `common::pid_in_process_table` on macOS lists zombies (`/bin/ps` shows a
   SIGKILLed child as `Z` until its parent waits it), and the killed pid is
   the local `ssh` stub whose parent is the engine or the test; Linux's
   `/proc/<pid>` also lists zombies, so if Linux passes, find who reaps
   there and does not on macOS. If the answer is that the engine does not
   reap a killed remote child on macOS, that is a product defect and the
   fix goes in `crates/view-engine/src/process.rs`, not in the probe. 30/30
   on mbp after, 30/30 on dev-linux.
3. **visual.** `scripts/acceptance/visual-sweep.sh` ~1300 greps
   `PaletteView::new("<literal>")` out of `crates/view-core/src/native/palette.rs`
   for the message-history title; the code now titles through `title_for`
   and a named constant (~159). Read the title from where it lives now,
   and add a case to the script's own cases file (the sweep has one; find
   it under `scripts/`) that reddens when the source shape moves again
   without the script following. Run the sweep locally to its verdict
   (`task acceptance:visual`, output to a file).
4. **compat (ubuntu + macos), noice.** Three scenarios fail identically on
   both hosts: `compat/scenarios/noice.toml` step 3 waits for `view asked
   noice.nvim to turn itself off at startup, and it did` (5 s) and never
   sees it; step 4 (superseded) probes `require("noice.config").is_running()`
   and never reads `off` in 30 s; `nvim-cmp.toml` step 28 waits for `view:
   noice.nvim is using the command line` and never sees it. The text comes
   from `crates/view-core/src/update/surface_conflict.rs` ~720. Between
   81aa630 and 793197e the startup path changed (attach inside VimEnter
   823b76c, settled screen before UIEnter 2ce9cce, synchronous parse
   a0c8314/48a72d5, notifications handback e276eaa, `nvim_list_uis` keys
   0eda547): bisect the noice scenario over those commits
   (`task compat -- compat/scenarios/noice.toml`, plugin cache warms on the
   first run) and state which commit broke it and why. The fix restores the
   supersession ask and its notice under the new startup ordering; it does
   not loosen the scenario's waits. Run all three failing scenarios locally
   to green and the full `task compat` once, output to a file.

**Files you may touch:** the five files named above, `scripts/acceptance/`
cases for the sweep, `crates/view-core/src/update/surface_conflict.rs`,
`crates/view-engine/src/process.rs` and the startup hook in that file
(`crates/view-engine/src/process.rs` carries the VimEnter/UIEnter hook)
only for item 4's root cause. Anything else: NEEDS_CONTEXT. One commit per
item (four subjects, each a changelog line: `fix(ci): …` for item 1 is
wrong — name what was broken: `fix(engine): a killed remote child is reaped
on macOS`, `fix(compat): …`, `test(…): …`).
