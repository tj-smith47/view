# view — Design Spec

Date: 2026-07-17
Status: awaiting user approval
Scope: product definition and architecture for v0.1 plus the strangler runway.
Implementation plans are derived from this document; they do not override it.

---

## 1. Product definition

**view** is a terminal-first modal editor whose engine is an embedded, pinned Neovim.
It makes three contracts to its users:

1. **Painless migration.** Your `init.lua`, plugins, LSP servers, and treesitter
   config run unmodified, because a real Neovim runs them. Compat is total by
   construction, not by reimplementation.
2. **Objectively faster, smoother UX than nvim.** Not "feels nice" — measured,
   budgeted, CI-gated, and published (uv-over-poetry level noticeable). See §3.
3. **A modern, coherent UI out of the box.** One design system, flicker-free,
   zero-config. The polish today's users assemble from noice + telescope +
   lualine + nvim-notify + which-key — native, instant, and consistent, with
   native features in the spirit of what nvim added over vi (§9), and native
   AI-agent integration (§10).

```bash
$ view .                    # your init.lua just works
$ view --nvim-bin ~/nvim    # override the bundled engine
$ view --tier basic         # force a terminal capability tier
$ view --clean              # no user config: "view bug or config bug" in one command
$ view doctor               # engine/terminal/config diagnostics
```

### Non-goals (v0.1)

- Reimplementing editing semantics, vimscript, or the nvim Lua API. The engine
  owns them; the strangler (§15) replaces subsystems only behind the oracle.
- Config migration tooling — because no migration exists to tool. The embedded
  engine resolves the user's existing nvim config exactly as bare nvim does
  (§5.4); "porting" is zero actions. What view will never do is wrap, patch, or
  generate that config: `init.lua` is never touched.
- Vim (non-neo) support.
- Collaborative editing.

### Strategic context (from 2026-07-17 market research)

The intersection "TUI editor + modern UI + native features + runs real nvim
plugins" is unoccupied. Nobody anywhere has reimplemented the nvim Lua API
(~1,300 functions + vimscript + Ex + LuaJIT ffi, measured against nvim 0.12.4);
the proven compat path is embedding real nvim (Neovide lineage; nvim 0.12 ships
`:detach`/`:connect` for external UIs). Zed is architecturally locked out
(CRDT buffer vs nvim buffer ownership), Helix ideologically. The durable moat is
the differential oracle and accumulated compat/perf evidence, not the code.

---

## 2. Engine contract (decided)

**C1 — embed at the RPC seam.** view spawns `nvim --embed`, attaches as an
external UI, and treats nvim as the sole owner of all buffer text and editing
semantics, forever mediated by msgpack-RPC. Strangling happens subsystem by
subsystem behind the differential oracle (§14), never by forking nvim's C.

Rejected alternatives, recorded for the decision log (§18): in-tree C fork
(neomacs play — upstream C merge treadmill; remains the escape hatch if the RPC
seam ever becomes the ceiling) and clean-room reimplementation (unattempted by
anyone including funded teams; incompatible with an always-working editor).

**Single source of truth rule (hard rule):** no view subsystem may hold buffer
text as authoritative state. Native features act on buffers exclusively via RPC
(`Effect::Rpc`). This is the vscode-neovim desync bug class made
unrepresentable.

---

## 3. Performance mandate

Performance is a standing contract, not a phase. Every design choice in every
implementation plan must state its latency/allocation consequences; every phase
lands with its benchmarks. "We are not done if we cannot objectively say there
is a faster, smoother UX over nvim."

### 3.1 Budgets (CI-gated once the harness lands, P3)

| Metric | Budget | Measured how |
|---|---|---|
| view input path: key event read from the terminal → RPC bytes written | **dev-linux p99 ≤ 232 µs; dev-macos UNMEASURED — no budget.** **Amended 2026-07-26, and this amendment has NO user sign-off; it is provisional and subject to reversion.** The original bar was `p99 ≤ 100 µs` measured from the harness's pty write. *The boundary was wrong*: it opened at the harness writing a byte to the pty master, so the largest single segment of the interval was the OS pty transport (dev-linux 78.6 µs p50 of a 233 µs whole), which no view code schedules. The boundary now opens at view's own key-read tap; the excluded prologue is still reported as the `pty->key-read` segment, as evidence rather than as a bar. **Caveat on the exclusion:** the key-read tap fires *after* `crossterm::event::read()` returns a parsed event, so crossterm's read and parse — view-controllable dependency code — sit inside the excluded segment and their share has never been isolated. A tap pair splitting raw-byte-readable from event-parsed would bound it. **Correction to an earlier claim in this row:** it previously asserted 100 µs was "physically inconsistent with the architecture". That is an overstatement and is withdrawn. At the *new* boundary the gated interval measures ~87 µs p50 on dev-linux and ~75 µs p50 on bare-metal mbp — both under 100 µs. What is actually established is narrower: `p99 ≤ 100 µs` is unreachable on shared and virtualized hosts, whose tails are scheduler-dominated. A quiet bare-metal p99 at the new boundary has never been measured, and until it is, no claim about the architecture's floor is supported. What remains inside the boundary is three thread transitions — input thread → runtime loop → RPC writer thread — mandated by "the paint loop never awaits RPC; the RPC reader thread never blocks"; only one gated segment is view CPU (`loop-wake->rpc-handoff`, `update()` plus the msgpack encode) at 9.4 µs p50. Note an unreconciled inconsistency: line 89 puts cross-thread wakes at 1.2–4.4 µs, while this decomposition assigns 44.7 + 32.8 µs to the two wake segments; the discrepancy is unexplained and ~70 µs of the gated interval therefore carries an unverified attribution. Recorded floor: **dev-linux 154.749 µs (machine-recorded); ×1.5 `ABSOLUTE_HEADROOM` = 232 µs.** The dev-macos floor previously stated here as "230.0 µs" **was never measured** — it was a hand-derived value, the only round number in a baseline file whose every recorded metric carries six or more decimals, and the 350 µs budget that rested on it is withdrawn along with it. dev-macos gates nothing on this row until a real capture exists. Evidence: `.claude/measurements/2026-07-26-input-path-boundary-and-tap-cost.md` | pty harness, §13.4 |
| view output path: redraw event parsed → terminal write | p99 ≤ 1 ms | pty harness |
| Keypress → cell change end-to-end, steady typing | p99 ≤ 8 ms every class; ratio ≤ 1.10× the paired bare-nvim run — **target unmet on every measured class, cause open. This amendment has NO user sign-off; it is provisional.** All classes gate ratio_p50 until the cause is attributed — **not** "measured-or-better", which this row previously claimed and which is false: the gate enforces `recorded × RATIO_HEADROOM` where `RATIO_HEADROOM = 1.25` (`crates/view-harness/src/baselines.rs`), so `echo.minimal` on dev-linux recorded at 1.3538 gates at **1.692** and the metric may silently degrade 25% — to 54% above the 1.10 target — without failing. Measured-or-better describes only the `--record` ratchet, which runs when someone deliberately re-records, not the gate. That headroom is also ~5× the apparatus's own resolvable effect (the resolution campaign measured ratio_p50 half-width 2.66%), so it was never derived from the measured floor and should be re-derived. **No mechanism carries the word "until"**: no open task exists to attribute the cause, so "until the cause is attributed" is currently a label, not a plan. History: the thread-hop explanation adopted at P3 T12 (hops ~100 µs ⇒ floor ≈ 1.19, 1.10 reachable only on bare metal) was **falsified by direct measurement**: the bare-metal M1 Max, whose cross-thread wakes are 3-4× cheaper (1.2 µs vs 4.4 µs), measures a *worse* ratio_p50 than the virtualized host, not a better one, and hops are ~500× too small to account for the view-vs-nvim gap. Do not cite the 1.19 hop floor. Quiet-host figures (mbp load 1.31, Linux calibration 0.9553): mbp 1.337 against Linux 1.199, with recorded baselines 1.3437 and 1.3538. An earlier mbp reading of 1.576 was taken at host load ~1.8-2.0 and is load-inflated by ~17% against both the recorded baseline and a quiet re-measurement; it should not be quoted, though the inversion it was cited for does persist at quiet load. The residual is unattributed and presumed to live in the RPC/UI-protocol process boundary. The taps rows put the largest single input-path segment at the pty transport (`pty->key-read`, 78.6 µs p50 / 139.4 µs p99 on dev-linux), which is ahead of that boundary; it is measured, reported every run, and is now explicitly outside the input-path bar (see the row above), but it is paid identically by bare nvim in the same paired run and so cannot explain a *ratio* — the residual remains open | pty harness, paired |
| Sustained scroll, 100k-line file, tier full | content staleness p99 ≤ 16 ms (input → corresponding scrolled content on screen) | frame log w/ staleness tags |
| First paint (UI shell visible, engine still loading) | ≤ 50 ms cold | pty harness |
| Picker match: keystroke → first results painted, 100k resident entries | ≤ 16 ms | bench suite |
| Picker scan: 1M-file tree | streaming (results while scanning, never scan-then-show); first page ≤ 100 ms warm-cache | bench suite |
| view-side memory (PSS), 10 buffers, post-workload | ≤ 150 MB | bench suite |
| Redraw under engine event burst (plugin storm, `:terminal` flood) | UI thread never blocks; coalesced paint stays inside the staleness budget | design invariant + test |

Budgets are targets until first measured; once measured, the measured-or-better
value becomes the regression baseline. **Gating is paired, not absolute:** every
bench run measures view and bare nvim in the same run on the same runner and
same resolved config, per-sample interleaved. The gated statistics are
class-dependent (amended at P3 T10 from measured evidence — median ratios were
stable within ×1.07 across load regimes whose tails swung ×300):

- **Every class** gates the view/nvim ratio_p50 (median-of-trials) AND a
  view-side absolute p99 bar within the row's budget — no row ships without a
  gated tail statistic.
- **Controlled classes** (dedicated runner) additionally gate ratio_p99 and
  the p99 of per-sample paired deltas.
- **Shared/dev classes** record those tail metrics with measured noise floors
  instead of gating them — gating on ambient noise is a false bar.
- `--gate` verifies its own precondition: a null-pair calibration (engine vs
  engine, interleaved) must sit inside a measured floor of 1.0, else the gate
  refuses and names the noise. A gate result is trustworthy by construction,
  never by hoping the host was quiet.

A dedicated bench runner is required before publishing absolute numbers.
Published comparisons: view vs bare nvim vs an nvim+LazyVim-style stack, same
machine, same config — and the picker comparison names the real incumbent:
telescope **with fzf-native**, not unaccelerated Lua.

### 3.2 The felt wins (why this is uv-over-poetry noticeable)

- **Instant shell:** view paints its UI shell before `init.lua` finishes;
  nvim's terminal is blocked until init completes. Cold start *feels* instant
  even with a heavy config.
- **UI never janks on plugin Lua:** rendering and native features live in Rust
  threads; a busy engine shows honest progress instead of freezing the frame.
- **Native picker/tree/statusline:** Rust + nucleo for the most-felt
  interactions in an editing session — benchmarked against the strongest
  incumbent (telescope + fzf-native), not a Lua strawman.
- **Flicker-free:** synchronized output + damage diffing; no partial paints,
  no "Press ENTER".

### 3.3 Design rules (enforced in review + audits)

- Hot paths (key dispatch, grid diff, paint) are allocation-free after warmup;
  `#[deny]`-level clippy perf lints; criterion micro-benches per hot path.
- The paint loop never awaits RPC. Engine I/O and input run on dedicated
  blocking threads feeding `Msg`s through a bounded channel into one
  synchronous event loop; AI sessions (P5) run async inside view-ai only,
  bridged at the crate boundary.
- Grid state uses double-buffered damage diffing; only dirty cells are encoded;
  output batched inside synchronized-update brackets.
- A standing `perf-audit` task (Taskfile) runs the bench suite and diffs
  against the recorded baseline; run before every release and after every
  strangled subsystem.

### 3.4 Measurement definitions (reproducibility contract)

Every §3.1 row is measured under a written procedure two engineers can run
independently and get the same number:

- Event boundaries: "key at pty" = byte written to the pty master by the
  harness; "terminal write" = first byte of the corresponding output flush;
  "cell change" = first vt100-parsed frame where the target cell differs.
- Environment: hermetic HOME with pinned fixture configs, fixed TERM and grid
  size (120×40); warm page cache unless the row says cold. Cold = fresh
  process, dropped caches, untouched fixture.
- Sampling: ≥ 1,000 samples after ≥ 100-sample warmup; report p50/p99/max;
  paired rows interleave view/nvim samples within one run.
- Memory is measured per-platform under its platform's own metric name, after
  the standard workload script, never peak RSS: Linux rows record `pss_mb`
  (smaps_rollup); macOS rows record `phys_footprint_mb` — the kernel's
  phys_footprint ledger, read through whichever accessor is available for the
  measured process (`proc_pid_rusage`'s `ri_phys_footprint` for a child, since
  `task_for_pid` on another process returns KERN_FAILURE unprivileged; verified
  bit-identical to `task_info(TASK_VM_INFO).phys_footprint` across 70 paired
  samples including cross-process) — baselined separately; the two platforms'
  metrics are related but not the same
  quantity, and pretending one is the other would make cross-platform numbers
  lie. Windows measurements run over ConPTY and gate that platform (§14).
- The baseline file records machine class; baselines are per-machine-class
  and per-platform.

### 3.5 Product success metrics

The product claims get falsifiable measures too, from dogfooding and tagged
issue triage:

- **Migration friction:** fraction of trial sessions reaching daily-driver
  with zero config edits — any required edit is a bug by definition.
- **Trust:** fraction of first-hour breakages triaging to view vs
  config/engine (triage flow, §12); view-caused must trend to zero before
  v0.1.
- **Noticeability:** paired-session A/B (same task, view vs nvim) — the
  uv-over-poetry bar is unprompted positive mention, logged per phase in the
  dogfooding journal.

---

## 4. Architecture

Cargo workspace. Frontend-agnostic core: nothing below `view-tui` may depend on
ratatui, crossterm, or any terminal type.

```
view/
├── Cargo.toml                 workspace
├── Taskfile.yml               build/fmt/lint/test/bench/commit/audit targets
├── crates/
│   ├── view-core/             Elm heart: Model, Msg, update() → (Model, Vec<Effect>)
│   │                          Pure. No I/O, no tokio, no rendering deps. §6
│   ├── view-engine/           nvim lifecycle + msgpack-RPC client (own, thin).
│   │                          UI events → Msg; Effect::Rpc → calls. §5
│   ├── view-surface/          Surface: render model (grids, overlays, chrome
│   │                          semantics) = render(&Model). Frontend-free. §7
│   ├── view-native/           picker, file tree, statusline, notifications,
│   │                          palette — each a Model/Msg/update sub-component. §9
│   ├── view-ai/               ACP agent client + agent panel + diff review. §10
│   ├── view-tui/              ratatui/crossterm frontend: paints Surface, reads
│   │                          input, tier detection, panic-safe terminal guard. §7
│   ├── view-oracle/           differential harness vs headless nvim (dev-dep). §13
│   ├── view-bench/            criterion + pty end-to-end latency suite. §13.4
│   └── view/                  bin: clap CLI, config, wiring, doctor
```

Dependency rules (audit-enforced, cfgd-style):
- `view-core` depends on nothing in-workspace; std + small pure crates only.
- `view-surface` depends only on `view-core`.
- `view-native` and `view-ai` depend on core (+surface for overlay types);
  never on engine or tui — effects are data, the runtime executes them.
- Only `view-engine` speaks RPC; only `view-tui` touches the terminal.
- No `unwrap`/`expect`/`panic!` in lib crates; typed errors per crate
  (`thiserror`); the bin crate renders them.

---

## 5. Engine seam

### 5.1 Process & lifecycle

- Spawn `nvim --embed` (bundled binary by default, §16) — **not**
  `--headless`, which lets startup race the attach. Plain `--embed` waits for
  `nvim_ui_attach` before sourcing the user's config (ui-startup,
  starting.txt), so `&columns`/`&lines`, `UIEnter`-gated lazy loading, and
  early blocking prompts (swapfile ATTENTION, `:confirm` during init) behave
  exactly as in bare nvim. view attaches immediately; config sources with the
  UI already live.
- Attach requesting `ext_linegrid`, `ext_cmdline`, `ext_popupmenu`,
  `ext_messages`, `ext_tabline`. `ext_multigrid` joins the set at P6 (§17)
  when pane composition lands — until then view runs single-grid by design,
  so every phase ships against the attach mode it actually uses; both modes
  are oracle-covered from P3 on. (Multigrid is the protocol's roughest
  corner; upstream PR #32691 makes multigrid the internal default — each
  engine-pin bump re-evaluates the `single_grid` knob's lifespan, and
  `doctor` recognizes multigrid-shaped failures and suggests the knob.)
- Supervision: engine exit → view keeps the last Surface painted, offers
  one-key restart; sessions/swapfiles make restart non-destructive. RPC calls
  carry timeouts; a hung engine (blocked synchronous Lua) yields an honest
  "engine busy" indicator with interrupt/restart affordances — the UI thread
  itself never blocks.
- Version handshake at attach: `nvim_get_api_info`; mismatch against the tested
  pin → doctor-grade warning, never a hard failure.
- **Clipboard:** an embedded engine has no TUI, hence no built-in OSC52 path —
  `"+y` breaking is a first-five-minutes bug. Unless the user's config sets
  `g:clipboard` (user's wins, precedence documented), view injects a provider
  that rpcrequests view: system clipboard natively, OSC52 emitted by view
  when remote (SSH/container) — clipboard works identically local and remote.

### 5.4 Config discovery & side-by-side operation

- The embedded engine inherits the environment and resolves config exactly as
  bare nvim: `$XDG_CONFIG_HOME/nvim` (`~/.config/nvim`), same runtimepath, same
  plugin state dirs. `view .` and `nvim .` run the identical setup with zero
  migration actions — this IS the compat contract, stated positively.
- `NVIM_APPNAME` passthrough (`view --appname foo` / `[engine] appname`) for
  users who want an isolated profile; never required.
- Side-by-side is a supported workflow, not an accident: both editors may run
  concurrently against the same config (normal nvim swapfile semantics apply
  to shared files; view adds no locking of its own). `view doctor` prints the
  resolved config path, engine version, and appname so users can confirm both
  processes see the same world.
- The side-by-side comparison is also the benchmark method: §13.4 runs its
  latency/startup measurements against bare nvim on the *same resolved
  config*, including a real-world heavy config fixture — not just minimal
  synthetic configs.

### 5.5 Config reconciliation (the seam between "your config" and "our UI")

Compat has three classes; only the first is "by construction":

| Class | Examples | Policy |
|---|---|---|
| Semantic (no UI ownership) | treesitter, LSP servers, cmp sources, surround, gitsigns data | By construction; compat suite covers it |
| UI-adjacent (draw inside the grid, own no surface view owns) | telescope, which-key, floating plugins | Coexist untouched; non-colliding mappings keep working |
| UI-owning (occupy a surface view renders natively) | lualine/`statusline` setters, noice, nvim-notify, tree sidebars (nvim-tree, neo-tree) | **Native wins by default** — view supersedes the overlapping surface at runtime; per-feature opt-out returns it |

- **Supersession is runtime-only and reversible.** Applied post-`VimEnter`,
  only while the native feature is enabled: statusline → `laststatus=0`
  (lualine still loads; its surface goes unused); notifications →
  `vim.notify` re-pointed at the engine default so messages flow through
  `ext_messages` into view's toasts; tree/picker → view claims its default
  keys (§5.3). **Nothing in the user's config files is ever edited, and
  nothing needs to be removed or disabled in `init.lua` for native features
  to win.** Superseded plugins keep loading; their cost is memory, not
  conflict.
- `doctor` lists every active supersession and the exact `[native]` key that
  reverses it. Overrides are per-feature (`picker = false` keeps your
  telescope; everything else stays native) — never all-or-nothing.
- Every UI-owning plugin in the §13.3 matrix is asserted in all three states:
  superseded (default), deferred (`feature = false` with the plugin
  present), and native-without-plugin.

### 5.6 CLI & process surface

- Engine-passthrough args work verbatim: `+42`, `-c`/`+cmd`, `-d`, `-R`,
  `-o/-O/-p`, `-u`. `view --clean` is the triage tool: bundled engine, no
  user config, native defaults — one command answers "view bug or config
  bug" (§12).
- `ls | view -`: RPC occupies the embed channel's stdin, so piped input
  attaches via the documented `stdin_fd` mechanism at `nvim_ui_attach` —
  behaves exactly like `ls | nvim -`.
- Exit codes propagate: `:cq[uit] N` exits view with N (git mergetool and
  `GIT_EDITOR=view` abort flows depend on it).
- Server/remote workflows (`view --remote`, attaching to a running instance
  via nvim 0.12 `:detach`/`:connect`) are a recorded post-v0.1 candidate.

### 5.2 RPC client (own, thin — decided)

Hand-rolled msgpack-RPC on `rmpv` + blocking std threads (~1–2k lines): a
reader thread and a writer thread own the pipe ends; request/response
correlation under a shared closed-flag lock; callers enqueue writes and never
touch the pipe. No async runtime: the seam's concurrency is two threads and
channels, proven against reproduced hang schedules. Flow control is
asymmetric by design — nvim's stdout is one ordered stream, so a response can
sit behind a redraw flood: the reader thread therefore *never blocks*. Responses are correlated in
the reader before any queueing; redraw notifications coalesce into a
compacting damage-merge structure (latest-wins per region); only
non-coalescible events use the bounded `Msg` channel. This is what keeps
"plugin storm" load from starving RPC responses into false engine-busy
verdicts.
Rationale: `nvim-rs` is LGPL-3.0 (static-link friction for a flagship OSS
binary) and API-unstable; the protocol is small, stable, and this seam is
exactly what the strangler instruments. Runner-up recorded in §18.

### 5.3 Input routing

```
Focus::Engine        → every key encoded and forwarded to nvim untouched.
Focus::Native(id)    → the focused overlay's update() consumes input;
                       Esc (or overlay-defined) returns Focus::Engine.
```

- **Mapping registration (native wins, never silently):** native-feature
  entry points are real nvim mappings (`<leader>ff` → `rpcnotify` back to
  view) so user remaps, which-key, and plugin introspection all see them.
  They register *after* the user's config has sourced (blocking `VimEnter`
  rpcrequest) so `mapleader` is the user's. For each **enabled** native
  feature, view's default keys are claimed even if the user's config mapped
  them (`maparg()` checked first, every supersession reported: first-run
  toast + `doctor`, with the exact `[native]` key to flip). Setting that
  feature `false` restores the user's mapping untouched — per-feature, never
  all-or-nothing. Non-colliding user mappings are never touched, and every
  native feature is also reachable via an unconditional `:View` command
  (`:View pick files`). The full default map set lives in one table in the
  docs.
- **Mouse:** terminal mouse events translate to `nvim_input_mouse` with
  per-grid coordinate translation under multigrid, after overlay hit-testing
  — clicks/wheel route to the topmost native overlay under the pointer,
  otherwise to the engine. `mouse_on`/`mouse_off` events gate whether view
  captures mouse reporting at all; with `'mouse'` off, the terminal's native
  selection is left alone.
- **Paste:** view-tui owns bracketed paste. Engine-focused pastes stream via
  `nvim_paste` — never replayed as keystrokes (no mid-paste mappings, no
  autoindent mangling, one undo unit). Native-focused pastes insert directly
  into the overlay's input.

---

## 6. Elm core

```rust
// view-core, shape (illustrative, not final signatures)
pub struct Model {
    pub engine: EngineModel,      // grids, cursor, mode, cmdline, messages, tabs
    pub native: NativeModel,      // picker, tree, statusline, notifications
    pub ai: AiModel,              // agent sessions, panel, pending diffs
    pub focus: Focus,
    pub tier: Tier,
}

pub enum Msg {
    Key(KeyEvent),
    Ui(UiEvent),                  // decoded nvim ext_* events
    Native(NativeMsg),
    Ai(AiMsg),
    EngineDown(ExitInfo),
    Tick(Instant),
}

pub enum Effect {
    Rpc(RpcCall),                 // the ONLY path that can change buffers
    SpawnAgent(AgentCmd),
    Fs(FsRequest),                // read-only (scans for picker/tree)
    Timer(Duration, TimerId),
    Quit,
}

pub fn update(model: &mut Model, msg: Msg) -> Vec<Effect>;
```

`update()` is pure and synchronous; all effects are data executed by the
runtime (engine/tui/ai tasks), whose results re-enter as `Msg`s. This is what
makes the core headless-testable and the oracle cheap.

---

## 7. Surface & rendering

- `Surface = render(&Model)`: a tree of positioned layers — engine grids,
  native overlays (picker, tree, palette, toasts, AI panel), statusline —
  with z-order, borders, padding, and style refs into one theme table.
- `view-tui` diffs consecutive Surfaces into minimal terminal ops, batched
  inside synchronized-update brackets (BSU/ESU). Damage tracking is
  view-side; ratatui's buffer diff is the last-mile backstop.
- **Theme:** one design system. Defaults derive from the engine's *live*
  highlight state — the `default_colors_set`/`hl_attr_define` event stream,
  not a one-shot query — and re-derive on `ColorScheme` via an
  autocmd→rpcnotify bridge (the same bridge the statusline's diagnostics and
  git segments use). The last derived theme is cached keyed by config path
  and used for first paint, so cold start opens in *your* colors with no
  theme flash. Native elements share border style, padding scale, and accent
  palette. Overridable in `view.toml`.
- **Cursor:** `mode_info_set` drives per-mode cursor shape (translated to
  DECSCUSR), and the real terminal cursor is positioned at the logical
  cursor — required for IME preedit placement and screen readers, not
  cosmetics.
- **Terminal capability tiers** (auto-detected via terminfo + capability
  queries with timeouts; tmux and SSH are first-class detection cases —
  query through rather than trusting `TERM=screen-256color`, with `doctor`
  guidance on tmux `allow-passthrough`; overridable):

| Tier | Assumes | Experience |
|---|---|---|
| `full` | kitty/ghostty/wezterm-class: synchronized output, truecolor, undercurl, kitty keyboard proto | Everything: rounded borders, animations (cell-eased), curly underlines |
| `standard` | truecolor ANSI, no sync guarantee | Full layout/design, animations off, internal double-buffer against flicker |
| `basic` | 16-color, ASCII-safe | Correct and complete, plain borders, no color derivation |

Degradation is a first-class tested surface (golden snapshots per tier, §13),
not a fallback apology.

---

## 8. Startup sequence

1. Paint UI shell (statusline frame, empty grid, spinner) — target ≤ 50 ms,
   themed from the §7 cache.
2. Immediately spawn engine and attach (attach precedes config sourcing,
   §5.1); the user's config sources with the UI live.
3. Keys typed before the engine is ready are buffered and replayed in order
   after attach (bounded; overflow drops with a toast, never silently).
4. Post-`VimEnter` (blocking rpcrequest): register native mappings (§5.3),
   apply reconciliation (§5.5), re-derive theme.
5. Stream first grid content as it arrives; swap spinner for buffer.
6. `doctor`-grade checks (version pin, tier, PATH-nvim drift) report as
   toasts, never block.

---

## 9. Native features (v0.1)

Each is a `view-native` sub-component with its own Model/Msg/update, rendered
as a Surface overlay, acting only via `Effect::Rpc`. Each ships with: config
toggle (disabling returns that surface to the user's plugins), theme
compliance, budget from §3.1, oracle non-interference test (feature open ≠
engine state drift).

| Feature | Replaces for the 90% case | v0.1 scope |
|---|---|---|
| Fuzzy picker | telescope | files / buffers / live-grep; `nucleo` matcher; streaming results; preview pane via RPC buffer read |
| File tree | neo-tree / netrw | toggleable sidebar, git status decorations, file ops via RPC/fs effects |
| Statusline | lualine | mode (`msg_showmode`, incl. macro `recording @q`), pending `showcmd`, file, diagnostics (RPC), git branch, ruler/position; single-line |
| Notifications | nvim-notify / noice messages | `ext_messages` with kind-aware routing (table below); kills "Press ENTER" without ever eating a prompt |
| Command palette | noice cmdline | `ext_cmdline` → centered floating palette with completion rendering (`ext_popupmenu` when sourced from cmdline) |

`ext_messages` routing — owning messages means owning *dialogs* (it forces
`cmdheight=0`; the engine's message area ceases to exist):

| Message kind | Treatment |
|---|---|
| `confirm`, `return_prompt`, inputlist-class | Modal native prompt overlay: takes `Focus::Native`, replies via RPC. The engine is *blocked* on these — a timeout toast here would hang first-run plugin bootstraps |
| `emsg` / `echoerr` | Sticky toast until dismissed; captured in history |
| `msg_showmode` / `msg_showcmd` / `msg_ruler` / `search_count` | Statusline segments (macro recording must always be visible) |
| everything else | Transient toast with timeout + scrollback history |

Post-v0.1 candidates (recorded, not dropped — pitch before any is cut):
which-key-style hint overlay, minimap, inline git hunks, popupmenu
documentation panel, session/project switcher.

---

## 10. AI integration (native, first-class)

Differentiator: an **agent-native terminal editor**. Today, agentic coding in
nvim means Lua-plugin UIs (avante, codecompanion) with the exact jank view
exists to kill. view treats the agent as a peer subsystem with native UI.

### 10.1 Architecture

- `view-ai` implements a client for the **Agent Client Protocol (ACP)** —
  JSON-RPC over stdio to agent processes (Claude Code, Gemini CLI, any
  ACP-speaking agent). One protocol, any agent; view proxies no credentials —
  agents own their own auth. *(Verify current ACP spec/version via context7 at
  implementation time; protocol is young.)*
- Agent sessions are tokio tasks emitting `Msg::Ai`; the panel is a native
  overlay (streaming markdown, tool-call status), toggled via a real nvim
  mapping like every native feature.
- **Context flows out** via RPC reads: current buffer, selection, cursor,
  diagnostics, quickfix — assembled by `view-ai`, never by scraping the
  screen.
- **Edits flow in** as reviewable native diff overlays: hunk-by-hunk
  accept/reject rendered by view, applied through batched
  `Effect::Rpc(nvim_buf_set_text …)` calls, one undo step per accept
  (undojoin policy). Hunks rebase live against concurrent user edits
  (`nvim_buf_attach` change events adjust offsets); a hunk whose context no
  longer matches is marked stale and re-diffed — never force-applied. Edits
  to files with no loaded buffer are reviewed in an RPC-loaded hidden buffer
  and written through the engine.
- **Out-of-band writes are detected, not pretended away.** view routes what
  ACP lets it route (client fs capabilities), but agents with shell tools
  can write disk directly — no client can prevent that. view watches the
  workspace; on external change to a loaded buffer it drives the engine's
  checktime path, with a conflict UI when unsaved edits collide. The §2 rule
  holds unconditionally for view's own subsystems; for agents it is
  routed-where-possible + detected-always, and the docs say so plainly.

### 10.2 Scope line (v0.1)

In: ACP client, agent panel, context providers, native diff review, workspace
fs-watch + conflict UI, config (`[ai] agent = "claude-code"` or arbitrary
command), per-project trust prompt before first agent launch. Out (recorded candidates): inline ghost-text
completions (users' existing plugins keep working meanwhile), multi-agent
orchestration, embedded model serving.

Existing AI nvim plugins remain fully functional throughout — compat contract.

---

## 11. Config

view-level config only; every key optional with a derived default (nothing
required that view can determine itself). Empty or absent file = full
experience.

```toml
# ~/.config/view/view.toml (XDG paths; all keys optional)
[ui]
tier = "auto"              # auto | full | standard | basic
theme = "auto"             # auto = derive from nvim colorscheme

[engine]
nvim_bin = "bundled"       # "bundled" | absolute path
appname = ""               # NVIM_APPNAME passthrough for isolated profiles
single_grid = false

[native]
picker = true
tree = true
statusline = true
notifications = true
palette = true

[ai]
enabled = true
agent = "claude-code"      # ACP agent id or ["cmd", "args…"]
```

CLI flags mirror config (`--tier`, `--nvim-bin`); precedence: flags > env
(`VIEW_*`) > file > derived defaults.

---

## 12. Error handling & resilience

- Terminal guard: raw mode / alt screen restored on every exit path including
  panic (Drop guard + panic hook). A crashed view never leaves a broken
  terminal.
- Engine failures: §5.1 supervision. User text is never lost by a view fault:
  view holds no authoritative text (§2), and engine restarts inherit nvim's
  swapfile/session safety.
- All lib-crate errors are typed; the bin maps them to actionable messages
  (and `doctor` explains environment-shaped failures: missing bundled engine,
  tier misdetection, ACP agent not found).
- Triage flow (day-2 trust): `view --clean` (§5.6) answers "view or config"
  in one command; `doctor` prints the exact bare-nvim repro invocation
  (`nvim -u <resolved-init>`) so any breakage bisects to view / engine /
  config in two steps.
- AI failures degrade to a panel-local error state; they can never stall the
  paint loop or the engine stream.

## 13. Testing & the oracle

The moat. CI-gated from P3 onward; nothing merges that fails it.

1. **Core tests:** pure `update()` → table-driven + property tests, no
   harness. Every Msg/Effect pair reachable headlessly.
2. **Differential oracle (`view-oracle`):** scripted input sequences run
   through (a) view headless with a vt100-parser terminal double and (b) a
   reference nvim with a UI attached over RPC using the *identical
   ext-option set* (a bare `--headless` nvim renders no grid to compare).
   The equivalence relation is explicit:
   - **State parity (exact):** buffer text, cursor, mode, registers, marks —
     RPC probes on both sides.
   - **Grid parity (masked):** engine-grid regions only; view-owned chrome
     (statusline, toasts, overlays) masked out; ext-layer structural
     differences equalized by the matched attach options.
   - **Quiesce protocol:** diffs are taken only when both sides are idle —
     N ms of redraw-event quiescence under a controlled clock, timers
     drained or disabled, fixed seeds, hermetic env (pinned HOME, TERM, grid
     size). Divergence outside the relation is a bug; a flaky-by-timing diff
     is a harness bug, equally filed.
   Gates every strangled subsystem (§15): parity proven over that
   subsystem's committed corpus manifest before a Rust path replaces a
   delegated path.
3. **Compat suite:** real-config matrix — bootstrap lazy.nvim + top plugins
   inside view's engine; drive interactive flows; assert zero errors and
   expected UI. The matrix must include the UI-owning class (§5.5) — lualine,
   noice, nvim-notify, nvim-tree/neo-tree, dressing, fidget — each asserted
   in all three §5.5 states, alongside the semantic class (telescope,
   nvim-treesitter, nvim-cmp, mini.nvim, which-key). A fresh-machine
   scenario (cold lazy.nvim bootstrap including its interactive prompts) is
   mandatory. The maintainer's own daily config is a standing scenario
   (§5.4). The published compat-evidence page (anodizer docsite pattern) has
   a defined row schema — plugin, version, engine pin, scenario, state,
   result, date — a coverage model (top-N by plugin-manager download rank
   per compat class), and a staleness rule: every engine-pin bump re-runs
   the matrix and re-dates the page.
4. **Perf harness (`view-bench`):** criterion micro-benches (hot paths) + pty
   end-to-end latency measurements implementing §3.1 under the §3.4
   measurement spec; baseline file in-repo per machine class; gating is the
   §3.1 paired-ratio rule. Scenarios run paired against bare nvim on the
   same resolved config (§5.4), minimal and heavy-config fixtures both, plus
   a `:terminal` output-flood scenario (the known slow path for external
   UIs). Windows measurements run over ConPTY and gate the §14 tier
   promotion.
5. **Tier goldens:** ANSI snapshot tests per tier via vt100 emulation.
6. **Fuzz:** random keystroke storms view-vs-oracle; divergence minimized and
   filed as a bug, never waived.
7. Windows/macOS validation over `ssh winserver` / `ssh mbp` (real evidence,
   not CI-only), per standing practice.

## 14. Platform targets

Linux and macOS tier-1 from P1; Windows tier-2 (crossterm + bundled nvim zip)
promoted to tier-1 before v0.1 ships. Promotion is a named P6 exit criterion:
§3.1 budgets measured over ConPTY (§13.4) and validated on winserver — not
deferred to CI faith.

## 15. Strangler roadmap (post-v0.1 direction)

Order by user-felt-latency × oracle tractability; each step lands only behind
its differential suite. Recorded intent, not v0.1 scope:

1. Fuzzy/search paths already native (picker) — extend to `:grep`-class flows.
2. Syntax/render pipeline (view-side treesitter highlighting for the visible
   viewport; note: query dialects differ from nvim-treesitter's — translation
   required, oracle-verified).
3. LSP UI surfaces (diagnostics rendering, hover, signature help) over
   engine-owned LSP state.
4. Core buffer/editing: last, possibly never — moves only if the oracle can
   prove parity per permutation and the perf win is measured, not assumed.

Each subsystem's gate includes a **corpus manifest committed before
implementation starts**: the exact input set parity is claimed over (file
set, grammar/LSP versions, locales, encodings, terminal sizes). "Parity
proven" means proven over that manifest — no manifest, no strangling.

## 16. Release & distribution

- Released by **anodizer** as a tested pair: view binary + pinned nvim per
  platform, checksummed, cosign-signed. A view release is never validated
  against a floating engine version.
- `--nvim-bin`/config override honored; `doctor` warns on drift from the pin.
- The engine runs with its own `$VIMRUNTIME` exported and the bundled binary
  fronted for child processes, so plugins that shell out to `nvim` or read
  the runtime see the pin — not whatever is on PATH; `doctor` reports
  PATH-nvim vs pin drift.
- ACP agents needing adapters (e.g. Claude Code via its ACP adapter) get
  them auto-provisioned on first `[ai]` use, pinned and checksummed like the
  engine — "no post-install steps" spans the AI feature too.
- Single install command; no post-install steps. Repo: `/opt/repos/view`,
  master branch, tj-smith47 identity, `.claude/` gitignored, Taskfile from
  day one.

## 17. Phases (implementation plans derive from these)

```
P0  Repo bootstrap: workspace, Taskfile, CI, lint/audit hooks, .claude rules
P1  Engine seam: spawn/attach, own RPC client, raw grid painted via ratatui,
    latency measurement v0 (measure from the first week)         ← editable
P2  Elm core + Surface + input/focus + tiers + ext_cmdline/messages/tabline
P3  Oracle + compat suite + bench harness + CI gates              ← the moat
P4  Native features: picker, tree, statusline, toasts, palette + theming
P5  AI: ACP client, agent panel, context providers, diff review
P6  Polish: multigrid attach + panes, config surface, doctor, docs, tier
    goldens, Windows tier-1 promotion (ConPTY-gated, winserver-validated)
                                                                  ← v0.1
```

Every phase ends with: `/verify` clean, its §3.1 budgets measured, its oracle/
bench additions in CI, and a dogfooding note. P3 sits deliberately before
features: the compounding assets (oracle, evidence) front-load ahead of the
copyable ones (polish).

## 18. Decision log

| Decision | Chosen | Runner-up, and why not |
|---|---|---|
| Engine contract | C1: embed real nvim at RPC seam, strangle behind oracle | In-tree C fork (neomacs play): upstream merge treadmill; kept as escape hatch. Clean-room: unattempted by anyone, breaks always-working rule |
| Primary frontend | TUI (ratatui/crossterm) | GUI-first: terminal is the primary use case |
| App architecture | Elm-shaped core, hand-rolled thin runtime | bubbletea-style framework dependency: the runtime IS the editor's control loop; frameworks own what we must own. Extracting ours later stays open |
| RPC client | Own thin client (rmpv + blocking threads) | nvim-rs: LGPL-3.0 static-link friction + unstable API |
| Runtime model (amended at P1 exit) | Sync event loop + std threads at the engine seam; async confined to view-ai from P5, bridged at the crate boundary | tokio throughout (original §5.2 text): re-opens a twice-hardened concurrency seam three phases before async has a consumer; scheduler layers on the latency-critical path; the sync unified loop is itself the latency fix. Two-way door: bridging the seam into a runtime later is exactly what the wrapper would have done |
| Fuzzy matcher | nucleo | skim/fzf bindings: slower, external-process shape |
| Buffer truth | Engine-only, RPC-mediated (hard rule) | Mirrored buffer state: vscode-neovim's permanent desync bug class |
| AI protocol | ACP (agent-agnostic) | Bespoke per-agent integrations: N× maintenance, no ecosystem leverage |
| Engine distribution | Bundled pinned pair + override | System nvim default: compat claims float against unknown versions |
| Native-vs-plugin overlap default | Native wins, per-feature opt-out, runtime supersession only (§5.5) | Defer-to-plugins default (reviewer-recommended): maximally unsurprising, but ships the product hidden — the migration audience would never see what view is |

## 19. Risks

| Risk | Mitigation |
|---|---|
| `ext_multigrid` instability | Single-grid is the shipped mode until P6; multigrid oracle-covered before panes ship; upstream unification (PR #32691) tracked — `single_grid` knob lifespan re-evaluated at each engine-pin bump |
| Agent out-of-band writes (shell tools bypass diff review) | Routed-where-possible + detected-always (§10.1): fs watch, checktime drive, conflict UI; documented honestly |
| Native-wins supersession surprises a user's muscle memory | Every supersession reported (first-run toast + doctor) with the exact per-feature off switch; §13.3 asserts all three states per UI-owning plugin |
| Blocked engine (sync Lua) still blocks editing | UI stays live with honest busy state + interrupt; positioning stays honest about it; strangler reduces exposure over time |
| nvim core absorbing UI polish (0.12 extui) | view's delta is native perf + coherence + native features + AI, not borders; re-audit positioning each nvim release |
| ACP churn (young protocol) | Version-pinned adapter behind a view-ai trait; verify spec at implementation time |
| Perf claim vs engine-bound latency | Budgets measure view overhead separately from engine echo; felt wins (§3.2) don't depend on beating C at editing |
| AI-era fast follower | Front-load the compounding moat (oracle, evidence, published benchmarks) before polish |
