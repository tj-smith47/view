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

## `<leader>ai` reads the panel before it acts

The AI panel is non-modal: `<Esc>` steps out of it and leaves it on screen
beside your buffer. So `<leader>ai` (`:View ai toggle`) is one verb over the
three states that gives you:

| the panel is | `<leader>ai` |
| --- | --- |
| closed | opens it and puts the cursor in the composer |
| open, and you are in it | closes it |
| open, and you have stepped out | puts you back in it |

The third row is why the key never dead-ends: a panel you escaped out of is
one press from being yours again, not one press from disappearing. Closing
it, whichever way you get there, leaves the agent session running -- reopen
and the transcript is where you left it.

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

Both are optional, and neither can fail your config: a whole number
outside the range opens at the nearest end, and a value that is not a
whole number at all opens at the default and tells you so.

## Dismissing an error

An error or warning is sticky: it stays on screen until you have read it,
where an ordinary message fades on its own. Motions, insert mode and idle
time all leave it standing, on purpose -- an error a `j` could wipe is an
error you never got to read.

`<Esc>` in normal mode takes it down:

```vim
:bogus
" E492: Not an editor command: bogus   -- the toast, still there after 10j
" <Esc>                                -- gone
```

Nothing else changes: the key still reaches nvim exactly as it always did,
so a pending count or operator is cancelled the same way, and `<Esc>` with
no error showing does nothing new at all. In insert, visual or
operator-pending mode `<Esc>` only leaves the mode -- press it again from
normal mode to clear the error.

Dismissing is not deleting. Every message view has shown, errors included,
stays in the history:

```vim
:View notifications
```

which `<leader>fm` also opens.

## `:View`

The command is registered whatever you have turned off, so a feature is
always reachable even with no keys at all:

```vim
:View picker files
:View picker grep
:View tree toggle
```

It completes both arguments against every entry point this build has.
