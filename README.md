<div align="center">
  <img src="assets/view-logo.svg" alt="view" width="120" />
  <h1>view</h1>
  <p><b>A terminal-first modal editor with a modern, coherent UI, powered by an embedded, pinned Neovim.</b></p>
  <p>
    <a href="https://github.com/tj-smith47/view/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/tj-smith47/view/actions/workflows/ci.yml/badge.svg" /></a>
    <img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-8be9fd?style=flat-square" />
    <img alt="Status: pre-alpha" src="https://img.shields.io/badge/status-pre--alpha-ffb86c?style=flat-square" />
  </p>
</div>

Your `init.lua`, plugins, LSP servers, and treesitter setup run unmodified,
because a real Neovim runs them.

**Status: pre-alpha. Not usable as a daily editor yet.** The engine seam,
the Elm-style runtime, and the testing and measurement infrastructure are
built. The visible feature set (picker, file tree, statusline, command
palette, notifications) is not. See [Where it stands](#where-it-stands).

## Why

- **Painless migration**: real Neovim is the engine, so plugin and config
  compatibility comes from running your setup rather than reimplementing
  it. A differential oracle checks view against a reference Neovim on every
  build, and a compat suite drives real pinned plugin configurations
  (lualine, telescope, noice, nvim-cmp, treesitter, mini.nvim and more)
  through a real pty.
- **Modern out of the box**: one design system for the surfaces view owns,
  rather than a plugin patchwork. Where view owns a surface natively, the
  plugin you already use keeps loading and can be handed the surface back
  with a single config key.
- **Honest measurement**: every latency claim on this page is paired, with
  view and bare Neovim measured in the same run, on the same host, with the
  same config, per-sample interleaved. The numbers below are recorded
  baselines rather than aspirations, and the ones that are worse than
  Neovim are listed alongside the ones that are better.

## Where it stands

| Area | State |
|---|---|
| Embedded engine, RPC seam, input and redraw paths | Built |
| Command line, messages, popup menu, tabline, cursor shapes | Built |
| Terminal capability tiers (kitty/ghostty class down to 16-color) | Built |
| Differential oracle, fuzz harness, compat suite, bench matrix | Built |
| Picker, file tree, statusline, command palette, notifications | **Not built.** Next phase |
| Clipboard provider, full CLI passthrough (`+42`, `-R`, `-O`, `ls \| view -`) | **Not built.** Next phase |
| AI integration (ACP client, agent panel, diff review) | **Not built** |
| Windows | Compiles and runs the portable test suite; not yet a supported tier |

## Measured today

Recorded baselines on a shared Linux dev host, Neovim pin `v0.12.4`,
1000 samples per cell, paired and interleaved against bare Neovim.
Reproduce with `task perf-audit`.

| What | view | bare Neovim | |
|---|---|---|---|
| First paint, cold, no plugins (p99) | **26.5 ms** | 132.2 ms | **5x faster** |
| First paint, cold, LazyVim-style stack of 14 plugins (p99) | **104.3 ms** | 167.2 ms | **1.6x faster** |
| Resident memory (PSS) | **3.4 MB** | n/a | budget was 150 MB |
| Redraw parsed to terminal write (p99) | **0.12 ms** | n/a | budget 1 ms |
| Keystroke to cell change, steady typing (p99) | 0.95 ms | 0.80 ms | **~1.2x slower** |
| Sustained scroll, 100k lines (p99 staleness) | 1.40 ms | n/a | budget 16 ms |
| Sustained scroll, versus Neovim | n/a | n/a | **~1.8 to 2.0x slower** |

**Read that honestly.** Cold start and memory are large, real wins. view
paints usable content while Neovim is still loading, because it is a
separate process that does not wait for your config.

Steady-state typing and scrolling are currently *slower* than bare Neovim,
by roughly 20% and 2x. Both are sub-millisecond in absolute terms and far
inside their budgets, so neither is felt today. But the target is to be
faster, not merely fast enough, and we are not there.

**We are actively working on it, and we do not yet know whether the gap is
closable.** The cause is unattributed. Three explanations were adopted and
then falsified by measurement (a thread-hop cost floor, the pty transport,
and the measurement instrumentation itself), and all three are retracted in
the design spec rather than quietly dropped. The remaining hypothesis is
that some of the gap is inherent to being an out-of-process UI speaking
Neovim's RPC protocol at all. If that is true, no external frontend could
beat it, and this page will say so plainly. The experiment that settles it
is a paired run against `nvim --remote-ui`, an external RPC client
containing none of our code. If it shows the same gap, the cost is the
protocol boundary and we will publish that as a permanent limitation. If it
does not, the gap is ours and it is a bug to fix.

Budgets are recorded per machine class and regression-gated, so a change
that makes any of these worse fails the build. Gating against the design
spec's own budget targets, as opposed to against the last recorded value,
is not yet wired up.

## Requirements

- A terminal. Best experience on kitty, ghostty, or WezTerm; degrades
  gracefully elsewhere.
- Neovim `v0.12.4` (pinned; see `.engine-pin`). Release builds will bundle it.

## License

MIT or Apache-2.0, at your option.
