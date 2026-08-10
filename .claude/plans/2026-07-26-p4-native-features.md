# P4 Implementation Plan — Native Features + Theming

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development to implement this plan
> task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** the visible product surface — picker, file tree, statusline,
toasts, command palette, theming — native-wins with per-feature opt-out
(spec §9, §5.5, §7; charter "the visible product surface").

**Architecture:** each feature is a sub-component with its own pure
model/msg/update — the pure state and update logic live in
`crates/view-core/src/native/` so `Model` can hold them and `update()`
can match on them without an edge (the crate-seam correction below
records why) — rendered as a `view-surface` overlay layer and acting
only through `Effect::Rpc`. `view-native` owns what genuinely needs a
crate of its own: the workers (nucleo matcher, fs scanner, grep, git
status), `[native]` config parsing, supersession planning, and the
mapping/entry-point planning. A feature registry — a
compile-time table, not a runtime registration list — is the single home
for every feature's id, enabled bit, supersession record, and off switch;
config, the first-run toast, and P6's doctor all read that one table. Input
routing moves from P2's single `Focus` field to an overlay stack whose top
element *derives* focus, so a modal prompt arriving over an open picker
destroys neither. Theming extends `view-core`'s `Theme` with the named
groups native chrome needs and gains the `ColorScheme` re-derive bridge
(autocmd → `rpcnotify`) that the statusline's diagnostics and git segments
also ride.

**Tech Stack:** existing workspace, plus three new dependencies in
`view-native` (each lands with its `scripts/audit-deps.sh` row in the same
commit): `nucleo` (picker matcher, spec decision log), and `ignore` +
`grep-searcher` (live-grep; the crates ripgrep itself is built from).
The `view` bin gains `arboard` for the local clipboard worker (T6 records
the decision and its runner-up). No new dependency in `view-core`, which
stays pure.

**Authored against:** tree at `b07f87a` (branch `dev/p3-oracle-bench`, P3
exit tip at dispatch time; the plan was first drafted at `4cd01bf` and
every interface in "As-built interfaces" below was re-verified against
`b07f87a` source, per planning-protocol step 3 — all matched, so the
re-pin changed no interface text).

**Status:** DRAFT — not approved for execution. Planning-protocol step 5
is complete: the adversarial pass, its re-review, and both rounds'
findings are folded into this text. No task dispatches until the user
approves the plan at P4 start.

## Global Constraints

Hard rules, embedded verbatim per planning-protocol step 8. Every task's
requirements implicitly include this section.

- nvim owns all buffer text. No view subsystem holds authoritative text
  state. Buffer mutation happens only through `Effect::Rpc`. **This binds
  hardest in this phase:** the file tree's file operations, the picker's
  preview pane, and every palette action are RPC calls, never direct
  writes to a buffer view keeps.
- The paint loop never awaits RPC. The RPC reader thread never blocks.
  Every feature that needs off-loop work (matching, scanning, grepping,
  clipboard reads, git status) gets a worker thread and answers with a
  generation-stamped `Msg`, exactly as P2's `HlProbeReply` already does.
- No unwrap/expect/panic in lib crates (workspace lints enforce; do not
  weaken them). Test modules may open with
  `#![allow(clippy::unwrap_used, clippy::expect_used)]`.
- Dependency direction: core ← surface ← {native, ai}; only view-engine
  speaks RPC; only view-tui touches the terminal. `scripts/audit-deps.sh`
  enforces the matrix; any new crate or edge lands with its audit row in
  the same commit. **`view-native` must not gain a `view-engine` edge:**
  it emits `Effect::Rpc` values, and the runtime executes them.
- Performance is a contract: any change touching key dispatch, grid
  apply, or paint states its latency consequence in the commit
  description. Nearly every task in this phase touches at least one of
  the three; the ones that touch none (registry, supersession, CLI
  surface, oracle coverage, bench rows — T1, T2, T7, T15, T16) commit
  without a latency statement, and every task that does touch one
  carries the statement in its own commit step below.
- `serde`/`toml` stay allowlisted per crate: `scripts/audit-deps.sh`
  confines them to the crates that genuinely parse config
  (`check_absent` on every pure layer; `view-core` stays serde-free).
  This phase widens that allowlist exactly once — `view-native` gains
  `serde` + `toml` in T1, an audit-policy change that lands with its
  audit row in the same commit, the same shape as `view-harness`'s
  widening when the bench harness needed its own scenario files.
- Use `task` targets, never raw cargo/git, for build/fmt/lint/test/commit.
  Commit only via `task commit -- -m "<msg>"`.
- Production files stay under 1000 production LOC (`task loc`); tests
  split to separate files once a file approaches the ceiling. The picker
  and the tree will both hit this — plan their module splits up front.
- Comments are WHY-only; doc comments render for users and carry a WHAT
  summary. No session-narrative markers of any kind.
- Hot paths (key dispatch, grid diff, paint) stay allocation-free after
  warmup (spec §3.3).
- Non-conventional commit prefixes are parenthesised scopes:
  `feat(picker):`, `test(bench):` — never `picker:` or `bench:`.

## Echo workstream (user-ratified 2026-08-03)

The shortfall-ledger ratification pinned two echo levers into this phase's
scope, in this order:

1. **Input-thread/runtime-loop unification** — replace the blocking-read
   input thread with one loop polling the terminal fd and the engine stdout
   together (HANDOFF §5.8). The biggest lever: `key-decoded->loop-wake` is
   49.1 µs p50 of view's 139 µs share of a round trip; expected to take
   `echo.minimal` near the ≤ 1.10 bar alone. Both hard rules survive — a
   readiness poll is not an await, and only view-tui touches the terminal.
2. **Incremental rendering** — extend the computed-damage path so
   `view_surface::render` stops rebuilding a full-screen `Surface` per
   frame; guarded by a shadow-equivalence assert (incremental result ==
   full rebuild, debug/CI) and cross-checked by the differential oracle.
   The second lever only: the 68 µs paint share is cache residency, and
   incremental surfaces help least where paint volume is highest
   (scroll/streaming touch most rows).

T16a (lever 1) and T16b (lever 2) are those levers as tasks, in the
ratified order, and both land **before** T16's features-enabled
re-measure so T16 measures the shipped loop rather than the one being
replaced. The four echo `ratio_p50` shortfall entries in
`crates/view-bench/budgets.toml` (dev-linux minimal 1.1719 / heavy
1.1838, dev-macos minimal 1.1066 / heavy 1.114) are the ledger those
two tasks retire — by measurement inside the bar, or by a residual
re-adjudicated with the user, never silently carried; the exit
checklist holds the gate.

## Measurement-layer carry-ins (from the 2026-08-03 dev-macos campaign)

Open harness-semantics questions the P3 exit campaign surfaced. They do not
block P3 exit — the gate fails loudly, never silently, in every case below —
but they are adjudicated during this phase's bench work (T16 step 5 walks
all five and records each ruling; the exit checklist cites the evidence),
not dropped:

1. `--record`'s ratchet-only-tightens semantics can pin a wide-spread cell
   to a below-median draw: `echo.heavy ratio_p50` on dev-macos spans
   0.974–1.218 across quiet-host replicates (per-spawn core placement, not
   load), so a lucky record becomes a bar honest draws then fail.
2. Single-shot gating cannot resolve regressions smaller than ~25% on that
   same cell. Candidates: gate on a replicate median, or placement-robust
   pairing.
3. The `scroll.minimal` headroom factor rests on quiet draws whose max
   (2.438) sits knife-edge under the bar (2.4387); per the campaign
   protocol a gate draw above it is new evidence to adjudicate, not
   automatically a regression.
4. Echo on dev-macos routes to the compiled 1.25 headroom default after the
   falsified host-wide `ratio_p50 = 1.02` key was removed; per-scenario
   spread characterization for the remaining scenarios is open.
5. Auto-staleness is dormant on dev-macos under the compiled defaults (an
   entry is provably spent only ~12-20% inside its bar), so shortfall-ledger
   cleanup on that class stays human judgment.

## Coverage walk (planning protocol step 0)

Every charter deliverable and spec MUST for this phase, mapped to a task
or a recorded deferral. An implementer or reviewer finding a phase
requirement absent from both lists has found a plan defect.

| Requirement | Where |
|---|---|
| Feature registry the config surface and doctor expose (charter Produces) | T1 |
| `[native]` per-feature keys: minimal file + flag read, enough for three states and the off switch (charter config split) | T1 (file) + T7 (`--config` flag, T15's runner drives states through it) |
| Per-feature off switch, exact, never all-or-nothing (§5.5) | T1 |
| Supersession is runtime-only and reversible; nothing in the user's config is ever edited (§5.5) | T2 |
| First-run toast reporting every supersession (charter) | T2 |
| Overlay focus routing: `Focus::Native` gains real consumers; `<Esc>` always returns to `Engine` (§5.3, P2 as-built) | T3 |
| Mouse routed by overlay hit-test: topmost native overlay under the pointer, else the engine (§5.3) | T3 |
| Feature entry points: real nvim mappings post-`VimEnter`, `maparg()` claims reported, `:View` unconditional, per-feature restore (§5.3) | T3a |
| Native overlay layers in `Surface`, z-ordered above engine grid (§7) | T4 |
| Tier degradation for every native surface, golden snapshots per tier (§7, §13) | T4 |
| Theme: named groups for native chrome; border style, padding scale, accent palette shared (§7) | T5 |
| Theme group storage enum-keyed before P4 adds groups (DRY/SSOT audit S2) | T5 — restructure precedes the bridge and every new group |
| `ColorScheme` re-derive bridge (autocmd → rpcnotify) (§7) | T5 |
| Cold-start theme cache keyed by config path, used for first paint (§7) | T5 — verify P2's `theme_cache.rs` against the spec's keying rule |
| Clipboard provider: `g:clipboard` injection so `"+y` works; user's own `g:clipboard` wins (§5.1) | T6 |
| OSC52 emitted when remote; clipboard works identically local and remote (§5.1) | T6 |
| CLI: engine passthrough `+42`, `-c`/`+cmd`, `-d`, `-R`, `-o`/`-O`/`-p`, `-u` (§5.6) | T7 |
| CLI: `--clean` triage tool — bundled engine, no user config, native defaults (§5.6, §12) | T7 |
| CLI: `ls \| view -` via `stdin_fd` at `nvim_ui_attach` (§5.6) | T7 |
| CLI: `--appname` (`NVIM_APPNAME` passthrough) (§11) | T7 |
| Exit codes propagate: `:cq[uit] N` exits view with N (§5.6) | T7 |
| `ext_messages` modal prompts: `confirm`, `return_prompt`, inputlist-class take `Focus::Native`, reply through RPC, never time out (§9) | T8 |
| `emsg`/`echoerr` sticky toasts until dismissed, captured in history (§9) | T9 |
| Transient toasts with timeout + scrollback history (§9) | T9 |
| Owning `ext_messages` forces `cmdheight=0` (§9) | T9 (attach-level and unconditional — see T9's ownership note) |
| Statusline: mode incl. macro `recording @q`, pending `showcmd`, file, diagnostics, git branch, ruler/position; single-line (§9) | T10 |
| `msg_showmode`/`msg_showcmd`/`msg_ruler`/`search_count` routed to statusline segments (§9) | T10 |
| Statusline supersession → `laststatus=0` (§5.5) | T10 |
| Picker on `nucleo`; files / buffers sources; streaming results (§9, decision log) | T11 |
| Picker live-grep — spec-committed v0.1 scope, not optional (charter) | T12 |
| Picker preview pane via RPC buffer read (§9) | T12 |
| File tree: toggleable sidebar, git status decorations, file ops via RPC/fs effects (§9) | T13 |
| Command palette: `ext_cmdline` → centered floating palette, completion via `ext_popupmenu` when cmdline-sourced (§9) | T14 |
| Every shipped feature oracle-covered in all three states (charter exit gate, §5.5, §13.3) | T15 |
| §9 non-interference test per feature: feature open causes no engine state drift (charter exit gate) | T15 |
| Echo lever 1: input-thread/runtime-loop unification (ratified 2026-08-03) | T16a |
| Echo lever 2: incremental rendering, shadow-equivalence guarded (ratified 2026-08-03) | T16b |
| Picker §3.1 rows gated: match ≤ 16 ms @ 100k resident; 1M scan streaming, first page ≤ 100 ms warm (§3.1, P3 deferral 1) | T16 |
| Budgets hold with features enabled — features may not eat the latency win (charter exit gate) | T16 |
| Dogfood note; real daily-driving expected to start this phase (charter exit gate) | Exit checklist |
| P5 plan authored under the planning protocol (charter exit gate) | Exit checklist |

Recorded deferrals (user approval required; nothing here silently
expires):

1. **Full §11 config surface** — precedence chain (flags > env `VIEW_*` >
   file > derived defaults), `VIEW_*` env vars, and the derived-defaults
   audit are P6 scope by the charter's own config split ("P4 owns the
   `[native]` per-feature keys (minimal file + flag read); P6 owns the
   full §11 surface"). P4 ships the `[native]` table and the two CLI
   flags §5.6 names; it does not ship the precedence chain. **Charter-
   sanctioned; no new approval needed.**
2. **`doctor` itself** — P6 deliverable. P4 ships the registry `doctor`
   reads (id, enabled, supersession record, off-switch key) and asserts
   its schema in tests, so P6 wires a consumer to a surface that already
   exists rather than inventing one. **Charter-sanctioned.**
3. **`ext_multigrid`** — spec §5.1 fixes multigrid's arrival at P6 when
   pane composition lands. Every P4 feature is designed single-grid; the
   file tree and picker are `Surface` overlays over one grid, not nvim
   windows. **Spec-sanctioned.**
4. **Post-v0.1 §9 candidates** — which-key-style hint overlay, minimap,
   inline git hunks, popupmenu documentation panel, session/project
   switcher. §9 records these as "post-v0.1 candidates (recorded, not
   dropped — pitch before any is cut)". They are not P4 scope and are not
   being cut here; they remain recorded. **Spec-sanctioned.**

## As-built interfaces this plan builds on

Read from the tree at `b07f87a`, per planning-protocol step 3. Re-verify
with `grep -n "pub " crates/<crate>/src/<file>.rs` if a brief seems stale;
reality wins and the plan gets fixed (protocol step 6).

```rust
// view-core/src/msg.rs -- the vocabulary P4 EXTENDS, never replaces
pub enum Msg {
    Key(Key), Redraw(Vec<UiEvent>), RedrawReady, EngineStopped(Option<String>),
    EngineReady, EngineDown(ExitInfo), EngineRequest(EngineRequest),
    Resized { width: u16, height: u16 }, Paste(String), Mouse(MouseInput),
    HlProbeReply { generation: u64, fg: Option<u32>, bg: Option<u32> },
}                                        // #[non_exhaustive]
pub enum EngineRequest { VimEnter { token: ReplyToken } }   // #[non_exhaustive]
pub struct ReplyToken { pub msgid: u64 }
pub enum ReplyValue { Nil }              // #[non_exhaustive]
pub enum Effect { Rpc(RpcCall), Reply { token, value }, Quit { exit_code: i32 } }
pub enum RpcCall {                       // #[non_exhaustive], core stays rmpv-free
    Input { notation: String }, TryResize { width, height }, Paste { text },
    InputMouse { button, action, modifier, row, col }, GetDefaultHl { generation },
}

// view-core/src/model.rs
pub struct Model { pub engine: EngineModel, pub focus: Focus, pub caps: TermCaps,
    pub dirty: bool, pub running: bool, pub term_width: u16, pub term_height: u16,
    pub content_painted: bool, pub fatal_reason: Option<String>, .. }
pub fn chrome_rows(&self) -> u16         // tabline is the only persistent chrome today
pub enum Focus { Engine, Native(OverlayId) }   // Native has NO consumer yet
pub struct OverlayId(pub u64);                 // nothing constructs this yet
pub struct TermCaps { pub tier: Tier, pub sync: bool, pub truecolor: bool,
                      pub kitty_kbd: bool }
pub enum Tier { Full, Standard, Basic }
pub struct Messages { pub entries: Vec<MessageEntry>, .. }
impl MessageEntry {
    pub fn lines(&self) -> Vec<String>       // splits embedded \n; sizing + paint agree
    pub fn is_persistent(&self) -> bool      // emsg|echoerr|wmsg|lua_error|rpc_error|shell_err
    pub fn is_prompt(&self) -> bool          // kind == "confirm"
}

// view-core/src/theme.rs
pub struct Theme { .. }
impl Theme {
    pub fn from_hl(hl: &HlTable) -> Self
    pub fn normal(&self) -> ResolvedStyle
    pub fn emphasis(&self) -> ResolvedStyle
    pub fn style_for(&self, hl_id: u64, hl: &HlTable) -> ResolvedStyle
    pub fn named(&self, hl: &HlTable, name: &str, fallback: ResolvedStyle) -> ResolvedStyle
}

// view-surface/src/lib.rs
pub struct Surface { pub layers: Vec<Layer>, pub cursor: Option<CursorSpec> }
pub struct Layer { pub rect: Rect, pub kind: LayerKind }
pub enum LayerKind {                     // #[non_exhaustive]
    EngineGrid, Cmdline(CmdlineState), Messages(Vec<String>),
    Tabline(TablineState), Popupmenu(PopupmenuState), Shell,
}
pub fn render(model: &Model) -> Surface  // total: never panics on any Model

// view-engine -- ONLY crate that speaks RPC
EngineHandle::{request, request_timeout, notify, reply, request_probe}
EngineHandle::register_vim_enter_autocmd(channel_id) -> Result<(), EngineError>
EngineHandle::ui_attach(width, height)   // wire::UI_EXT_OPTIONS, single source
pub struct EngineConfig { pub nvim_bin: PathBuf, pub extra_args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>, pub env_remove: Vec<OsString>,
    pub handshake_timeout: Duration, pub shutdown_timeout: Duration,
    hermetic: bool }                     // hermetic is PRIVATE by design
EngineConfig::{default, isolated, with_nvim_bin, with_arg}

// view/src/main.rs -- the CLI as built TODAY (T7 extends it)
struct Cli { file: Option<PathBuf>, nvim_bin: Option<PathBuf>, tier: Option<TierArg> }
// view/src/theme_cache.rs -- P2 shipped a versioned on-disk cache
//   CachedTheme { schema_version, .. } with deny_unknown_fields; T5 verifies
//   its KEY against spec 7's "keyed by config path" rule.

// view-native/src/lib.rs, view-ai/src/lib.rs -- doc-comment stubs, no code yet
```

**The two wire facts this plan does NOT state, and must not.** Per
planning-protocol step 1, these are captured live by the implementer and
pinned as a committed capture artifact (the command plus its verbatim
output), never recalled from this plan or from any model's memory:

1. **The exact `ext_messages` traffic for a `confirm` prompt** (T8). The
   as-built `MessageEntry::is_prompt` doc says nvim emits the question as
   `msg_show` kind `"confirm"` and the answer line separately as
   `cmdline_show`, and re-arms by re-emitting `cmdline_show` ALONE. T8's
   design depends on that shape. Capture it: drive `:confirm q` into a
   real pinned nvim with the identical ext-option set and dump the event
   stream.
2. **The `g:clipboard` provider contract** (T6): the exact table shape
   nvim expects, which functions receive which arguments, and what
   `paste` must return. Capture from `nvim --api-info` plus
   `:help g:clipboard` on the pinned engine.

**Plan-level correction to the charter, recorded per protocol step 6.**
The charter says modal prompt overlays "take focus and reply via RPC" and
names `Msg::EngineRequest` + `Effect::Reply` as the seam both prompts and
the clipboard provider use. Read against the tree, that is right about
the clipboard and wrong about prompts:

| Feature | Engine blocked how | Reply seam |
|---|---|---|
| Modal prompt (`confirm`, `return_prompt`, inputlist) | nvim's own input loop, awaiting a keystroke | `Effect::Rpc(RpcCall::Input { notation })` — T8 |
| Clipboard provider | a real `rpcrequest` from the injected Lua | `Msg::EngineRequest` + `Effect::Reply` — T6 |

Both are "via RPC" and both keep the exactly-once reply discipline T6
states; they are different mechanisms and T8 must not invent an
`EngineRequest` variant for prompts. **This is a derivation from the
as-built source and the capture in step 1 above must confirm it before T8
writes code. If the capture contradicts it, reality wins and this table
gets fixed.**

**Plan-level correction on the crate seam, recorded per protocol step 6.**
Spec §9 says each feature "is a `view-native` sub-component with its own
Model/Msg/update" — read literally, that would put `PromptState`,
`PickerState`, and their siblings in `view-native`, and then `Model`
could not hold them and `update()` could not match on them without a
`view-core → view-native` edge, which the dependency-direction hard rule
(§4: core ← surface ← {native, ai}, `scripts/audit-deps.sh` enforced)
forbids. §4 wins, and §9's phrasing is read as the feature's
*ownership* — a native feature, superseding a plugin, carried in the
native registry — not as its state's crate. The seam, which every
feature task builds on:

- **Pure state + pure update live in `view-core`**, under
  `crates/view-core/src/native/{prompt,toast,statusline,picker,tree,palette}.rs`
  — the identical reasoning that already put the registry table in core:
  core holds the data so surface and native both read it without an edge.
  This is also what keeps `update()` headless-testable and the Msg-level
  oracle able to reach native behavior (§6: "what makes the core
  headless-testable and the oracle cheap").
- **`view-native` owns what actually needs a crate**: the workers (nucleo
  matcher, fs scanner, live-grep, git status), `[native]` config parsing,
  supersession planning, and mapping/entry-point planning. Worker threads
  are spawned by the runtime and answer with generation-stamped `Msg`s.
- **`LayerKind` payloads are named types in core**: `Picker(PickerView)`,
  `Tree(TreeView)`, `Statusline(StatuslineView)`, `Prompt(PromptView)`,
  `Palette(PaletteView)` — each a render-ready view struct produced by
  its state's `view()`, defined beside that state in
  `view-core::native::*` (surface depends on core, so the edge is legal).
- **Dispatch-time overlay geometry lives in core**:
  `crates/view-core/src/native/geometry.rs` defines `OverlayBox`
  (width/height percentages, anchor) and `OverlayBox::rect(term_w,
  term_h) -> OverlayRect`; `update()`'s mouse hit-test (T3) and
  `view-surface::render`'s layer placement (T4) both call the same
  function, so the painted rect and the routing rect cannot drift.

The runner-up — state stays in `view-native` and overlay dispatch is
hoisted out of `update()` into the runtime — was rejected: it moves input
routing off the pure path, so the Msg-level oracle and the headless tests
can no longer see it, which trades a compile-time seam question for a
permanent testing hole.

## File structure (new/changed this phase)

```
crates/view-core/src/
  model.rs          # T3: overlay stack; focus becomes DERIVED, not a field
  msg.rs            # T2/T3a/T5/T6/T9/T11: EngineRequest + RpcCall + Msg arms
  theme.rs          # T5: ChromeGroup enum + named groups for native chrome
  native/           # pure state + update per feature (crate-seam correction
    registry.rs     #   above): core holds the data so surface and native
    geometry.rs     #   both read it without an edge. T3/T4 share geometry.rs
    mappings.rs     # T3a: MappingSpec + the default map table (pure data)
    prompt.rs       # T8: modal prompt state, accepts(), PromptView
    toast.rs        # T9: kind routing, expiry state, history ring, view
    statusline.rs   # T10: segment state + single-line truncating view
    picker.rs       # T11/T12: query/results/selection/preview state, view
    tree.rs         # T13: rows, expand/collapse, decorations, view
    palette.rs      # T14: cmdline + cmdline-sourced popupmenu state, view
crates/view-surface/src/
  lib.rs            # T4: LayerKind native variants + z-order; T16b: render
                    #   goes incremental behind the shadow-equivalence guard
  overlay.rs        # T4: border/padding painting over core's OverlayBox rects
crates/view-native/src/
  lib.rs            # module wiring
  config.rs         # T1: [native] table load
  supersede.rs      # T2: runtime-only, reversible supersession
  mappings.rs       # T3a: registration plan, maparg() claims, :View command
  picker/           # T11/T12: sources.rs, matcher.rs, preview.rs -- workers
  tree/             # T13: fs.rs, git.rs -- workers
  clipboard.rs      # T6: provider state; the WORKER lives in `view`
crates/view-tui/src/
  paint.rs          # T4: painters per native layer, per tier
  terminal.rs       # T16a: blocking input thread becomes a pollable handle
crates/view/src/
  main.rs           # T7: CLI surface
  runtime.rs        # T9: toast-expiry timer worker; T16a: one loop
                    #   polling terminal fd + engine stdout
  startup.rs        # T7: CLI flags feed startup; T16a: attach wiring
                    #   moves onto the unified loop
  clipboard.rs      # T6: worker thread + OSC52 (only view-tui touches the
                    #   terminal, so the OSC52 write routes through it)
  bridge.rs         # T5: autocmd -> rpcnotify bridge registration
                    #   (T10 consumes the bridge; it edits no file here)
view.toml.example   # T1: every registry id keyed; the drift test reads it
corpus/native-*.toml          # T15: three-state oracle entries
compat/scenarios/*.toml       # T15: states[] added to UI-owning scenarios
crates/view-bench/src/scenarios/picker.rs   # T16
crates/view-bench/baselines/*.toml          # T16: picker rows
scripts/audit-deps.sh         # T1/T6/T11/T12/T13: new crate edges + dep rows
```

---

### Task 1: Feature registry + `[native]` config

**Files:**
- Create: `crates/view-core/src/native/registry.rs`,
  `crates/view-native/src/config.rs`
- Modify: `crates/view-core/src/lib.rs` (module), `scripts/audit-deps.sh`
  (view-native gains `toml`/`serde` — see the serde note below)

**Consumer call-site first** (protocol step 2). Two shapes, both written
before either was implemented:

```rust
// Option A -- compile-time table, enabled bit resolved from config (CHOSEN)
for f in registry::features() {
    println!("{:<12} enabled={:<5} supersedes={:<10} off: {}",
             f.id, cfg.enabled(f.id), f.supersedes.unwrap_or("-"), f.off_switch);
}
// picker       enabled=true  supersedes=telescope  off: native.picker = false
// tree         enabled=true  supersedes=neo-tree   off: native.tree = false
// statusline   enabled=false supersedes=lualine    off: native.statusline = false

// Option B -- runtime registration, each feature registers as it starts
registry.register(FeatureDesc { id: "picker", .. });   // called from picker::init
```

**Chosen A over B** because B makes the honesty machinery unreliable in
exactly the case it exists for: a feature that fails to initialize never
registers, so it silently vanishes from `doctor`'s list and from the
first-run supersession toast — the user is told nothing was superseded
while the plugin they expected to win is still not winning. With A the
table is closed at compile time and `enabled` is a resolved fact, so a
failed feature reports as present-and-broken instead of absent.

**Interfaces:**

```rust
// view-core/src/native/registry.rs -- pure data, no I/O, no serde
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeatureDesc {
    /// Stable id. This is the consumer-facing key: it is the `[native]`
    /// table key, the doctor row name, and the off-switch name, and it
    /// may never drift between the three.
    pub id: &'static str,
    /// The plugin surface this feature takes over when enabled, or `None`
    /// for a feature that supersedes nothing.
    pub supersedes: Option<&'static str>,
    /// The exact line a user writes to turn this feature off, rendered
    /// verbatim by doctor and by the first-run toast. A rendered string
    /// rather than a derived one: a user who is told `native.picker =
    /// false` must be able to paste it, and deriving it at three call
    /// sites is three chances to drift.
    pub off_switch: &'static str,
}

#[must_use]
pub fn features() -> &'static [FeatureDesc];

/// Whether `id` names a feature in the table. The `[native]` loader uses
/// this to reject an unknown key loudly instead of silently ignoring a
/// typo -- `pickr = false` must not read as "picker stays on".
#[must_use]
pub fn is_feature(id: &str) -> bool;
```

```rust
// view-native/src/config.rs
#[must_use]
pub struct NativeConfig { .. }
impl NativeConfig {
    /// Every feature on: the config-absent default, and what `--clean`
    /// resolves to (spec 5.6: "bundled engine, no user config, native
    /// defaults").
    pub fn all_enabled() -> Self;
    /// Parses a `[native]` table. Unknown keys are an error, not a
    /// no-op.
    pub fn from_toml_str(s: &str) -> Result<Self, NativeConfigError>;
    /// Reads `$XDG_CONFIG_HOME/view/view.toml`, or `all_enabled()` when
    /// the file is absent. An absent file is the full experience
    /// (spec 11); an unparseable file is an error the CLI reports.
    pub fn load(config_path: Option<&Path>) -> Result<Self, NativeConfigError>;
    #[must_use] pub fn enabled(&self, id: &str) -> bool;
}
```

**serde placement.** `view-core` stays serde-free (audit `check_absent`);
the table above is `&'static str` data with no derive. `view-native`
gains `serde` + `toml` and its audit row in this task's commit. It is a
shipping crate, not a bin, so this is a real widening — justified because
§11's config file is a shipping surface and the alternative (parsing
`[native]` in the `view` bin and passing a resolved struct down) puts the
schema and its error messages in the binary crate where neither
`view-native`'s tests nor the oracle can reach them.

**Why first:** every later task registers into this table, and T2's
supersession, T15's three-state oracle coverage, and P6's doctor all read
it. Building any feature before the registry means retrofitting the
honesty machinery onto features that already shipped without it.

**Falsifiable check:** a test walks `registry::features()` and asserts
every id appears as a key in `view.toml.example`, and that
`NativeConfig::from_toml_str("[native]\npickr = false\n")` is an `Err`
naming the unknown key. Deleting a feature's row from the table makes
the first assertion fail.

- [ ] **Step 1: Failing test for the unknown-key rejection.** Write
  `from_toml_str` returning `Ok(all_enabled())` unconditionally, and the
  test asserting `pickr` is rejected. Run it: expect FAIL, and read the
  failure — it must fail on the missing error, not on a compile error.
- [ ] **Step 2:** Implement `NativeConfig::from_toml_str` with
  `deny_unknown_fields` plus a `registry::is_feature` cross-check. Test
  passes.
- [ ] **Step 3: Failing test for table/example drift.** Assert every
  `registry::features()` id is a key in `view.toml.example`. Write the
  example file with one id deliberately missing; observe FAIL naming it;
  add it; observe pass.
- [ ] **Step 4:** Populate the table with the five §9 features (picker,
  tree, statusline, notifications, palette) and their `supersedes`
  values from spec §9's own "Replaces for the 90% case" column.
- [ ] **Step 5: Disconfirm.** Delete `picker`'s row; `task test` fails
  naming it; restore; passes.
- [ ] **Step 6:** `scripts/audit-deps.sh` gains view-native's serde/toml
  row. `task ci`. Commit: `feat(native): feature registry and [native] config`.

---

### Task 2: Supersession — runtime-only, reversible, and reported

**Files:**
- Create: `crates/view-native/src/supersede.rs`
- Modify: `crates/view-core/src/msg.rs` (`RpcCall::SetOption` +
  `OptionValue`)

**Consumer call-site first:**

```rust
// applied post-VimEnter, only for features that are enabled
let plan = supersede::plan(&cfg, registry::features());
// -> [Supersession { feature: "statusline", rpc: RpcCall::SetOption {
//        name: "laststatus".into(), value: OptionValue::Int(0) },
//      reverses_with: "native.statusline = false" }]
for s in plan.iter() { effects.push(Effect::Rpc(s.rpc.clone())); }
toast::first_run(&plan);   // "view is drawing the statusline (lualine still
                           //  loads). Turn it off with native.statusline = false"
```

**Options ride an API call, never the keyboard.** `RpcCall` gains

```rust
SetOption { name: String, value: OptionValue },   // nvim_set_option_value
// with pub enum OptionValue { Int(i64), Bool(bool), Str(String) } -- core
// stays rmpv-free; the runtime maps it to the wire value
```

because `RpcCall::Input` shares a stream with §8 step 3's buffered-key
replay and with live typeahead: an ex-command interleaved into that
stream lands wherever the mode happens to be (a user left in insert mode
by replayed keys would get `:set laststatus=0` typed into their buffer).
Every supersession entry, and any other non-interactive intent this
phase writes, uses `SetOption` (or a dedicated `RpcCall` variant); the
one legitimate `Input` use in this phase is T8's prompt answers, where
nvim is genuinely waiting in its own input loop for a keystroke.

**Runner-up rejected:** having each feature apply its own supersession
inside its own `init`. It reads fine at the call site but there is then
no single place that can answer "what has view taken over?" — the exact
question `doctor` and the first-run toast both ask — so the answer would
be reassembled by inspection at two sites and drift.

**The hard rule this task exists to honor:** supersession is *runtime
only and reversible*. Nothing in the user's config files is ever edited,
and nothing needs removing from `init.lua`. Every entry in the plan is an
RPC call against the live session; a superseded plugin keeps loading and
its cost is memory, not conflict.

**Falsifiable check:** an oracle entry runs the heavy fixture with
`native.statusline = true`, asserts `&laststatus == 0` via an RPC probe,
then runs the identical fixture with `native.statusline = false` and
asserts `&laststatus` is whatever the user's config set. A test asserting
the fixture's `init.lua` bytes are identical before and after both runs
pins "nothing in the user's config is ever edited".

- [ ] **Step 1: Failing test.** `plan()` returns an empty vec; assert an
  enabled statusline yields exactly one entry whose `reverses_with`
  equals that feature's `off_switch` from the registry. FAIL.
- [ ] **Step 2:** Implement `plan()` over the registry, skipping disabled
  features. Pass.
- [ ] **Step 3: Failing test for the config-untouched invariant.** Hash
  the fixture's `init.lua` before and after applying a plan; assert
  equal. Then deliberately add a plan entry that writes the file, observe
  FAIL, remove it, observe pass. (The disconfirming direction matters
  more than the confirming one here: a test that only ever sees a
  non-writing implementation proves nothing.)
- [ ] **Step 4:** First-run toast. It fires once per feature per config
  path, records that in the cache dir beside the theme cache, and names
  the exact off switch verbatim from the registry.
- [ ] **Step 5:** Oracle entry for the `laststatus` pair above.
- [ ] **Step 6:** `task ci`. Commit: `feat(native): runtime supersession with
  reversal reporting`.

---

### Task 3: Overlay stack — focus becomes derived

**Files:**
- Create: `crates/view-core/src/native/geometry.rs` (`OverlayBox` — the
  rect function T4's placement shares)
- Modify: `crates/view-core/src/model.rs`,
  `crates/view-core/src/update.rs`

**Consumer call-site first.** Two shapes:

```rust
// Option A -- overlay STACK, focus derived from the top (CHOSEN)
let picker = model.push_overlay(OverlayBox::new(80, 60), OverlayKind::Picker(picker_state));
assert_eq!(model.focus(), Focus::Native(picker));
// a confirm prompt arrives while the picker is open:
model.push_overlay(OverlayBox::new(40, 20), OverlayKind::Prompt(prompt_state));
// ...prompt answered, popped, and the picker is still there, still focused:
model.pop_overlay();
assert!(matches!(
    model.overlays().last().map(|o| &o.kind),
    Some(OverlayKind::Picker(_))
));

// Option B -- P2's shape kept: one `focus: Focus` field, overlays in Options
model.picker = Some(picker_state);
model.focus = Focus::Native(OverlayId(1));
model.prompt = Some(prompt_state);
model.focus = Focus::Native(OverlayId(2));   // who restores the picker's focus?
```

**Chosen A over B.** B's defect is visible in its own call site: focus and
overlay-presence are two facts that must agree, and nothing makes them.
Every push and pop is a chance to leave `focus` naming an overlay that
was closed, which routes keys into a component that no longer exists. A
derives focus from the stack top, so that state is unrepresentable. The
stack is also *required*, not merely tidy: nvim can emit a `confirm` while
a picker-triggered `:edit` is in flight, so two overlays genuinely coexist.

**Interfaces (as built):**

```rust
// view-core/src/model.rs
pub struct Overlay { pub id: OverlayId, pub geometry: OverlayBox, pub kind: OverlayKind }

/// Feature state lives HERE, inside the stack element. Later tasks add a
/// variant carrying their own state; none of them adds an `Option` field
/// on `Model`.
#[non_exhaustive]
pub enum OverlayKind { Bare /* T11: Picker(PickerState), T8: Prompt(PromptState), ... */ }

impl Model {
    /// Who owns input this frame: the top overlay if any, else the engine.
    /// Derived rather than stored -- a stored focus can name a closed
    /// overlay, and the routing arms in `update()` would then send keys
    /// into nothing.
    #[must_use] pub fn focus(&self) -> Focus;
    /// Topmost overlay whose rect contains (row, col), else `None` --
    /// the engine. Mouse routing calls this, not `focus()`.
    #[must_use] pub fn overlay_at(&self, row: u16, col: u16) -> Option<OverlayId>;
    /// The stack, read-only. `overlays` itself is private so no caller can
    /// push an id the model did not issue.
    #[must_use] pub fn overlays(&self) -> &[Overlay];
    #[must_use] pub fn top_overlay_mut(&mut self) -> Option<&mut Overlay>;
    /// The ONLY way to open an overlay: it issues the id from a monotonic
    /// counter, so duplicate ids are unrepresentable and `overlay_at` can
    /// never be ambiguous.
    pub fn push_overlay(&mut self, geometry: OverlayBox, kind: OverlayKind) -> OverlayId;
    pub fn pop_overlay(&mut self) -> Option<Overlay>;
    /// The rect an overlay occupies at the model's current terminal size.
    #[must_use] pub fn overlay_rect(&self, overlay: &Overlay) -> OverlayRect;
}

// view-core/src/native/geometry.rs (shared with T4 -- see the crate-seam
// correction): each overlay carries its OverlayBox, and
// OverlayBox::rect(term_w, term_h) is the ONE function both update()'s
// hit-test and view-surface::render's placement call, so the painted
// rect and the routing rect cannot drift.
pub enum Anchor { Center, Left, Right, Top, Bottom }
pub struct OverlayBox { pub width_pct: u16, pub height_pct: u16, pub anchor: Anchor }
impl OverlayBox {
    /// Centered; pair with `with_anchor` for T13's left-flush sidebar.
    #[must_use] pub fn new(width_pct: u16, height_pct: u16) -> Self;
    #[must_use] pub fn with_anchor(self, anchor: Anchor) -> Self;
    #[must_use] pub fn rect(&self, term_w: u16, term_h: u16) -> OverlayRect;
}
```

**Decision, binding on every later task in this phase: overlay state lives
in the stack element, never in a parallel `Option` field on `Model`.** A
feature that opens an overlay adds an `OverlayKind` variant carrying its
state and reaches it through `overlays()` / `top_overlay_mut()`. The
alternative -- `OverlayKind` as a bare tag beside `model.picker:
Option<PickerState>` -- reintroduces exactly the two-facts-that-must-agree
defect that sank Option B, one level down: the stack would say a picker is
open while the field says it is not. `OverlayKind` is `#[non_exhaustive]`
so adding a variant is not a breaking change, and `Overlay` therefore
cannot be `Copy`/`Eq` once a variant carries state -- it is `Clone` +
`PartialEq` only.

A mouse gesture belongs to whoever took its `press` until the matching
`release` (`Model::mouse_capture`), so a drag that crosses the overlay
boundary cannot strand nvim mid-selection; `pop_overlay` drops a capture
the closed overlay held.

**Keys and paste route on `focus()`; mouse routes by hit-test** (§5.3:
clicks/wheel go to the topmost native overlay *under the pointer*,
otherwise to the engine). Focus alone would be wrong for the mouse: with
a picker open, a click on a visible engine-grid cell outside the picker
must fall through to the existing `InputMouse` path, not be swallowed by
the focused overlay.

`Model::focus` changes from a public field to a method. This is a
breaking change to P2's as-built surface, made deliberately and named
here so the reviewer checks it: every existing `model.focus` read becomes
`model.focus()`, and the two existing writes (`update()`'s `<Esc>` arm)
become a `pop()`.

**Falsifiable check:** `<Esc>` with two overlays stacked pops exactly one
and leaves focus on the lower overlay, not on the engine — the behavior a
single-field model cannot express. A test drives push/push/Esc and
asserts the picker still holds focus. For the mouse: with an overlay
open (a synthetic overlay state here; the picker is the eventual
consumer, and T15's runs exercise it for real), a click on a cell inside
the overlay's rect routes to that overlay's update, and a click outside
it produces the `InputMouse` `RpcCall` — the engine cursor moves.

- [ ] **Step 1: Failing test** for the push/push/Esc sequence above,
  against P2's current single-field model. Observe FAIL (it will report
  focus on `Engine`, which is the bug).
- [ ] **Step 2:** Add a private `overlays: Vec<Overlay>` with its id
  counter, `push_overlay`/`pop_overlay`/`overlays()`, and
  `Model::focus()`; delete the `focus` field. Fix every call site the
  compiler names.
- [ ] **Step 3:** `update()`'s Key and Paste arms route on `focus()`;
  the Mouse arm routes on `overlay_at(row, col)` — topmost overlay
  containing the point, else the existing `InputMouse` path — with a
  press-to-release capture so a drag stays with its owner. `<Esc>`
  pops one overlay. Both directions of the mouse check above pass.
- [ ] **Step 4: Totality.** Property test: any sequence of pushes, pops
  and `<Esc>` keys leaves `focus()` consistent with `overlays().last()`
  and every issued id distinct.
- [ ] **Step 5:** Latency check — this is key-dispatch code. Run
  `task bench -- --scenario input_path --fixture minimal --class dev-linux`
  before and after; the commit description states the delta. A stack
  lookup replacing a field read must not move the number outside the
  measured noise band; if it does, that is a finding, not an acceptable
  cost.
- [ ] **Step 6:** `task ci`. Commit: `refactor(core): derive focus from an
  overlay stack` with the latency delta in the description.

---

### Task 3a: Feature entry points — mappings, `:View`, key-claim reporting

**Files:**
- Create: `crates/view-core/src/native/mappings.rs`,
  `crates/view-native/src/mappings.rs`
- Modify: `crates/view-core/src/msg.rs` (`Msg::FeatureInvoke`,
  `Msg::MappingsClaimed`, `RpcCall::RegisterMappings`),
  `crates/view-core/src/update.rs` (the invoke arm)

Spec §5.3's entry-point contract, without which no feature in this plan
is reachable: native entry points are **real nvim mappings**
(`<leader>ff` → `rpcnotify` back to view) so user remaps, which-key, and
plugin introspection all see them. They register post-`VimEnter` — the
same seam as `register_vim_enter_autocmd` — so `mapleader` is the
user's. For each *enabled* feature, view's default keys are claimed even
over a user mapping, `maparg()` checked first and **every claim recorded
into T2's supersession plan** so the first-run toast and doctor report
key claims and option supersessions through one mechanism, each with the
exact `[native]` key to flip. Setting a feature `false` restores the
user's mapping untouched; non-colliding user mappings are never touched.
Every feature is *also* reachable via an unconditional `:View` command
(`:View pick files`) — registered regardless of the enabled bit, so a
user who turned the default keys off still has a path in. The full
default map set renders into the docs from `default_maps()`, one table,
one source.

**Consumer call-site first:**

```rust
// post-VimEnter, alongside T2's supersession application
effects.push(Effect::Rpc(mappings::register_plan(&cfg, channel_id)));
// engine side (one Lua chunk): for each spec -- maparg() check, set the
// map to `<Cmd>call rpcnotify(chan, 'view_invoke', 'picker', 'files')<CR>`,
// register :View -- and notify the claims back:
//   Msg::MappingsClaimed { claimed } -> folded into T2's report
// later, the user presses <leader>ff (or runs :View pick files):
//   Msg::FeatureInvoke { feature: "picker", verb: "files" }
//   -> update() opens the overlay (T11 lands the picker arm)
```

**Interfaces** (later tasks rely on these exact names):

```rust
// view-core/src/native/mappings.rs -- pure data, like the registry
pub struct MappingSpec { pub feature: &'static str,
    pub lhs: &'static str, pub verb: &'static str }
/// A default key that was set over an existing user mapping: what was
/// claimed, and the off switch that gives it back.
pub struct MappingClaim { pub feature: String, pub lhs: String,
    pub had_user_mapping: bool }
#[must_use] pub fn default_maps() -> &'static [MappingSpec];

// view-core/src/msg.rs
Msg::FeatureInvoke { feature: String, verb: String },
Msg::MappingsClaimed { claimed: Vec<MappingClaim> },
RpcCall::RegisterMappings { specs: Vec<MappingSpec>, channel_id: u64 },

// view-native/src/mappings.rs -- the planner; reads config + registry
#[must_use] pub fn register_plan(cfg: &NativeConfig, channel_id: u64) -> RpcCall;
```

**Runner-up rejected:** registering each mapping as its own RPC call
from the runtime, with `maparg()` probed per key in separate requests.
That splits one fact ("what did view claim?") across N replies
interleaved with startup traffic, and the claim report would be
reassembled view-side from partial answers. One `RegisterMappings` chunk
executes atomically engine-side and returns the complete claim list in
one notification.

**Falsifiable check:** a fixture maps `<leader>ff` to a user command.
With `native.picker = true`, pressing `<leader>ff` opens the picker and
the claim is reported (toast text names `native.picker = false`
verbatim). With `native.picker = false`, the user's mapping fires
untouched — asserted via an RPC probe of `maparg('<leader>ff')` and the
command's observable effect.

- [ ] **Step 1: Failing test** for the per-feature restore: a plan built
  from a config with `picker = false` contains no picker spec, and the
  fixture's `<leader>ff` probe shows the user mapping intact. FAIL
  first against a planner that registers unconditionally.
- [ ] **Step 2:** `default_maps()` + a drift test: every spec's
  `feature` is a `registry::features()` id, and every feature with an
  entry point has at least one spec.
- [ ] **Step 3:** `register_plan` + the engine-side chunk: `maparg()`
  check, map registration, `:View` command (unconditional), claims
  notified back as `Msg::MappingsClaimed`, folded into T2's report.
- [ ] **Step 4:** `Msg::FeatureInvoke` routing arm in `update()`; until
  T11 lands the picker, the test asserts the decoded Msg reaches the
  arm and is dropped loudly (logged), never silently.
- [ ] **Step 5: Disconfirm.** Remove the claim recording; the
  claim-reported assertion in the falsifiable check fails naming the
  missing claim; restore.
- [ ] **Step 6:** `task ci`. Commit: `feat(native): feature entry
  mappings with claim reporting and :View`. The description states the
  latency consequence (`input_path` bench delta — this adds routing arms
  to `update()`).

---

### Task 4: Native overlay layers + per-tier painting

**Files:**
- Create: `crates/view-surface/src/overlay.rs`
- Modify: `crates/view-surface/src/lib.rs`, `crates/view-tui/src/paint.rs`

**Consumer call-site first:** every native feature builds its layer the
same way, so the geometry lives once:

```rust
// in view-surface::render, for each overlay in model.overlays()
for ov in model.overlays() {
    let rect = model.overlay_rect(ov);
    //         ^ core's geometry.rs -- the SAME rect update()'s mouse
    //           hit-test uses (T3), so paint and routing cannot disagree
    let kind = match &ov.kind {
        OverlayKind::Picker(state) => LayerKind::Picker(state.view()),
        OverlayKind::Bare => continue,
    };
    layers.push(overlay::framed(rect, kind, theme.border_style(model.caps.tier)));
}
```

**Runner-up rejected:** each feature computing its own rect from
`term_width`/`term_height`. Five features clamping independently is five
chances to place a layer outside the frame — the exact class
`clip_to_frame` exists to backstop, and a backstop is not a design. The
rect math itself lives in `view-core::native::geometry` (not here)
because `update()` needs it at dispatch time for mouse hit-testing and
core cannot call into surface; `overlay.rs` owns what only paint needs —
border, padding, and title framing over a rect it is handed.

**Tier degradation is a tested surface, not a fallback apology** (§7):
`full` gets rounded borders, `standard` gets the same layout with plain
borders and no animation, `basic` gets ASCII-safe borders and no color
derivation. Golden snapshots per tier per overlay, per §13.

**Falsifiable check:** a golden-snapshot test renders every native
overlay at all three tiers into `view_oracle::raster::screen_text` output
and diffs against committed goldens. Changing a border glyph fails the
`full` golden and leaves `basic` green.

- [ ] **Step 1:** `LayerKind` gains `Picker(PickerView)`,
  `Tree(TreeView)`, `Statusline(StatuslineView)`, `Prompt(PromptView)`,
  `Palette(PaletteView)` — payload types from `view-core::native::*`
  per the crate-seam correction. It is `#[non_exhaustive]`, so this is
  additive; every `match` in `view-tui` gains an arm and the compiler
  names them all.
- [ ] **Step 2: Failing golden test** for the picker overlay at tier
  `full` with no golden committed. Observe FAIL. Commit the golden after
  reading it — a golden accepted without being read is a snapshot of a
  bug.
- [ ] **Step 3:** Painters for each layer in `view-tui/src/paint.rs`,
  reading `model.caps.tier` for border and color decisions. Never gate on
  tier where a probed bit is the real predicate: BSU/ESU gates on
  `caps.sync`, exactly as P2 built it.
- [ ] **Step 4:** Goldens for all three tiers × five overlays.
- [ ] **Step 5: Disconfirm.** Change one border glyph; exactly the `full`
  and `standard` goldens fail and `basic` passes; revert.
- [ ] **Step 6:** Latency check — this is paint code. `task bench --
  --scenario output_path` before/after, delta in the commit description.
- [ ] **Step 7:** `task ci`. Commit: `feat(surface): native overlay layers
  with per-tier painting`.

---

### Task 5: Theming — named groups, ColorScheme bridge, cache keying

**Files:**
- Create: `crates/view/src/bridge.rs`
- Modify: `crates/view-core/src/theme.rs`, `crates/view/src/theme_cache.rs`,
  `crates/view-core/src/msg.rs` (`Msg::ColorSchemeChanged`,
  `RpcCall::RegisterBridge`)

**Consumer call-site first:**

```rust
// per-frame chrome paints use the O(1) enum accessor -- no string
// compare, no allocation, so §3.3's allocation-free-after-warmup rule
// holds trivially on the paint path
let border = theme.chrome(ChromeGroup::Border);
let accent = theme.chrome(ChromeGroup::Accent);
let sel    = theme.chrome(ChromeGroup::Selected);
// Theme::named stays for genuinely dynamic names (a user override
// naming an arbitrary hl group); it is not the per-frame path
let custom = theme.named(hl, user_group_name, theme.normal());
```

`Theme::named` already exists as-built with exactly this fallback shape,
so native chrome extends P2's mechanism rather than adding a parallel
one. The runner-up — a `NativeTheme` struct carrying resolved colors —
was rejected because it would have to be rebuilt on every
`hl_attr_define` and would drift from the engine's live highlight state
between rebuilds, which is precisely what §7's "live highlight state, not
a one-shot query" rule forbids.

**Group storage goes enum-keyed before any group is added (DRY/SSOT
audit S2).** `Theme`'s named chrome groups are a 5-site shotgun today
(Theme field, `from_hl` body, `CachedTheme` field, and both `From`
impls), and the failure is silent: a group added to `Theme` but missed
in the `CachedTheme` mirror compiles clean and never persists, so
cold-start paints stale defaults for exactly that element —
`deny_unknown_fields` catches renames on load, not omissions on store.
The audit's ruling stands: not urgent before groups multiply, wrong to
do after. So T5 restructures BEFORE registering the bridge or adding
any P4 group: a `#[non_exhaustive] enum ChromeGroup` with
`ALL`/`hl_name()`/fallback-kind, `Theme` holding
`[ResolvedStyle; ChromeGroup::COUNT]` behind
`pub fn chrome(&self, g: ChromeGroup) -> ResolvedStyle` (the paint-side
accessor in the call site above), `from_hl` looping
`ChromeGroup::ALL`, and the cache serializing a map keyed by
`hl_name()` — adding a group then touches exactly one enum arm plus its
consumers. This is the *persisted* storage layer; `Theme::named` stays
as the dynamic-lookup API above. The two reconcile by ownership: any
group the cold-start cache must carry for first paint rides the enum;
ad-hoc `named` lookups against live `hl` state stay dynamic and
uncached. (Full finding: `.superpowers/sdd/dry-ssot-audit.md` §S2.)

**The ColorScheme bridge.** `Effect::Rpc(RpcCall::RegisterBridge)`
registers one autocmd group whose callbacks `rpcnotify` view on
`ColorScheme`, `DiagnosticChanged`, and the git-branch triggers T10
needs. One bridge, three consumers — §7 says the statusline's diagnostics
and git segments ride the same bridge as the theme re-derive, and
registering three separate autocmds would make three chances for one to
be missed on an engine restart.

**Cache keying is a verification, not an assumption.** Spec §7 requires
the cold-start theme cache be "keyed by config path". P2 shipped
`crates/view/src/theme_cache.rs`. This task READS that file and either
confirms the key matches the spec or fixes it. Do not assume it does.

**Falsifiable check:** an oracle entry sets a colorscheme mid-session and
asserts a native overlay's border color changed in the rendered output
without a restart. Two configs at different paths write two distinct
cache files, verified by listing the cache dir.

- [ ] **Step 1:** Read `theme_cache.rs`; write down what it actually keys
  on. If it is not the config path, that is a P2 defect and this task
  fixes it — record which was true.
- [ ] **Step 2:** The ChromeGroup restructure above, landing with the
  existing seven groups only — no P4 groups yet. Regression pin: a test
  that a group present in the enum round-trips Theme → cache → Theme
  for every arm (`ChromeGroup::ALL`), so a future arm missing from the
  cache map is a red test, not a silent stale default.
- [ ] **Step 3: Failing test** for two config paths yielding two cache
  entries.
- [ ] **Step 4:** Bridge registration, at the same point in startup as
  `register_vim_enter_autocmd` (which as-built must be registered BEFORE
  `ui_attach` returns — read `nvim_api.rs`'s doc comment for why, and do
  not repeat the race it describes).
- [ ] **Step 5:** `Msg::ColorSchemeChanged` re-derives `Theme::from_hl`
  and writes the cache.
- [ ] **Step 6: Disconfirm.** Break the bridge registration; the oracle
  entry fails with the border unchanged; restore.
- [ ] **Step 7:** `task ci`. Commit: `feat(theme): ColorScheme re-derive
  bridge and native named groups`. The description states the latency
  consequence (`output_path` bench delta — this touches paint-side theme
  lookups; the enum accessor should keep it inside noise).

---

### Task 6: Clipboard provider

**Files:**
- Create: `crates/view-native/src/clipboard.rs`, `crates/view/src/clipboard.rs`
- Modify: `crates/view-core/src/msg.rs`

**Wire fact — capture, never recall.** The exact `g:clipboard` table
shape is captured live per protocol step 1 and committed as a capture
artifact before any code is written.

**Consumer call-site first.** The user's call site is one keystroke:

```
$ view file.txt
"+yy        # yanks to the system clipboard
"+p         # pastes from it -- identical over SSH, via OSC52
```

Two implementation shapes for the read path:

```rust
// Option A -- view keeps a clipboard cache, refreshed in the background
Msg::EngineRequest(ClipboardGet { token, .. }) =>
    vec![Effect::Reply { token, value: ReplyValue::Lines(cache.get()) }]

// Option B -- the read happens at paste time, off-loop (CHOSEN)
Msg::EngineRequest(ClipboardGet { token, register }) =>
    vec![Effect::ClipboardRead { token, register }]
// -> executor hands it to the clipboard worker, which replies through
//    the writer channel; the loop never blocks, and never returns stale text
```

**Chosen B over A.** A is wrong by construction: the system clipboard can
change between the cache refresh and the paste, so `"+p` pastes text the
user did not most recently copy — a silent data-correctness bug, the
worst category. B keeps the loop non-blocking because the executor only
sends on a channel; the honest read happens where it must.

**The local backend is a real dependency decision, made here.** Two
shapes for how the worker actually touches the system clipboard:

```rust
// Option A -- platform commands, spawned per operation
Command::new("pbcopy") / "xclip -sel clip" / "wl-copy" ...
// Option B -- arboard, in-process (CHOSEN); dep lives in the `view` bin
arboard::Clipboard::new()?.set_text(text)
```

**Chosen B over A.** A is the same first-five-minutes failure class T12
rejects for `rg`: a Linux box without `xclip`/`wl-copy` installed (most
minimal installs) silently loses `"+y`, and which command to spawn is a
per-platform matrix view would then own. B is one crate that speaks the
native clipboard APIs on all three §14 platforms with no external
binary, and it avoids a subprocess spawn on the paste path. The `view`
bin (where the worker lives) gains `arboard`; its
`scripts/audit-deps.sh` row lands in the same commit.

**OSC52 respects the crate boundary.** Only `view-tui` touches the
terminal, so the OSC52 *write* is an effect routed to the terminal side,
never a write from the clipboard worker. For the remote *read* path,
OSC52 paste-back is unreliable and security-gated in most terminals: view
falls back to its own last-copied shadow register, which is the behavior
every remote nvim setup already has. Document that in the provider's doc
comment so the limitation is a stated contract, not a surprise.

**User's `g:clipboard` wins** (§5.1). The injection is conditional and
the precedence is documented; a test asserts an existing `g:clipboard` is
left untouched.

**Falsifiable check:** an oracle entry yanks with `"+yy` and asserts the
system clipboard (read independently, not through view) holds the line.
A second entry sets `g:clipboard` in the fixture's init.lua and asserts
view did not overwrite it.

- [ ] **Step 1:** Capture the `g:clipboard` contract; commit the artifact.
- [ ] **Step 2: Failing test** for the user-config-wins precedence.
- [ ] **Step 3:** `EngineRequest::{ClipboardGet, ClipboardSet}` +
  `ReplyValue::Lines` + `Effect::ClipboardRead`/`ClipboardWrite`. Every
  `EngineRequest` token must be answered exactly once — never zero times
  (a blocked engine), never twice (a corrupted msgid stream). For
  `VimEnter` the arm answers directly with `Effect::Reply`; for
  `ClipboardGet`/`ClipboardSet` the arm delegates via
  `Effect::ClipboardRead`/`ClipboardWrite` and the WORKER owns the reply
  obligation, sending it through the same writer channel. A test asserts
  one-reply-per-token across both shapes (the step-5 disconfirm covers
  the zero case; a double-reply assert covers the two case).
- [ ] **Step 4:** Worker thread in `view` over `arboard` (dep + audit
  row in this commit); OSC52 write routed via view-tui.
- [ ] **Step 5: Disconfirm.** Drop the reply from one arm; assert the
  test suite catches a blocked engine rather than hanging forever (a
  hanging test is not a passing test — the check must be a timeout with a
  named failure).
- [ ] **Step 6:** `task ci`. Commit: `feat(native): clipboard provider with
  OSC52 remote support`. The description states the latency consequence:
  this adds `Msg::EngineRequest` routing arms to `update()`, so the
  `input_path` delta is measured and stated — expected ~zero because the
  arms sit off the keystroke path, but stated from the measurement, not
  assumed.

---

### Task 7: CLI surface (§5.6)

**Files:**
- Modify: `crates/view/src/main.rs`, `crates/view/src/startup.rs`

**Consumer call-site first** — this is the surface users script against,
so it is a one-way door and gets both call-sites written out (protocol
step 3):

```bash
view +42 notes.md                 # engine passthrough: open at line 42
view -c 'set nu' -R notes.md      # -c command, read-only
view -d a.txt b.txt               # diff mode
view -O a.rs b.rs                 # vertical splits
view -u NONE notes.md             # explicit init
view --clean                      # bundled engine, NO user config, native defaults
ls | view -                       # stdin via stdin_fd at nvim_ui_attach
view --appname work notes.md      # NVIM_APPNAME passthrough
view --config ./off.toml notes.md # explicit view.toml path -> NativeConfig::load;
                                  #   T15's runner drives the three states with it
view --nvim-bin /opt/nvim/bin/nvim --tier basic notes.md   # as-built today
```

**The distinction that must not blur:** `--clean` is view's triage tool
and is NOT `nvim --clean`. It means bundled engine + no user config +
**native defaults on**. The as-built `engine_config` doc comment already
warns that `EngineConfig::isolated` spawns with `--clean` and would
discard the very config being measured; `--clean` must therefore resolve
`NativeConfig::all_enabled()` and pass engine isolation, never reuse a
constructor whose meaning is "measure nothing".

**Passthrough parsing.** clap must not try to interpret `+42` or `-c`.
Two shapes: (A) a `trailing_var_arg` catch-all forwarded verbatim to the
engine, view claiming only its own long flags; (B) enumerating each nvim
flag in view's own parser. **A is chosen**: B is a maintenance treadmill
that silently breaks on every engine-pin bump that adds a flag, and the
failure is quiet — an unrecognized flag becomes a clap error naming
*view*, blaming the wrong tool for the user's correct nvim invocation.

**Exit codes propagate** (`:cq 3` → view exits 3). `git mergetool` and
`GIT_EDITOR=view` abort flows depend on it.

**Falsifiable check:** a pty test per invocation above asserting the
observable result (line 42 on screen; two vertical splits; stdin content
in the buffer). `view --clean` with a fixture whose init.lua would set
`laststatus=2` asserts the config was not sourced. `:cq 3` asserts the
process exit status is 3.

- [ ] **Step 1: Failing test** for `+42` reaching the engine verbatim.
- [ ] **Step 2:** `trailing_var_arg` passthrough; test passes.
- [ ] **Step 3: Failing test** for `:cq 3` → exit 3. Implement; pass.
- [ ] **Step 4:** `--clean`, `--appname`, `--config` (feeds
  `NativeConfig::load(config_path)` — this is the charter's "flag read"
  half, and the mechanism T15's runner launches each state through), and
  `-` (stdin via `stdin_fd` at `nvim_ui_attach` — capture the
  parameter's exact name from `nvim --api-info`, do not recall it).
- [ ] **Step 5: Disconfirm.** Assert `view --tier basic` is still parsed
  by view and NOT forwarded to the engine; a passthrough that swallows
  view's own flags is the failure mode option A risks.
- [ ] **Step 6:** `task ci`. Commit: `feat(cli): engine passthrough,
  --clean, stdin and --appname`.

---

### Task 8: Modal prompt overlays — the blocking path

**Files:**
- Create: `crates/view-core/src/native/prompt.rs` (pure state + update,
  per the crate-seam correction)
- Modify: `crates/view-core/src/update.rs`

**Interfaces** (T15 and the dispatch arm rely on these exact names):

```rust
// view-core/src/native/prompt.rs
pub struct PromptState { .. }
impl PromptState {
    /// Some for the prompt kinds (confirm / return_prompt /
    /// inputlist-class), None for every other message.
    #[must_use] pub fn from_entry(e: &MessageEntry) -> Option<Self>;
    /// Whether `notation` is one of the captured choice keys.
    #[must_use] pub fn accepts(&self, notation: &str) -> bool;
    #[must_use] pub fn view(&self) -> PromptView;   // LayerKind::Prompt payload
}
```

**This is the highest-risk task in the phase.** Owning `ext_messages`
means owning *dialogs*: `cmdheight=0` is forced and the engine's message
area ceases to exist. If a prompt is mishandled, a first-run plugin
bootstrap hangs forever with no visible reason — a silent, expensive
failure, which by §4's ranking gets the deepest verification in this plan.

**Wire fact — capture, never recall.** Drive `:confirm q`, a swapfile
ATTENTION, and an `inputlist()` into a real pinned nvim with
`wire::UI_EXT_OPTIONS` attached; dump and commit the event stream. The
as-built `MessageEntry::is_prompt` doc asserts the question arrives as
`msg_show` kind `"confirm"` with the answer line as a separate
`cmdline_show`, and that an unmatched key re-arms by re-emitting
`cmdline_show` alone. **Confirm that against the capture before writing
code.** If the capture disagrees, reality wins, this plan is corrected,
and downstream briefs are re-extracted (protocol step 6).

**Consumer call-site first:**

```rust
// the prompt overlay answers by feeding the engine a keystroke -- the
// engine is blocked in its OWN input loop, not on an rpcrequest
match model.top_overlay_mut().map(|ov| &mut ov.kind) {
    Some(OverlayKind::Prompt(p)) => match key.notation.as_str() {
        n if p.accepts(n) => vec![Effect::Rpc(RpcCall::Input { notation: n.into() })],
        _                 => vec![],   // unmatched: nvim re-arms, overlay stays open
    },
    _ => vec![],
}
```

**No timeout exists on this path, by design.** The charter is explicit: a
timeout toast here would hang first-run plugin bootstraps. The overlay
stays until nvim resolves the prompt. The disconfirming test below is
what proves it.

**Falsifiable check:** an oracle entry drives a `:confirm q` on a
modified buffer, sends `n`, and asserts the session is still alive with
the buffer intact — and a second sends an unmatched key first, asserting
the prompt is still showing rather than dismissed. A third runs the
cold-bootstrap compat scenario (lazy.nvim on a fresh cache, which emits
real prompts) and asserts it completes; that scenario existing already is
why this is checkable at all.

- [ ] **Step 1:** Capture and commit the three prompt event streams.
- [ ] **Step 2: Failing test** for the unmatched-key case: prompt stays
  open. Observe FAIL against a naive "any key dismisses" implementation —
  write that naive version first specifically to watch this test catch it.
- [ ] **Step 3:** Implement `accepts()` from the captured choice list.
- [ ] **Step 4:** `return_prompt` and inputlist-class kinds, each with
  its own captured stream and its own test.
- [ ] **Step 5: Disconfirm the no-timeout rule.** Add a 5 s timeout that
  auto-answers; run the cold-bootstrap compat scenario; observe it break
  (or, if it does not, that is a finding — the rationale for the rule
  would then be unproven and must be re-derived, not assumed). Remove
  the timeout.
- [ ] **Step 6:** `task ci` + `task compat`. Commit: `feat(native): modal
  prompt overlays for confirm and return_prompt`. The description states
  the latency consequence (`input_path` bench delta — this adds a
  routing arm; and a paint layer, so note `output_path` too if the
  overlay is open in any measured scenario).

---

### Task 9: Toast routing — sticky, transient, and history

**Files:**
- Create: `crates/view-core/src/native/toast.rs` (pure routing + expiry
  state + history, per the crate-seam correction)
- Modify: `crates/view-core/src/model.rs`, `crates/view-core/src/msg.rs`
  (`Msg::ToastExpired`), `crates/view/src/runtime.rs` (the timer worker)

P2 already ships `Messages`, `MessageEntry::is_persistent` (the six
error/warning kinds) and `is_prompt`, plus overflow ranking. T9 adds what
§9's routing table still owes: a timeout for transient toasts, the
scrollback history, and the `cmdheight=0` that owning messages forces.

**`ext_messages` ownership is attach-level, not feature-level.**
`ext_messages` sits in `wire::UI_EXT_OPTIONS`, fixed for the session at
attach; `native.notifications = false` cannot un-attach it, and messages
route to view either way. So `cmdheight=0` is applied unconditionally
post-`VimEnter` as a documented consequence of the attach set (via
`RpcCall::SetOption`), **not** keyed to the notifications feature —
feature-off restoring the user's `cmdheight` while messages still route
to view would be an incoherent state. The notifications feature's T2
supersession entry is only the `vim.notify` re-point, which is what §5.5
defines the feature as owning; with the feature off, message rendering
falls back to P2's as-built minimal rendering and the plugin's
`vim.notify` interception returns. T15's deferred state asserts exactly
that meaning for nvim-notify/noice.

**Interfaces** (T10 and T14 rely on these exact names):

```rust
// view-core/src/native/toast.rs
pub enum Route { Prompt, Sticky, Statusline, Transient }
#[must_use] pub fn route(kind: &str) -> Route;   // §9's table, one match
pub struct ToastHistory { .. }                   // bounded ring
impl ToastHistory {
    pub fn push(&mut self, e: &MessageEntry);
    /// Newest-first; the palette's message-history view (T14) reads it.
    pub fn entries(&self) -> impl Iterator<Item = &MessageEntry>;
}
```

**Consumer call-site first:**

```rust
// §9's routing table, one match -- the table IS the implementation
match kind {
    "confirm" | "return_prompt"                    => Route::Prompt,      // T8
    k if MessageEntry::is_persistent_kind(k)       => Route::Sticky,      // as-built
    "msg_showmode" | "msg_showcmd" | "msg_ruler"
        | "search_count"                           => Route::Statusline,  // T10
    _                                              => Route::Transient,
}
```

**Timeouts without a timer, because the loop is timer-free.** P2's
runtime loop has no free-running clock and `LayerKind::Shell`'s doc
records that no animation lives in the paint layer for exactly that
reason. Two shapes: (A) a timer thread emitting `Msg::ToastExpired`;
(B) expiry evaluated at paint time against a monotonic stamp on each
entry. **A is chosen** — B never fires when nothing else causes a
repaint, so a toast on an idle editor stays forever, which is the bug.
The timer thread sends one `Msg` and never blocks the loop; it is the
same shape as every other worker in this phase.

**Falsifiable check:** an oracle entry emits a transient message, waits
past the timeout with no other input, and asserts the toast is gone —
the idle case that shape B fails. A second asserts an `emsg` is still
present after the same wait.

- [ ] **Step 1: Failing test** for the idle-expiry case.
- [ ] **Step 2:** Timer thread + `Msg::ToastExpired { id }`. Pass.
- [ ] **Step 3:** History buffer with a bounded ring; a `:messages`-style
  view reachable from the palette (T14 wires the entry point).
- [ ] **Step 4:** `cmdheight=0` applied unconditionally post-`VimEnter`
  via `RpcCall::SetOption` (the attach-level ownership note above); the
  notifications feature's T2 supersession entry carries only the
  `vim.notify` re-point. A test asserts `&cmdheight == 0` with
  `native.notifications = false`.
- [ ] **Step 5: Disconfirm.** Make the timer never fire; the idle test
  fails; restore.
- [ ] **Step 6:** `task ci`. Commit: `feat(native): transient toast expiry
  and message history`. The description states the latency consequence
  (`output_path` bench delta — toast layers are paint code; the timer
  worker adds no loop work until a toast exists).

---

### Task 10: Statusline

**Files:** Create `crates/view-core/src/native/statusline.rs` (pure
state + update, per the crate-seam correction; every segment source is
an event or a bridge callback, so no worker crate is needed)

**Interfaces** (T15 relies on these exact names):

```rust
// view-core/src/native/statusline.rs
pub struct StatuslineState { .. }
pub enum SegmentUpdate { Mode(..), Showcmd(..), Ruler(..), SearchCount(..),
    Diagnostics(..), GitBranch(..) }   // from msg_* events + T5's bridge
impl StatuslineState {
    pub fn apply(&mut self, seg: SegmentUpdate);
    /// One line, truncated least-important-segment-inward at `width`.
    #[must_use] pub fn view(&self, width: u16) -> StatuslineView;
}
```

**Consumer call-site first** — the user sees one line:

```
 NORMAL  recording @q   notes.md [+]   ● 2  ▲ 1   main   42:7   58%
 └ mode    └ macro       └ file         └ diagnostics └ git  └ ruler
```

Segments and their sources, each an event this phase already receives or
a bridge callback T5 registered — **no polling anywhere**:

| Segment | Source |
|---|---|
| mode, macro recording | `msg_showmode` (§9: macro recording must ALWAYS be visible) |
| pending keys | `msg_showcmd` |
| file, modified flag | `Model`'s existing buffer state |
| diagnostics | `DiagnosticChanged` via T5's bridge |
| git branch | bridge callback, refreshed on focus/write, never polled |
| ruler / position | `msg_ruler`, `search_count` |

**Supersession:** `laststatus=0` via T2's plan. lualine still loads; its
surface goes unused.

**Falsifiable check:** an oracle entry starts a macro with `qq` and
asserts `recording @q` is on screen — the §9 MUST that is easiest to lose
and hardest to notice missing. A three-state compat run against the
lualine fixture asserts the native line in state 1 and lualine's own
powerline glyph in state 2 (the existing `lualine.toml` already pins that
glyph at row 29 col 85, so state 2's assertion is already written).

- [ ] **Step 1: Failing test** for `recording @q`.
- [ ] **Step 2:** Mode + macro segments from `msg_showmode`. Pass.
- [ ] **Step 3:** Remaining segments, each with its own test.
- [ ] **Step 4:** Width policy: single-line, truncation from the least
  important segment inward, tested at 40 and 200 columns.
- [ ] **Step 5: Disconfirm.** Drop the macro branch; the step-1 test
  fails; restore.
- [ ] **Step 6:** `task ci`. Commit: `feat(native): statusline with
  mode, diagnostics, git and ruler segments`. The description states the
  latency consequence (`output_path` bench delta — a persistent paint
  layer on every frame).

---

### Task 11: Picker core — nucleo, files and buffers, streaming

**Files:** Create `crates/view-core/src/native/picker.rs` (pure state,
per the crate-seam correction) and
`crates/view-native/src/picker/{mod,sources,matcher}.rs` (the workers);
modify `crates/view-core/src/msg.rs` (`Effect::PickerQuery`,
`Msg::PickerResults`)

**Consumer call-site first:**

```rust
let picker = PickerState::open(Source::Files { root: cwd });
// keystroke -> Effect::PickerQuery { generation, needle }  (off-loop)
// worker    -> Msg::PickerResults { generation, items }    (stale gens dropped)
```

**Interfaces** (T12/T15/T16 rely on these exact names):

```rust
// view-core/src/native/picker.rs -- pure; no matcher handle in the Model
pub enum Source { Files { root: PathBuf }, Buffers, LiveGrep }   // LiveGrep: T12
pub struct PickerState { .. }
impl PickerState {
    #[must_use] pub fn open(source: Source) -> Self;
    /// Applies the edit, bumps and returns the new generation.
    pub fn edit_query(&mut self, notation: &str) -> u64;
    /// Results for a stale generation are dropped, never merged.
    pub fn apply_results(&mut self, generation: u64, items: Vec<PickerItem>);
    #[must_use] pub fn view(&self) -> PickerView;   // LayerKind::Picker payload
}

// view-native/src/picker/matcher.rs -- the nucleo worker; spawned by the
// runtime, answers with Msg::PickerResults through the writer channel
pub fn spawn(rx: Receiver<MatchRequest>, tx: Sender<Msg>) -> JoinHandle<()>;
```

The generation stamp is not new machinery: P2's `HlProbeReply` already
drops replies whose generation no longer matches, and its doc explains
why (a reply for a superseded state must never clobber a newer one). The
picker has the identical hazard at far higher frequency.

**Streaming is a contract, not an optimization** (§3.1: "results while
scanning, never scan-then-show"). The source thread pushes into nucleo's
injector as it walks; the picker paints whatever has arrived.

**Falsifiable check:** the §3.1 row itself — keystroke to first results
painted ≤ 16 ms with 100k resident entries. T16 gates it; T11 must
measure it and record the number in the commit description. A test also
asserts results are painted while a 1M-entry scan is still running, by
observing a non-empty result set before the scan thread has exited.

- [ ] **Step 1:** `nucleo` dependency + audit row.
- [ ] **Step 2: Failing test** for generation dropping: feed results for
  generation 1 after generation 2 was issued; assert they are ignored.
- [ ] **Step 3:** Matcher worker + `Effect::PickerQuery`/`Msg::PickerResults`.
- [ ] **Step 4:** Files source over `ignore` (respects `.gitignore`,
  which is what makes a picker usable in a real repo), buffers source
  from engine state.
- [ ] **Step 5: Failing test** for streaming: assert a painted result set
  before scan completion.
- [ ] **Step 6:** Measure the §3.1 latency row; record it.
- [ ] **Step 7:** `task ci`. Commit: `feat(picker): nucleo matcher with
  streaming files and buffers sources`. The description states the
  latency consequence (`input_path` bench delta for the routing arms,
  plus the §3.1 picker number from step 6).

---

### Task 12: Picker live-grep + preview pane

**Files:** Create `crates/view-native/src/picker/preview.rs`; modify
`crates/view-native/src/picker/sources.rs` and
`crates/view-core/src/native/picker.rs` (preview text joins the pure
state)

**Live-grep is spec-committed v0.1 scope, not optional** (charter).

Two shapes: (A) shell out to `rg`; (B) in-process via `ignore` +
`grep-searcher`, the crates ripgrep itself is built from. **B is chosen:**
A is a first-five-minutes bug on any machine without ripgrep installed,
and parsing another tool's output is a compat surface view does not
control. B also keeps the streaming contract under our own control rather
than depending on another process's flush behavior.

**The preview pane reads through RPC, never from disk.** nvim owns all
buffer text: previewing an open, modified buffer by reading the file
would show the user stale content that disagrees with their own editor.
The preview issues an RPC buffer read for loaded buffers and falls back
to a file read only for paths with no buffer.

**Falsifiable check:** open a file in view, modify it without saving,
preview it in the picker, and assert the preview shows the *modified*
text. That single test is the whole hard rule.

- [ ] **Step 1:** `ignore` + `grep-searcher` deps + audit rows.
- [ ] **Step 2: Failing test** for the modified-buffer preview above.
  Implement the naive disk read first and watch it fail.
- [ ] **Step 3:** RPC-backed preview; pass.
- [ ] **Step 4:** Live-grep source, streaming, with the same generation
  discipline as T11.
- [ ] **Step 5: Disconfirm.** Revert to the disk read; step 2 fails;
  restore.
- [ ] **Step 6:** `task ci`. Commit: `feat(picker): live-grep source and
  RPC-backed preview`. The description states the latency consequence
  (`output_path` bench delta — the preview pane adds paint volume while
  the picker is open; grep runs entirely off-loop).

---

### Task 13: File tree

**Files:** Create `crates/view-core/src/native/tree.rs` (pure state,
per the crate-seam correction) and
`crates/view-native/src/tree/{mod,fs,git}.rs` (the workers)

**Consumer call-site first:** a toggleable sidebar overlay (not an nvim
window — multigrid is P6), git status decorations, and file operations.

**Interfaces** (T15 relies on these exact names):

```rust
// view-core/src/native/tree.rs
pub struct TreeState { .. }
impl TreeState {
    pub fn toggle_expand(&mut self, ..);
    pub fn apply_scan(&mut self, generation: u64, entries: Vec<TreeEntry>);
    /// Decorations only; an empty status (no git) is a valid state, not
    /// an error -- the tree renders undecorated.
    pub fn apply_git(&mut self, generation: u64, status: Vec<GitEntry>);
    #[must_use] pub fn view(&self) -> TreeView;   // LayerKind::Tree payload
}

// the sidebar is flush left and full height, which is what T3's anchor is for:
model.push_overlay(
    OverlayBox::new(30, 100).with_anchor(Anchor::Left),
    OverlayKind::Tree(tree_state),
);

// view-native/src/tree/{fs,git}.rs -- workers: `ignore`-walked scan;
// `git status --porcelain=v2` on a thread, bridge-triggered, never polled
```

**File ops go through RPC where nvim owns the truth.** Opening a file is
`RpcCall`; renaming a file with an open modified buffer must not orphan
that buffer. Creating and deleting files on disk are genuine fs effects,
but any operation touching a *buffer* is RPC.

**Git status** shells out to `git status --porcelain=v2` on a worker
thread, refreshed on the T5 bridge's write/focus callbacks, never polled.
Runner-up `gix` rejected for P4: a large dependency surface for
decorations, where `git` is present on any machine where git decorations
mean anything. The tree works fully without git present.

**Falsifiable check:** rename a file with unsaved changes open in a
buffer; assert the buffer follows the rename and its modified flag
survives. Run the tree with `git` removed from PATH; assert the tree
still lists files and simply shows no decorations.

- [ ] **Step 1: Failing test** for the rename-with-modified-buffer case.
- [ ] **Step 2:** RPC-routed rename; pass.
- [ ] **Step 3:** Tree model, expand/collapse; entry keys registered and
  claimed through T3a's mapping mechanism (`default_maps()` gains the
  tree specs; claims report through T2's plan like every other).
- [ ] **Step 4:** Git decorations on a worker; the no-git test.
- [ ] **Step 5: Disconfirm.** Route rename through fs instead of RPC;
  step 1 fails; restore.
- [ ] **Step 6:** `task ci`. Commit: `feat(native): file tree with git
  decorations and RPC-routed file operations`. The description states
  the latency consequence (`output_path` bench delta — a sidebar layer
  present on every frame while toggled on; scans and git run off-loop).

---

### Task 14: Command palette

**Files:** Create `crates/view-core/src/native/palette.rs` (pure state,
per the crate-seam correction)

**Interfaces** (T15 relies on these exact names):

```rust
// view-core/src/native/palette.rs -- wraps P2's decoded state; no new decode
pub struct PaletteState { .. }   // CmdlineState + cmdline-sourced PopupmenuState
impl PaletteState {
    #[must_use] pub fn view(&self) -> PaletteView;   // LayerKind::Palette payload
}
```

**Runner-up rejected:** a second decode path (palette-owned handlers for
`ext_cmdline`/`ext_popupmenu` events). P2's decode into
`CmdlineState`/`PopupmenuState` is pinned by committed captures and
oracle entries; a parallel decode would be a second interpretation of
the same wire traffic that can drift from the first. The palette is a
*presentation* of the already-decoded state — the only new logic is the
cmdline-sourced/buffer-sourced popupmenu distinction, which is exactly
what the step-1 capture pins.

P2 already decodes `ext_cmdline` into `CmdlineState` and renders a
`Cmdline` layer, and decodes `ext_popupmenu` into `PopupmenuState`. T14
turns that into §9's centered floating palette and renders completion
from `ext_popupmenu` **when it is cmdline-sourced** — the wire
distinguishes cmdline-sourced from buffer-completion popups, and painting
a buffer completion inside the palette box would be wrong. Capture which
field carries that distinction; do not recall it.

**Falsifiable check:** `:` opens the centered palette; typing a partial
command shows completions inside it; an insert-mode buffer completion
shows in its own popupmenu at the cursor and NOT inside the palette.
That third assertion is the one that catches the mis-sourced case.

- [ ] **Step 1:** Capture the `ext_popupmenu` source distinction.
- [ ] **Step 2: Failing test** for the buffer-completion case rendering
  outside the palette.
- [ ] **Step 3:** Palette layer + completion routing; pass.
- [ ] **Step 4:** Message-history entry point (T9's ring).
- [ ] **Step 5: Disconfirm.** Route all popupmenus into the palette; step
  2 fails; restore.
- [ ] **Step 6:** `task ci`. Commit: `feat(native): command palette with
  cmdline-sourced completion`. The description states the latency
  consequence (`output_path` bench delta — the palette replaces the
  bottom-line cmdline paint while `:` is open).

---

### Task 15: Three-state oracle coverage + non-interference

**Files:** `corpus/native-*.toml`, `compat/scenarios/*.toml`

This closes P3's recorded deferral 2. Every UI-owning plugin in the
§13.3 matrix is asserted in all three states (§5.5):

```toml
# compat/scenarios/lualine.toml -- the existing file gains states[]
[[states]]
name = "superseded"          # default: native statusline wins
native = {}
steps = [
  # col 1 "N" alone proves nothing about WHO owns the line: lualine's own
  # rendering (pinned by the present-state cells in this same file) also
  # puts "N" at col 1. The discriminator is lualine's powerline separator
  # glyph at col 85 being GONE -- the native line draws no powerline
  # separators -- so each state's assertion is a cell the other state
  # cannot also satisfy.
  { wait_for_cell = { row = 29, col = 1, expected = "N" } },
  { assert_cell_not = { row = 29, col = 85, glyph = "" } },
]

[[states]]
name = "deferred"            # native off, plugin returns
native = { statusline = false }
steps = [ { wait_for_cell = { row = 29, col = 85, expected = "" } } ]

[[states]]
name = "native-only"         # no plugin present at all
fixture = "minimal"
```

**How a state launches:** the runner materializes each state's `native`
table into a `view.toml` under the scenario's hermetic config dir and
launches `view --config <that path>` (the flag T7 ships) — the same
loader real users go through, so a state exercises the shipping config
path, not a test-only backdoor.

**What "deferred" asserts per feature:** for lualine, the plugin's own
rendering returns (the glyph above). For nvim-notify/noice, the deferred
state asserts the plugin's `vim.notify` interception is back in effect
while message *rendering* falls back to P2's as-built minimal rendering —
`cmdheight` stays 0 in every state because `ext_messages` ownership is
attach-level (T9's ownership note), and the scenario must not assert
otherwise.

**Runner-up rejected:** one scenario file per state
(`lualine-superseded.toml`, `lualine-deferred.toml`, …). Three files
drift independently — the fixture and steps get copy-edited apart — and
"a UI-owning scenario declares fewer than three states" stops being a
checkable property of any single file, which is exactly the loader
guarantee step 1 needs.

**Non-interference per feature** (§9, charter exit gate): opening a
feature must cause no engine state drift. The check is a state snapshot
(the parity relation P3 already built: buffer text, cursor, mode,
registers, marks) taken before opening the feature and after closing it,
asserted equal. A picker that leaves the cursor moved, or a tree that
changes the alternate file, fails.

**Falsifiable check:** the non-interference test must catch a real
violation — deliberately have the picker set a mark, observe the test
fail naming the drifted field, then remove it.

- [ ] **Step 1:** Extend the scenario schema with `states[]` and the
  `assert_cell_not` step form (the discriminating negative the
  superseded state needs); the loader rejects a UI-owning scenario that
  declares fewer than three states. The runner materializes each state's
  config and launches through `view --config`.
- [ ] **Step 2:** Fill all three states for every UI-owning plugin.
- [ ] **Step 3:** Non-interference harness over the parity snapshot.
- [ ] **Step 4: Disconfirm** with the deliberate mark-setting picker.
- [ ] **Step 5:** `task compat` + `task oracle`. Commit: `test(compat):
  three-state assertions and per-feature non-interference`.

---

### Task 16a: Input-thread/runtime-loop unification (echo lever 1)

**Files:**
- Modify: `crates/view-tui/src/terminal.rs` (`spawn_input_thread`
  becomes a pollable handle), `crates/view/src/runtime.rs`,
  `crates/view/src/startup.rs`, `crates/view/src/main.rs`

The first of the two user-ratified echo levers (the workstream section
above), and the biggest: `key-decoded->loop-wake` is 49.1 µs p50 of
view's 139 µs share of a round trip — a cross-thread wake paid at
deep-idle depth because a human pauses between keystrokes. The blocking
input thread and its channel hop are replaced by one loop polling the
terminal fd and the engine stdout together. Both hard rules survive by
construction: a readiness poll is not an RPC await, and only view-tui
touches the terminal — view-tui exposes the terminal's pollable fd plus
a non-blocking read-and-decode on its own handle, and the runtime polls
readiness; the fd's *readiness* is the only fact that crosses the crate
boundary, every read and every byte of decode stays in view-tui.

**Runner-up rejected:** keeping the input thread and tuning the wake
(spin-then-park, eventfd). The cost is deep-idle wake latency at human
keystroke cadence; tuning the parking strategy shrinks it, unification
deletes the hop outright, and the ratified closure path pins
unification as the lever.

**Falsifiable check:** the taps decomposition re-measured after the
change shows the `key-decoded->loop-wake` segment collapsed from ~49 µs
p50 to poll-return cost, and the paired echo bench puts `echo.minimal
ratio_p50` on dev-linux at or under the ≤ 1.10 bar — or the measured
residual is recorded against its `[[shortfall]]` entry for
re-adjudication. §8 step 3's buffered-key replay tests still pass
unchanged: pre-attach keys are buffered and replayed in order.

- [ ] **Step 1: Pin behavior first.** The startup buffered-replay and
  input-ordering tests are the contract this restructure must not move;
  run them and record the passing baseline before touching code.
- [ ] **Step 2:** view-tui's pollable terminal handle: expose the fd for
  readiness polling plus a non-blocking `read_keys()`; the input thread
  is not yet deleted (both paths compile side by side).
- [ ] **Step 3:** Unify the runtime loop over terminal fd + engine
  stdout readiness; delete `spawn_input_thread` and its channel; fix
  every call site the compiler names. Step 1's tests pass unchanged.
- [ ] **Step 4:** Re-run the taps decomposition; record the
  `key-decoded->loop-wake` segment before/after.
- [ ] **Step 5:** Paired echo re-measure on dev-linux (and dev-macos
  when that host is available): retire the echo `[[shortfall]]` entries
  the measurement now clears — the ledger enumerates them, absolute
  tails included — or record the residual with the numbers for
  re-adjudication with the user — never silently carried.
- [ ] **Step 6:** `task ci`. Commit: `perf(runtime): unify the input
  thread into the runtime poll loop`. The description states the latency
  consequence with the step-4/5 numbers — this task IS key dispatch.

---

### Task 16b: Incremental rendering behind a shadow-equivalence guard (echo lever 2)

**Files:**
- Modify: `crates/view-surface/src/lib.rs` (`render`),
  `crates/view-tui/src/paint.rs`

The second ratified echo lever, strictly after T16a. The paint side's
68 µs share is attributed to cache and TLB residency, proportional to
memory touched per frame: `view_surface::render` rebuilds a full-screen
`Surface` every frame even for a one-cell change. This task extends the
computed-damage path so `render` reuses the undamaged portion instead of
rebuilding it. Expectations stay honest: this lever helps least where
paint volume is highest (scroll/streaming touch most rows), so the echo
scenarios are where the win should show and the scroll scenarios are
where the guard must prove no regression.

**The guard is the design, not an afterthought:** in debug and CI
builds, every incremental frame is compared against a from-scratch full
rebuild of the same `Model` (shadow equivalence — incremental == full,
asserted cell-for-cell), and the differential oracle cross-checks the
rendered raster against the reference session across the corpus. An
incremental renderer without the equivalence guard is a silent-drift
machine; with it, a divergence is a red assert naming the frame.

**Falsifiable check:** seed a deliberate divergence (skip one damaged
row in the incremental path); the shadow assert fires in debug/CI *and*
the oracle corpus run catches the raster divergence; remove the seed.
Both catchers must be observed catching — a guard that has never fired
is unproven.

- [ ] **Step 1:** Shadow-equivalence harness first: debug/CI-gated
  comparison of incremental output against a full rebuild each frame,
  wired before any incremental logic exists (it trivially passes while
  `render` is still full-rebuild).
- [ ] **Step 2: Disconfirm the guard.** Seed the deliberate divergence;
  observe the shadow assert fail naming the frame and the oracle catch
  the raster diff; remove the seed.
- [ ] **Step 3:** Incremental path over the computed damage; the guard
  and the full oracle corpus stay green.
- [ ] **Step 4:** Paired bench: `output_path` plus the echo scenarios,
  before/after; update the shortfall ledger together with T16a's step-5
  ruling (the two levers' residuals are adjudicated as one picture).
- [ ] **Step 5:** `task ci` + `task oracle`. Commit: `perf(surface):
  incremental render behind a shadow-equivalence guard`. The description
  states the latency consequence with the step-4 numbers — this task IS
  the paint path.

---

### Task 16: Picker budget rows + budgets-hold-with-features

**Files:** `crates/view-bench/src/scenarios/picker.rs`, baselines

Closes P3's recorded deferral 1. Two §3.1 rows gate here:

| Row | Budget |
|---|---|
| Picker match: keystroke → first results painted, 100k resident | ≤ 16 ms |
| Picker scan: 1M-file tree | streaming; first page ≤ 100 ms warm-cache |

**Plus the charter's exit gate that features may not eat the latency
win:** every existing §3.1 row is re-measured with native features
enabled, against the same baselines. A regression here is a P4 defect,
not a new baseline — `--record` is a ratchet and will refuse to move a
value in the wrong direction, which is exactly the guard this needs.

**Depends on the §3.1 budget table** (harness work carried from P3): the
picker rows must gate against the spec budget, not only against the first
number they happen to record. A row whose first recording is 40 ms would
otherwise ratchet at 40 ms and never report that it is 2.5× over budget.

**Runs after T16a and T16b**, so the features-enabled matrix measures
the shipped loop and renderer, not the ones they replaced.

**Falsifiable check:** the picker gate fails on a deliberately slowed
matcher (step 2 observes it), and deleting a picker baseline row makes
the gate fail on the missing row rather than silently passing an
ungated scenario.

- [ ] **Step 1:** Picker scenario with a generated 100k-entry corpus.
- [ ] **Step 2:** Measure; record; verify the gate fails on a
  deliberately slowed matcher, and that deleting a baseline row fails
  the gate; restore both.
- [ ] **Step 3:** 1M scan row; assert streaming by observing painted
  results before scan completion.
- [ ] **Step 4:** Full `task perf-audit` with features enabled; compare
  every row against the P3-exit baselines; any regression is a defect to
  fix in this phase.
- [ ] **Step 5:** Adjudicate measurement-layer carry-ins 1–5 (the
  section above: replicate-median gating, `--record` pinning semantics,
  the `scroll.minimal` knife-edge, dev-macos per-scenario headroom,
  auto-staleness dormancy); record each ruling in `budgets.toml`
  comments or `.claude/measurements/`, with the evidence that decided
  it. A carry-in whose answer needs the user (a gating-semantics change)
  is surfaced with the measured options, not decided silently.
- [ ] **Step 6:** Commit: `test(bench): picker latency rows and
  features-enabled matrix`.

---

## P4 Exit Checklist

Authored with the plan per protocol step 7. Each item closes with an
evidence citation — the command run and its observed output — never a
bare checkmark.

- [x] `task ci` green (fmt-check, lint, audit, style, loc, test). Cited at
      HEAD c579b41: `~/.claude/tmp/battery-ci.log`, `EXIT:0`.
- [x] `task oracle` green, including every three-state entry (T15). Cited:
      `~/.claude/tmp/battery-oracle.log`, 26/26 `PARITY`, `EXIT:0`, incl.
      `laststatus-superseded` (line 27) and `laststatus-restored` (line 26).
- [x] `task compat` green across the §13.3 named set in all three states,
      or each red row filed with a user-approved deferral. Cited:
      `~/.claude/tmp/battery-compat.log` (30 OK + 1 SKIPPED — `daily-config`
      skipped because `VIEW_DAILY_CONFIG` is unset by design), plus
      `~/.claude/tmp/battery-compat-daily.log` (31/31 OK with
      `VIEW_DAILY_CONFIG` set) covering native-only/superseded/deferred and
      the present-only class; both `EXIT:0`.
- [x] `task perf-audit` green with native features ENABLED, every §3.1 row
      compared against its P3-exit baseline (T16). Cited:
      `~/.claude/tmp/battery-perf-audit-retry2.log`, `perf-audit wrapper
      exit: 0` (line 375), `EXIT:0` (line 376), `gate OK: 13 cell(s) within
      recorded bars, 0 metric(s) checked against spec 3.1 budgets, 0
      accepted shortfall(s) still held, 0 reading(s) past spec` (line 134);
      `bench-micro` (6 benches: grid_apply, update_key, input_handoff,
      damage_fold, render_frame, paint_frame) also clean in the same run.
      `BUDGET SKIP [dev-linux]` (line 133) is expected, not an anomaly:
      `crates/view-harness/src/baselines.rs::is_controlled_class` gates
      spec-3.1 attestation on a `controlled-*`-prefixed class, and no such
      class is provisioned anywhere in this repo (`dev-linux`, `dev-macos`,
      `gh-linux`, `gh-macos` are all regression-tripwire-only by design,
      matching the gate-attestation-split ruling). Two prior contamination
      events on this leg are disclosed rather than hidden: the original run
      (`~/.claude/tmp/battery-perf-audit.log`) hit a genuine host-load `GATE
      BREACH` (picker.minimal `first_page_p50_ms` 3.8460 > 3.6737 at load1
      7.57) and was not the authorized retry; the first retry
      (`battery-perf-audit-retry.log`) was killed by an operator Bash-timeout
      mistake, not a host or product failure. retry2 is the one attested
      retry, run at load1 1.32→2.19, and is clean.
- [x] Picker §3.1 rows measured and gated (closes P3 deferral 1). Cited:
      `~/.claude/tmp/battery-perf-audit-retry2.log` lines 120-125, gated
      `match_paint_p50_ms 2.880`, `match_paint_p99_ms 4.739`,
      `first_page_p50_ms 2.594`, `first_page_p99_ms 7.548`, all within the
      recorded dev-linux bars.
- [x] The echo `[[shortfall]]` entries in `crates/view-bench/budgets.toml`
      — the ledger enumerates them; the four `ratio_p50` rows and the
      dev-macos `view_p99_ms` tail alike — retired by measurement
      (T16a/T16b), or the residual re-adjudicated with the user — never
      silently carried. Cited: `crates/view-bench/budgets.toml`
      `[[shortfall]]` section, each entry carries a `why` +
      "User-adjudicated 2026-08-09"; dev-linux `echo.heavy ratio_p50`
      retired by T16b measurement; the 3 residual `ratio_p50` rows
      (dev-linux echo.minimal, dev-macos echo.minimal, dev-macos
      echo.heavy) plus the dev-macos `view_p99_ms` tail are re-adjudicated
      in `.claude/measurements/2026-08-08-measurement-layer-rulings.md`.
- [x] Measurement-layer carry-ins 1-5 adjudicated (T16 step 5), each
      ruling recorded in `budgets.toml` comments or
      `.claude/measurements/` with its evidence citation. Cited:
      `.claude/measurements/2026-08-08-measurement-layer-rulings.md`, 6
      sections all headed "RULED 2026-08-09": (1) `--record` pinning, (2)
      single-shot gating <25% regressions, (3) scroll.minimal dev-macos
      knife-edge, (4) dev-macos echo 1.25 default, (5) auto-staleness
      dormancy, (6) T16 exit pair.
- [x] Non-interference test passing per feature, each shown to catch a
      deliberately introduced drift (T15 step 4). Cited:
      `~/.claude/tmp/battery-ci.log` lines 1133-1143, 5/5 pass (picker,
      picker-grep, picker-buffers, tree, message-history);
      `.superpowers/sdd/2026-07-26-p4-native-features/task-15-report.md`
      records 3 sabotage rounds, each correctly FAILED before revert,
      `git diff --stat` clean after revert.
- [x] Every feature in `registry::features()` reachable, opt-out-able by
      its exact `off_switch`, and reported by the first-run toast. Cited:
      `crates/view-core/src/native/registry.rs` unit tests
      `ids_are_unique` / `off_switch_spells_the_id_a_user_can_paste` /
      `is_feature_answers_only_for_table_rows`
      (`~/.claude/tmp/battery-ci.log` lines 482-494);
      `crates/view-native/src/toast.rs::first_run` unit test
      `the_first_run_announces_every_handed_over_surface_with_its_off_switch`
      (line 1231); supersede-table tests (lines 1215-1219) confirm every
      takeover row names a live registry feature and reverses with its own
      `off_switch` verbatim. Live-confirmed interactively this session (see
      the guided-QA item below): removing/restoring `native.picker = false`
      in `view.toml` suppresses/restores `<leader>ff` symmetrically.
- [x] Golden snapshots present for every native overlay × all three tiers.
      Cited: `~/.claude/tmp/battery-ci.log` lines 1421-1442, 18/18 pass (6
      overlays: palette/picker/prompt/statusline/statusline_bar/tree × 3
      tiers: basic/standard/full).
- [x] `.claude/known-bugs.md` drained, or every remaining item carrying
      explicit user approval. Cited: file read in full this session —
      0 unchecked live items; the sole entry is `[x] RESOLVED 2026-08-03`
      (the `settings.json` tracked-vs-gitignored shape decision).
- [x] Dogfood note appended to `.claude/dogfood-journal.md` — real daily
      driving is expected to start this phase, so this note should record
      actual use, not a smoke test. Cited: `.claude/dogfood-journal.md`,
      "## 2026-08-10 — P4 exit" entry, appended this session from real
      interactive tmux sessions against `target/release/view` at c579b41
      covering the picker, tree, statusline, notifications, message
      history, completion palette, cmdline, first-run announcement
      (persistence confirmed across relaunch in two independently-isolated
      scratch HOMEs), and the picker off-switch both directions; records
      two real documentation bugs the pass caught and fixed in
      `.claude/qa/p4-guided-qa.md` (tree focus-routing key set;
      transient-vs-sticky toast wording), and the discovery that
      `docs/compat.md` was stale in the tracked tree (regenerated via
      `task oracle -- page`, `~/.claude/tmp/battery-oracle-page.log`,
      `EXIT:0`, "wrote docs/compat.md (31 rows, pin v0.12.4, 2026-08-10)").
- [x] Guided acceptance QA pass (the P3 doc's successor, covering the
      user-visible surface this phase actually ships). Cited:
      `.claude/qa/p4-guided-qa.md` (28 steps, sections A-H), run
      interactively this session against the release binary in three
      isolated scratch HOME/XDG environments; all 28 steps observed
      passing, including step 28's gate-enforced doc/build agreement
      (`mappings::tests::the_keys_page_renders_the_table_this_build_registers`,
      `~/.claude/tmp/battery-ci.log` line 1185). Two documentation defects
      found and fixed in the doc itself during the pass (see the dogfood
      note above for detail).
- [x] P5 plan authored under the planning protocol, with the ACP spec
      verified live via context7/docs first (the charter requires it).
      Cited: `.claude/plans/INDEX.md` line 13, "Drafted (adversarially
      reviewed, 3 rounds)"; live ACP verification via WebFetch against
      agentclientprotocol.com is recorded in the P5 planning session
      (context7 unavailable that session, a sanctioned fallback). All five
      P5.5 invented-capability plans are also drafted and adversarially
      reviewed (`.claude/plans/INDEX.md` lines 14-19).
- [x] Every concession or metric degradation encountered during the phase
      carries a Fable 5 adversarial review, per the user's standing rule.
      Cited: `.superpowers/sdd/2026-07-26-p4-native-features/progress.md`
      line 194 (Task 16: `pss_mb` gate breach 5.2100 vs 4.9526 surfaced,
      review dispatched to a `fable` subagent with a measurement-integrity
      reconciliation mandate); line 209 (ratchet-guard: `fable` implementer
      per the 2026-08-09 carry-in-1 ruling); lines 226-231 (footprint-diet:
      `fable` high-scrutiny review dispatched — verdict spec-mechanically-met
      / evidentially-broken — remedy applied, scoped re-review dispatched to
      the same reviewer and CLOSED, "No new Critical/Important. Quality
      APPROVED").

