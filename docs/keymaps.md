# Default keys

Every native feature is reached through a real nvim mapping, registered
after your config has run so `<leader>` is whatever you set `mapleader`
to. `:map`, `maparg()`, and which-key see these exactly as they see your
own mappings, because that is what they are.

The table below is generated from `default_maps()` in
`crates/view-core/src/native/mappings.rs`; a test fails if this page and
the keys view registers disagree.

<!-- generated from default_maps() -->
| key | feature | command |
| --- | --- | --- |
| `<leader>ff` | `picker` | `:View picker files` |
| `<leader>fb` | `picker` | `:View picker buffers` |
| `<leader>fg` | `picker` | `:View picker grep` |
| `<leader>e` | `tree` | `:View tree toggle` |
| `<leader>fm` | `notifications` | `:View notifications history` |
| `<leader>ai` | `ai` | `:View ai toggle` |

## Turning them off

A default key is registered only for a feature that is on, and only for
features that are on:

```toml
[native]
picker = false
```

With that line in `view.toml`, view registers none of the picker's keys
and whatever your own config mapped `<leader>ff` to keeps working. The
first time view takes a key you had mapped, it tells you so and names the
line above verbatim.

`ai` is the one exception: it has no `[native]` entry, since its own
enabled state lives in `[ai]` instead:

```toml
[ai]
enabled = false
```

With that line, `<leader>ai` registers nothing (no `[native]` line can turn
it off) and `:View ai …` answers with a notice instead of opening the panel.
The same first-run notice the picker example above gets applies here too: if
`<leader>ai` was already yours, taking it is reported, and the line above is
what the notice names to give it back.

## Resizing the sidebars

The file tree and the AI panel are sidebars, and the focused one resizes
with the same pair, 5% of the terminal per press:

| key | does |
| --- | --- |
| `<S-Right>` | one notch wider |
| `<S-Left>` | one notch narrower |

Direction reads the way `<C-w><` and `<C-w>>` do -- right widens, left
narrows -- whichever edge the sidebar is pinned to. These are view's own
keys inside its own surfaces, not nvim mappings, so they take nothing from
your config and appear in no `:map` listing.

A width holds between 15% and 70% and lasts the session. The width a
session starts at is `view.toml`'s:

```toml
[native]
tree_width = 25            # percent of the terminal; 15..70, default 30

[ai]
panel_width = 40
```

Both are optional, and a value outside the range opens at the nearest end
rather than refusing to start.

## `:View`

The command is registered whatever you have turned off, so a feature is
always reachable even with no keys at all:

```vim
:View picker files
:View picker grep
:View tree toggle
```

It completes both arguments against every entry point this build has.
