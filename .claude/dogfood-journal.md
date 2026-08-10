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
