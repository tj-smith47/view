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
