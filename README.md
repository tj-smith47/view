<div align="center">

<img src="assets/view-logo.svg" width="120" alt="view logo">

# view

An agentic, Rust-fast terminal editor with a modern UI and Neovim mechanics.

[![CI](https://github.com/tj-smith47/view/actions/workflows/ci.yml/badge.svg)](https://github.com/tj-smith47/view/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Status: pre-alpha](https://img.shields.io/badge/Status-pre--alpha-orange.svg)](#roadmap)

[Features](#features) &bull;
[Not a distro](#not-another-neovim-distro) &bull;
[Performance](#performance) &bull;
[Roadmap](#roadmap) &bull;
[Building](#building-from-source)

![view editing Rust code, Dracula themed, with a plugin-heavy lazy.nvim config loaded](assets/view-screenshot.png)

</div>

> [!WARNING]
> view is pre-alpha and not usable as a daily editor yet. The engine, the
> runtime, and the test infrastructure are built; most of the visible feature
> set is still landing. See the [roadmap](#roadmap).

## What is view?

view is a terminal editor that embeds a real Neovim as its engine, so your
existing config, plugins, LSP servers, and treesitter setup work on day one:
the same Neovim you already run is running them. Around that engine, view draws its
own UI in native Rust: one design system for the editor chrome instead of a
patchwork of plugins, a process that paints before your config has finished
loading, and AI agents as a first-class part of the editor rather than a
bolt-on: an agent panel (`<leader>ai`) speaks [ACP](https://agentclientprotocol.com)
to real agents, with in-editor review of every proposed change. See
[docs/ai.md](docs/ai.md).

## Features

- **Bring your whole config.** Real Neovim is the engine, so compatibility
  comes from running your setup, not reimplementing it. A differential
  oracle checks view against a reference Neovim on every build, and a compat
  suite drives pinned real-world plugin stacks (telescope, lualine, noice,
  nvim-cmp, treesitter, mini.nvim, and more) through a real pty.
- **Fast where you feel it.** view paints a usable shell in ~4 ms and first
  content 2.1 to 5.2x sooner than bare Neovim, at 4.96 MB resident for view's
  own process (the embedded Neovim engine runs separately and is not
  included). Every claim is measured, paired, and regression-gated in CI.
  See [Performance](#performance).
- **Modern out of the box.** The surfaces view owns (statusline, picker,
  file tree, notifications, command palette) share one design system. Prefer
  the plugin you already use? It still loads, and a single config key hands
  the surface back to it. Copy [`view.toml.example`](view.toml.example) to
  `~/.config/view/view.toml`, change `picker = true` to `picker = false`
  under its `[native]` table, and restart. Writing the file from scratch
  instead? Then `native.picker = false` on a line of its own is the whole
  config. view never edits your config, so that one key is the whole
  reversal.
- **Honest about the gaps.** The benchmark table below includes the rows
  where view is currently *slower* than Neovim, and the build fails if any
  of them quietly regress further.

## Not another Neovim distro

A fair question, so here is the direct answer. LazyVim, NvChad, and friends
are plugin collections running inside stock Neovim: same process, same
render path, same startup. view is a separate Rust program that owns the
terminal, embeds Neovim as a headless engine over its UI protocol, and
paints every frame itself.

That architecture is why none of view's surfaces can be a repackaged plugin:
the render path, input handling, the native UI, and the AI integration are
all view's own code. It is also where the speed comes from. A distro waits
for your config before it can draw anything; view's shell is on screen in
about 4 ms while your config is still loading (3.8 to 4.1 ms across
fixtures), regardless of whether your setup has zero plugins or forty.

## Performance

Numbers below are recorded baselines on a Linux dev host, Neovim `v0.12.4`,
1000 samples per cell, measured *paired*: view and bare Neovim in the same
run, same host, same config, samples interleaved. Reproduce with
`task perf-audit`, which gates every recorded row and reports the `user`
row above as uncovered until that recording lands.

| | view | bare Neovim |
|---|---|---|
| Shell painted, config still loading (p99) | **3.8-4.1 ms** | n/a |
| First paint, cold, no plugins, `minimal` (p99) | **25.2 ms** | 130.3 ms |
| First paint, cold, 15-plugin lazy.nvim stack, `heavy` (p99) | **79.3 ms** | 164.3 ms |
| First paint, cold, full login, `user` (p99) | not yet recorded | not yet recorded |
| Resident memory (PSS), view process only, no plugins | **4.96 MB** | n/a |
| Keystroke to cell change, steady typing (p99) | 0.73 ms | **0.67 ms** |

Cold start is a big win. The no-plugins memory row above is view's own
process only (the embedded Neovim engine is a separate process this budget
excludes) and has no bare-Neovim comparison. Under the 15-plugin lazy.nvim
stack, a diagnostic (not CI-gated) reading does have one: bare Neovim's
whole process is 4.39 MB, view's own process is 5.00 MB, and view's own
process plus its embedded Neovim engine child -- the honest comparison,
since view can never be smaller than the Neovim it embeds -- is 27.96 MB,
about 6.4x bare Neovim. See [Performance](#performance) for the full
equivalence matrix. Typing and sustained scrolling are
currently a bit slower than bare Neovim (about 13% and 1.9x, on paths that
are sub-millisecond either way, so neither is something you can feel). We
are actively closing those gaps rather than explaining them away: profiling
already cut the typing overhead roughly in half, and what remains is
itemized down to the microsecond.

The full story, including methodology, the per-stage breakdown of a
keystroke, and how budgets are enforced in CI, lives in
[docs/performance.md](docs/performance.md).

## Roadmap

The goal is one terminal binary for anything you can view: a file, another
machine's tree, an image, a website, a video. Rows marked ★ need code
outside the Neovim process, so no plugin can provide them.

view is the product of bringing together the best features and ideas
throughout the open-source community: [Omarchy](https://omarchy.org) and
[Hyprland](https://hypr.land), for familiar window tiling navigation and
a single config that propagates everywhere;
[qutebrowser](https://qutebrowser.org), a browser driven by vim motions;
[tmux](https://github.com/tmux/tmux), sessions that outlive a connection;
[herdr](https://github.com/herdrdev/herdr), a fleet of agents as an
attention queue; Zed's [Agent Client Protocol](https://agentclientprotocol.com),
the seam an agent plugs into; [kitty](https://sw.kovidgoyal.net/kitty/),
the graphics and keyboard protocols; [mpv](https://mpv.io), video playback.

### Shipped

- [x] Embedded engine, RPC seam, input and redraw paths
- [x] Command line, messages, popup menu, tabline, cursor shapes
- [x] Terminal capability tiers (kitty/ghostty class down to 16-color)
- [x] Differential oracle, fuzz harness, compat suite, benchmark matrix
- [x] **Native UI**: picker, file tree, statusline, command palette,
      notifications, theme derived live from your colorscheme
- [x] Clipboard provider and full CLI passthrough (`+42`, `-R`, `-O`,
      `ls | view -`)
- [x] **AI**: agent panel, ACP client, context providers, diff review in
      the file itself
- [x] ★ **Engine supervision.** A hung or crashed Neovim is interrupted or
      restarted with buffers rehydrated from swap; the UI never blanks.
- [x] ★ **Remote editing.** `view --remote host:path`: engine over SSH,
      paint and input local, keystrokes echoed ahead of the round trip,
      OSC 52 clipboard.

### Landing before v0.1

- [ ] ★ **Session DVR.** Scrub, branch, and export the session's keystream
      and frames.
- [ ] ★ **Key introspector.** `:View keys`: which mapping fired, whose it
      was, what it displaced.
- [ ] ★ **Image viewing.** Kitty graphics on capable terminals, half-block
      cells elsewhere; picker preview and tree hover included.
- [ ] ★ **Media handoff.** `view talk.mp4`, or a video picked in the tree,
      hands the terminal to `mpv` and takes it back on exit.
- [ ] **Migration integrity.** Capability probing that survives SSH and
      tmux, a register of which plugin still owns which surface,
      `vim.notify` through view's notifications, a compat suite that fails
      on migration defects.
- [ ] **Multigrid.** One grid per window: chrome between splits, redraws
      scoped to the window that changed.
- [ ] **`view doctor`.** Terminal, tier and why, tmux passthrough, `mpv`
      on the path, a repro invocation to paste into an issue.
- [ ] **Config surface.** `[ui]` (tier, theme) and `[engine]` (own nvim,
      `NVIM_APPNAME`) go live; anything derivable stays optional.
- [ ] **Windows as a supported tier.** ConPTY-validated, with its own
      budgets, oracle and compat legs in CI.

### After v0.1

- [ ] **Workspace arc (v0.2).** Tiled panes for N content surfaces, a
      qutebrowser-style pane over CDP, mpv composited in a pane.
- [ ] ★ **Agent-fleet attention.** Agent tabs with status (working,
      blocked on you, done) as an attention queue inside the editor.
- [ ] **Detach and reconnect.** tmux-style persistence for the remote
      engine: drop the link, reattach where you left off.
- [ ] **Theme-switcher interop.** An Omarchy-style switcher that retargets
      your colorscheme carries view with it; no view config to rewrite.
- [ ] **Native rendering, behind the oracle.** Viewport highlighting and
      LSP UI move to view's side one subsystem at a time, each only after
      the differential oracle proves parity over a committed corpus.

## Building from source

You will need stable Rust, [Task](https://taskfile.dev), and Neovim
`v0.12.4` on your `PATH` (the engine is pinned; see `.engine-pin`). Release
builds will eventually bundle Neovim so only the pre-built binary needs
nothing installed.

```bash
git clone https://github.com/tj-smith47/view.git
cd view
task build
target/release/view yourfile.rs
```

Best experience on kitty, ghostty, or WezTerm; view degrades gracefully on
less capable terminals.

## License

MIT or Apache-2.0, at your option.
