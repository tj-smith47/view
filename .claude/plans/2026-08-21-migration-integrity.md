# Migration Integrity Implementation Plan — capability probing, surface ownership, compat evidence, notification surface

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development to implement this plan
> task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Authorities.** Spec of record: `.claude/specs/2026-07-17-view-design.md`;
on conflict the spec wins. Charters: `.claude/plans/2026-08-20-migration-integrity-charters.md`
(C1 capability probing, C2 surface ownership, C3 compat evidence, C4
notification surface) — the charters pin the WHAT and a charter item is a
commitment, not a suggestion (ruled 2026-08-14: dropping one at planning
time requires user approval; nothing is dropped here, and every place the
code makes a chartered deliverable impossible as written is recorded under
"Deviations" below with its reasoning). Planning protocol:
`.claude/plans/2026-07-18-p3-p6-charters.md` (binding: coverage walk;
wire facts captured, never recalled). Spec amendments this plan depends on
are drafted verbatim in `2026-08-21-migration-integrity-spec-amendments.md`
beside this file and land as their own commits, ahead of the tasks that
rely on them.

**Goal.** Make three things true that a real user's first dogfood proved
false: what view detects about a terminal is *detected* (C1); a plugin
claiming a surface view owns is *noticed and resolved*, generically, for
plugins nobody tested (C2); the compat suite can *fail* on a migration
defect (C3). And build the notification surface the same session asked for
— slot-timed, pausable, recoverable (C4).

**Start gate.** P5 AI complete (exit checklist closed, whole-branch final
review folded). Ruled by the user 2026-08-20.

**Authored against:** tree at `dev/p5-ai`, 2026-08-21. Every signature,
constant and file path cited below was read from the as-built source this
session (`crates/view-tui/src/tiers.rs`, `crates/view-tui/src/terminal.rs`,
`crates/view-tui/src/paint.rs`, `crates/view-surface/src/overlay.rs`,
`crates/view-surface/src/lib.rs`, `crates/view-core/src/model.rs`,
`crates/view-core/src/msg.rs`, `crates/view-core/src/native/{toast,palette,registry,mappings}.rs`,
`crates/view-core/src/update/mod.rs`, `crates/view-native/src/supersede.rs`,
`crates/view-native/src/config.rs`, `crates/view-engine/src/nvim_api.rs`,
`crates/view/src/{main,startup,native,runtime,clipboard,osc52,vlog}.rs`,
`crates/view-harness/src/scenario.rs`,
`crates/view-harness/src/bin/oracle/compat.rs`,
`crates/view-oracle/src/compat.rs`, `compat/fixtures/heavy/nvim/init.lua`,
`compat/scenarios/{noice,nvim-cmp}.toml`, `scripts/audit-god-files.sh`,
`crates/view-bench/budgets.toml`), plus the pinned noice source in the
compat plugin cache (`compat/.cache/8c01779ece3ede73/nvim/lazy/noice.nvim/lua/noice/`
— `health.lua`, `init.lua`, `config/init.lua`; commit `7bfd942`).
Re-verify against the tree at execution time per protocol step 6 if any of
it has moved.

---

## Board mapping

Nine board items, #10–#18. Every task below names its board item; every
board item is covered by at least one task.

| # | Board item | Charter | Tasks | Size |
|---|---|---|---|---|
| #10 | Capability line moves to `VIEW_LOG`; surfaced only on request | C1 | T1 | S |
| #11 | Border charset decoupled from tier | C1 | T2, T6 | S, S |
| #12 | Terminal probe wire capture (DECRQM sync, DECRQSS truecolor, CPR box-glyph width) | C1 | T3 | M |
| #13 | Truecolor and box-glyph support are probed, not inferred | C1 | T4 | M |
| #14 | Capability register: every `TermCaps` bit has a named probe | C1 | T5 | S |
| #15 | Compat suite proves the unaccommodated config (states + lint) | C3 | T7, T15 | M, S |
| #16 | The ext set follows `[native]`; `vim.notify` takeover; the default first launch says one thing | C2 | T8, T9, T19 | M, S, M |
| #17 | Generic surface-ownership conflict detection + matrix | C2 | T10, T11, T13 | M, L, S |
| #18 | Notification surface: slot timers, motion, pause, history, copy | C4 | T14, T16, T17, T18 | S, M, M, M |

Two larges, each preceded by its own design/capture task: T11 (generic
surface-conflict detection) is preceded by T10 (float wire capture); T12
(cmdline float absorption) is preceded by the same capture plus T11's
policy table. Cheap independents (T1, T2, T7) land first.

T19 is numbered last but sequenced between T11 and T14: it is the
completion of the C2 fork's *defaults* half, and it needs T11's claimant
table to exist. Execution order is the dependency graph below, not the task
numbering; T19 is written beside T9 in this file because that is the task
it reads with.

---

## Global Constraints

Hard rules, embedded verbatim per planning-protocol step 8. Every task's
requirements implicitly include this section.

- **nvim owns all buffer text.** No view subsystem holds authoritative text
  state; buffer mutation happens only through `Effect::Rpc`. This phase
  never mutates buffer text at all — but it *reads* a foreign plugin's
  float buffer (T12) and *hides* a foreign window (T11/T12). Both are
  window/UI operations through `Effect::Rpc`; neither writes buffer text,
  and neither may fall back to writing one.
- **The paint loop never awaits RPC. The RPC reader thread never blocks.**
  C4's animation is damage-driven and ticked by a one-shot timer effect on
  the same shape `Effect::ScheduleToastExpiry` already uses
  (the `Effect::ScheduleToastExpiry` arm in `crates/view/src/runtime.rs`); no frame ever waits on the engine,
  and an idle stack schedules no timer at all.
- **`view-core` stays pure.** No I/O, no env access, no clock. Time enters
  only as a `Msg`. C4's slot timers and pause therefore live as state in
  `view-core` advanced by `Msg::ToastExpired`/`Msg::AnimTick`, with the
  shell owning every timer thread.
- **No unwrap/expect/panic in lib crates** (workspace lints enforce; do not
  weaken them). Test modules may open with
  `#![allow(clippy::unwrap_used, clippy::expect_used)]`.
- **Dependency direction: core ← surface ← {native, ai}**; only
  `view-engine` speaks RPC; only `view-tui` touches the terminal.
  Consequence this phase keeps tripping over: `vlog` lives in the bin crate
  `view`, so a diagnostic `view-tui` resolves is *returned* to the bin and
  logged there (T1), never logged from `view-tui`. `scripts/audit-deps.sh`
  enforces the matrix; run `task audit`.
- **Production files stay under the 1000-line ceiling**
  (`scripts/audit-god-files.sh`, `task loc`; the exemption list is empty and
  neither pin may be raised). T11, T12 and T16–T18 all grow files that are
  already large — each names its module split in its own steps rather than
  discovering the ceiling at commit time.
- **Wire strings come from capture docs, never from recall.** Two capture
  tasks precede every task that consumes a wire string: T3 (terminal escape
  replies: DECRQSS, CPR, DA1) before T4; T10 (nvim float geometry and
  content) before T11/T12. A task may not embed a query, a reply grammar,
  or a plugin-observable field name that no capture doc in `docs/` records.
- **Performance is a contract.** Any change touching key dispatch, grid
  apply, or paint states its latency consequence and the §3.1 budget row
  that covers it, in the commit description. Tasks that touch none of the
  three (capture docs, scenario schema, config plumbing, doc matrices)
  commit without a latency statement.
- **Comments are WHY-only**; no session-narrative markers, no caller refs,
  no deferred-work pointers (`scripts/check-style.sh`, `task style`).
- **`task` targets only** — never raw cargo. `task ci` before every commit;
  commit only via `task commit -- -m "<msg>"`. Gates run in the
  FOREGROUND with a generous timeout when a subagent runs them.

---

## Deviations from the charters (recorded, not dropped)

1. **C2, shape B: "detectable from geometry view already receives" — it is
   not.** `win_float_pos` and the rest of nvim's window events are
   `ext_multigrid` events; view attaches without multigrid
   (`UI_EXT_OPTIONS` in `crates/view-engine/src/nvim_api.rs`), so a float composites into
   the base grid and view receives *no* geometry for it, only `grid_line`.
   Detection is therefore built as an engine-side watcher on the existing
   `view_bridge` autocmd channel (T11), reporting `nvim_win_get_config`
   geometry over RPC. Same deliverable, different source; the charter's
   conclusion (the conflict class is generically detectable) survives
   intact, and the watcher is strictly more informative than a composited
   grid diff would have been.
2. **C2, shape A: the charter's fork is settled against option 1 by the
   plugin's own source — on evidence the charter states imprecisely.** The
   charter says "no noice option gates it". Read exactly, at pinned commit
   `7bfd942`:
   - `noice.setup(opts)` calls `Health.check({checkhealth=false,
     loaded=false})` in the pinned `lua/noice/init.lua`, **before**
     `require("noice.config").setup(opts)` on the lines just below parses any user
     option. The ext loop inside that check (its `nvim_list_uis()` loop in `health.lua`, over
     `nvim_list_uis()` × `{ext_cmdline, ext_popupmenu, ext_messages}`) is
     unconditional, and the check returns `true` regardless, so setup
     proceeds and the errors stand.
   - The 1 s re-check **is** gated: `M.checker = Util.interval(1000, …)`
     (in `health.lua`) is started only under `if
     Config.options.health.checker then` (in `init.lua`), whose default
     `checker = true` is documented as "Disable if you don't want health
     checks to run" (in `config/init.lua`). Its errors also dedup through
     `Util.error_once`, so it never re-shows what setup already showed.

   The setup-time check alone settles the fork, and settles it harder than
   the interval would have: **no options-level disable can suppress a first
   launch's errors, because they are raised before the options exist.**
   "Detect and disable the conflicting component" therefore requires
   reaching into plugin privates (exactly the `_once` pre-seed the heavy
   fixture performs today) — the treadmill the charter exists to avoid, and
   a contract no upstream owes us. Option 2 (an ext-set opt-out so the
   config runs unchanged) is taken, in the form that adds no new config
   surface: the ext set follows the `[native]` switches that already exist
   and that §5.5/§9 already promise return the surface (T8). Deferred
   externalization (attach without the exts, enable them after `VimEnter`)
   was considered and rejected: the gated 1 s checker is on by default and
   would see the exts the moment they were enabled, so the errors would
   arrive a second later instead of not at all.
3. **C2, "naming the plugin" is best-effort and bounded.** A float carries
   no authorship. The notice names what the window itself identifies —
   buffer filetype, buffer name, and the window's `zindex`/anchor — and
   falls back to naming the surface and the remedy without a plugin name
   rather than guessing one. T10's capture records exactly which of those
   fields the observed plugins actually set.
4. **C4, "copy routes through the existing clipboard path, including its
   no-system-clipboard notice" — that notice does not exist yet.** The
   clipboard worker (`crates/view/src/clipboard.rs`) falls back to shadow
   registers and records the reason to `VIEW_LOG` only; the one
   user-visible message lives in `--print-clipboard`'s stderr path
   (`print_clipboard`'s no-system-clipboard line in `crates/view/src/main.rs`), which never runs in a session. T18
   therefore *builds* that notice (through the existing
   `record_native_notice_once` family mechanism) as part of the copy path,
   rather than wiring to something that is not there.
5. **C4, `Effect::ClipboardWrite` requires a `ReplyToken`.** A
   view-initiated copy answers no engine request. Rather than a second
   copy path (which would duplicate the shadow-register and notice logic),
   T18 makes the field `Option<ReplyToken>` and the worker skips the reply
   when it is `None` — one copy path, one notice, one place the OSC 52
   companion is emitted.

---

## Coverage walk (planning protocol step 0)

| Charter deliverable | Task(s) |
|---|---|
| C1 truecolor probed, `COLORTERM` demoted to a hint | T3, T4 |
| C1 borders decoupled from tier, ASCII as the honest fallback | T2, T4, T6 |
| C1 caps line to `VIEW_LOG`, interactive only behind a flag/override | T1 |
| C1 "per capability, the authoritative probe and its fallback, so a fourth instance is unrepresentable" | T5 |
| C2 shape A resolved when the user opts out (fork settled at plan time) | T8, T9 |
| C2 shape A's *defaults* half — the charter's "one explanatory notice" for the out-of-box case, which option 2 does not supply on its own | T19 |
| C2 shape B detected generically | T10, T11 |
| C2 shape B resolved for the observed case (cmp-cmdline) | T12 |
| C2 one clear notice naming plugin, surface, remedy | T11 |
| C2 surface-ownership matrix, with coverage gaps visible | T13 |
| C3 unaccommodated first-class state, adjusted state retained | T7 |
| C3 binding rule enforced by mechanism, not reviewer habit | T7 |
| C3 nvim-cmp's untested cmdline path | T15 |
| C4 slot-scoped dismissal timer | T14 |
| C4 slide-up / exit-right motion, damage-driven | T16 |
| C4 pause hotkey | T17 |
| C4 scrollable history with per-entry copy through the clipboard path | T18 |

---

## As-built interfaces this plan builds on

- `view_tui::tiers::{detect, resolve, caps_for_override, PROBE_DEADLINE}`;
  `QUERY_SYNC`/`QUERY_KITTY`/`QUERY_DA1_FENCE`; `ReplySource` (the
  unit-testable seam, with `ScriptedSource` in its tests);
  `scan_csi_replies` returning `(sync, kitty, da1, residue)`;
  `truecolor_from_colorterm` accepting exactly `truecolor`/`24bit`;
  `log_caps` (`eprint!`, called unconditionally from `resolve`).
- `view_core::model::TermCaps { tier, sync, truecolor, kitty_kbd }` and
  `TermCaps::from_probe(sync, truecolor, kitty_kbd)` — the single place
  the tier formula lives; `Tier::{Full, Standard, Basic}`, `#[non_exhaustive]`.
- `view_tui::terminal::Term::init(tier_override)` in `terminal.rs`,
  calling `tiers::resolve` between raw mode and the alternate screen;
  `Term::caps()`, `Term::take_residue()`.
- `crates/view/src/main.rs` — `Term::init`, then `model.caps =
  term.caps()`, then the existing `vlog::log_with("startup", …)` line that
  already records `caps tier=… sync=… truecolor=… kitty_kbd=…`.
- `view_surface::overlay::BorderSet::{ROUNDED, PLAIN, ASCII, for_tier}`
  (in `overlay.rs`), consumed by `overlay::rows(width, height, kind,
  borders)` and by `view_surface::lib`'s layer construction via
  `model.caps.tier`.
- `view_engine::nvim_api`: the `ext_*` list `UI_EXT_OPTIONS`
  (`ext_linegrid`, `ext_cmdline`, `ext_popupmenu`, `ext_messages`,
  `ext_tabline`), `EngineHandle::ui_attach`/`ui_attach_with_stdin_relay`,
  `RegisterBridge` and the `view_bridge` autocmd chunk
  (`REGISTER_BRIDGE_CHUNK`) with its `rpcnotify(channel, 'view_bridge', …)`
  occurrence-count test.
- `view_native::supersede::{Supersession, plan, TAKEOVERS}` — one row
  today (`statusline` → `laststatus=0`), rendered as
  `RpcCall::HoldOption` by `takeover_call`; `view_core::native::registry::FEATURES`
  (5 rows) and `mappings::DEFAULT_MAPS` (6 rows, regenerated into
  `docs/keymaps.md` with a drift test).
- `crates/view/src/native.rs` `NativeSession::load(config_path,
  channel_id, &mut model)` in `main.rs` — **after** attach today.
- `view_core::model::Messages` (`entries`, `flush_generation`,
  `next_message_id`, `push`, `clear`, `push_native`,
  `set_native_condition`, `visible_lines(max_rows)`);
  `EngineModel::record_message`/`record_native_notice`/`record_native_notice_once`;
  `view_core::native::toast::{route, Route, timeout_for,
  TRANSIENT_TOAST_TIMEOUT (4 s), ToastHistory, DEFAULT_CAPACITY (200)}`;
  `Effect::ScheduleToastExpiry { id, after }` → `Msg::ToastExpired { id }`.
- `OverlayKind::MessageHistory(MessageHistoryState)` (in `model.rs`),
  opened by `update::open_message_history` (in `update/mod.rs`),
  rendered as `LayerKind::Palette(state.view())` (in `lib.rs`), with **no
  navigation state of its own** — `<Esc>` closes it through the generic
  fallback.
- `view_surface::lib`'s single `LayerKind::Messages(Vec<Vec<Span>>)` layer,
  top-right anchored, sized from `visible_lines(grid_h - 2)`
  (in `lib.rs`), painted by `view_tui::paint::paint_messages`
  (in `paint.rs`).
- `Effect::{ClipboardWrite { token, register, lines, regtype }, Osc52Copy {
  register, lines, regtype }}`, emitted as a pair from
  `update/mod.rs`'s `EngineRequest::ClipboardSet` arm; `crates/view/src/osc52.rs`
  (`Osc52Sink`, `drain_osc52`, `OSC52_MAX_PAYLOAD_BYTES`);
  `crates/view/src/clipboard.rs` (`ClipboardJobKind`, `ReplyRoute`, shadow
  registers).
- `view_harness::scenario`: `RawScenario { schema, plugin, class, fixture,
  cold_bootstrap, states }`, `RawState { name, native, fixture, steps }`,
  `REQUIRED_UI_OWNING_STATES = ["superseded","deferred","native-only"]`,
  `ScenarioError::{IncompleteUiOwningStates, UnsupportedState, …}`;
  `view_oracle::compat::{ScenarioState, parse_state_name, state_name}`.
- `crates/view-harness/src/bin/oracle/compat.rs` — fixture resolution,
  hermetic `XDG_*`, its `cmd.env(...)` sweep; the fixture already reads
  `vim.env.VIEW_COMPAT_SOCK` (its `vim.fn.serverstart(vim.env.VIEW_COMPAT_SOCK)` line),
  which is the proof a `VIEW_COMPAT_*` variable reaches the nvim
  grandchild through `make_hermetic`'s sweep.
- §3.1 budget rows that cover this phase:
  `first_paint/shell_visible_cold_ms` (the startup probe), `output_path/p99_ms`
  and `echo/{ratio_p50, view_p99_ms}` (paint), `flood/cadence_p99_ms`.

---

## Task order and dependencies

```
T1 ─┐                                  (cheap independents, no deps)
T2 ─┤
T7 ─┘
T3 (capture) ──► T4 ──► T5
                  └───► T6            (border charset onto the probed bit)
T8 ──► T9 ─────────────────┐
   └───────────────────────┤       (T19 needs both: T8 owns which surfaces
T10 (capture) ──► T11 ─────┤        view attached, T9 owns the notify path)
                    │      ▼
                    │     T19 ──► T14 ──► T16 ──► T17 ──► T18
                    └──► T12 ──► T13
                           └──► T15 (nvim-cmp cmdline state; red until T12)
```

T7 lands before the C2 tasks (charter C3: "should land with or before C2,
since C2's matrix asserts through this mechanism"). C4 (T14–T18) starts
only after T11, per C4's own start gate — until the conflict detector
exists, view cannot tell which toasts are its own. T19 sits at that same
seam and is the first consumer of the answer: it is what makes "which
toasts are its own" a decision the *user* sees, and it is the task the
noice `unaccommodated` state turns green on.

**Which task turns which noice state green** (the red→green story T7 makes
a point of, stated once here so no task has to imply it):

| Scenario state | `[native]` it runs under | Red until | Green because |
|---|---|---|---|
| `unaccommodated` | defaults (everything on) | **T19** | view emits one conflict notice; noice's own three errors are in history, not the stack |
| `deferred` | `palette = false, notifications = false` | **T8** | the exts are not attached, so noice's check finds nothing to complain about |
| `superseded`, `native-only` | unchanged from today | — | already green |

---

### Task 1: The capability line becomes a diagnostic (#10)

**Files:**
- Modify: `crates/view-tui/src/tiers.rs` (delete `log_caps`; `resolve`
  returns the source label), `crates/view-tui/src/terminal.rs` (`Term`
  carries the label; `Term::caps_source()`), `crates/view/src/main.rs`
  (Cli gains `--print-caps`; the existing `vlog` startup line gains the
  label; the interactive print), `docs/` — none.

**Consumer call-site first:**

```rust
// main.rs, unchanged position (immediately after `model.caps = term.caps()`)
vlog::log_with("startup", || format!(
    "version={VERSION} caps tier={:?} sync={} truecolor={} kitty_kbd={} \
     unicode_boxes={} source={} term={width}x{height}",
    /* … */ term.caps_source()));
if cli.print_caps || cli.tier.is_some() {
    term.print_caps_notice()?;   // one line, inside the alt screen, as a toast-free write
}
```

**Interfaces:** `tiers::resolve` stops writing to stderr and returns
`(TermCaps, Vec<u8>, CapsSource)` where `CapsSource` is a
`#[non_exhaustive]` enum `{ Probed, Assumed, Override }` with a `label()`
— the `"probed"`/`"assumed"`/`"--tier override"` strings move onto it
verbatim from today's `PROBE_SOURCE_LABEL`. The interactive print is a
`record_native_notice` toast (it lands after the alternate screen is up,
where a stderr write is invisible — the reason `startup.rs`'s `paint_shell_frame` doc comment gives for
routing its own line through `vlog`), not an `eprint!`.

**What the new tests prove:**
`caps_source_labels_match_the_probe_arm_taken` (override vs probed vs
assumed); `print_caps_flag_emits_exactly_one_notice`;
`print_caps_is_silent_without_the_flag`.

That "`resolve` writes to stderr at all" is the defect, and no unit test in
`view-tui` can honestly prove its absence: stderr is a process-global fd, a
parallel test may not capture it, and the new signature threads no writer
(threading one purely to test it would be inventing surface to test
surface). The proof is structural and end-to-end instead — `log_caps` and
`PROBE_SOURCE_LABEL` are deleted, which the compiler enforces for every
call site, and step 5's pty smoke test observes the real fd of a real
launch. That is the honest pairing, and it is stated here rather than
papered over with a unit test that asserts on a sink nobody writes to.

**Falsifiable check:** delete the `if cli.print_caps || cli.tier.is_some()`
guard so the notice is unconditional →
`print_caps_is_silent_without_the_flag` fails by finding a message entry on
a plain launch. Independently: reintroduce any `eprint!` in `resolve` →
step 5's pty smoke test fails with bytes on stderr.

**Dependencies:** none.

- [ ] **Step 1: Failing test.** `print_caps_is_silent_without_the_flag` —
  a plain launch records no caps notice; red today only after step 4 exists,
  so pair it with `caps_source_labels_match_the_probe_arm_taken`, which is
  red immediately (no `CapsSource` exists).
- [ ] **Step 2:** Delete `log_caps` and `PROBE_SOURCE_LABEL`; introduce
  `CapsSource` and thread it through `resolve` → `Term::init` →
  `Term::caps_source()`.
- [ ] **Step 3:** Extend `main.rs`'s existing `"startup"` vlog line with
  `source=`; add `--print-caps` (long only, documented in `--help` — this
  one is user-facing, unlike `--print-clipboard`'s hidden flag).
- [ ] **Step 4:** The interactive path emits one `record_native_notice`
  through `pre_executor_effects`, the same buffer `main.rs` already uses
  for pre-loop notices.
- [ ] **Step 5: Disconfirm.** Launch under a pty with `--tier basic` and no
  `VIEW_LOG`: the smoke test asserts nothing is written to the process's
  stderr at any point before exit (today's line lands there and surfaces on
  alt-screen teardown — that is the observed defect).
- [ ] **Step 6:** `task ci`. Commit: `fix(tui): the capability line lands in
  VIEW_LOG instead of on the screen at quit`.

---

### Task 2: The border charset stops asking a color question (#11)

**Files:**
- Modify: `crates/view-surface/src/overlay.rs`,
  `crates/view-surface/src/overlay/tests.rs`, any golden snapshots that
  depict a `Tier::Standard` frame.

**Interfaces:** `BorderSet::PLAIN` is deleted. `BorderSet::for_tier`
becomes `Tier::Full | Tier::Standard => ROUNDED`, `Tier::Basic => ASCII`,
`_ => ASCII`, matching spec §7.1 verbatim ("Rounded corners on `full` and
`standard` — corner glyphs are font coverage, not a terminal capability;
`basic` falls back to ASCII"). This task is pure spec conformance and
deliberately does *not* wait for the probe: T6 re-points the same function
at the probed bit once T4 lands.

**What the new tests prove:**
`standard_tier_draws_the_same_frame_as_full` (the corner glyphs are
identical; the sets differ in nothing else, which is the argument the
module's own `LINE_H`/`LINE_V` comment already makes);
`basic_tier_draws_ascii`.

**Falsifiable check:** re-add `Tier::Standard => PLAIN` →
`standard_tier_draws_the_same_frame_as_full` fails naming `┌` where `╭` is
expected.

**Dependencies:** none. **Perf:** touches paint — no per-frame work
changes (a `match` arm returns a different `const`); no budget row moves;
covered by `output_path/p99_ms` if it ever did.

- [ ] **Step 1: Failing test** for `standard_tier_draws_the_same_frame_as_full`.
- [ ] **Step 2:** Delete `PLAIN`, re-point `for_tier`, update its doc
  comment (the current one argues tier is the right predicate; A3 makes
  that false — the replacement states the probe is).
- [ ] **Step 3:** Re-record affected tier goldens; confirm no golden's
  *layout* moved (the sets are the same width by construction).
- [ ] **Step 4: Disconfirm.** Grep the tree for `PLAIN` — no consumer
  remains; `task loc`/`task ci` green.
- [ ] **Step 5:** `task ci`. Commit: `fix(surface): rounded borders no
  longer depend on a terminal's color depth`.

---

### Task 3: Wire capture — terminal probe replies (#12)

**Files:**
- Create: `docs/terminal-probe-wire-capture.md`
- Create: `scripts/acceptance/capture-terminal-probe.sh` (the capture
  harness itself: writes the batch, dumps the raw reply bytes hex-escaped)

**Why a capture task at all:** T4 consumes two escape grammars this repo
has never spoken — DECRQSS SGR readback and cursor-position reporting —
and the Global Constraint is that no task embeds a wire string no capture
doc records. Recall is not evidence, and the truecolor readback in
particular differs across terminals in the separator it echoes (`;` vs
`:`), which is exactly the class of detail a plan cannot invent.

**Interfaces:** the doc records, per terminal, the exact bytes sent and the
exact bytes received, hex-escaped, with the terminal's identity
(`$TERM`, version banner, DA1 reply) beside them:

1. **Truecolor:** `ESC [ 48 ; 2 ; 1 ; 2 ; 3 m` then `ESC P $ q m ESC \`;
   record the DCS reply verbatim. Truecolor is "supported" when the reply
   preserves the exact triple; a terminal that quantizes answers with a
   different SGR.
2. **Box glyphs:** `\r`, one `╭`, `ESC [ 6 n`, `\r ESC [ K`; record the CPR
   reply and therefore the column the glyph advanced to.
3. Both, batched ahead of the existing `ESC [ c` DA1 fence, to confirm the
   fence still answers last (the property `detect`'s early break depends
   on).
4. **What a keystroke looks like next to a CPR reply.** The residue
   contract today rests on "a keyboard cannot produce `ESC [ ?`"
   (`scan_csi_replies`'s doc comment in `tiers.rs`), which is true of the private-CSI arms and of the
   DCS arm — but the CPR reply `ESC [ Pr ; Pc R` shares its grammar with
   xterm's modified-F3 encoding `ESC [ 1 ; m R`. Capture, on each host: a
   plain `F3`, a `Shift-F3`, and a `Ctrl-F3` press, verbatim; and the CPR
   reply from a cursor parked at a column other than 1, so the two are on
   the page beside each other. This is what lets T4 encode an accepted
   ambiguity instead of a false absolute.

**Hosts:** dev-linux (xterm, tmux inside it, and a Termius SSH session from
the iPad — the configuration that produced the finding), mbp (Terminal.app,
kitty or ghostty if installed), winserver (Windows Terminal over ConPTY).
Each host's section names the host and the terminal; an unavailable
terminal is recorded as unavailable, never assumed.

**Falsifiable check:** the doc is not evidence unless it distinguishes.
Include one terminal that answers DECRQSS *and* one that ignores it, and
one confirmed non-truecolor terminal (`TERM=xterm-256color` with a
quantizing emulator, or `tmux` without `Tc`), and show their replies
differ. If every captured terminal answers identically, the probe cannot be
shown to discriminate and T4 must not proceed on it.

**Dependencies:** none. **Perf:** none (documentation + a script).

- [ ] **Step 1:** Write the capture script (raw-mode, VMIN/VTIME bounded,
  hex-dump output) and run it on dev-linux under xterm.
- [ ] **Step 2:** Run it under tmux and under the Termius SSH session;
  record `COLORTERM`'s presence/absence beside each.
- [ ] **Step 3:** Run it on mbp and winserver (`ssh mbp 'zsh -lc …'` /
  `winps.sh` — the shells that see the real PATH).
- [ ] **Step 4:** Write the doc: per-capture "sent / received / read as",
  plus a "what a terminal that does not support this answers" section.
- [ ] **Step 5:** Commit: `docs(tui): capture the truecolor and box-glyph
  probe replies from real terminals`.

---

### Task 4: Truecolor and box glyphs are probed (#13)

**Files:**
- Modify: `crates/view-tui/src/tiers.rs`, `crates/view-core/src/model.rs`
  (`TermCaps` gains `unicode_boxes`; `from_probe` gains a parameter),
  `crates/view-tui/src/paint.rs` (caps construction in tests),
  `crates/view-surface/src/lib.rs` (no behavior change; construction sites).

**Consumer call-site first:**

```rust
// tiers::detect — one batch, DA1 still the fence
writer.write_all(QUERY_SYNC)?;
writer.write_all(QUERY_KITTY)?;
writer.write_all(QUERY_TRUECOLOR)?;   // SGR set + DECRQSS readback (T3 §1)
writer.write_all(QUERY_BOX_GLYPH)?;   // glyph + CPR + erase   (T3 §2)
writer.write_all(QUERY_DA1_FENCE)?;
// …
let probed = scan_replies(&buf);
let truecolor = probed.truecolor.unwrap_or_else(|| truecolor_hint(colorterm));
```

**Interfaces:** `scan_csi_replies` becomes `scan_replies` returning a
`ProbeReplies { sync: bool, kitty: bool, truecolor: Option<bool>,
unicode_boxes: Option<bool>, da1: bool, residue: Vec<u8> }` — `Option`
because "the terminal did not answer" and "the terminal answered no" are
different facts, and only the first may fall back to the hint. It gains a
DCS arm (`ESC P … ESC \`) alongside today's private-CSI arm, and a CPR arm
(`ESC [ <row> ; <col> R`); the residue contract is unchanged and its
existing tests must still pass verbatim — a keystroke in the middle of the
probe window still survives to `residue`.

**The CPR arm's accepted ambiguity, stated rather than assumed.** The two
new arms are not equally safe. A DCS reply is unambiguously the terminal's:
no key produces `ESC P`, exactly as no key produces `ESC [ ?`. The CPR
reply is not — `ESC [ Pr ; Pc R` is also how xterm encodes a modified F3
(`ESC [ 1 ; m R`), the documented CPR/F3 collision, so the CPR arm can in
principle swallow a modified-F3 press. This plan takes that trade knowingly
and bounds it three ways: the arm is live only inside the `PROBE_DEADLINE`
window (50 ms, at startup, before the first frame); it consumes at most one
CPR match and stops matching after it; and the modified-F3 byte sequences
it could collide with sit on the capture page (T3 §4) beside the real
replies, so the tests encode the accepted trade rather than a grammar claim
that is false. A user who presses `Shift-F3` inside the first 50 ms of a
launch loses that keypress; nothing else about the residue contract
changes, and the alternative — no box-glyph probe at all — costs every SSH
session its chrome.

`truecolor_from_colorterm` is renamed `truecolor_hint` and demoted: it is
consulted only when the readback did not answer, and its doc says so. The
tier formula in `TermCaps::from_probe` is unchanged — this task changes
where `truecolor` comes from, not what it gates.

**What the new tests prove:**
`a_ssh_session_without_colorterm_still_probes_truecolor` (the observed
defect, as a scripted `ReplySource` replaying T3's captured bytes with
`colorterm: None`) — this is the regression test of record for the whole
charter; `a_quantizing_terminal_reports_no_truecolor_despite_colorterm`
(the hint must not override a negative answer);
`an_unanswered_readback_falls_back_to_the_colorterm_hint`;
`box_glyph_cpr_of_column_two_reads_as_supported` /
`…column_three_reads_as_unsupported`;
`a_keystroke_typed_during_the_dcs_reply_is_still_returned_as_residue`;
`a_modified_f3_after_the_cpr_reply_survives_as_residue` (the arm consumes
one CPR match and no more, so the ambiguity's blast radius is exactly one
sequence, not every `…R` in the window — the bytes come from T3 §4).
Every scripted byte string in these tests is copied from
`docs/terminal-probe-wire-capture.md` with a comment naming the capture
section, never hand-written.

**Falsifiable check:** delete the DCS arm from `scan_replies` so
`truecolor` stays `None` → `a_ssh_session_without_colorterm_still_probes_truecolor`
fails with `Tier::Basic` where `Tier::Full`/`Standard` is expected. Mutate
the CPR arm to read the row instead of the column → the two box-glyph tests
fail.

**Dependencies:** T3. **Perf:** startup only. Two extra queries in the
existing single batched write; the read loop still breaks on the DA1 fence,
so a terminal that answers everything costs the extra reply bytes and
nothing else, and a terminal that answers nothing still costs exactly
`PROBE_DEADLINE` (50 ms, unchanged). Covered by
`first_paint/shell_visible_cold_ms` (p99 ≤ 50 ms cold, recorded 4.08 ms);
the task's own acceptance is that row re-measured on dev-linux, not
merely a green unit test.

- [ ] **Step 1: Failing test.** `a_ssh_session_without_colorterm_still_probes_truecolor`
  against a `ScriptedSource` replaying the captured reply bytes.
- [ ] **Step 2:** `ProbeReplies` + the DCS and CPR arms; keep every
  existing residue test passing untouched.
- [ ] **Step 3:** `TermCaps.unicode_boxes` + `from_probe`'s new parameter;
  `caps_for_override` sets it (`full`/`standard` ⇒ true, `basic` ⇒ false —
  an override is a claim about the terminal, and the two Unicode tiers
  claim glyphs).
- [ ] **Step 4:** Demote `truecolor_hint`; the fallback is
  `Option::unwrap_or_else`, so a negative answer can never be overridden.
- [ ] **Step 5: Disconfirm.** Run the built binary over the real SSH
  session from T3's capture with `VIEW_LOG` set; the startup line reads
  `truecolor=true tier=Full|Standard`. Then run under a terminal captured
  as non-truecolor and confirm `truecolor=false`. Both hosts named in the
  commit message.
- [ ] **Step 6:** `task bench -- --scenario first_paint --fixture minimal`
  on dev-linux; state `shell_visible_cold_ms` before/after in the commit.
- [ ] **Step 7:** `task ci`. Commit: `feat(tui): truecolor and box-drawing
  support are probed instead of inferred from the environment`.

---

### Task 5: The capability register (#14)

**Files:**
- Create: `crates/view-tui/src/tiers/register.rs` (or a `register` section
  in `tiers.rs` if it stays under the ceiling — check `task loc` first;
  `tiers.rs` is 716 lines today, and T4 grows it)
- Modify: `crates/view-tui/src/tiers.rs`, `docs/performance.md` — none;
  the register's prose home is the spec (amendment A1).

**Interfaces:** a `static REGISTER: [CapabilityRow; 4]` of
`{ name: &'static str, probe: ProbeKind, hint: Option<&'static str>,
absent_is: bool }`, plus `pub fn register() -> &'static [CapabilityRow]`.
This is data, not a `match`, for the same reason `supersede::TAKEOVERS` is
data: the set has to be *enumerable* for the drift check to walk it.

**What the new tests prove:**
`every_termcaps_field_has_a_register_row` — the mechanism the charter asks
for ("so a fourth instance is unrepresentable rather than merely absent").
It walks the register against the field names of a `TermCaps` value
rendered with `{:?}` (the debug shape is stable and `#[non_exhaustive]`
blocks struct-literal construction elsewhere, so this is the honest
enumeration available in-crate), failing with the name of any field that
has no row and any row that names no field.
`no_row_lists_an_env_var_as_its_probe` — the register's central claim: a
hint is never a probe.

**Falsifiable check:** add a fifth `bool` to `TermCaps` without a row →
`every_termcaps_field_has_a_register_row` fails naming the new field. Move
`COLORTERM` from `hint` to `probe` on the truecolor row →
`no_row_lists_an_env_var_as_its_probe` fails.

**Dependencies:** T4. **Perf:** none (static data + tests).

- [ ] **Step 1: Failing test** with the register absent.
- [ ] **Step 2:** The register table and its accessor; each row's doc
  comment cites the capture-doc section that pins its probe.
- [ ] **Step 3:** `--print-caps` (T1) renders from the register, so the
  user-visible listing and the enforced table are one thing. This is the
  one back-edge in the graph: T1 ships the flag printing a plain line
  because the register does not exist yet, and this step re-points it. T1
  is not blocked on T5 and T5 does not re-open T1's tests — the flag's
  behavior (one notice, only when asked) is unchanged; only its text
  gains rows.
- [ ] **Step 4: Disconfirm.** Temporarily add a `bool` to `TermCaps`;
  confirm the drift test fails; revert.
- [ ] **Step 5:** `task ci`. Commit: `feat(tui): every probed capability
  names its own probe, or the build fails`.

---

### Task 6: The border charset follows the probed bit (#11)

**Files:**
- Modify: `crates/view-surface/src/overlay.rs`,
  `crates/view-surface/src/lib.rs` (layer construction passes `caps`, not
  `caps.tier`), `crates/view-tui/src/paint.rs` (test constructions).

**Interfaces:** `BorderSet::for_tier(tier)` → `BorderSet::for_caps(caps:
TermCaps)`: `caps.unicode_boxes` ⇒ `ROUNDED`, else `ASCII`. `for_tier` is
deleted rather than kept as a wrapper — a second entry point is a second
answer.

**What the new tests prove:**
`a_basic_tier_terminal_that_draws_box_glyphs_gets_rounded_corners` (the
decoupling, stated as the case the old code got wrong in the other
direction); `a_full_tier_terminal_without_box_glyphs_gets_ascii`.

**Falsifiable check:** re-point `for_caps` at `caps.tier` → both tests
fail.

**Dependencies:** T2, T4. **Perf:** paint; per-frame cost unchanged (one
`bool` read replaces one enum match).

- [ ] **Step 1: Failing tests** for both directions.
- [ ] **Step 2:** `for_caps`; thread `TermCaps` through the two layer
  construction sites in `view-surface/src/lib.rs` that pass `model.caps.tier`
  today.
- [ ] **Step 3:** Re-record tier goldens; a `basic`-tier golden now depends
  on the fixture's `unicode_boxes` value, so the golden's fixture states it
  explicitly.
- [ ] **Step 4: Disconfirm.** Launch with `--tier basic` under a
  box-drawing terminal and confirm ASCII (an override is a claim, and
  `caps_for_override` says basic ⇒ no glyphs) — the one place tier still
  legitimately decides, because the user said so.
- [ ] **Step 5:** `task ci`. Commit: `feat(surface): the border charset
  follows the box-glyph probe, not the tier`.

---

### Task 7: The compat suite can fail on a migration defect (#15)

**Files:**
- Modify: `crates/view-oracle/src/compat.rs` (`ScenarioState`,
  `parse_state_name`, `state_name`), `crates/view-harness/src/scenario.rs`
  (`REQUIRED_UI_OWNING_STATES`, error text),
  `crates/view-harness/src/bin/oracle/compat.rs` (the accommodation switch
  on the child env), `compat/fixtures/heavy/nvim/init.lua` (every
  accommodation moves behind the switch),
  `compat/scenarios/{noice,nvim-notify,lualine,nvim-tree,neo-tree,dressing,fidget}.toml`
  (a new `unaccommodated` state each)
- Create: `scripts/check-compat-accommodations.sh` (style-gate leg)

**Interfaces:**

Every step below uses the schema as it exists (`send` / `wait_for` /
`wait_for_cell` / `assert_absent` / `assert_cell_not` / `probe` /
`wait_for_probe`; `RawStep` in `scenario.rs`) — there is no `assert_present`
step and none is invented; the positive assertion is `wait_for`.

A state says two things explicitly, and neither is inferred from its name:
which `[native]` config it runs under, and whether it wants the fixture's
accommodations. `RawState` already carries `native`
(`RawState` in `scenario.rs` — the `deferred` state uses it today); it gains
`accommodations: Option<bool>`, defaulting to `true`.

```toml
# compat/scenarios/noice.toml — every ui-owning state, spelled out

[[states]]
name = "unaccommodated"          # the first launch of record
accommodations = false           # nothing pre-seeded, no component disabled
native = {}                      # view's defaults: every feature on, full ext set
steps = [
  { wait_for_probe = "luaeval('package.loaded.noice ~= nil')", expect = "true", timeout_ms = 180000 },
  # What a defaults user must see: view's one notice, carrying the remedy.
  { wait_for = "noice.nvim is using the command line" },
  { wait_for = "[native] palette = false" },
  # And what they must NOT see: the plugin's own three errors on the stack.
  { assert_absent = "Noice can't work when the GUI has" },
  # Not discarded, though -- one key away, verbatim.
  { send = "<leader>fm" },
  { wait_for = "Noice can't work when the GUI has" },
]

[[states]]
name = "deferred"                # the remedy, proven to work
accommodations = false           # the remedy must hold on an unadjusted config
native = { palette = false, notifications = false }
steps = [
  { wait_for_probe = "luaeval('require(\"noice.config\").is_running()')", expect = "true", timeout_ms = 180000 },
  { assert_absent = "Noice can't work when the GUI has" },
  { assert_absent = "noice.nvim is using the command line" },
]
```

```lua
-- compat/fixtures/heavy/nvim/init.lua
local accommodate = vim.env.VIEW_COMPAT_ACCOMMODATIONS ~= "0"
-- view-compat-accommodation: noice health.check ext_* errors (#1137)
if accommodate then ... end
```

The runner sets `VIEW_COMPAT_ACCOMMODATIONS=0` for, and only for, a state
whose `accommodations` is `false`; the variable rides the same path
`VIEW_COMPAT_SOCK` already proves reaches the nvim grandchild through
`make_hermetic`'s sweep (the sweep unsets a variable only when its value
still equals the host's, and the host exports neither).

**Why the switch is a field and not the state's name.** Keying the switch
on `name == "unaccommodated"` would make exactly one state per scenario
capable of proving anything about an unadjusted config — and the remedy
(`deferred`) is the state that most needs to. A field says what it means,
lets `deferred` run unadjusted too, and leaves room for a future state to
do the same without renaming itself.

Three enforcement legs, because the charter asks for a mechanism rather
than a reviewer's habit:
1. `REQUIRED_UI_OWNING_STATES` gains `"unaccommodated"` — a ui-owning
   scenario missing it fails to *load*, in the loader that already fails
   for the other three.
2. `scripts/check-compat-accommodations.sh` (added to `task style`'s chain)
   fails when a `-- view-compat-accommodation:` marker in any fixture is
   not inside an `if accommodate` block, and when a fixture reaches a
   plugin private (`_once`, `__`, `package.loaded[...]` assignment) with no
   marker above it.
3. `every_ui_owning_scenario_declares_an_unaccommodated_state` — a unit
   test over the real `compat/scenarios/` directory, so the rule holds for
   the files on disk and not merely for a hand-built fixture. It asserts
   the state's *shape* too: `accommodations = false`, and no `[native]`
   override that turns a feature off. An `unaccommodated` state that quietly
   opts out of the surfaces is the same suppression in a new place, and this
   is the leg that makes it unrepresentable.

**What the new tests prove:** the three legs above, plus
`an_unaccommodated_state_clears_the_accommodation_env` in the runner
(the switch follows the field, not the name),
`a_state_that_asks_for_accommodations_keeps_them` (the default is `true`,
so the four existing states are unchanged), and
`parse_state_name_round_trips_all_five_states` (present, unaccommodated,
superseded, deferred, native-only).

**Falsifiable check:** delete the `unaccommodated` state from
`compat/scenarios/noice.toml` → the loader test names it and `task compat`
refuses to run the file. Give that state `native = { palette = false }` →
`every_ui_owning_scenario_declares_an_unaccommodated_state` fails naming
the override, which is the leg that keeps the defaults case honest. Move
noice's `_once` pre-seed back outside the `if accommodate` block → the
style script fails naming the line. Set the env var unconditionally →
`an_unaccommodated_state_clears_the_accommodation_env` fails.

**Dependencies:** none (lands before the C2 tasks that assert through it).
**Perf:** none.

- [ ] **Step 1: Failing tests** for legs 1 and 3 (before any scenario file
  gains the state, so the red is the real coverage gap).
- [ ] **Step 2:** `ScenarioState::Unaccommodated` + the loader requirement
  + error text; `state_name` round-trip.
- [ ] **Step 3:** `RawState.accommodations` (default `true`) + the runner's
  env switch driven by it.
- [ ] **Step 4:** Gate every accommodation in the heavy fixture behind
  `accommodate`, each with its marker comment; the existing prose (which
  is good, and cites #1137) stays.
- [ ] **Step 5:** The style script + its `task style` wiring, with a
  negative fixture under `scripts/test-fixtures/`.
- [ ] **Step 6:** Add the `unaccommodated` state to every ui-owning
  scenario, each with its `[native]` spelled out, and set
  `accommodations = false` on noice's `deferred` state. **Expect red, and
  expect it in two places for two different reasons** (the table under
  "Task order and dependencies" is the record):
  - `unaccommodated` (defaults) is red until **T19** — it asserts view's
    one conflict notice, which does not exist yet, *and* the absence of
    noice's three errors from the stack.
  - `deferred` (the remedy) is red until **T8** — with accommodations off
    the exts are still attached today regardless of `[native]`, which is
    exactly the defect T8 fixes.

  Neither assertion is weakened to buy green. A suite that is green here is
  a suite that cannot fail, which is the whole finding.
- [ ] **Step 7:** `task compat -- compat/scenarios/noice.toml`; record both
  failures in the commit body as the evidence of record that the suite now
  detects the defect it previously suppressed, and name the task that
  clears each.
- [ ] **Step 8:** `task ci`. Commit: `feat(compat): every ui-owning
  scenario proves the config a user actually starts with`.

---

### Task 8: The externalized surface set follows `[native]` (#16)

**Files:**
- Modify: `crates/view/src/main.rs` (config resolved before spawn/attach),
  `crates/view/src/startup.rs` (`spawn_and_attach` takes the ext set),
  `crates/view/src/native.rs` (`NativeSession::load` accepts an
  already-resolved `ViewConfig` instead of re-reading it),
  `crates/view-engine/src/nvim_api.rs` (`ui_attach`/`ui_attach_with_stdin_relay`
  take the ext set; `UI_EXT_OPTIONS` becomes a function of it),
  `crates/view-native/src/config.rs` (a `ext_surfaces(&NativeConfig)`
  mapping), `compat/scenarios/noice.toml` (the `deferred` state's
  assertions), `docs/compat.md`
- Spec: amendment A4 + A7 land first, in their own commit.

**Consumer call-site first:**

```rust
// main.rs, before the engine is spawned
let resolved = ViewConfig::load(config_path.as_deref()).unwrap_or_else(|e| { /* notice, defaults */ });
let exts = view_native::config::ext_surfaces(&resolved.native);   // &[&str]
let engine = startup::spawn_and_attach(cfg, width, height, exts)?;
```

```rust
// view-native/src/config.rs
pub fn ext_surfaces(cfg: &NativeConfig) -> Vec<&'static str> {
    let mut v = vec!["ext_linegrid", "ext_tabline"];
    if cfg.enabled("palette")       { v.push("ext_cmdline"); v.push("ext_popupmenu"); }
    if cfg.enabled("notifications") { v.push("ext_messages"); }
    v
}
```

**Interfaces:** the ordering change is the substance of the task. Today
`NativeSession::load` runs in `main.rs`, after attach, and
`native.rs`'s "`ui_attach` already ran, at the raw terminal height, before this config was even read" comment documents the consequence it already works around. After
this task the config is read once, before spawn, and handed to both. A
config that cannot be read still falls back to the full experience with a
notice (unchanged contract) — which means every ext attached, the current
behavior. `view --clean` skips config entirely and therefore attaches
everything, also unchanged.

`ext_linegrid` is unconditional (it is the grid protocol). `ext_tabline`
stays unconditional and is recorded as such in T13's matrix — no native
feature owns the tabline, so there is no switch to follow, and inventing
one is not this charter's business.

**What the new tests prove:**
`palette_off_attaches_no_cmdline_ext` / `notifications_off_attaches_no_messages_ext`
(unit, over `ext_surfaces`); `ui_attach_sends_exactly_the_requested_exts`
(over the `nvim_api` request encoder);
`an_unreadable_config_attaches_every_ext` (fail-open, matching the loader's
own documented contract); and a live compat assertion in the noice
`deferred` state that `cmdheight` is no longer forced to 0 — the scenario
file's own comment today says "`cmdheight` stays 0 in every state
regardless — `ext_messages` attaches at the session level, not per
feature", which is precisely the sentence this task falsifies, so the
comment is rewritten rather than left as documentation of a fixed defect.

**Falsifiable check:** hardcode the full ext list in `ext_surfaces` →
`palette_off_attaches_no_cmdline_ext` fails, and the noice `deferred`
compat state fails on `cmdheight`.

**Dependencies:** spec A4/A7; T7 (so noice's `deferred` state exists,
running unaccommodated, to turn green here — the defaults case is T19's).
**Perf:** touches startup, not key dispatch/grid
apply/paint; one file read moves earlier. State it against
`first_paint/shell_visible_cold_ms` — the read now precedes the shell
frame, so if it costs anything, that row is where it shows.

- [ ] **Step 1: Failing test** `notifications_off_attaches_no_messages_ext`.
- [ ] **Step 2:** `ext_surfaces` + the `nvim_api` signature change; the
  `UI_EXT_OPTIONS` static becomes the default-all list the function builds
  from, so there is still exactly one spelling of each ext name.
- [ ] **Step 3:** Move the config read ahead of spawn; `NativeSession::load`
  takes the resolved value (delete the second read, and with it the risk of
  two different answers in one session).
- [ ] **Step 4:** Rewrite the noice scenario's stale comment; add the
  `cmdheight` probe to its `deferred` state.
- [ ] **Step 5: Disconfirm.** `task compat -- compat/scenarios/noice.toml`:
  the `deferred` state (`native = { palette = false, notifications = false
  }`, `accommodations = false`) goes green — noice's three ERROR
  notifications do not appear and its own chrome renders, on a config with
  nothing pre-seeded. The `unaccommodated` state stays red, and the commit
  body says so, naming T19. Record both in the commit body.
- [ ] **Step 6:** `task ci`. Commit: `feat(engine): turning a native
  feature off gives its surface back to your plugins`.

---

### Task 9: The `vim.notify` takeover the spec already promised (#16)

**Files:**
- Modify: `crates/view-native/src/supersede.rs` (a second takeover kind),
  `crates/view-core/src/msg.rs` (`RpcCall::HoldNotify`),
  `crates/view-engine/src/nvim_api.rs` (the chunk that performs and holds
  it), `crates/view-native/src/report.rs` (doctor listing),
  `compat/scenarios/nvim-notify.toml`

**Interfaces:** §5.5 states "notifications → `vim.notify` re-pointed at the
engine default so messages flow through `ext_messages` into view's toasts",
and `TAKEOVERS` implements none of it. This task adds the row. The takeover
must *hold*, exactly as `laststatus` does: noice patches `vim.notify` at
its deferred load, after `VimEnter`, so a one-shot set loses. The hold
re-asserts on the same `SafeState` autocmd the API chunk already installs
(the chunk's `nvim_create_autocmd('SafeState', …)` registration), which is the cheapest event that fires after any
plugin's deferred work and never during a redraw.

`Takeover`'s table gains a `kind` (`Option { option, value }` |
`Notify`), and `takeover_call` renders each; the existing
`no_two_takeover_rows_claim_one_option` invariant is extended so two rows
cannot claim `vim.notify` either.

**What the new tests prove:**
`notifications_enabled_supersedes_vim_notify` (plan-level);
`the_takeover_reasserts_after_a_plugin_repatches_it` — a live
`task compat-supersede` assertion against the heavy fixture: patch
`vim.notify` from Lua after startup, trigger `SafeState`, read it back and
find view's again. That mirrors the lualine `laststatus` evidence the
existing hold was built from.

**Falsifiable check:** render the row as a plain set instead of a hold →
the re-assert test fails after the fixture's own patch.

**Dependencies:** T8. **Perf:** none on the paint path; one extra autocmd
body on `SafeState`, which already runs — state the added Lua cost in the
commit and cite `flood/cadence_p99_ms` as the row that would catch a
regression under event pressure.

- [ ] **Step 1: Failing test** for the plan row.
- [ ] **Step 2:** `RpcCall::HoldNotify` + the chunk (captured from the
  live engine: the chunk text goes in
  `docs/toast-routing-wire-capture.md`'s successor section or a new capture
  doc if the shape is not already pinned there).
- [ ] **Step 3:** The takeover-kind refactor; doctor listing.
- [ ] **Step 4: Disconfirm.** `task compat-supersede` with the fixture's
  nvim-notify present: a `vim.notify` call lands as a view toast, not a
  float, and `[native] notifications = false` returns the float.
- [ ] **Step 5:** `task ci`. Commit: `feat(native): view's notification
  takeover holds against a plugin that re-patches vim.notify`.

---

### Task 19: A default first launch says one thing, not three (#16) — sequenced after T11, before T14

**Why this task exists.** T8 resolves the C2 conflict *for a user who opts
out*. On view's defaults — the actual first launch, and the finding of
record — every ext is attached, noice's ungated setup-time check still
raises its three errors, and after T9 they arrive as three view toasts with
no remedy in any of them. The charter's option 1 carried "one explanatory
notice" for exactly this case; taking option 2 does not retire that
obligation, it relocates it. This task is that notice. It is a completion
of the settled fork, not a re-fork: view keeps its surfaces by default,
because they are the product, and it owns the explanation.

**The decided default experience**, stated once so every downstream
assertion can quote it:

```
╭─ view ──────────────────────────────────────────────────────────╮
│ view: noice.nvim is using the command line and messages, which   │
│ view owns. Set [native] palette = false and notifications =      │
│ false in view.toml to give them back. Startup messages from      │
│ this launch are in the history — <leader>fm.                     │
╰──────────────────────────────────────────────────────────────────╯
```

**One notice per claimant, aggregating every surface that claimant claims
and view attached** — not one per (claimant, surface). Raised once, never
re-recorded, carrying no running count (a count that updates is a notice
that re-records, and under T14/T16 a notice that re-records re-enters the
stack and re-animates). Nothing discarded: the plugin's own messages are in
the history, one key away, and the notice says so.

**Files:**
- Modify: `crates/view-core/src/native/surfaces.rs` (claimant rows gain the
  Lua module names that identify the class),
  `crates/view-core/src/update/surface_conflict.rs` (the probe-reply arm),
  `crates/view-core/src/native/toast.rs` (`Route::HistoryOnly`; the
  `"native_sticky"` kind), `crates/view-core/src/model.rs`
  (`Messages::{startup_hold, held}`, `MessageEntry::is_native`,
  `is_persistent_kind`, `record_native_notice_sticky_once`),
  `crates/view-core/src/msg.rs` (`Msg::{ClaimantsProbed,
  StartupHoldExpired}`, `RpcCall::ProbeClaimants`,
  `Effect::ScheduleStartupHold`), `crates/view/src/runtime.rs` (the
  deadline timer, on `ScheduleToastExpiry`'s exact shape),
  `crates/view-core/src/update/watch.rs` (`file_gone_prefix` is renamed
  into the family namespace — see interface 4),
  `crates/view-engine/src/nvim_api.rs` (the probe chunk),
  `docs/toast-routing-wire-capture.md` (a new section),
  `compat/scenarios/noice.toml` (the `unaccommodated` state goes green)
- Spec: A4's conflict bullet and A5's paragraph on notice lifetime — both
  outcome-level, both landing with T8; nothing new is owed here.

**The ordering fact this design is built on.** noice's three errors are
raised inside `noice.setup()`, which the heavy fixture runs *eagerly*
during config sourcing (`compat/fixtures/heavy/nvim/init.lua` — its lazy
spec carries no `event`/`lazy` trigger), i.e. **before `VimEnter`**. In
embed mode nvim sources config after the UI attaches, so they reach view
immediately. Any mechanism that starts deciding at `VimEnter` — or one RPC
round-trip after it — is already too late. The hold therefore opens at
model construction, before a single byte of engine traffic, and the probe
decides only what to do with what the hold already caught.

**Consumer call-site first:**

```rust
// view-core/src/native/surfaces.rs — the claimant table gains identity
pub struct Claimant {
    class: &'static str,     // "noice.nvim"
    module: &'static str,    // "noice"  — what `package.loaded` is asked about
    surfaces: &'static [Surface],
}
/// Every notice this claimant can raise starts with this string.
fn conflict_family(c: &Claimant) -> String { format!("view: {} is using ", c.class) }

// update/surface_conflict.rs, on Msg::ClaimantsProbed — one notice per claimant
for c in probed_loaded {
    let claimed: Vec<Surface> = c.surfaces.iter().copied().filter(|s| model.owns(*s)).collect();
    if claimed.is_empty() { continue; }                 // view gave that surface away
    model.record_native_notice_sticky_once(&conflict_family(c), notice_text(c, &claimed));
}
model.messages.resolve_startup_hold(if any_claim { Collapse } else { Release });
```

**Interfaces:**

1. **The startup hold opens at model construction.** `Messages::startup_hold`
   is `Pending` from `Model::default()` — before the engine is spawned,
   before attach, before config sourcing, so it cannot lose a race it was
   built to win. While `Pending`, a message that `route()` would call
   `Transient` is recorded into the history ring exactly as today and
   parked in `Messages::held` instead of `entries`: it paints nothing and
   takes no slot. `Route::HistoryOnly` is the name of that outcome.
   **Four classes are never held**, and the fourth is what keeps the
   collapse honest:
   - persistent kinds (`emsg`, `lua_error`, …) — an error nvim itself
     classifies as an error is not startup chatter;
   - prompts — the editor is blocked on the answer;
   - statusline routes — they were never toasts;
   - **view's own native notices** (`MessageEntry::is_native`). view is not
     a claimant and never speaks in one's name, so a broken-config notice
     (`NativeSession::load`'s "every native feature stays on this session" line in `crates/view/src/native.rs`), the pre-executor notices
     (`main.rs`'s `record_native_notice` calls on its startup path, and `startup.rs`'s "startup key buffer full" notice) and every other line view raises
     about itself paint immediately, exactly as they do today, conflict or
     no conflict. This is one `is_native()` read, and without it the
     collapse would demote a config typo the user needs to see behind a
     notice that never mentions it.
2. **The hold resolves exactly once, on the first of three triggers.** No
   trigger can be starved, so the state is not reachable at rest:
   - `Msg::ClaimantsProbed` naming ≥ 1 claimant on a surface view attached
     → **Collapse.** One sticky notice per claimant is raised, and the held
     foreign messages stay where they are — in the history ring — rather
     than draining to the stack. The hold then stays on until the first
     key, so a late complaint from the same startup is collapsed too, with
     the notice already standing to explain it and no re-record, because
     the notice carries no count.

     **What "attributable" means, and the residue when it cannot be
     narrowed.** view's own notices are already excluded (interface 1), so
     the held set at collapse is foreign `msg_show` traffic only. If step
     1's capture yields an attribution key for the claimant's own
     complaints — a distinguishing `kind`, or a caller source that T9's
     takeover can attach the way noice's own `Health.get_source` does — the
     collapse keeps exactly those and **releases the rest** into the stack.
     If it yields none, no text-matching stands in for it: the whole
     foreign held set stays in the history. The cost of that residue,
     stated rather than argued away: an unrelated plugin that echoed
     something during the same startup has its line in the history instead
     of on the stack, which is why the notice's own last clause says
     "startup messages from this launch" and not "its own". No message is
     lost on any path; a foreign one can be *deferred to the history*, and
     that is the whole of the trade.
   - `Msg::ClaimantsProbed` naming none → **Release.** `held` drains into
     `entries` in arrival order; from there everything behaves exactly as
     it does today. A clean config sees no behavior change at all.
   - First `Msg::Key`, or `Msg::StartupHoldExpired` (a one-shot
     `Effect::ScheduleStartupHold` armed at attach) → **Release.** The
     deadline is what makes an engine that never answers degrade to
     today's behavior rather than to silence.

   **Drain re-stamps `shown_at_flush`.** A held entry carries the flush
   generation it was pushed at, and by the time anything releases it the
   generation has advanced many times.
   `Messages::dismiss_transient_on_keypress` retains a transient only when
   `e.shown_at_flush == current` (in `model.rs`) — the
   at-least-one-visible-frame guard — so a drained entry with its original
   stamp is dismissed by the very keypress that released it, painting zero
   frames. The drain therefore re-stamps every drained entry to the current
   generation, which is the same convention `update/ui_event.rs`'s `UiEvent::Flush` arm
   maintains for a freshly pushed toast, and the drain runs *before* the
   dismissal pass in the same `Msg::Key` handling
   (its `dismiss_transient_on_keypress` call in `update/mod.rs`) so the re-stamp is what the pass sees.

   **The degrade when the deadline effect is not wired.**
   `Effect::ScheduleStartupHold` → `Msg::StartupHoldExpired` copies a shape
   the runtime already wires (`Effect::ScheduleToastExpiry` →
   `Msg::ToastExpired`, both declared in `msg.rs`), but a runtime
   or harness that drops the effect leaves the first key as the only
   remaining trigger. That degrades to: an engine that dies before the
   probe answers, in a session where the user never types, holds its
   startup messages in the history and paints none of them. Stated so it is
   a known ceiling rather than a mystery, and asserted the other way round
   by `the_first_key_releases_the_hold_even_with_no_probe_reply`.
3. **Presence, probed once.** On the first `SafeState` after `VimEnter` —
   the same event T9's hold rides, and the cheapest that fires after a
   plugin manager's deferred load — the engine evaluates one Lua expression
   built from the claimant table (`package.loaded[m] ~= nil` per row) and
   replies as `Msg::ClaimantsProbed`. One RPC, once, off the paint path,
   asking about no plugin private: `package.loaded` is the public module
   registry. **Bound, stated:** a claimant that loads *after* the probe is
   not collapsed — its messages release with the rest and toast normally.
   That is the honest degradation, and it is the case the heavy fixture
   does not exercise, because it loads noice eagerly.
4. **The notice's family and route, pinned** — the committed mechanism
   requires both and states neither:
   - **Family = claimant-scoped**, `"view: {class} is using "`, and
     **`notice_text` must begin with that exact string**.
     `record_native_notice_once` withdraws by
     `line.starts_with(family)` in `is_standing_native_notice` (`model.rs`), so a wording that
     does not start with its own family cannot withdraw its predecessor and
     stacks instead.
   - **Families are pairwise non-prefix.** Prefix withdrawal is
     cross-family the moment one family prefixes another, and after
     `is_native` accepts `"native_sticky"` every native notice shares one
     withdrawal pool. All four schemes in the tree after this phase, each
     pinned in its own task and each satisfying begins-with-family:

     | Scheme | Family | A notice's text |
     |---|---|---|
     | Surface claimant (T19) | `"view: {class} is using "` | `view: noice.nvim is using the command line and messages, which view owns. Set …` |
     | Float claim (T11) | `"view: {identity} is drawing over "`, or `"view: a plugin is drawing over "` when the float carries no identity | `view: cmp_menu is drawing over the command line, which view owns. Set …` |
     | Clipboard (T18) | `"view: no system clipboard "` | `view: no system clipboard is reachable; copies went to view's own registers and to OSC 52.` |
     | File gone (shipped, renamed here) | `"view: file {path} "` | `view: file /x/y.rs is no longer readable on disk` |

     The last row is a rename: `file_gone_prefix`
     (in `update/watch.rs`) is `"{path} is no longer a readable file
     on disk"` today — path-first, so a file literally named
     `view: noice.nvim is using …` (legal on POSIX) has its notice
     cross-withdrawn by the conflict notice. Moving the static
     discriminator to the front (`"view: file {path} "`) makes the
     collision decidable instead of pathological: every other family's
     variable part comes from a **static table** (claimant classes, and a
     float identity read from a buffer), so a drift test can settle it.
     Raise and withdraw both go through the one function, so the rename is
     one `format!` plus its existing test expectations.

     The drift test instantiates every dynamic scheme with each shipped
     claimant class, a sample float identity, and a sample path — including
     the adversarial instantiation where one scheme's variable is another
     scheme's fixed head — and asserts pairwise non-prefix in both
     directions. Static-only assertions would pass while the real strings
     collide.
   - **Route = sticky.** `record_native_notice*` records kind `"native"`,
     and `route("native")` is `Transient`/4 s
     (in `toast.rs`), so the remedy notice would vanish before it could
     be acted on. This task adds the kind `"native_sticky"`:
     `MessageEntry::is_native` accepts it (so family withdrawal still
     applies), `is_persistent_kind` lists it (so `route()` returns
     `Sticky` through the arm that already exists), and T14 keeps sticky
     entries out of the slot queue, so the standing notice cannot freeze
     the transient stack behind it. `set_native_condition` was considered
     and rejected: it holds exactly one condition entry model-wide
     (its `find(|e| e.condition)` replace branch in `model.rs`), and a second condition would silently
     overwrite this one.
   - **End of life, because sticky has none by default.**
     `dismiss_transient_on_keypress` retains persistent entries
     (its `e.is_persistent()` retain arm in `model.rs`) and `Messages::clear` retains native ones through
     nvim's `msg_clear` (its `retain(MessageEntry::is_native)`), so nothing in the tree takes
     this notice down. The session lifetime is right — the conflict stays
     true until the remedy is applied and view restarts — but "stands
     forever with no gesture" is not, so **T18 adds `d` in the history
     overlay**, calling `withdraw_native_notice(family)`; the notice's own
     closing clause already sends the user to `<leader>fm`. Between T19 and
     T18 the notice is undismissable within a session, which is stated
     rather than discovered at dogfood.

**Why the hold is level-independent.** The narrower rule — "hold only
ERROR-level messages" — is not available on evidence: noice raises these
through `vim.notify(…, ERROR)`, whose default handler renders through
`nvim_echo`, and whether the level survives into the `msg_show` `kind` is
recorded in no capture doc in this tree. Step 1 captures what the three
messages actually look like; if a discriminating `kind` is there, the hold
narrows to it in the same task and the capture doc says so. Until then the
hold is broad over *foreign* traffic and brief: it starts at construction
and resolves on the first of three triggers. Its cost, on each path
separately, because they differ:

| Path | What a foreign startup message gets |
|---|---|
| No claimant found | Released to the stack in arrival order — today's behavior, delayed by the probe round-trip |
| Claimant found, attribution key available | Attributable ones stay in history; the rest released to the stack |
| Claimant found, no attribution key | Stays in history, reachable by `<leader>fm`, disclosed by the notice's closing clause |

view's own notices are on none of those rows: they are never held.

**Module-size plan:** `surface_conflict.rs` gains ~120 lines on top of
T11's ~250; `surfaces.rs` ~40; `toast.rs` is 195 lines today. Nothing
approaches the ceiling.

**What the new tests prove:**
`a_message_arriving_before_the_probe_answers_is_held_not_toasted` — the
ordering defect, as the test of record: the message is recorded at a point
where no notice stands and no probe has replied, which is exactly when
noice's three arrive;
`a_probe_naming_no_claimant_releases_every_held_message_in_order` (a clean
config sees today's behavior, and sees it in the right order);
`the_first_key_releases_the_hold_even_with_no_probe_reply` and
`the_deadline_releases_the_hold_even_with_no_probe_reply` (the two
starvation guards);
`a_persistent_kind_is_never_held` (an `emsg` toasts during startup as it
always has);
`view_s_own_notice_is_never_held_even_on_the_collapse_path` — the
broken-config line paints on a launch that also finds a claimant, the one
path where a collapse that dropped everything held would swallow it;
`a_released_message_survives_the_key_that_released_it` (the re-stamp: drain
then dismissal pass, same keypress, entry still standing);
`a_loaded_claimant_on_an_attached_surface_raises_exactly_one_notice`
(one per claimant, not one per surface — the count is asserted, not
described);
`the_notice_text_starts_with_its_own_family` and
`no_two_native_notice_families_prefix_each_other` (the two contracts
`record_native_notice_once`'s prefix withdrawal silently requires);
`the_conflict_notice_outlives_the_transient_timeout` (it is sticky, and
this is the test that fails if the kind is ever recorded as `"native"`);
`a_loaded_claimant_on_a_yielded_surface_notices_nothing` (with `palette =
false` view owns nothing there — the notice follows T8's ownership, not a
constant);
`an_absent_claimant_notices_nothing` (the test that keeps this from
becoming a startup banner);
`no_message_is_ever_dropped` — its fixture includes a foreign message no
claimant accounts for, so the non-claimant fate on the collapse path is
asserted, not implied — every held entry is in `ToastHistory` on
both the collapse and the release path, the invariant that makes this a
collapse rather than a swallow.

`no_two_native_notice_families_prefix_each_other` ranges over the four
schemes in interface 4's table, instantiated from the shipped claimant
list, a sample identity and a sample path, including the adversarial
instantiation.

**Falsifiable check:** open the hold at `VimEnter` instead of at
construction → `a_message_arriving_before_the_probe_answers_is_held_not_toasted`
fails, which is precisely the race this design exists to lose-proof. Record
the notice with kind `"native"` → `the_conflict_notice_outlives_the_transient_timeout`
fails after 4 s. Emit per (claimant, surface) →
`a_loaded_claimant_on_an_attached_surface_raises_exactly_one_notice` fails
with three notices where one is expected. Drop the `held` entries on the
release path → `no_message_is_ever_dropped` fails. Hold view's own notices
too (delete the `is_native()` exclusion) →
`view_s_own_notice_is_never_held_even_on_the_collapse_path` fails, and a
config typo goes unseen on a noice launch. Skip the drain's re-stamp →
`a_released_message_survives_the_key_that_released_it` fails with zero
painted frames. Leave `file_gone_prefix` path-first →
`no_two_native_notice_families_prefix_each_other` fails on the adversarial
path instantiation.

**Dependencies:** T8 (ownership is config-dependent), T9 (the notify path
view now owns), T11 (the claimant table and the notice mechanism).
**Perf:** one `nvim_exec_lua` on the first `SafeState`, off the paint path,
answered as a `Msg`; `route()` gains one enum read per message; the hold is
a `Vec` push instead of an entries push. No per-frame cost, and one fewer
toast animation on a conflicting first launch. State against
`first_paint/shell_visible_cold_ms` (the probe is a startup RPC) and re-run
that cell; cite `flood/cadence_p99_ms` as the row that would catch a
`route()` regression under message pressure.

- [ ] **Step 1: Capture.** With the heavy fixture unaccommodated, record in
  `docs/toast-routing-wire-capture.md`: the exact `msg_show` events noice's
  three startup errors produce (`kind`, chunk highlights, whether the notify
  level survives into any field); **where those events land relative to
  `VimEnter` and the first `SafeState`** — the ordering the whole design
  turns on, observed rather than argued; and the `package.loaded` probe's
  expression and reply verbatim.
- [ ] **Step 2: Failing test**
  `a_message_arriving_before_the_probe_answers_is_held_not_toasted`.
- [ ] **Step 3:** `Messages::{startup_hold, held}` + `Route::HistoryOnly` +
  the three resolve triggers and the deadline effect. Narrow the hold to a
  message `kind` if step 1's capture showed a discriminating one.
- [ ] **Step 4:** The `"native_sticky"` kind (`is_native`,
  `is_persistent_kind`, `record_native_notice_sticky_once`); rename
  `file_gone_prefix` into the family namespace and update its existing test
  expectations; then the family drift test over all four schemes.
- [ ] **Step 5:** The claimant identity rows + the probe chunk + decode +
  `Msg::ClaimantsProbed`; extend the bridge chunk's occurrence-count test
  rather than bypassing it. Then the notice arm: one per claimant,
  ownership-aware, remedy string generated from the `[native]` switch.
- [ ] **Step 6:** Turn noice's `unaccommodated` state green — it asserts
  the notice text, the remedy line, and the *absence* of "Noice can't work
  when the GUI has" from the stack while it is present in the history.
- [ ] **Step 7: Disconfirm.** Live on the heavy fixture, unaccommodated,
  defaults: exactly one view notice, still on screen a minute later (its
  dismissal ships with T18's overlay key, not here); `<leader>fm` shows the
  three noice errors verbatim. Add a deliberate typo to the fixture's
  `view.toml` for one run and confirm view's own broken-config notice
  paints on that same launch, beside the conflict notice — the never-held
  guarantee, as a live check rather than only a unit test. Then the same launch
  with noice removed from the fixture: no notice at all, and any startup
  message the config does produce toasts normally — the release path is
  the one an ordinary user takes, so it is the one that must be observed
  working, not merely unit-asserted.
- [ ] **Step 8:** `task bench -- --scenario first_paint --fixture heavy`
  and `--scenario flood --fixture heavy`; state both in the commit.
- [ ] **Step 9:** `task ci`. Commit: `feat(native): a first launch that
  hits a surface conflict gets one notice with the fix, not a plugin's
  error wall`.

---

### Task 10: Wire capture — floats that claim a view-owned surface (#17)

**Files:**
- Create: `docs/surface-float-wire-capture.md`
- Create: `scripts/acceptance/capture-surface-floats.lua`

**Why:** T11 and T12 consume float geometry and identity fields that view
receives from *no* event today (see Deviation 1), and T12 reads a foreign
plugin's buffer. Which fields the observed plugins actually set — whether
cmp's menu buffer carries a filetype, whether its selection is a cursor
position or an extmark, what `zindex` a notify float uses — is exactly the
class of fact that must be captured.

**Interfaces:** against the heavy fixture, with view attached, capture for
each of: nvim-cmp's cmdline float (typing `:e ` and `:se `), nvim-notify's
toast float, noice's error float, telescope's picker (the negative control
— a float that claims *nothing* view owns and must never be flagged):

- `nvim_win_get_config(win)` verbatim (`relative`, `row`, `col`, `width`,
  `height`, `anchor`, `zindex`, `focusable`, `hide` if present)
- `nvim_win_get_buf`, `vim.bo[buf].filetype`, `nvim_buf_get_name`
- `nvim_buf_get_lines(buf, 0, -1, false)` and `nvim_win_get_cursor(win)`
  before and after one `<C-n>`
- any extmark namespace present on the buffer
  (`nvim_get_namespaces` + `nvim_buf_get_extmarks`)
- the autocmd events that fire when the float opens, moves and closes
  (`WinNew`, `WinScrolled`, `WinResized`, `WinClosed`, `CursorMovedI`,
  `TextChangedI`) and their ordering relative to the window becoming
  configured
- **Churn: what the float does per keystroke.** nvim-cmp closes and
  reopens or reconfigures its windows on text and selection changes, so
  T12's absorption may be facing a new window id on every key. Capture,
  while typing a five-character prefix at `:` one key at a time: the window
  id after each key (same id reused, or a new one), whether the config is
  re-set on an existing window, how many `WinNew`/`WinClosed` pairs the
  five keys produce, and — the one that decides T12's failure mode —
  whether a window that view has hidden with `nvim_win_set_config(win, {
  hide = true })` is re-shown by the plugin's own next
  `nvim_win_set_config`, and how many frames later. Record the wall time
  between the keystroke and the float's reconfiguration, since that
  interval is the width of any double-chrome flash.

**Falsifiable check:** the doc must show at least one field that
distinguishes a claiming float from telescope's. If nothing distinguishes
them, T11's detector cannot be built as designed and the design returns for
a re-think rather than shipping a detector that flags every float.
Second, the churn capture must produce a number: keys typed, hide calls
implied, re-shows observed. "Cmp recreates its window sometimes" is not a
capture; T12's cadence bound is written from this figure, so an absent
figure blocks T12 the same way an absent reply grammar blocks T4.

**Dependencies:** none (runs against the shipped tree). **Perf:** none.

- [ ] **Step 1:** The capture chunk, run under `task compat` against the
  heavy fixture with a scenario that opens each float in turn.
- [ ] **Step 2:** Capture the cmp cmdline float across `:`, `/`, and a
  no-candidates case.
- [ ] **Step 3:** Capture the selection-change delta (`<C-n>`), which is
  what T12's absorbed selection must read.
- [ ] **Step 3b:** Capture the per-keystroke churn above: five keys typed
  at `:`, window ids after each, and one hidden-then-plugin-reconfigured
  window with the interval measured.
- [ ] **Step 4:** Write the doc, one section per float, with a closing
  "what distinguishes a claiming float" table.
- [ ] **Step 5:** Commit: `docs(engine): capture the geometry and identity
  of floats that land on view-owned surfaces`.

---

### Task 11: Generic surface-ownership conflict detection (#17) — **L**

**Files:**
- Modify: `crates/view-engine/src/nvim_api.rs` (the `view_bridge` chunk
  gains float autocmds; its rpcnotify-count test),
  `crates/view/src/bridge.rs` (decode), `crates/view-core/src/msg.rs`
  (`Msg::FloatObserved`), `crates/view-core/src/update/` (a new
  `surface_conflict.rs` — do **not** grow `update/mod.rs`, 1553 lines),
  `crates/view-core/src/native/` (a new `surfaces.rs`: the owned-surface
  table and the policy), `crates/view-core/src/model.rs` (conflict state)
- Create: `crates/view-core/src/native/surfaces.rs`,
  `crates/view-core/src/update/surface_conflict.rs`

**Consumer call-site first:**

```rust
// view-core/src/native/surfaces.rs — data, so the set is enumerable
pub enum Surface { Cmdline, Messages, Popupmenu, Tabline, Grid }
pub enum Policy { Own, Yield, Absorb }
pub struct OwnedSurface { surface: Surface, policy: Policy, remedy: &'static str }
pub fn claims(rect: FloatRect, model: &Model) -> Option<Surface>;
```

**Interfaces:** the bridge chunk gains one autocmd group emitting
`vim.rpcnotify(channel, 'view_bridge', 'float', win, buf, row, col, width,
height, zindex, filetype, name)` on the events T10 captured, debounced
through the same `vim.schedule`/`SafeState` discipline the existing bridge
already uses so a float storm cannot flood the channel. `claims` maps a
rect to a surface view owns using the geometry view already tracks
(`model.term_height`, the grid offset, the statusline reservation) — the
cmdline row and the message-area rect are computed in `view-core`, from
state it already holds, so the decision is pure and unit-testable.

The notice is one line **per float identity**, aggregating the surfaces
that identity claims — the same shape T19 uses for claimant classes, and
for the same reason: `record_native_notice_once` withdraws by family
prefix, so a second notice sharing a family retracts the first rather than
sitting beside it. Pinned here because T19's drift test ranges over it:

- **family** = `"view: {identity} is drawing over "`, or
  `"view: a plugin is drawing over "` when the float carries no identity
  (Deviation 3);
- **text** begins with its own family, which
  `is_standing_native_notice`'s `starts_with` requires
  (`is_standing_native_notice` in `model.rs`): `view: cmp_menu is drawing over the command
  line, which view owns. Set [native] palette = false to give it back.`

A repeating detection therefore replaces its own wording instead of
stacking, and never touches another identity's notice.

**Module-size plan:** `surfaces.rs` ≈ 200 lines, `surface_conflict.rs` ≈
250, `bridge.rs` grows ~60. Nothing approaches the 1000-line ceiling;
`update/mod.rs` gains only the dispatch arm.

**What the new tests prove:**
`a_float_on_the_cmdline_row_is_a_cmdline_claim`;
`a_float_in_the_message_area_is_a_messages_claim`;
`a_centered_picker_float_claims_nothing` (telescope, the negative control
from T10 — this is the test that keeps the detector from becoming noise);
`one_identity_claiming_two_surfaces_raises_one_notice_naming_both`;
`a_repeated_detection_replaces_its_wording_instead_of_stacking`;
`each_float_notice_starts_with_its_own_family`;
`a_claim_on_a_surface_view_yielded_notices_nothing` (with `palette =
false`, view does not own the cmdline and must stay quiet — the detector
must follow T8's ownership, not a constant).

**Falsifiable check:** widen `claims` to return `Some(Cmdline)` for any
float → `a_centered_picker_float_claims_nothing` fails. Remove the
ownership check → `a_claim_on_a_surface_view_yielded_notices_nothing`
fails.

**Dependencies:** T8 (ownership is now config-dependent), T10.
**Perf:** the bridge notification arrives on the engine channel and is
decoded on the reader thread like every other bridge event, then dispatched
as a `Msg`; `claims` is arithmetic on integers. No RPC is awaited anywhere
on the paint path. Under a float storm the debounce bounds the traffic —
state that against `flood/cadence_p99_ms` and re-run that bench cell.

- [ ] **Step 1: Failing tests** for the three `claims` cases, using rects
  transcribed from `docs/surface-float-wire-capture.md`.
- [ ] **Step 2:** `surfaces.rs` (table + `claims`), pure, no I/O.
- [ ] **Step 3:** The bridge chunk + decode + `Msg::FloatObserved`; extend
  the chunk's `vim.rpcnotify(channel, 'view_bridge'` occurrence-count test rather than
  bypassing it.
- [ ] **Step 4:** `surface_conflict.rs` — the notice, once per pair,
  ownership-aware.
- [ ] **Step 5: Disconfirm.** Live under `task compat` on the heavy
  fixture, unaccommodated: opening telescope produces no notice; typing `:e `
  with cmp-cmdline produces exactly one, naming the cmdline.
- [ ] **Step 6:** `task bench -- --scenario flood --fixture heavy`;
  state the cadence figure in the commit.
- [ ] **Step 7:** `task ci`. Commit: `feat(native): a plugin drawing over a
  surface view owns is named, once, with the line that resolves it`.

---

### Task 12: The cmdline completion float is absorbed into the palette (#17) — **L**

**Files:**
- Modify: `crates/view-core/src/native/palette.rs` (a second row source),
  `crates/view-core/src/native/surfaces.rs` (the `Absorb` policy arm),
  `crates/view-core/src/msg.rs` (`RpcCall::{HideWindow, ReadFloatRows}`),
  `crates/view-engine/src/nvim_api.rs` (both calls),
  `crates/view-core/src/update/surface_conflict.rs`,
  `crates/view-surface/src/lib.rs` (the palette layer's completion source)

**Consumer call-site first:**

```rust
// PaletteState today takes the ext_popupmenu completion; it gains a second
// constructor for rows that came from a plugin's own float instead.
PaletteState::with_absorbed(cmdline.clone(), AbsorbedRows { lines, selected })
```

**Interfaces:** when `claims` returns `Cmdline` with policy `Absorb` and a
cmdline is open, view (a) hides the float —
`nvim_win_set_config(win, { hide = true })`, reversible and touching no
buffer text — and (b) reads its rows and selection with the exact fields
T10 captured, rendering them as palette rows. When the float closes or the
cmdline closes, view stops absorbing; if the hide call fails (an older
engine, a plugin that re-shows), view falls back to T11's notice rather
than leaving two chromes on screen — the failure mode is "we told you",
never "we drew over it".

This is what makes the charter's observation actionable: cmp-cmdline never
drives nvim's popupmenu, so `popupmenu_show` never fires and view's
existing absorption path is never reached. Absorption by float content is
the generic version of that path — it needs no cmp API, so any float-menu
plugin on the cmdline gets the same treatment.

**Churn, as a designed outcome rather than a discovered one.** cmp does not
hold one window across a cmdline session: it recreates or reconfigures on
text and selection changes, and `hide = true` holds only until the plugin's
own next `nvim_win_set_config`. So absorption faces, in the worst case, a
new window per keystroke on the path that sits next to key dispatch. Two
bounds, both written from T10 §3b's captured figures and both asserted:

- **RPC cadence: at most one `HideWindow` per observed float identity, not
  per keystroke.** view hides a window id once and remembers it; a repeat
  observation of the same id emits nothing. A genuinely new id costs one
  hide. If the capture shows cmp recreating per key, that is one small
  effect per key on a path that already sends per-key RPC — the echo cell
  (step 6) is the gate, and the commit states the measured ratio, not an
  argument that it is fine.
- **Re-show: a bounded one-frame flash, disclosed, never a silent
  double chrome.** Between a plugin's re-show and view's next hide there
  can be one frame carrying both chromes. That is accepted (view cannot
  hold a window against its owner without a lock nvim does not offer) and
  bounded: view re-hides on the very next float observation, and if the
  same id re-shows more than twice, view stops absorbing that float and
  falls back to T11's notice — the "we told you" failure mode, reached by
  a counter rather than by a user noticing flicker. The
  `the_float_is_hidden_before_its_rows_are_painted` test cannot see this,
  which is precisely why the counter, not the test, is the mechanism.

**Module-size plan:** `palette.rs` is 300 lines today and gains ~120;
`surface_conflict.rs` gains ~150. Both stay well inside the ceiling.

**What the new tests prove:**
`absorbed_rows_render_in_the_palette_with_the_selection_marked`;
`the_float_is_hidden_before_its_rows_are_painted` (ordering — a frame with
both is the bug);
`a_hide_failure_degrades_to_the_notice_and_never_paints_both`;
`absorption_stops_when_the_cmdline_closes`;
`the_same_float_is_hidden_once_however_often_it_is_observed` (the cadence
bound);
`a_float_that_reshows_three_times_falls_back_to_the_notice` (the flash
bound, as a counter).

**Falsifiable check:** skip the hide call → `the_float_is_hidden_before_…`
fails. Delete the `absorption_stops_…` teardown → the palette keeps stale
rows after `<Esc>`, which that test names. Emit a hide on every observation
instead of every new identity → `the_same_float_is_hidden_once_…` fails
with N effects where one is expected, which is the regression that would
put a per-keystroke RPC on the cmdline path.

**Dependencies:** T10, T11. **Perf:** paint — the palette gains rows from
a source that arrives as a `Msg`, not as an RPC read on the paint path
(the read is an effect whose reply is a `Msg`, per the never-await rule).
State against `echo/{ratio_p50, view_p99_ms}` and re-run the echo cell on
the heavy fixture, since this path is active during cmdline typing.

- [ ] **Step 1: Failing test** for absorbed rows in the palette.
- [ ] **Step 2:** `RpcCall::HideWindow` + `ReadFloatRows` and their
  encoders, field names transcribed from the capture doc.
- [ ] **Step 3:** `PaletteState::with_absorbed` + the surface layer.
- [ ] **Step 4:** The absorb arm in `surface_conflict.rs`, including
  teardown and the hide-failure degrade.
- [ ] **Step 5: Disconfirm.** Live, heavy fixture, unaccommodated: type
  `:e src/` and see one palette carrying cmp's candidates, with cmp's own
  float gone; `<C-n>` moves the palette selection.
- [ ] **Step 6:** `task bench -- --scenario echo --fixture heavy`; state
  the ratio in the commit, and state the measured hide-calls-per-keystroke
  figure beside it. This cell is the gate for the churn: a ratio that moved
  is the churn showing up, and it is a blocker, not a note.
- [ ] **Step 7:** `task ci`. Commit: `feat(native): a plugin's cmdline
  completion menu renders inside view's palette instead of over it`.

---

### Task 13: The surface-ownership matrix (#17)

**Files:**
- Create: `docs/surface-ownership.md`
- Modify: `crates/view-core/src/native/surfaces.rs` (the doc generator's
  source of truth), a drift test beside it

**Interfaces:** the matrix is generated from `surfaces.rs` plus the loaded
scenario set, on the pattern `docs/keymaps.md` already uses ("The table
below is generated from `default_maps()`; a test fails if this page and the
keys view registers disagree"). Columns: surface, ext option, view's
policy, the `[native]` switch that yields it, the plugin classes observed
to claim it, and the compat scenario+state that proves the policy. A
surface with no proving scenario renders as `— none —`, which is the
charter's "coverage gaps become visible rather than implicit".

**What the new tests prove:**
`the_surface_matrix_page_matches_the_policy_table`;
`every_owned_surface_names_its_off_switch_or_says_it_has_none`
(`ext_tabline` is the honest `none` row).

**Falsifiable check:** change a policy in `surfaces.rs` without
regenerating → the drift test fails naming the row.

**Dependencies:** T11, T12, and T7 (the scenario column reads real state
names). **Perf:** none.

- [ ] **Step 1: Failing drift test** with the page absent.
- [ ] **Step 2:** Generator + page.
- [ ] **Step 3: Disconfirm.** Delete a scenario's state and confirm the
  page's proving column becomes `— none —` rather than silently keeping the
  old text.
- [ ] **Step 4:** `task ci`. Commit: `docs(native): the surface-ownership
  matrix names every claim, policy, and the scenario that proves it`.

---

### Task 14: Toast slots — the dismissal timer belongs to the top slot (#18)

**Files:**
- Modify: `crates/view-core/src/native/toast.rs`,
  `crates/view-core/src/model.rs` (`Messages` gains slot bookkeeping),
  `crates/view-core/src/update/ui_event.rs` and `update/mod.rs`
  (`Msg::ToastExpired` arm)
- Spec: amendment A5's routing-table row lands first.

**Interfaces:** `record_message` stops arming a timer for every transient
entry. Instead `Messages` exposes `top_slot() -> Option<MessageId>` and the
update loop arms `ScheduleToastExpiry` for the top slot only, re-arming
when the top slot changes. Sticky (`Route::Sticky`) and prompt entries are
*not* in the slot queue — they never expire, and a sticky entry parked at
the top would otherwise freeze every transient behind it forever.
`Route::HistoryOnly` (T19) is likewise not in the queue, by definition: it
never occupies a slot. That is
the one design decision here that is not directly stated by the charter and
it is made deliberately: the queue is the transient queue; sticky notices
are pinned and dismissed explicitly, exactly as `Route::Sticky` already
promises.

An expiry `Msg::ToastExpired { id }` for an entry that is no longer the top
slot is ignored (it can only arrive from a stale timer), which is the same
generation-guard shape `supervision`'s probe already uses.

**What the new tests prove:**
`only_the_top_slot_arms_a_timer`;
`a_toast_behind_another_starts_its_timer_when_it_reaches_the_top`
(the charter's exact sentence, as a test name);
`a_sticky_entry_does_not_hold_the_slot_queue`;
`a_stale_expiry_for_a_lower_slot_is_ignored`.

**Falsifiable check:** arm the timer in `record_message` again →
`only_the_top_slot_arms_a_timer` fails with two effects where one is
expected, and `a_toast_behind_another_…` fails by expiring an unread
notice.

**Dependencies:** T19 (and through it T11, C4's start gate — the slot queue
must already know which routes never take a slot). **Perf:** no paint
change yet;
strictly fewer timer threads than today (one, not one per toast) — state
that.

- [ ] **Step 1: Failing test** `a_toast_behind_another_starts_its_timer_when_it_reaches_the_top`.
- [ ] **Step 2:** Slot bookkeeping in `Messages`; re-arm on slot change.
- [ ] **Step 3:** The stale-expiry guard.
- [ ] **Step 4: Disconfirm.** Emit five `:echomsg` lines in one flush under
  the oracle and assert the fifth is still on screen after
  `4 × TRANSIENT_TOAST_TIMEOUT` minus a margin — today it is long gone.
- [ ] **Step 5:** `task ci`. Commit: `fix(core): a notification's dismissal
  timer starts when it reaches the top of the stack, not when it arrives`.

---

### Task 15: nvim-cmp's untested path (#15)

**Files:**
- Modify: `compat/scenarios/nvim-cmp.toml`

**Interfaces:** the scenario drives only insert-mode buffer completion and
never types at `:`, so the one path that actually breaks is the one path
untested (charter C3's second instance). Add steps that open the cmdline,
type a prefix one key at a time (the same pacing the file already
documents for cmp's paste-suppression heuristic), and assert that view's
palette carries the candidate — the T12 behavior — rather than that cmp's
float exists.

**What the new tests prove:** the scenario itself is the test. It is red
before T12 and green after; the commit lands after T12 for that reason, but
the steps are written during T12's own red phase.

**Falsifiable check:** revert T12's absorb arm → the scenario fails on the
palette assertion, not on a timeout.

**Dependencies:** T7 (state vocabulary), T12. **Perf:** none.

- [ ] **Step 1:** Add the cmdline steps; run red against the pre-T12 tree
  and record the failure.
- [ ] **Step 2:** Run green against the post-T12 tree.
- [ ] **Step 3:** Commit: `test(compat): nvim-cmp is asserted at the
  command line, where it actually collides`.

---

### Task 16: Toast motion — slide up, exit right, damage-driven (#18)

**Files:**
- Modify: `crates/view-surface/src/lib.rs` (one layer per toast),
  `crates/view-tui/src/paint.rs` (`paint_messages` paints one toast),
  `crates/view-core/src/model.rs` (animation state),
  `crates/view-core/src/msg.rs` (`Msg::AnimTick`, `Effect::ScheduleAnimTick`),
  `crates/view/src/runtime.rs` (the tick timer, beside `ScheduleToastExpiry`)
- Spec: amendment A6 lands first.

**Interfaces:** the single `LayerKind::Messages(Vec<Vec<Span>>)` becomes
one layer per visible entry (`LayerKind::Toast { lines, slot, x_offset }`),
which is what lets the top one move right while the others move up. The
motion is one animation with one clock: `Msg::AnimTick` advances a
`ToastMotion { phase, elapsed_steps }` in `view-core`; `update` returns
`Effect::ScheduleAnimTick { after }` only while a motion is live, so an
idle stack schedules nothing — the damage-driven property, enforced by a
test rather than asserted in a comment. Below `Tier::Full` there is no
interpolation at all (spec §7.1 rule: `standard`/`basic` paint final
frames), and the slot timers still work — motion is presentation, timing is
behavior.

**Module-size plan:** `paint.rs` is at 4369 total lines (production count
under the ceiling, per `task loc`); this task moves the toast painting into
`crates/view-tui/src/paint/toast.rs` rather than growing it. Check `task
loc` before and after.

**What the new tests prove:**
`an_idle_stack_schedules_no_tick` (the damage-driven contract);
`the_top_toast_exits_right_while_the_rest_slide_up_in_one_motion`;
`a_tick_after_the_motion_ends_schedules_nothing`;
`below_full_tier_the_stack_jumps_to_its_final_state`;
`input_during_a_motion_completes_it_on_that_frame` (spec §7.1 rule 2).

**Falsifiable check:** return `ScheduleAnimTick` unconditionally →
`an_idle_stack_schedules_no_tick` fails, which is the exact regression that
would put a free-running timer on an idle editor.

**Dependencies:** T14. **Perf:** touches paint. Each motion is ≤ `slow`
(120 ms) at cell granularity, so a dismissal costs a bounded handful of
extra frames and nothing at rest. State against `output_path/p99_ms` and
`echo/ratio_p50`, and re-run both cells: a motion model that costs frames
is a regression against the shipping contract, not a decoration on it
(charter C4's performance obligation, verbatim).

- [ ] **Step 1: Failing test** `an_idle_stack_schedules_no_tick`.
- [ ] **Step 2:** Per-toast layers in `view-surface`; move painting into
  `paint/toast.rs`.
- [ ] **Step 3:** `ToastMotion` in `view-core` + the tick effect and its
  one-shot timer in `runtime.rs`, on `ScheduleToastExpiry`'s exact shape.
- [ ] **Step 4:** Tier gate + the interrupt rule.
- [ ] **Step 5: Disconfirm.** `VIEW_LOG` a real session with three toasts:
  the log shows ticks only across the two motion windows and none between.
- [ ] **Step 6:** `task bench -- --scenario echo --fixture minimal` and
  `--scenario output_path`; state both in the commit.
- [ ] **Step 7:** `task ci`. Commit: `feat(surface): the toast stack slides
  up as its top notice exits to the right`.

---

### Task 17: Pause (#18)

**Files:**
- Modify: `crates/view-core/src/native/mappings.rs` (a seventh default map —
  `DEFAULT_MAPS: [MappingSpec; 6]` today: `ff`, `fb`, `fg`, `e`, `fm`, `ai`),
  `crates/view-core/src/native/registry.rs` (the `notifications` feature
  gains a second verb), `crates/view-core/src/update/mod.rs` (the invoke
  arm), `crates/view-core/src/model.rs` (`Messages::paused`),
  `crates/view-surface/src/lib.rs` (the paused indicator), `docs/keymaps.md`
  (regenerated; its drift test enforces this)

**Interfaces:** `<leader>fp` → `:View notifications pause`, toggling
`Messages::paused`. While paused: no `ScheduleToastExpiry` is armed, and an
in-flight `ToastExpired` is ignored; on unpause the top slot's timer is
armed fresh for the full timeout (a notice half-expired when the user
paused deserves the whole window, not the remainder). The paused state is
*visible* — a `⏸` mark in the top toast's border run — because a freeze the
user cannot see is indistinguishable from a stuck editor.

**What the new tests prove:**
`pausing_disarms_the_slot_timer`;
`an_expiry_arriving_while_paused_is_ignored`;
`unpausing_arms_the_full_timeout_not_the_remainder`;
`the_paused_stack_renders_its_indicator`.

**Falsifiable check:** drop the in-flight guard → `an_expiry_arriving_while_paused_is_ignored`
fails, and a notice vanishes mid-read with pause on, which is the exact
complaint.

**Dependencies:** T14 (slots), T16 (the indicator sits in the toast
layer). **Perf:** paint — one glyph in an existing border run.

- [ ] **Step 1: Failing test** `an_expiry_arriving_while_paused_is_ignored`.
- [ ] **Step 2:** The verb, the mapping, the model flag, the arm.
- [ ] **Step 3:** The indicator + `docs/keymaps.md` regeneration.
- [ ] **Step 4: Disconfirm.** Live: raise a toast, hit `<leader>fp`, wait
  three times the timeout, and read it; unpause and watch it expire.
- [ ] **Step 5:** `task ci`. Commit: `feat(native): a key freezes
  notification expiry so a notice can be read`.

---

### Task 18: History — scroll, and copy the path (#18)

**Files:**
- Modify: `crates/view-core/src/native/palette.rs`
  (`MessageHistoryState` gains selection + scroll),
  `crates/view-core/src/update/mod.rs` (a key arm for the overlay — it has
  none today and falls to the generic `<Esc>` fallback),
  `crates/view-core/src/msg.rs` (`ClipboardWrite.token` becomes
  `Option<ReplyToken>`; `Msg::ClipboardUnavailable`),
  `crates/view/src/clipboard.rs` (skip the reply for a `None` token; report
  an unreachable system clipboard), `crates/view-surface/src/lib.rs`
  (selection rendering — `PaletteView` already carries a selection concept
  the picker uses)

**Interfaces:** the history overlay gains `j`/`k`/`<C-d>`/`<C-u>`/`gg`/`G`
scrolling over the snapshot it already takes, and `y` copies the selected
entry's text through the existing pair — `Effect::ClipboardWrite { token:
None, .. }` plus `Effect::Osc52Copy`, exactly as `update/mod.rs`'s `EngineRequest::ClipboardSet` arm
emits them for an engine-initiated copy, so a remote session copies to the
local machine for free. When the worker cannot reach a system clipboard it
sends `Msg::ClipboardUnavailable`, which raises the notice the charter
assumed existed (Deviation 4), through `record_native_notice_once` with its
family pinned here because T19's drift test ranges over it:

- **family** = `"view: no system clipboard "`;
- **text** = `view: no system clipboard is reachable; copies went to
  view's own registers and to OSC 52.` — begins with its family, as
  `is_standing_native_notice`'s `starts_with` requires.

**The overlay also dismisses a standing notice (`d`).** A sticky native
notice — T19's surface-conflict line, which stands for the session because
the conflict does — has no other end of life:
`dismiss_transient_on_keypress` retains persistent entries
(its `e.is_persistent()` retain arm in `model.rs`) and `Messages::clear` retains native ones across nvim's
`msg_clear` (its `retain(MessageEntry::is_native)`). `d` on a selected history entry calls
`withdraw_native_notice(family)` for that entry's family, which is the
existing, already-tested retraction path; nothing new is invented, and the
gesture lives where the notice's own text already sends the user
(`<leader>fm`). This is the mechanism behind A4/A5's "stands until you
dismiss it from the notification history".

Paths are the primary target and the reason this exists, so the copy is the
entry's text verbatim: no trimming, no quoting, no "copied 1 line" cleverness
that would break a path with a space in it.

**What the new tests prove:**
`the_history_overlay_scrolls_past_its_visible_rows`;
`the_copy_key_emits_both_the_local_write_and_the_osc52_escape`;
`a_copy_with_no_reply_token_is_not_replied_to` (the worker invariant —
today's one-reply-per-token contract must not break);
`an_unreachable_system_clipboard_notices_once`;
`the_copied_text_is_the_entry_verbatim`;
`the_dismiss_key_takes_down_a_standing_sticky_notice` (T19's conflict
notice, the one entry nothing else can retire);
`the_dismiss_key_on_an_ordinary_history_entry_takes_down_nothing`
(dismissal retracts a standing notice, it does not edit history).

**Falsifiable check:** emit only `Osc52Copy` → `the_copy_key_emits_both_…`
fails, and a local session copies nothing. Make the worker reply on a
`None` token → `a_copy_with_no_reply_token_is_not_replied_to` fails
(msgpack would carry a reply for a request that never existed).

**Dependencies:** T14 (history and the live stack share `MessageEntry`),
T19 (its sticky conflict notice is what `d` exists to retire, and its
family scheme is what `d` keys on).
**Perf:** paint, overlay-only, unchanged per-frame cost for a closed
overlay.

- [ ] **Step 1: Failing test** `the_copy_key_emits_both_the_local_write_and_the_osc52_escape`.
- [ ] **Step 2:** Selection + scroll on `MessageHistoryState`; the key arm
  (this is the first arm the overlay has had — keep the generic `<Esc>`
  fallback reachable).
- [ ] **Step 3:** `Option<ReplyToken>` + the worker's guard.
- [ ] **Step 4:** `Msg::ClipboardUnavailable` + the once-per-family notice;
  the `d` arm calling `withdraw_native_notice` on the selected entry's
  family.
- [ ] **Step 5: Disconfirm.** Over a real SSH session (the configuration
  that produced the charter): raise a toast naming a path, open the
  history, copy it, and paste it on the *local* machine. Then run with
  `DISPLAY` unset and confirm the notice appears exactly once across two
  copies.
- [ ] **Step 6:** `task ci`. Commit: `feat(native): notification history
  scrolls, and one key copies the path you needed`.

---

## Exit checklist

- [ ] Spec amendments A1–A9 applied verbatim from
  `spec-amendments-draft.md`, each in its stated commit.
- [ ] `task ci` green at the final tip.
- [ ] `task compat` green, **including** every new `unaccommodated` state,
  with the noice states passing for their stated reasons rather than
  because they were weakened: `unaccommodated` (defaults, nothing
  pre-seeded) shows view's one conflict notice and keeps noice's own
  errors off the stack and in the history; `deferred` (the remedy applied,
  still nothing pre-seeded) shows noice running clean with no view notice
  at all.
- [ ] `task oracle` unchanged (no corpus entry regressed).
- [ ] `task bench -- --gate` on dev-linux: the four rows this phase touches
  (`first_paint/shell_visible_cold_ms`, `output_path/p99_ms`,
  `echo/ratio_p50`, `flood/cadence_p99_ms`) inside their gates, with the
  figures quoted.
- [ ] The two capture docs exist, name their hosts, and every wire string in
  the tree traces to one of them.
- [ ] `docs/surface-ownership.md` and `docs/keymaps.md` regenerate clean
  (drift tests green).
- [ ] A live SSH dogfood from the same client that produced the charter,
  in **two named runs against the same noice-carrying config**, because one
  run cannot show both halves of the C2 settlement:

  **Run 1 — view's defaults** (`[native]` untouched; the first launch of
  record). Full-tier chrome with rounded borders over SSH (no `COLORTERM`),
  no debug line on quit, **exactly one** view notice naming noice, the
  surfaces, and the `[native]` lines — and none of noice's own three errors
  on the toast stack, with all three present in `<leader>fm`. Then, in the
  same session: a toast read under pause, a path copied out of the history
  to the *local* clipboard, and the conflict notice dismissed with `d` from
  that same overlay. Every C4 bullet lives here, because
  `notifications = true` is what makes view's stack the stack.

  **Run 2 — the remedy applied** (`[native] palette = false, notifications
  = false`). noice renders its own cmdline and messages, raises no errors,
  and view raises no notice. This is the run that proves the line in run
  1's notice is a line that works.

  Also in run 1, once: relaunch with `--tier basic` under the same
  box-drawing terminal and confirm ASCII borders — the one place a tier
  override still decides the charset, and otherwise only unit-asserted
  (T6 step 4).

  This is the acceptance of record — the charter exists because no CI leg
  runs what a user's own machine presents.
