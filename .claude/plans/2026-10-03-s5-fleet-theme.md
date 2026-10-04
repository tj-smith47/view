# Agent-fleet attention and theme-switcher interop: implementation plan (2026-10-03)

Delivers two charters from `.claude/plans/2026-08-14-post-v01-charters.md`, both part of v0.1 under the 2026-09-04 ruling:

- C2 agent-fleet attention (charters:97-130), and README.md:171-172 "Agent-fleet attention".
- C3 theme-switcher interop (charters:134-163). No README row.

Every path, line and count below was read from today's tree, including the uncommitted S5.1 key-introspector diff in the working copy. Line counts come from `scripts/audit-god-files.sh --counts` (comments elided, test code excluded). C2 tasks come first, then C3. C3 has no dependency on C2 and may run in parallel with it.

## 0. Ground truth that changed the plan

| # | Recon / earlier assumption | Tree today | Consequence |
|---|---|---|---|
| G1 | C2.4: the attention list is a fifth `NativeSurface` | Adding a `NativeSurface` variant touches about 250 match and table sites across about 30 files. model.rs (999), update/surfaces.rs (953), view-surface/overlay.rs (942), view-tui/paint.rs (983) are among them. | The list is a mode of the existing agent surface. It reaches the overlay (view-surface/lib.rs:1165) and the windowed tile (view-tui/paint/panes.rs:287-299) through one view builder. Recorded in Decisions taken. |
| G2 | One `AiWorker`, one `AiSlot` | `AiWorker { …, slot: Arc<Mutex<AiSlot>>, …, watch }` (view/src/ai_worker.rs:263), `dispatch` (:367), `spawn_in_background` (~:506) | The worker holds `Vec<(AgentId, AiSlot)>`. The emit closure tags each `Msg` with the agent it came from. The shared fs watch gains a holder count. |
| G3 | Fs request ids are unique | The ACP driver numbers requests per session (`next_boundary_id`), so two agents produce the same ids. `RpcCall::AiFsRead { request_id, … }` (msg.rs:2790) correlates its reply on `request_id`. | RPC correlation moves to the process-wide hidden-buffer generation (`model.next_hidden_generation`, model.rs:766-768). `PendingFsOp` gains `agent`. `AiFsState::take_by_request` is deleted. |
| G4 | Reviews belong to the agent's conversation | `pending_diff` and `pending_diff_next` sit on `AiPanelState` but every review handler reads them through `model.ai_panel_mut()` and binds a hidden buffer in the editor (update/review.rs, `bind_effect` review.rs:217, `show_effect` review.rs:675) | Reviews stay editor-level: two slots shared by every agent, each tagged with the agent that proposed it. A parked agent's proposal is held on its fleet entry (at most 2) and replayed when it is focused. |
| G5 | Locations come with every permission request | `on_permission_request` (view-ai/src/acp/driver.rs:694) never reads `toolCall.locations`. The wire capture has the `locations` property (acp-v1-wire-capture.md:424-431, :1273) but not the `ToolCallLocation` definition. | T7 captures the `ToolCallLocation` schema from the pinned ACP schema before it codes. Whether claude-code-acp actually sends `locations` is unverified, and the jump falls back to the conversation when it does not. |
| G6 | `RpcCall::OpenFile` can jump to a line | `RpcCall::OpenFile { path }` (msg.rs:2452) has no line. `open_file` (nvim_api.rs:3967) runs `OPEN_FILE_CHUNK`. | New `RpcCall::OpenFileAt { path, line, open_target }` in a new `nvim_api/open_at.rs`, following `nvim_api/fit.rs`. One `mod` line in nvim_api.rs (968). |
| G7 | C3 needs a `--print-server` flag or `--listen` | nvim 0.12.4 starts a server by default unless `--listen` is given (`starting.txt:449`), at `stdpath("run").."/nvim.{pid}.0"`. view never passes `--listen` in production, keeps `XDG_RUNTIME_DIR` in `HERMETIC_PASSTHROUGH_VARS`, and scrubs `NVIM_LISTEN_ADDRESS` and `NVIM`. | A switcher loop over `$XDG_RUNTIME_DIR/nvim.*.0` already reaches the engine inside view. C3 adds no production code for the live path. |
| G8 | Spec §7.1 tokens are configurable | spec:649 says "every derivation is overridable in `view.toml`". The sample at spec:753-758 sets `surface_raised = "#343746"` under `[ui.tokens]` (:755-757). The loader's `UiTokensTable { accent }` (view-native/src/config.rs:229-243) has `deny_unknown_fields`, so that sample fails to load. | C3.3 amends the spec to the loader (accent only). Adding a key later is additive for switchers; removing one would break them. |
| G9 | Line headroom | model.rs 999, view-engine/handle.rs 992, view-native/config/resolve.rs 992, submit_hold.rs 992, view-harness/bin/bench.rs 978, nvim_api.rs 968, update/surfaces.rs 953, overlay.rs 942, view-ai/acp/driver.rs 938, update/mod.rs 898, view/main.rs 873, view-native/config.rs 862, msg.rs 787, runtime.rs 779 | model.rs gets the ai accessor block moved out before any field is added (T2). New code goes in new modules: `native/fleet.rs`, `update/fleet.rs`, `overlay/fleet.rs`, `nvim_api/open_at.rs`, `bin/bench/matrix.rs` if needed. driver.rs takes the location decode in a helper file if it would pass 970. |

## 1. C2 design: agent-fleet attention

### 1.1 Who owns what

```
view-core     native/ai_event.rs   AgentId, AgentLocation, AiEvent::PermissionLocations
              native/fleet.rs      Fleet: parked conversations, held proposals, list cursor,
                                   attention edges, revision. AgentAttention, AgentRow.
              native/ai_panel/     AiPanelState::exchange_conversation (+ unread_turn,
                                   attention_edge)
              native/ai_fs.rs      PendingFsOp.agent, drain_agent, generation correlation
              native/review.rs     DiffReviewState::with_agent
              native/permission.rs PermissionPrompt::with_locations
              model/agents.rs      the ai accessor block moved out of model.rs, plus fleet
              update/fleet.rs      on_agent_event, fold_parked, :View ai agents/new,
                                   list keys, jump
              msg.rs               Msg::AiFrom, Effect::AiTo, Effect::AiSubmitTo,
                                   RpcCall::OpenFileAt
view-surface  overlay/fleet.rs     list body (rows, footer, no caret)
              lib.rs, pill.rs      agent_panel_view, fleet_word
view-ai       acp/driver.rs        decode toolCall.locations, emit PermissionLocations
view-engine   nvim_api/open_at.rs  open_file_at: edit or switch, set cursor
view          ai_worker.rs         slots per agent, dispatch_to, tagged emit
              ai_context_worker.rs AiContextJob carries the agent
              runtime/executor.rs  Effect::AiTo / AiSubmitTo / RpcCall::OpenFileAt arms
view-tui      paint/panes.rs       windowed tile calls agent_panel_view
```

nvim still owns all buffer text. The list never touches buffers. A jump is an `Effect::Rpc`. Agents run on worker threads as today, and nothing in the paint loop waits on them.

### 1.2 Types and signatures

```rust
// view-core/src/native/ai_event.rs (beside the existing types)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AgentId(pub u16);
impl AgentId {
    pub const FIRST: AgentId = AgentId(0);
    /// The number a person sees: 1-based.
    pub fn shown(self) -> u16;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLocation { pub path: PathBuf, pub line: Option<u32> }

// AiEvent is #[non_exhaustive] (ai_event.rs:30); a new variant is additive.
AiEvent::PermissionLocations { request_id: u64, locations: Vec<AgentLocation> },
```

```rust
// view-core/src/native/fleet.rs
pub const MAX_AGENTS: usize = 8;

/// Declaration order is the sort order of the attention list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AgentAttention { Blocked, Crashed, Done, Working, Idle }

pub fn attention(panel: &AiPanelState, held: &[HeldProposal], reviewing: bool) -> AgentAttention;

#[derive(Debug)]
pub struct HeldProposal { pub request_id: u64, pub path: PathBuf, pub hunks: Vec<Hunk> }

#[derive(Debug)]
struct Parked { id: AgentId, panel: AiPanelState, held: Vec<HeldProposal> }

#[derive(Debug, Default)]
pub struct Fleet {
    enabled: bool,
    focused: AgentId,
    parked: Vec<Parked>,
    next: u16,
    list: Option<usize>,
    edges: u64,
    revision: u64,
}

impl Fleet {
    pub fn set_enabled(&mut self, on: bool);
    pub fn is_enabled(&self) -> bool;
    pub fn focused(&self) -> AgentId;
    pub fn len(&self) -> usize;                       // parked + the focused one
    pub fn add(&mut self, fresh: AiPanelState) -> Option<AgentId>;   // None at MAX_AGENTS
    pub fn focus(&mut self, id: AgentId, live: &mut AiPanelState) -> Vec<HeldProposal>;
    pub fn take_parked(&mut self, id: AgentId) -> Option<(AiPanelState, Vec<HeldProposal>)>;
    pub fn restore_parked(&mut self, id: AgentId, panel: AiPanelState, held: Vec<HeldProposal>);
    pub fn hold(&mut self, id: AgentId, p: HeldProposal) -> Result<(), HeldProposal>; // at most 2
    pub fn rows(&self, live: &AiPanelState, reviewing_by: &dyn Fn(AgentId) -> bool) -> Vec<AgentRow>;
    pub fn list_open(&mut self);
    pub fn list_close(&mut self);
    pub fn list_selected(&self) -> Option<usize>;
    pub fn list_move(&mut self, delta: isize, rows: usize);
    pub fn note_edge(&mut self, panel: &mut AiPanelState);  // stamps attention_edge
    pub fn revision(&self) -> u64;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub id: AgentId,
    pub attention: AgentAttention,
    /// Blocked: the question, `path:line`, or the review path. Crashed: the error. Else empty.
    pub detail: String,
    pub edge: u64,
}

pub fn row_spans(row: &AgentRow, selected: bool) -> Vec<Span>;
```

Ties within one attention class sort by `edge`, oldest first, so the agent that has waited longest is on top.

```rust
// view-core/src/native/ai_panel/mod.rs
impl AiPanelState {
    /// Swaps every conversation field with `other` and leaves the editor-side fields in place.
    pub(crate) fn exchange_conversation(&mut self, other: &mut Self);
}
```

`exchange_conversation` destructures both sides exhaustively, so a new field fails to compile until it is classified.

- Editor-side, stay put: `focused`, `pending_diff`, `pending_diff_next`, `hidden_generation`, `configured_agent`, `cwd`, `home`.
- Conversation-side, swap: every other field, plus the new `unread_turn: bool` and `attention_edge: u64`.

The focused agent's conversation always lives in `model.ai_panel`, so every existing panel reader is unchanged.

```rust
// view-core/src/msg.rs (add beside; old forms kept until T4 switches the last producer)
Msg::AiFrom { agent: AgentId, event: AiEvent },
Effect::AiTo { agent: AgentId, command: AiCommand },
Effect::AiSubmitTo { agent: AgentId, text: String },
RpcCall::OpenFileAt { path: PathBuf, line: Option<u32>, open_target: ReviewOpenTarget },
```

`Msg::Ai(e)` stays and routes to `AgentId::FIRST`, since tests and runtime.rs build it in many places (runtime.rs:1826, :1906, :2983 and others). `Effect::Ai` and `Effect::AiPromptSubmit` are deleted in T4, the commit that switches their last producer.

```rust
// view-core/src/native/ai_fs.rs
pub(crate) struct PendingFsOp { request_id, generation, path, intent, agent: AgentId }
impl AiFsState {
    pub(crate) fn drain_agent(&mut self, agent: AgentId) -> Vec<PendingFsOp>;
    // take_by_request deleted; replies use take_by_generation
}

// view-core/src/native/review.rs
impl DiffReviewState { pub fn with_agent(self, agent: AgentId) -> Self; pub fn agent(&self) -> AgentId; }

// view-core/src/native/permission.rs
impl PermissionPrompt { pub fn with_locations(self, l: Vec<AgentLocation>) -> Self; pub fn locations(&self) -> &[AgentLocation]; }
```

`DiffReviewState::new` (review.rs:189) and `PermissionPrompt::new` (permission.rs:49) are called from view-oracle, view-engine tests and view-surface, so both stay unchanged and the builders default to `AgentId::FIRST` and no locations.

```rust
// view-core/src/update/fleet.rs
pub(super) fn on_agent_event(model: &mut Model, agent: AgentId, event: AiEvent) -> Vec<Effect>;
fn fold_parked(model: &mut Model, agent: AgentId, event: AiEvent) -> Vec<Effect>;
pub(super) fn open_list(model: &mut Model) -> Vec<Effect>;
pub(super) fn new_agent(model: &mut Model) -> Vec<Effect>;
pub(super) fn list_key(model: &mut Model, notation: &str) -> Option<Vec<Effect>>;
fn jump(model: &mut Model, agent: AgentId) -> Vec<Effect>;

// view-core/src/update/ai.rs (signature change is crate-private)
pub(super) fn on_ai_event(model: &mut Model, agent: AgentId, event: AiEvent) -> Vec<Effect>;

// view-surface/src/lib.rs (add beside, then switch both call sites)
pub fn agent_panel_view(model: &Model, h: u16, w: u16, has_keyboard: bool) -> AiPanelView;
// AiPanelView (views.rs:1189) gains: fleet: Vec<Vec<Span>>, fleet_selected: Option<usize>
impl AiPanelView { pub fn with_fleet(self, rows: Vec<Vec<Span>>, selected: Option<usize>) -> Self; }

// view-surface/src/pill.rs (add beside agent_word, pill.rs:435)
pub fn fleet_word(model: &Model) -> &'static str;

// view-engine/src/nvim_api/open_at.rs
impl EngineHandle { pub fn open_file_at(&self, path: &Path, line: Option<u32>, target: ReviewOpenTarget) -> Result<(), EngineError>; }

// view/src/ai_worker.rs
pub(crate) fn dispatch_to(&self, agent: AgentId, command: AiCommand);
pub(crate) fn dispatch(&self, command: AiCommand) { self.dispatch_to(AgentId::FIRST, command) }

// view/src/ai_context_worker.rs
pub enum AiContextJob { Submit { agent: AgentId, text: String }, Direct { agent: AgentId, command: AiCommand } }
```

### 1.3 Data flow

```
agent N (ACP subprocess)
   │ stdio
   ▼
view-ai driver ── AiEvent ──► ai_worker emit closure (slot N)
                                  │ wraps Msg::Ai(e) as Msg::AiFrom { agent: N, event: e }
                                  ▼
                             LoopSender ──► update::on_agent_event(model, N, e)
                                               │
                     N == fleet.focused ───────┼──────── N parked
                                ▼              │              ▼
                   on_ai_event(model, N, e)    │   fold_parked: DiffProposed → fleet.hold
                   (today's panel path,        │   else swap N in, on_ai_event, swap out
                    effects tagged with N)     │
                                               ▼
                               fleet.note_edge on attention change; revision += 1
                                               │
                       Effect::AiTo{N,..} / AiSubmitTo{N,..} ──► executor ──► AiContextJob{N}
                                                                              ──► ai.dispatch_to(N)
list <CR> on N ──► jump: focus N (exchange_conversation)
                    ├─ held proposal → replay DiffProposed → bind → show_effect(true, target)
                    ├─ editor review by N → show_effect(true, target)
                    ├─ question with locations → Effect::Rpc(OpenFileAt{path,line,target}),
                    │                            panel stays open with the question, unfocused
                    └─ else → open the conversation, focused
```

### 1.4 Usage

```toml
# view.toml
[ai]
agent = "claude-code"
fleet = true            # derived default; false: one agent per view, no agent list
```

| Input | What happens |
|---|---|
| `<leader>an` / `:View ai new` | Starts another agent with the configured `ai.agent` in the same project and focuses its conversation. At eight agents: notice "Eight agents are running; close one to start another". Behind the trust prompt like `<leader>ai`. |
| `<leader>aa` / `:View ai agents` | Toggles the attention list in the agent window (floating or beside your code, whichever placement the agent panel uses). |
| `j` / `k` / `<Down>` / `<Up>` | Move the selection. |
| `<CR>` | Focus that agent and land on what it is asking about (§1.3). |
| `q` / `<Esc>` | Back to the focused agent's conversation. |
| `[ai] fleet = false` | Neither key is registered. `:View ai agents` and `:View ai new` raise "Agent list is off: set [ai] fleet = true". `<leader>ai` behaves exactly as today. |
| One agent running | The pill, the panel title and every key behave exactly as today. The panel title gains the agent number only when a second agent exists ("Agent 2 · claude-code"). |

The list, drawn in the agent window (overlay placement shown):

```
╭ Agents ─────────────────────────────╮
│▸2  waiting  Edit src/wire.rs:42     │
│ 4  crashed  adapter exited          │
│ 1  done                             │
│ 3  running                          │
│ 5  idle                             │
│ <CR> open  j/k move  q back         │
╰─────────────────────────────────────╯
```

Words come from `agent_word` (pill.rs:435) plus `done` for a turn that ended while its conversation was not on screen. Styles reuse `StyleRole::AiPermissionAsk` (waiting), `NoticeError` (crashed), `AiToolDone` (done), `AiToolRunning` (running), `Plain` (idle). No new token.

The pill (`fleet_word`): with no parked agent it returns `agent_word` unchanged. With parked agents it returns the word of the most urgent agent in attention order, `waiting` > `crashed` > `done` > `running` > `idle`.

### 1.5 Feasibility assessment (charter gate)

The charter (charters:99-103, 126-130) asks for a fresh assessment recorded in the plan, with a go/no-go the user rules on.

- **Session model:** P5 built exactly one shape of agent, an ACP session owned by `AiWorker`. Terminal jobs and worktrees are not modelled. The fleet source is therefore ACP sessions in this process. That is the "arrive free with P5" source the charter names.
- **Jump-to-artifact:** reachable for two of the three artifacts the charter names. A diff is the existing review path. A buffer and line come from `toolCall.locations` on the permission request (G5). A worktree is not modelled by P5 and is left out.
- **Cost:** about 12 commits, no new crate, no new process, no socket. The single-agent path keeps its code, its goldens and its bench rows.
- **Risks:** claude-code-acp may omit `locations` (then the jump opens the conversation, which still beats a terminal pane); review slots are shared, so a third concurrent proposal across agents is held on its fleet entry.
- **Recommendation: go.** C2 code starts only after the user rules. T1 records the ruling.

## 2. C3 design: theme-switcher interop

### 2.1 What already works

```
switcher (omarchy-class) ─ for s in $XDG_RUNTIME_DIR/nvim.*.0:
                               nvim --server $s --remote-expr "execute('colorscheme X')"
                                   │
                                   ▼
nvim embedded in view (default server, G7) ── ColorScheme autocmd ── rpcnotify
                                   │
                                   ▼
Msg::ColorSchemeChanged (update/mod.rs:825) → theme re-derived → cache rewritten
($XDG_STATE_HOME/view/theme-*.toml) → next frame drawn in the new look
```

No view process restarts and no engine restarts.

### 2.2 What C3 adds

- `theme_switcher_key_paths_are_frozen` in view-native/src/config/keys.rs tests, and one bullet in `.claude/rules/rust.md` "## view specifics" (:45): the `[ui] theme` and `[ui.tokens]` key paths are a consumer contract for theme switchers; a rename or a move to another file is refused.
- `crates/view/tests/theme_switch_live.rs`: an outside process changes the colorscheme of a running view binary over the engine's default server.
- `docs/theming.md`: the integrator page.
- Spec §7.1 and §11 brought in line with the loader (G8).

### 2.3 Usage

```bash
# What a switcher runs. Reaches plain nvim and view alike.
for server in "${XDG_RUNTIME_DIR:-/tmp}"/nvim.*.0; do
  nvim --server "$server" --remote-expr "execute('colorscheme tokyonight')"
done
```

```toml
# What a switcher may also sed, read at launch only.
[ui]
theme = "tokyonight"     # runs :colorscheme at launch; the next launch uses it
[ui.tokens]
accent = "#7aa2f7"       # "auto" follows the colorscheme
```

## 3. Performance bounds (owed in commit bodies)

- **Agent event, focused agent:** one `AgentId` compare added in front of today's `on_ai_event`. No allocation.
- **Agent event, parked agent:** two `exchange_conversation` calls, each a field-by-field `mem::swap` (no allocation, no clone), around today's handler. A held proposal moves its hunks; it does not copy them.
- **Key dispatch:** unchanged when the list is closed. One `Option` check (`fleet.list_selected()`) runs in the agent window's key route. T10's diagnostic row holds `key_to_rpc_p99_us` ≤ 100 with two agents streaming.
- **Paint:** the cache compares `fleet.revision()` (a `u64`) beside the existing agent inputs. The list body is at most `MAX_AGENTS` + 1 rows built only while the list is open. T10's felt row bounds list open to the next frame at 16 ms p99.
- **Pill:** `fleet_word` is one pass over at most 8 parked entries, run only when the pill is rebuilt.
- **C3:** no production change. The theme re-derive cost is today's ColorScheme path.

## 4. Tasks

All implementer commits use `nice -n 15 task commit PATHS="…" -- -m "…"`, the one gate run per commit. Docs and spec commits use `task commit:quick`. Implementers run no bench.

**Population tests every task keeps green:**
- `every_model_field_is_a_paint_input_or_named_here` (view-surface/src/cache.rs:515)
- `every_question_reads_keys_only_after_it_was_painted` (update/tests.rs `QUESTIONS`)
- `the_registry_and_the_example_document_the_same_keys` (keys.rs:381), `the_two_resolvers_answer_the_whole_registry_between_them`, `every_specified_table_is_documented_in_the_example` (view/tests/config_registry.rs)
- the registry and exempt reachability tests in mappings.rs, and the `render_table`/`render_usage` docs tests behind docs/keymaps.md
- every golden under `crates/view-oracle/tests` unchanged (the single-agent non-regression evidence for C2)
- `scripts/audit-deps.sh`, `scripts/audit-god-files.sh`, `check-style.sh` (comment frames: add none)
- the bench.rs scenario-module list test (bench.rs:2129) and `every_felt_bound_is_seated_on_the_config_it_claims`

**Depends on:** S5.1 committed (model.rs 999, mappings.rs `[MappingSpec; 27]`). Where DVR T1/T4 land first, counts shift by one; see §6.

### C2.T1: Record the go/no-go ruling and the §10.2 amendment

- **Depends on:** the user's ruling on §1.5.
- **Files:** `.claude/specs/2026-07-17-view-design.md`: §10.2 (:890-897) replaced with the text in §5.1; §18 decision log gets one row (§5.1).
- **Tests:** none (docs).
- **Capture:** none.
- **Performance statement:** none (docs).
- **Subject:** `docs(spec): bring agent-fleet attention into the initial release`

### C2.T2: Core fleet types and model room

- **Files:**
  - new `crates/view-core/src/native/fleet.rs`
  - new `crates/view-core/src/model/agents.rs`: the ai accessor block from model.rs (~:1143-1196: `ai_panel_mut`, `ai_panel`, `ai_panel_overlay_open`, `close_ai_panel`) moved as `impl Model`, plus `fleet()`/`fleet_mut()`
  - changed:
    - `crates/view-core/src/model.rs` (999): −~50 for the moved block, +1 `mod agents;` beside :2223-2233, +2 field `pub fleet: Fleet` and its init
    - `crates/view-core/src/native/mod.rs`: +1 `pub mod fleet;`
    - `crates/view-core/src/native/ai_event.rs`: `AgentId`, `AgentLocation`, `AiEvent::PermissionLocations`
    - `crates/view-core/src/native/ai_panel/mod.rs` (554): `unread_turn`, `attention_edge`, `exchange_conversation`
    - `crates/view-core/src/native/ai_fs.rs`: `PendingFsOp.agent` (FIRST at every current construction), `drain_agent`
    - `crates/view-core/src/native/review.rs` (425): `with_agent`, `agent`
    - `crates/view-core/src/native/permission.rs`: `with_locations`, `locations`
    - `crates/view-surface/src/cache.rs`: classify `fleet` as a paint input compared by `revision()`
    - `crates/view/src/vlog.rs`: log arm for the new event (vlog.rs:262 matches `Msg::Ai`)
- **Signatures:** §1.2 core types, `exchange_conversation`, the builders.
- **Tests** (`native/fleet.rs` and `ai_panel` `mod tests`):
  - `attention_orders_blocked_then_crashed_then_done_then_working_then_idle`: five rows of each class in shuffled order sort to declaration order.
  - `agents_waiting_equally_long_keep_the_order_they_started_waiting`: two Blocked rows sort by `edge`.
  - `the_fleet_refuses_a_ninth_agent`: `add` returns `None` at `MAX_AGENTS` and `len()` stays 8.
  - `exchange_conversation_swaps_the_transcript_and_keeps_the_review`: after a swap the transcript, usage and pending permission moved; `pending_diff`, `hidden_generation`, `cwd`, `focused` did not.
  - `exchanging_twice_restores_both_conversations`.
  - `a_parked_agent_holds_at_most_two_proposals`: the third `hold` returns `Err` with the proposal.
  - `a_review_built_without_an_agent_belongs_to_the_first`.
- **Capture:** none; nothing is visible yet. The commit body says so.
- **Performance statement:** "no runtime path changes; the swap is field moves only".
- **Subject:** `feat(ai): give each agent session an id and a place to wait while another is in front`

### C2.T3: Agent-tagged messages and a worker that runs several agents

- **Files:**
  - changed:
    - `crates/view-core/src/msg.rs` (787): `Msg::AiFrom`, `Effect::AiTo`, `Effect::AiSubmitTo` added beside the old forms
    - `crates/view-core/src/update/mod.rs` (898): `Msg::AiFrom { agent, event } => fleet::on_agent_event(model, agent, event)`; `Msg::Ai(e)` (:1339) routes to `on_agent_event(model, AgentId::FIRST, e)`; +1 `mod fleet;`
    - new `crates/view-core/src/update/fleet.rs` with `on_agent_event` only (focused → `on_ai_event`; parked arm lands in T4)
    - `crates/view-core/src/update/ai.rs` (310): `on_ai_event` takes `agent`, not yet used
    - `crates/view/src/ai_worker.rs`: `slots: Vec<(AgentId, AiSlot)>`, `dispatch_to`, `dispatch` delegating to FIRST, emit closure wraps into `Msg::AiFrom { agent, .. }`, `WatchSlot` gains `holders` so the watch stops when the last agent ends; `shutdown` stops every slot
    - `crates/view/src/ai_context_worker.rs`: `AiContextJob` carries `agent`
    - `crates/view/src/runtime/executor.rs`: arms for `Effect::AiTo` and `Effect::AiSubmitTo`; the old arms (:975) queue with `AgentId::FIRST`
- **Tests:**
  - `ai_worker.rs`: `two_agents_answer_on_their_own_ids`: stub agents in slots 0 and 1, each sent `prompt`; every `Msg::AiFrom` from slot 1 carries `AgentId(1)`.
  - `ai_worker.rs`: `the_watch_outlives_the_first_agent_to_end`: slot 0 crashes, slot 1 keeps its watch running; slot 1 ends and the watch stops.
  - `ai_worker.rs`: `a_command_for_an_idle_agent_names_that_agent`: `dispatch_to(AgentId(3), Cancel)` on an idle slot yields `AiFrom { agent: AgentId(3), SessionCrashed }`.
  - update/tests.rs: `an_untagged_ai_event_is_the_first_agents`: `Msg::Ai(MessageChunk)` and `Msg::AiFrom { FIRST, MessageChunk }` produce the same model.
  - existing ai_worker, recovery and runtime ai tests unchanged.
- **Capture:** `scripts/dogfood/cap.sh --ansi --submit` with `<leader>ai` and one prompt under the real config, against the stub (`scripts/dogfood/stub-proposal.sh`), showing the panel unchanged from master.
- **Performance statement:** "one AgentId compare per agent event; no key, grid or paint path changes".
- **Subject:** `feat(ai): run several agent sessions side by side in one editor`

### C2.T4: Every agent command names its agent

- **Files:**
  - changed:
    - `crates/view-core/src/update/ai.rs`: every `Effect::Ai(` (8 sites) and `AiPromptSubmit` becomes `AiTo { agent, .. }` / `AiSubmitTo { agent, .. }`; PermissionRequested opens the panel only when `agent == fleet.focused()`; SessionCrashed takes only reviews whose `agent()` matches and calls `ai_fs::drain_agent(agent)`; TurnEnded sets `unread_turn` unless the conversation is on screen; DiffProposed builds `DiffReviewState::new(..).with_agent(agent)`
    - `crates/view-core/src/update/ai_fs.rs` (151): 5 sites; `PendingFsOp.agent`; `RpcCall::AiFsRead` carries `op.generation` in its `request_id` field; replies call `take_by_generation`
    - `crates/view-core/src/native/ai_fs.rs`: delete `take_by_request`
    - `crates/view-core/src/update/review.rs`: `DiscardProposal` (:113, :239) routed to `review.agent()`
    - `crates/view-core/src/update/fleet.rs`: `fold_parked` (DiffProposed → `fleet.hold`, falling back to the existing "both slots full" notice; else swap in, `on_ai_event`, swap out); `note_edge` on attention change
    - `crates/view-core/src/msg.rs`: delete `Effect::Ai` and `Effect::AiPromptSubmit`
    - `crates/view/src/runtime/executor.rs`: delete the old arms
    - test-only updates where tests match `Effect::Ai(` (watch.rs:1200 and update/tests.rs)
- **Tests** (update/tests.rs):
  - `a_parked_agents_question_does_not_open_the_panel`: AiFrom{2, PermissionRequested} with agent 1 focused leaves the panel closed and agent 2's row Blocked.
  - `a_parked_agents_crash_leaves_the_focused_agents_review`: review tagged agent 1 survives AiFrom{2, SessionCrashed}.
  - `a_crash_drains_only_that_agents_file_reads`: two pending fs ops, one per agent; crash of agent 2 drains only its op.
  - `two_agents_reading_with_the_same_request_id_get_their_own_replies`: both send FsReadRequested id 7; each reply reaches its own agent.
  - `a_parked_agents_proposal_waits_on_its_entry`: DiffProposed from a parked agent binds nothing and the entry holds it.
  - `a_rejected_review_answers_the_agent_that_proposed_it`: `DiscardProposal` lands as `AiTo { agent: 2, .. }`.
  - `a_turn_that_ends_off_screen_reads_done`.
- **Capture:** a second agent cannot be started from the binary until T6. The commit body repeats the T3 single-agent capture and says the two-agent capture comes with T6.
- **Performance statement:** "the focused path is unchanged; a parked agent's event costs two field-swap passes".
- **Subject:** `fix(ai): an agent's file reads, reviews and crashes stay with that agent when several run`

### C2.T5: `[ai] fleet` config key

- **Files:**
  - changed:
    - `crates/view-ai/src/config.rs`: `AI_KEYS` 4→5 (:79) with `("ai", "fleet")`; `AiConfig::fleet() -> bool`, derived `true`; env name follows `env_names()` (:316)
    - `crates/view-native/src/config/keys.rs`: `ConfigKey { table: "ai", key: "fleet", derived: "true" }` beside the ai rows (:277-298)
    - `view.toml.example`: `fleet = true  # false: one agent per view, no agent list` in the `[ai]` block
    - `crates/view/src/main.rs` (873): `seed_ai_enabled` (:870) adds `model.fleet.set_enabled(cfg.fleet())` (+1)
- **Tests:**
  - view-ai config: `fleet_is_on_unless_turned_off`, `fleet_reads_false_from_the_file`, `fleet_env_overrides_the_file`.
  - existing: `the_registry_and_the_example_document_the_same_keys`, `the_two_resolvers_answer_the_whole_registry_between_them`.
- **Capture:** `view --config-report` under the real config showing `ai.fleet = true (default)`.
- **Performance statement:** "read once at start; no runtime cost".
- **Subject:** `feat(config): add [ai] fleet to turn the agent list and extra agents off`

### C2.T6: Start another agent

- **Files:**
  - changed:
    - `crates/view-core/src/native/mappings.rs` (421): DEFAULT_MAPS +1 row `<leader>an` → `:View ai new`, feature `ai`
    - `crates/view-core/src/update/mod.rs`: in the ai FeatureInvoke block (:607-670), `new` goes after the `!ai_trusted` gate and calls `fleet::new_agent`; refused with the "Agent list is off" notice when `!fleet.is_enabled()`
    - `crates/view-core/src/update/fleet.rs`: `new_agent` parks the focused conversation, seeds a fresh `AiPanelState` with the editor-side fields, and opens the panel focused. The new slot spawns on its first prompt (`AiSubmitTo { agent, .. }`), the same way an `AiSlot::Idle` spawns today (ai_worker.rs:367).
    - `crates/view/src/native.rs` (674): beside the "ai" filter (:965-971), drop the `new` and `agents` specs when the fleet is off
    - `docs/keymaps.md`: regenerated by the docs test
- **Tests:**
  - `new_agent_parks_the_conversation_and_focuses_an_empty_one` (update/tests.rs).
  - `new_agent_is_refused_at_eight` with the notice text.
  - `new_agent_is_refused_when_the_fleet_is_off`.
  - `new_agent_waits_for_trust_like_the_panel_does`.
  - mappings.rs registry tests with the count +1.
- **Capture:** `cap.sh --ansi` with the stub: `<leader>ai`, a prompt, `<leader>an`, a prompt; the second panel titled "Agent 2 · …".
- **Performance statement:** "one DEFAULT_MAPS row; no dispatch path change".
- **Subject:** `feat(ai): start another agent in the same editor with <leader>an`

### C2.T7: The question names its file and line

- **Depends on:** the `ToolCallLocation` definition captured from the pinned ACP schema into acp-v1-wire-capture.md (first step of this task).
- **Files:**
  - changed:
    - `crates/view-ai/src/acp/driver.rs` (938): `on_permission_request` (:694) decodes `toolCall.locations` through a helper in new `crates/view-ai/src/acp/locations.rs` and emits `AiEvent::PermissionLocations` after `PermissionRequested`
    - `crates/view-ai/tests/fixtures/stub_agent.rs`: script `ask-at PATH:LINE` beside `ask` (:397 area), building request_permission (:859-902) with `toolCall.locations`
    - `crates/view-core/src/update/ai.rs`: PermissionLocations attaches to the matching pending prompt via `with_locations`
    - acp-v1-wire-capture.md: the `ToolCallLocation` definition
- **Tests:**
  - view-ai: `locations_on_a_permission_request_reach_the_editor` (stub `ask-at src/a.rs:42`).
  - view-ai: `a_permission_request_without_locations_sends_none`.
  - view-ai locations.rs: `a_location_without_a_line_keeps_the_path`, `a_relative_location_is_dropped` (ACP paths are absolute).
  - update/tests.rs: `locations_for_another_request_are_ignored`.
- **Capture:** none visible yet; the jump uses it in T9.
- **Performance statement:** "decode runs once per permission request on the driver thread".
- **Subject:** `feat(ai): an agent's question carries the file and line it is about`

### C2.T8: The attention list

- **Files:**
  - new `crates/view-surface/src/overlay/fleet.rs`: `fleet_body(view) -> Body` (rows, footer, `selected`, no caret)
  - changed:
    - `crates/view-surface/src/lib.rs` (862): `agent_panel_view` beside, `OverlayKind::Ai` (:1165) switched to it
    - `crates/view-tui/src/paint/panes.rs` (267): the windowed tile (:287-299) switched to it
    - `crates/view-surface/src/views.rs` (788): `AiPanelView.fleet`, `fleet_selected`, `with_fleet`
    - `crates/view-surface/src/overlay.rs` (942): `ai_body` (:1196) delegates to `fleet_body` when `fleet` is non-empty (+3); +1 `mod fleet;`
    - `crates/view-surface/src/pill.rs` (344): `fleet_word`; `shown_agent` switched to it
    - `crates/view-surface/src/cache.rs`: `agent_of` (:204) switched to `fleet_word`
    - `crates/view-core/src/native/mappings.rs`: DEFAULT_MAPS +1 row `<leader>aa` → `:View ai agents`
    - `crates/view-core/src/update/mod.rs`: `agents` verb in the ai block → `fleet::open_list`
    - `crates/view-core/src/update/ai.rs`: `ai_panel_key` (:400) asks `fleet::list_key` first while the list is open
    - `crates/view-core/src/update/fleet.rs`: `open_list`, `list_key` (j/k/<Up>/<Down>/q/<Esc>; <CR> calls `jump` which in this task only focuses the agent and opens its conversation)
    - `crates/view-harness/tests/native_interference.rs`: one case
    - `docs/keymaps.md`: regenerated
- **Tests:**
  - view-surface: `the_list_shows_the_waiting_agent_first_with_its_question`.
  - view-surface: `one_agent_draws_the_panel_exactly_as_before`: `agent_panel_view` with one agent equals the old `model.ai_panel().view(..)`.
  - view-surface: `the_pill_reads_waiting_when_any_agent_waits`, `the_pill_with_one_agent_reads_as_before`.
  - view-surface: `the_list_reaches_the_windowed_agent_tile`.
  - update/tests.rs: `enter_on_an_idle_agent_opens_its_conversation`, `q_returns_to_the_focused_conversation`, `agents_is_refused_when_the_fleet_is_off`.
  - native_interference.rs: `the_agent_list_leaves_the_engine_untouched` via `assert_no_interference` (:238).
  - existing view-oracle goldens unchanged.
- **Capture:** `cap.sh --ansi` under the real config, two stub agents (one `ask`, one `stream-forever`), `<leader>aa`, in both overlay and windowed placement.
- **Performance statement:** "list closed: one Option check in the agent key route; open: at most nine rows rebuilt when `fleet.revision()` changes".
- **Subject:** `feat(ai): <leader>aa lists your agents with the one waiting on you first`

### C2.T9: Selecting an agent lands on what it is asking about

- **Files:**
  - new `crates/view-engine/src/nvim_api/open_at.rs`: `open_file_at` (lua chunk: `:edit` or switch to the window showing the file per `ReviewOpenTarget`, then set the cursor when `line` is given)
  - changed:
    - `crates/view-engine/src/nvim_api.rs` (968): +1 `mod open_at;` (:7-13)
    - `crates/view-core/src/msg.rs`: `RpcCall::OpenFileAt`
    - `crates/view/src/runtime/engine_ops.rs` (863): trait `open_file_at` beside `open_file` (:190), impls beside :484 and :735
    - `crates/view/src/runtime/executor.rs`: arm beside `RpcCall::OpenFile` (:413)
    - `crates/view-core/src/update/fleet.rs`: `jump` per §1.3 (held proposal replay, editor review by that agent via `show_effect(true, model.ai_review_open_target)`, location → `OpenFileAt` with the panel open and unfocused, else the conversation)
- **Tests:**
  - update/tests.rs: `enter_on_a_waiting_agent_opens_its_file_at_the_line`: emits `RpcCall::OpenFileAt { line: Some(42), .. }` and the panel keeps the question without the keyboard.
  - update/tests.rs: `enter_on_an_agent_with_a_held_proposal_opens_the_review`.
  - update/tests.rs: `enter_on_an_agent_whose_review_is_open_shows_that_review`.
  - update/tests.rs: `enter_on_a_waiting_agent_without_locations_opens_its_conversation`.
  - view-engine live: `open_file_at_puts_the_cursor_on_the_line` against a real nvim.
  - view/tests (live, stub `ask-at`): `jumping_to_a_waiting_agent_lands_on_its_line`: the binary's screen shows the file and the cursor row after `<leader>aa` `<CR>`.
- **Capture:** `cap.sh --ansi` under the real config: two agents, one `ask-at src/main.rs:12`; `<leader>aa`, `<CR>`; the buffer at line 12 with the question in the panel beside it.
- **Performance statement:** "one RPC per jump, sent on <CR>; no key-dispatch change".
- **Subject:** `feat(ai): picking a waiting agent opens the file and line it is asking about`

### C2.T10: Bench plumbing (implementer adds, does not run)

- **Files:**
  - new `crates/view-bench/src/scenarios/fleet.rs`: two stub agents (`stream-forever` and `ask`), opens the list with `<leader>aa`; setup commands `silent`-prefixed, the measured action is the key
  - changed:
    - `crates/view-bench/src/scenarios/mod.rs` (scenario list), `crates/view-harness/src/rows.rs` (dispatch arm, :29), `crates/view-harness/src/builds.rs` if a row table entry is needed (:56)
    - `crates/view-harness/src/bin/bench.rs` (978): two DIAGNOSTIC_MATRIX rows (:189). If that passes 990, DIAGNOSTIC_MATRIX moves to new `bin/bench/matrix.rs` first.
    - `crates/view-harness/src/budgets.rs`: UNPAIRED_FELT (:142) entry for `fleet` with grounds "view's own list; bare nvim has no counterpart"
    - `crates/view-bench/budgets.toml`: rows with `max` and no seat yet:

```toml
[[budget]]
spec_row = "Agent list open, two agents"
scenario = "fleet"
metric   = "open_paint_p99_ms"
max      = 16.0
kind     = "felt"
felt     = "press <leader>aa -> the list of agents is on screen"
config   = "fixture"

[[budget]]
spec_row = "Key path with two agents streaming"
scenario = "ai_fleet_active"
metric   = "key_to_rpc_p99_us"
max      = 100.0
kind     = "diagnostic"
decomposes = "echo.view_p99_ms"
config   = "fixture"
```

- **Tests:** bench.rs scenario-module list test (:2129), `every_felt_bound_is_seated_on_the_config_it_claims`, `every_unpaired_felt_exemption_names_a_shipped_row`, the driver silence and wait walks (`driver_commands.rs`, `driver_waits.rs`).
- **Capture:** none.
- **Performance statement:** "bench-only code".
- **Subject:** `test(bench): measure the agent list and the key path with two agents streaming`

### C2.T11: Coordinator: seat the rows on controlled-linux

- Owned by the coordinator, with the quiet-host lock and the cfgd peer's consent (CLAUDE.md).
- `task bench` for `fleet` and `ai_fleet_active` on controlled-linux, `--record`, add the spec §3.1 rows (:93-160) with the recorded numbers, run `scripts/check-budget-drift.sh` and `task drift:sweep`.
- **Subject:** `test(bench): seat the agent list rows on the controlled host`

### C2.T12: Docs, spec and README for the fleet

- **Files (`task commit:quick`):**
  - spec §9 invented capabilities table (:795-808): row "Agent-fleet attention"; §9 note at :778 area per §5.1; §11 `[ai]` block (:995-1001) gains the `fleet` line
  - `docs/ai.md`: new section "Several agents" after the panel sections (:127 area): `<leader>an`, `<leader>aa`, the list words, what `<CR>` does, `[ai] fleet`
  - `README.md`: remove :171-172 from Next; add after "Agents in the editor" (:81-83):

```
**Several agents, one queue.** `<leader>an` starts another agent and
`<leader>aa` lists them, the one waiting on you first. Picking it puts you
on the file and line it is asking about.
```

  - checked against `PROSE_MECHANISM` and `check_prose_frames`; no Neovim credit; roadmap influence line for herdr (:148) stays.
- **Subject:** `docs: describe running several agents and the attention list`

### C3.T1: Freeze the theme switcher key paths

- **Files:**
  - changed: `crates/view-native/src/config/keys.rs` tests; `.claude/rules/rust.md` "## view specifics" (:45) +1 bullet
- **Tests:**
  - `theme_switcher_key_paths_are_frozen`: `("ui","theme")` and `("ui.tokens","accent")` are in `keys()`; a scratch `view.toml` with `[ui] theme = "blue"` and `[ui.tokens] accent = "#112233"`, loaded by `ViewConfig::load` (config.rs:1149) and `resolve` (resolve.rs:258), yields that theme and accent. The failure message says a rename breaks theme switchers that edit view.toml.
- **Capture:** none.
- **Performance statement:** "test only".
- **Subject:** `test(config): refuse a rename of the theme keys switchers edit`

### C3.T2: An outside process retheming a running view

- **Files:** new `crates/view/tests/theme_switch_live.rs` (pattern: `key_flood.rs`, `view_oracle::PtySession::spawn`).
- **Test** `an_outside_colorscheme_change_rethemes_a_running_view`:
  1. Spawn the view binary in a pty with `--tier full -u init.lua` (view passes arguments through). `init.lua` runs `colorscheme habamax` and writes `v:servername` to a file at `VimEnter`.
  2. Assert the address is `stdpath("run")/nvim.<pid>.0` with `<pid>` a child of the view pid.
  3. `record_raw_output`, then from a separate process: `nvim --server ADDR --remote-expr "execute('colorscheme blue')"`.
  4. Wait for the screen to settle. Assert row 0 and the frame cells match a second view started cold with `colorscheme blue` (a differential; no colour is written in the test).
  5. Assert the view pid and the engine pid are the ones from step 2.
  6. Assert the theme cache file under `$XDG_STATE_HOME/view/` was rewritten.
  7. Smoothness: split `raw_output` at `\x1b[?2026l` (terminal.rs:966); every frame's chrome sample is either the habamax style or the blue style, never a mix.
- Hermetic home through the existing test helper; kills only its own pids on drop.
- **Capture:** two `--ansi` tmux captures under the real config, before and after an outside `nvim --server … colorscheme` on the running instance, attached to the commit body.
- **Performance statement:** "test only".
- **Subject:** `test(theme): a theme switcher retheming a running view takes effect without a restart`

### C3.T3: Integrator page and spec fix

- **Files (`task commit:quick`):**
  - new `docs/theming.md`: `theme = "auto"` follows the colorscheme live; the switcher loop from §2.3 over `$XDG_RUNTIME_DIR/nvim.*.0`; `[ui] theme` runs at launch and wins at the next launch; `[ui.tokens] accent` is read at launch and does not follow the scheme; the key paths are fixed and live in one `view.toml`
  - spec §7.1: :649 becomes "the derivations a user names are the `[ui.tokens]` keys in §11"; the sample at :755-757 becomes `accent = "auto"` under `[ui.tokens]`
  - spec §11 tokens block (:926-928): comment that switchers edit these paths
- **Tests:** `check_prose_frames` on the new page.
- **Subject:** `docs: how a theme switcher drives view`

## 5. Spec amendments

### 5.1 C2

§10.2 (:890-897), whole replacement:

> In: ACP client, agent panel, context providers, native diff review, workspace fs-watch + conflict UI, config (`[ai] agent = "claude-code"` or arbitrary command), per-project trust prompt before first agent launch, and up to eight agent sessions inside one view with an attention list ordered by which agent needs a decision first, where selecting an agent lands on the review, file or line it is asking about (amended 2026-10-03, C2 agent-fleet attention, folded into the initial release by the 2026-09-04 ruling). Out (recorded candidates): inline ghost-text completions (users' existing plugins keep working meanwhile), orchestration across processes (a socket API, a daemon, or agents hosted in a multiplexer's panes; herdr and tmux stay the tools for hosting processes), embedded model serving.

§9 table (:795-808), new row: "Agent-fleet attention | several agents in one editor, listed by who needs a decision first; picking one opens the review, file or line it asks about | `[ai] fleet = false`".

§9 (:778 area), one sentence: "`[ai] fleet = false` removes the agent list and its keys; no plugin is replaced, so nothing is handed back."

§11 `[ai]` (:995-1001): `fleet = true  # false: one agent per view, no agent list`.

§18 decision log, one row: "2026-10-03 | C2 fleet source is ACP sessions in this process; the list is a mode of the agent window | a fifth NativeSurface costs ~250 sites; cross-process sources wait on C1 attach".

§3.1 (:93-160): two rows added by C2.T11 with recorded numbers.

### 5.2 C3

- §7.1 :649 and the sample :755-757 per C3.T3.
- §11 :926-928 comment per C3.T3.

## 6. Decisions taken

| Fork | Taken | Alternative | Taken, in use | Alternative, in use | Cost to switch later |
|---|---|---|---|---|---|
| C2 fleet source (recon F1) | ACP sessions in this process | sessions from every view on the machine | `<leader>an` in one view; `<leader>aa` lists that view's agents | `<leader>aa` in view A lists agents started in view B | Medium: the list reads `Fleet::rows`; a second source adds rows from C1's attach channel. No user-facing change. |
| C2 toggle (recon F3) | `[ai] fleet = true`, derived default on | always on, governed by `ai.enabled` alone | `[ai]\nfleet = false` | `[ai]\nenabled = false` (drops the whole panel) | Low: one key. |
| C2 host (recon C2.4) | a list mode of the agent window, both placements | a fifth `NativeSurface` | `<leader>aa` replaces the conversation with the list in the agent window | `<leader>aa` opens its own window beside the agent window | High: ~250 sites. The builder `agent_panel_view` keeps the list body separable if that is ever wanted. |
| C2 cap | `MAX_AGENTS = 8`, no knob | `[ai] max_agents` | ninth `<leader>an` → notice | `[ai]\nmax_agents = 16` | Low: a const becomes a key. |
| C2 reviews | editor-level slots tagged with an agent, parked proposals held per agent | review slots per conversation | agent 2's proposal waits until you pick agent 2 | agent 2's diff opens while you work with agent 1 | Medium: moves `pending_diff` to the conversation side of `exchange_conversation`. |
| C2 fs correlation | hidden-buffer generation | request id plus agent id on the wire | n/a (internal) | n/a (internal) | Low: internal. |
| C2 keys | `<leader>aa` list, `<leader>an` new | `:View ai agents` only | `<leader>aa` | `:View ai agents` | Low: a DEFAULT_MAPS row; users remap. |
| C3 tokens reload (recon F1) | `[ui.tokens]` read at launch, documented | live reload of view.toml | edit accent, relaunch | edit accent, the running view retints | Low: additive. |
| C3 spec mismatch (X5) | amend spec §7.1 to accent only | implement `surface_raised` and the other §7.1 tokens | `[ui.tokens]\naccent = "auto"` | `[ui.tokens]\nsurface_raised = "#343746"` | Low: adding a key later is additive for switchers. |
| C3 server discovery | engine's default server under `stdpath("run")` | a `view --print-server` flag | `for s in $XDG_RUNTIME_DIR/nvim.*.0; do nvim --server $s …; done` | `nvim --server "$(view --print-server)" …` | Low: a flag can be added; the loop keeps working. |

## 7. Conflicts with work in flight

| File | Other work | Resolution |
|---|---|---|
| `crates/view-core/src/model.rs` (999) | S5.1 (uncommitted), DVR T1 (moves `is_standing_native_notice`) | C2.T2 moves the ai accessor block (~50 lines) first; independent of DVR's move. Whoever lands second rebases the `mod` list (:2223-2233). |
| `crates/view-core/src/native/mappings.rs` | S5.1 `<leader>fk` (27), DVR `<leader>fv` (28) | C2.T6 and T8 add one row each; the array length and registry tests take whatever count is current. |
| `crates/view-core/src/update/mod.rs` (898) | S5.1, DVR (≤ 8 lines) | C2 adds ~6 lines; the fleet logic lives in `update/fleet.rs`. |
| `crates/view-core/src/msg.rs` (787) | S5.1, DVR | Add-beside for every new variant. |
| `crates/view/src/runtime/executor.rs` (582) | DVR mailbox | Separate arms. |
| `crates/view-harness/src/bin/bench.rs` (978) | DVR T8 | Whoever passes 990 first moves DIAGNOSTIC_MATRIX to `bin/bench/matrix.rs`. |
| `crates/view-native/src/config/keys.rs`, `view.toml.example` | DVR T2 (`[dvr]` table) | Different tables; the example-keys test covers both. |
| `crates/view/src/main.rs` (873) | DVR T2 seeding | One line each. |
| `crates/view/src/native.rs` | DVR key filter | Separate `retain` lines. |
| `crates/view-engine/src/nvim_api.rs` (968) | none in flight | One `mod` line. |
| `crates/view-surface/src/overlay.rs` (942) | none in flight | +4 lines; the body goes to `overlay/fleet.rs`. |
| `docs/keymaps.md` | S5.1, DVR | Regenerated by the docs test in each commit. |
| spec | DVR T10 | Different sections (§10.2, §9, §11, §18 for C2; §7.1 for C3). |
| `update/review.rs`, `update/ai.rs` | C2.T4 and C2.T7-T9 | Sequential within this plan. |

## 8. Left out

### C2

- Fleet sources other than ACP sessions in this process (external processes, non-ACP agents, worktrees) and a per-agent directory (`:View ai new DIR`). The charter (charters:119-124) admits them only if the P5 session model already carries them, and it does not.
- Jump to a worktree. P5 does not model worktrees.

### C3

- Live reload of `[ui.tokens]`. The charter asks for reload semantics to be documented, not for a reload (charters:157-159).
