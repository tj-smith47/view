# Prompt for the next session — P4 plan review and elaboration

Paste the block below as the first message of a fresh session in
`/opt/repos/view`. It is written to be self-contained: it names the files
to read rather than assuming any carried context.

---

You are reviewing and elaborating the P4 implementation plan for `view`.
The plan is a DRAFT authored at P3 exit and has NOT been approved for
execution. Your job is planning-protocol step 5 — the fresh-context
adversarial review — followed by whatever elaboration the review shows is
missing. Do not dispatch any implementation task until both are done.

**Read first, in this order:**

1. `.claude/CLAUDE.md` and `.claude/HANDOFF.md` — working mode, hard rules,
   and the judgment no other file records.
2. `.claude/specs/2026-07-17-view-design.md` — the spec of record. On any
   conflict with a plan, the spec wins and the plan gets fixed. Sections
   §9, §5.1, §5.5, §5.6, §7, §11, §13.3 are P4's anchors.
3. `.claude/plans/2026-07-18-p3-p6-charters.md` — the P4 charter AND the
   binding "Planning protocol" section. The protocol governs you, not just
   the plan's author.
4. `.claude/plans/2026-07-18-p3-oracle-bench-gates.md` — the P3 plan. This
   is the quality, granularity and format bar to match: bite-sized TDD
   tasks, a falsifiable check stated before the steps, complete code in
   steps, no placeholders, consumer call-sites with a named runner-up.
5. `.claude/plans/2026-07-26-p4-native-features.md` — the draft under
   review.

**The review, per protocol step 5.** Check every task against every repo
hard rule *by name*, not only against the spec. The rules that bind
hardest in P4:

- nvim owns all buffer text; mutation only through `Effect::Rpc`. The
  file tree's file ops, the picker's preview, and every palette action are
  the places a plan can bless a violation without looking like it does.
- The paint loop never awaits RPC; the RPC reader thread never blocks.
  Every worker in this plan (matcher, scanner, grep, clipboard, git,
  toast timer) is a chance to get this wrong.
- Dependency direction core ← surface ← {native, ai}; only view-engine
  speaks RPC; only view-tui touches the terminal; view-core is pure.
- No unwrap/expect/panic in lib crates. Files under 1000 production LOC.
- Comments are WHY-only; doc comments carry a WHAT summary.

Findings are FIXED IN THE PLAN, then the changed sections are re-reviewed.
Findings are never carried as notes.

**Verify these specific things, which the draft's author flagged as the
places it is most likely wrong:**

- **The charter correction in "As-built interfaces".** The draft asserts
  modal prompts reply through `Effect::Rpc(RpcCall::Input)` (nvim blocked
  in its own input loop) while the clipboard provider replies through
  `Msg::EngineRequest` + `Effect::Reply` (a real `rpcrequest`) — a
  correction to the charter's text, derived from `MessageEntry::is_prompt`'s
  doc comment in `crates/view-core/src/model.rs`. It is a DERIVATION, not
  a capture. T8 step 1 requires a live capture to confirm it. If you can
  capture it cheaply, do so now; the whole of T8 rests on it.
- **T3 changes `Model::focus` from a public field to a method.** That is a
  breaking change to P2's as-built surface. Check every consequence.
- **The three new `view-native` dependencies** (`nucleo`, `ignore`,
  `grep-searcher`) and the `serde`/`toml` widening in T1. Each needs an
  audit-matrix row. Is the T1 widening actually justified, or should the
  `[native]` schema live in the `view` bin?
- **Coverage walk completeness.** Re-derive it from §9, §5.1, §5.5, §5.6,
  §7 and the charter yourself; do not trust the draft's table. A plan
  whose tasks are all well-formed can still silently drop a deliverable —
  that is the failure class step 0 exists to catch.

**Elaboration after the review.** The draft's tasks state their falsifiable
check and their step sequence, but most steps do not yet carry complete
code the way P1/P3's do. Bring them to that bar. Where a step says
"implement X", it should show X. Where a wire fact is needed, the plan must
direct a live capture and must not state the fact itself — a prose citation
of a capture is never sufficient.

**Two things are deliberately absent and must stay absent** until captured
live by the implementer (protocol step 1): the exact `ext_messages` traffic
for a `confirm` prompt, and the `g:clipboard` provider contract. If you
find yourself writing either from memory, stop.

**Standing rules that apply to you:**

- Commit only via `task commit -- -m "<msg>"`.
- Use `task` targets, never raw cargo.
- Scratch files go in `~/.claude/tmp/`, never `/tmp/`.
- No concession, metric degradation, or budget amendment is accepted
  without an adversarial review by a Fable 5 subagent, which also reports
  any safety or security concerns it finds along the way.
- Never decline, drop, defer, or de-scope any feature the spec or charter
  names. An open question is a trigger to research and pitch, never
  permission to cut.
