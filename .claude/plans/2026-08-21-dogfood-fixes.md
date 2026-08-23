# 2026-08-21 dogfood fixes — first personal AI-session findings

Four defects from the user's first personal dogfood of the AI panel
(Termius/SSH, real config, 263x88). All reproduced live in tmux against
the release binary at ebc2812; captures under the session scratchpad.
Spec of record: `.claude/specs/2026-07-17-view-design.md`. These are
fixes to shipped P5/P4 surface, not new phase work.

## Global constraints

- nvim owns buffer text; paint loop never awaits RPC; core ← surface ←
  {native, ai}; no unwrap/expect in lib code.
- T2/T3 touch paint: the commit message states the latency consequence,
  and `task bench` budgets must hold.
- Commit only via `task commit -- -m`. One commit per task.

## T1 — Bare `:View <feature>` resolves to the feature's canonical verb

Reproduced: `:View ai` sends `FeatureInvoke { feature: "ai", verb: "" }`;
no arm matches an empty verb, so the user gets the trust prompt, answers
it, and the re-dispatched empty verb lands in the "`:View` needs a
feature and a verb" notice. The panel never opens. This is the exact
flow behind "ai doesn't work at all".

Fix in `crates/view-core/src/update/mod.rs`'s `FeatureInvoke` arm: when
`verb` is empty and `feature` is not, resolve the verb to the feature's
canonical entry point before any gate runs (so the trust prompt's
pending re-dispatch carries the resolved verb too). Canonical verb = the
first `default_maps()` spec whose `feature` matches: ai→toggle,
picker→files, tree→toggle, notifications→history. Unknown features keep
the existing notice. The discoverability notice text stays; it now only
fires for genuinely unknown pairs.

Tests: bare `ai` on a trusted model toggles the panel; bare `ai` on an
untrusted model opens the trust prompt and, once trusted, the re-dispatch
opens the panel (this is the regression that shipped); bare `picker`
opens the files picker; an unknown feature still gets the notice.

## T2 — Overlay chrome takes the theme; Confirm prompt sized to content

Reproduced (escape captures, dracula active and cached). Four distinct
paint defects, and the primary user complaint is the NOTIFICATION
surface, not the prompt:

1. Overlay interiors and borders emit `ESC[49m` (terminal default
   background) instead of the theme's float background — every box
   (toast, message history, Confirm prompt) renders as an unthemed
   black slab over the themed buffer.
2. The buffer's cursorline highlight (`48;2;68;71;90`, full window
   width) runs beneath the default-bg toast box, so the user sees a
   selection-colored bar extending all the way across with an alien box
   sitting in it. The overlay must fully own its cells (opaque themed
   bg) so underlying row highlights read as behind it, not through it.
3. Confirmed attr bleed-through: inside the message-history overlay one
   character mid-row carried the underlying layer's background
   (`48;2;33;34;44`) while its neighbors ran default — overlay cell
   attrs are not consistently overriding what is beneath.
4. The Confirm prompt's selected row is `ESC[7m` reverse video padded
   to the full interior width (~180 columns on the user's terminal);
   toasts stack as adjacent boxes of differing widths that read as one
   ragged lump.

Fix: overlay chrome (interior fill, borders, title, selection row)
draws with theme colors — the same probed/derived palette the statusline
already uses (investigate the `theme` flow: `default_colors_set`, probe
replies, the theme cache). Every overlay cell is opaque: themed bg
written explicitly, never `[49m`, never inherited from the layer
beneath. Selection = themed highlight bg, never bare reverse video. The
Confirm prompt's box width follows its content (question + options +
padding) capped at the current 70%. The toast stack gets one coherent
treatment: consistent width or a single themed container, not ragged
adjacent borders.

Tests: paint-level assertions that every overlay-interior cell carries
the themed bg (non-default) when a theme is active; a regression test
for the bleed-through (underlying non-default bg never survives into an
overlay row); Confirm prompt width tracks content. Latency: chrome
painting is per-frame; state the consequence in the commit.

## T3 — AI transcript follows the tail; scrollable

Reproduced by the user ("I can't even see everything"): the transcript
region is head-keep — a session taller than the panel shows only its
OLDEST screenful. `.claude/plans/2026-08-09-p5-ai.md` records this as
known deferred surface; the user hit it in their first real session, so
it lands now.

Fix in the AI panel state/paint: default to follow-tail (newest
transcript lines visible, like a terminal). Explicit scroll keys while
the panel is focused — keys that cannot collide with prompt text entry
(PageUp/PageDown plus Ctrl-u/Ctrl-d half-pages; implementer reads
`route_key`'s Ai arm and keeps every existing binding working). Scrolling
up holds the viewport (stops following); scrolling to the bottom or
PageDown past the end resumes follow. A one-line indicator (e.g. more
below marker) when scrolled away from the tail. Keys documented in
`docs/ai.md`'s panel section.

Tests: update-level tests for follow-then-hold-then-resume across
appended transcript lines; a paint/rows test that the newest line is in
the derived window by default.

## T4 — Panel width is configurable and resizable

The AI panel and tree sidebar are hardcoded `OverlayBox::new(30, 100)`.
The user asked how to resize; there is no way.

Fix: `[ai] panel_width` and `[native] tree_width` in view.toml (percent,
optional, default 30 — derivable default per the config rule), read
where the other `[ai]`/`[native]` keys already are. Runtime resize while
the panel/tree is focused: `<` and `>` (or the closest non-colliding
pair the existing key routing allows) step the width by 5% within
[15, 70], session-scoped (not persisted). `docs/ai.md` + config docs
updated.

Tests: config round-trip for both keys including out-of-range clamping;
update-level test that resize keys change the overlay geometry and mark
the model dirty.

## T5 — Panel prompt input echo is fast

User report (Termius/SSH, real session): keystroke delay inside the
agent panel's prompt is "unbearable" — noticeably worse than typing in
the buffer, which is backwards: buffer echo round-trips nvim RPC under a
CI-gated budget, while panel input is native and should be strictly
faster. Root-cause with measurement before any fix (systematic
debugging): instrument or bench the panel-focused key → paint path;
suspects include whole-overlay/transcript repaint per keystroke, damage
tracking not covering the prompt row, or paint scheduling waiting on an
unrelated tick. Fix at the source; add a bench or test that pins the
panel-input echo path to the same class of budget the buffer echo has.
Latency consequence stated in the commit (this IS the latency).

## T6 — Long prompt input stays visible

User report: past a certain number of words, typed prompt text stopped
showing (input still accepted, echo gone). The prompt row renders only
what fits the panel width — no wrap, no horizontal scroll. Fix: the
prompt input area grows (wraps to multiple rows, transcript yielding
space) or horizontally scrolls to keep the cursor and tail of the input
visible at all times; cursor position always on screen. Tests: an input
longer than the panel width keeps its tail visible in the derived rows;
cursor tracking asserted at the boundary.

## T7 — Visual acceptance sweep (the standing gate)

The mechanism that makes this class of defect machine-caught instead of
user-caught: a scripted tmux acceptance leg (extending the existing
acceptance harness) that launches the release binary with a real theme
(dracula cache in the fixture HOME), drives the real entry points
(`:View` bare forms, `<leader>` keys, toasts, history, trust prompt,
panel typing), captures panes with escapes, and ASSERTS:

- no `[49m`/default-background cell inside any overlay interior;
- no `[7m` reverse-video anywhere view paints;
- no underlying-layer attr survives into an overlay row (the T2 bleed);
- every registered entry point changes the screen within a bounded time
  (silent no-op = failure);
- panel prompt echo appears in the capture after each simulated
  keystroke burst (T5's invariant, held at the acceptance level);
- prompt input longer than the panel width keeps its tail visible (T6).

Wired as a `task` target and into `task acceptance`; added to the exit
checklist of this plan and every future phase plan's template. Runs
headless in tmux on the dev host; no human in the loop.

## T8 — The pump's event filter reaches gitignore parity with the walk

CI evidence (run 32537625267, ci macos-latest):
`watch::tests::a_gitignored_directory_is_never_watched` failed with the
gitignored `generated/out.rs` present in the delivered batch. Root
cause: gitignore rules are honored only at registration time (the
`ignore::WalkBuilder` walk skips ignored directories, so they get no
descriptor), which suffices on inotify — but macOS's FSEvents backend
delivers events recursively regardless of `RecursiveMode::NonRecursive`,
and the pump's second filter (`is_excluded`) checks only the static
exclusion list. A leaked event under a gitignored directory therefore
reaches the batch on macOS, and a real session probes nvim for
gitignored churn (build output) the design promises to skip.

Fix at the filter, not the test: the pump's per-event filter gains
gitignore parity with the walk. Registration already walks every
directory; collect the `.gitignore` files the walk passes into a shared
matcher (`ignore::gitignore::GitignoreBuilder` rooted at the project
root, one add per discovered `.gitignore`, rebuilt the same way when a
later-created directory registers) and drop any event whose path the
matcher ignores, alongside the existing `is_excluded` check. The
files-created-before-registration path (found_files) must keep working:
non-ignored files still pass.

Tests: a unit test that the pump filter drops a path matched by a
nested `.gitignore` even when the event arrives without a registered
parent (the FSEvents leak shape, simulated directly); the existing
event-level test stays as-is and must pass on macOS. macOS verification
owed via an mbp run of the view-ai suite (script file, provenance line,
caffeinate) before the next push is called green-capable.

## T9 — User messages appear in the transcript

Second-dogfood report (2026-08-22): the panel shows agent responses but
never the user's own messages. Root cause: the `<CR>` submit path takes
the composer text and emits `Effect::AiPromptSubmit` without appending a
transcript entry; the only writer of `TranscriptRole::User` is
`AiEvent::MessageChunk { from_agent: false }`, which requires the agent
to replay the user's message over the wire — the Claude Code ACP adapter
does not. The `You:` role, styling, and rendering already exist and are
reachable only from tests.

Fix: on submit, append the prompt text locally as a `User` entry (instant
echo, terminal-chat convention) with a locally minted message id, and
reconcile incoming `from_agent: false` chunks so an adapter that does
replay the message produces no duplicate (drop a replay that matches the
newest local `User` entry for the in-flight turn; a genuinely new user
chunk — e.g. adapter-side context injection — still appends).

Tests: submit appends a `User` entry and the follow-tail keeps it
visible; a replayed user chunk after a local echo does not duplicate; a
user chunk with no matching local echo still appends; the echoed entry
wraps through the same transcript width path T6 fixed.

Rendering (user-ruled 2026-08-22): NO literal word prefixes — the
current `You:`/`Agent:`/`Thinking:`/`done:` strings go away.
Differentiation is color alone: user prompt bodies in one theme color,
agent responses in another, thought text dimmed — new AI `StyleRole`
variants on the spans, mapped to theme colors in the painter (the same
role→color path `Title` and the `Git*` glyph roles already use). Each
entry opens with a marker glyph so consecutive messages read as discrete
items. Tool-call status renders as glyphs, not words: done ✓, failed ✗,
pending ·, and running an animated braille spinner (⠋⠙⠹⠸⠼⠴⠦⠧)
advancing off the existing tick source so only the marker cell is
damaged per frame; the spinner stops when the call resolves.

Tests: role spans carry the intended style roles; no word prefix
appears in rendered rows; the spinner frame advances on tick and
damages only its cell; a resolved call paints its final glyph.

## T10 — A sticky toast can be dismissed

T7's sweep measured it: a sticky `emsg` toast survives Escape, motions,
insert entry, `:echo ""`, and indefinite idle — `toast::route` gives
`Sticky` no timeout and no dismissal path, so the only exit is another
error replacing it. Fix: an explicit dismissal (the natural key while
no overlay owns input; the message-history overlay stays the archive),
documented, with the sweep gaining a dismiss assert. Stickiness stays —
an error must still survive incidental keypresses (the T2-era
contract); dismissal is deliberate, not incidental.

## T11 — The panel title survives narrow widths

T7's sweep measured it: below ~136 total columns the AI panel top edge
drops the title entirely (`FOCUSED_TITLE` needs 35 cells; a 30% panel
of a 112-col terminal has a 34-cell edge, and `top_edge` falls back to
a bare border). Render a truncated or abbreviated title instead of
none — the box must never be anonymous. Test at the exact boundary
widths; sweep gains a title-present assert at a narrow geometry.

## T12 — `task acceptance` skips class-scoped legs it cannot run

T7 surfaced it: on dev-linux, `remote-rtt.sh` refuses (its budget row
is `classes = ["controlled-linux"]`) and ABORTS the whole target, so
legs after it never run — a silent mid-target drop. Class-scoped legs
must detect an out-of-class host and print an explicit `SKIPPED
(class …)` line, exiting 0, so every runnable leg always runs and the
skip is visible. Refusing to invent baselines for uncontrolled hosts
stays — the fix is skip-with-announcement, not measurement.

## T14 — a paste lands in the focused native surface

Found live 2026-08-23: the user could not paste the recorded-dogfood
prompt into the AI composer. The input reader decodes bracketed paste
into `Msg::Paste`, and update() routes it to `nvim_paste` under engine
focus — but the `Focus::Native(_)` arm returns nothing, so a paste into
the composer (or any native surface) vanishes without a trace.

Fix: the focused composer takes the pasted text at its cursor —
verbatim, one insertion, and a trailing newline must NOT submit (that
prevention is bracketed paste's whole purpose). Wrap/cap accounting
(T6) holds; submit-echo (T9) is unchanged. A native surface with a text
input (picker filter) inserts sans newlines; one with none answers with
a visible notice, never silence (the checklist rule: anything that ends
in nothing on screen is a bug). Sweep leg: tmux `load-buffer` +
`paste-buffer -p` into the focused composer, assert the text appears in
the box and no submission happened.

## T15 — `<leader>ai` re-enters an open, unfocused panel

Found live 2026-08-23, seconds after T14: `<Esc>` un-enters the panel
(non-modal, stays visible), and then no key returns to it —
`<leader>ai` is `:View ai toggle`, which closes an open panel outright,
so refocus needs `:View ai open` typed into the cmdline.

Fix: toggle becomes focus-aware. Open + focused → close (today's
behavior). Open + unfocused → re-enter (claim focus, do not close).
Closed → open. `:View ai toggle` and `<leader>ai` stay one verb;
docs/keymaps.md's line updates with it (its render test will insist).
Tests pin all three states; the sweep's panel leg gains the
esc-then-leader-ai round trip.

## T16 — the native tree paints its entries again (compat regression)

Caught by CI at e432867 and reproduced locally: the compat harness's
neo-tree and nvim-tree native-only legs fail at step 1 — `wait_for
"init.lua"` times out; the native tree opens but never shows the file.
Green at ebc2812, so a chain commit regressed it, and `task compat` was
in no local gate this chain — the fix closes that process gap too
(compat joins the chain-exit evidence, welded to the final-review
dispatch).

Root-cause first (bisect ebc2812..HEAD if reading doesn't pin it), fix
at the cause, keep the failing legs as the red/green proof, and state
why every other native-only leg stayed green.

## T17 — engine `ui_send` bytes reach the terminal

Found live 2026-08-23: `"+y` never reaches the system clipboard for a
user whose config sets `g:clipboard` to nvim's OSC 52 provider under
`$SSH_TTY` — view's own provider correctly stands down, nvim emits the
escape via `nvim_ui_send()`, and that event is delivered only to UIs
attached with the `stdout_tty` ui-option, which view neither sets nor
decodes. The bytes vanish silently (root-cause boundary table:
`.superpowers/sdd/2026-08-21-dogfood-fixes/task-17-rootcause.md`).

Fix: attach with `stdout_tty: true`; decode the `ui_send` redraw event
and write its raw bytes to the terminal through the existing
`Osc52Job`/`drain_osc52` direct-write path, generalized to raw bytes —
one route for every `nvim_ui_send` caller, clipboard included. Absorb
the terminal queries nvim starts issuing at attach (`\x1b]11;?` +
`\x1b[5n`) so their replies never surface as keystrokes. Close the
named coverage gap: an end-to-end yank test with a PRE-EXISTING
`g:clipboard` (the stand-down branch that today is asserted as correct
and never exercised further).

## T18 — `"+p` pastes through the user's OSC 52 provider

Ruled 2026-08-23: paste is T17's other half, not separate work — the
clipboard feature is not done while `"+p` hangs ~10s and delivers
nothing. nvim's OSC 52 provider paste emits a read query
(`ESC ] 52 ; {sel} ; ?`) via `nvim_ui_send()` and waits on a
`TermResponse` the UI must deliver back; T17's whitelist drops the
query as unanswerable because view has no way to capture the
terminal's reply — it would arrive on stdin and be typed into the
buffer as garbage.

Fix (as shipped — the terminal round trip was investigated and
rejected): view answers the query itself. The read query is never
forwarded to the terminal — crossterm decodes an OSC 52 reply into
destructive normal-mode keystrokes and exposes no raw-byte capture
point, so an input-side reply router would mean forking the input
decoder for a reply many terminals refuse anyway (tmux gates it
behind set-clipboard). Instead the query becomes an effect answered
by view's clipboard worker — host clipboard when reachable within a
bounded read budget, else the shadow the passthrough writes now feed
— delivered back through `nvim_ui_term_event("termresponse", …)`,
resolving the provider's wait in well under its first 1s window.
Every query gets exactly one immediate answer; nothing to report
answers empty rather than silent, so no terminal configuration can
reproduce the hang. Coverage: end-to-end paste legs mirroring T17's
yank test (yank feeds the store, paste lands in the buffer, empty
clipboard answers fast), red/green under mutation.

## Exit

- All tasks (T1–T12) fixed, `task ci` green, budgets hold, docs current.
- T7's sweep passes against the rebuilt release binary — the same sweep
  that would have caught T1/T2/T5/T6 before the user did.
- Dogfood journal appended with the findings + fix commits.
- `task compat` exit 0 at every chain exit, run alongside `task ci` in the
  final-review dispatch: `task ci` never spawns a real pty, so the whole
  plugin-supersession surface — and every paint path only a live terminal
  reaches — is unproven until compat runs (T16 shipped to CI unseen for
  exactly this reason).
- Verified live in the same tmux repro flow that reproduced each defect.

### Exit record — 2026-08-23

Every box observed at the chain tip (d2b1533):

- T1–T12 landed through SDD loops (ledger:
  `.superpowers/sdd/2026-08-21-dogfood-fixes/progress.md`); final
  whole-chain review of ebc2812..1a5b08b APPROVED — zero blockers,
  majors, or minors; its three NITs drained (precedence pin test
  d2b1533, host debris removed, one commit-message magnitude ledgered).
  `task ci` green (3156 tests) in that review's own run and again at
  every slot commit.
- `task acceptance` exit 0 at the tip: visual sweep 4/4 legs including
  narrow-title, ai-conformance 7/7, supervision 3/3,
  remote-reconnect 4/4, and remote-rtt announcing
  `SKIPPED (class controlled-linux, host is dev-linux)`.
- Journal appended (6dbb581) with findings and fix commits.
- Live tmux pass against the rebuilt release binary (code tip ffe4a75):
  trust prompt → panel, composer echo, `❯`-marked user entry in its own
  theme color under truecolor capture, sticky `E492` toast dismissed by
  a deliberate `<Esc>`.

Bench evidence en route: `ai_composer` gate OK (p50 0.08ms, p99 0.141ms
vs 0.169 bar) in the final review's run; the gh legs re-record on the
next push round. Latency note owed by 2010df6's body: one fewer
O(overlays) scan per keypress, sub-microsecond, no allocation.

### Exit record — 2026-08-23, second stretch (T13–T18)

Every box observed at the chain tip (e3ec6a8):

- T13–T18 landed through SDD loops (same ledger); each task closed
  APPROVED after its fix round; final whole-branch review of
  e432867..22b5de2 APPROVED — its one MINOR (this file's T18 paragraph
  described the rejected router) and one NIT (audit-tag comments in
  clipboard.rs) drained at e3ec6a8; the ponytail ceiling marker in
  `read_text` ruled in-policy and kept.
- Gates in the final review's own run at 22b5de2: `task ci` exit 0
  (3235 tests), `task compat` exit 0 (29 scenarios, both formerly-red
  tree legs and the clipboard-precedence leg green).
- Visual sweep 5/5 legs against the rebuilt release binary at e3ec6a8 —
  now including the panel-paste leg (composer paste + blocked-prompt
  paste) and the toggle re-enter assertion, the legs born from the live
  session's findings.
- Live tmux smoke against the rebuilt binary: trust prompt → panel,
  composer echo, `❯`-marked user entry under truecolor capture, sticky
  `E492` toast dismissed by `<Esc>`.
- Journal appended with the T13–T18 story, including the honest
  residual: over SSH `"+p` answers from the session/host clipboard,
  not foreign local-machine content (needs the terminal round trip
  crossterm's decoder cannot survive; terminal-native paste covers it).
- Known ceilings documented in code + reports, none actionable-unfixed:
  startup-window yank drop (attach→VimEnter, identical to pre-fix), one
  stranded thread per clipboard-read overrun, uncapped ClipboardStore
  (payload already in memory).

Owed beyond this plan: the recorded long-session dogfood (unblocked by
this stretch), then the push round that re-records the gh bench legs
and flips them to --gate.
