# Dogfooding journal

One entry per phase exit: date, what was used for real work, what felt
fast/slow/wrong, unprompted reactions. Feeds spec §3.5 product metrics.

## 2026-07-18 — P1 exit

First end-to-end session: `view <file>`, insert-mode typing, `:wq` writes the
file and exits 0; `:cq 5` propagates exit 5; SIGKILL of the embedded nvim maps
to exit 137 with the terminal restored. Keypress-to-paint v0 baseline: view
p50 6.82ms vs nvim 0.44ms (~15x slower). The gap is structural, in two
parts: the drain loop paints only after a 4ms recv_timeout expires with no
further redraw traffic (the larger, deterministic component), and input is
drained only after that same loop yields (up to 4ms more). The P2 runtime
redesign owns closing both, and the P3 protocol makes it a gate. Numbers from a shared
container, informational only.

## 2026-07-18 — P2 exit
First real edits through view at 2931d22: scripted tmux session (isolated
config) inserted a line into a scratch file and persisted it via :wq —
10/10 checklist observations passed, terminal intact after exit. A
real-config launch (user's nvim setup) rendered NvimTree, lualine, and a
plugin's startup error toasts through view's native message path: plugin
compat holding on a heavyweight config with zero view-side changes.
Residual echo-latency ratio 1.21x vs paired nvim (budget ≤1.10x) is P3's
first gate. Guided QA doc for a human pass: .claude/qa/p2-guided-qa.md.

P3 exit (2026-08-01, tip 94f8732): scripted tmux session against the
release binary opened a scratch file, inserted a line in normal->insert
flow, and persisted it via :wq — file contents verified after exit,
terminal intact. The user's real config rendered NvimTree and lualine
through view, and noice.nvim's ext_cmdline startup warning surfaced as a
native toast — the exact open item tracked as the noice ext_* suppression
task, observed in real use rather than a harness. The differential oracle
now watches this surface: corpus 24/24 PARITY at 56177dd (ten fuzz-found
scripts promoted to regression entries) and seeded fuzz runs recorded in
the P3 exit checklist evidence.

P3 exit refresh (2026-08-03, code tip bb139c5): re-observed at the exit
battery tip. Scripted tmux session against the release binary opened a
scratch file, appended a line in normal->insert flow, and persisted it via
:wq — contents verified after exit 0, terminal intact. The real config
again rendered NvimTree, the alpha dashboard, and lualine through view,
with noice.nvim's ext_cmdline warning surfacing as native toasts — the
still-open noice ext_* suppression item, observed in real use. New since
the 94f8732 entry: the daily-config compat scenario now exercises this
same real-config surface as a harness row (15/15 OK on both hosts at
bb139c5, twice back-to-back on macOS).

## 2026-08-10 — P4 exit

Guided acceptance pass against the release binary at c579b41, three scratch
HOME/XDG environments (isolated from the real host's own state dir) driven
through scripted tmux sessions covering the full P4 native surface: the
picker (files/buffers/grep opened, an entry selected, `<Esc>` closed it with
the buffer underneath untouched), the file tree (`<leader>e` opens; while it
has focus keys route through view's own tree state machine rather than
nvim's keymap table, so only `<Down>`/`<Up>`/`<CR>`/`<Esc>`/`a`/`r`/`d` do
anything and `<leader>e` itself is silently swallowed until focus returns to
the buffer — this is a real behavior difference from an nvim-native tree
plugin and is now called out explicitly in the guided QA doc), the
statusline (rendered via toast on first activation), notification toasts
(`:echo` renders transient and expires on its own; `:bogus` renders sticky
and survives both the idle timeout and a keypress, matching bare nvim's own
error persistence), message history (`<leader>fm`), the completion palette
and cmdline, the first-run announcement (fires once per config path, with a
config-path-keyed record confirmed to persist correctly across relaunch in
two independently-isolated scratch HOMEs), and the picker off-switch
(`native.picker = false` suppresses `<leader>ff` and falls back to the
user's own mapping or none; removing the line restores it — confirmed
symmetric both directions).

Two real documentation-accuracy bugs were caught and fixed by actually
driving the steps rather than assuming: the guided QA doc's tree section
described `j`/`k` navigation and a plain `<leader>e` close, when the real
overlay-focus routing only accepts the keys above; and its notifications
section described all messages as transient, when nvim's error/warning
kinds route Sticky. Both are exactly the class of defect a guided pass
exists to catch. `docs/compat.md` was also found stale in the tracked tree
(2026-08-03, 15 rows, pre-three-state) against the current
`compat/results.json` (31 rows, all three-state OK) and was regenerated via
`task oracle -- page`. `docs/keymaps.md` cross-checks clean against every
mapped key exercised live, and is gate-enforced by
`mappings::tests::the_keys_page_renders_the_table_this_build_registers`.

2026-08-14 — deliberately induced engine wedges, driven by hand in live
tmux sessions (not the acceptance harness). On dev-linux: a typed
60s-bound `:lua` hrtime spin against an idle session raised the read-side
banner at 11.26s with no further input — the idle wake-gap fix doing its
job in a real terminal. On the mbp (held awake with caffeinate): the same
typed wedge raised the banner at ~11-12s, held it through the modal
window, and the session came back clean when the spin released. Watching
the readout tick by eye surfaced one wart worth keeping: after a
swap-recovery restart, nvim's multi-line recovery report sits over the
buffer top until a manual Ctrl-L — a real user would wonder whether the
restart worked. Filed in known-bugs.md, treatment owed to the S5
recovery-notice ruling. Also driven by hand: a transient `:echo` toast on
an idle macOS session expires and repaints at exactly 4s with zero input.
The macOS sessions only behave when the laptop is actually awake — the
Deep-Idle/DarkWake cycling that corrupted weeks of remote evidence is a
host property, and any future by-hand macOS dogfooding over ssh needs the
caffeinate wrapper too.

## 2026-08-17 — remote editing (P5.5-remote exit note)

What was actually driven: the stub-ssh path end to end, repeatedly and
adversarially — supervision.sh (3 legs), remote-reconnect.sh (4 legs,
including live crash → backoff → give-up → modal → recovery with unsaved
work), remote-rtt.sh (4 injected RTT tiers through delay-relay), plus 26
remote oracle PARITY legs in task oracle. Real ssh: T12's real-host legs
ran single automated sessions against winserver and mbp (documented in the
plan as once-per-release opt-in evidence, not continuous coverage).

Owed, stated honestly: no human has yet edited over a real network by hand
— a caffeinate-wrapped by-hand session against mbp (real latency, real
sleep/wake, real fingers) is the missing dogfood. The stub proves the
machinery; it cannot surface feel (echo latency perception, banner timing
under real jitter). That session should happen before the first release
tag, and its warts belong here.

## 2026-08-21 — P5 AI exit

What was actually driven: the shipped binary under tmux with the AI panel
open against the real pinned adapter — `claude-code 0.69.0`, provisioned
through the first-run path a new user gets, no `[ai]` table in the config
at all. The work given to it was this phase's own: `git show 4e732df --
crates/view-tui/src/paint.rs`, the paint-attribution slice of the
liveness-file fix, handed over as a file with "review this". Two hunks
were accepted through the review overlay with `a`, and the buffer took
them byte-exactly; every file touched was a scratch copy, never the tree.

What felt right: the agent's review was good, and it caught something
unprompted — that the artifact was only the `paint.rs` slice of the
commit its message describes. It is. Being told that by the panel, about
our own commit, is the first time this feature has been the thing doing
the work rather than the thing being tested.

What felt wrong, and both of these are the reason for dogfooding rather
than asserting:

The adapter writes files with its own tool instead of deferring to the
client, so every proposal arrived already applied to disk. view sees the
external write, reloads the buffer, and the queued hunk's base stops
matching — it is then correctly labelled stale rather than force-applied,
which is the conflict machinery working. But the user's accept is racing
the agent it serves, and the review answers a question the disk settled a
second ago. Not a code change this phase; the capability-negotiation side
is P6 plan input.

Typing a prompt while a review sits unanswered did nothing at all. The
review owns the panel's keys totally and by design (`a` cannot mean both
"accept this hunk" and "type an a"), but an unmapped key was swallowed in
silence, so a whole sentence went nowhere and read as a dead panel. Fixed
this round: an unmapped key now raises one notice naming the open review
and the way out of it.

Evidence: `.claude/HANDOFF.md`'s dated dogfood section, with the pane
captures, accepted files and full session log under
`.superpowers/sdd/2026-08-09-p5-ai/dogfood/`.

Still owed: nobody has lived in the panel for a working day. Two accepts
prove the path; they do not surface what a long session's transcript,
context assembly or repeated turns feel like, and that session's warts
belong here before the first release tag. One wart is already known
before that session runs: the transcript window is head-keep with no
scroll or follow-tail, so a session longer than one panel height shows
only its oldest screenful -- the long-session dogfood will hit this
first, and it lands as planned work, not a surprise.
