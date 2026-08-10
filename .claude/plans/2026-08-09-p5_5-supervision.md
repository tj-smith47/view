# P5.5 Implementation Plan — Engine Supervision

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development to implement this plan
> task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** the editor that cannot freeze (spec:615, §9 invented capability,
v0.1 CORE — ruled 2026-08-05, spec:896, no "post-v0.1" framing anywhere in
this plan). Stall detection on both directions of the RPC connection
(write-side already exists; read-side is the gap this plan closes),
an interrupt offer, and automatic restart with swap (`-r`) rehydration —
"a misbehaving plugin cannot take the editor down." Sequenced first inside
P5.5 alongside P3 remote editing (spec:608-609, `HANDOFF.md:67-68`).

**Why this is invention, not tuning:** nvim's own TUI is in-process with its
core, so a wedged core is a dead terminal — a category difference view can
claim that bare nvim structurally cannot
(`p4-end-perf-pitch-table.md:158-160`). This plan does not move an existing
latency number; it creates a new measured quantity — availability — where
today there is none (`p4-end-perf-pitch-table.md:133-137`: zero bench
scenarios, zero recorded rows for hang/survival/restart).

**Architecture:** a read-side liveness watch in `view-engine`, shaped like
the existing `OutboxStallWatch` (`view-engine/src/stall.rs`) — fold two
cheap observations, never block, never touch the connection lock — riding
the `nvim_get_mode` heartbeat channel that already answers under a blocked
main loop (`view-engine/src/nvim_api.rs:36-41`). A new `EngineBusy` model
state and UI affordance in `view-core`/`view-tui`, reusing the sticky-banner
mechanism the write-stall notice already established for "busy" and a new
modal overlay for the interrupt/restart offer. A `restart()` path in
`view-engine::process` that reuses `Engine::spawn`/`EngineConfig` and adds
the `-r` recovery flag automatically, with a headless-safe `SwapExists`
auto-recover so restart never blocks on nvim's own interactive swap prompt.
`view-oracle` coverage for reproduced hang schedules (both wedge kinds) and
a `view-bench` scenario for the new availability metrics.

**Tech stack:** no new dependency. The heartbeat rides the existing
msgpack-RPC channel and `nvim_get_mode`; the watch itself is atomics, the
same shape `OutboxStallWatch` already uses.

**Authored against:** tree at `b96296c` (branch `dev/p4-native-features`).
Re-verify signatures with `grep -n "pub " crates/<crate>/src/<file>.rs`
before writing code if this plan's citations seem stale; reality wins.

**Status:** DRAFT — not approved for execution.

## Revision history

**Round 1 fixes** (review verdict: NOT APPROVED), applied against the
original draft:

- **BLOCKER — Hard Rule 8 violation in proposed shipping doc:**
  `Engine::restart()`'s rustdoc no longer references "Task 2's dispatch";
  rewritten to state the contract standalone. Full-draft swept for other
  instances; four additional in-fence Task-number references found in the
  "As-built interfaces" citation block (`stall.rs`, `handle.rs`,
  `runtime.rs`, `model.rs` annotations) and cleaned to standalone prose.
- **IMPORTANT — heartbeat reinvented an existing seam:** Task 1 redesigned
  to reuse `EngineHandle::request_async`/`request_probe`
  (`handle.rs:858-865`) via a new `Waiter::Heartbeat` variant,
  `Msg::HeartbeatReply`, and `route_heartbeat`, instead of the original
  bespoke `probe_sent_at`/`probe_acked_at` atomics side-channel. The
  prober thread (cadence source) and two atomics (`sent_generation`,
  `sent_at`, now the only genuine cross-thread edge) are kept as the one
  piece the reused seam cannot provide on its own.
- **MINOR — impossible acceptance-sample timing:** Task 7's sample output
  corrected to be consistent with `HEARTBEAT_WEDGE_THRESHOLD=10s` +
  `PROBE_INTERVAL=2s` (banner at 11.6s, not 2.1s). Task 2 gained an
  explicit **Dead bypasses the grace period** rule (banner+modal
  immediate, `Interrupt` disabled) since Dead never self-resolves, making
  the corrected 0.4s sample consistent with the plan's own escalation
  design rather than just its constants.

Status remains DRAFT pending re-review.

**Round 2 fixes** (landing-order amendment, requested by P5.5-media's
plan): `HeartbeatWatch` gains `pause()`/`resume()` — media's Task 3
(`.claude/plans/2026-08-09-p5_5-media.md`, "Landing-order dependency on
P1 supervision") suspends this plan's prober around its `mpv`
full-terminal handoff and cannot begin implementation until this
contract lands here. The contract is media's own round-3/round-4
settled design, reproduced here verbatim rather than re-derived:
`pause()` stops the prober thread's `tick()` from issuing any *new*
probe; `resume()` resets only `sent_at`, never `sent_generation` or
`acked_generation`, so the counters are monotonic-forward across a
pause/resume cycle. Combined with `record_ack`'s existing `max(current,
incoming)` fold, a stale ack from a probe already in flight when
`pause()` fires (bounded by `HEARTBEAT_WEDGE_THRESHOLD /
HEARTBEAT_PROBE_INTERVAL`, ~4-5 at this plan's constants, since ticks
are not ack-gated) can raise `acked_generation` to at most that
pre-pause value — always strictly less than any post-resume
`sent_generation` — so it cannot mask a genuine post-resume hang. New
`HeartbeatWatch` struct field (`paused: AtomicBool`), two new API
methods, a `tick()` doc clause, and a new Task 1 step (implement
pause/resume with a stale-ack-after-resume test) added below. No
existing Task 1 method signature or verdict logic changes.

## Global Constraints

Hard rules, embedded verbatim. Every task's requirements implicitly include
this section, and every task states which rule bounds it (this phase lives
dangerously close to three of them at once).

- **nvim owns all buffer text. No view subsystem holds authoritative text
  state. Buffer mutation happens only through `Effect::Rpc`.** Binds
  hardest here: a restart never reconciles "view's own copy" of buffer
  text against the fresh engine, because view never held one. The only
  state a restart recovers is whatever nvim's own swap file already
  persisted (§ "State recovery and buffer truth" below) — never a
  view-side undo log, never a replayed keystream.
- **The paint loop never awaits RPC. The RPC reader thread never blocks.**
  Binds hardest here: the heartbeat *send* must never originate on the
  paint loop's own turn, and the paint loop's `observe()` call must be
  pure atomic reads, exactly like `OutboxStallWatch::observe`
  (`stall.rs:78-90`). Task 1 states the exact wiring that keeps this true.
- **No unwrap/expect/panic in lib crates.** `EngineError` (`handle.rs:18-38`,
  `#[non_exhaustive]`) is the type every liveness-probe and restart failure
  path returns through; no new panic path is introduced by this plan.
- **Dependency direction: core ← surface ← {native, ai}; only view-engine
  speaks RPC; only view-tui touches the terminal.** The liveness watch and
  restart mechanism live in `view-engine`; the busy/interrupt/restart model
  state lives in `view-core`, pure, with no RPC awareness; the affordance's
  paint lives in `view-tui` via the existing `Surface`/overlay path.
  `scripts/audit-deps.sh` gates every new edge in the same commit that
  introduces it — this plan introduces none.
- **Performance is a contract.** The liveness probe is hot-path by
  definition — it runs every loop pass. Its latency consequence is not an
  afterthought; it is the design constraint every task in this plan is
  built around (§ "The hot-path cost bound" below).
- Use `task` targets, never raw cargo/git, for build/fmt/lint/test/commit.
  Commit only via `task commit -- -m "<msg>"`.
- Comments are WHY-only; doc comments render for users and carry a WHAT
  summary. No session-narrative markers.
- Non-conventional commit prefixes are parenthesised scopes:
  `feat(supervision):`, `test(oracle):` — never `supervision:`.
- **Never weaken an existing budget, the 5 µs tap bar, or the 1.15
  calibration floor.** New capabilities get NEW budget rows (Task 6);
  no existing row's `max` or `ratio` changes.
- **v0.1 framing: this is a CORE v0.1 feature** (2026-08-05 ruling,
  spec:896) — no task, comment, or commit message in this plan uses
  "post-v0.1" language about anything this plan ships.

## The hot-path cost bound the design must respect

Quoted verbatim from the pitch table because it is the number every task
below is designed against (`p4-end-perf-pitch-table.md:114-124,179-194`):

```
tap instrumentation, measured:   p50 0.288 us / p99 0.332 us   (task-16a-report.md:118)
tap overhead bar:                5.0 us p99                    (taps_rows.rs:32)
key-decoded->loop-wake segment:  13.9 us p50 / 28.4 us p99      (post-T16a)
```

Constraint, stated as a number: **the supervision probe must add ≤ 5 µs p99
to a loop pass.** A per-keystroke *send-and-wait* heartbeat is unaffordable
against a 13.9 µs segment; a per-loop-pass *observation* is the only
affordable shape — the same one `note_write_stall` already uses every pass
at zero measured cost across the T16a/T16b/footprint-diet campaigns
(`runtime.rs:1127`). Every task below that touches the loop states this
bound explicitly in its own commit.

## Open design questions — resolved in this plan

The research (`invention-research.md:560-589`) left three P1 forks with no
recommendation, or a recommendation this plan now adopts as the design.
Resolved here, not escalated, per each question's own nature (a plan
decision, not a business call):

1. **Which UI mechanism carries busy / interrupt / restart?** *Decided:
   both, split by what each already means in this codebase.* "Busy"
   (wedged-but-alive, no user action needed yet) is a sticky-banner
   condition via `Messages::set_native_condition` — the exact mechanism
   `note_write_stall` already uses for the write-side notice, so a wedged
   read side gets the same visual language a wedged write side already has.
   "Interrupt offer" and "restart confirmation" (a moment that needs an
   explicit user choice, once the busy state has held past a second,
   longer threshold) is a new `OverlayKind::EngineBusy` modal — Prompt-
   shaped (accept/decline), reusing the accept/decline machinery
   `PromptState` already has rather than inventing a second one. This
   coordinates with P5-images' own need for a new overlay variant
   (`invention-research.md:490-493`) by following the same growth pattern:
   a new `OverlayKind` variant, `#[non_exhaustive]`-compatible, no change
   to the existing four.
2. **Does the heartbeat live inside `note_write_stall`'s call site, or
   beside it?** *Decided: a sibling call*, per the research's own
   recommendation (`invention-research.md:571-579`) — `note_write_stall`'s
   name and doc comment are specifically about the write side; a read-side
   heartbeat is a distinct type-level concern (Task 1's `HeartbeatWatch`,
   not a new mode of `OutboxStallWatch`), called right beside
   `note_write_stall` at `runtime.rs:1127`, never merged into it.
3. **What counts as wedged vs. dead, and what threshold?** *Decided: a
   distinct, supervision-owned threshold*, sized by the same pattern
   `WRITER_STALL_THRESHOLD`'s doc comment already reasons from (double the
   existing 5 s single-round-trip allowance) but not the same *value* —
   `GET_MODE_TIMEOUT` stays a caller-side abort bound for synchronous
   callers (test/oracle harnesses), and gains no second meaning. Task 1
   introduces `HEARTBEAT_WEDGE_THRESHOLD: Duration = Duration::from_secs(10)`,
   independently named and independently justified, exactly mirroring
   `WRITER_STALL_THRESHOLD`'s own doc-comment reasoning rather than
   sharing its constant.

## State recovery and buffer truth

Spec `spec:67` (§2, Engine contract): "text as authoritative state. Native
features act on buffers exclusively via RPC." Restated as a hard rule in
`.claude/CLAUDE.md`. What this plan trusts, stated exactly, because the
charter's "restart with swap rehydration" clause is meaningless without it:

| After a restart, view trusts... | ...and must rehydrate from swap... |
|---|---|
| Which files/buffers were open (view's own `TablineState`/buffer-list model, already held for the tabline — no new state) | Every unsaved edit since nvim's last swap-file flush (`updatetime`/`updatecount`-paced, not per-keystroke) |
| Nothing about buffer *content* — view never had a copy to trust | The content itself, via nvim's own `-r` recovery, not a view-side log |
| The cursor/window layout it last painted, as a **display** hint only (repainted from the new engine's own state once attached, never asserted as fact) | Nothing — layout is not swap-recoverable and this plan makes no claim that it is |

This is honest about loss: the recovery guarantee is exactly nvim's own
crash-recovery guarantee (whatever the swap file captured), no better and
no worse — view adds *detection* and *automatic recovery invocation*, not a
stronger data guarantee than nvim itself provides. No task in this plan
claims zero-loss recovery; Task 4's acceptance criterion states the real
bound.

## Coverage walk

Every charter deliverable and pitch-table finding, mapped to a task.

| Requirement | Where |
|---|---|
| Read-side / main-loop hang detection (absent today — `p4-end-perf-pitch-table.md:146`) | Task 1 |
| Wedged-vs-dead discrimination, distinct recoveries (interrupt vs. restart) | Task 1 |
| ≤ 5 µs p99 per-loop-pass probe cost | Task 1 (design), Task 6 (measured) |
| `EngineBusy` model state (absent today — `grep EngineBusy` 0 hits) | Task 2 |
| Busy banner + interrupt/restart modal affordance | Task 2 |
| `restart()` on `Engine` (absent today — `grep restart` 0 hits) | Task 3 |
| `-r` flag applied automatically on a recovering respawn (absent today) | Task 3 |
| Headless-safe `SwapExists` auto-recover (no interactive prompt hang) | Task 4 |
| `[supervision]` config: `auto_restart` toggle | Task 2 (registry follows T1-of-P4's `[native]` pattern) |
| Oracle: reproduced hang schedules, both wedge kinds, survival asserted | Task 5 |
| New bench scenario: time-to-notice, UI survival, restart+rehydration time | Task 6 |
| No existing latency row moves | Task 6 (gate), Exit checklist |
| Kill/hang acceptance script per detection path | Task 7 |

## As-built interfaces this plan builds on

Read from the tree at `b96296c`.

```rust
// view-engine/src/stall.rs -- the write-side liveness watch this plan's
// read-side probe mirrors the shape of, not the value
pub const WRITER_STALL_THRESHOLD: Duration = Duration::from_secs(10);
pub struct OutboxStallWatch { threshold: Duration, delivered: u64, since: Option<Instant> }
impl OutboxStallWatch {
    pub fn new(threshold: Duration) -> Self;
    pub fn observe(&mut self, handle: &EngineHandle) -> bool;  // 2 relaxed atomic loads, never blocks
}

// view-engine/src/nvim_api.rs -- the heartbeat channel this plan builds on
const GET_MODE_TIMEOUT: Duration = Duration::from_secs(5);
// doc: "nvim_get_mode is answered on receipt even while nvim's main loop
// is busy or blocked" -- the wedged-vs-dead discriminator

// view-engine/src/handle.rs
#[non_exhaustive]
pub enum EngineError { Rpc(RpcError), Io(io::Error), Remote(Value), Closed,
    Timeout { method: String, timeout: Duration } }

// view-engine/src/handle.rs -- the existing async-probe seam this plan
// reuses rather than re-inventing (handle.rs:64-123,845-865; damage.rs:514-525).
// EngineHandle::request_probe issues a fire-and-forget request tagged with
// a generation via Waiter::HlProbe { generation }; the reader thread's
// Response-dispatch decodes the eventual reply and routes it to the pump
// as Msg::HlProbeReply { generation }, so a stale reply can be dropped by
// update() instead of clobbering a newer verdict. Five existing waiter
// variants (HlProbe, BufferList, Preview, Rename, CreatePrompt, ...) all
// share this exact shape.
enum Waiter { HlProbe { generation: u64 }, /* ...existing variants... */ }
impl EngineHandle {
    pub fn request_probe(&self, method: &str, params: Vec<Value>, generation: u64)
        -> Result<(), EngineError>;
    fn request_async(&self, method: &str, params: Vec<Value>, waiter: Waiter)
        -> Result<(), EngineError>;  // shared by every async request wrapper
}

// view-engine/src/process.rs -- the ONLY place a child is spawned
#[non_exhaustive]
pub struct EngineConfig { pub nvim_bin: PathBuf, pub extra_args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>, pub env_remove: Vec<OsString>,
    pub handshake_timeout: Duration, pub shutdown_timeout: Duration,
    hermetic: bool, /* + private stdin_relay */ }
impl Engine {
    pub fn spawn(cfg: EngineConfig) -> Result<Self, EngineError>;
    // NO restart()/respawn() method exists today
}
fn build_command(cfg: &EngineConfig) -> Command;  // the sole spawn seam

// view/src/runtime.rs -- the loop pass this plan extends
const ENGINE_STALLED_NOTICE: &str = "keystrokes queued: nvim has stopped reading view's output";
fn note_write_stall(model: &mut Model, watch: &mut OutboxStallWatch, handle: &EngineHandle) -> bool;
// called once per pass at the point immediately before the paint that
// would show its effect -- the sibling liveness call this plan adds goes
// right beside it

// view-core/src/model.rs
#[non_exhaustive]
pub enum OverlayKind { Prompt(PromptState), Picker(PickerState), Tree(TreeState),
    MessageHistory(MessageHistoryState) }   // this plan adds EngineBusy(EngineBusyState)
impl Messages {
    pub fn set_native_condition(&mut self, text: Option<&str>) -> bool;  // the sticky-banner mechanism
}

// view-engine/src/env.rs -- swap directory pre-creation (f7cff8c), a
// race-condition fix, NOT swap-recovery machinery; establishes the
// directory exists, does not read or replay it
```

## File structure (new/changed this phase)

```
crates/view-engine/src/
  heartbeat.rs      # T1: HeartbeatWatch, HEARTBEAT_WEDGE_THRESHOLD, wedged()/dead() discrimination
  process.rs        # T3: Engine::restart(), recovery-flag application
  handle.rs         # T1: Waiter::Heartbeat + EngineHandle::request_heartbeat, mirrors the existing request_probe/HlProbe seam
  damage.rs         # T1: PumpShared::route_heartbeat, mirrors route_probe_reply
crates/view-core/src/
  model.rs          # T2: OverlayKind::EngineBusy, EngineBusyState
  msg.rs            # T1: Msg::HeartbeatReply { generation }, mirrors Msg::HlProbeReply
  native/
    supervision.rs  # T2: pure model + update for the busy/interrupt/restart affordance
crates/view/src/
  runtime.rs        # T1: sibling heartbeat observation beside note_write_stall
  main.rs           # T2: [supervision] config read (auto_restart)
crates/view-oracle/src/
  hang.rs           # T5: reproduced hang schedules, both wedge kinds
crates/view-bench/src/scenarios/
  supervision.rs    # T6: new scenario -- time-to-notice, survival, restart+rehydration time
crates/view-bench/budgets.toml   # T6: new [[budget]] rows, none existing touched
scripts/acceptance/supervision.sh  # T7: kill/hang acceptance script
```

---

### Task 1: Read-side liveness probe — wedged-vs-dead heartbeat

**Files:**
- Create: `crates/view-engine/src/heartbeat.rs`
- Modify: `crates/view-engine/src/handle.rs` (`Waiter::Heartbeat`,
  `EngineHandle::request_heartbeat` — mirrors the existing
  `Waiter::HlProbe`/`request_probe` seam exactly), `crates/view-engine/src/damage.rs`
  (`PumpShared::route_heartbeat`, mirrors `route_probe_reply`),
  `crates/view-core/src/msg.rs` (`Msg::HeartbeatReply { generation: u64 }`,
  additive, `#[non_exhaustive]`), `crates/view-engine/src/lib.rs` (module),
  `crates/view/src/runtime.rs` (sibling call beside `note_write_stall`,
  plus one new arm in the existing Msg-dispatch that already handles
  `Msg::HlProbeReply`)

**Rule bound:** "the paint loop never awaits RPC; the RPC reader thread
never blocks." This is the task where that rule is easiest to violate by
accident — the fix is that nothing on the paint loop's turn ever *sends*
anything; it only reads atomics, exactly like `OutboxStallWatch::observe`.

**Consumer call-site first:**

```rust
// runtime.rs's loop, right beside note_write_stall (T1's sibling-call decision)
if note_write_stall(&mut model, &mut write_stall, &engine.handle) {
    model.dirty = true;
}
if note_engine_liveness(&mut model, &mut heartbeat, &engine.handle) {
    model.dirty = true;
}
```

**Design — reusing the existing async-probe seam instead of a bespoke
atomics side-channel.** `EngineHandle` already has exactly this shape for a
different probe: `request_probe`/`request_async`/`Waiter::HlProbe { generation }`
issues a fire-and-forget request, and the reader thread's existing msgid-
resolution match decodes the eventual `Response` and routes it to the
connection's pump as `Msg::HlProbeReply { generation }`
(`handle.rs:64-123,845-865`, `damage.rs::PumpShared::route_probe_reply`) —
tagged with a generation specifically so a stale reply can never clobber a
newer verdict, which is precisely both opens this task would otherwise
need bespoke atomics to solve (never blocking the caller; a stale reply
losing to a newer one). Task 1 adds a sibling to that seam, not a new
mechanism: a `Waiter::Heartbeat { generation: u64 }` variant, a matching
`EngineHandle::request_heartbeat(generation) -> Result<(), EngineError>`
wrapper (mirrors `request_probe` line for line), a
`Msg::HeartbeatReply { generation: u64 }` arm in `view-core::msg`, and a
`route_heartbeat` method on `PumpShared` mirroring `route_probe_reply`.

What the reused mechanism genuinely cannot provide, and what stays
bespoke: neither `request_async` nor the Msg-reply plumbing has any notion
of a clock or a cadence — they only send when told and deliver when
replied. A dedicated, single-purpose prober thread (spawned once,
alongside the existing reader/writer threads, same lifetime as `Engine`)
is still needed as the cadence source: it ticks on a fixed interval
(`HEARTBEAT_PROBE_INTERVAL`, 2 s — well inside the 10 s wedge threshold so
at least 4 probes land before a verdict flips) and calls
`request_heartbeat` with a monotonically increasing generation each tick.

Reusing the Msg-reply seam also makes the ack-side state *simpler* than a
bespoke design would have been, not merely equivalent: because the reply
now arrives on the same Msg channel the runtime loop already drains every
pass (the same channel `Msg::HlProbeReply` rides today), the ack can be
recorded by a plain field mutated from that one dispatch site — no
cross-thread atomic write is needed for the ack at all, since the write
and the eventual `observe()` read both happen on the runtime loop's own
thread. Only `sent_generation`/`sent_at` remain atomics, because those two
really are written by the separate prober thread and read by the paint
loop's `observe()` call — the one genuine cross-thread edge this design
has, down from two in the original bespoke-atomics shape.

**Interfaces:**

```rust
// view-engine/src/handle.rs additions -- mirrors Waiter::HlProbe /
// request_probe / route_probe_reply exactly (see the as-built citation
// above); the only difference is which Waiter variant carries the reply
enum Waiter {
    /// Mirrors `HlProbe`: the eventual `Response` carries no payload this
    /// plan cares about (an ack is a "still reading" signal, not a
    /// value), decoded and routed to the pump as `Msg::HeartbeatReply`,
    /// tagged with `generation` so a stale reply can never clobber a
    /// newer verdict -- the same "safe default over a stuck generation"
    /// precedent every other async waiter in this enum already documents.
    Heartbeat { generation: u64 },
    // ...existing variants unchanged...
}

impl EngineHandle {
    /// Issues `nvim_get_mode` as a fire-and-forget liveness probe tagged
    /// with `generation`. Mirrors [`request_probe`](Self::request_probe)
    /// line for line. Intended for `crate::heartbeat`'s prober thread
    /// only -- the paint loop never calls this.
    ///
    /// # Errors
    /// Returns `EngineError::Closed` on the same terms as
    /// [`request_probe`](Self::request_probe).
    pub fn request_heartbeat(&self, generation: u64) -> Result<(), EngineError> {
        self.request_async("nvim_get_mode", vec![], Waiter::Heartbeat { generation })
    }
}
// reader thread's existing Response-dispatch match gains one arm,
// mirroring the HlProbe arm exactly:
//   Some(Waiter::Heartbeat { generation }) => {
//       pump.route_heartbeat(Msg::HeartbeatReply { generation });
//   }
```

```rust
// view-core/src/msg.rs -- one new Msg arm, mirroring HlProbeReply
pub enum Msg {
    // ...existing arms unchanged...
    /// A heartbeat probe was acknowledged. `generation` lets the receiver
    /// discard a stale reply exactly as `HlProbeReply` already does.
    HeartbeatReply { generation: u64 },
}
```

```rust
// view-engine/src/damage.rs -- mirrors route_probe_reply exactly
impl PumpShared {
    pub(crate) fn route_heartbeat(&self, msg: Msg) { /* same shape as route_probe_reply */ }
}
```

```rust
// view-engine/src/heartbeat.rs
use std::time::{Duration, Instant};
use crate::handle::EngineHandle;

/// How long a probe may go unacknowledged, while the connection is still
/// open, before the engine counts as wedged rather than merely slow to
/// answer this one probe.
///
/// Independently sized from `GET_MODE_TIMEOUT` (a caller-side abort bound
/// for synchronous callers) and from `WRITER_STALL_THRESHOLD` (the write
/// side's own verdict), per the same doubling-the-round-trip-allowance
/// reasoning `WRITER_STALL_THRESHOLD`'s doc comment already applies --
/// copying the pattern, not the value, because a wedged read side and a
/// wedged write side are different failures needing different recoveries.
pub const HEARTBEAT_WEDGE_THRESHOLD: Duration = Duration::from_secs(10);

/// How often the background prober issues a fresh `nvim_get_mode` probe.
/// Four probes fit inside `HEARTBEAT_WEDGE_THRESHOLD` before a verdict can
/// flip, so one dropped or slow reply does not read as a false wedge.
pub const HEARTBEAT_PROBE_INTERVAL: Duration = Duration::from_secs(2);

/// What one [`HeartbeatWatch::observe`] call reports about the read side.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// The last probe was acknowledged inside the threshold, or no probe
    /// has had time to go stale yet.
    Alive,
    /// A probe has been outstanding past the threshold while the
    /// connection itself is still open -- nvim is reading fine and
    /// blocked in something synchronous (spec's own "hung engine (blocked
    /// synchronous Lua)" shape).
    Wedged,
    /// The connection is closed. Distinct from `Wedged`: a wedged engine
    /// may still recover on its own (a long Lua call returning); a dead
    /// one never will, so the recovery this reports drives is
    /// restart-with-`-r`, not an interrupt offer.
    Dead,
}

/// Watches one connection's read side across successive observations.
/// `sent_generation`/`sent_at` are relaxed atomics, written by the prober
/// thread's [`tick`](Self::tick) and read by [`observe`](Self::observe) --
/// the one genuine cross-thread edge left in this design.
/// `acked_generation` is a plain field: it is written only by
/// [`record_ack`](Self::record_ack), called from the runtime loop's own
/// Msg-dispatch on `Msg::HeartbeatReply` -- the same thread that later
/// calls `observe`, so no atomic is needed for the ack. No allocation on
/// either path -- the paint loop can afford to call `observe` every pass,
/// the same discipline [`crate::stall::OutboxStallWatch::observe`]
/// already holds to.
#[derive(Debug)]
pub struct HeartbeatWatch {
    threshold: Duration,
    sent_generation: AtomicU64,
    sent_at: AtomicU64,      // epoch millis
    acked_generation: u64,
    paused: AtomicBool,      // gates the prober thread's tick(), see pause()/resume()
}

impl Default for HeartbeatWatch {
    fn default() -> Self { Self::new(HEARTBEAT_WEDGE_THRESHOLD) }
}

impl HeartbeatWatch {
    #[must_use] pub fn new(threshold: Duration) -> Self { /* ... */ }

    /// Called by the prober thread each tick: bumps `sent_generation`,
    /// stamps `sent_at`, and issues the probe via
    /// [`EngineHandle::request_heartbeat`]. The one send this design
    /// performs; never called from the paint loop. No-ops (returns
    /// `Ok(())` without sending) while [`pause`](Self::pause) is in
    /// effect.
    pub fn tick(&self, handle: &EngineHandle) -> Result<(), EngineError>;

    /// Called from the runtime loop's Msg dispatch on
    /// `Msg::HeartbeatReply { generation }` -- the same dispatch site that
    /// already handles `Msg::HlProbeReply`. Records the max of the
    /// current and incoming generation, so an out-of-order stale reply
    /// (the entire reason the probe is generation-tagged) cannot move the
    /// ack backwards.
    pub fn record_ack(&mut self, generation: u64);

    /// Folds `sent_generation`/`sent_at`/`acked_generation`, plus whether
    /// the connection itself is still open, into a verdict. Never blocks,
    /// never sends anything -- the send lives on the prober thread's
    /// `tick`, the ack-write lives on the runtime loop's `record_ack`;
    /// this call is a pure read on whichever thread calls it, so it
    /// cannot violate "the paint loop never awaits RPC" no matter what
    /// the connection is doing.
    #[must_use]
    pub fn observe(&self, connection_closed: bool) -> Liveness { /* ... */ }

    /// Stops the prober thread's [`tick`](Self::tick) from issuing any
    /// *new* probe (checks the `paused` flag before sending). Does NOT
    /// guarantee no reply arrives afterward: probes already dispatched by
    /// prior ticks (bounded by `HEARTBEAT_WEDGE_THRESHOLD /
    /// HEARTBEAT_PROBE_INTERVAL`, ~4-5 at this module's constants, since
    /// ticks are not ack-gated) are still in flight and the connection's
    /// peer still answers them. Never rewinds `sent_generation`. First
    /// consumer: P5.5-media's terminal handoff, which pauses this watch
    /// for the duration of a blocking child-process call.
    pub fn pause(&self);

    /// Clears the paused flag and resets `sent_at` to now. Deliberately
    /// does NOT reset `sent_generation` or `acked_generation` — both are
    /// left exactly where `pause()` found them, so the pair is
    /// monotonic-forward across a pause/resume cycle. Because
    /// `record_ack`'s fold is `max(current, incoming)`, a stale ack for a
    /// probe sent before `pause()` can raise `acked_generation` to at
    /// most that pre-pause value — strictly less than any
    /// `sent_generation` a post-resume tick produces — so it can never
    /// satisfy a post-resume `Wedged` check and mask a genuine subsequent
    /// hang; the late reply folds in harmlessly instead.
    pub fn resume(&mut self);
}
```

**Falsifiable check:** a test ticks the watch (advancing `sent_generation`
via a fake `EngineHandle`) without ever calling `record_ack`;
`HeartbeatWatch::observe` reports `Alive` before `HEARTBEAT_WEDGE_THRESHOLD`
elapses (using an injectable clock, the same `new(threshold)` escape
`OutboxStallWatch` already provides for tests) and `Wedged` after. A
second test passes `connection_closed = true`; `observe` reports `Dead`
regardless of elapsed time or ack state.

- [ ] **Step 1: Failing test for the wedged verdict.** Write
  `HeartbeatWatch::observe` returning `Alive` unconditionally; the test
  ticks past the threshold with no ack and asserts `Wedged`. Run: expect
  FAIL on the wrong verdict, not a compile error.
- [ ] **Step 2:** Implement `tick`/`record_ack`/the `observe` fold. Test
  passes.
- [ ] **Step 3: Failing test for the dead verdict.** Pass
  `connection_closed = true` immediately; assert `Dead` even with zero
  elapsed time and no ticks issued. Implement the connection-state check
  `observe` needs. Passes.
- [ ] **Step 4:** Implement `Waiter::Heartbeat`, `EngineHandle::request_heartbeat`
  (mirrors `request_probe`), the reader thread's new dispatch arm, and
  `Msg::HeartbeatReply`/`PumpShared::route_heartbeat` (mirrors
  `Msg::HlProbeReply`/`route_probe_reply`) — all five pieces trace
  directly to an existing precedent, per the design section above.
- [ ] **Step 5:** Implement the prober thread, spawned in `Engine::spawn`
  alongside the existing reader/writer threads, calling `tick` at
  `HEARTBEAT_PROBE_INTERVAL`. Wire `record_ack` into the runtime's
  existing Msg-dispatch arm for `Msg::HeartbeatReply`, and wire
  `note_engine_liveness`'s `observe` call into `runtime.rs`, sibling to
  `note_write_stall` at `runtime.rs:1127`, never merged into it (open
  question 2, decided above).
- [ ] **Step 6: `pause()`/`resume()` for the media landing-order
  amendment.** Implement the `paused` flag and the two methods per their
  doc comments above. Test: (a) tick the watch once, `pause()`, `resume()`,
  then deliver that first tick's `record_ack` late (after `resume()`) —
  assert `observe` still reports `Alive` (the stale ack is harmless); (b)
  from that same post-resume point, advance the fake clock past
  `HEARTBEAT_WEDGE_THRESHOLD` without ever acking a post-resume tick —
  assert `observe` now reports `Wedged` (a genuine post-resume hang is
  still caught despite the earlier stale ack). Both cases must pass
  together — proving the fold masks the stale ack without also masking a
  real one is the point of the test, not either half alone.
- [ ] **Step 7: Disconfirm.** Tick the watch against a fake handle whose
  replies never route back to `record_ack`; `task test` shows the wedged
  path firing; restore the healthy fake (replies routed); passes.
- [ ] **Step 8:** `task perf-audit` on `taps` and `echo` scenarios; confirm
  the added atomics reads and the one new Msg-dispatch arm cost nothing
  measurable against the 5 µs p99 tap bar (state the actual delta in the
  commit, per the perf contract). Commit: `feat(supervision): read-side
  liveness probe, reusing the request_probe/HlProbe async-reply seam`.

---

### Task 2: `EngineBusy` model state + UI affordance

**Files:**
- Create: `crates/view-core/src/native/supervision.rs`
- Modify: `crates/view-core/src/model.rs` (`OverlayKind::EngineBusy`),
  `crates/view-core/src/msg.rs` (a `Msg` arm carrying `Liveness`
  transitions — `#[non_exhaustive]`, additive only), `crates/view/src/runtime.rs`
  (dispatch), `crates/view/src/main.rs` (`[supervision]` config read)

**Rule bound:** dependency direction — this state is pure `view-core`, no
RPC awareness, matching every other native feature's crate-seam correction
already established in P4 (`.claude/plans/2026-07-26-p4-native-features.md:339-378`);
this plan reuses that seam rather than re-deciding it.

**Design (open question 1, resolved above):** two states, one mechanism
each.

```rust
// view-core/src/native/supervision.rs
/// The busy/interrupt/restart affordance's own state, pure and headless-
/// testable. Owns no RPC handle and issues no `Effect::Rpc` itself except
/// the one restart request, which the runtime executes exactly like any
/// other `Effect`.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineBusyState {
    /// When the wedge was first observed, for a "how long" readout the
    /// modal shows -- not for any timing decision made in `view-core`,
    /// which has no clock; the runtime stamps this on transition.
    pub since: SinceStamp,
    pub kind: WedgeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WedgeKind { ReadSide, WriteSide, Dead }

/// What the user chose on the interrupt/restart modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisionChoice { Interrupt, Restart, Dismiss }
```

Two thresholds define the transition, and they apply differently to
`Wedged` and `Dead` because the two verdicts warrant different patience:
`HeartbeatWatch`/`OutboxStallWatch` report `Wedged`/stalled → the sticky
banner (`set_native_condition`) raises immediately, same as today's
write-side notice. For `Wedged`, a **second, longer** threshold (proposed
`ENGINE_BUSY_MODAL_THRESHOLD = Duration::from_secs(30)`, independently
justified in the doc comment: long enough that a legitimate slow
operation — a large plugin's synchronous startup hook, a big
`:%s/.../.../ge` — does not interrupt the user with a modal for something
that was always going to finish) escalates the sticky banner into the
`OverlayKind::EngineBusy` modal, offering `Interrupt` (sends a `SIGINT`-
equivalent nvim can react to, via `RpcCall::Input` with the interrupt key
notation) or `Restart` (Task 3). For `Dead`, the modal escalation is
**immediate** — the banner and the modal both raise on the same
observation, with no grace period — because the entire reason
`ENGINE_BUSY_MODAL_THRESHOLD` exists is to give a *possibly self-resolving*
condition room to resolve on its own, and a closed connection is never
going to resolve on its own; waiting 30 s to offer the only recovery that
exists (`Restart`) would only delay it. `Dead` also never offers
`Interrupt` (no input path survives a closed connection) — `Restart` is
the modal's only enabled choice in that case.

**Falsifiable check:** a headless `update()` test drives a `Liveness::Wedged`
sequence past both thresholds and asserts the banner raises at the first,
the modal at the second; a second test drives `Liveness::Dead` directly and
asserts banner and modal both raise on the same observation, with
`Interrupt` disabled and only `Restart`/`Dismiss` offered. `Interrupt`/
`Restart`/`Dismiss` each produce the right `Effect` or state transition
with no engine attached (the Msg-level oracle's own reach, per P4's
crate-seam note).

- [ ] **Step 1: Failing test for banner-then-modal escalation.** Drive a
  `Wedged` past the first threshold only; assert banner raised, no
  overlay. Drive past the second; assert `OverlayKind::EngineBusy`
  present. FAIL first (no escalation implemented), then implement, then
  pass.
- [ ] **Step 1b: Failing test for the `Dead` bypass.** Drive `Liveness::Dead`
  directly (no elapsed time); assert banner and `OverlayKind::EngineBusy`
  both present on the same observation, and that the modal's `Interrupt`
  choice is disabled. FAIL first (no bypass implemented), then implement,
  then pass.
- [ ] **Step 2:** `SupervisionChoice::Interrupt` produces
  `Effect::Rpc(RpcCall::Input { notation: "<C-c>" })` (verify the exact
  notation against a live pinned nvim in the same step, per planning-
  protocol wire-fact discipline — do not assume `<C-c>` reaches a
  synchronous Lua call the same way it reaches normal-mode input; this is
  a genuine unknown this plan does not guess past).
- [ ] **Step 3:** `SupervisionChoice::Restart` produces the effect Task 3
  consumes; `Dismiss` clears the modal, leaves the sticky banner (the
  underlying condition has not changed).
- [ ] **Step 4:** `[supervision]` config: `auto_restart: bool` (default
  `true`), the one genuine user choice this plan exposes — everything else
  (thresholds, probe cadence) is an internal constant, not a config knob,
  per the config-derivability rule (a threshold the tool has already sized
  correctly is not a footgun to hand the user). Example:
  ```toml
  [supervision]
  auto_restart = true   # false: view surfaces busy/dead and waits for a manual restart choice
  ```
  When `false`, the modal's `Restart` choice still works manually; only the
  *automatic* respawn-on-`Dead` (Task 3) is gated by this flag.
- [ ] **Step 5:** `task ci`. Commit: `feat(supervision): EngineBusy model
  state, sticky banner, interrupt/restart modal`.

---

### Task 3: `Engine::restart()` — reusing the local spawn seam

**Files:** Modify `crates/view-engine/src/process.rs`

**Rule bound:** "only view-engine speaks RPC" — the restart path is
entirely inside `view-engine`; the runtime only sees a new `Engine` handle
after the fact, same as the first `spawn()`.

**Interfaces:**

```rust
impl Engine {
    /// Tears down the current child (the existing `Drop` sequence: `qa!`,
    /// bounded wait, `SIGKILL` fallback) and spawns a fresh one from the
    /// same `EngineConfig`, with `-r` appended to `extra_args` so the new
    /// process attempts nvim's own swap recovery. Returns the new
    /// `Engine`; the caller's old handle is consumed, matching `spawn`'s
    /// own ownership shape -- there is never a moment with two live
    /// engines the runtime could address by mistake.
    ///
    /// # Errors
    /// Same as [`Engine::spawn`]: a restart that fails to come back up
    /// returns the same `EngineError` shapes; the caller is responsible
    /// for surfacing that as a second, more severe notice rather than
    /// retrying silently.
    pub fn restart(self, cfg: EngineConfig) -> Result<Self, EngineError>;
}
```

`build_command` needs no change — `-r` travels through the existing
`extra_args` passthrough (`process.rs:606-618`) exactly like any other
engine-passthrough arg §5.6 already names.

**Falsifiable check:** an oracle-level test spawns an `Engine`, kills the
child process directly (bypassing `Engine`'s own `Drop`, simulating a real
crash), and asserts `restart()` produces a new, live `Engine` whose
`extra_args` contains `-r`.

- [ ] **Step 1: Failing test.** Kill the child out-of-band; call
  `restart()`; assert the new `Engine` answers `nvim_get_mode`. FAIL
  (method does not exist), implement, pass.
- [ ] **Step 2: Failing test for the `-r` flag.** Assert the new child's
  command line (readable via `/proc/<pid>/cmdline` on the CI's Linux
  runners, or an injectable command-capture seam for portability) contains
  `-r`. Implement, pass.
- [ ] **Step 3: Disconfirm.** Restart twice in a row (simulating a second
  crash immediately after recovery); assert no zombie survives either
  teardown (same guarantee `spawn`'s own doc already states) and no panic.
- [ ] **Step 4:** `task ci`. Commit: `feat(supervision): Engine::restart
  reuses the local spawn seam, applies -r`.

---

### Task 4: Headless-safe swap recovery — `SwapExists` auto-answer

**Files:** Modify `crates/view-engine/src/process.rs` or
`crates/view-engine/src/env.rs` (whichever already owns startup Lua
injection — verify against the tree before choosing; `g:clipboard`
injection in P4's T6 is the precedent to match)

**Rule bound:** "the paint loop never awaits RPC" extends to startup: a
restart's handshake must not hang waiting on nvim's own interactive
`[O]pen/[E]dit/[R]ecover/[D]elete/[Q]uit` swap prompt, which an embedded,
headless `--embed` process cannot render or answer through the normal
terminal path.

**Design:** a `SwapExists` autocmd, injected the same way the clipboard
provider's Lua is injected (P4 T6's precedent — verify the exact injection
call site against the as-built tree, since this plan does not re-derive
it), programmatically sets `v:swapchoice = 'r'` (nvim's own documented
mechanism, `:help SwapExists`) so recovery is automatic and headless-safe
on every restart, not only the one Task 3 triggers. This is the "-r
rehydration" the charter names, made non-interactive.

**Falsifiable check:** an oracle test opens a buffer, kills the engine
mid-session (leaving a swap file), restarts via Task 3, and asserts (a) the
handshake completes within `handshake_timeout` with no hang, and (b) the
recovered buffer's content matches what was on screen at the last swap
flush — not necessarily the last keystroke, per the honest bound in
"State recovery and buffer truth" above.

- [ ] **Step 1: Failing test for the hang.** Without the autocmd, restart
  after a real crash with a dirty buffer; assert the handshake would time
  out (this characterizes the bug before fixing it — run once, observe the
  timeout, then proceed).
- [ ] **Step 2:** Inject the `SwapExists` autocmd at the same startup point
  P4's clipboard provider Lua lands. Re-run Step 1's scenario; handshake
  completes.
- [ ] **Step 3: Failing test for content recovery.** Assert the recovered
  buffer contains the swap-flushed content, not empty and not silently
  discarding the recovery.
- [ ] **Step 4: Disconfirm.** Crash with a *clean* buffer (no unsaved
  changes, no swap file created); assert restart produces no
  `SwapExists` event at all and no spurious recovery notice.
- [ ] **Step 5:** `task ci`. Commit: `fix(supervision): SwapExists
  auto-answers 'r' so a headless restart never hangs on nvim's own prompt`.

---

### Task 5: Oracle — reproduced hang schedules, both wedge kinds

**Files:** Create `crates/view-oracle/src/hang.rs`

**Rule bound:** none directly (oracle-only), but this is the task that
proves Tasks 1-4 respect every rule above under adversarial conditions,
not just the happy path.

**Design:** two reproducible hang shapes, driven the same way
`view-oracle/src/pty.rs`'s existing kill-signal helper already drives a
crash (`killpg`/`SIGKILL`, cited in the research as existing precedent):

1. **Read-side wedge:** a lua chunk executed via the existing eval seam
   that blocks synchronously (`vim.wait(large_ms)` or an infinite loop with
   a bounded external kill as the test's own safety net) — exercises
   Task 1's `Wedged` verdict and Task 2's banner-then-modal escalation.
2. **Dead connection:** `killpg` the child directly (existing helper) —
   exercises `Liveness::Dead` and Task 3's restart path end-to-end,
   including Task 4's swap recovery.

**Falsifiable check:** both schedules run under `task oracle`; each asserts
the UI survives (the harness's own process does not crash or hang), the
correct `Liveness` verdict is reported within
`HEARTBEAT_WEDGE_THRESHOLD + HEARTBEAT_PROBE_INTERVAL` of the schedule
starting, and — for the dead case — a restarted engine answers
`nvim_get_mode` again within `handshake_timeout` of the restart firing.

- [ ] **Step 1:** Read-side wedge schedule; assert `Wedged` verdict timing.
- [ ] **Step 2:** Dead-connection schedule; assert `Dead` verdict timing.
- [ ] **Step 3:** Auto-restart end-to-end (with `auto_restart = true`);
  assert the harness process survives and a fresh engine answers within
  bound.
- [ ] **Step 4:** `auto_restart = false` variant; assert no automatic
  respawn occurs and the modal stays open until a manual `Restart` choice.
- [ ] **Step 5: Disconfirm.** Remove Task 1's heartbeat wiring temporarily;
  assert both schedules now fail to detect their wedge (the oracle catches
  the regression it exists to catch). Restore.
- [ ] **Step 6:** `task ci`. Commit: `test(oracle): reproduced hang
  schedules for both wedge kinds, survival and restart asserted`.

---

### Task 6: New bench scenario — availability metrics

**Files:**
- Create: `crates/view-bench/src/scenarios/supervision.rs`
- Modify: `crates/view-bench/budgets.toml` (new `[[budget]]` rows only —
  no existing row's value changes)

**Rule bound:** performance contract — this is where the ≤ 5 µs p99
per-pass claim (Task 1) and "no existing latency row moves" (this plan's
own gate) become measured, gated facts rather than design intentions.

**New metrics, proposed** (per the claim-ladder framing —
`invention-research.md:72-99` — this unlocks claim rung 2,
"can't-freeze smoothness"):

```toml
[[budget]]
spec_row = "Engine supervision: time to notice a read-side hang"
scenario = "supervision"
metric = "wedge_detect_p99_ms"
max = 12000.0   # HEARTBEAT_WEDGE_THRESHOLD (10s) + HEARTBEAT_PROBE_INTERVAL (2s) ceiling

[[budget]]
scenario = "supervision"
metric = "restart_rehydrate_p99_ms"
# no max yet -- first recording establishes the baseline this row gates
# against on the next campaign, same as every other scenario's first run
```

Plus a regression-tripwire check (not a new row, an assertion inside this
scenario's own test) that `echo.minimal ratio_p50` and
`input_path.key_to_rpc_p99_us` are unchanged, paired, with the heartbeat
prober running vs. not — closing the "heartbeat cost leaks onto the echo
path" risk the pitch table names (`p4-end-perf-pitch-table.md:200`).

**Falsifiable check:** `task bench -- supervision` produces both new
metrics from a real run against a real pinned nvim; the paired echo/input
comparison shows no measurable delta (state the actual numbers in the
commit, per the perf contract — a bare "no regression" claim is not
evidence).

- [ ] **Step 1:** `supervision.rs` scenario: drives Task 5's two hang
  schedules through the bench harness's own `BenchSession`
  (`view-bench/src/session.rs`), timing wedge-to-detection and
  restart-to-rehydrated.
- [ ] **Step 2:** Baseline recording on dev-linux; arm the new rows.
- [ ] **Step 3:** Paired A/B campaign, heartbeat prober present vs. a build
  with Task 1 reverted, on `echo.minimal` and `input_path`; record the
  delta (expected: within noise, per the design bound).
- [ ] **Step 4:** `task ci` + `task perf-audit`. Commit: `perf(bench): new
  supervision scenario, availability metrics armed, echo/input rows
  confirmed unmoved`.

---

### Task 7: Conformance acceptance script

**Files:** Create `scripts/acceptance/supervision.sh`

**Design:** a scripted run driving each detection path independently
against a real terminal session (not the bench/oracle harness's simulated
pty, though it may reuse the same spawn helpers) and observing recovery,
matching the exit checklist's required acceptance shape.

```bash
$ scripts/acceptance/supervision.sh
[1/3] read-side wedge (blocked Lua) ... banner at 11.6s, modal at 30.4s, interrupt recovers        OK
[2/3] dead connection (SIGKILL)     ... banner+modal at 0.4s (Dead skips the grace period),
                                         restart recovers, swap rehydrated                          OK
[3/3] write-side wedge (existing)   ... banner at 10.2s (regression check, unchanged)                OK
```

Timings above are illustrative, bounded by the plan's own constants: the
read-side banner cannot fire before `HEARTBEAT_WEDGE_THRESHOLD` (10s) and
should land within one further `HEARTBEAT_PROBE_INTERVAL` (2s) of it, so a
plausible sample sits in `[10.0s, 12.0s)`; the modal for a `Wedged` verdict
follows at `ENGINE_BUSY_MODAL_THRESHOLD` (30s) from wedge onset; a `Dead`
verdict's banner and modal coincide, near-instantly, per Task 2's bypass
rule.

- [ ] **Step 1:** Script the read-side wedge path end-to-end, real
  terminal.
- [ ] **Step 2:** Script the dead-connection path end-to-end, asserting
  swap content survives.
- [ ] **Step 3:** Script the pre-existing write-side path as a regression
  guard (this plan must not have disturbed it).
- [ ] **Step 4:** Wire into `task ci` or a dedicated `task acceptance`
  target, whichever the repo's `Taskfile.yml` already exposes a slot for
  (verify at implementation; do not invent a new top-level task name if an
  existing acceptance-class target fits).
- [ ] **Step 5:** Commit: `test(acceptance): scripted supervision
  detection/recovery proof, all three paths`.

---

## P5.5-Supervision Exit Checklist

- [ ] `task ci` green (fmt-check, lint, audit, style, loc, test).
- [ ] `task oracle` green, including Task 5's reproduced hang schedules
      (both wedge kinds).
- [ ] `task perf-audit` green: the two new supervision rows recorded and
      gated; every pre-existing §3.1 row unchanged (Task 6's paired
      campaign cited with its actual numbers).
- [ ] Task 7's acceptance script run and its output captured as evidence
      (kill/hang under each detection path, recovery observed).
- [x] Spec-amendment check: this plan touches no owed spec amendment.
      Verified against `invention-research.md:§1.4,§6.2` — spec:343-346
      already correctly states remote editing (not supervision) as v0.1
      core with no "post-v0.1" language; the one owed amendment in this
      research package is the media-handoff row at `spec:620`, out of
      this plan's scope. No spec edit ships with this plan -- that
      cross-plan amendment satisfied by `a9f2c98`.
- [ ] No "post-v0.1" language anywhere in this plan's shipped code,
      comments, or commit messages (v0.1 CORE framing, 2026-08-05 ruling).
- [ ] `.claude/known-bugs.md` drained, or every remaining item carrying
      explicit user approval.
- [ ] Dogfood note appended to `.claude/dogfood-journal.md` — a real
      wedge/crash encountered (or deliberately induced) during daily use,
      not only the scripted acceptance run.
- [ ] `.claude/plans/INDEX.md` gains the P5.5-supervision row (this
      draft's HTML-comment header above) when the plan is moved under
      `.claude/plans/` — resolves `invention-research.md:§6.4`.
