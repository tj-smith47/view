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

`<leader>ug` is a real nvim mapping (`:View ui gaps`) and answers wherever
the keyboard is aimed, working only under `panes = "tiles"`: the outer grid
re-attaches at its new size and every open window is asked for a fresh
inner size, gapped or flush against its neighbours. See
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
