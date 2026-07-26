# P4 Implementation Plan — Native Features + Theming

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development to implement this plan
> task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** the visible product surface — picker, file tree, statusline,
toasts, command palette, theming — native-wins with per-feature opt-out
(spec §9, §5.5, §7; charter "the visible product surface").

**Architecture:** `view-native` grows one sub-component per feature, each
with its own model/msg/update, each rendered as a `view-surface` overlay
layer and acting only through `Effect::Rpc`. A feature registry — a
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
No new dependency in `view-core`, which stays pure.

**Authored against:** tree at `4cd01bf` (branch `dev/p3-oracle-bench`, P3
exit). Every interface in "As-built interfaces" below was read from source
at that commit, per planning-protocol step 3.

**Status:** DRAFT — not approved for execution. Planning-protocol step 5
(fresh-context adversarial review against the spec, the charter, and the
protocol verbatim) has not run. No task dispatches until it has.

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
  description. Every task in this phase touches at least one of the three.
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

## Coverage walk (planning protocol step 0)

Every charter deliverable and spec MUST for this phase, mapped to a task
or a recorded deferral. An implementer or reviewer finding a phase
requirement absent from both lists has found a plan defect.

| Requirement | Where |
|---|---|
| Feature registry the config surface and doctor expose (charter Produces) | T1 |
| `[native]` per-feature keys: minimal file + flag read, enough for three states and the off switch (charter config split) | T1 |
| Per-feature off switch, exact, never all-or-nothing (§5.5) | T1 |
| Supersession is runtime-only and reversible; nothing in the user's config is ever edited (§5.5) | T2 |
| First-run toast reporting every supersession (charter) | T2 |
| Overlay focus routing: `Focus::Native` gains real consumers; `<Esc>` always returns to `Engine` (§5.3, P2 as-built) | T3 |
| Native overlay layers in `Surface`, z-ordered above engine grid (§7) | T4 |
| Tier degradation for every native surface, golden snapshots per tier (§7, §13) | T4 |
| Theme: named groups for native chrome; border style, padding scale, accent palette shared (§7) | T5 |
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
| Owning `ext_messages` forces `cmdheight=0` (§9) | T9 |
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

Read from the tree at `4cd01bf`, per planning-protocol step 3. Re-verify
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

Both are "via RPC" and both keep the one-reply-per-arm dispatch contract;
they are different mechanisms and T8 must not invent an `EngineRequest`
variant for prompts. **This is a derivation from the as-built source and
the capture in step 1 above must confirm it before T8 writes code. If the
capture contradicts it, reality wins and this table gets fixed.**

## File structure (new/changed this phase)

```
crates/view-core/src/
  model.rs          # T3: overlay stack; focus becomes DERIVED, not a field
  msg.rs            # T6/T8/T11: EngineRequest + RpcCall + Msg arms
  theme.rs          # T5: named groups for native chrome
  native/           # T1: registry (pure data; the feature TABLE lives in core
    registry.rs     #   so surface and native both read it without an edge)
crates/view-surface/src/
  lib.rs            # T4: LayerKind native variants + z-order
  overlay.rs        # T4: shared box/border/padding geometry
crates/view-native/src/
  lib.rs            # module wiring
  config.rs         # T1: [native] table load
  supersede.rs      # T2: runtime-only, reversible supersession
  prompt.rs         # T8: modal prompt overlay
  toast.rs          # T9: sticky/transient routing + history
  statusline.rs     # T10: segments
  picker/           # T11/T12: mod.rs, sources.rs, matcher.rs, preview.rs
  tree/             # T13: mod.rs, fs.rs, git.rs
  palette.rs        # T14
  clipboard.rs      # T6: provider state; the WORKER lives in `view`
crates/view-tui/src/
  paint.rs          # T4: painters per native layer, per tier
crates/view/src/
  main.rs           # T7: CLI surface
  clipboard.rs      # T6: worker thread + OSC52 (only view-tui touches the
                    #   terminal, so the OSC52 write routes through it)
  bridge.rs         # T5/T10: autocmd -> rpcnotify bridge registration
corpus/native-*.toml          # T15: three-state oracle entries
compat/scenarios/*.toml       # T15: states[] added to UI-owning scenarios
crates/view-bench/src/scenarios/picker.rs   # T16
crates/view-bench/baselines/*.toml          # T16: picker rows
scripts/audit-deps.sh         # T1/T11/T12/T13: new crate edges + dep rows
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
- Modify: `crates/view-core/src/msg.rs` (no new variants expected; verify)

**Consumer call-site first:**

```rust
// applied post-VimEnter, only for features that are enabled
let plan = supersede::plan(&cfg, registry::features());
// -> [Supersession { feature: "statusline", rpc: RpcCall::Input {
//        notation: ":set laststatus=0<CR>" }, reverses_with: "native.statusline = false" }]
for s in plan.iter() { effects.push(Effect::Rpc(s.rpc.clone())); }
toast::first_run(&plan);   // "view is drawing the statusline (lualine still
                           //  loads). Turn it off with native.statusline = false"
```

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
- Modify: `crates/view-core/src/model.rs`,
  `crates/view-core/src/update.rs`

**Consumer call-site first.** Two shapes:

```rust
// Option A -- overlay STACK, focus derived from the top (CHOSEN)
model.overlays.push(Overlay::Picker(picker_state));
assert!(matches!(model.focus(), Focus::Native(_)));
// a confirm prompt arrives while the picker is open:
model.overlays.push(Overlay::Prompt(prompt_state));
// ...prompt answered, popped, and the picker is still there, still focused:
model.overlays.pop();
assert!(matches!(model.overlays.last(), Some(Overlay::Picker(_))));

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

**Interfaces:**

```rust
// view-core/src/model.rs
impl Model {
    /// Who owns input this frame: the top overlay if any, else the engine.
    /// Derived rather than stored -- a stored focus can name a closed
    /// overlay, and the routing arms in `update()` would then send keys
    /// into nothing.
    #[must_use] pub fn focus(&self) -> Focus;
}
```

`Model::focus` changes from a public field to a method. This is a
breaking change to P2's as-built surface, made deliberately and named
here so the reviewer checks it: every existing `model.focus` read becomes
`model.focus()`, and the two existing writes (`update()`'s `<Esc>` arm)
become a `pop()`.

**Falsifiable check:** `<Esc>` with two overlays stacked pops exactly one
and leaves focus on the lower overlay, not on the engine — the behavior a
single-field model cannot express. A test drives push/push/Esc and
asserts the picker still holds focus.

- [ ] **Step 1: Failing test** for the push/push/Esc sequence above,
  against P2's current single-field model. Observe FAIL (it will report
  focus on `Engine`, which is the bug).
- [ ] **Step 2:** Add `overlays: Vec<Overlay>` and `Model::focus()`;
  delete the `focus` field. Fix every call site the compiler names.
- [ ] **Step 3:** `update()`'s Key/Paste/Mouse arms route on `focus()`.
  `<Esc>` pops one overlay. Pass.
- [ ] **Step 4: Totality.** Property test: any sequence of pushes, pops
  and `<Esc>` keys leaves `focus()` consistent with `overlays.last()`.
- [ ] **Step 5:** Latency check — this is key-dispatch code. Run
  `task bench -- --scenario input_path --fixture minimal --class dev-linux`
  before and after; the commit description states the delta. A stack
  lookup replacing a field read must not move the number outside the
  measured noise band; if it does, that is a finding, not an acceptable
  cost.
- [ ] **Step 6:** `task ci`. Commit: `refactor(core): derive focus from an
  overlay stack` with the latency delta in the description.

---

### Task 4: Native overlay layers + per-tier painting

**Files:**
- Create: `crates/view-surface/src/overlay.rs`
- Modify: `crates/view-surface/src/lib.rs`, `crates/view-tui/src/paint.rs`

**Consumer call-site first:** every native feature builds its layer the
same way, so the geometry lives once:

```rust
// in view-surface::render, for each overlay in model.overlays
layers.push(overlay::centered(model, OverlayBox {
    kind: LayerKind::Picker(state.view()),
    width_pct: 80, height_pct: 60, border: theme.border_style(model.caps.tier),
}));
```

**Runner-up rejected:** each feature computing its own rect from
`term_width`/`term_height`. Five features clamping independently is five
chances to place a layer outside the frame — the exact class
`clip_to_frame` exists to backstop, and a backstop is not a design.

**Tier degradation is a tested surface, not a fallback apology** (§7):
`full` gets rounded borders, `standard` gets the same layout with plain
borders and no animation, `basic` gets ASCII-safe borders and no color
derivation. Golden snapshots per tier per overlay, per §13.

**Falsifiable check:** a golden-snapshot test renders every native
overlay at all three tiers into `view_oracle::raster::screen_text` output
and diffs against committed goldens. Changing a border glyph fails the
`full` golden and leaves `basic` green.

- [ ] **Step 1:** `LayerKind` gains `Picker`, `Tree`, `Statusline`,
  `Prompt`, `Palette` variants. It is `#[non_exhaustive]`, so this is
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
- Modify: `crates/view-core/src/theme.rs`, `crates/view/src/theme_cache.rs`

**Consumer call-site first:**

```rust
// native chrome asks the theme for a role, never for a raw color
let border = theme.named(hl, "ViewBorder",   theme.normal());
let accent = theme.named(hl, "ViewAccent",   theme.emphasis());
let sel    = theme.named(hl, "ViewSelected", theme.emphasis());
```

`Theme::named` already exists as-built with exactly this fallback shape,
so native chrome extends P2's mechanism rather than adding a parallel
one. The runner-up — a `NativeTheme` struct carrying resolved colors —
was rejected because it would have to be rebuilt on every
`hl_attr_define` and would drift from the engine's live highlight state
between rebuilds, which is precisely what §7's "live highlight state, not
a one-shot query" rule forbids.

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
- [ ] **Step 2: Failing test** for two config paths yielding two cache
  entries.
- [ ] **Step 3:** Bridge registration, at the same point in startup as
  `register_vim_enter_autocmd` (which as-built must be registered BEFORE
  `ui_attach` returns — read `nvim_api.rs`'s doc comment for why, and do
  not repeat the race it describes).
- [ ] **Step 4:** `Msg::ColorSchemeChanged` re-derives `Theme::from_hl`
  and writes the cache.
- [ ] **Step 5: Disconfirm.** Break the bridge registration; the oracle
  entry fails with the border unchanged; restore.
- [ ] **Step 6:** `task ci`. Commit: `feat(theme): ColorScheme re-derive
  bridge and native named groups`.

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
  `EngineRequest` arm must produce exactly one `Effect::Reply` — that is
  the as-built dispatch contract and the engine is blocked until it does.
- [ ] **Step 4:** Worker thread in `view`; OSC52 write routed via view-tui.
- [ ] **Step 5: Disconfirm.** Drop the reply from one arm; assert the
  test suite catches a blocked engine rather than hanging forever (a
  hanging test is not a passing test — the check must be a timeout with a
  named failure).
- [ ] **Step 6:** `task ci`. Commit: `feat(native): clipboard provider with
  OSC52 remote support`.

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
view -O a.rs b.rs                 # vertical splits
view -u NONE notes.md             # explicit init
view --clean                      # bundled engine, NO user config, native defaults
ls | view -                       # stdin via stdin_fd at nvim_ui_attach
view --appname work notes.md      # NVIM_APPNAME passthrough
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
- [ ] **Step 4:** `--clean`, `--appname`, and `-` (stdin via `stdin_fd`
  at `nvim_ui_attach` — capture the parameter's exact name from
  `nvim --api-info`, do not recall it).
- [ ] **Step 5: Disconfirm.** Assert `view --tier basic` is still parsed
  by view and NOT forwarded to the engine; a passthrough that swallows
  view's own flags is the failure mode option A risks.
- [ ] **Step 6:** `task ci`. Commit: `feat(cli): engine passthrough,
  --clean, stdin and --appname`.

---

### Task 8: Modal prompt overlays — the blocking path

**Files:**
- Create: `crates/view-native/src/prompt.rs`
- Modify: `crates/view-core/src/update.rs`

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
Overlay::Prompt(p) => match key.notation.as_str() {
    n if p.accepts(n) => vec![Effect::Rpc(RpcCall::Input { notation: n.into() })],
    _                 => vec![],   // unmatched: nvim re-arms; we stay open
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
  prompt overlays for confirm and return_prompt`.

---

### Task 9: Toast routing — sticky, transient, and history

**Files:**
- Create: `crates/view-native/src/toast.rs`
- Modify: `crates/view-core/src/model.rs`

P2 already ships `Messages`, `MessageEntry::is_persistent` (the six
error/warning kinds) and `is_prompt`, plus overflow ranking. T9 adds what
§9's routing table still owes: a timeout for transient toasts, the
scrollback history, and the `cmdheight=0` that owning messages forces.

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
- [ ] **Step 4:** `cmdheight=0` applied as a T2 supersession entry, so it
  is reversible and reported like every other one.
- [ ] **Step 5: Disconfirm.** Make the timer never fire; the idle test
  fails; restore.
- [ ] **Step 6:** `task ci`. Commit: `feat(native): transient toast expiry
  and message history`.

---

### Task 10: Statusline

**Files:** Create `crates/view-native/src/statusline.rs`

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
  mode, diagnostics, git and ruler segments`.

---

### Task 11: Picker core — nucleo, files and buffers, streaming

**Files:** Create `crates/view-native/src/picker/{mod,sources,matcher}.rs`

**Consumer call-site first:**

```rust
let picker = Picker::open(Source::Files { root: cwd });
// keystroke -> Effect::PickerQuery { generation, needle }  (off-loop)
// worker    -> Msg::PickerResults { generation, items }    (stale gens dropped)
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
  streaming files and buffers sources`.

---

### Task 12: Picker live-grep + preview pane

**Files:** Create `crates/view-native/src/picker/preview.rs`; modify `sources.rs`

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
  RPC-backed preview`.

---

### Task 13: File tree

**Files:** Create `crates/view-native/src/tree/{mod,fs,git}.rs`

**Consumer call-site first:** a toggleable sidebar overlay (not an nvim
window — multigrid is P6), git status decorations, and file operations.

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
- [ ] **Step 3:** Tree model, expand/collapse, keys claimed per §5.3.
- [ ] **Step 4:** Git decorations on a worker; the no-git test.
- [ ] **Step 5: Disconfirm.** Route rename through fs instead of RPC;
  step 1 fails; restore.
- [ ] **Step 6:** `task ci`. Commit: `feat(native): file tree with git
  decorations and RPC-routed file operations`.

---

### Task 14: Command palette

**Files:** Create `crates/view-native/src/palette.rs`

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
  cmdline-sourced completion`.

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
steps = [ { wait_for_cell = { row = 29, col = 1, expected = "N" } } ]

[[states]]
name = "deferred"            # native off, plugin returns
native = { statusline = false }
steps = [ { wait_for_cell = { row = 29, col = 85, expected = "" } } ]

[[states]]
name = "native-only"         # no plugin present at all
fixture = "minimal"
```

**Non-interference per feature** (§9, charter exit gate): opening a
feature must cause no engine state drift. The check is a state snapshot
(the parity relation P3 already built: buffer text, cursor, mode,
registers, marks) taken before opening the feature and after closing it,
asserted equal. A picker that leaves the cursor moved, or a tree that
changes the alternate file, fails.

**Falsifiable check:** the non-interference test must catch a real
violation — deliberately have the picker set a mark, observe the test
fail naming the drifted field, then remove it.

- [ ] **Step 1:** Extend the scenario schema with `states[]`; the loader
  rejects a UI-owning scenario that declares fewer than three.
- [ ] **Step 2:** Fill all three states for every UI-owning plugin.
- [ ] **Step 3:** Non-interference harness over the parity snapshot.
- [ ] **Step 4: Disconfirm** with the deliberate mark-setting picker.
- [ ] **Step 5:** `task compat` + `task oracle`. Commit: `test(compat):
  three-state assertions and per-feature non-interference`.

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

- [ ] **Step 1:** Picker scenario with a generated 100k-entry corpus.
- [ ] **Step 2:** Measure; record; verify the gate fails on a
  deliberately slowed matcher.
- [ ] **Step 3:** 1M scan row; assert streaming by observing painted
  results before scan completion.
- [ ] **Step 4:** Full `task perf-audit` with features enabled; compare
  every row against the P3-exit baselines; any regression is a defect to
  fix in this phase.
- [ ] **Step 5:** Commit: `test(bench): picker latency rows and
  features-enabled matrix`.

---

## P4 Exit Checklist

Authored with the plan per protocol step 7. Each item closes with an
evidence citation — the command run and its observed output — never a
bare checkmark.

- [ ] `task ci` green (fmt-check, lint, audit, style, loc, test).
- [ ] `task oracle` green, including every three-state entry (T15).
- [ ] `task compat` green across the §13.3 named set in all three states,
      or each red row filed with a user-approved deferral.
- [ ] `task perf-audit` green with native features ENABLED, every §3.1
      row compared against its P3-exit baseline (T16).
- [ ] Picker §3.1 rows measured and gated (closes P3 deferral 1).
- [ ] Non-interference test passing per feature, each shown to catch a
      deliberately introduced drift (T15 step 4).
- [ ] Every feature in `registry::features()` reachable, opt-out-able by
      its exact `off_switch`, and reported by the first-run toast.
- [ ] Golden snapshots present for every native overlay × all three tiers.
- [ ] `.claude/known-bugs.md` drained, or every remaining item carrying
      explicit user approval.
- [ ] Dogfood note appended to `.claude/dogfood-journal.md` — real daily
      driving is expected to start this phase, so this note should record
      actual use, not a smoke test.
- [ ] Guided acceptance QA pass (the P3 doc's successor, covering the
      user-visible surface this phase actually ships).
- [ ] P5 plan authored under the planning protocol, with the ACP spec
      verified live via context7/docs first (the charter requires it).
- [ ] Every concession or metric degradation encountered during the phase
      carries a Fable 5 adversarial review, per the user's standing rule.

