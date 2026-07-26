# P3 Implementation Plan — Oracle + Compat Suite + Bench Harness + CI Gates

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development to implement this plan
> task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** the compounding evidence assets — a differential oracle proving
view renders what nvim renders, a compat suite over real plugin
configurations, and the real §3.4 latency protocol wired into CI as
enforced budgets (spec §13, §3; charter "the moat").

**Architecture:** view-oracle grows a ReferenceSession (bare nvim attached
with the identical ext-option set, grid built by an independent naive
applier) and a parity relation (exact RPC state parity + masked grid
parity under a quiesce protocol); a corpus of input scripts runs both
sides and fails on divergence with a minimized repro. view-bench is
rebuilt on the §3.4 measurement contract (event boundaries, hermetic env,
paired interleaved sampling) with in-repo per-machine-class baselines and
paired-ratio CI gates. The compat suite drives real pinned plugin
configurations through the pty leg and asserts zero errors + expected UI.

**Tech Stack:** existing workspace (rmpv wire, portable-pty + vt100 in
view-oracle, criterion micro-benches). No new runtime dependencies in
shipping crates; the oracle/bench crates may add dev-side crates only with
an audit-matrix row (T2 consolidates before anything new is added).

**Authored against:** tree at 2931d22 (branch dev/p2, P2 exit). Every
interface in "As-built interfaces" below was read from source at that
commit, per planning-protocol step 3.

## Global Constraints

Hard rules, embedded verbatim per planning-protocol step 8. Every task's
requirements implicitly include this section.

- nvim owns all buffer text. No view subsystem holds authoritative text
  state. Buffer mutation happens only through `Effect::Rpc`.
- The paint loop never awaits RPC. The RPC reader thread never blocks.
- No unwrap/expect/panic in lib crates (workspace lints enforce; do not
  weaken them). Test modules may open with
  `#![allow(clippy::unwrap_used, clippy::expect_used)]`.
- Dependency direction: core ← surface ← {native, ai}; only view-engine
  speaks RPC; only view-tui touches the terminal. `scripts/audit-deps.sh`
  enforces the matrix; any new crate or edge lands with its audit row in
  the same commit.
- Performance is a contract: any change touching key dispatch, grid
  apply, or paint states its latency consequence in the commit
  description.
- Use `task` targets, never raw cargo/git, for build/fmt/lint/test/commit.
  Commit only via `task commit -- -m "<msg>"`.
- Production files stay under 1000 production LOC (`task loc`); tests
  split to separate files once a file approaches the ceiling.
- Comments are WHY-only; doc comments render for users and carry a WHAT
  summary. No session-narrative markers of any kind.
- Hot paths (key dispatch, grid diff, paint) stay allocation-free after
  warmup (spec §3.3). The oracle/bench/compat code is NOT hot-path code:
  clarity beats speed there, and the reference applier is deliberately
  naive.
- rmpv containment: `check_transitive_reach rmpv view-engine view
  view-oracle` is the current allowlist. Exactly two sanctioned
  widenings this phase, both as named policy changes in their tasks:
  T2 adds view-bench only if the resolved graph actually reaches rmpv
  after consolidation (verify, don't assume), and T5 adds view-harness
  (it reaches rmpv only through the sanctioned view-oracle →
  view-engine path and exposes no rmpv API). Nothing else may widen it.
- serde/toml confinement: cargo dependencies are per-PACKAGE, not
  per-target, so a bin target inside view-oracle/view-bench cannot parse
  TOML without dragging serde into those libs' manifests and failing the
  audit's `check_absent` checks. ALL TOML parsing therefore lives in one
  new bin-only package, `view-harness` (T5 creates it), which depends on
  the oracle/bench libs and is the only crate besides `view` on the
  serde/toml allowlists. This is a named audit-policy change (T5 lands
  the audit-script diff with a rationale comment, same precedent as the
  script's rmpv allowlist); view-oracle and view-bench LIB code stays
  serde-free and their absence checks stay green.

## Coverage walk (planning protocol step 0)

Every charter deliverable and spec MUST for this phase, mapped to a task
or a recorded deferral. An implementer or reviewer finding a phase
requirement absent from both lists has found a plan defect.

| Requirement | Where |
|---|---|
| Engine pin single source; CI's three OS legs install the pin (§16, audit S4) | T1 |
| pty scaffolding consolidation before a third copy appears (charter carried finding, audit S1/M6) | T2 |
| Reference nvim with UI attached over RPC, identical ext-option set (§13.2) | T3 |
| Quiesce protocol: settle signal, no fixed sleeps, controlled clock, hermetic env (§13.2, charter risk) | T3 |
| State parity exact: buffer text, cursor, mode, registers, marks — RPC probes both sides (§13.2) | T4 |
| Grid parity masked: engine-grid regions only, view chrome masked (§13.2) | T4 |
| Divergence = failure with a minimized repro (§13.2, §13.6) | T5/T6 |
| Corpus format: inputs + engine pin + ext-option set per entry; survives pin bumps (§13.2, §15 manifest rule) | T5 |
| Runner CLI that P4-P6 extend per feature (charter Produces) | T5 |
| Fuzz harness: keystroke storms, divergence minimized (§13.6) | T6 |
| Compat suite floor: lualine, noice, nvim-notify, nvim-tree/neo-tree, dressing, fidget, telescope, treesitter, cmp, mini.nvim, which-key (§13.3) | T8 |
| Cold-bootstrap scenario (lazy.nvim fresh-machine incl. prompts) + daily-config scenario (§13.3, §5.4) | T7 |
| Compat-evidence page: row schema, coverage model, staleness rule (§13.3) | T9 |
| Bench harness v1 under §3.4 (boundaries, hermetic env, ≥1000 samples, paired interleave, p50/p99/max) | T10 |
| §3.1 rows measurable without P4 features, measured and gated per T11's per-row table (paired rows gate in CI; absolute tail-latency rows gate on the dev class via perf-audit, per §3.1's own shared-runner clause) | T10/T11 |
| Paired-ratio gating (view/nvim ratio + p99 of per-sample paired deltas), never absolute on shared runners (§3.1) | T11 |
| `perf-audit` task target (§3.3) | T11 |
| Baselines in-repo, per machine class and platform (§3.4, §13.4) | T10/T11 |
| CI: oracle + compat + bench legs in the matrix, nvim pinned (charter) | T1/T11 |
| Echo budget as a gate: p99 ≤ 8 ms AND ≤ 1.10× paired nvim (§3.1; measured 1.21 at P2 exit) | T12 |
| Latency-gap attribution re-measured after P2's loop redesign (charter exit gate) | T12 |
| Heavy-config + minimal fixtures both; `:terminal` output-flood scenario (§13.4) | T7/T10 |

Recorded deferrals (user approval required; nothing here silently
expires):

1. **Picker §3.1 rows** (match ≤16 ms, 1M-scan streaming): the picker is a
   P4 deliverable; its rows gate at P4. Sanctioned by the charter's exit
   gate text ("picker rows gate at P4 — record the split in the plan").
2. **Three-state UI-owning compat assertions** (§13.3 × §5.5: superseded /
   deferred / native-without-plugin): supersession machinery (`[native]`
   config, runtime supersession, doctor) is P4 scope by the spec's own
   phase sequence (§17). P3 asserts the UI-owning set in the only state
   that exists — plugin present, no supersession — and builds the
   three-state harness shape so P4 fills the other two columns. The
   evidence page's state column ships now and shows the two pending
   states as such.
3. **Compat-evidence page hosting**: the page generator, row schema, and
   staleness rule land in P3 (T9) as a generated in-repo page; docsite
   publication wires up with P6's docs deliverable. Generation is
   automated now; hosting is not.
4. **Windows oracle/compat/bench legs only**: ConPTY measurement and
   the Windows tier promotion are P6 exit criteria (§14). Linux AND
   macOS both run all three new CI jobs (T11) — macOS is tier-1 and
   the charter requires the additions in the matrix; nothing about
   macOS is deferred. The portable-vs-unix split in the suite is
   already enforced (fixture tests unix-gated).

## As-built interfaces this plan builds on (read from the tree at
2931d22; re-verify with `grep -n "pub fn" crates/<crate>/src/<file>.rs`
if a brief seems stale)

```rust
// view-oracle (crates/view-oracle/src/lib.rs, pty.rs, raster.rs)
Session::new(cols, rows) -> Self            // Msg-level, pure
Session::feed(&mut self, msg: Msg)
Session::surface(&self) -> Surface
Session::screen_text(&self) -> String
EngineSession::spawn(cols, rows) -> Result<Self, OracleError>   // real engine, headless
EngineSession::input(&mut self, notation: &str) -> Result<(), OracleError>
EngineSession::pump_until_flush(&mut self, deadline: Duration) -> bool
EngineSession::eval_str(&mut self, expr: &str) -> Result<String, OracleError>
EngineSession::screen_text(&self) -> String
PtySession::spawn(cmd, args, cols, rows) -> Result<Self, OracleError>
PtySession::spawn_configured(..) / send / screen / screen_raw / resize
PtySession::wait_for(needle, timeout) -> bool / wait_for_cell / wait / wait_for_exit / kill
raster::screen_text(&Surface, &Grid) -> String   // pure, no view-tui

// view-engine
Engine::spawn(EngineConfig) -> Result<Engine, EngineError>
EngineHandle::{request, request_timeout, notify}    // Clone + Send
wire::UI_EXT_OPTIONS                                // single source, all five exts

// Taskfile targets today: ci, loc, bench-micro (4 benches), bench-latency
// (v0 pairing bin — REPLACED by T10), commit, fmt, lint, audit, style, test
// CI: .github/workflows/ci.yml — 3 OS legs, nvim installed UNPINNED today
// (linux: "stable" appimage; macos: brew; windows: choco) — T1 fixes this
```

Baseline facts at authoring time (observed 2026-07-18): echo latency view
p50 0.68 ms vs nvim 0.56 ms, ratio 1.21 (budget ≤ 1.10 — T12 closes or
escalates); `task ci` green at 2931d22, 27 suites.

## File structure (new/changed this phase)

```
.engine-pin                          # T1: single-source nvim version
crates/view-oracle/src/              # LIB: serde-free
  lib.rs                             # Session/EngineSession (as-built)
  pty.rs                             # PtySession (as-built)
  raster.rs                          # + masked rendering (T4)
  reference.rs                       # T3: ReferenceSession + naive applier
  parity.rs                          # T4: StateSnapshot, Divergence, compare
  minimize.rs                        # T6: ddmin over token vecs (pure)
  compat.rs                          # T7: pty step-driving primitives (no TOML)
crates/view-bench/src/               # LIB: sampler/boundaries/pairing/report,
                                     # serde-free (T10)
crates/view-harness/                 # T5: NEW bin-only crate; the only TOML/
  src/bin/oracle.rs                  #   serde home besides `view`. run/
  src/bin/bench.rs                   #   minimize/fuzz/compat subcommands +
  src/corpus.rs                      #   bench scenarios --record/--gate.
  src/scenario.rs                    #   Schema loaders live here, one module
  src/baselines.rs                   #   each, not in bin files (LOC gate).
  src/page.rs                        # T9: evidence-page generator
corpus/*.toml                        # T5: committed corpus entries
compat/fixtures/<name>/{init.lua,lazy-lock.json}   # T7
compat/scenarios/<name>.toml         # T7/T8
docs/compat.md                       # T9: generated evidence page
crates/view-bench/baselines/<class>.toml  # data files, read by view-harness
scripts/check-engine-pin.sh          # T1: CI-vs-pin consistency check
.github/workflows/ci.yml             # T1 pin, T11 oracle/compat/bench legs
```

---

### Task 1: Engine pin single source

**Files:**
- Create: `.engine-pin`, `scripts/check-engine-pin.sh`
- Modify: `.github/workflows/ci.yml` (three install steps), `Taskfile.yml`
  (audit chain gains the check)

**Interfaces:**
- Produces: `.engine-pin` — one line, exact nvim release tag (format
  `v0.X.Y`). Consumed by CI install steps (T1), corpus entries (T5),
  bench metadata (T10). Later consumers (doctor, anodizer release pair)
  read the same file.

**Why first:** audit S4 found the three CI legs install unpinned nvim
("stable" appimage / brew / choco) and can silently run three different
engine versions today; every oracle/compat/bench artifact this phase
produces must be stamped with a pin that actually governs CI.

**Falsifiable check:** `scripts/check-engine-pin.sh` exits 0 only when
every nvim-install step in ci.yml derives its version from `.engine-pin`
and no floating install remains; breaking one leg on purpose makes it
exit 1.

- [ ] **Step 1: Capture the pin value.** Run `nvim --version | head -1`
  locally and write the observed version tag (exactly as the GitHub
  release tag) as the sole line of `.engine-pin`. Wire fact captured,
  never recalled: the value is deliberately absent from this plan — pin
  what you observe.
- [ ] **Step 2: Failing check.** Write `scripts/check-engine-pin.sh`:

```bash
#!/usr/bin/env bash
# Gate: CI must install the engine version .engine-pin names -- three OS
# legs that each resolve "latest stable" independently can and did drift.
set -euo pipefail
pin="$(tr -d '[:space:]' < .engine-pin)"
[ -n "$pin" ] || { echo "PIN FAIL: .engine-pin is empty"; exit 1; }
case "$pin" in
  v[0-9]*.[0-9]*.[0-9]*) ;;
  *) echo "PIN FAIL: '$pin' is not a vX.Y.Z tag"; exit 1 ;;
esac
fail=0
# every OS leg must read the pin file itself: one read per install step,
# or a leg can hardcode a version that silently drifts from the pin
reads="$(grep -c 'ENGINE_PIN=$(cat .engine-pin)' .github/workflows/ci.yml || true)"
if [ "$reads" -lt 3 ]; then
  echo "PIN FAIL: expected >=3 install steps reading .engine-pin, found $reads"
  fail=1
fi
if grep -nE 'neovim/releases/download/v[0-9]' .github/workflows/ci.yml; then
  echo "PIN FAIL: hardcoded nvim version literal above (must derive from .engine-pin)"
  fail=1
fi
for floating in 'download/stable/' 'brew install neovim' 'choco install neovim'; do
  if grep -qF "$floating" .github/workflows/ci.yml; then
    echo "PIN FAIL: floating install remains: $floating"; fail=1
  fi
done
exit $fail
```

  Run it: expected FAIL (ci.yml still floating) — observe all three
  floating hits.
- [ ] **Step 3: Pin the three legs.** Each leg exports
  `ENGINE_PIN=$(cat .engine-pin)` and installs that release: linux
  downloads `releases/download/${ENGINE_PIN}/nvim-linux-x86_64.appimage`;
  macos downloads the pinned release tarball (capture the correct asset
  name for the runner arch from the release's asset list — do not guess
  it) and extracts to PATH; windows downloads the pinned
  `nvim-win64.zip`. The existing `nvim --version` step gains an
  assertion that the reported version matches the pin
  (`nvim --version | head -1 | grep -F "${ENGINE_PIN#v}"`).
- [ ] **Step 4:** `scripts/check-engine-pin.sh` exits 0; `actionlint`
  clean. Add the check to the Taskfile `audit` target's command list.
- [ ] **Step 5: Disconfirm.** Revert one leg to `download/stable/`
  temporarily; check exits 1 naming it; restore; exits 0.
- [ ] **Step 6: Commit** via `task commit`.

---

### Task 2: Bench pty consolidation onto view-oracle

**Files:**
- Modify: `crates/view-bench/src/bin/latency.rs` (delete local pty
  scaffolding, use `view_oracle::PtySession`),
  `crates/view-bench/Cargo.toml` (dep on view-oracle),
  `scripts/audit-deps.sh` (matrix row: view-bench → view-oracle edge
  allowed; rmpv transitive allowlist gains view-bench only if the
  resolved graph now reaches it — verify with the audit script's own
  output, don't assume)

**Interfaces:**
- Consumes: `PtySession::{spawn, send, screen, wait_for, wait_for_exit}`
  (as-built, signatures above).

**Why now:** the charter requires one shared pty home before a third
copy appears; T6/T7/T10 all write pty-driving code this phase. The
runner-up shape — a new `view-testkit` crate both depend on — was
rejected at the call-site: `use view_oracle::PtySession;` vs
`use view_testkit::PtySession;` read identically to the consumer, and
the new crate adds an audit row, a manifest, and a coverage surface for
zero deduplication gain over the oracle crate that already owns the
abstraction and its tests.

**Falsifiable check:**
`grep -rn "openpty\|portable_pty" crates/view-bench/src` returns nothing
after the change; `task bench-latency` still produces a pairing-file
output identical in format to the pre-change run (v0 format lives until
T10 replaces it).

- [ ] **Step 1:** Run `task bench-latency` once, keep its output file as
  the before-format reference.
- [ ] **Step 2:** Replace latency.rs's local pty scaffolding with
  `view_oracle::PtySession`; delete the dead module(s).
- [ ] **Step 3:** `task bench-latency` again — output format unchanged
  (diff the two runs' headers/shape, not the numbers).
- [ ] **Step 4:** `task ci` (audit matrix row included). Commit.

---

### Task 3: ReferenceSession — independent applier over a reference nvim

**Files:**
- Create: `crates/view-oracle/src/reference.rs`
- Modify: `crates/view-oracle/src/lib.rs` (module + re-export)
- Test: inline `#[cfg(test)]` first; split to a separate tests file
  before the LOC ceiling

**Interfaces:**
- Consumes: `view_engine::{Engine, EngineConfig}` and the decoded
  ui-event stream (as-built); `wire::UI_EXT_OPTIONS`.
- Produces (T4/T5/T6 rely on these exact names):

```rust
pub struct ReferenceSession { /* engine + RefGrid + cursor/mode state */ }
impl ReferenceSession {
    pub fn spawn(cols: u16, rows: u16) -> Result<Self, OracleError>;
    pub fn spawn_configured(cfg: EngineConfig, cols: u16, rows: u16)
        -> Result<Self, OracleError>;
    pub fn input(&mut self, notation: &str) -> Result<(), OracleError>;
    /// Waits for the SafeState idle marker, then confirms with a
    /// `silence`-length quiet window on the event channel, bounded by
    /// `deadline`. Returns false on deadline.
    pub fn quiesce(&mut self, silence: Duration, deadline: Duration) -> bool;
    pub fn screen_text(&self) -> String;
    pub fn eval_str(&mut self, expr: &str) -> Result<String, OracleError>;
}
```

**Design (decided; runner-up recorded):** the reference side spawns nvim
through view-engine — same transport, same decode, same
`UI_EXT_OPTIONS` — and applies the decoded events with an
**independent, deliberately naive grid applier** (`RefGrid`: plain
row-of-cells storage, full-row writes, scroll by whole-region copy, no
damage tracking, no compaction). The differential surface is therefore
everything above decode: view's update()/Model/damage/render pipeline
against a dumb-but-obvious interpretation of the same instructions —
the layer where every P2 review Critical actually lived (tombstoning,
scroll compaction, cutover). Runner-up — an independent rmpv decoder
inside view-oracle for full-stack independence — rejected: it widens
the rmpv surface for a layer already pinned by P2's committed
wire-capture fixtures, and duplicates decode without touching the
historically buggy layers. DO-NOT-CONSOLIDATE: RefGrid intentionally
re-implements grid semantics; folding it into view-core's Grid would
blind the oracle to grid-apply bugs.

**Quiesce protocol (charter's open question, answered):** no fixed
sleeps, and no reliance on RPC-reply ordering — an `nvim_eval`
round-trip only proves the channel consumed prior messages, NOT that
key processing or its redraw burst happened (`nvim_input` queues and
returns; processing is async on nvim's main loop). The settle signal is
therefore nvim's own idle notification: the hermetic oracle config
registers a `SafeState` autocmd (fired when the main loop is idle with
no pending input) that sends an `rpcnotify` marker; `quiesce` = (1)
drive input, (2) wait for the SafeState marker, (3) confirm with a
silence window on the event channel (backstop for event batches already
in flight), all bounded by `deadline`. The implementer verifies
SafeState's firing semantics live at implementation time and pins the
observation (protocol step 1); if SafeState proves unsuitable, the
recorded fallback is marker-text round-trips (drive a buffer-visible
sentinel and wait for it in the redraw stream) — never a bare sleep.
Hermetic timer control is part of the protocol, not optional: the
oracle config pins `timeoutlen` and `updatetime` to values that cannot
fire inside a run, and the clean fixture has no plugin timers.
Determinism pin test: a mapping deliberately delayed via a one-shot
`timer_start` at a harness-chosen interval proves quiesce does NOT
settle before the delayed burst and DOES settle after it — a
deterministic construction, not a race on scheduling. Two guards, part
of the design: SafeState markers carry a sequence number and `quiesce`
accepts only markers issued AFTER the input was driven (a marker
already in flight from a pre-input idle period must not satisfy step
2); and `Unknown { name }` events observed during a quiesce are
counted and reported in the run output, never silently dropped — an
unrecognized event class is a potential divergence source the operator
must see.

**Falsifiable check:** the paired self-test drives the same input into
`EngineSession` and `ReferenceSession` and their buffer-region text
agrees; the non-tautology test proves the comparison can fail.

- [ ] **Step 1: Failing test** —
  `reference_and_engine_sessions_agree_on_a_plain_insert`: spawn both at
  60x12, drive `ihello world<Esc>` into each, flush/quiesce each,
  compare buffer-region rows (T4 replaces the region helper with the
  real mask). FAILS: ReferenceSession does not exist.
- [ ] **Step 2:** Implement `RefGrid` (resize, put, scroll by full copy,
  clear, cursor) + the event `match` over the as-built `UiEvent` enum at
  `crates/view-core/src/events.rs` (read from the tree at 2931d22):
  GridResize, GridLine, GridCursorGoto, GridScroll, GridClear,
  HlAttrDefine, DefaultColorsSet, HlGroupSet, Flush, ModeInfoSet,
  ModeChange, CmdlineShow/Pos/Hide, MsgShow/Clear, TablineUpdate,
  PopupmenuShow/Select/Hide, MouseOn/Off, Unknown. **Ext-event policy
  (stated so no one invents grid-rendering of ext content):** RefGrid
  applies only the Grid* family plus Flush; ext variants (Cmdline*,
  Msg*, Tabline*, Popupmenu*) are consumed and DISCARDED for grid
  purposes — matched attach options mean neither side's engine grid
  contains that content (§13.2's "ext-layer structural differences
  equalized"), and ext-layer correctness is covered by state probes and
  view's own P2 unit suites, not by this oracle's grid diff. Mode/Hl
  variants update reference cursor/attr state only where the comparison
  consumes them. Re-verify the variant list against the tree before
  coding; a new variant since 2931d22 must be handled or explicitly
  discarded, never silently dropped by a `_ =>` arm (deny
  `wildcard_enum_match_arm` in this module or match exhaustively).
  Run test: PASS.
- [ ] **Step 3: Non-tautology test.** Corrupt one RefGrid cell after
  quiesce (test-only accessor); assert the comparison FAILS.
- [ ] **Step 4: Quiesce deadline test.** `quiesce` during a pending
  `:sleep 10` returns false at the deadline — bounded, never hangs.
- [ ] **Step 5:** `task ci`; commit.

---

### Task 4: Parity relation — state probes + masked grid diff

**Files:**
- Create: `crates/view-oracle/src/parity.rs`
- Modify: `crates/view-oracle/src/raster.rs` (masked variant), `lib.rs`

**Interfaces:**
- Consumes: `eval_str` on both session types; `Surface` (view-core);
  `raster::screen_text`.
- Produces (T5/T6 rely on these exact names):

```rust
pub trait Probe { fn eval_str(&mut self, expr: &str) -> Result<String, OracleError>; }
pub struct StateSnapshot {
    pub buffer_lines: Vec<String>,
    pub cursor: (u64, u64),
    pub mode: String,
    pub registers: Vec<(char, String)>,
    pub marks: Vec<(String, u64, u64)>,
}
pub fn snapshot(probe: &mut dyn Probe) -> Result<StateSnapshot, OracleError>;
#[derive(Debug)]
pub enum Divergence {
    State { field: String, view: String, reference: String },
    Grid { row: u16, view: String, reference: String },
}
pub fn compare(view_state: &StateSnapshot, ref_state: &StateSnapshot,
               view_rows: &[String], ref_rows: &[String],
               mask: &[u16]) -> Vec<Divergence>;
pub fn masked_rows(surface: &Surface) -> Vec<u16>;
```

**State probes:** buffer text, cursor (`getpos('.')`), mode
(`mode(1)`), registers (`getreg` over a fixed printable register list),
marks (`getmarklist()`). The vimscript expressions here name intent,
not verified syntax — capture each probe's actual reply shape live at
implementation time and pin it in a unit test (protocol step 1).

**Masking:** `masked_rows` returns the rows the Surface's own overlay
state occupies (tabline row when visible, cmdline overlay row when
active, toast rows currently painted). The mask derives from the same
Surface the comparison reads, so chrome can never silently drift out of
the mask — one render call feeds both.

**Falsifiable check:** each Divergence arm is provably reachable: a
doctored register produces exactly the State arm, a corrupted grid row
exactly the Grid arm, and a corrupted MASKED row produces nothing.

- [ ] **Step 1: Failing tests** for the three cases above plus
  identical-inputs-empty.
- [ ] **Step 2:** Implement snapshot/compare/masked_rows. PASS.
- [ ] **Step 3: End-to-end parity test:** EngineSession vs
  ReferenceSession over `ihello<Esc>yyp` (yank exercises registers),
  full `compare` empty.
- [ ] **Step 4:** `task ci`; commit.

---

### Task 5: view-harness crate, corpus format + runner CLI

**Files:**
- Create: `crates/view-harness/` (new bin-only package: `Cargo.toml`
  with `[[bin]] oracle`, deps view-oracle + serde + toml + clap — clap
  and TOML stay at this bin boundary exactly as they do in `view`),
  `crates/view-harness/src/corpus.rs`,
  `crates/view-harness/src/bin/oracle.rs`, `corpus/` seed entries
- Modify: `Taskfile.yml` (`task oracle`), workspace `Cargo.toml`
  (member), `scripts/audit-deps.sh` — a NAMED POLICY CHANGE with a
  rationale comment (same precedent as the script's rmpv allowlist):
  view-harness joins the serde/toml transitive allowlists and the
  dependency matrix (edges: view-harness → view-oracle allowed, later
  → view-bench in T10; nothing may depend on view-harness). The
  per-crate `check_absent` loop does NOT gain view-harness — it is the
  sanctioned TOML home. Cargo dependencies are per-package, not
  per-target, which is WHY this crate exists: a bin target inside
  view-oracle would drag serde into the oracle lib's manifest and fail
  its absence check. The same policy change adds view-harness to the
  rmpv transitive allowlist (its dep on view-oracle reaches rmpv
  through the already-sanctioned path; the Global Constraints bullet
  names this as one of the two permitted widenings).

**Interfaces:**
- Consumes: everything T3/T4 produced.
- Produces: the corpus entry schema and runner exit contract P4-P6
  extend. Consumer call-sites first:

```toml
# corpus/insert-basic.toml
schema = 1
name = "insert-basic"
input = "ihello world<Esc>0x"
engine_pin = "vX.Y.Z"       # stamped from .engine-pin at authoring (real
                            # value, never this placeholder); the runner
                            # WARNS on drift vs the live engine and still
                            # runs -- corpus survives pin bumps
ext_set = "default"         # names wire::UI_EXT_OPTIONS; the only set today
# optional overrides: quiesce_silence_ms = 50, quiesce_deadline_ms = 5000
```

```bash
$ task oracle                          # whole corpus
$ task oracle -- corpus/insert-basic.toml
oracle: insert-basic ... PARITY (60x12, settled, 84ms)
$ echo $?                              # 0 = all parity; 1 = divergence,
                                       # report names entry + Divergence
```

  Runner-up shape, shown at the call-site rather than as prose:

```
corpus/default/vX.Y.Z/insert-basic.keys    # metadata encoded in the path:
  ihello world<Esc>0x                      # ext set + pin as directories
```

  Rejected: the §15 manifest rule requires engine pin + ext set to
  travel WITH the entry; path-encoded metadata cannot carry per-entry
  quiesce overrides, silently re-keys every entry on a pin bump
  (defeating "corpus survives pin bumps"), and is exactly the
  hand-maintained duplication the SSOT audit exists to kill.

**Falsifiable check:** `task oracle` runs the seed corpus green; a
deliberately-divergent run (T6's injection hook) exits 1 naming the
entry.

- [ ] **Step 1:** Create the view-harness package + audit-script policy
  change. Disconfirm the confinement both ways: `task audit` green with
  the new crate; then append `serde = "1"` to view-oracle's
  `[dependencies]` temporarily and observe `task audit` FAIL on the
  oracle lib's absence check; revert. The policy change is proven
  enforced, not assumed.
- [ ] **Step 2:** corpus.rs loader — failing tests first: valid entry
  parses; unknown field REJECTED (hard error, not ignored); missing
  `ext_set` rejected; unknown `schema` rejected.
- [ ] **Step 3:** oracle.rs bin: `run <path>` iterates entries, spawns
  both sessions per entry, drives input, quiesces, compares, reports;
  exit contract as shown. Pin-drift warning compares entry
  `engine_pin` to `.engine-pin` (T1).
- [ ] **Step 4:** Seed corpus, each entry committed only after observed
  PARITY locally: insert-basic; delete-undo (`ihello<Esc>ddu`);
  registers (`"ayy"ap`); visual-yank (`vjy P`); scroll (100-line
  fixture + `<C-d><C-d>`); cmdline-search (`/pattern<CR>n`); tab-cycle
  (`:tabnew<CR>gt`). A failing entry is a P3 bug to fix, never to skip.
- [ ] **Step 5:** `task oracle` target; `task ci`; commit.

---

### Task 6: Divergence minimizer + fuzz harness

**Files:**
- Create: `crates/view-oracle/src/minimize.rs` (pure ddmin over token
  vectors — no TOML, stays in the serde-free lib); extend
  `crates/view-harness/src/bin/oracle.rs` (`minimize`, `fuzz`
  subcommands); `corpus/quarantine/`

**Interfaces:**
- Consumes: corpus schema, runner internals (T5).
- Produces:

```bash
$ task oracle -- minimize corpus/quarantine/fuzz-42-17.toml
minimized: 148 keys -> 6 keys        # written back to the entry
$ task oracle -- fuzz --seed 42 --rounds 200 --keys 150
fuzz: 200 rounds, seed 42 ... 0 divergences
# on divergence: writes corpus/quarantine/fuzz-<seed>-<round>.toml
# (already minimized), exits 1
```

**Minimizer:** notation-token-aware ddmin over the input script
(halves, complements, single-token drops), re-running the oracle per
candidate; a candidate reproduces when compare returns a non-empty list
whose first arm kind matches. All randomness is seeded — `--seed` is
required (the harness owns the clock and the RNG; wall-clock defaults
are banned by the P2 zero-clock discipline).

**Falsifiable check:** a planted divergence minimizes to a script no
longer than the plant's prefix; fuzz with a fixed seed reproduces
byte-identical scripts across two runs.

- [ ] **Step 1:** Failing minimizer unit tests against a fake runner
  closure (divergence iff token "X" present → minimizes to exactly
  `["X"]`; divergence iff tokens "A" then "B" → minimizes to `["A","B"]`).
- [ ] **Step 2:** Implement ddmin; PASS.
- [ ] **Step 3:** Fuzz generator: seeded RNG over a weighted key
  alphabet (printable, motions, operators, mode switches, registers,
  cmdline, window ops — the alphabet is a reviewed constant in the
  source). Each round: fresh sessions, generated script, compare,
  minimize + quarantine on divergence.
- [ ] **Step 4:** Reproducibility test (same seed twice → identical
  scripts); planted-divergence end-to-end via a hidden
  `--inject-divergence-at N` runner flag (documented in the bin's help
  as test-support). `task ci`; commit.

---

### Task 7: Compat harness — fixtures, bootstrap, scenario driver

**Files:**
- Create: `crates/view-oracle/src/compat.rs` (pty step-driving
  primitives, serde-free), `crates/view-harness/src/scenario.rs`
  (schema + loader), `compat/fixtures/minimal/`, `compat/fixtures/heavy/`
  (LazyVim-style stack), scenarios in `compat/scenarios/`
- Modify: `crates/view-harness/src/bin/oracle.rs` (`compat`
  subcommand), `Taskfile.yml` (`task compat`)

**Interfaces:**
- Consumes: `PtySession::spawn_configured` (hermetic XDG env),
  `wait_for`/`wait_for_cell`, T5's runner reporting shape.
- Produces: scenario schema + per-row result records (T8 fills the
  matrix; T9 renders the page). Consumer call-site first:

```toml
# compat/scenarios/lualine.toml
schema = 1
plugin = "lualine"
class = "ui-owning"           # semantic | ui-adjacent | ui-owning
fixture = "heavy"
state = "present"             # §5.5 states: present today; superseded /
                              # native-without-plugin gate at P4
steps = [
  { send = "ihello<Esc>" },
  { wait_for = "hello", timeout_ms = 5000 },
  { assert_absent = "E5108" },      # no lua errors surfaced
  { probe = "luaeval('lualine ~= nil')", expect = "true" },
]
```

```bash
$ task compat                        # all scenarios
$ task compat -- compat/scenarios/lualine.toml
compat: lualine (heavy, present) ... OK (4 steps, 2.1s)
# results written to compat/results.json for T9's page generator
```

  Runner-up schema shape, at the call-site:

```lua
-- compat/scenarios/lualine.lua : imperative Lua driven by the harness
send("ihello<Esc>"); wait_for("hello", 5000)
assert_probe("luaeval('lualine ~= nil')", "true")
```

  Rejected: an imperative script can express unbounded control flow the
  results-recorder and page generator cannot introspect (a row is a
  fixed record: plugin, state, steps, result); declarative TOML steps
  keep every scenario diffable, schema-validatable, and renderable as
  evidence — and P4-P6 extend rows, not a scripting language.

**Fixtures are pinned:** each fixture directory commits `init.lua` plus
`lazy-lock.json`; the harness bootstraps lazy.nvim at the locked
commits into a cache directory keyed by the lockfile hash (network on
first run only; CI caches by the same key). Hermetic per-run XDG dirs;
the fixture cache is the only shared state.

**Probe channel (decided; runner-ups recorded):** compat drives the
real `view` binary through a pty, so `probe` steps and the zero-error
epilogue need a channel to the embedded engine that the pty's rendered
screen is not. The mechanism: **each compat fixture's `init.lua` calls
`vim.fn.serverstart(vim.env.VIEW_COMPAT_SOCK)`** (the driver sets the
env var to a per-run path), and the driver executes probes with the
pinned nvim binary as a client — `nvim --server $SOCK --remote-expr
'<expr>'` — reading the expression result from its stdout. Zero new
view surface, zero new RPC code (nvim itself is the client), and the
socket rides the fixture files the harness already owns. Runner-ups at
their call-sites:

```
# (a) pty scraping: send `:echo luaeval('...')` and parse the screen
$ send ":echo luaeval('lualine ~= nil')\r"; wait_for "true"
#     rejected: collides with the plugin UI under test, breaks on
#     overlays/truncation/timing -- the fragility IS what we're testing
# (b) new engine-passthrough surface: view --engine-arg --listen ...
$ view --engine-arg --listen --engine-arg "$SOCK" file.txt
#     rejected for P3: adds a consumer-facing CLI surface this phase
#     doesn't otherwise need; generic passthrough is P6 config-surface
#     scope, and fixture-side serverstart needs nothing from view
```

**Zero-error assertion (every scenario, implicit):** after the scripted
steps, the driver probes `:messages` content and `v:errmsg` over the
probe channel and fails the row on any E-numbered or Lua-traceback
content — capture the exact probe expressions live and pin their reply
shapes (protocol step 1).

**Falsifiable check:** `task compat` runs the minimal fixture green; a
scenario with a deliberately-wrong `wait_for` fails its row and exits 1
naming scenario + step index.

- [ ] **Step 1:** Scenario schema loader (failing tests: valid parses,
  unknown field rejected, unknown `state` rejected).
- [ ] **Step 2:** Driver: hermetic spawn, step executor (`send`,
  `wait_for`, `wait_for_cell`, `assert_absent`, `probe`), implicit
  zero-error epilogue, JSON result record per row.
- [ ] **Step 3:** `minimal` fixture (no plugins) + a smoke scenario;
  observed green. Deliberately-broken scenario observed red with the
  right step index.
- [ ] **Step 4: Cold-bootstrap scenario** (§13.3 mandatory): a fixture
  whose cache key is deliberately absent — the scenario drives the full
  lazy.nvim bootstrap INCLUDING its interactive prompts through the pty
  and asserts a clean post-install state. Runs in CI with network;
  marked slow.
- [ ] **Step 5:** `heavy` fixture: LazyVim-style stack pinned by
  lockfile (capture the plugin list from a real LazyVim baseline at
  implementation time; commit the lockfile). Smoke scenario green.
- [ ] **Step 6:** Daily-config scenario (§5.4 standing): runs against
  `$VIEW_DAILY_CONFIG` when set (maintainer's machine), reported
  SKIPPED-with-notice when unset (CI); the heavy fixture is CI's proxy.
  Fixture-less scenarios have no `serverstart` in their init.lua, so
  the driver opens the probe channel itself: it types
  `:call serverstart($VIEW_COMPAT_SOCK)<CR>` into the pty at scenario
  start, then probes over the socket as usual. `task ci`; commit.

---

### Task 8: Compat matrix — the §13.3 named floor

**Files:**
- Create: one scenario per named plugin under `compat/scenarios/`
- Modify: fixture init.lua/lockfiles as plugins are added

**The floor (may add, never subtract):** lualine, noice, nvim-notify,
nvim-tree, neo-tree, dressing, fidget (UI-owning class); telescope,
which-key (UI-adjacent); nvim-treesitter, nvim-cmp, mini.nvim
(semantic). Plus the cold-bootstrap and daily-config scenarios (T7).

**Per-class assertion shape:**
- *Semantic:* drive the feature, assert its effect (treesitter:
  highlight groups active on a fixture file via probe; cmp: completion
  menu content from a source; mini.nvim: one module's observable
  behavior). Zero-error epilogue.
- *UI-adjacent:* drive the plugin's UI into the grid (telescope open +
  type + candidate visible; which-key popup after leader) and assert
  via `wait_for` markers. Zero-error epilogue.
- *UI-owning, `present` state (the only P3 state — recorded deferral
  2):* plugin loads, draws its surface through the grid or ext stream,
  zero errors, one plugin-specific marker each (lualine: statusline
  content in grid; noice/nvim-notify: message routed and visible;
  nvim-tree/neo-tree: sidebar rendered; dressing: input UI opens;
  fidget: progress text appears during a scripted LSP-less progress
  event — capture how to trigger each marker from the plugin's own
  docs/behavior at implementation time and pin the observed marker).

**Falsifiable check:** `task compat` green across every row on Linux;
each row's marker assertion observed failing once against a fixture
with the plugin removed (proves the marker tests the plugin, not the
harness).

- [ ] **Step 1:** Add plugins to the heavy fixture lockfile
  incrementally — one commit per class batch, each batch's scenarios
  green before the next.
- [ ] **Step 2:** Marker-validity disconfirm per row (plugin absent →
  row red), batched per class.
- [ ] **Step 3:** `task ci` (compat suite included per T11's wiring
  order — run locally regardless); commit per batch.

---

### Task 9: Compat-evidence page

**Files:**
- Create: `docs/compat.md` (generated), generator in
  `crates/view-harness/src/page.rs` (its own module, not more weight in
  the bin file — the oracle bin file only routes the `page` subcommand)

**Interfaces:**
- Consumes: `compat/results.json` (T7's row records), `.engine-pin`.
- Produces the §13.3 page: row schema **plugin, version (from the
  lockfile), engine pin, scenario, state, result, date** — date is the
  run date injected by the runner, machine-stamped, never hand-edited.
  Coverage model stated on the page: top-N by plugin-manager download
  rank per compat class, currently the §13.3 named floor. Staleness
  rule stated on the page and enforced: the generator refuses to emit
  if `results.json`'s recorded engine pin differs from `.engine-pin`
  (a pin bump forces a re-run before the page can regenerate).

```bash
$ task compat && task oracle -- page
wrote docs/compat.md (13 rows, pin vX.Y.Z, <run date>)
```

**Falsifiable check:** page regenerates deterministically from
results.json; with a doctored stale pin in results.json the generator
exits 1 naming the drift.

- [ ] **Step 1:** Failing generator tests (row rendering from a fixture
  results.json; stale-pin refusal).
- [ ] **Step 2:** Implement; generate the real page from a full compat
  run; commit page + generator. Hosting wires up at P6 (recorded
  deferral 3).

---

### Task 10: Bench harness v1 — the §3.4 protocol

**Files:**
- Create: `crates/view-bench/src/` measurement modules (boundaries,
  sampling, pairing, report — serde-free lib),
  `crates/view-harness/src/bin/bench.rs`,
  `crates/view-harness/src/baselines.rs` (TOML read/write),
  `crates/view-bench/baselines/` (schema + first recorded class)
- Delete: `crates/view-bench/src/bin/latency.rs` + the v0 pairing-file
  mechanism and its Taskfile target
- Modify: `Taskfile.yml` (`bench-latency` replaced by `task bench`),
  `scripts/audit-deps.sh` (view-harness → view-bench edge), view crates
  gain `bench-taps` feature (below)

**Interfaces:**
- Consumes: `view_oracle::PtySession` (T2), `.engine-pin` (T1).
- Produces: the harness + baseline format T11 gates on and P4-P6 extend
  with new scenarios. Consumer call-sites first:

```bash
$ task bench -- --scenario echo --fixture minimal --class dev-linux
echo/minimal: view p50 0.61ms p99 0.94ms | nvim p50 0.55ms p99 0.71ms
      ratio(p99) 1.32  paired-delta p99 0.29ms  samples 1000 (+100 warmup)
$ task bench -- --all --class dev-linux --record   # writes baselines/dev-linux.toml
$ task bench -- --all --class dev-linux --gate     # exit 1 on any breach,
                                                   # per the T11 gating table
```

```toml
# crates/view-bench/baselines/dev-linux.toml
schema = 1
engine_pin = "vX.Y.Z"       # real pin at record time, never this placeholder
[echo.minimal]
ratio_p99 = 1.32            # measured-or-better becomes the regression bar
paired_delta_p99_ms = 0.29
[echo.heavy]
ratio_p99 = 1.41
paired_delta_p99_ms = 0.44
[first_paint.minimal]
cold_ms = 38.0
ratio_vs_nvim = 0.4
# ... one [scenario.fixture] table per measured cell
```

**Scenario × fixture matrix (§3.4/§5.4: "minimal and heavy-config
fixtures both — not just minimal synthetic configs"):** `echo`,
`first_paint`, and `scroll` run on BOTH the minimal fixture and the
heavy fixture (the same pinned lazy-lock fixture T7 commits — one
fixture, two consumers, no duplication); `memory` and `flood` run on
minimal (their budgets are view-side and config-independent — an
assumption recorded here; if heavy-config numbers diverge wildly at
first measurement, plan-sync adds the cells). Rows 4-5 run on minimal.
Baselines are keyed `[scenario.fixture]` as shown.

**Scenarios (every §3.1 row measurable without P4 features):**
1. `echo` — keypress → cell change, steady typing, paired interleaved
   view/nvim within one run (§3.4), p99 ≤ 8 ms AND ratio ≤ 1.10.
2. `scroll` — 100k-line fixture file, sustained `<C-d>` stream,
   staleness p99 ≤ 16 ms (input byte → corresponding content visible;
   line-numbered fixture text makes "corresponding" checkable from
   vt100 frames).
3. `first_paint` — cold process → first UI-shell frame ≤ 50 ms, paired
   against bare nvim's first frame on the same config (the felt win,
   measured).
4. `input_path` — key at pty → RPC bytes written, p99 ≤ 100 µs
   (bench-taps build, below).
5. `output_path` — redraw parsed → terminal write, p99 ≤ 1 ms
   (bench-taps build).
6. `memory` — PSS from smaps_rollup after the standard workload script
   (10 buffers), ≤ 150 MB.
7. `flood` — `:terminal` output flood, run PAIRED against the same
   flood in bare nvim on the same fixture (§13.4 places this scenario
   under the paired rule; T11's staleness-ratio CI gate consumes the
   pair); asserts the coalescing invariant (paint cadence stays inside
   the staleness budget; UI thread never blocks — measured, not
   assumed).

**bench-taps (internal boundaries without contaminating the shipped
binary):** rows 4-5 need timestamps at internal boundaries. A
`bench-taps` cargo feature on `view`/`view-engine`/`view-tui` compiles
in monotonic-timestamp writes to a file descriptor named by
`VIEW_BENCH_TAP_FD`; without the feature the tap sites are `cfg`'d out
entirely (zero cost, type-checked absent). E2e rows (1,2,3,6,7) run the
PLAIN release binary — gates on those rows always reflect what ships.
**Rows 4-5 GATE on the taps build** — the §3.4 written procedure for
those rows names the taps build as the measurement configuration, which
satisfies reproducibility (two engineers, same procedure, same number).
Precondition, measured not assumed: the harness characterizes tap
overhead first (timestamp-write cost at the tap sites, measured by a
micro-bench) and records it in the report; if overhead exceeds 5% of
the 100 µs budget, the tap design must change before the row can gate.

**Hermetic env (§3.4):** pinned fixture config, fixed TERM and 120x40
grid, warm page cache unless the row says cold; machine class is an
explicit required `--class` argument — the harness refuses to gate
without one (shared-runner numbers must never silently gate as if
dedicated).

**Falsifiable check:** `task bench -- --all --class dev-linux` produces
every matrix cell's report with ≥1000 samples each; `--gate` against a
doctored baseline (ratio lowered below measured) exits 1 naming the
cell; sampling discipline verified by a unit test on the sampler
(interleaving, warmup exclusion, percentile math against a known
distribution).

- [ ] **Step 1:** Sampler + percentile + pairing module with unit tests
  first (known distributions → known p50/p99; interleave order
  asserted; warmup excluded).
- [ ] **Step 2:** `echo` scenario end-to-end paired; numbers reported.
- [ ] **Step 3:** Remaining e2e scenarios (scroll, first_paint, memory,
  flood), one at a time, each observed producing sane numbers before
  the next.
- [ ] **Step 4:** `bench-taps` feature + tap sites + rows 4-5. Verify
  the plain build has zero tap code (`cargo expand`-free check: grep
  the plain binary's symbols, or assert the feature-off build compiles
  the tap module to nothing via `#[cfg]` unit structure). State the
  latency consequence in the commit description (hard rule): tap sites
  are cfg'd out of shipping builds; zero effect.
- [ ] **Step 5:** Delete latency.rs + v0 mechanism; `--record` the
  first baseline for the dev machine class; commit baseline.
- [ ] **Step 6:** `task ci`; commit.

---

### Task 11: CI gates — oracle, compat, bench legs + perf-audit

**Files:**
- Modify: `.github/workflows/ci.yml` (three new jobs), `Taskfile.yml`
  (`perf-audit` target), `crates/view-bench/baselines/` (CI class)

**Wiring:**
- `oracle` job (linux + macos): `task oracle` over the committed corpus
  — hard gate, merge-blocking, both legs.
- `compat` job (linux + macos): `task compat` with the lockfile-keyed
  cache; cold-bootstrap scenario allowed network — hard gate; red rows
  carry the T7 reporting so a failure names plugin + step. The charter
  requires the additions "run in the matrix" and macOS is tier-1
  (§14); only Windows defers (deferral 4).
- `bench` job (linux `gh-linux` + macos `gh-macos`):
  `task bench -- --all --class <class> --gate`, per-row semantics from
  the table below.
- `task perf-audit` (§3.3): `task bench -- --all --class <local>
  --gate` plus the micro-benches, one command for pre-release and
  post-strangle runs. On the dev class, EVERY row gates, absolutes
  included.

**Per-row gating table (resolves §3.1's "paired, not absolute" clause
against the charter's "budgets become CI gates" — §3.1's own text
governs on shared runners; the spec wins on conflict):**

| Row | CI shared runners (gh-*) | dev class (`perf-audit`) |
|---|---|---|
| echo (minimal+heavy) | ratio ≤ 1.10 AND paired-delta p99 measured-or-better | + absolute p99 ≤ 8 ms |
| first_paint (minimal+heavy) | ratio vs nvim measured-or-better | + absolute ≤ 50 ms cold |
| scroll staleness | report-only (absolute tail latency) | ≤ 16 ms gate |
| input_path (taps) | report-only | ≤ 100 µs gate |
| output_path (taps) | report-only | ≤ 1 ms gate |
| memory PSS | ≤ 150 MB gate (not a tail latency; §3.1's noise argument does not apply) | same |
| flood invariant | staleness-ratio measured-or-better | staleness-within-budget gate |

  Rows report-only in CI are NOT informational-print drift from the
  charter: they gate on every `perf-audit` run (pre-release, mandatory,
  dev class) and their CI reports fail loudly on missing data — the
  split is §3.1's own gating rule applied honestly, recorded here for
  the user's visibility at plan review.

**Baseline provenance (the mechanism, or the gate can never arm):**
`--gate` HARD-FAILS when no baseline exists for its class UNLESS the
workflow passes `--bootstrap`, which runs record-mode and uploads the
baseline TOML as a CI artifact. The T11 implementer's flow: push the
branch with `--bootstrap` set, download the two gh-class artifacts,
commit them, remove `--bootstrap` in the same commit. From then on a
missing baseline is a loud failure and every baseline change is a
reviewed diff. (CI-runner verification itself remains push-gated —
known-bugs.md item, user-owned; the flow above is written into the
task so the first push executes it.)

**Falsifiable check:** a doctored baseline (impossible ratio) makes the
bench gate command exit 1 locally naming the cell; `--gate` with the
class's baseline file deleted exits 1 (provenance rule); actionlint
clean; the three jobs appear in the workflow graph and run the exact
task targets named here (no inline duplication of task logic in YAML —
the Taskfile is the single source).

- [ ] **Step 1:** Taskfile targets final (`oracle`, `compat`, `bench`,
  `perf-audit`); each runs locally green.
- [ ] **Step 2:** ci.yml jobs calling exactly those targets; actionlint
  clean; engine-pin check still green (T1's guard sees the new jobs).
- [ ] **Step 3:** Local disconfirm of the gate path (doctored baseline
  → exit 1, named row). CI-runner verification itself remains
  push-gated (known-bugs.md item; user-owned).
- [ ] **Step 4:** `task ci`; commit.

---

### Task 12: Echo-gap attribution + close (1.21 → ≤ 1.10)

**Files:**
- Modify: wherever attribution points — unknown until measured. The
  task is measurement-first by construction.

**Method:** T10's decomposed rows bound the pipeline: nvim-side time is
the paired bare-nvim echo run; view's additions decompose into
input-path (row 4), engine round-trip (paired delta minus rows 4+5),
and output-path (row 5). Attribute the 0.12 ms excess (P2-exit
measurement) to its component with the taps build, then fix the
dominant component. Candidate hypotheses to test — labeled assumptions,
not conclusions: terminal-writer flush batching (one syscall per frame
vs several), key-encode allocation on the input path, damage-fold
overhead at tiny damage sizes. Each fix lands with its micro-bench
delta and a re-run of the paired echo row (hard rule: latency
consequence stated per commit).

**Falsifiable check (the phase gate):** `task bench -- --scenario echo
--class dev-linux --gate` passes with ratio ≤ 1.10 on the dev class;
the CI class gates measured-or-better thereafter.

**Escalation door (honest):** if attribution shows the residual lives
in costs view cannot remove at this layer (e.g. the embed round-trip
itself), that is a spec-budget conflict — plan-sync (protocol step 6):
present the attribution evidence and the ≤ 1.10 budget to the user;
the number is the spec's, and only the user revises the spec. Do not
silently reframe the budget.

- [ ] **Step 1:** Attribution run: all rows on the taps build + paired
  echo, written to a dated report file; dominant component named from
  observed numbers.
- [ ] **Step 2:** Fix the dominant component (TDD: micro-bench or unit
  test pinning the improvement where the component has one; e2e paired
  re-run as the integration proof).
- [ ] **Step 3:** Repeat until the gate passes or the escalation door
  is the honest answer. Each iteration is one commit with its measured
  delta.
- [ ] **Step 4:** `task ci`; final paired run recorded into the
  baseline; commit.

---

## P3 Exit Checklist

Every item closes with an evidence citation — the command run and its
observed output — never a bare checkmark (planning protocol step 7).

- [ ] `task ci` green at the branch tip (now includes oracle + compat
  legs locally).
- [ ] Oracle corpus green over the seed set; fuzz `--rounds 200` at two
  seeds with zero unquarantined divergences — or every quarantined
  entry fixed or user-approved as a known bug.
- [ ] Compat matrix green across the §13.3 floor on Linux AND macOS
  (CI legs per T11), plus a real-hardware `ssh mbp` run of
  `task compat` (§13.7 evidence, supplemental to CI); evidence page
  regenerated from the run.
- [ ] Every §3.1 row measurable without P4 features measured, and gated
  per T11's per-row table (CI paired gates armed with committed
  baselines; dev-class absolute gates green via `task perf-audit`) —
  baseline file diff cited. Picker rows recorded as gating at P4
  (deferral 1).
- [ ] Echo ratio ≤ 1.10 gate passing (T12), or the escalation door
  taken with attribution evidence and the user's decision recorded.
- [ ] Latency-gap attribution re-measured and written to the baseline
  file (charter exit gate).
- [ ] `perf-audit` target runs end-to-end.
- [ ] Zero-clock discipline holds: the T4-era grep on runtime.rs still
  clean; no wall-clock defaults anywhere in oracle/bench harness code
  (seeded RNG, harness-owned deadlines only).
- [ ] Coverage-walk deferrals 1-4 re-confirmed with the user at exit
  (none silently expire).
- [ ] `.claude/known-bugs.md` drained or user-approved deferrals only.
- [ ] Dogfood note appended (real editing sessions through view, now
  with the oracle watching).
- [ ] Guided QA doc updated for P3 surface (oracle/compat/bench have no
  user-visible UI, so this may be a no-op — record the decision).
- [ ] P4 plan authored under the charter + planning protocol, against
  the tree; S2 (Theme enum-keyed groups) explicitly in its scope per
  the DRY/SSOT audit handoff.

