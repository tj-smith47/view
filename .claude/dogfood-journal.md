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
