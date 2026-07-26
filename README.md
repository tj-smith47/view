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
| UI shell painted, engine still loading (p99) | **4.1 ms** | n/a | budget 50 ms |
| First paint, cold, no plugins (p99) | **26.5 ms** | 131.4 ms | **5x faster** |
| First paint, cold, LazyVim-style stack of 14 plugins (p99) | **120.5 ms** | 199.8 ms | **1.7x faster** |
| Resident memory (PSS) | **3.4 MB** | n/a | budget was 150 MB |
| Redraw parsed to terminal write (p99) | **0.12 ms** | n/a | budget 1 ms |
| Keystroke to cell change, steady typing (p99) | 0.95 ms | 0.80 ms | **~1.2x slower** |
| Sustained scroll, 100k lines (p99 staleness) | 1.40 ms | n/a | budget 16 ms |
| Sustained scroll, versus Neovim | n/a | n/a | **~1.8 to 2.0x slower** |

**Read that honestly.** Cold start and memory are large, real wins. view
paints usable content while Neovim is still loading, because it is a
separate process that does not wait for your config.

The first row is unpaired on purpose, and the `n/a` is the point: view
paints its shell before it has started the Neovim child at all, so bare
Neovim has no counterpart event to compare against. It shows nothing until
your config finishes. That 4.1 ms figure is the same on a bare config and
on a 14-plugin stack, because nothing in your config has run yet.

Steady-state typing and scrolling are currently *slower* than bare Neovim,
by roughly 20% and 2x. Both are sub-millisecond in absolute terms and far
inside their budgets, so neither is felt today. But the target is to be
faster, not merely fast enough, and we are not there.

**The typing gap is ours, and it is a bug to fix.** This page previously
said the remaining hypothesis was that the gap is inherent to being an
out-of-process UI speaking Neovim's RPC protocol at all, and that a paired
run against `nvim --remote-ui` would settle it either way. That run has
happened. Neovim ships its own out-of-process UI, so we measured it under
the identical protocol:

| steady typing, dev-linux | vs bare Neovim |
|---|---|
| Neovim's own TUI driving a headless Neovim over the UI protocol | **1.02x** |
| view | **1.22x** |

Speaking the protocol from another process costs about 2%. view costs
about 22%. So roughly nine tenths of the gap is view's own code, not the
protocol boundary, and the "permanent limitation" explanation is retracted
along with the three before it (a thread-hop cost floor, the pty transport,
and the measurement instrumentation itself). All four retractions are
recorded in the design spec rather than quietly dropped.

Where it goes is now measured rather than guessed. A tapped build times
every stage of one keystroke's round trip; on the plugin-free fixture the
chain resolves for all 3000 samples with a 0.2% residual, so the split is
trustworthy. Of view's 805 microseconds, 463 are spent inside Neovim
itself, 46 in the terminal and its parser, and the remaining ~300 are
view's: about 215 to carry a keystroke from the pty into an RPC write, and
about 84 to paint the redraw that comes back. There is no single hot spot
to delete, which is why this is honest work rather than a quick fix.

Budgets are recorded per machine class and regression-gated, so a change
that makes any of these worse fails the build. Every measurement is also
checked against the design spec's own budget, not just against the last
recorded value. Seven metrics do not meet their spec budget today; each one
is listed in `crates/view-bench/budgets.toml` with the value it was
accepted at and why, and the build fails if a new one appears, if a listed
one gets worse, or if a listed one is quietly fixed and left on the list.

## Requirements

- A terminal. Best experience on kitty, ghostty, or WezTerm; degrades
  gracefully elsewhere.
- Neovim `v0.12.4` (pinned; see `.engine-pin`). Release builds will bundle it.

## License

MIT or Apache-2.0, at your option.
