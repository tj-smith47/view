# Theming, and driving view from a theme switcher

## What view follows

view holds no palette of its own. Its frames, bars, menus and panels take
their colours from the colorscheme the editor is showing.

With `theme = "auto"`, the default, view follows the colorscheme your config
ends on. When the colorscheme changes while view is running, every one of
view's own surfaces is redrawn in the new colours on the next frame. Nothing
restarts.

## Changing the theme of a running view

A switcher changes the colorscheme of each running editor through its server
socket. The same loop reaches plain Neovim and view.

```bash
for server in "${XDG_RUNTIME_DIR:-/tmp}"/nvim.*.0; do
  nvim --server "$server" --remote-expr "execute('colorscheme tokyonight')"
done
```

`:colorscheme tokyonight` typed in view does the same for that one session.

## Setting the theme for the next launch

Two keys in `view.toml` are read at launch. Their paths are fixed and both
live in that one file, so a switcher can edit them in place.

```toml
[ui]
theme = "tokyonight"     # runs :colorscheme tokyonight at launch

[ui.tokens]
accent = "#7aa2f7"       # the active tile's frame colour
```

| key | read | what it does |
|---|---|---|
| `[ui] theme` | at launch | `"auto"` follows your config's colorscheme. A name runs `:colorscheme` with it. |
| `[ui.tokens] accent` | at launch | `"auto"` takes the accent from the colorscheme and follows it live. A hex colour stays as written until the next launch. |

`view --theme gruvbox` and `VIEW_UI_THEME=gruvbox` set the theme for one
session and outrank the file.

A name the editor cannot find is reported in a notice, and view keeps the
colours it had.
