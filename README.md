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

view is a terminal editor written in Rust with a modern, cohesive UI and
AI agents as a first-class part of the editor. One design system covers the
whole editor, the editor is on screen before your config has
finished loading, and an agent panel (`<leader>ai`) speaks
[ACP](https://agentclientprotocol.com) to real agents, with in-editor review
of every proposed change. view embeds Neovim as its engine, so your existing
config, plugins, LSP servers and treesitter setup run unchanged. See
[docs/ai.md](docs/ai.md).

## Features

**Your config, unchanged.** telescope, lualine, noice, nvim-cmp, treesitter,
mini.nvim and the rest load on day one. Where a plugin already draws
something view also draws, view draws it, and one config key hands it back.
Plugin messages show up in view's notifications, and everything holds up
over SSH and inside tmux.

**One design system.** The parts of the screen view draws (picker, file tree,
statusline, command palette, notifications, tabline) share one look, themed
live from your colorscheme. A single config key hands any one of them back
to the plugin you already use: copy [`view.toml.example`](view.toml.example)
to `~/.config/view/view.toml` and set `picker = false` under `[native]`.

**Every window in a frame.** Each window gets a frame of its own, with a
gap between them and your accent colour on the one you are working in. Turn
it off with one config key, or let view read your desktop and decide. See
[`docs/tiled-ui.md`](docs/tiled-ui.md).

![two windows, each in its own frame and gap](assets/tapes/tiled-panes.gif)

**Status in every frame.** Each window carries its own status along the
bottom of its frame: the mode in the window you are working in, and the
branch, diagnostics and cursor position in every window.

**Your tabs in the top row.** The row names what you have open, with the
host when you are editing remotely. Click a name to switch.

**The tree and the agent in windows of their own.** The file tree and the
agent panel can open beside your code as windows of their own, and your
buffers make room for them.

**Beside your code or floating over it.** The tree, the agent panel, the
command palette and the notifications each open as a window beside your code
or float over it. One key moves all four between the two.

**Desktop keys where there is no desktop.** Over SSH or on a bare console,
view answers a tiling desktop's window keys: move between windows, zoom one,
open the tree or the palette. On a desktop that already owns those keys,
view keeps to leader keys. See [`docs/keymaps.md`](docs/keymaps.md).

**Agents in the editor.** An agent panel that speaks ACP, an agent that
sees the file, selection and diagnostics you are looking at, and every
proposed change reviewed as a diff in the file itself.

![the agent panel open beside a buffer, reviewing a proposed change](assets/tapes/agent-panel.gif)

**Remote editing.** `view --remote host:path` edits files on another
machine over SSH with the same editor, config and clipboard as at home.
What typing over a link feels like is under [Performance](#performance).

![view opening a file on a remote host over SSH](assets/tapes/remote-editing.gif)

**Your work survives a crash.** If Neovim hangs or crashes underneath
view, it is interrupted or restarted with your buffers restored, and the
screen never goes blank.

![the hang banner, then the same buffer back after a restart](assets/tapes/engine-restart.gif)

**Fast where you feel it.** Launch, keypress and scroll are measured with
your config loaded. See [Performance](#performance).

**Everyday details.** Looks right in every terminal, from kitty and ghostty
down to 16 colors. The config is small: `[ui]` for the theme, `[engine]`
for your own nvim or `NVIM_APPNAME`, everything else optional.

## Performance

The numbers below were taken beside Neovim `v0.12.4` on a shared Linux dev
host. `dev-linux` is the default class of this page.

**You open a project.** You type `view ~/.config` and wait for the screen you
can start working in. Under a login-shaped plugin config (lazy.nvim, noice
and nvim-notify) that screen arrives in 53.9 ms under view against 52.4 ms
under Neovim: view 1.5 ms behind. The bar for this moment is level with
Neovim, so it is a bar view has not met. view's own tree, tabline and
status line are on screen in about 4 ms, drawn while your config is still
loading.

**You type.** You press a key and the character appears. Under that same
login-shaped config, view's worst keystroke in a thousand takes 1.58 ms
against Neovim's 1.43 ms, and at the median view is 11% behind against a bar
of 10%, a second bar missed by 1%. view can also draw the character it
expects before Neovim confirms it: under that same config the predicted
glyph is on screen in 0.32 ms at that same worst case, beside Neovim's
1.25 ms, and the glyph is corrected the moment Neovim answers. The
prediction is for editing over a network, which a separate acceptance leg
measures by adding network delay at four levels
(`scripts/acceptance/remote-rtt.sh`).

Scrolling, the picker, what happens when Neovim hangs, memory, and what
contributes to each of the numbers above:
[docs/performance.md](docs/performance.md). How they are measured:
[docs/benchmarking.md](docs/benchmarking.md).

## Roadmap

The goal is one terminal binary for anything you can view: a file, another
machine's tree, an image, a website, a video.

view brings together ideas from across the open-source community:

- [Omarchy](https://omarchy.org) and [Hyprland](https://hypr.land): tiling
  navigation, the window keys view answers over SSH, and one config that
  propagates everywhere
- [qutebrowser](https://qutebrowser.org): a browser driven by vim motions
- [tmux](https://github.com/tmux/tmux): sessions that outlive a connection
- [herdr](https://github.com/herdrdev/herdr): a fleet of agents as an
  attention queue
- Zed's [Agent Client Protocol](https://agentclientprotocol.com): the
  protocol an agent plugs into
- [kitty](https://sw.kovidgoyal.net/kitty/): the graphics and keyboard
  protocols
- [mpv](https://mpv.io): video playback

### Next

- **Rewind the session.** Scrub back through everything you typed and
  saw, branch from any point, export a clip.
- **Key introspector.** `:View keys`: which mapping fired, whose it was,
  what it displaced.
- **Image viewing.** Open an image in a pane, or preview one from the
  picker or the tree. Sharp on terminals that can show pictures, blocky
  elsewhere.
- **Media handoff.** `view talk.mp4`, or a video picked in the tree, hands
  the terminal to `mpv` and takes it back on exit.
- **`view doctor`.** Which terminal you are in and what it can show,
  whether tmux is in the way, whether `mpv` is installed, and a report to
  paste into an issue.
- **Windows.** A first-class Windows Terminal experience.
- **Panes for anything.** An image, a video, another machine's file
  tree, or a vim-driven web browser, each in a pane beside your code.
- **Agent-fleet attention.** Agent tabs with status (working, blocked on
  you, done) as an attention queue inside the editor.
- **Detach and reconnect.** Drop a remote connection and pick up where
  you left off, the way tmux does.

Beyond the feature list there is one standing direction: syntax
highlighting and LSP UI are drawn by view itself, one piece at a time.

## Install

Every release ships a bundle per platform: the `view` binary and the exact
Neovim the release was tested against, together in one archive. Nothing else
is needed on the machine. view ships with its own Neovim.

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
