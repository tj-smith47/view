# Default keys

Every native feature is reached through a real nvim mapping, registered
after your config has run so `<leader>` is whatever you set `mapleader`
to. `:map`, `maparg()`, and which-key see these exactly as they see your
own mappings.

The table below is generated from `default_maps()` in
`crates/view-core/src/native/mappings.rs`.

<!-- generated from default_maps() -->
| key | feature | command |
| --- | --- | --- |
| `<leader>ff` | `picker` | `:View picker files` |
| `<leader>fb` | `picker` | `:View picker buffers` |
| `<leader>fg` | `picker` | `:View picker grep` |
| `<leader>e` | `tree` | `:View tree toggle` |
| `<leader>fm` | `notifications` | `:View notifications history` |
| `<leader>fp` | `notifications` | `:View notifications pause` |
| `<leader>ai` | `ai` | `:View ai toggle` |
| `<leader><leader>` | `palette` | `:View palette open` |
| `<leader>ug` | `ui` | `:View ui gaps` |
| `<leader>uw` | `ui` | `:View ui cycle_surfaces` |
| `<leader>wn` | `window` | `:View window new` |
| `<leader>wz` | `window` | `:View window zoom` |
| `<leader>ws` | `window` | `:View window flip` |
| `<leader>uf` | `window` | `:View window float` |
| `<leader>fd` | `notifications` | `:View notifications dismiss` |
| `<leader>w1` | `window` | `:View window to_tabpage_1` |
| `<leader>w2` | `window` | `:View window to_tabpage_2` |
| `<leader>w3` | `window` | `:View window to_tabpage_3` |
| `<leader>w4` | `window` | `:View window to_tabpage_4` |
| `<leader>w5` | `window` | `:View window to_tabpage_5` |
| `<leader>w6` | `window` | `:View window to_tabpage_6` |
| `<leader>w7` | `window` | `:View window to_tabpage_7` |
| `<leader>w8` | `window` | `:View window to_tabpage_8` |
| `<leader>w9` | `window` | `:View window to_tabpage_9` |

## `<leader>ai` reads the panel before it acts

The AI panel is non-modal: `<Esc>` steps out of it and leaves it on screen
beside your buffer. So `<leader>ai` (`:View ai toggle`) is one verb over the
three states that gives you:

| the panel is | `<leader>ai` |
| --- | --- |
| closed | opens it and puts the cursor in the composer |
| open, and you are in it | closes it |
| open, and you are not in it | puts you back in it |

A panel you are not in, one you escaped out of or one an agent's own
permission request opened beside you, is one press from being yours.
Closing it, whichever way you get there, leaves the agent session running,
and reopening it brings the transcript back where you left it.

## `<leader>e` reads the tree the same way

With `placement = "windowed"` under `[ui.surfaces.tree]` the file tree takes
a window of its own beside your buffers, and `<leader>e` reads it over the
same three states:

| the tree is | `<leader>e` |
| --- | --- |
| closed | opens it and puts the cursor in it |
| open, and you are in it | closes it |
| open, and you are not in it | puts you back in it |

Close the tree while its window is the only window of the only tab and the
window stays, holding the file you were last in. On any other tab the window
closes, and the tab goes with it.

Open a file in the tree's window and the window is yours from that moment:
the file is what you read there, with the look your config gives that file,
and the next `<leader>e` opens a fresh tree beside it.

`<Esc>` inside the tree takes you back to the window you came from and
leaves the tree standing. Under the default `placement = "overlay"` the
tree draws over your buffers instead, and `<Esc>` closes it.

## Your own keys reach nvim under nvim's names

Every key view does not own itself is forwarded to nvim by the name nvim
gives it, which is the name your mapping is written in. That includes the
four chords a terminal spells as bare control bytes:

| you press | the terminal sends | view forwards | stock vim |
| --- | --- | --- | --- |
| `Ctrl`+`\` | `0x1c` | `<C-\>` | `<C-\><C-n>`, the way out of terminal and insert mode |
| `Ctrl`+`]` | `0x1d` | `<C-]>` | jump to the tag under the cursor |
| `Ctrl`+`^` | `0x1e` | `<C-^>` | edit the alternate file |
| `Ctrl`+`/` | `0x1f` | `<C-_>` | what most comment plugins map |

On a terminal that speaks the kitty keyboard protocol the same chords
arrive as key reports, and `Ctrl`+`4` then arrives as the digit key it
says it is, distinct from `Ctrl`+`\`. Either way the name view forwards is
the one nvim's own input layer would have produced, so a mapping fires in
view exactly where it fires in nvim:

```vim
:nnoremap <C-]> <Cmd>lua vim.lsp.buf.definition()<CR>
```

## Key profiles

view answers the omarchy desktop's own chords on a machine with no desktop
of its own, and nvim's leader keys on a machine that has one:

```toml
[keys]
profile          = "auto"   # "auto" | "desktop" | "editor"
desktop_modifier = "auto"   # "auto" | "super" | "alt"
```

`"auto"` reads the environment, first match wins:

| marker | value |
| --- | --- |
| `SSH_CONNECTION` set | `desktop` |
| `SSH_TTY` set | `desktop` |
| `WAYLAND_DISPLAY` set | `editor` |
| `DISPLAY` set | `editor` |
| macOS, no ssh marker | `editor` |
| Windows, no ssh marker | `editor` |
| anything else | `desktop` |

`:View keys profile` reports the profile and the modifier, each with the
marker that decided it. `:View keys profile desktop|editor|auto` flips the
profile for the running session: every key the new profile takes from a
mapping of your own is reported the way the picker example above reports
one, and every key the old profile had taken is given back.

The chords are `Super` chords, and a terminal reports `Super` only under
the kitty keyboard protocol. `desktop_modifier = "auto"` answers `super`
where the protocol is in force and `alt` everywhere else; `alt` reaches
view from every terminal, as the `Esc`-prefixed spelling every terminal
sends for it.

From a macOS client over ssh, `desktop_modifier = "alt"` together with the
terminal's Option-as-Alt setting is the pair to use: Terminal, iTerm and
kitty on macOS hold `Cmd` for themselves before the pty sees it.

### The chords

46 chords, one row per `[keys.desktop]` key. `super` and `alt` are the two
spellings `desktop_modifier` picks between; `twin` is the key that reaches
the same result under the editor profile, and stays bound under both:

<!-- generated from desktop_chords() -->
| omarchy chord | `[keys.desktop]` row | super | alt | reaches | twin |
| --- | --- | --- | --- | --- | --- |
| `SUPER + LEFT` | `focus_left` | `<D-Left>` | `<M-Left>` | `<C-w>h` | `<C-w>h` |
| `SUPER + RIGHT` | `focus_right` | `<D-Right>` | `<M-Right>` | `<C-w>l` | `<C-w>l` |
| `SUPER + UP` | `focus_up` | `<D-Up>` | `<M-Up>` | `<C-w>k` | `<C-w>k` |
| `SUPER + DOWN` | `focus_down` | `<D-Down>` | `<M-Down>` | `<C-w>j` | `<C-w>j` |
| `SUPER + SHIFT + LEFT` | `move_left` | `<S-D-Left>` | `<S-M-Left>` | `<C-w>H` | `<C-w>H` |
| `SUPER + SHIFT + RIGHT` | `move_right` | `<S-D-Right>` | `<S-M-Right>` | `<C-w>L` | `<C-w>L` |
| `SUPER + SHIFT + UP` | `move_up` | `<S-D-Up>` | `<S-M-Up>` | `<C-w>K` | `<C-w>K` |
| `SUPER + SHIFT + DOWN` | `move_down` | `<S-D-Down>` | `<S-M-Down>` | `<C-w>J` | `<C-w>J` |
| `SUPER + W` | `close` | `<D-w>` | `<M-w>` | `<C-w>c` | `<C-w>c` |
| `SUPER + Q` | `close_alt` | `<D-q>` | `<M-q>` | `<C-w>c` | `<C-w>c` |
| `SUPER + RETURN` | `new_tile` | `<D-CR>` | `<M-CR>` | `:View window new` | `<leader>wn` |
| `SUPER + F` | `zoom` | `<D-f>` | `<M-f>` | `:View window zoom` | `<leader>wz` |
| `SUPER + ALT + F` | `full_width` | `<M-D-f>` | `<C-M-f>` | `<C-w>|` | `<C-w>|` |
| `SUPER + J` | `flip_split` | `<D-j>` | `<M-j>` | `:View window flip` | `<leader>ws` |
| `SUPER + T` | `float` | `<D-t>` | `<M-t>` | `:View window float` | `<leader>uf` |
| `SUPER + SPACE` | `palette` | `<D-Space>` | `<M-Space>` | `:View palette open` | `<leader><leader>` |
| `SUPER + ALT + SPACE` | `files` | `<M-D-Space>` | `<C-M-Space>` | `:View picker files` | `<leader>ff` |
| `SUPER + SHIFT + F` | `tree` | `<S-D-f>` | `<M-F>` | `:View tree toggle` | `<leader>e` |
| `SUPER + 1` | `tabpage_1` | `<D-1>` | `<M-1>` | `1gt` | `1gt` |
| `SUPER + 2` | `tabpage_2` | `<D-2>` | `<M-2>` | `2gt` | `2gt` |
| `SUPER + 3` | `tabpage_3` | `<D-3>` | `<M-3>` | `3gt` | `3gt` |
| `SUPER + 4` | `tabpage_4` | `<D-4>` | `<M-4>` | `4gt` | `4gt` |
| `SUPER + 5` | `tabpage_5` | `<D-5>` | `<M-5>` | `5gt` | `5gt` |
| `SUPER + 6` | `tabpage_6` | `<D-6>` | `<M-6>` | `6gt` | `6gt` |
| `SUPER + 7` | `tabpage_7` | `<D-7>` | `<M-7>` | `7gt` | `7gt` |
| `SUPER + 8` | `tabpage_8` | `<D-8>` | `<M-8>` | `8gt` | `8gt` |
| `SUPER + 9` | `tabpage_9` | `<D-9>` | `<M-9>` | `9gt` | `9gt` |
| `SUPER + SHIFT + 1` | `to_tabpage_1` | `<S-D-1>` | `<M-!>` | `:View window to_tabpage_1` | `<leader>w1` |
| `SUPER + SHIFT + 2` | `to_tabpage_2` | `<S-D-2>` | `<M-@>` | `:View window to_tabpage_2` | `<leader>w2` |
| `SUPER + SHIFT + 3` | `to_tabpage_3` | `<S-D-3>` | `<M-#>` | `:View window to_tabpage_3` | `<leader>w3` |
| `SUPER + SHIFT + 4` | `to_tabpage_4` | `<S-D-4>` | `<M-$>` | `:View window to_tabpage_4` | `<leader>w4` |
| `SUPER + SHIFT + 5` | `to_tabpage_5` | `<S-D-5>` | `<M-%>` | `:View window to_tabpage_5` | `<leader>w5` |
| `SUPER + SHIFT + 6` | `to_tabpage_6` | `<S-D-6>` | `<M-^>` | `:View window to_tabpage_6` | `<leader>w6` |
| `SUPER + SHIFT + 7` | `to_tabpage_7` | `<S-D-7>` | `<M-&>` | `:View window to_tabpage_7` | `<leader>w7` |
| `SUPER + SHIFT + 8` | `to_tabpage_8` | `<S-D-8>` | `<M-*>` | `:View window to_tabpage_8` | `<leader>w8` |
| `SUPER + SHIFT + 9` | `to_tabpage_9` | `<S-D-9>` | `<M-(>` | `:View window to_tabpage_9` | `<leader>w9` |
| `SUPER + TAB` | `tabpage_next` | `<D-Tab>` | `<M-Tab>` | `gt` | `gt` |
| `SUPER + SHIFT + TAB` | `tabpage_prev` | `<S-D-Tab>` | `<S-M-Tab>` | `gT` | `gT` |
| `SUPER + code:20` | `narrower` | `<D-->` | `<M-->` | `<C-w><lt>` | `<C-w><` |
| `SUPER + code:21` | `wider` | `<D-=>` | `<M-=>` | `<C-w>>` | `<C-w>>` |
| `SUPER + SHIFT + code:20` | `shorter` | `<S-D-->` | `<M-_>` | `<C-w>-` | `<C-w>-` |
| `SUPER + SHIFT + code:21` | `taller` | `<S-D-=>` | `<M-+>` | `<C-w>+` | `<C-w>+` |
| `SUPER + SHIFT + BACKSPACE` | `gaps` | `<S-D-BS>` | `<C-M-g>` | `:View ui gaps` | `<leader>ug` |
| `SUPER + SHIFT + ALT + comma` | `messages` | `<S-M-D-,>` | `<C-M-n>` | `:View notifications history` | `<leader>fm` |
| `SUPER + comma` | `dismiss` | `<D-,>` | `<M-,>` | `:View notifications dismiss` | `<leader>fd` |
| `SUPER + SHIFT + CTRL + A` | `agent` | `<C-S-D-a>` | `<C-M-a>` | `:View ai toggle` | `<leader>ai` |

### Chords with no editor meaning

The omarchy desktop binds more chords than a terminal session has a use
for; each row below names what it would have to reach for:

<!-- generated from unbound() -->
| omarchy chord | unbound on |
| --- | --- |
| `CTRL + ALT + DELETE` | menus |
| `SUPER + P` | the Hyprland layout modes |
| `SUPER + CTRL + F` | compositor surface properties |
| `SUPER + O` | compositor surface properties |
| `SUPER + ALT + Home` | the saved window width |
| `SUPER + Home` | the saved window width |
| `SUPER + L` | the Hyprland layout modes |
| `SUPER + SHIFT + ALT + code:10` | menus |
| `SUPER + SHIFT + ALT + code:11` | menus |
| `SUPER + SHIFT + ALT + code:12` | menus |
| `SUPER + SHIFT + ALT + code:13` | menus |
| `SUPER + SHIFT + ALT + code:14` | menus |
| `SUPER + SHIFT + ALT + code:15` | menus |
| `SUPER + SHIFT + ALT + code:16` | menus |
| `SUPER + SHIFT + ALT + code:17` | menus |
| `SUPER + SHIFT + ALT + code:18` | menus |
| `SUPER + code:19` | menus |
| `SUPER + SHIFT + code:19` | menus |
| `SUPER + SHIFT + ALT + code:19` | menus |
| `SUPER + S` | the scratchpad |
| `SUPER + ALT + S` | the scratchpad |
| `SUPER + grave` | the scratchpad |
| `SUPER + SHIFT + grave` | the scratchpad |
| `SUPER + CTRL + TAB` | the Hyprland layout modes |
| `SUPER + SHIFT + ALT + LEFT` | monitors |
| `SUPER + SHIFT + ALT + RIGHT` | monitors |
| `SUPER + SHIFT + ALT + UP` | monitors |
| `SUPER + SHIFT + ALT + DOWN` | monitors |
| `ALT + TAB` | ALT + TAB |
| `ALT + SHIFT + TAB` | ALT + TAB |
| `CTRL + ALT + TAB` | monitors |
| `CTRL + ALT + SHIFT + TAB` | monitors |
| `SUPER + ALT + code:20` | menus |
| `SUPER + ALT + code:21` | menus |
| `SUPER + SHIFT + ALT + code:20` | menus |
| `SUPER + SHIFT + ALT + code:21` | menus |
| `SUPER + CTRL + code:20` | menus |
| `SUPER + CTRL + code:21` | menus |
| `SUPER + CTRL + SHIFT + code:20` | menus |
| `SUPER + CTRL + SHIFT + code:21` | menus |
| `SUPER + mouse_down` | the Hyprland layout modes |
| `SUPER + mouse_up` | the Hyprland layout modes |
| `SUPER + mouse:272` | compositor surface properties |
| `SUPER + mouse:273` | compositor surface properties |
| `SUPER + G` | window groups |
| `SUPER + ALT + G` | window groups |
| `SUPER + ALT + LEFT` | window groups |
| `SUPER + ALT + RIGHT` | window groups |
| `SUPER + ALT + UP` | window groups |
| `SUPER + ALT + DOWN` | window groups |
| `SUPER + ALT + TAB` | window groups |
| `SUPER + ALT + SHIFT + TAB` | window groups |
| `SUPER + CTRL + LEFT` | window groups |
| `SUPER + CTRL + RIGHT` | window groups |
| `SUPER + ALT + mouse_down` | window groups |
| `SUPER + ALT + mouse_up` | window groups |
| `SUPER + ALT + code:10` | window groups |
| `SUPER + ALT + code:11` | window groups |
| `SUPER + ALT + code:12` | window groups |
| `SUPER + ALT + code:13` | window groups |
| `SUPER + ALT + code:14` | window groups |
| `SUPER + SLASH` | monitors |
| `SUPER + ALT + SLASH` | monitors |
| `SUPER + CTRL + E` | menus |
| `SUPER + CTRL + C` | capture and media |
| `SUPER + CTRL + O` | menus |
| `SUPER + CTRL + H` | menus |
| `SUPER + SHIFT + code:201` | menus |
| `SUPER + ESCAPE` | menus |
| `XF86PowerOff` | menus |
| `SUPER + K` | menus |
| `SUPER + ALT + K` | menus |
| `SUPER + CTRL + K` | menus |
| `SUPER + CTRL + Q` | application launching |
| `XF86Calculator` | application launching |
| `SUPER + SHIFT + SPACE` | menus |
| `SUPER + CTRL + SPACE` | menus |
| `SUPER + SHIFT + CTRL + SPACE` | menus |
| `SUPER + BACKSPACE` | compositor surface properties |
| `SUPER + CTRL + BACKSPACE` | compositor surface properties |
| `SUPER + CTRL + ALT + F` | compositor surface properties |
| `SUPER + SHIFT + comma` | menus |
| `SUPER + CTRL + comma` | menus |
| `SUPER + ALT + comma` | menus |
| `SUPER + CTRL + I` | menus |
| `SUPER + CTRL + N` | monitors |
| `SUPER + CTRL + Delete` | monitors |
| `SUPER + CTRL + ALT + Delete` | monitors |
| `switch:on:Lid Switch` | menus |
| `switch:off:Lid Switch` | menus |
| `PRINT` | capture and media |
| `ALT + PRINT` | capture and media |
| `SUPER + ALT + code:34` | capture and media |
| `SUPER + ALT + code:35` | capture and media |
| `SUPER + PRINT` | capture and media |
| `SUPER + CTRL + PRINT` | capture and media |
| `SUPER + CTRL + S` | menus |
| `SUPER + CTRL + PERIOD` | capture and media |
| `SUPER + CTRL + R` | menus |
| `SUPER + CTRL + ALT + R` | menus |
| `SUPER + SHIFT + CTRL + R` | menus |
| `SUPER + CTRL + ALT + T` | menus |
| `SUPER + CTRL + ALT + B` | menus |
| `SUPER + CTRL + ALT + W` | menus |
| `SUPER + CTRL + A` | menus |
| `SUPER + CTRL + B` | menus |
| `SUPER + CTRL + D` | menus |
| `SUPER + CTRL + ALT + D` | menus |
| `SUPER + CTRL + W` | menus |
| `SUPER + CTRL + P` | menus |
| `SUPER + CTRL + T` | menus |
| `SUPER + CTRL + Z` | compositor surface properties |
| `SUPER + CTRL + ALT + Z` | compositor surface properties |
| `SUPER + CTRL + L` | menus |
| `SUPER + SHIFT + RETURN` | application launching |
| `SUPER + ALT + SHIFT + F` | application launching |
| `SUPER + SHIFT + B` | application launching |
| `SUPER + SHIFT + ALT + B` | application launching |
| `SUPER + SHIFT + N` | application launching |
| `SUPER + ALT + RETURN` | application launching |
| `SUPER + CTRL + RETURN` | application launching |
| `SUPER + SHIFT + M` | application launching |
| `SUPER + SHIFT + ALT + M` | application launching |
| `SUPER + SHIFT + D` | application launching |
| `SUPER + SHIFT + G` | application launching |
| `SUPER + SHIFT + O` | application launching |
| `SUPER + SHIFT + W` | application launching |
| `SUPER + SHIFT + SLASH` | application launching |
| `SUPER + SHIFT + A` | application launching |
| `SUPER + SHIFT + ALT + A` | application launching |
| `SUPER + SHIFT + C` | application launching |
| `SUPER + SHIFT + E` | application launching |
| `SUPER + SHIFT + ALT + E` | application launching |
| `SUPER + SHIFT + Y` | application launching |
| `SUPER + SHIFT + ALT + G` | application launching |
| `SUPER + SHIFT + CTRL + G` | application launching |
| `SUPER + SHIFT + P` | application launching |
| `SUPER + SHIFT + S` | application launching |
| `SUPER + SHIFT + X` | application launching |
| `SUPER + SHIFT + ALT + X` | application launching |
| `SUPER + CTRL + code:10` | menus |
| `SUPER + CTRL + code:11` | menus |
| `SUPER + CTRL + code:12` | menus |
| `SUPER + CTRL + code:13` | menus |
| `SUPER + CTRL + code:14` | menus |
| `SUPER + CTRL + code:15` | menus |
| `SUPER + CTRL + code:16` | menus |
| `SUPER + CTRL + code:17` | menus |
| `SUPER + CTRL + code:18` | menus |

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
it off) and `:View ai …` answers with a notice. The same first-run notice
the picker example above gets applies here too: if `<leader>ai` was already
yours, taking it is reported, and the line above is what the notice names
to give it back.

The desktop chords turn off together, with the profile:

```toml
[keys]
profile = "editor"
```

With that line, view registers none of the desktop chords, and every
`<C-w>`-shaped twin keeps working. A single chord unbinds on its own row,
with an empty value:

```toml
[keys.desktop]
close_alt = ""
```

## Answering an agent's permission request

While a permission request is up, the entered panel's keys are the digits
the prompt paints against its own option rows, in the order the agent
offered them:

| key | answers |
| --- | --- |
| `1` … `9` | the option on that row, whatever the agent called it |
| `<Esc>` | cancels the request |

No letter answers a prompt: the same agent edit that raises the question
raises a review in the buffer beside it, and that buffer stays an ordinary
editable one. See [ai.md](ai.md) for what each option does and for what
the two "always" answers stand for.

## Deciding an agent's proposed edit

A proposal is drawn in the file itself, and its keys are buffer-local nvim
mappings on the reviewed buffer, set when the review opens and deleted when
it closes. They are in `:map` for exactly that window, and they take
nothing from your config in between: the whole set lives under
`<leader>h` and on `]c`/`[c`.

<!-- generated from review_keys() -->
| key | does | command |
| --- | --- | --- |
| `<leader>ha` | accept the hunk under the cursor | `:View review accept` |
| `<leader>hA` | accept every hunk still fresh, as one write | `:View review accept_all` |
| `<leader>hx` | reject the hunk under the cursor | `:View review reject` |
| `<leader>hR` | re-anchor a hunk your own edit moved under | `:View review rediff` |
| `<leader>hq` | leave the review, deciding nothing further | `:View review leave` |
| `]c` | the next hunk still awaiting a decision | `:View review next` |
| `[c` | the previous hunk still awaiting a decision | `:View review prev` |

`:View review reject_all` rejects the whole proposal at once and is the one
verb with no key, so it is asked for by name.

Every verb is also a `:View` form, which is what to map if you want the
review on keys of your own. A global mapping of yours is never touched, and
a buffer-local one on a key the table above takes (gitsigns puts one on
`]c`, `[c` and `<leader>hR` in every file it attaches to) is given back when
the review ends:

```vim
nnoremap <silent> ga <Cmd>View review accept<CR>
```

That form is also the way in when `<leader>h` is already yours, and the way
out of a review whose buffer view can no longer write to at all.

See [ai.md](ai.md) for what a review is and what each decision writes.

## Writing a prompt of more than one line

`<CR>` in the composer sends the prompt, so a line break is its own key:

| key | does |
| --- | --- |
| `<M-CR>` | breaks the line, works everywhere |
| `<S-CR>` | breaks the line, needs the kitty keyboard protocol |
| `<CR>` | sends the prompt |

Alt+Enter arrives as `ESC` + Enter from nearly every terminal, so `<M-CR>` is
the one to reach for. A shifted Enter is distinguishable from a plain one only
under the kitty keyboard protocol, and where the terminal does not speak it both
send the same byte, so Shift+Enter *sends the prompt*.

What decides it is the startup capability probe's answer. Every
`full`-tier terminal answers the kitty keyboard query, which is part of
what defines the tier, and a terminal can answer that query and still land
below `full` for an unrelated reason, where `<S-CR>` works too. Over
ssh is the ordinary way to meet that: the tier also wants truecolor, which
takes a second question and a slower round trip, so a kitty-class terminal
can spend the first moments of a session on a lower tier with the keyboard
protocol already on.

| the probe's kitty keyboard answer | what view sends |
| --- | --- |
| yes | `CSI > 1 u` once the alternate screen is up, `CSI < u` before leaving it |
| no | `CSI < u` on the way out and nothing else. A pop nothing pushed is ignored |

`--tier full` asserts all three capabilities, so view sends the sequence
without asking first. A terminal that does not speak the protocol ignores
it, and `<S-CR>` still will not reach the composer.

The window view holds the protocol open for is the one nvim holds it open
for when you run nvim directly in kitty, ghostty or WezTerm. Every exit view
takes for itself pops it before leaving the alternate screen: quitting,
`:cq`, a panic, an error during startup, and the first
`SIGHUP`/`SIGTERM`/`SIGINT`, which view folds into its own teardown. Four
endings cannot pop it, because no view code runs at all: a *second* fatal
signal (view's escape hatch for a session that will not die otherwise, which
leaves from the signal handler), `SIGQUIT`, `SIGKILL`, and an abort. Those
strand raw mode and the alternate screen too, so `reset` is the repair; to
put only the keyboard back, `printf '\e[<u'` pops the protocol on its own.

A pasted line break needs no key at all: paste a multi-line prompt and it
keeps its lines.

The break the agent receives is the same `\n` either way, and the composer
paints the text after it on a row of its own with the cursor on that row.

```toml
[keys]
composer_newline = ["<S-CR>", "<M-CR>"]     # the defaults
```

Alt is `M-` above because that is what view's own encoder emits; `A-` is
read as the same modifier, so either spelling binds the same key.

## Resizing the sidebars

The file tree and the AI panel are sidebars, and the focused one resizes
with the same keys, 5% of the terminal per press. A windowed notification
stream or ticker answers the same keys while it holds focus, stepping
columns at a left or right anchor and rows at a top or bottom one:

| key | does |
| --- | --- |
| `<S-Right>` | one notch wider |
| `<C-w>>` | one notch wider |
| `<S-Left>` | one notch narrower |
| `<C-w><` | one notch narrower |

Direction reads the way `<C-w><` and `<C-w>>` do in nvim, right widens and
left narrows, whichever edge the sidebar is pinned to. Two bindings per
direction because macOS Terminal and Termius keep the shifted arrows for
themselves and view never sees them; the chord reaches through both.

These are view's own keys inside its own surfaces, so they take nothing
from your config and appear in no `:map` listing. They are yours to change:
`[keys]` takes one key notation per action, or a list of them, and a
binding may be a two-key chord:

```toml
[keys]
sidebar_wider = ["<S-Right>", "<C-w>>"]     # the defaults
sidebar_narrower = ["<S-Left>", "<C-w><"]
```

The same rules hold for every action `[keys]` carries, the composer's line
break above included.

Spell a key the way nvim spells it, case and all: `<S-Right>`. A `<...>`
notation that cannot be a key is reported: a modifier prefix outside `S-`,
`C-`, `M-` and `A-`, or a name that is neither a single character nor one
of nvim's own (`Left`, `Right`, `Up`, `Down`, `Home`, `End`, `PageUp`,
`PageDown`, `CR`, `Esc`, `Tab`, `BS`, `Del`, `Insert`, `Space`, `lt`,
`F1`...). So is a value view cannot read *as* keys at all: a value of any
other type, or more than two keys in one binding. Either leaves that one
action on its defaults and says so, and neither ever keeps your config
from loading.

What is still silent is a *well-formed* name this build never receives:
it is accepted and simply never pressed.

A chord's first key waits for exactly one more, and only inside the sidebar
you armed it in. Press something that finishes no binding and it is handled
as if you had pressed it alone, another resize key included, so a doubled
`<C-w>` is still waiting on the same follower.

A width holds between 15% and 70% and lasts the session. The width a
session starts at is `view.toml`'s:

```toml
[native]
tree_width = 25            # percent of the terminal; 15..70, default 30

[ai]
panel_width = 40
```

Both are optional, and neither can fail your config: a whole number
outside the range opens at the nearest end, and a fractional or
non-numeric value opens at the default and tells you so.

With `placement = "windowed"` a sidebar's resize keys move nvim's own
window width, the same `<C-w>>`/`<C-w><` command it would answer directly.
The percent it moved to is what the session remembers, so the next
`<leader>e` or `<leader>ai` reopens it at that width. Under the default
`placement = "overlay"` there is no window to resize, so the keys still
step `tree_width`/`panel_width`, and a reopened float honours the new
number.

Two windowed surfaces pinned to the same edge are stacked in one nvim
column or row. Widening a stacked agent panel and the notification stream
beside it keeps the same width, whichever one held focus when you pressed
the key. A windowed palette stacked on that edge follows along too, though
every keystroke while it is open reaches nvim's own command line.

## Gaps and the placement ring

| key | does |
| --- | --- |
| `<leader>ug` | toggles `[ui] gaps` for the session |
| `<leader>uw` | steps every surface through the placement ring |

`<leader>ug` is a real nvim mapping (`:View ui gaps`), registered in normal
mode only: insert mode, a windowed surface's own key table and the composer
each bind the same keys first. It works only under `panes = "tiles"`: the
outer grid re-attaches at its new size and every open window is asked for a
fresh inner size, gapped or flush against its neighbours. See
[tiled-ui.md](tiled-ui.md#choosing-a-mode) for what a gap is.

`<leader>uw` is `:View ui cycle_surfaces`, and moves the tree, the agent
panel, the palette and the notification stream together, one ring position
at a time: `config` (what `view.toml` gave each of them) `->` `windowed`
`->` `overlay` `->` back to `config`. A surface open when the ring steps
moves with it: the tree keeps its cursor row, the agent panel keeps its
transcript. See [tiled-ui.md](tiled-ui.md#placing-a-surface) for what
each position looks like.

Rebind either one under `[keys]` with a single notation:

```toml
[keys]
toggle_gaps = "<leader>ug"     # the default
cycle_surfaces = "<leader>uw"  # the default
```

## Dismissing an error

An error or warning is sticky: it stays on screen until you have read it,
where an ordinary message fades on its own. Motions, insert mode and idle
time all leave it standing.

`<Esc>` in normal mode takes it down:

```vim
:bogus
" E492: Not an editor command: bogus   -- the toast, still there after 10j
" <Esc>                                -- gone
```

Nothing else changes: the key still reaches nvim exactly as it always did,
so a pending count or operator is cancelled the same way, and `<Esc>` with
no error showing does nothing new at all. In insert, visual or
operator-pending mode `<Esc>` only leaves the mode. Press it again from
normal mode to clear the error.

Dismissing takes the toast off the screen and keeps the record. Every
message view has shown, errors included, stays in the history:

```vim
:View notifications
```

which `<leader>fm` also opens.

`<Esc>` clears nvim's own errors and warnings, and only those. A notice
view raised itself about something it went and checked (a plugin drawing
over the command line, a file that stopped being readable) stays up while
that is still true. Those come down one at a time, with `d` in the history.

## The message history

The history overlay lists what view has said this session, newest first,
and scrolls:

| key | does |
| --- | --- |
| `j` | select the next entry |
| `k` | select the previous entry |
| `<C-d>` | select half a screen further down |
| `<C-u>` | select half a screen further up |
| `gg` | select the newest entry |
| `G` | select the oldest entry |
| `y` | copy the selected entry verbatim, to the system clipboard and over OSC 52 |
| `d` | take down the standing notice the selected entry belongs to |

`<Esc>` closes it.

`y` copies the selected line byte for byte. A path with a space in it arrives
with the space. It goes to your system clipboard and, in the same keystroke, out
as an OSC 52 escape, so a `view` running over SSH puts the line on the clipboard
of the machine you are reading it on:

```vim
:View notifications
" > view: file /home/tj/my notes/plan v2.md is no longer readable
" y     -- that line, exactly, on your clipboard
```

If there is no system clipboard to reach, view says so once and the copy
still goes to its own registers and out over OSC 52.

`d` takes down the notice the selected entry belongs to, wherever the
entry sits in the history, an older wording of a notice that has since
re-worded itself included. The entry stays: the history is the record of
what was said. On a message from nvim, which has no notice standing behind
it, `d` does nothing.

## `:View`

The command is registered whatever you have turned off, so a feature is
always reachable even with no keys at all:

```vim
:View picker files
:View picker grep
:View tree toggle
```

It completes both arguments against every entry point this build has.
