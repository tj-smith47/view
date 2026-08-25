# P2 — Elm Core + Runtime + Surface + Tiers + ext Layers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** replace the P1 scaffolding loop with the real architecture: a pure
Elm core (`Model`/`Msg`/`update`/`Effect`), a synchronous unified event loop
that paints on Flush with zero poll ticks, Surface-based rendering with
capability tiers, and native rendering of `ext_cmdline`/`ext_messages`/
`ext_tabline`/`ext_popupmenu` — plus the headless entry P3's oracle requires.

**Architecture:** blocking std threads at every seam (spec §5.2 as amended,
decision log "Runtime model"): an input thread and the engine reader thread
feed one bounded `mpsc` channel of `Msg`s; the main thread runs
`update()` + `render()` + paint synchronously and never blocks on RPC.
Coalescible redraw traffic folds into a damage structure on the reader
thread (latest-wins) so a plugin storm can neither flood the channel nor
starve responses. No async runtime anywhere (async arrives P5, inside
view-ai only).

**Tech Stack:** existing workspace crates. New dependencies, all named here
and nowhere else: `serde` + `toml` in the `view` bin crate only (theme cache
file), `portable-pty` + `vt100` promoted from dev-deps to regular deps of
`view-oracle` (headless pty driver), `criterion` as a dev-dependency of the
crates gaining micro-benches, `rustix` (features `std` + `termios` only,
default-features off, unix-only) as a direct dependency of `view-tui` for
VMIN=0/VTIME=0 non-blocking reads during capability probing — crossterm
exposes no API for reading arbitrary CSI replies, and rustix is already in
the lockfile via crossterm, so this adds an edge, not a package. Capability
detection uses raw escape queries
(COLORTERM + DECRQM 2026 + kitty `CSI ? u` + DA1 fence); no terminfo crate.

## Global Constraints

- Hard rules, verbatim from repo CLAUDE.md (each binds every task; a task
  whose text conflicts with one of these is defective):
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
  In-loop engine calls go through `notify`-backed helpers only;
  `scripts/audit-deps.sh` enforces the direction and `task audit` must pass
  at every commit.
- Hot-path allocation posture: the per-key `String` notation and per-batch
  `Vec` allocations remain accepted this phase; allocation-free enforcement
  and CI gating land with P3's bench gates. Any NEW per-frame allocation
  beyond these must be named in the commit's latency sentence.
- view-core stays dependency-free and I/O-free. `update()` is pure and
  synchronous; effects are data.
- Workspace lints deny unwrap/expect/panic in lib code; test modules may
  open `#![allow(clippy::unwrap_used, clippy::expect_used)]` only. No
  panic-macro substitutions (`unreachable!()` included).
- Comments: rustdoc `///` = WHAT + contract; inline = non-obvious WHY only.
  Banned everywhere: Phase/Task/Step/Wave/Cycle/Session + number, `§` as a
  marker, we/I/Claude, plan/review/finding/audit references, em dashes
  (`scripts/check-style.sh` scans crate sources and will fail the commit).
- Build/lint/test via task targets only; commit via
  `task commit -- -m "<msg>"`. Trailer on every commit:
  `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`. Any commit
  touching key dispatch, grid apply, or paint states its latency consequence
  in one sentence.
- **Wire-capture rule (planning protocol step 1):** every fixture for a
  wire event in this plan is either (a) copied from a capture the plan
  itself performed and cites, or (b) mandated to be captured live by the
  implementer before the RED test is written, with the capture command
  given. Writing a fixture from memory is a defect even if tests pass.
- Latency is a contract: this phase must delete both structural latency
  components recorded in `.claude/bench-baselines/p1-latency.txt` (the 4ms
  post-redraw silence timeout and the up-to-4ms input-drain delay). T11
  re-measures; the plan fails its exit if the structural tick survives.

## Coverage walk (planning protocol step 0)

Every charter deliverable and spec MUST for this phase, mapped to a task or
a recorded deferral. An implementer or reviewer finding a phase requirement
absent from both lists has found a plan defect.

| Requirement | Where |
|---|---|
| Elm core: Model/Msg/update/Effect, pure and rmpv-free | T1 |
| Full ext attach set including ext_popupmenu | T2 |
| Damage coalescing, bounded channel, reader never blocks | T3 |
| Engine-request dispatch seam (P4 modal prompts + clipboard reply through it) | T1/T3/T4/T10 |
| Zero-tick unified loop, paint on Flush | T4 |
| Surface = render(&Model) in the view-surface crate | T5 |
| Cursor shape + position from ModeInfoSet via DECSCUSR | T5/T6 |
| Native cmdline/messages/tabline/popupmenu rendering | T6 |
| Focus routing seam, bracketed paste, mouse | T7 |
| Capability tiers as TermCaps; BSU/ESU gated on the probed sync bit | T5/T8 |
| Theme derivation from live hl state + cold-start cache | T9 |
| Startup sequence incl. blocking VimEnter rpcrequest | T10 |
| Headless driver meeting the P3 charter's four-leg contract | T11 |
| Latency re-measure: both P1 structural components gone | T11 |
| Criterion micro-benches per hot path | T11 |

Recorded deferrals (user approval required; nothing here is silently
dropped):

1. Engine supervision on crash (keep last frame + restart prompt, spec's
   engine-lifecycle section): P2 maps EngineDown to Quit with the engine's
   exit code. Proposed owner: P4, before daily-driving starts. Awaiting
   the user's call.
2. Surface-tree memoized diffing: view-side damage exists this phase as
   Grid damage coalescing (T3) plus ratatui's buffer diff at paint (T5).
   Additional Surface-level diffing is measured at P3's bench gates before
   being built. Recorded as a claim for user visibility, not a silent drop.
3. Allocation-free hot-path enforcement + CI gating: P3 owns the gates; P2
   states its allocation posture (Global Constraints) and ships the
   micro-benches those gates consume.

## As-built interfaces this plan builds on (read from the tree at authoring
time; re-verify with `grep -n "pub fn" crates/<crate>/src/<file>.rs` if a
brief seems stale)

```rust
// view-engine
Engine::spawn(EngineConfig) -> Result<Engine, EngineError>
Engine::take_notifications(&mut self) -> Option<Receiver<EngineNotification>>
Engine::pid(&self) -> u32
Engine::shutdown(self) -> std::io::Result<ExitStatus>
EngineHandle::{request, request_timeout, notify}   // Clone + Send
EngineHandle::{ui_attach(w,h), input(notation), try_resize(w,h)}  // nvim_api
decode_redraw(&[Value]) -> Vec<UiEvent>
UiEvent::{GridLine, GridResize, GridScroll, GridCursorGoto, GridClear,
          HlAttrDefine{underline included}, DefaultColorsSet{Option<u32>},
          Flush, Unknown}
EngineNotification { pub method: String, pub params: Vec<Value> }

// view-core
Grid::{new, apply(GridOp), size, cursor, cell, row_text}
GridOp::{Resize, Clear, CursorGoto, PutLine, Scroll}

// view-tui
keys::encode_key(&KeyEvent) -> Option<String>
paint::{HlAttr, HlTable, clamp_dim, saturate_u16, paint}   // relocating in T1/T5
terminal::{Term::{init, size, draw, set_cursor, restore_now}, TerminalGuard,
           InputEvent, drain_input}                        // reshaped in T4/T5
```

## File Structure (end state)

```
crates/view-core/src/
  grid.rs            (unchanged)
  events.rs          NEW: UiEvent/GridCell as pure data (moved from view-engine)
  hl.rs              NEW: HlAttr/HlTable as pure data (moved from view-tui)
  model.rs           NEW: Model, EngineModel, Focus, TermCaps, Tier
  msg.rs             NEW: Msg, Effect, EngineRequest, and their payload types
  update.rs          NEW: update(&mut Model, Msg) -> Vec<Effect>
  theme.rs           NEW: Theme derived from HlTable + DefaultColors, pure
crates/view-surface/src/
  lib.rs             NEW: Surface/Layer/CursorSpec + render(&Model); depends on view-core only
crates/view-engine/src/
  ui_events.rs       EXTENDED: mode/cmdline/messages/tabline/popupmenu events
  damage.rs          NEW: coalescing damage buffer + pump handle
  handle.rs          EXTENDED: reader folds damage + dispatches known requests
  process.rs         EXTENDED: start_pump, wait_exit
  nvim_api.rs        EXTENDED: full ext attach set, paste, input_mouse, eval_str
crates/view-tui/src/
  paint.rs           RESHAPED: Surface -> ratatui, style conversion only
  tiers.rs           NEW: capability detection -> TermCaps
  terminal.rs        EXTENDED: input thread, BSU/ESU brackets, DECSCUSR cursor
crates/view/src/
  main.rs            SHRUNK: CLI + wiring only
  runtime.rs         NEW: unified loop + Effect executor
  startup.rs         NEW: shell paint, key buffering, attach, VimEnter hook
  theme_cache.rs     NEW: cold-start theme cache file
crates/view-oracle/src/
  lib.rs             NEW: Session + EngineSession headless drivers (P3 contract)
  pty.rs             NEW: PtySession (promoted from test scaffolding)
  raster.rs          NEW: pure Surface+Grid -> text; no view-tui dependency
crates/{view-core,view-engine,view-surface}/benches/
                     NEW: criterion micro-benches per hot path
```

---

### Task 1: Core vocabulary — events, hl, Model/Msg/Effect, update() skeleton

**Files:**
- Create: `crates/view-core/src/events.rs` (UiEvent + GridCell move here as pure data)
- Create: `crates/view-core/src/hl.rs` (HlAttr/HlTable move here, ratatui-free)
- Create: `crates/view-core/src/model.rs`, `crates/view-core/src/msg.rs`, `crates/view-core/src/update.rs`
- Modify: `crates/view-core/src/lib.rs` (module wiring)
- Modify: `crates/view-engine/src/ui_events.rs` (decode into `view_core::events` types; delete local definitions)
- Modify: `crates/view-engine/Cargo.toml` (add `view-core` dependency)
- Modify: `scripts/audit-deps.sh` (the engine→core edge is legal today: no rule forbids it, and `check_absent view-core view-engine` — core must not depend on engine — already exists and must remain; add a comment beside it stating engine→core is a sanctioned edge so a later audit sweep does not "fix" it)
- Modify: `crates/view-tui/src/paint.rs` and `crates/view/src/main.rs` (imports follow the moved types; behavior unchanged this task)

**Interfaces:**
- Consumes: as-built `Grid`/`GridOp`, `UiEvent` variant list, `HlAttr`/`HlTable` fields — read them from the tree first.
- Produces (later tasks depend on these exact names):

```rust
// msg.rs — the runtime loop (Task 4) is the consumer; its exact call-site:
//   let msg = match msg_rx.recv() {
//       Ok(Msg::RedrawReady) => Msg::Redraw(pump.take_damage()),
//       Ok(Msg::EngineStopped) => Msg::EngineDown(engine.wait_exit()),
//       Ok(m) => m,
//       Err(_) => Msg::EngineDown(ExitInfo { code: None, by_signal: false }),
//   };
//   for eff in update(&mut model, msg) { /* executor, never blocks */ }
#[non_exhaustive]
pub enum Msg {
    Key(Key),                  // already-encoded nvim notation from the input thread
    Redraw(Vec<UiEvent>),      // one compacted damage batch, drained by the loop
    RedrawReady,               // pump token: damage staged; the loop MUST drain it into
                               // Redraw before update() sees it (raw = silent no-op)
    EngineStopped,             // reader token: engine stream ended; the loop resolves ExitInfo
    EngineDown(ExitInfo),
    EngineRequest(EngineRequest),
    Resized { width: u16, height: u16 },
}
pub struct Key { pub notation: String }
// Producers compute code before this reaches update(): unix signal death is
// code = Some(128 + signal). update() maps code None (status unreadable) to
// exit 1. RedrawReady/EngineStopped are loop plumbing: the loop resolves
// them before update(); update() returns no effects for them (totality).
pub struct ExitInfo { pub code: Option<i32>, pub by_signal: bool }

// Engine-initiated requests, decoded to a closed vocabulary in view-engine;
// unknown methods never reach core (the reader auto-errors them, as built).
// The engine BLOCKS awaiting the reply, so every arm MUST produce exactly
// one Effect::Reply; the reply routes through the writer thread's channel
// and never blocks the loop. P4's modal prompts and clipboard provider
// extend this enum; the dispatch seam itself is this phase's contract.
#[non_exhaustive]
pub enum EngineRequest { VimEnter { token: ReplyToken } }
pub struct ReplyToken { pub msgid: u64 }
#[non_exhaustive]
pub enum ReplyValue { Nil }

#[non_exhaustive]
pub enum Effect {
    Rpc(RpcCall),
    Reply { token: ReplyToken, value: ReplyValue },
    Quit { exit_code: i32 },
}
// Closed vocabulary instead of (method, Vec<Value>): core stays rmpv-free
// and an unencodable call is unrepresentable. Runner-up (stringly method +
// opaque params) rejected: re-opens the door to core building wire values.
#[non_exhaustive]
pub enum RpcCall {
    Input { notation: String },
    TryResize { width: u16, height: u16 },
    Paste { text: String },
}

// model.rs
#[non_exhaustive]
pub struct Model {
    pub engine: EngineModel,
    pub focus: Focus,
    pub caps: TermCaps,
    pub dirty: bool,           // set by update() on Flush; cleared by the loop after paint
    pub running: bool,
}
#[non_exhaustive]
pub struct EngineModel {
    pub grid: Grid,
    pub hl: HlTable,
    pub mode: ModeState,       // filled in Task 2; starts minimal
}
#[non_exhaustive]
pub enum Focus { Engine }      // Native(id) arrives with the first native overlay
// Tier is coarse UX vocabulary; the probed bits are what gates behavior
// (BSU/ESU gates on caps.sync, never on tier alone). Detection fills this
// in Task 8; the default is conservative: Standard with all probes false.
#[non_exhaustive]
pub struct TermCaps { pub tier: Tier, pub sync: bool, pub truecolor: bool, pub kitty_kbd: bool }
#[non_exhaustive]
pub enum Tier { Full, Standard, Basic }

// update.rs
pub fn update(model: &mut Model, msg: Msg) -> Vec<Effect>;
```

- The `ai`/`native` Model fields from the spec's illustrative shape are
  deliberately absent: they arrive with their phases; `#[non_exhaustive]`
  keeps the door open without dead placeholders.

- [ ] **Step 1: Failing tests for the move + update() behavior** (`update.rs` tests)

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::events::UiEvent;

    fn model() -> Model { Model::new() }

    #[test]
    fn redraw_batch_applies_to_grid_and_sets_dirty_only_on_flush() {
        let mut m = model();
        let effects = update(&mut m, Msg::Redraw(vec![
            UiEvent::GridResize { grid: 1, width: 10, height: 3 },
            UiEvent::GridLine { grid: 1, row: 0, col_start: 0,
                cells: vec![crate::events::GridCell {
                    text: "h".into(), hl_id: 0, repeat: 1 }] },
        ]));
        assert!(effects.is_empty());
        assert!(!m.dirty, "no Flush yet: must not request paint");
        let effects = update(&mut m, Msg::Redraw(vec![UiEvent::Flush]));
        assert!(effects.is_empty());
        assert!(m.dirty);
        assert_eq!(m.engine.grid.row_text(0).trim_end(), "h");
    }

    #[test]
    fn key_in_engine_focus_becomes_rpc_input_effect() {
        let mut m = model();
        let effects = update(&mut m, Msg::Key(Key { notation: "<C-x>".into() }));
        assert!(matches!(&effects[..],
            [Effect::Rpc(RpcCall::Input { notation })] if notation == "<C-x>"));
    }

    #[test]
    fn engine_down_maps_signal_and_code_to_exit_effects() {
        let mut m = model();
        let effects = update(&mut m, Msg::EngineDown(ExitInfo { code: Some(5), by_signal: false }));
        assert!(matches!(&effects[..], [Effect::Quit { exit_code: 5 }]));
        assert!(!m.running);
    }

    #[test]
    fn engine_down_without_code_exits_one_and_signal_code_passes_through() {
        let mut m = model();
        let effects = update(&mut m, Msg::EngineDown(ExitInfo { code: None, by_signal: false }));
        assert!(matches!(&effects[..], [Effect::Quit { exit_code: 1 }]));
        let mut m = model();
        let effects = update(&mut m, Msg::EngineDown(ExitInfo { code: Some(137), by_signal: true }));
        assert!(matches!(&effects[..], [Effect::Quit { exit_code: 137 }]));
    }

    #[test]
    fn loop_tokens_are_noops_and_engine_request_always_replies() {
        let mut m = model();
        assert!(update(&mut m, Msg::RedrawReady).is_empty());
        assert!(update(&mut m, Msg::EngineStopped).is_empty());
        let effects = update(&mut m, Msg::EngineRequest(
            EngineRequest::VimEnter { token: ReplyToken { msgid: 9 } }));
        assert!(matches!(&effects[..],
            [Effect::Reply { token: ReplyToken { msgid: 9 }, value: ReplyValue::Nil }]));
    }

    #[test]
    fn resize_produces_try_resize_effect() {
        let mut m = model();
        let effects = update(&mut m, Msg::Resized { width: 120, height: 40 });
        assert!(matches!(&effects[..],
            [Effect::Rpc(RpcCall::TryResize { width: 120, height: 40 })]));
    }
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p view-core` — expected: FAIL to compile (types absent).
- [ ] **Step 3: Implement.** Move `UiEvent`/`GridCell` verbatim from view-engine (pure data; keep `#[non_exhaustive]`, `Option<u32>` colors, doc comments intact) into `events.rs`; move `HlAttr`/`HlTable` minus any ratatui types into `hl.rs` (the `Modifier` conversion stays in view-tui). Implement `Model::new()`, `update()` covering exactly the four tested behaviors; the redraw arm folds `UiEvent → GridOp` (the translation formerly in `main.rs` moves here verbatim, including `clamp_dim`/`saturate_u16`, which move to `events.rs` since they guard wire values). view-engine's `ui_events.rs` keeps `decode_redraw` but imports the types.
- [ ] **Step 4: Verify** — `cargo test -p view-core && task ci` — expected: view-core tests pass AND the whole workspace still builds green (the type move touches engine/tui/bin imports; `task ci` proves the move is complete, not partial).
- [ ] **Step 5: Commit** — `task commit -- -m "refactor(core): own the event vocabulary, model, and update loop"` + trailer. State: no latency consequence (type moves + pure logic).

### Task 2: Full ext attach + decode of mode/cmdline/messages/tabline/popupmenu events

**Files:**
- Modify: `crates/view-engine/src/nvim_api.rs` (`ui_attach` requests ext_linegrid, ext_cmdline, ext_popupmenu, ext_messages, ext_tabline)
- Modify: `crates/view-core/src/events.rs` (new UiEvent variants + payload structs)
- Modify: `crates/view-engine/src/ui_events.rs` (decoders)
- Modify: `crates/view-core/src/model.rs` + `update.rs` (ModeState, CmdlineState, Messages, TablineState, PopupmenuState + their update arms)
- Test: extend `crates/view-engine/tests/redraw_live.rs`

**WIRE-CAPTURE FIRST (mandatory, before any fixture is written):** run

```bash
nvim --api-info > ~/.claude/tmp/api-info.mpack   # decode with the rpc codec in a scratch test
```

and capture a real event stream: spawn the engine (existing test helpers),
attach with the full ext set, drive `:`, `q`, `<CR>`, `:tabnew<CR>`, and an
insert-mode completion (`i`, `<C-n>`), draining notifications to a log.
Every fixture in this task's tests is copied from that capture and cited
with a one-line comment naming the capture step. The api-info `ui_events`
section is the arity/type authority. Do not write any event fixture from
memory; the P1 decoder shipped a wrong-arity fixture exactly that way.

**Interfaces — Produces (Task 6 renders these):**

```rust
// events.rs additions (payloads per capture; names fixed here)
UiEvent::ModeInfoSet { cursor_style_enabled: bool, modes: Vec<ModeInfo> },
UiEvent::ModeChange { mode: String, mode_idx: u64 },
UiEvent::CmdlineShow { content: Vec<(u64, String)>, pos: u64, firstc: String,
                       prompt: String, indent: u64, level: u64 },
UiEvent::CmdlinePos { pos: u64, level: u64 },
UiEvent::CmdlineHide,
UiEvent::MsgShow { kind: String, content: Vec<(u64, String)>, replace_last: bool },
UiEvent::MsgClear,
UiEvent::TablineUpdate { current: TabHandle, tabs: Vec<TabEntry> },
UiEvent::PopupmenuShow { items: Vec<PmItem>, selected: i64, row: u64, col: u64, grid: u64 },
UiEvent::PopupmenuSelect { selected: i64 },
UiEvent::PopupmenuHide,
```

Payload field types above are the plan's contract for downstream tasks;
if the capture contradicts a field's type, the capture wins, the implementer
fixes the plan text in the same commit (plan-sync duty), and says so in the
report.

- [ ] **Step 1:** perform the capture; check the decoded api-info arities for each event against the variant shapes; write RED decode tests from captured payloads (tolerant trailing-`..` slice patterns as in the existing decoders; unknown events still fall through to `Unknown`).
- [ ] **Step 2:** `cargo test -p view-engine` — FAIL (variants absent).
- [ ] **Step 3:** implement decoders + model state (`ModeState` tracks current mode + cursor shape table; messages append/replace per `replace_last`; popupmenu/cmdline set/clear their `Option` state) + update arms setting `dirty` only via `Flush`.
- [ ] **Step 4:** `cargo test -p view-engine && cargo test -p view-core` — PASS; extend `redraw_live.rs` to attach with the full ext set and assert at least one `ModeChange` and, after driving `:` via `nvim_input`, one `CmdlineShow` decode non-Unknown within deadline.
- [ ] **Step 5:** `task commit -- -m "feat(engine,core): full ext attach and decoded cmdline, messages, tabline, popupmenu, mode events"` + trailer. No latency consequence (decode breadth, same paths).

### Task 3: Damage coalescing + bounded engine pump + request dispatch

**Files:**
- Create: `crates/view-engine/src/damage.rs`
- Modify: `crates/view-engine/src/handle.rs` (the reader thread gains the fold sink and known-request dispatch; the handle gains `reply`)
- Modify: `crates/view-engine/src/process.rs` (`Engine::start_pump`, `Engine::wait_exit`)
- Test: `crates/view-engine/tests/flood.rs` (new; the flood test moves up a level)

**The consumer call-site (Task 4's loop) this is designed against — the two
sketches are the same code; if they ever differ, that is a plan defect:**

```rust
// runtime loop: one blocking recv on ONE channel of Msg; no polling anywhere
let msg = match msg_rx.recv() {
    Ok(Msg::RedrawReady) => Msg::Redraw(pump.take_damage()),   // drain + compact, non-blocking
    Ok(Msg::EngineStopped) => Msg::EngineDown(engine.wait_exit()),
    Ok(m) => m,
    Err(_) => Msg::EngineDown(ExitInfo { code: None, by_signal: false }),
};
for eff in update(&mut model, msg) { /* executor */ }
```

**Interfaces — Produces:**

```rust
// process.rs
Engine::start_pump(&mut self, sink: std::sync::mpsc::SyncSender<Msg>) -> DamagePump
Engine::wait_exit(&mut self) -> ExitInfo   // graceful wait bounded by shutdown_timeout,
                                           // then kill; unix signal death -> Some(128 + sig)
// damage.rs
DamagePump::take_damage(&self) -> Vec<UiEvent>   // clears pending + drains in ONE lock hold
// handle.rs
EngineHandle::reply(&self, token: ReplyToken, value: ReplyValue) -> Result<(), EngineError>
// routes [1, msgid, nil, value] through the writer channel; never blocks the caller
```

**Design (spec: reader never blocks; responses correlate before queueing;
coalescible redraw folds latest-wins; only non-coalescible traffic uses the
bounded channel):**

- The reader thread (handle.rs — the fold happens where the decode already
  happens, NOT on a second thread consuming an unbounded queue; the
  as-built unbounded notification channel is deleted with `take_notifications`
  when its last caller dies in Task 4) decodes each `redraw` notification
  and folds the events into a `DamageBuffer` held in a `Mutex`.
- **The pending flag lives INSIDE that same `Mutex`**, not in a separate
  atomic: the reader folds and arms the flag under one lock hold, then
  sends `Msg::RedrawReady` after unlock iff it transitioned the flag
  false→true; `take_damage` clears the flag and drains the buffer in one
  lock acquisition. A separate flag cleared outside the drain lock loses
  the wakeup for an event folded between drain and clear (a frozen frame
  until the next unrelated engine event); make that schedule
  unrepresentable, and test the interleaving: a fold that lands after
  `take_damage` returns must leave a token pending.
- The `DamageBuffer` exists from `Engine::spawn`; `start_pump` installs the
  sink and immediately sends one token if damage is already staged, so
  redraws that arrive between attach and loop start are never lost. The
  shared state also holds a small FIFO of pending non-coalescible `Msg`s:
  a request or stream-end arriving before the sink is installed is staged
  there, and `start_pump` drains it into the sink in arrival order before
  sending the damage token — startup registers the VimEnter autocmd before
  the loop starts, so a fast config can fire the rpcrequest inside that
  window, and a dropped request leaves the engine blocked forever.
- Bounded channel semantics (size 64, created by the runtime, and SHARED
  with the input thread from the next task — so "full" does not imply a
  token is queued): on a failed `try_send` of `RedrawReady` the sender
  re-acquires the buffer lock and disarms the pending flag, so a later
  fold transitions again and retries the send; the runtime loop's
  residue drain (next task) covers the storm-ended-exactly-then case.
  `EngineStopped` uses a BLOCKING `send` — the reader is exiting anyway,
  a dropped death signal is a permanent runtime hang, and disconnect
  returns `Err` immediately. A failed `try_send` of `EngineRequest` means
  the loop is gone or wedged — the reader logs and exits (document these
  contracts in damage.rs rustdoc; the module doc must state the real
  drop-safety invariant, not a sole-producer assumption).
- Non-coalescible traffic is exactly: `Msg::EngineStopped` (stream end)
  and `Msg::EngineRequest` (known engine-initiated requests). `mode_change`
  and every other redraw-batch event are wire members of `redraw` and fold
  into the DamageBuffer; no separate path exists for them.
- Known engine-initiated requests (method `"view_vim_enter"` this phase)
  become `Msg::EngineRequest(EngineRequest::VimEnter { token })` with the
  msgpack msgid as the `ReplyToken`; unknown request methods keep the
  as-built auto-error reply. The engine blocks until `EngineHandle::reply`
  routes the response through the writer channel.

`DamageBuffer` compaction rules (latest-wins per region): a `GridLine` run
may replace an earlier fully-covered run on the same row-span **only when
no `GridScroll` was staged between them** — a scroll relocates earlier
rows, so it is a compaction barrier for runs staged before it. `GridClear`
drops earlier staged **cell-content** events only (`GridLine`,
`GridScroll`, earlier `GridClear`); it never drops `CursorGoto`,
`HlAttrDefine`, `DefaultColorsSet`, or non-grid events — Clear resets
cells, not cursor position or highlight definitions. `GridResize` is a
full barrier: nothing staged before it is dropped or elided (resize
preserves the overlapping region, so earlier content events still matter).
`Flush` marks the buffer paintable. Correctness first: compaction that
only ever *keeps* extra events is legal (over-painting is idempotent on a
Grid); compaction must never reorder events for the same cell. Property to
test — and the generator MUST be generative (proptest-style hand-rolled
generator is fine), not captured-only, MUST include scroll-interleaved
overwrites of the same rows, and MUST also interleave `CursorGoto` and
`HlAttrDefine` with `GridClear`/`GridResize`: for any event sequence,
applying the compacted batch to a `Grid` (via the `UiEvent` → `GridOp`
translation) yields the same final grid and cursor as applying the raw
sequence (`CursorGoto` is a `GridOp`, so grid application covers it), AND
the compacted batch's subsequence of non-`GridOp` events (`HlAttrDefine`,
`DefaultColorsSet`, and every mode/cmdline/message/tabline/popupmenu/
`Unknown` event) is identical, in content and relative order, to that
subsequence of the raw sequence — the ext events share the buffer and
`MsgShow`'s `replace_last` makes their relative order load-bearing, so
exact subsequence survival is the compactor's contract for everything it
does not understand.

- [ ] **Step 1:** RED tests: (a) the compaction property, generative with
  scroll-interleaved overwrites, plus a captured-storm fixture (reuse the
  Task 2 capture; big `:e` on a generated large file); (b) token dedup: N
  redraw notifications while the consumer never drains produce exactly one
  `RedrawReady` in the channel; (c) the lost-wakeup interleaving: fold
  after `take_damage` returns leaves a token pending; (d) `take_damage`
  returns batches ending at the last staged `Flush` and leaves post-Flush
  partial damage staged; (e) flood: 10k redraw notifications with no drain
  neither block the reader (assert via watchdog) nor grow the channel;
  (f) request dispatch: an incoming `view_vim_enter` request surfaces as
  `Msg::EngineRequest` carrying the request's msgid, and `reply` sends the
  correlated response (assert on the fake engine's read end); an unknown
  method still gets the auto-error; (g) `wait_exit`: normal exit code
  passes through; signal death maps to 128 + sig (unix); (h) pre-sink
  staging: a `view_vim_enter` request delivered before `start_pump`
  surfaces as `Msg::EngineRequest` after it, in arrival order.
- [ ] **Step 2:** `cargo test -p view-engine` — FAIL.
- [ ] **Step 3:** implement. Until Task 4 deletes the unbounded channel,
  the reader BOTH folds redraws into the DamageBuffer AND forwards every
  notification to the deprecated `take_notifications` channel unchanged —
  the P1-shaped bin still paints from that channel at this task's commit,
  and a fold-only reader would blank its screen and fail the pty suite
  inside `task commit`'s ci chain. Delete the forwarding, the channel, and
  the method in the same commit that removes their last caller (Task 4).
- [ ] **Step 4:** `cargo test -p view-engine` — PASS, plus the pre-existing
  handle/spawn/wedged/shutdown suites untouched and green.
- [ ] **Step 5:** `task commit` message must state the latency consequence:
  redraw traffic no longer queues unboundedly; the loop wakes once per
  storm and paints compacted damage; response correlation is unaffected.

---

### Task 4: The unified runtime loop — input thread, executor, zero ticks

**Files:**
- Create: `crates/view/src/runtime.rs`
- Modify: `crates/view/src/main.rs` (shrinks to CLI parse + wiring)
- Modify: `crates/view-tui/src/terminal.rs` (`spawn_input_thread` replaces `drain_input`: blocking `crossterm::event::read()` on a dedicated thread, translating to core `Msg`s via `encode_key` — keys and resize this task; paste and mouse arrive with their `Msg` variants in Task 7; AND add `Term::draw_model(&Model)` this task — it wraps the existing grid+hl paint against the moved core types; Task 5 replaces it with `draw_surface(&Surface)`)
- Test: `crates/view/src/runtime.rs` unit tests + the pty suite must stay green unchanged

**The loop (this code is the deliverable; transcribe, then make it true —
it is the same code as Task 3's consumer sketch, extended with the executor
and paint):**

```rust
pub fn run(mut model: Model, mut engine: Engine, term: &mut Term) -> Result<i32> {
    let (msg_tx, msg_rx) = mpsc::sync_channel(64);
    let pump = engine.start_pump(msg_tx.clone());        // reader feeds msg_tx directly
    view_tui::terminal::spawn_input_thread(msg_tx);      // Key/Resized Msgs (Paste/Mouse arrive later)
    let executor = Executor::new(engine.handle.clone()); // notify-backed, never blocks

    loop {
        let msg = match msg_rx.recv() {
            Ok(Msg::RedrawReady) => Msg::Redraw(pump.take_damage()),
            Ok(Msg::EngineStopped) => Msg::EngineDown(engine.wait_exit()),
            Ok(m) => m,
            Err(_) => Msg::EngineDown(ExitInfo { code: None, by_signal: false }),
        };
        let mut queue = vec![msg];
        let mut drained_residue = false;
        while let Some(msg) = queue.pop() {
            for eff in update(&mut model, msg) {
                match executor.run(eff) {
                    Flow::Continue => {}
                    // run() owns engine: returning here runs Drop (graceful qa! then kill)
                    Flow::Quit(code) => return Ok(code),
                    // an engine write failed: the engine is gone, not the UI;
                    // resolve the real exit status and let update() decide
                    Flow::EngineLost => queue.push(Msg::EngineDown(engine.wait_exit())),
                }
            }
            // a RedrawReady is dropped when the shared channel is momentarily
            // full (the pump disarms pending so a later fold retries); this
            // drain makes a stranded batch impossible: a full channel
            // guarantees another queued wakeup, and every wakeup runs this
            // before the loop can sleep. Once per wakeup, so a sustained
            // storm still paints per batch instead of starving the frame.
            if queue.is_empty() && !drained_residue {
                drained_residue = true;
                let residue = pump.take_damage();
                if !residue.is_empty() { queue.push(Msg::Redraw(residue)); }
            }
        }
        // paint immediately when update marked dirty: there is no timer,
        // no recv_timeout, no tick anywhere in this loop
        if model.dirty {
            term.draw_model(&model)?;   // terminal I/O errors abort; engine errors never do
            model.dirty = false;
        }
    }
}
```

- `Executor::run(eff) -> Flow` — infallible by signature. It maps
  `RpcCall::Input/TryResize/Paste` onto the existing `nvim_api` notify
  helpers (add `paste` to nvim_api: `nvim_paste` with `crlf=false,
  phase=-1` per the api-info you captured in Task 2) and `Effect::Reply`
  onto `EngineHandle::reply`. Any engine-write failure returns
  `Flow::EngineLost` — never an `Err` that would abort the UI while the
  EngineDown path exists precisely for that case. It performs zero
  `request` calls: startup owns the only requests.
- Ownership: `run()` owns `Engine`; the reader and writer threads live
  inside it; `Drop` (graceful qa! then kill) runs exactly once when `run`
  returns. Document the ownership chain in runtime.rs rustdoc.
- The old `should_keep_draining`/budget machinery is deleted with the old
  loop; the flood protection now lives structurally in Task 3's pump.
  `Engine::take_notifications` loses its last caller here: delete it and
  the unbounded notification channel in this task's commit.

- [ ] **Step 1:** RED: unit-test the executor mapping (mock trait over the
  notify surface: define `trait EngineOps` in runtime.rs implemented by the
  real handle, test with a recording fake) and the quit path.
- [ ] **Step 2:** `cargo test -p view` — FAIL.
- [ ] **Step 3:** implement; wire main.rs: startup (still P1-shaped this
  task) then `run()`.
- [ ] **Step 4:** `task ci` — everything green including the pty suite
  (typed text still paints; :cq still propagates; signal-death still 137).
- [ ] **Step 5:** commit; latency sentence mandatory and must state: paint
  now fires on Flush with no silence timeout and input wakes the loop
  directly with no drain-budget delay (the residue drain adds one
  uncontended mutex acquisition per wakeup, negligible); expected
  keypress-to-paint change measured in Task 11.

### Task 5: Surface = render(&Model) in view-surface, BSU/ESU, cursor spec

**Files:**
- Create: `crates/view-surface/src/lib.rs` (replaces the stub; Surface, Layer, LayerKind, CursorSpec, render — the spec's crate layout puts Surface in its own crate between core and the consumers, and P4/P5 depend on that seam)
- Modify: `crates/view-surface/Cargo.toml` (dependency: view-core only — the audit script already forbids everything else for this crate)
- Modify: `crates/view-tui/Cargo.toml`, `crates/view/Cargo.toml` (+ view-surface; both edges verified legal against audit-deps.sh at authoring time)
- Modify: `crates/view-tui/src/paint.rs` (consumes Surface, owns style conversion + z-order compositing)
- Modify: `crates/view-tui/src/terminal.rs` (`draw_model` becomes `draw_surface`; writes wrapped in synchronized-update brackets `CSI ? 2026 h/l` gated on `caps.sync`)
- Test: surface unit tests in view-surface; paint golden test in view-tui

**Interfaces — Produces:**

```rust
// view-surface/src/lib.rs — pure data, no drawing
#[non_exhaustive]
pub struct Surface {
    pub layers: Vec<Layer>,                        // painted in order (z asc)
    pub cursor: Option<CursorSpec>,                // real terminal cursor: IME and
}                                                  // screen readers depend on it
#[non_exhaustive]
pub struct Layer {
    pub rect: Rect,                                // row/col/w/h in cells
    pub kind: LayerKind,
}
#[non_exhaustive]
pub enum LayerKind {
    EngineGrid,                                    // the grid, full-frame at z0
    Cmdline(CmdlineState),
    Messages(Vec<Message>),
    Tabline(TablineState),
    Popupmenu(PopupmenuState),
}
#[non_exhaustive]
pub struct CursorSpec { pub row: u16, pub col: u16, pub shape: CursorShape }
#[non_exhaustive]
pub enum CursorShape { Block, Horizontal(u8), Vertical(u8) }  // u8 = cell percentage
pub fn render(model: &Model) -> Surface;
```

`render` is pure and total: for any Model it yields a Surface whose layers
never exceed the model's grid bounds (clamp, never panic). The cursor comes
from the grid cursor position plus the current mode's `ModeInfo` cursor
shape (decoded in `ModeInfoSet`; this is that data's consumer). view-tui
composites layers into the ratatui frame. View-side damage exists this
phase as the grid damage coalescing plus ratatui's buffer diff at paint;
Surface-tree memoized diffing is measured at P3's bench gates before being
built (recorded in the coverage walk as a user-visible claim).

- [ ] **Step 1:** RED: view-surface tests: engine-only model renders exactly
  one EngineGrid layer with a Block cursor at the grid cursor; a model with
  cmdline state renders it above the grid; insert-mode ModeInfo yields a
  Vertical cursor shape; layers clamp to grid bounds on hostile rects.
- [ ] **Step 2:** FAIL run. **Step 3:** implement render + tui compositing +
  BSU/ESU brackets gated on `caps.sync` (never on tier alone: a probed
  no-sync terminal at Standard must not receive the brackets, and Task 8
  fills the probe; until Task 8, `TermCaps::default()` keeps sync false so
  the gate is conservative by construction). **Step 4:** `task ci` green;
  pty smoke unchanged.
- [ ] **Step 5:** commit + latency sentence (bracketed writes reduce flicker,
  no added per-frame cost beyond two escape sequences).

### Task 6: Native cmdline, messages, tabline, popupmenu rendering

**Layout reality (discovered against a live engine; binds every renderer
below):** with the full ext attach nvim's grid spans the whole terminal —
no row is reserved for chrome, so a layer painted over the grid at rest
destroys visible buffer text. Two mechanisms, chosen per layer:

- **Overlay (transient, painted over grid content only while active):**
  cmdline (bottom row, only while the user is typing a command — matching
  the cmdheight=0 floating UX external UIs give), messages (top-right
  toasts over content; nvim owns their lifetime), popupmenu (anchored
  float; floating over text is its native semantics). Overlays are
  z-above EngineGrid and vanish with their state; the transient
  overwrite of resting text while active is correct UX, not corruption.
- **Row reservation (persistent chrome may never sit over buffer text):**
  the tabline. When tabs > 1, the engine grid must be one row shorter
  than the terminal: `Model` gains `term_width`/`term_height` (fed by
  `Msg::Resized` and startup wiring), `update()` computes the grid target
  as `(term_width, term_height - chrome_rows)` and emits
  `Effect::Rpc(TryResize)` both on `Resized` and on a `TablineUpdate`
  that crosses the 1-tab boundary; `render()` offsets the EngineGrid
  layer rect and the cursor down by the chrome offset. Back to one tab,
  the reservation is released the same way.

The previous task pinned an EngineGrid-only compositor with a
non-corruption regression test; this task deliberately supersedes that
test with the overlay/reservation semantics above (replace it with tests
of the new invariant: resting buffer text is never covered by persistent
chrome, and overlays vanish with their state).

**Files:**
- Modify: `crates/view-core/src/model.rs` + `update.rs` (term dims, chrome offset, TryResize emission; arms from the decode task's state into Surface-ready state; message TTL policy)
- Modify: `crates/view-surface/src/lib.rs` (render: EngineGrid offset + overlay layer rects per the mechanisms above)
- Modify: `crates/view-tui/src/paint.rs` (layer renderers: cmdline bottom-row overlay, messages stacked toasts top-right, tabline in the reserved top row, popupmenu anchored at its grid coords clamped)
- Modify: `crates/view-tui/src/terminal.rs` (cursor application: emit DECSCUSR — `CSI n SP q`, steady variants only this phase: 2 block, 4 underline, 6 bar; blink is unmodeled in `CursorShape` — when the Surface cursor shape changes, and position via `set_cursor` every frame; unit-test the emitted byte sequence with an injected writer)
- Modify: `crates/view-oracle/tests/smoke.rs` (the pty additions below; ALSO fix the stale comment at ~line 332 referencing the deleted `exit_code_for` in main.rs — point it at `exit_info_from_status` in view-engine process.rs and `update()`'s EngineDown arm)
- Test: exact TestBackend full-frame string assertions (never partial "contains" checks: the frame is the contract)

**Behavior contracts (the reviewer checks these, so they are exact):**
- Cmdline: visible iff `CmdlineShow` unhidden; content renders `firstc` +
  chunks; cursor at `pos`; `CmdlineHide` removes the layer on the same Flush.
- Messages: `MsgShow` appends (or replaces last when `replace_last`);
  `MsgClear` clears; no TTL this phase (nvim owns message lifetime; view
  renders state, it does not invent timers for engine-owned UI).
- Tabline renders only when >1 tab (matching bare nvim's default
  showtabline), in a reserved row per the layout mechanism above; the
  grid resize round-trips (open second tab -> grid shrinks by one row;
  close it -> grid regains the row); resting buffer text is never
  covered by the tabline.
- Popupmenu anchors at (row,col) from the event, clamped into the grid;
  `selected` highlights; `PopupmenuSelect` moves it; `Hide` removes.
- [ ] Steps: RED (TestBackend snapshots per contract) → FAIL → implement →
  `task ci` + extend the pty smoke: drive `:echo "hi"` and assert the
  message text appears; drive `:` and assert the cmdline line shows `:`.
- [ ] Commit + latency sentence (overlay composition adds bounded per-layer
  work on paint only).

---

### Task 7: Focus routing, bracketed paste, mouse passthrough

**Files:**
- Modify: `crates/view-core/src/model.rs` (`Focus` gains `Native(OverlayId)` shape; unused until P4 but the routing seam is this phase's contract), `update.rs` (key arm consults focus)
- Modify: `crates/view-tui/src/terminal.rs` (input thread: `Event::Paste` → `Msg::Paste`, mouse events → `Msg::Mouse` with cell coords)
- Modify: `crates/view-core/src/msg.rs` (`Msg::Paste(String)`, `Msg::Mouse(MouseInput)`, `RpcCall::InputMouse { button, action, modifier, row, col }`)
- Modify: `crates/view-engine/src/nvim_api.rs` (`input_mouse` notify helper; verify the exact `nvim_input_mouse` parameter names/order against the Task 2 api-info capture, not memory)

**Contracts:**
- Engine focus: keys → `RpcCall::Input`; paste → `RpcCall::Paste` (never
  replayed as keystrokes: one undo unit, no mapping interference); mouse →
  `RpcCall::InputMouse` (single-grid: grid 0 semantics per api-info).
- Native focus: consumed by the overlay's update arm; Esc returns Engine
  focus. This phase has no native overlay that takes focus; the tests drive
  the routing with a test-only overlay state to pin the seam P4 builds on.
- Terminal mouse capture is enabled only when the engine reports
  `mouse_on` (decode it in Task 2's event set if present in the capture;
  otherwise add it here with its own mini-capture, cited).
- [ ] Steps: RED unit tests (routing table: focus x input-kind → effect) →
  FAIL → implement → `task ci` green; pty test: paste a two-line string,
  assert both lines land and `u` undoes them as one unit.
- [ ] Commit + latency sentence (input path unchanged in cost; paste leaves
  the key path entirely).

### Task 8: Terminal capability tiers

**Files:**
- Create: `crates/view-tui/src/tiers.rs`
- Modify: `crates/view-tui/src/terminal.rs` (`Term::init` splits: enter raw mode FIRST, run detection, THEN enter the alt screen — CSI replies are unreadable in canonical mode: line-buffered, no newline terminator, echoed; probing before raw mode silently times out on every terminal and degrades everyone to Basic, the exact silent-failure class this plan exists to avoid)
- Modify: `crates/view/src/main.rs` (detection result into `Model.caps`)
- Test: unit tests with injected responses; a `--tier` CLI override for deterministic testing and user escape hatch

**Detection (auto, overridable; the queries are decided here, not at
implementation time):** truecolor via `COLORTERM=truecolor|24bit`;
synchronized-update via DECRQM `CSI ? 2026 $ p` (reply mode 1 or 2 =
supported); kitty keyboard via `CSI ? u` (any `CSI ? … u` reply =
supported); all queries sent in one batch followed by DA1 (`CSI c`) as the
fence — every terminal answers DA1, so a DA1 reply with no preceding
capability replies means those capabilities are absent, and the 50ms
deadline is only the safety net for terminals that ignore even DA1.
tmux/SSH: query *through* (tmux passthrough caveat lands in doctor text
later) rather than trusting `TERM`. Produces `TermCaps`: tier is derived
(sync+truecolor+kitty-kbd → Full; truecolor only → Standard; else Basic)
and the probed booleans are kept — behavior gates on the booleans, tier is
UX vocabulary. `--tier full|standard|basic` overrides tier AND derives the
booleans from it (full = all true, standard = truecolor only, basic = none):
an escape hatch must be deterministic, not half-probed. The chosen caps and
why go into a startup log line (stderr, pre-alt-screen only).
- [ ] Steps: verify the DECRQM 2026 and kitty `CSI ? u` reply grammars
  against current xterm ctlseqs and kitty keyboard-protocol docs BEFORE
  writing the fake replies (the sequences above are the plan's recall;
  the docs are the authority — wire-capture rule applies to escape
  protocols too) → RED (mapping table from injected capability sets; a
  fully-replying fake yields Full — the positive path is the one that
  silently breaks; override wins; deadline path with a never-replying fake
  yields all-false caps and never hangs) → FAIL → implement → `task ci`;
  pty smoke asserts view still starts under the dumb pty (Basic path
  exercised for real).
- [ ] Commit; latency sentence: detection cost is startup-only, bounded by
  the 50ms deadline, off the key path.

### Task 9: Theme derivation from live highlight state + cache

**Files:**
- Create: `crates/view-core/src/theme.rs` (Theme derived from HlTable + DefaultColors; pure)
- Modify: `crates/view-tui/src/paint.rs` (style refs resolve through Theme)
- Create: `crates/view/src/theme_cache.rs` (load/store last Theme keyed by resolved config path; `$XDG_STATE_HOME/view/theme-<hash>.toml` or the platform state dir; serialization via `serde` + `toml`, added to the `view` bin crate only, per the Tech Stack list; `<hash>` is FNV-1a over the resolved config path implemented locally in this file — std's `DefaultHasher` is not stable across toolchains and a toolchain bump must not orphan every cache; corrupt/missing cache = defaults, loudly logged, never fatal)
- [ ] Steps: RED (derivation: given hl events for Normal/StatusLine etc.
  produce stable Theme; cache round-trip; corrupt-cache fallback) → FAIL →
  implement (derive from the event stream state you already model: this is
  a read of `Model.engine.hl`, not a new RPC query) → `task ci`.
- [ ] Commit; latency: theme resolution is a lookup on paint, no RPC.

### Task 10: Startup sequence

**Files:**
- Create: `crates/view/src/startup.rs`
- Modify: `crates/view/src/main.rs`, `crates/view/src/runtime.rs`
- Test: pty startup test (shell frame visible before first grid content on a delayed engine; keys typed pre-attach replay in order)

**Sequence (spec-ordered, each step observable):**
1. Load theme cache; paint the shell frame (statusline placeholder + empty
   grid + spinner) via the normal Surface path. Startup shell paint target
   is 50ms from process start: measure with an `Instant` log line in debug
   builds; the formal budget gate is P3's.
2. Spawn engine + attach full ext set immediately (attach precedes config
   sourcing per the embed contract; the existing spawn/handshake already
   guarantees this ordering).
3. Buffer keys typed pre-attach in the runtime (bounded ring of 64; on
   overflow drop-oldest and surface a message layer toast, never silent);
   replay in order through the normal `Msg::Key` path once attached.
4. Post-attach: `VimEnter` hook via a one-shot autocmd registered at attach
   time (`nvim_create_autocmd` with an **rpcrequest** callback sending
   method `view_vim_enter` — the spec mandates a blocking request here, and
   it is the end-to-end proof of the request-dispatch seam: the engine
   blocks until the loop's `update()` arm replies via `Effect::Reply`;
   verify the autocmd callback syntax against the api-info capture). The
   P2 handler re-derives the theme and replies Nil; native mapping
   registration is P4's (the seam exists and is proven under load here —
   a deadlock would hang startup, so the pty startup test doubles as the
   seam's liveness test).
5. First grid Flush swaps spinner for content (a model flag flips; render
   covers it).
- [ ] Steps: RED (pty: with `--nvim-bin` pointed at a wrapper script that
  sleeps 500ms then execs real nvim, assert the shell frame is on screen
  within 200ms and typed keys appear after attach in order) → FAIL →
  implement → `task ci`.
- [ ] Commit; latency sentence: startup path only; key path untouched.

### Task 11: Headless drivers (four-leg contract) + micro-benches + latency re-measurement

**Files:**
- Create: `crates/view-oracle/src/lib.rs` (`Session` + `EngineSession`)
- Create: `crates/view-oracle/src/pty.rs` (`PtySession`, promoted from the test scaffolding duplicated in view-bench/view-oracle tests)
- Create: `crates/view-oracle/src/raster.rs` (pure Surface + Grid → text; NO view-tui dependency — the audit script's crossterm/ratatui reach checks fail the moment view-oracle takes a normal dep on view-tui, and lib code cannot use dev-deps; TestBackend goldens stay inside view-tui's own tests)
- Modify: `crates/view-oracle/Cargo.toml` (deps: view-core, view-surface, view-engine, portable-pty, vt100 — the last two promoted from dev-deps; promotion exposes their transitive graphs to the async-runtime reach checks for the first time, so an audit failure here means a real new leak, not a script bug)
- Modify: `crates/view-engine/src/nvim_api.rs` (typed probe helper `eval_str(&self, expr: &str) -> Result<String, EngineError>` — probes return typed values, never raw rmpv `Value`s, so the oracle stays rmpv-free at its API)
- Modify: `scripts/audit-deps.sh` (view-oracle now reaches rmpv transitively via view-engine: add view-oracle to the rmpv reach allowlist — a deliberate, named policy change; say so in the commit message)
- Create: `crates/view-core/benches/grid_apply.rs`, `crates/view-core/benches/update_key.rs`, `crates/view-engine/benches/damage_fold.rs`, `crates/view-surface/benches/render_frame.rs` (criterion dev-deps; these are the hot-path micro-benches P3's gates consume)
- Modify: `Taskfile.yml` (`bench-micro` target running `cargo bench` for the four bench crates)
- Test: oracle self-tests, one per driver level

**Interfaces — Produces (this is the API the P3 oracle scripts against; the
P3 charter's four requirements are marked):**

```rust
// Msg-level driver: pure, no engine, no terminal (fast oracle path).
Session::new(cols: u16, rows: u16) -> Session
Session::feed(&mut self, msg: Msg)                    // leg (a): Msg-level injection
Session::surface(&self) -> Surface                    // leg (b): deterministic capture
Session::screen_text(&self) -> String                 // raster.rs, pure

// Engine-attached headless driver: real engine, no terminal (truth path).
EngineSession::spawn(cols: u16, rows: u16) -> Result<EngineSession, OracleError>
EngineSession::input(&mut self, notation: &str) -> Result<(), OracleError>
EngineSession::pump_until_flush(&mut self, deadline: Duration) -> bool
    // leg (c): the harness owns all timing; the runtime loop itself has no
    // clock (exit checklist proves it by grep)
EngineSession::surface(&self) -> Surface
EngineSession::screen_text(&self) -> String
EngineSession::eval_str(&mut self, expr: &str) -> Result<String, OracleError>
    // leg (d): engine state-parity probes (buffer text, cursor, mode, registers)

// Pty-level driver: full stack through a real pty (integration path).
PtySession::spawn(cmd: &str, args: &[&str], cols: u16, rows: u16) -> Result<PtySession, OracleError>
PtySession::send(&mut self, bytes: &[u8]) -> Result<(), OracleError>   // leg (a): pty-level injection
PtySession::screen(&mut self) -> String
```

- [ ] **Step 1:** RED self-tests: `Session` fed a scripted Redraw + Flush
  yields the known screen text; `EngineSession` spawned, `input("ihello<Esc>")`,
  `pump_until_flush`, `screen_text()` contains "hello" AND
  `eval_str("getline(1)")` returns "hello" (screen and engine state agree —
  the oracle's own equivalence seed); `PtySession` against
  `target/debug/view` shows a typed character on screen.
- [ ] **Step 2:** FAIL run. **Step 3:** implement, including the raster and
  the bench bodies (bench the storm fixture from Task 3, a `<C-x>` key
  dispatch, a full-model render). **Step 4:** `task ci` green including the
  audit with its amended allowlist; `task bench-micro` runs and reports.
- [ ] **Step 5: re-measure.** `task bench-latency` with the new loop; append
  the run to `.claude/bench-baselines/p1-latency.txt` as a dated second
  entry (do not overwrite the P1 baseline: the delta IS the evidence).
  Expected: view p50 collapses from ~6.8ms toward low-single-digit ms; if
  the structural tick is truly gone and the number does not collapse,
  that is a finding to investigate, not to explain away.
- [ ] **Step 6:** commit + latency sentence (driver and benches are
  off-path; the re-measure numbers go in the commit body).

---

## P2 Exit Checklist

Every item closes with an evidence citation — the command run and its
observed output — never a bare checkmark (planning protocol step 7).

- [x] `task ci` green at the branch tip. Evidence (2026-07-18, tip 2931d22):
  `task ci` exit 0, 27 test suites "test result: ok", zero FAILED
  (~/.claude/tmp/p2-exit-ci.log).
- [x] Latency re-measured (Task 11): both P1 structural components gone;
  baseline file carries the dated before/after pair. Evidence:
  `.claude/bench-baselines/p1-latency.txt` — view p50 6.82ms -> 0.68ms
  (10.0x), ratio 15.38 -> 1.21, dated pair present.
- [x] Zero clocks in the runtime loop, proven: grep at 2931d22 returns two
  comment lines (241, 282) and two test-module hits (542, 570 — both after
  the first `#[cfg(test)]` at line 302); zero clock calls in loop code.
- [x] Headless drivers meet all four P3-charter legs, each cited (all green
  in the tip ci run): Msg-level
  `session_msg_level_injection_yields_the_expected_screen_text`; pty-level
  `pty_session_against_the_view_binary_shows_a_typed_character_on_screen`;
  Surface capture `plain_grid_renders_row_text_joined_by_newlines` +
  `cmdline_overlay_paints_over_the_bottom_row` (raster.rs); harness-owned
  clock: the zero-clock grep above +
  `pump_until_flush_returns_false_at_the_deadline_when_no_flush_arrives`;
  engine probes
  `engine_session_input_and_pump_until_flush_agree_with_eval_str_probe`.
- [x] `task bench-micro` runs; benches report numbers (recorded, not gated:
  gates are P3's). Evidence (2026-07-18, tip, exit 0):
  grid_apply_full_frame_put_line 44.3µs; grid_apply_scroll_full_width
  43.6µs; update_key_dispatch_ctrl_x 16.2ns; damage_fold_storm_500_events
  23.0µs; render_frame_full_model 259ns (~/.claude/tmp/p2-exit-bench.log).
- [x] Manual session. Evidence (2026-07-18, tip): scripted tmux drive of
  the real binary (~/.claude/tmp/p2-manual-session.py, log p2-manual.log)
  10/10 — launch shows file, insert lands, `:` bottom-row overlay, `:echo`
  toast on top rows, `:tabnew`/`:tabclose` tabline, `<C-n>` popupmenu with
  buffer candidates, `:wq` persists the edit, shell prompt + echo intact
  after exit. Separately observed: launch under the user's real nvim
  config rendered NvimTree + lualine + a plugin's error toasts through
  view (plugin-compat spot evidence).
- [ ] Coverage-walk deferrals resolved: each of the three recorded items
  approved by the user or pulled into the phase; none silently expire.
  (Surfaced to the user 2026-07-18; awaiting their call.)
- [ ] `.claude/known-bugs.md` drained or user-approved deferrals only.
  (One item: GitHub Actions CI unverified on a real runner — resolved
  2026-08-03, when the workflow first ran on GitHub Actions.)
- [x] Dogfood note appended (start using view for real edits). Evidence:
  `.claude/dogfood-journal.md` entry dated 2026-07-18.
- [x] P3 plan authored under the charter + planning protocol, against
  the tree. Evidence (2026-07-18):
  `.claude/plans/2026-07-18-p3-oracle-bench-gates.md` (12 tasks, 4
  recorded deferrals), authored at tree 2931d22; fresh-context
  adversarial review round 1 verdict "do not execute as-is"
  (1 Critical + 6 Important + 4 Minor), all fixed same round; round 2
  verified all 11 fixes (RefGrid variant list tree-checked) and
  returned "execute after listed fixes" (1 Important + 3 Minor, all
  applied as specified, pre-approved without a third round). Status:
  approved for execution.
- [x] Deep DRY/SSOT audit (user-ordered 2026-07-18): a dedicated agent runs
  alongside the final whole-branch review, auditing (a) the tree as it
  stands for duplication and single-source-of-truth violations, and (b)
  forward-looking structure for the remaining phases: maintainability,
  module boundaries that keep future LOC minimal, extraction candidates.
  Findings funnel into the final review's single fix wave; the ledger's
  minor roll-up (ui_events.rs map-lookup closures; no pty resize test)
  goes to the same wave.
