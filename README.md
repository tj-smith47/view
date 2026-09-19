<div align="center">

<img src="assets/view-logo.svg" width="120" alt="view logo">

# view

An agentic, Rust-fast terminal editor with a modern UI and Neovim mechanics.

[![CI](https://github.com/tj-smith47/view/actions/workflows/ci.yml/badge.svg)](https://github.com/tj-smith47/view/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Status: pre-alpha](https://img.shields.io/badge/Status-pre--alpha-orange.svg)](#roadmap)

[Install](#install) &bull; [Features](#features) &bull;
[Performance](#performance) &bull; [Roadmap](#roadmap) &bull;
[Building](#building-from-source)

![view editing Rust code, Dracula themed, with a plugin-heavy lazy.nvim config loaded](assets/view-screenshot.png)

</div>

> [!WARNING]
> view is pre-alpha and not usable as a daily editor yet. Most of the visible
> feature set is still landing. See the [roadmap](#roadmap).

## What is view?

view is a terminal editor that embeds a real Neovim as its engine, so your
existing config, plugins, LSP servers, and treesitter setup work on day one.
Around that engine, view draws its own UI in native Rust: one design system
for the whole editor chrome, a process that paints before your config has
finished loading, and AI agents as a first-class part of the editor. An
agent panel (`<leader>ai`) speaks [ACP](https://agentclientprotocol.com) to
real agents, with in-editor review of every proposed change. See
[docs/ai.md](docs/ai.md).

## Features

- **Bring your whole config.** Real Neovim is the engine, so your setup
  runs as it is: telescope, lualine, noice, nvim-cmp, treesitter, mini.nvim
  and the rest load on day one.
- **Fast where you feel it.** Launch, keypress and scroll are measured with
  your config loaded. See [Performance](#performance).
- **Modern out of the box.** The surfaces view owns (statusline, picker,
  file tree, notifications, command palette) share one design system, and a
  single config key hands one back to the plugin you already use. Copy
  [`view.toml.example`](view.toml.example) to `~/.config/view/view.toml`,
  change `picker = true` to `picker = false` under its `[native]` table, and
  restart. In a file written from scratch, `native.picker = false` on a line
  of its own is the whole config.

## Performance

Neovim `v0.12.4` on a shared Linux dev host, whose `dev-linux` is the
default class of this page.

**You open a project.** You type `view ~/.config` and wait for the screen you
can start working in. Under a login-shaped plugin config (lazy.nvim, noice
and nvim-notify) that screen arrives in 53.9 ms under view against 52.4 ms
under Neovim: view 1.5 ms behind. The bar for this moment is level with
Neovim, so it is a bar view has not met. view's own chrome is on screen in
about 4 ms, painted while your config is still loading.

**You type.** You press a key and the character appears. Under that same
login-shaped config, view's worst keystroke in a thousand takes 1.58 ms
against Neovim's 1.43 ms, and at the median view is 11% behind against a bar
of 10%, a second bar missed, by 1% of the round trip. view can also draw the
character it expects before the engine confirms it: under that same config
the predicted glyph is on screen in 0.32 ms at that same worst case, where
the Neovim it is paired against takes 1.25 ms, and the glyph is corrected the
moment the engine answers. What the prediction is for is an engine a network
away, which a separate acceptance leg measures by injecting the round trip at
four tiers (`scripts/acceptance/remote-rtt.sh`).

Scrolling, the picker, what happens when the engine hangs, memory, and what
contributes to each of the numbers above:
[docs/performance.md](docs/performance.md). How they are measured:
[docs/benchmarking.md](docs/benchmarking.md).

## Roadmap

The goal is one terminal binary for anything you can view: a file, another
machine's tree, an image, a website, a video. Rows marked ★ need code
outside the Neovim process.

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

- [x] Command line, messages, popup menu, tabline, cursor shapes
- [x] Terminal capability tiers (kitty/ghostty class down to 16-color)
- [x] **Native UI**: picker, file tree, statusline, command palette,
      notifications, theme derived live from your colorscheme
- [x] Clipboard provider and full CLI passthrough (`+42`, `-R`, `-O`,
      `ls | view -`)
- [x] **AI**: agent panel, ACP client, context providers, diff review in
      the file itself
- [x] ★ **Engine supervision.** A hung or crashed Neovim is interrupted or
      restarted with buffers rehydrated from swap; the UI never blanks.
- [x] ★ **Remote editing.** `view --remote host:path`: engine over SSH,
      paint and input local, keystrokes echoed without waiting for the round
      trip, OSC 52 clipboard.
- [x] **Your plugins keep working.** view knows which plugin still owns
      which surface and steps aside for it; `vim.notify` lands in view's
      notifications; everything holds up over SSH and inside tmux.
- [x] **Small config.** `[ui]` for tier and theme, `[engine]` for your own
      nvim or `NVIM_APPNAME`; everything view can work out for itself is
      optional.

### Landing in the first release

- [ ] **Tiled UI.** Framed panes with gaps and an active accent, status
      segments in the frame edge, a tabpage pill, the tree and the agent
      panel as overlays or sidebars per surface; `[ui] panes` keeps
      Neovim's own separators and statusline for anyone who wants them.
- [ ] ★ **Session DVR.** Scrub, branch, and export the session's keystream
      and frames.
- [ ] ★ **Key introspector.** `:View keys`: which mapping fired, whose it
      was, what it displaced.
- [ ] ★ **Image viewing.** Kitty graphics on capable terminals, half-block
      cells elsewhere; picker preview and tree hover included.
- [ ] ★ **Media handoff.** `view talk.mp4`, or a video picked in the tree,
      hands the terminal to `mpv` and takes it back on exit.
- [ ] **`view doctor`.** Terminal, tier and why, tmux passthrough, `mpv`
      on the path, a repro invocation to paste into an issue.
- [ ] **Windows.** A first-class Windows Terminal experience.
- [ ] **Workspace arc.** Tiles for N content surfaces: an image, a media
      player, a remote tree, a qutebrowser-style browser over CDP, mpv
      composited in a pane.
- [ ] ★ **Agent-fleet attention.** Agent tabs with status (working,
      blocked on you, done) as an attention queue inside the editor.
- [ ] **Detach and reconnect.** tmux-style persistence for the remote
      engine: drop the link, reattach where you left off.
- [ ] **Theme-switcher interop.** An Omarchy-style switcher that retargets
      your colorscheme carries view with it; no view config to rewrite.

Beyond the feature list there is one standing direction: viewport
highlighting and LSP UI move to view's side one subsystem at a time.

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
`bin/` on your `PATH`, keeping the directory intact:

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

Releases are signed with [cosign](https://docs.sigstore.dev) keylessly, and
the signing identity is the release workflow itself. Every archive ships a
`.sig` and a `.pem` beside it, and a `view_<version>_checksums.txt` covers
all of them.

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

You will need stable Rust, [Task](https://taskfile.dev), and the Neovim version
`.engine-pin` names on your `PATH`. A source build resolves `nvim` from your
`PATH`.

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
