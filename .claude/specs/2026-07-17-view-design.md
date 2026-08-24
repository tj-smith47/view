# view — Design Spec

Date: 2026-07-17
Status: awaiting user approval
Scope: product definition and architecture for v0.1 plus the strangler runway.
Implementation plans are derived from this document; they do not override it.

---

## 1. Product definition

**view is the first fully native AI-first TUI terminal editor for agentic
development.** It is Neovim (painless migration), written in Rust (objectively
faster), with a modern, cohesive — but still configurable — UI.

That is the differentiator. Everything below is how it is delivered: the three
contracts are the benefits that follow from it, the performance mandate (§3) is
a quality bar rather than the reason the product exists, and the moat (§13.2)
is what makes the strangler roadmap (§15) possible. The AI architecture that
the positioning rests on is §10, not an appendix to the editor.

Structurally, **view** is a terminal-first modal editor whose engine is an
embedded, pinned Neovim. It makes three contracts to its users:

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
| view input path: key event read from the terminal → RPC bytes written | **dev-linux p99 ≤ 100 µs; dev-macos UNMEASURED — no budget.** **The bar amendment of 2026-07-26 is withdrawn 2026-07-27.** That amendment moved the bar to 232 µs, which was the then-recorded 154.749 µs × `ABSOLUTE_HEADROOM` 1.5 — a bound computed from the very measurement it bounds, and therefore one no measurement can fail. It was a self-referential bar wearing a budget's clothes, and it is retracted. The original `p99 ≤ 100 µs` stands again, because a budget stated before anything was measured is a promise and a budget derived from a measurement is a restatement. The gap to it closed on 2026-08-01: **dev-linux measures 88.965 µs p99, inside the 100 µs bar** (117.739 on 2026-07-27, 154.749 before the writer hop was collapsed; the `[[shortfall]]` in `crates/view-bench/budgets.toml` that carried the gap is spent and deleted). How much room is left is now known rather than guessed: the one cross-thread hop this path is architecturally required to make — input thread → runtime loop, because the model decides whether a key is view's own or nvim's — costs **80.0 µs p99 by itself** on this class, measured without the editor in the way by `cargo bench -p view-core --bench input_handoff` at a 10 ms idle gap (the gap steady typing actually presents). So 100 µs leaves roughly 20 µs for the model dispatch, the msgpack encode, the writability poll and the write. Closing it means removing that hop, not tuning around it; see the `echo` row for what that would take. **The boundary amendment of 2026-07-26 stands** — only the number is withdrawn. *The boundary was wrong*: it opened at the harness writing a byte to the pty master, so the largest single segment of the interval was the OS pty transport (dev-linux 78.6 µs p50 of a 233 µs whole), which no view code schedules. The boundary now opens at view's own key-read tap; the excluded prologue is still reported as the `pty->key-read` segment, as evidence rather than as a bar. **Caveat on the exclusion:** the key-read tap fires *after* `crossterm::event::read()` returns a parsed event, so crossterm's read and parse — view-controllable dependency code — sit inside the excluded segment and their share has never been isolated. A tap pair splitting raw-byte-readable from event-parsed would bound it. **Correction to an earlier claim in this row:** it previously asserted 100 µs was "physically inconsistent with the architecture". That is an overstatement and is withdrawn. At the *new* boundary the gated interval measures ~87 µs p50 on dev-linux and ~75 µs p50 on bare-metal mbp — both under 100 µs. What is actually established is narrower, and 2026-07-27 sharpened it further: the p99 target is missed on this shared virtualized host by 18%, not by a margin that indicts the architecture, and 80 of the 117.7 µs is the one hop the architecture requires. "Unreachable" was also asserted here previously and is likewise withdrawn — it was never measured. A quiet bare-metal p99 at the new boundary has never been measured either, and until it is, no claim about the architecture's floor on other hardware is supported. What remains inside the boundary was **two** thread transitions — input thread → runtime loop → RPC writer thread — of which only the first is mandated by "the paint loop never awaits RPC". The second is now gone: the runtime loop writes the RPC bytes itself when the pipe has said it can accept them and nothing is queued ahead (`crates/view-engine/src/outbox.rs`), which preserves both the non-blocking and the ordering guarantees the writer thread was there to provide. That took `rpc-handoff->rpc-written` from 42.5 to 10.5 µs p50, and the whole row from 154.7 to 117.7 µs p99. **The previously unreconciled inconsistency is resolved.** This row and line 89 disagreed by an order of magnitude on what a cross-thread wake costs (1.2–4.4 µs against 44.7 + 32.8 µs), and ~70 µs of the interval carried an unverified attribution as a result. Direct measurement of the production channel primitive shows both figures are right and neither is a wake's "cost": **a hop's price is set by how deeply the receiving core has parked, not by the channel.** The same `SyncSender` handoff measures 7.5 µs p50 after 50 µs of idle and 36.0 µs p50 / 80.0 µs p99 after 10 ms — a 5× spread over idle depth alone. Steady typing parks the receiver for a keystroke interval, so the editor always pays the deep end, while a microbenchmark that sends in a tight loop never lets the receiver park and measures the shallow end. Current decomposition (2026-07-27, 3000 samples, chain resolved on all of them, 0 ambiguous wakes, 0 repeated round tags): `key-decoded->loop-wake` 49.1 µs p50 / 91.0 p99 (the mandated hop), `loop-wake->rpc-handoff` 11.4 / 24.9 (view CPU: `update()` plus the msgpack encode), `rpc-handoff->rpc-written` 10.5 / 22.7 (the poll and the inline write). Evidence: `.claude/measurements/2026-07-26-the-input-gap-is-thread-hops-not-cpu.md` and `.claude/measurements/2026-07-27-collapsing-the-writer-hop.md`. Recorded floor: **dev-linux 80.682 µs (machine-recorded 2026-08-01, inside the 100 µs bar; the shortfall that carried the earlier 117.739 is spent and deleted).** The dev-macos floor previously stated here as "230.0 µs" **was never measured** — it was a hand-derived value, the only round number in a baseline file whose every recorded metric carries six or more decimals, and the 350 µs budget that rested on it is withdrawn along with it. dev-macos gates nothing on this row until a real capture exists. Evidence: `.claude/measurements/2026-07-26-input-path-boundary-and-tap-cost.md` **Correction 2026-08-10:** the hop this row measured at 80.0 µs p99 and prescribed removing is gone -- `219f532` (input-thread/runtime-loop unification) collapsed it by decoding keys inline in the runtime's own poll loop instead of hopping them from a dedicated input thread, taking `key-decoded->loop-wake` p50 52.4 -> 13.9 µs and p99 95.9 -> 28.4 µs on dev-linux. The "Recorded floor" above (dev-linux 80.682 µs, 2026-08-01) predates that change and is superseded. The tree's current baseline is `key_to_rpc_p99_us = 59.332` in `crates/view-bench/baselines/dev-linux.toml` (campaign-logged median, hand-edited at `9547077`; six-of-six kept legs spanning 40.2-60.8 µs, attributed to a host-side level shift rather than a view regression), still inside the 100 µs bar at 1.15x headroom (59.332 x 1.15 = 68.232). **Correction 2026-08-21:** that 59.332 is superseded and is no longer the tree's value. The current recorded baseline is `key_to_rpc_p99_us = 70.077` (`crates/view-bench/baselines/dev-linux.toml`, machine-recorded at `b898427`), still inside the bar at this class's published 1.15 spread (70.077 x 1.15 = 80.589). The cell was **deleted before that recording rather than ratcheted onto**, which is why the number moved upward at all: the 59.332 bar was taken on 2026-08-18, before the sample anchor was redefined at `60fa2e7`, so it held a quantity the harness no longer produces, and the min-ratchet would have kept the stale number and read the honest one as a regression. The direction is the one the redefinition predicts — walking past a stray writer-thread tap can only close the interval later — so the rise is a change of quantity, not of view. The same window recorded this cell at 66.427 on controlled-linux, the class the budget actually attests on. This row's prose above (the hop-cost decomposition, "closing it means removing that hop") is retained as the historical record of the measurement that motivated the unification, not as the current architecture | pty harness, §13.4 |
| view output path: redraw event parsed → terminal write | p99 ≤ 1 ms | pty harness |
| view input path, AI session active: key event read from the terminal → RPC bytes written (`ai_session_active`) | **p99 ≤ 100 µs, controlled-linux — the same bar the session-absent input-path row above carries, and deliberately not a number computed from this row's own first recording.** This row exists to hold one claim to a measurement instead of an assertion: with an agent session live on screen and a turn streaming into the panel, the editor's input path must not degrade. The honest way to state that is that the bar does not move — the gated interval is the same `key_to_rpc_p99_us` the input-path row measures, so the AI-present number is held to the AI-absent promise. A bar derived from the first recording instead would be the defect that withdrew that row's own 232 µs bar. First recorded on controlled-linux 2026-08-21 at **70.865 µs**, inside the bar, in a quiet window with this host's peer session stopped. The regression ratchet is the tighter of the two today (70.865 × the 1.5 default absolute headroom = 106.3 µs; no measured spread is published for this class yet), which is the intended relationship the first-paint row below describes rather than a redundancy | pty harness, §13.4, with the fixture agent streaming |
| view output path, agent response streaming: redraw event parsed → terminal write (`ai_streaming`) | **p99 ≤ 1 ms, controlled-linux — the same bar the session-absent output-path row above carries**, for the reason the AI input-path row states: the gated interval is the identical `p99_ms` that row measures, and the claim being held is that a turn streaming into the agent panel does not degrade the paint path, which is exactly the statement that the bar does not move. First recorded on controlled-linux 2026-08-21 at **0.4306 ms**, inside the bar, in the same quiet window; the ratchet is again the tighter instrument today (0.4306 × 1.5 = 0.6459 ms). What this row puts under measurement is the panel's own render: across the recording window's own draws, `draw-start→flush-start` reads **17.2–20.6 µs p50 with no session on screen** (three `output_path` draws) against **153.8–231.5 µs p50 while a turn streams** (three `ai_streaming` draws) — non-overlapping groups an order of magnitude apart, stated as ranges because the streaming spread is wide and its low draw flatters the comparison. That segment is evidence, bounded by nothing; the gated statistic is the whole boundary interval. Every frame the panel explains is attributed through the tap channel rather than assumed — 43, 45 and 70 of 3300 keystrokes across those three draws, with no paint left unexplained on any of them. Evidence: `.claude/measurements/2026-08-21-ai-panel-render-segment.md` | pty harness, paired with the fixture agent streaming, every panel paint attributed through the tap channel |
| Keypress → cell change end-to-end, steady typing | p99 ≤ 8 ms every class; ratio ≤ 1.10× the paired bare-nvim run — **target unmet on every measured class, cause open. This amendment was ratified by the user 2026-08-03 together with the full shortfall ledger (all eleven entries): the ≤ 1.10 target stands as the contract, the gate holds at recorded × measured headroom, and the pinned P4 closure path for the echo ratio entries is input-thread/runtime-loop unification first, incremental rendering second.** All classes gate ratio_p50 until the cause is attributed — **not** "measured-or-better", which this row previously claimed and which is false: the gate enforces `recorded × headroom`. **The headroom was re-derived from measurement on 2026-07-27 and is no longer a constant.** It was `RATIO_HEADROOM = 1.25`, which let `echo.minimal` degrade 25% silently — to 38% above the 1.10 target — without failing. Eight replicates on dev-linux spanning host loads 0.44 to 8.53 put `ratio_p50`'s half-width at **1.70%** and its worst excursion above the recorded value at **2.14%**, so 1.25 was covering a spread twelve times smaller than itself. dev-linux now declares `ratio_p50 = 1.06` in its own `[headroom]` table, gating at **1.242**: it clears the pre-registered 2×-half-width threshold (3.4%) and the worst observation (2.8× margin) while cutting the silent-degradation window from 25% to 6%. 1.25 remains the default for every metric and class whose spread has not been characterised, because guessing tighter than the evidence is how a gate starts failing on weather. The `[[shortfall]]` ledger shares that ceiling rather than pinning the metric at its accepted value: pinning was tried and failed on the first re-run over a 0.35% difference (1.176 measured against 1.172 accepted), because an accepted value is one sample of a noisy statistic and not a constant. Measured-or-better describes only the `--record` ratchet, which runs when someone deliberately re-records, not the gate. That headroom is also ~5× the apparatus's own resolvable effect (the resolution campaign measured ratio_p50 half-width 2.66%), so it was never derived from the measured floor and should be re-derived. The cause is now partly attributed (see the retraction history and the `echo_control` row below), so "until the cause is attributed" names measured work rather than a label. History: the thread-hop explanation adopted at P3 T12 (hops ~100 µs ⇒ floor ≈ 1.19, 1.10 reachable only on bare metal) was **falsified by direct measurement**: the bare-metal M1 Max, whose cross-thread wakes are 3-4× cheaper (1.2 µs vs 4.4 µs), measures a *worse* ratio_p50 than the virtualized host, not a better one, and hops are ~500× too small to account for the view-vs-nvim gap. Do not cite the 1.19 hop floor. Quiet-host figures (mbp load 1.31, Linux calibration 0.9553): mbp 1.337 against Linux 1.199, with recorded baselines 1.3437 and 1.3538 — both figures predate the writer-hop fix, and the dev-macos side has not been re-measured since it (task 21). An earlier mbp reading of 1.576 was taken at host load ~1.8-2.0 and is load-inflated by ~17% against both the recorded baseline and a quiet re-measurement; it should not be quoted, though the inversion it was cited for does persist at quiet load. The presumption that the residual lives in the RPC/UI-protocol process boundary is **falsified**. nvim's own out-of-process UI (`nvim --server <sock> --remote-ui`, an RPC client containing none of this project's code) was measured against bare nvim by the identical paired protocol as the `echo_control` matrix row: it costs **1.015 on minimal and 1.013 on heavy** (dev-linux recorded), against view's 1.354 and 1.244 at the time. **Acting on that attribution then moved the number for the first time in this project's history:** collapsing the runtime-loop-to-writer-thread hop into a guarded inline write took `echo.minimal` to **1.1719** and `echo.heavy` to **1.1838**, with `ratio_p99` reaching **1.0917** and **1.0103** — the heavy fixture's tail is now within 1% of bare nvim. Evidence is a back-to-back A/B whose bare-nvim arm did not move between legs (0.544 → 0.543 ms p50), which is what rules out load compression explaining the change: `.claude/measurements/2026-07-27-collapsing-the-writer-hop.md`. Speaking the protocol out of process costs ~2%; view costs ~22-35%. The confound was checked: `echo/minimal` re-run immediately after the control, on the same host state, returned ratio_p50 1.224 while its bare arm moved 24% in absolute terms, so the ratios are comparable and the control's figure is not load compression. The `echo_path` decomposition then places view's own cost inside a round trip most of which is not view's at all. **Re-measured 2026-07-27** after the writer hop was collapsed (see the input-path row above), on a quiet host, 3000 samples, chain resolved on every one of them, 0 ambiguous wakes, 0 repeated round tags, 3.5% residual: of a 643.5 µs p50 round trip, **366.2 µs is inside the engine** (`rpc-written->redraw-parsed`), **80.0 µs is the OS pty transport ahead of view's boundary**, and **36.2 µs is the terminal and its parser** (`term-written->glyph-seen`). view's own share is **139 µs p50 — 71 on the input path and 68 on the paint path**. The single largest item in it is `key-decoded->loop-wake` at 49.1 µs p50 / 91.0 p99: the mandated input-thread-to-runtime-loop hop, paid at deep-idle depth because a human pauses between keystrokes. Nothing else on either path exceeds 21 µs p50, so there is no hot spot left to delete — the remaining levers are architectural, not tuning ones. **The paint side's 68 µs is now attributed too, and not to instruction count:** the identical frame costs 2.94 µs measured back-to-back and **21.27 µs measured with a 10 ms keystroke gap before it**, a 7.2× difference from idle alone, so the cost is cache and TLB residency and is proportional to memory touched per frame rather than to work done. `view_surface::render` rebuilds a full-screen `Surface` every frame even for a one-cell change, which is a deliberate property of the Elm-style runtime; incremental rendering would cut this and is a design decision, not an optimization. **Consequence for every number in this document sourced from a criterion micro-bench:** those measure the hot state, which the editor never occupies. They are valid as relative regression instruments and understate absolute per-keystroke cost by roughly the factor above. Evidence: `.claude/measurements/2026-07-27-the-paint-path-is-cold-cache-not-instructions.md`. The heavy fixture's decomposition tripped its own repeated-chain-tag guard (2 of 3000) and its per-stage numbers are therefore not quoted. The taps rows put the largest single input-path segment at the pty transport (`pty->key-read`, 78.6 µs p50 / 139.4 µs p99 on dev-linux), which is ahead of that boundary; it is measured, reported every run, and is now explicitly outside the input-path bar (see the row above), but it is paid identically by bare nvim in the same paired run and so cannot explain a *ratio* — the residual remains open. **Adjudication ratified 2026-08-09:** both pinned P4 levers landed and were measured paired -- unification collapsed `key-decoded->loop-wake` 52.4 -> 13.9 µs p50 while moving this ratio only 1.215 -> 1.173 / 1.234 -> 1.193, and incremental rendering moved echo.minimal within noise while putting echo.heavy inside its budget (that shortfall entry is retired) -- leaving echo.minimal at 1.1719 with the residual attributed to `rpc-written->redraw-parsed` (~357 µs p50 of a ~610 µs round trip), which no view-side input or paint lever addresses. The user accepted the residual: the ≤ 1.10 target remains the contract and the gate holds at recorded × headroom, but the pinned closure path is now the speculative-echo invention, which removes the engine round trip from *perceived* echo instead of shrinking view's share of the measured one. When it lands, the speculated path gates on its own metric; this row continues to measure the honest unspeculated round trip | pty harness, paired |
| Speculative echo: keypress → predicted glyph visible, RTT hidden (`speculated_ratio_p50`, `speculated_paint_p99_ms`) | **ratio_p50 ≤ 1.0× the paired bare-nvim run, dev-linux.** A bar stated before anything measured it, and the only bar this row can honestly carry: at 1.0 view puts the typed glyph on screen exactly as fast as bare nvim does, and predicting the glyph is worth having only below that. A first recording above 1.0 is a shortfall to write down with its cause, never a bound to relax. The row's absolute tail (`speculated_paint_p99_ms`) deliberately carries no budget: nothing here bounds how long a predicted paint may take, and a bound written from the first recording would be a number computed from the measurement it bounds -- the defect that withdrew the input-path row's 232 µs bar and the first-paint row's cold_ms bar. It is recorded as the seed a controlled class would ratchet from. This row does not relax the echo row above it and cannot be read as improving it: that row measures the round trip, this one measures the paint that happens on view's own tick before the round trip finishes, and the two never share a metric name. | pty harness, paired against bare nvim, every sample attributed through the tap channel (a predicted paint and an authoritative one are the same character in the same cell on screen, so the attribution is the only evidence that tells them apart) |
| Sustained scroll, 100k-line file, tier full | content staleness p99 ≤ 16 ms (input → corresponding scrolled content on screen) | frame log w/ staleness tags |
| First paint: view's own UI shell visible, engine still loading (`shell_visible_ms`) | p99 ≤ 50 ms cold. View-side absolute, deliberately unpaired: view paints this frame *before it has spawned the nvim child at all*, so bare nvim has no counterpart event and any ratio taken here would divide two different things. Boundary: the screen holding `view_surface::SHELL_PLACEHOLDER`, the exact text the product paints, read from the crate that owns the layer rather than copied. Measured dev-linux p99 **4.08 ms** (20 samples, host load 0.23), 12x under budget | pty harness |
| First paint: the opened file's content on screen (`marker_cold_ms`, `marker_ratio_p50`, `marker_ratio_p99`) | **p99 ≤ 30 ms cold; ratio_p50 ≤ 0.30× the paired bare-nvim run.** Stated by the user 2026-07-27, closing the gap this row previously carried: a cold-start bar for the event a user actually waits for (their file, visible) is the number the product claim rests on, and it was never the harness author's to invent. **Why 30 and not a rounder number — the reasoning is the bar's whole justification, and a session that finds only the number will widen it:** the regression ratchet never widens a bound, and it already holds this metric above the budget (today the recorded 25.151 ms × the class's measured headroom 2.0 = **50.3 ms**; at the ruling, the then-recorded 26.507 ms × the 1.5 default = 39.8 ms). Any budget above that ceiling is therefore decorative: the ratchet is always the thing that fires first and the budget never is, and a bar that cannot fail is exactly the defect for which `cold_ms` and `ratio_vs_nvim` were withdrawn from this row (see the amendment below) and the 232 µs bar from the input-path row above. 30 ms sits below the ratchet and above the measurement, so it can break. **Neither bound is to be widened to fit the heavy fixture:** `first_paint.heavy` measures 120.5 ms p99 and ratio_p50 0.460, against bare nvim's 199.8 ms p99 on the same paired run — mostly the 14-plugin init the engine child finishes before the file can be on screen — and it is carried as a `[[shortfall]]` in `crates/view-bench/budgets.toml` that may hold or improve but never widen. The ratio bound binds where the absolute cannot: a class whose cold start is slow for reasons outside view still has to show view reaching the file in under a third of bare nvim's time. On the minimal fixture the ratchet is the tighter of the two (recorded 0.135 × `RATIO_HEADROOM` 1.25 = 0.169), which is the intended relationship and not a redundancy — the budget states where the number must be, the ratchet states that it may not get worse than where it is, and whichever is tighter is the one that fires. Measured dev-linux p99 24.9 ms against bare nvim's 129.3 ms, ratio 0.193; recorded baseline 26.507 ms and 0.135 **Correction 2026-08-10:** the 26.507 ms / 0.135 pair is a superseded recording from before the ratchet values in this row's opening sentence were re-derived; it is not the current baseline. The tree's current recorded values, matching the opening sentence's 25.151 ms figure, are `marker_cold_ms = 25.150733` and `marker_ratio_p50 = 0.12916062518448798` (`crates/view-bench/baselines/dev-linux.toml`) | pty harness, paired |
| Picker match: keystroke → first results painted, 100k resident entries | ≤ 16 ms | bench suite |
| Picker scan: 1M-file tree | streaming (results while scanning, never scan-then-show); first page ≤ 100 ms warm-cache | bench suite |
| view-side memory (PSS), 10 buffers, post-workload | ≤ 150 MB | bench suite |
| Remote editing: view's own local footprint once the engine runs remotely | ~~≤ 6 MB PSS, dev-linux — headroom over the recorded 4.962 MB `memory.minimal` local-spawn figure this class inherits from~~: moving the engine off-host must not grow view's own process footprint, since view holds no more buffer state when the engine is remote than when it is local (local footprint must not grow when the engine moves remote). Measured by the `remote_memory` row against the same 10-buffer workload `memory.minimal` uses, `--remote`-armed over the committed stub-ssh double in CI (`view_oracle::remote`) with an opt-in real-SSH leg for local/acceptance runs (`VIEW_REMOTE_TEST_HOST`). **Correction 2026-08-17:** the struck 6 MB bar is withdrawn. It gated dev-linux, a shared class the spec budget gate never actually attests on (`bin/bench.rs` loads the budget table only for a `controlled-` class), so the row was dead by construction, and separately it bounded a settled-process absolute that a within-window headroom sidecar cannot hold fixed against the host's own ambient memory regime: one unchanged binary pair swung `pss_mb` +/-20% across days on shared dev-linux with nothing about view changing. A single window put the paired remote-vs-local ratio at a +1.2% delta — `remote_memory.pss_mb`'s `[headroom]` entry in `baselines/dev-linux.headroom.toml` (recorded 2026-08-16) put `remote_memory/minimal`'s 9-draw median PSS at 3.734 MB against `memory/minimal`'s own paired-window median of 3.6885 MB, both solo, non-co-resident spawns. Whether that delta also holds across the regime itself, not just within one window, is a separate claim tracked in `.claude/plans/2026-08-09-p5_5-remote.md:405-413` (the ±20% across-day host-regime envelope and a +1.23% paired delta landing inside it) — so that ratio, not either absolute, is now the bound: **`remote_local_ratio` (remote-leg absolute / local-leg absolute, `view_bench::scenarios::remote_memory::run_paired`) ≤ 1.10×, every class.** Both absolutes are still measured and recorded every run, for reference, record-only on a shared class the same way a tail statistic already is (`view_harness::baselines::gate_headroom`) and gated on a controlled one. 1.10 is a promise stated before any controlled class has measured this cell, in the same spirit as the input-path row's original 100 µs bar and the echo row's 1.10 ratio bar, not a number computed from a recording | bench suite (`remote_memory` row, pty) |
| Redraw under engine event burst (plugin storm, `:terminal` flood) | UI thread never blocks; coalesced paint stays inside the staleness budget | design invariant + test |
| Engine supervision: time to notice a read-side hang | p99 ≤ 12500 ms, from the engine entering a synchronous Lua loop to the banner naming the wedge being on screen. The number is the heartbeat's own arithmetic plus what reading the result costs, not a chosen target: `HEARTBEAT_WEDGE_THRESHOLD` (10 s) plus one `HEARTBEAT_PROBE_INTERVAL` (2 s) is the 12000 ms `view_oracle::hang::DETECTION_BOUND` computes, because the last probe an engine can answer is the one fired just before it stops serving, and the further 500 ms is that module's `OBSERVATION_SLACK` -- the identical allowance its `detection_deadline()` adds where the oracle asserts this same quantity, needed here for the same reason: the bench covers the probe interval with its samples rather than repeating one offset into it, so the measured p99 is the top of that cover and reaches the bound whenever a sample lands just past a tick, and a bound reached rather than approached fails honest measurements if it allows nothing for the frame that paints the banner and the poll that reads it. A read-side hang is the one failure no redraw traffic can reveal — the connection stays open and view's output is drained — so this row is what stands behind the claim that a misbehaving plugin cannot silently take the editor down. Its paired quantity, how long a replacement takes to paint a swap-recovered buffer (`restart_rehydrate_p99_ms`), is recorded by the same row and bounded by no promise here yet: the first recording seeds the baseline a controlled class would ratchet from (a tail is recorded and not gated on a shared one), and a bound derived from that recording would be one no measurement could fail | bench suite (`supervision` row, pty) |

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

  **Amendment 2026-08-10, attestation split:** the bullet above this one ("no row ships without a gated tail statistic") predates the ruling that budgets attest on controlled classes only; a shared-host leg is a regression tripwire, and its cold absolutes and tails are recorded, not gated. That "Every class" bullet is read narrowly from here forward: it names the statistics every class *reports*, not a promise every class *gates* them -- the very next bullet already carves shared/dev classes out of gating, and this amendment resolves the surface tension between the two by naming it. Recorded-not-gated tails on a shared class, e.g. the dev-macos picker cell recorded at `885871d`, are conformant under the split. Ruling: `.claude/measurements/2026-08-08-measurement-layer-rulings.md`, section 2 ("Single-shot gating cannot resolve <25% regressions on that cell -- RULED 2026-08-09: option (c)"): "This matches the attestation split already in force -- shared classes are regression tripwires; budgets attest on controlled classes."
- `--gate` verifies its own precondition: a null-pair calibration (engine vs
  engine, interleaved) must sit inside a measured floor of 1.0, else the gate
  refuses and names the noise. A gate result is trustworthy by construction,
  never by hoping the host was quiet.

**Amendment 2026-07-26, first-paint split. Ratified by the user 2026-08-03:
the 2026-07-27 budget ruling (marker p99 ≤ 30 ms, ratio_p50 ≤ 0.30×) already
presupposed this split, and the user confirmed the amendment itself when that
was surfaced.** The single first-paint row above was one budget over two
different events, and the harness measured a third. The recorded metrics
`cold_ms` (dev-linux 3.583243, dev-macos 8.60825) and `ratio_vs_nvim`
(0.019123, 0.036566) were taken with an "any cell has ink" boundary, which
`crates/view-bench/src/boundaries.rs` documents in its own source as the wrong
predicate for a paired spawn: view satisfies it with pre-attach chrome while
bare nvim satisfies it with its buffer window. Both keys are withdrawn rather
than carried forward under a new name; the shell quantity is re-measured
against the placeholder text itself, and the content quantity is new. The
re-measured shell p99 (4.08 ms) agrees with the withdrawn `cold_ms` (3.58 ms)
closely enough to show the old number was not grossly contaminated by the
per-spawn socket collision fixed the same day, but it was never the paired
quantity its `ratio_vs_nvim` sibling claimed.

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
- Supervision: an engine death (any exit nvim never announced — a signal, a
  crash that manages a clean exit status, a reader that stopped for its own
  reason) → view keeps the last Surface painted, offers one-key restart, and
  swapfiles make that restart non-destructive; an engine told to exit (`:q`,
  `:cq`) announces its own departure over view's bridge before the channel
  closes, and that announcement — never the exit status, which carries no
  intent at all on Windows — is what ends the session with nvim's own status
  instead of respawning the editor a user had just closed. RPC calls carry
  timeouts; a hung engine (blocked synchronous Lua) yields an honest "engine
  busy" indicator with interrupt/restart affordances — the UI thread never
  blocks.
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
- Remote editing is v0.1 core (§9 invented capabilities, ruled 2026-08-05):
  the engine spawns over SSH while paint and input stay local. Attaching to
  an already-running instance via `:detach`/`:connect` is chartered
  post-v0.1, after remote ships (plans/2026-08-14-post-v01-charters.md C1).

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
| `basic` | ASCII-safe, no capability replies | Correct and complete, plain borders, no probed refinements |

A tier governs what is *probed and assumed* — synchronized output, the kitty
keyboard protocol, border charset, animation. It does not gate color: every
tier emits 24-bit SGR, because the grid nvim hands over is 24-bit and a
palette approximation applied to chrome alone would leave the two halves of
the screen disagreeing. Terminals that cannot render truecolor degrade the
SGR themselves, uniformly, which is the only place the approximation can be
consistent.

Degradation is a first-class tested surface (golden snapshots per tier, §13),
not a fallback apology.

### 7.1 Design language (drafted 2026-08-04; pending user ratification)

The tier table above names "rounded borders" and "cell-eased animations"
without defining either; this section is the concrete visual system P4
builds to, so candidate surfaces are judged against renderings rather than
adjectives. Reference renderings (Dracula, `full` tier) live in
`assets/mockups/`; Dracula is the reference *rendering*, not the theme —
every token below derives from the user's live colorscheme via the §7
bridge, and every derivation is overridable in `view.toml`. Golden
snapshots per tier (§13) become the enforcement once the surfaces exist.

**Elevation.** Three background steps, derived, never hardcoded:

| Token | Derivation | Used by |
|---|---|---|
| `surface.base` | `Normal` bg | the grid |
| `surface.raised` | `NormalFloat` bg when the scheme defines a distinct one, else base with 8% of fg mixed in | every float: palette, picker, prompts, toasts, AI panel |
| `surface.dim` | base with 30% black mixed in | shadows, modal backdrop |

**Shadow.** Every float composites a drop shadow: the backdrop cells in the
float's footprint offset by (+1 row, +2 cols) — clipped exactly to that
offset footprint — keep their glyphs but restyle to `surface.dim` bg with
fg blended 70% toward it. A shadow is a restyling of what lies beneath, not
a black block, and because it is computed inside the view-side z-ordered
compositor it structurally cannot bleed past its region the way `winblend`
pseudo-transparency does. No other transparency exists in the system.

**Modal backdrop.** While a native surface holds `Focus::Native` (palette,
picker, confirm prompt), the grid beneath paints with fg blended 40% toward
bg. Focus becomes legible at a glance instead of inferred from a cursor.
Applies on every tier, since color is not tier-gated.

**Borders and spacing.** Rounded corners `╭ ╮ ╰ ╯` on `full` and `standard`
— corner glyphs are font coverage, not a terminal capability; `basic` falls
back to ASCII `+ - |`. Border fg is `accent` blended 50% toward the
surface's bg (`FloatBorder` when the scheme defines it). A float's title
renders inside the top border run, space-padded, in **`FloatTitle` fg,
bold** — amended from `accent` fg at the P4 exit drain (2026-08-09,
coordinator ruling, reported to the user): the `accent` vocabulary below is
unimplemented tree-wide, `hl-FloatTitle` is nvim's own builtin for exactly
this element, and a switching user's colorscheme already drives it, so
parity with the editor being migrated from beat a role no code resolves.
The fg comes from that group over the surface's own bg, so the top edge
still reads as one continuous run; bold applies on every tier, since on a
terminal with no color an attribute is the only way a title can outrank the
border it sits in. Panels keep
at least one row / two cols of padding inside the border; content never
touches a border cell. Selection bars span the full inner width on
`surface.sel` with a `▌` accent marker in the left padding column.

**Accent roles.** Semantic, derived from highlight groups; components name
roles, never colors:

| Role | Derived from | Dracula renders as |
|---|---|---|
| `accent` | `Function` fg (fallback `Statement`) | purple |
| `match` | `IncSearch`/`Search` fg | pink |
| `info` | `DiagnosticInfo` | cyan |
| `success` | `DiagnosticOk`, git add signs | green |
| `warn` | `DiagnosticWarn` | yellow |
| `error` | `DiagnosticError` | red |
| `surface.sel` | `PmenuSel`/`Visual` bg | `#44475a` |

**Motion** (`full` tier only; `standard` and `basic` paint final frames
directly). Rules first, catalogue second:

1. **State-first.** The model reaches its final state on the first frame;
   motion is presentation-only interpolation toward what is already true.
   Nothing waits on an animation and no animation adds latency to the
   input path.
2. **Interruptible.** Any input during an animation completes it on that
   frame.
3. **Ordinary paints.** Animation frames go through the same paint path
   inside the §3.1 budgets, ticked by the runtime's existing timer effect
   — no dedicated thread, and the paint loop still never awaits RPC.
4. **Two durations.** `motion.fast = 80 ms`, `motion.slow = 120 ms`;
   ease-out for enters, ease-in for exits, quantized to cell steps for
   position and theme steps for fades. Anything longer than 120 ms is
   latency wearing a costume.

| Surface event | Motion |
|---|---|
| Palette / picker open | bg and border fade up through 3 theme steps at final geometry, `fast` ease-out; no position animation on open |
| Palette / picker close | instant — dismissal returns focus and must feel like release, not choreography |
| Toast enter | slides 3 cells in from the right edge, `fast` ease-out |
| Toast exit | fades to backdrop over `slow`, then the row collapses |
| Tree expand | children reveal top-down over `fast` |
| Tree collapse | instant |
| List scroll / selection move | instant, always — smoothness in a list is latency, not interpolation |

Config surface (every key optional; defaults derive as above):

```toml
[ui]
motion = "on"        # "off" for reduced motion; ignored below full tier
backdrop = "dim"     # "none" keeps the grid at full brightness under modals

[ui.tokens]          # explicit values override theme derivation per token
accent = "#bd93f9"
surface_raised = "#343746"
```

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

Invented capabilities — also v0.1 core (ruled 2026-08-05; plans authored at
the P4 exit, supervision and remote editing sequenced first). These are not
plugin replacements: each is possible only because the UI and the engine are
separate processes with view interposed on every keystroke and frame.

| Capability | v0.1 scope |
|---|---|
| Engine supervision | The UI survives an engine hang or crash: stall detection, interrupt offer, automatic restart with swap (`-r`) rehydration — a misbehaving plugin cannot take the editor down |
| Remote editing | Engine spawned over SSH, paint and input local, speculative echo hides link latency (§5.6); clipboard already identical local/remote via OSC52 (§5.3) |
| Session DVR | Visual scrub/replay of what the screen showed, branch from any point, exportable replay file — over the same keystream+frame recording the oracle uses |
| Key introspector | `:View keys`: which mapping fired, whose it was, what it displaced — live, over the key-claim reporting layer (§5.3) |
| Image viewing | kitty graphics protocol on the `full` tier (§7), half-block cell fallback below, painted as a native overlay; extends to picker preview and tree hover; the engine keeps the buffer, view supersedes only the paint |
| Media playback (ruled 2026-08-07; open-dispatch ruled 2026-08-08) | `view file.mp4` / audio: seamless full-terminal handoff to a detected system `mpv` (terminal video output, audio native to mpv; doctor-guided when absent, never bundled), view resumes on exit. Not CLI-only: every open path (CLI arg, tree select, picker accept) runs one shared open-dispatch — the same type detection routing to buffer, image overlay, or media handoff — so selecting a video in the tree plays it (plan 2026-08-09-p5_5-media.md owns the mechanism); composited in-pane playback arrives with the workspace arc (§15.1) |

`ext_messages` routing — owning messages means owning *dialogs* (it forces
`cmdheight=0`; the engine's message area ceases to exist):

| Message kind | Treatment |
|---|---|
| `confirm`, `return_prompt`, inputlist-class | Modal native prompt overlay: takes `Focus::Native`, replies via RPC. The engine is *blocked* on these — a timeout toast here would hang first-run plugin bootstraps. Also captured in history, matching the `emsg`/transient rows below — amended at the P4 exit drain (2026-08-09, coordinator ruling, reported to the user): a `:messages`-style scrollback that dropped the confirm prompt the user just answered would be the surprising behavior, not the documented one, and nvim `:messages` parity is the migration contract this row exists to honor |
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
  `Effect::Rpc(nvim_buf_set_text …)` calls, **one undo entry per review,
  not per accept**: the first accept of a review opens the entry and every
  later accept joins it, so a single `u` retracts the whole review. Never
  one entry per line-range within a hunk, and never an accept chain joined
  onto the user's own preceding edit. **Amended 2026-08-21.** This line
  promised "one undo step per accept (undojoin policy)" until the plan's
  `ea41a92` found that promise unsatisfiable: `:undojoin` merges into the
  previous entry by construction, so per-hunk undo stepping is not
  representable with it at all, and the two halves of the old wording
  contradicted each other. The spec follows the mechanism rather than
  asking the mechanism to follow it. Hunks rebase live against concurrent user edits
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
tree_width = 30            # tree sidebar's share of the terminal width, 15..70

[keys]
sidebar_wider = ["<S-Right>", "<C-w>>"]     # resize the focused sidebar; one
sidebar_narrower = ["<S-Left>", "<C-w><"]   # notation or a list, chords of two
composer_newline = ["<S-CR>", "<M-CR>"]     # break a line in the AI composer

[supervision]
auto_restart = true        # false: surface a dead engine and wait for a manual restart

[ai]
enabled = true
agent = "claude-code"      # ACP agent id or ["cmd", "args…"]
panel_width = 30           # agent panel's share of the terminal width, 15..70

[ai.review]
open_target = "current"    # current | split -- where a proposal's file opens
                           # when no window already has it
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
4. Core buffer/editing: last — slated for a dedicated feasibility and
   effort/maintenance-vs-benefit assessment after v0.2 (user ruling
   2026-08-08: fresh-eyes evaluation, not foreclosed in advance); moves only
   if the oracle can prove parity per permutation and the perf win is
   measured, not assumed.

Each subsystem's gate includes a **corpus manifest committed before
implementation starts**: the exact input set parity is claimed over (file
set, grammar/LSP versions, locales, encodings, terminal sizes). "Parity
proven" means proven over that manifest — no manifest, no strangling.

### 15.1 Terminal workspace arc (v0.2, ruled 2026-08-07)

The identity extension past v0.1: "use it for anything you can view" —
files, another machine's tree, pictures, websites, videos — bridging a bare
server and a minimal desktop experience from one terminal binary. Larger
lifts explicitly deferred out of v0.1 by the user (2026-08-07); the v0.1
graphics substrate (§9 image viewing) and media handoff are their seeds.

- **Pane compositor**: generalize the single-grid-plus-overlays surface to N
  tiled content surfaces (engine grid, browser, media, image, remote tree)
  with a tiling layout tree; the overlay class persists alongside, per
  element non-negotiable (notifications), default, or configurable.
- **Browser pane**: CDP-driven **system** Chromium (`chromiumoxide`) —
  screencast frames painted through the §9 graphics substrate, input
  forwarded via the CDP Input domain, qutebrowser-style modal bindings with
  injected-JS link hints owned by view (theme- and keymap-coherent). The
  browser is detected, never bundled; `doctor` guides when absent.
- **In-pane media**: mpv composited into a pane (IPC or libmpv), replacing
  the v0.1 full-terminal handoff shape.
- **Servo watch**: the long-term in-process replacement for the CDP path
  once its embedding API stabilizes and open-web compat suffices;
  controlled-content surfaces (docs/HTML preview) may adopt it earlier.

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
  master branch, tj-smith47 identity, machine-local `.claude/` session files gitignored (spec/plans/hooks tracked), Taskfile from
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
P5.5 Invented capabilities (§9, ruled v0.1 core): supervision, remote
    editing, session DVR, key introspector, image viewing, media handoff —
    plans authored at the P4 exit, supervision + remote sequenced first
P6  Polish: multigrid attach + panes, config surface, doctor, docs, tier
    goldens, Windows tier-1 promotion (ConPTY-gated, winserver-validated)
                                                                  ← v0.1
P7  Workspace arc (§15.1, v0.2): pane compositor, browser pane via CDP,
    in-pane media
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
| First-release scope (ruled 2026-08-05) | Five invented capabilities (§9: supervision, remote editing, session DVR, key introspector, image viewing) ship in v0.1 as core features alongside AI | Post-v0.1 rollout: differentiation would rest on refined plugin equivalents alone, which reads as repackaged nvim rather than a new category |
| v0.1 media playback shape (ruled 2026-08-07) | Full-terminal handoff to detected system mpv; doctor-guided | Composited in-pane playback: requires the workspace-arc pane compositor, deferred to v0.2 with it |
| Float title style (§7.1, ruled 2026-08-09 at the P4 exit drain) | `ChromeGroup::FloatTitle` (nvim's builtin `hl-FloatTitle`) fg over the surface's own bg, bold on every tier | §7.1's original `accent` fg: the accent role vocabulary is unimplemented tree-wide (no code resolves `Function`/`Statement` into a role), so it would have shipped as a hardcoded style nvim has no way to reach — the opposite of the migration audience's expectation that their colorscheme drives view's chrome. Coordinator ruling, reported to the user; §7.1 is itself still pending user ratification |
| `ToastHistory` scope vs §9's letter (ruled 2026-08-09 at the P4 exit drain) | Keep recording confirm-class entries (`confirm`/`return_prompt`/inputlist-class) to scrollback history alongside the sticky and transient rows, and amend §9's routing table to say so | Narrow the recorder to only what §9's routing table listed: a history that silently omits the confirm prompt the user just answered is the surprising outcome for anyone coming from nvim's `:messages`, and painless migration is the tie-breaker. Coordinator ruling, reported to the user |
| Browser pane engine (§15.1, v0.2) | CDP-driven system Chromium via `chromiumoxide`: screencast frames through the native graphics substrate; view owns bindings, hints, theme; browser detected, never bundled | Carbonyl/Carboxyl child process: stalled-upstream Chromium fork, supply-chain risk, UX never view's. Servo embedding: watched as the long-term in-process fit — API unstable and open-web compat incomplete as of 2026-08. Handrolled engine: never |

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
