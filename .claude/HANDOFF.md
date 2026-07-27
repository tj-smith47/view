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

0. **Read `.claude/IN-FLIGHT.md` first.** As of 2026-07-27 there is
   uncommitted work in the tree, one half-landed change, and an adversarial
   review whose findings are pre-commit blockers. That file supersedes
   section 3's task table where the two disagree, and it is deleted once its
   contents land as commits.
1. Create tasks from section 3 below, verbatim, in the order listed. The
   harness task store does not persist across sessions and is not
   shell-accessible, so this file is the only carrier.
2. `git log --oneline -3` and `git status --porcelain` to confirm where you are.
3. `tail -c 4000 .superpowers/sdd/progress.md` for the last few entries.
4. `task --list` to index the project's task targets before running anything.

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

| # | Task | Why it sits here |
|---|---|---|
| 53 | P3 exit checklist execution, evidence-cited per plan protocol | Gates the phase, and carries this file's own retirement (section 9) |
| 26 | Close the attributed echo typing gap in view's input and paint paths | **Half done.** The writer hop is gone (`outbox.rs`: the loop writes inline when the pipe says it can and nothing is queued). `echo.minimal` ratio_p50 1.354 -> 1.172, `input_path` p99 154.7 -> 117.7. view's own share is now 139 us p50 of a 644 us round trip: 71 input, 68 paint. What remains is one architectural lever, not tuning -- see the pitch in section 5.7 |
| 28 | Give Windows an inline-write fast path, or record why it cannot have one | `can_write_inline` is `false` off unix, so the typing win is unix-only today. The two POSIX guarantees it rests on (PIPE_BUF atomicity, POLLOUT meaning PIPE_BUF of room) do not transfer; overlapped I/O on a named pipe would give an equivalent proof. winserver can measure it |
| 23 | Re-derive the gate headroom constants; fix scroll's tier mismatch and flood's cross-class stimulus divergence | 18 landed, so a spec bar now exists to size the headrooms against; flood's shortfall entry names this task as its resolution |
| 24 | Allowlist the environment at the bench/oracle spawn funnel | Must land before CI ever runs with a secret configured. Today the funnel is a denylist, into editors that execute fixture Lua and network-fetched plugins |
| 21 | Record a quiet-host dev-macos baseline: input_path and first_paint's split metrics | The rows are runnable again; they need a quiet mbp, not more code. Until it lands, the dev-macos first_paint cell gates red on `unmeasured_metrics` |
| 25 | noice's `ext_*` disable opts are not suppressing its startup error notifications | Cosmetic in the compat fixture, real as a compat finding |
| 20 | P4 plan adversarial review | A fresh session; the prompt is written at `.claude/plans/2026-07-26-p4-review-prompt.md` |

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

**`known-bugs.md` is fully drained** — zero unchecked items. It must stay that
way, or carry only explicit user-approved deferrals, before declaring P3 done.
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

**A caveat that outlives this pitch:** every criterion micro-bench in the
repo measures the hot state. They are sound relative instruments and wrong
as absolute costs, by roughly the factor above. Task 29 gives the rest of
them cold variants; until it lands, do not quote a hot number as what a
keystroke pays.

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
