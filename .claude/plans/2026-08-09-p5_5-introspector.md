# P5.5 Implementation Plan — Key Introspector

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development to implement this plan
> task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `:View keys log` — a live overlay answering "which mapping fired,
whose it was, what it displaced," on demand, during a real session
(spec:618, §9 invented capability, v0.1 CORE — ruled 2026-08-05, spec:896,
no "post-v0.1" framing anywhere in this plan).

**Why this is invention, not tuning:** nvim's own `:map`/`:verbose map`
answer "what is bound" as a static lookup against the current mapping
table; neither answers "what just fired," live, as keys are pressed — and
neither can, since nvim's own TUI has no interposed observer between a
keypress and the mapping it triggers. view's separate-process, every-
keystroke-interposed architecture is what makes a live log possible at
all, the same premise every other P5.5 capability rests on
(spec:609-611).

**Correcting `invention-research.md`'s characterization of the seam:** the
research sheet states "no per-dispatch event... present in this table" —
true only of `mappings.rs`'s own static registration table
(`MappingSpec`/`MappingClaim`, compile-time, never fires per-keystroke).
It is not true of the tree as a whole: `Msg::FeatureInvoke { feature:
String, verb: String }` (`crates/view-core/src/msg.rs:92-95`) is nvim's
own `rpcnotify` landing as a live, per-dispatch event *every time* a
registered default key or `:View` command actually fires, confirmed live
in two places — `update.rs`'s `Msg::FeatureInvoke { feature, verb } =>`
arm and `vlog.rs:121`'s existing `"invoke feature={feature} verb={verb}"`
log line. This plan's real job is not inventing a new event; it is
combining this already-existing live event with the already-stored
registration-time claim data (`Model::claimed_keys() -> &[MappingClaim]`,
`model.rs:75,166,176`) into a bounded, browsable history.

**Architecture:** a new `native::keys` module holding `KeyLogState`, a
bounded `VecDeque`-backed ring buffer shaped identically to `ToastHistory`
(`toast.rs:81-120`), fed by a sibling arm inside `update()`'s existing
`Msg::FeatureInvoke` match (the same message `vlog.rs` already logs, no
new message invented). Each entry carries the fired `{feature, verb}`
pair, a timestamp, and — resolved at push time, not lazily — whether this
invocation's key currently has a user override (`had_user_mapping`, from
the matching `MappingClaim` looked up once at registration time and
copied into the log entry, so the overlay never needs to re-cross-
reference the static table at render time). A new sixth registry feature
(`id: "keys"`) gives the introspector its own default key and doctor row
for free, matching every other native feature's registration shape.

**Tech stack:** no new dependency; reuses `Msg::FeatureInvoke`,
`MappingClaim`, and the `ToastHistory` ring-buffer shape verbatim.

**Authored against:** tree at `ad2a39a` (branch `dev/p4-native-features`).
Re-verify signatures with `grep -n "pub " crates/<crate>/src/<file>.rs`
before writing code if this plan's citations seem stale; reality wins.

**Status:** DRAFT — not approved for execution.

## Revision history

**Round 1 fixes** (review verdict: APPROVED, minors only), applied against
the original draft:

- **MINOR — spec-line citations:** spec:616 → spec:618 (this plan's own
  charter row), both occurrences plus the Exit Checklist's spec-amendment
  check.
- **MINOR — `MappingClaim` interface citation:** corrected `feature`/`lhs`
  from `&'static str` to the real owned `String` fields
  (`mappings.rs:49`); `MappingSpec` (a genuinely `&'static str`-backed,
  compile-time table) was already correct and is unchanged.
- **MINOR — Task 1's "matching ToastHistory exactly" overstatement:**
  narrowed to the real match — ring-buffer *shape* is identical,
  ownership signature is not (`ToastHistory::push` borrows `&MessageEntry`;
  `KeyLogState::push` takes an owned `KeyLogEntry`, a deliberate difference
  stated with its rationale, not a defect).

## Global Constraints

- **nvim owns all buffer text. No view subsystem holds authoritative text
  state. Buffer mutation happens only through `Effect::Rpc`.** Binds
  loosely here (the introspector touches no buffer text at all) but is
  stated for completeness: the log stores only feature/verb identifiers
  and mapping metadata already resident in `view-core`, never anything
  read from or written to a buffer.
- **The paint loop never awaits RPC. The RPC reader thread never blocks.**
  Binds hardest here: the log push happens inside `update()`'s existing
  synchronous `Msg::FeatureInvoke` handling — no RPC round-trip, no new
  channel, a plain `VecDeque::push_back` against an already-owned struct.
- **No unwrap/expect/panic in lib crates.** Ring eviction and the one
  registration-time `MappingClaim` lookup (a linear scan over `DEFAULT_
  MAPS`, 5-6 entries, never a panic on miss — falls back to `had_user_
  mapping: false` with a WHY-only comment on why a miss is a display gap,
  not a defect).
- **Dependency direction: core ← surface ← {native, ai}; only view-engine
  speaks RPC; only view-tui touches the terminal.** `KeyLogState` and its
  ring buffer live in `view-core`, pure; the overlay paint lives behind
  the existing `LayerKind`/`composite_into()` boundary in `view-tui`, no
  new terminal-touching code outside it.
- **Performance is a contract.** `Msg::FeatureInvoke` fires only when a
  registered feature is actually invoked (a handful of times per
  session, not per-keystroke) — the cost is bounded by construction, not
  by a new hot-path budget row; stated explicitly in Task 4 rather than
  assumed.
- Use `task` targets, never raw cargo/git, for build/fmt/lint/test/commit.
  Commit only via `task commit -- -m "<msg>"`.
- Comments are WHY-only; doc comments render for users and carry a WHAT
  summary. No session-narrative markers.
- Non-conventional commit prefixes are parenthesised scopes:
  `feat(introspector):` — never `introspector:`.

## Open design questions — resolved in this plan

1. **Live event source.** Resolved: `Msg::FeatureInvoke`, not a new
   message (see "Correcting `invention-research.md`'s characterization"
   above) — this is the entire architectural finding this plan is built
   on, not a fork with two options.
2. **"Whose it was, what it displaced."** Resolved: resolved once, at
   push time, from `MappingClaim.had_user_mapping` (already computed at
   registration and stored on `Model` via `claimed_keys()`), not
   re-derived per-render. A `true` value renders as "user mapping
   displaced"; `false` renders as "no prior mapping." The introspector
   never re-queries nvim for the live mapping table — `MappingClaim` is
   already the authoritative record of what view itself claimed and
   whether it won a conflict, computed once at startup, which is exactly
   the question "what did it displace" is asking.
3. **Ring capacity.** Resolved: 200 entries, matching `ToastHistory::
   DEFAULT_CAPACITY` exactly — feature invocations are lower-frequency
   than toasts, so 200 comfortably covers a long session's worth of
   browsable history without a bespoke constant.
4. **Command grammar.** Resolved: `:View keys log`, not the charter's
   shorthand `:View keys` — `mappings.rs`'s `is_spellable`/
   `invocations()`/`render_usage()` already enforce a closed two-word
   `{feature} {verb}` grammar for every native command; inventing a
   one-word exception for this feature alone would be the kind of small
   drift the platform-engineering framing this codebase is built under
   exists to prevent. `feature: "keys"`, `verb: "log"`.

## As-built interfaces this plan builds on

```rust
// crates/view-core/src/msg.rs:92-95 — the live per-dispatch event this
// plan's entire design rests on; already fires today, unmodified by this
// plan.
Msg::FeatureInvoke { feature: String, verb: String }

// crates/view-core/src/native/mappings.rs — the static registration
// table this plan cross-references at push time, never re-scans at
// render time.
pub struct MappingSpec { pub feature: &'static str, pub lhs: &'static str, pub verb: &'static str }
pub struct MappingClaim { pub feature: String, pub lhs: String, pub had_user_mapping: bool }
pub static DEFAULT_MAPS: [MappingSpec; 5] = [ /* picker x3, tree, notifications */ ];

// crates/view-core/src/model.rs:75,166,176 — already-stored claim data,
// read (never mutated) by this plan.
impl Model {
    pub fn claimed_keys(&self) -> &[MappingClaim];
}

// crates/view/src/vlog.rs:121 — confirms Msg::FeatureInvoke is already a
// live, matched event elsewhere in the tree; this plan's push sits
// alongside this existing log line, not in place of it.
// "invoke feature={feature} verb={verb}"

// crates/view-core/src/native/toast.rs:81-120 — the ring-buffer shape
// this plan's KeyLogState copies verbatim (capacity, VecDeque, push
// evicts oldest, entries() newest-first).
pub struct ToastHistory { capacity: usize, entries: VecDeque<MessageEntry> }

// crates/view-core/src/native/palette.rs:109-122 — the "snapshot the
// ring at overlay-open time" precedent this plan's KeyLogOverlayState
// copies, avoiding a live-mutation race between the recorder and a user
// scrolling the open overlay.
impl MessageHistoryState {
    pub fn snapshot(history: &ToastHistory) -> Self;
}

// crates/view-core/src/native/registry.rs — the closed FeatureDesc table
// this plan adds a sixth entry to.
pub struct FeatureDesc {
    pub id: &'static str,
    pub supersedes: Option<&'static str>,
    pub off_switch: &'static str,
    pub entry_keys: bool,
}
```

## File structure (new/changed this phase)

```
crates/view-core/src/native/keys.rs       NEW  KeyLogEntry, KeyLogState,
                                                KeyLogOverlayState
crates/view-core/src/native/mod.rs        CHANGED  + pub mod keys;
crates/view-core/src/native/registry.rs   CHANGED  + FeatureDesc { id: "keys", ... }
crates/view-core/src/native/mappings.rs   CHANGED  + DEFAULT_MAPS entry
crates/view-core/src/model.rs             CHANGED  + Model::key_log: keys::KeyLogState,
                                                    + OverlayKind::Keys(KeyLogOverlayState)
crates/view-core/src/update.rs            CHANGED  Msg::FeatureInvoke arm pushes to key_log
crates/view-surface/src/lib.rs            CHANGED  + LayerKind::Keys(KeyLogView)
crates/view-tui/src/paint.rs              CHANGED  + composite_into() arm for LayerKind::Keys
```

### Task 1: `KeyLogState` — the bounded live-invocation ring buffer

**Files:** Create `crates/view-core/src/native/keys.rs`; edit `mod.rs`.

**Rule bound:** paint loop never awaits RPC (push is a synchronous
in-process append, no RPC involved).

**Design:** `KeyLogEntry { feature: String, verb: String, had_user_
mapping: bool, at: std::time::Instant }`. `KeyLogState { capacity: usize,
entries: VecDeque<KeyLogEntry> }`, `new()`/`with_capacity(capacity: usize)`
(clamps to minimum 1, mirroring `ToastHistory::with_capacity`), `push(&mut
self, entry: KeyLogEntry)` (evicts oldest at capacity), `entries(&self) ->
impl Iterator<Item = &KeyLogEntry>` (newest-first). This matches
`ToastHistory`'s ring-buffer *shape* exactly (capacity/`VecDeque`/evict-
oldest/newest-first iteration) but not its ownership signature: `Toast
History::push(&mut self, e: &MessageEntry)` (`toast.rs:105`) borrows its
entry and clones internally, where `KeyLogState::push` takes an owned
`KeyLogEntry` — a deliberate difference, not an oversight, since each
`KeyLogEntry` is constructed fresh at the `Msg::FeatureInvoke` call site
with no pre-existing owner to borrow from. So any future shared
"ring buffer overlay" abstraction has two shape-identical, ownership-
differing precedents to generalize from, not two identical ones.

**Interfaces:**

```rust
pub struct KeyLogEntry {
    pub feature: String,
    pub verb: String,
    pub had_user_mapping: bool,
    pub at: std::time::Instant,
}

pub struct KeyLogState { /* capacity, entries */ }

impl KeyLogState {
    pub fn new() -> Self;
    pub fn with_capacity(capacity: usize) -> Self;
    pub fn push(&mut self, entry: KeyLogEntry);
    pub fn entries(&self) -> impl Iterator<Item = &KeyLogEntry>;
}
```

**Falsifiable check:** unit test pushes 250 entries at the default
capacity (200) and asserts `entries().count() == 200`, with the oldest
surviving entry being invocation #51 — the exact FIFO-eviction contract
`ToastHistory` is already tested against, applied to the new type.

- [ ] **Step 1:** `KeyLogEntry`/`KeyLogState`, eviction, `entries()`.
- [ ] **Step 2:** Eviction-order unit test.
- [ ] **Step 3:** `task ci`. Commit: `feat(introspector): bounded live
  key-invocation ring buffer`.

### Task 2: Wire `Msg::FeatureInvoke` into the log

**Files:** `crates/view-core/src/model.rs`, `crates/view-core/src/
update.rs`.

**Rule bound:** paint loop never awaits RPC; performance is a contract
(push cost is bounded by invocation frequency, stated in Task 4).

**Design:** `Model` gains `key_log: keys::KeyLogState`, constructed at
startup. `update.rs`'s existing `Msg::FeatureInvoke { feature, verb } =>`
arm (already matched, confirmed live at 30+ sites across the file's test
coverage) gains one new line pushing a `KeyLogEntry` built from
`{feature, verb}`, `std::time::Instant::now()`, and `had_user_mapping`
resolved via a single linear scan of `model.claimed_keys()` for a
`MappingClaim` whose `feature`/`lhs` matches — `lhs` is not carried on
`Msg::FeatureInvoke` itself, so the match key is `feature` alone (the
first matching claim for that feature id; `DEFAULT_MAPS` has at most one
claim per feature today, so this is unambiguous, and a doc comment states
the assumption explicitly for a future feature with multiple bound
keys). A miss (no matching claim — a `:View` command invoked with no
corresponding default key) resolves `had_user_mapping: false` rather than
skipping the push, since a command-only invocation is still a real,
loggable event.

**Interfaces:** (additive — the existing `Msg::FeatureInvoke` arm in
`update.rs`, no new signature.)

**Falsifiable check:** integration test dispatches a real
`Msg::FeatureInvoke { feature: "picker", verb: "open" }` through `update()`
and asserts `model.key_log.entries()` gained exactly one entry with the
expected feature/verb, and `had_user_mapping` matching whatever
`DEFAULT_MAPS`' picker-open entry's registered `had_user_mapping` claim
was at startup — proving the cross-reference is live data, not a
hardcoded `false`.

- [ ] **Step 1:** `Model::key_log` field, startup construction.
- [ ] **Step 2:** Push logic inside the existing `Msg::FeatureInvoke` arm,
  `had_user_mapping` resolved from `claimed_keys()`.
- [ ] **Step 3:** Integration test proving live cross-reference.
- [ ] **Step 4:** `task ci`. Commit: `feat(introspector): every
  FeatureInvoke is logged with its registration-time claim data`.

### Task 3: `:View keys log` overlay

**Files:** `crates/view-core/src/model.rs` (`OverlayKind::Keys`),
`crates/view-core/src/native/keys.rs` (`KeyLogOverlayState`),
`crates/view-surface/src/lib.rs` (`LayerKind::Keys`),
`crates/view-tui/src/paint.rs` (`composite_into()` arm).

**Rule bound:** dependency direction (only `view-tui` paints); only
`view-tui` touches the terminal.

**Design:** `KeyLogOverlayState { entries: Vec<KeyLogEntry>, selected:
Option<u16> }`, built via `KeyLogOverlayState::snapshot(log: &KeyLogState)`
— copying `palette.rs`'s `MessageHistoryState::snapshot` pattern exactly,
so the overlay's contents are frozen at open time and immune to the ring
continuing to evict/append while a user is reading it. Rendered rows use
the ordinary `Span`/`StyleRole` styled-text pipeline (unlike image/DVR
scrub, this overlay is plain text — feature name, verb, a "user mapping
displaced" or "no prior mapping" indicator, and a relative timestamp —
nothing needs a new styling primitive). `LayerKind::Keys(KeyLogView {
rows: Vec<Vec<Span>> })`, one new `composite_into()` match arm following
the existing `Picker | Tree | Statusline | Prompt | Palette` combined-
fallthrough shape (`paint.rs:485`ish) since a text-rows overlay needs no
special painting beyond what that shared arm already does.

**Interfaces:**

```rust
pub struct KeyLogOverlayState {
    entries: Vec<KeyLogEntry>,
    selected: Option<u16>,
}

impl KeyLogOverlayState {
    pub fn snapshot(log: &KeyLogState) -> Self;
}
```

**Falsifiable check:** open the overlay via `:View keys log`, push three
new invocations to the live `KeyLogState` while the overlay stays open,
assert the overlay's rendered content is unchanged (frozen snapshot,
race-proof) — then close and reopen, assert the new invocations now
appear.

- [ ] **Step 1:** `KeyLogOverlayState`, snapshot-at-open.
- [ ] **Step 2:** `LayerKind::Keys`, `composite_into()` arm, row
  rendering (feature/verb/displaced-indicator/timestamp).
- [ ] **Step 3:** Freeze-on-open falsifiable test.
- [ ] **Step 4:** `task ci`. Commit: `feat(introspector): live key log
  overlay, snapshotted at open`.

### Task 4: Registry entry, default key, doc/usage coverage

**Files:** `crates/view-core/src/native/registry.rs`,
`crates/view-core/src/native/mappings.rs`.

**Rule bound:** consistency across consumer-facing surfaces (the
introspector must appear in every listing every other native feature
already appears in — `render_usage()`, doctor's future row, the
`[native]` off-switch table — with zero special-casing).

**Design:** `FeatureDesc { id: "keys", supersedes: None, off_switch:
"native.keys = false", entry_keys: true }`, appended to `registry.rs`'s
`FEATURES` array as its 6th entry. `DEFAULT_MAPS` gains `MappingSpec {
feature: "keys", lhs: "<leader>fk", verb: "log" }`, in the existing
`<leader>f*` namespace picker/tree/notifications already occupy (`f` for
"find/feature," matching the established mnemonic). Performance note
made explicit here rather than assumed: `Msg::FeatureInvoke` fires only
on actual feature invocation (a handful of times per session at most),
so this task adds no bench row — the cost is bounded by construction, not
by measurement, and stating that plainly is the falsifiable claim rather
than a fabricated "always under budget" bench assertion for an event
that is not on the hot path.

**Interfaces:** (additive entries in existing static arrays.)

**Falsifiable check:** `render_usage()`'s existing test coverage (already
asserts every `FEATURES` entry with `entry_keys: true` has a matching
`DEFAULT_MAPS` row, per `mappings.rs`'s own doc comment on `entry_keys`)
passes unmodified against the new 6th entry — proving the introspector's
own registration satisfies the same drift-check every other feature must
already satisfy, with no exception carved out for it.

- [ ] **Step 1:** Registry entry.
- [ ] **Step 2:** `DEFAULT_MAPS` entry, `<leader>fk`.
- [ ] **Step 3:** Confirm `render_usage()`'s existing cross-check test
  passes against the new entries without modification.
- [ ] **Step 4:** `task ci`. Commit: `feat(introspector): registers as
  the sixth native feature, <leader>fk opens the log`.

## P5.5-Introspector Exit Checklist

- [ ] `task ci` green (fmt-check, lint, audit, style, loc, test).
- [ ] Task 1-4's falsifiable checks all pass, captured as evidence.
- [ ] `render_usage()`'s existing feature/key cross-check test passes
      against the new 6th registry entry with no special-casing.
- [ ] Spec-amendment check: spec:618 already correctly describes this
      capability's scope ("which mapping fired, whose it was, what it
      displaced"); no spec edit is owed by this plan.
- [ ] No "post-v0.1" language anywhere in this plan's shipped code,
      comments, or commit messages (v0.1 CORE framing, 2026-08-05 ruling).
- [ ] `.claude/known-bugs.md` drained, or every remaining item carrying
      explicit user approval.
- [ ] Dogfood note appended to `.claude/dogfood-journal.md` — the log
      actually used to answer a real "what just fired" question during
      daily editing.
- [ ] `.claude/plans/INDEX.md` gains the P5.5-introspector row (this
      draft's HTML-comment header above) when the plan is moved under
      `.claude/plans/`.
