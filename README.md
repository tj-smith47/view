<div align="center">

<img src="assets/view-logo.svg" width="120" alt="view logo">

# view

An agentic, Rust-fast terminal editor with a modern UI and Neovim mechanics.

[![CI](https://github.com/tj-smith47/view/actions/workflows/ci.yml/badge.svg)](https://github.com/tj-smith47/view/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Status: pre-alpha](https://img.shields.io/badge/Status-pre--alpha-orange.svg)](#roadmap)

[Install](#install) &bull;
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
- **Fast where you feel it.** Every performance claim is a moment you live
  through -- launch until the screen is ready, keypress until the character
  is there, scrolling a huge file -- measured paired against bare Neovim in
  the same run and regression-gated in CI. Where a moment has not been
  measured under a real config yet, it says so instead of borrowing a
  bench-fixture number. See [Performance](#performance).
- **Modern out of the box.** The surfaces view owns (statusline, picker,
  file tree, notifications, command palette) share one design system. Prefer
  the plugin you already use? It still loads, and a single config key hands
  the surface back to it. Copy [`view.toml.example`](view.toml.example) to
  `~/.config/view/view.toml`, change `picker = true` to `picker = false`
  under its `[native]` table, and restart. Writing the file from scratch
  instead? Then `native.picker = false` on a line of its own is the whole
  config. view never edits your config, so that one key is the whole
  reversal.
- **Honest about the gaps.** The moments where view is currently *slower*
  than Neovim are written down with the rest, and the build fails if any of
  them quietly regresses further.

## Not another Neovim distro

A fair question, so here is the direct answer. LazyVim, NvChad, and friends
are plugin collections running inside stock Neovim: same process, same
render path, same startup. view is a separate Rust program that owns the
terminal, embeds Neovim as a headless engine over its UI protocol, and
paints every frame itself.

That architecture is why none of view's surfaces can be a repackaged plugin:
the render path, input handling, the native UI, and the AI integration are
all view's own code. It also changes what a launch looks like: a distro has
nothing to draw until your config has run, while view's own chrome is on
screen in about 4 ms whether your setup has zero plugins or forty, with your
config still loading behind it. That frame is not the screen you start
working in -- that one arrives when your config is done, and it is the
moment the numbers below are about.

## Performance

Two moments, measured paired: view and bare Neovim launched in the same run,
on the same host, with the same config, samples interleaved. Neovim
`v0.12.4`, a shared Linux dev host.

**You open a project.** You type `view ~/.config` and wait for the screen you
can start working in. Under a real plugin config that moment is not yet
recorded, and it is left blank rather than filled in from a bench fixture.
What is recorded: with no plugins at all, that screen arrives in 16.1 ms
under view against 14.7 ms under Neovim -- view 1.4 ms behind, on a config
nobody runs. view's own chrome is on screen in about 4 ms regardless, while
your config is still loading, and view does not make Neovim's own startup
slower: the embedded engine reaches its started mark within 0.7 ms of the
same engine under Neovim's terminal UI.

**You type.** You press a key and the character appears. Under a real
config, not yet recorded. Plugin-free, view's worst keystroke in a thousand
takes 0.73 ms against Neovim's 0.67 ms, and at the median view is about 13%
behind -- both a fraction of the ~10 ms where a person starts to notice a
key lagging their finger. With the engine on the far side of a network, view
paints a predicted character at 0.39x the time bare Neovim takes locally.

Scrolling, the picker, what happens when the engine hangs, memory, and what
contributes to each of the numbers above:
[docs/performance.md](docs/performance.md). How they are measured and which
cells are diagnostics that carry no claim:
[docs/benchmarking.md](docs/benchmarking.md).

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

## Install

Every release ships a bundle per platform: the `view` binary and the exact
Neovim the release was tested against, together in one archive. Nothing else
is needed on the machine, and the bundled engine is used in preference to any
`nvim` already on your `PATH`.

```bash
# pick your platform from https://github.com/tj-smith47/view/releases/latest
tar xzf view-v0.1.0-aarch64-apple-darwin.tar.gz
./view-v0.1.0-aarch64-apple-darwin/bin/view yourfile.rs
```

Move the whole extracted directory where you keep local software and put its
`bin/` on your `PATH`; the editor finds its engine relative to itself, so the
directory stays intact:

```
view-v0.1.0-aarch64-apple-darwin/
├── bin/view
└── libexec/view/
    ├── nvim
    └── share/nvim/runtime/
```

On Windows the archive is a `.zip` with the same shape (`bin\view.exe`,
`libexec\view\nvim.exe`).

### Verifying a download

Releases are signed with [cosign](https://docs.sigstore.dev) keylessly, so
the signing identity is the release workflow itself and there is no key to
trust. Every archive ships a `.sig` and a `.pem` beside it, and a
`view_<version>_checksums.txt` covers all of them.

```bash
cosign verify-blob \
  --certificate view-v0.1.0-aarch64-apple-darwin.tar.gz.pem \
  --signature view-v0.1.0-aarch64-apple-darwin.tar.gz.sig \
  --certificate-identity "https://github.com/tj-smith47/view/.github/workflows/release.yml@refs/tags/v0.1.0" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  view-v0.1.0-aarch64-apple-darwin.tar.gz
```

```bash
sha256sum --check --ignore-missing view_0.1.0_checksums.txt
```

## Building from source

You will need stable Rust, [Task](https://taskfile.dev), and the Neovim
version `.engine-pin` names on your `PATH`. A source
build resolves `nvim` from your `PATH`; only the released bundles carry an
engine of their own.

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
