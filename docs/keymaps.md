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
| `<leader>fp` | `notifications` | `:View notifications pause` |
| `<leader>ai` | `ai` | `:View ai toggle` |

## `<leader>ai` reads the panel before it acts

The AI panel is non-modal: `<Esc>` steps out of it and leaves it on screen
beside your buffer. So `<leader>ai` (`:View ai toggle`) is one verb over the
three states that gives you:

| the panel is | `<leader>ai` |
| --- | --- |
| closed | opens it and puts the cursor in the composer |
| open, and you are in it | closes it |
| open, and you are not in it | puts you back in it |

The third row is why the key never dead-ends: a panel you are not in -- one
you escaped out of, or one an agent's own permission request opened beside
you -- is one press from being yours, not one press from disappearing. Closing
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
editable one -- a letter that answered the prompt from there would mean two
things at once. See [ai.md](ai.md) for what each option does and for what
the two "always" answers stand for.

## Deciding an agent's proposed edit

A proposal is drawn in the file itself, and its keys are buffer-local nvim
mappings on the reviewed buffer -- set when the review opens, deleted when
it closes. They are not in `:map` before that and not there after, and they
take nothing from your config in between: the whole set lives under
`<leader>h` and on `]c`/`[c`, so no bare letter is claimed in a buffer that
stays editable throughout.

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
verb with no key: it decides everything in one press and offers no undo of
its own, so it is asked for by name.

Every verb is also a `:View` form, which is what to map if you want the
review on keys of your own -- a global mapping of yours is never touched, and
a buffer-local one on a key the table above claims (gitsigns puts one on
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
| `<M-CR>` | breaks the line -- works everywhere |
| `<S-CR>` | breaks the line -- needs the kitty keyboard protocol |
| `<CR>` | sends the prompt |

Both are bound because terminals disagree about Enter. Alt+Enter arrives as
`ESC` + Enter from nearly every one of them, so `<M-CR>` is the one to reach
for. A shifted Enter is distinguishable from a plain one only under the
kitty keyboard protocol, and where the terminal does not speak it both send
the same byte, so Shift+Enter *sends the prompt*.

What decides it is the startup capability probe's answer, not the tier.
Every `full`-tier terminal answers the kitty keyboard query -- the tier is
partly defined by it -- but a terminal can answer that query and still land
below `full` for an unrelated reason, and `<S-CR>` works there too. Over
ssh is the ordinary way to meet that: the tier also wants truecolor, which
takes a second question and a slower round trip, so a kitty-class terminal
can spend the first moments of a session on a lower tier with the keyboard
protocol already on.

| the probe's kitty keyboard answer | what view sends |
| --- | --- |
| yes | `CSI > 1 u` once the alternate screen is up, `CSI < u` before leaving it |
| no | `CSI < u` on the way out and nothing else -- a pop nothing pushed is ignored |

`--tier full` asserts all three capabilities instead of asking, so view
sends the push. That is an assertion and not a negotiation: a terminal that
does not speak the protocol ignores the push, and `<S-CR>` still will not
reach the composer. No flag can add a protocol to a terminal that lacks one.

The window view holds the protocol open for is the one nvim holds it open
for when you run nvim directly in kitty, ghostty or WezTerm. Every exit view
takes for itself pops it before leaving the alternate screen: quitting,
`:cq`, a panic, an error during startup, and the first
`SIGHUP`/`SIGTERM`/`SIGINT`, which view folds into its own teardown. Four
endings cannot pop it, because no view code runs at all -- a *second* fatal
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
with the same keys, 5% of the terminal per press:

| key | does |
| --- | --- |
| `<S-Right>` | one notch wider |
| `<C-w>>` | one notch wider |
| `<S-Left>` | one notch narrower |
| `<C-w><` | one notch narrower |

Direction reads the way `<C-w><` and `<C-w>>` do in nvim -- right widens,
left narrows -- whichever edge the sidebar is pinned to. Two bindings per
direction because macOS Terminal and Termius keep the shifted arrows for
themselves and view never sees them; the chord reaches through both, and it
is the one you already resize an nvim window with.

These are view's own keys inside its own surfaces, not nvim mappings, so
they take nothing from your config and appear in no `:map` listing -- but
they are yours to change. `[keys]` takes one key notation per action, or a
list of them, and a binding may be a two-key chord:

```toml
[keys]
sidebar_wider = ["<S-Right>", "<C-w>>"]     # the defaults
sidebar_narrower = ["<S-Left>", "<C-w><"]
```

The same rules hold for every action `[keys]` carries, the composer's line
break above included.

Spell a key the way nvim spells it, case and all: `<S-Right>`, not
`<S-right>`. A `<...>` notation that cannot be a key is reported -- a
modifier prefix that is not `S-`, `C-`, `M-` or `A-`, or a name that is
neither a single character nor one of nvim's own (`Left`, `Right`, `Up`,
`Down`, `Home`, `End`, `PageUp`, `PageDown`, `CR`, `Esc`, `Tab`, `BS`,
`Del`, `Insert`, `Space`, `lt`, `F1`...). So is a value view cannot read
*as* keys at all: not a string or a list of them, or more than two keys in
one binding. Either leaves that one action on its defaults and says so, and
neither ever keeps your config from loading.

What is still silent is a *well-formed* name this build never receives:
it is accepted and simply never pressed.

A chord's first key waits for exactly one more, and only inside the sidebar
you armed it in. Press something that finishes no binding and it is handled
as if you had pressed it alone -- including another resize key, so a doubled
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

`<Esc>` clears nvim's own errors and warnings, and only those. A notice
view raised itself about something it went and checked -- a plugin drawing
over the command line, a file that stopped being readable -- stays up while
that is still true, because nothing re-raises it once it is gone. Those
come down one at a time, with `d` in the history.

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

`y` copies the selected line byte for byte -- a path with a space in it
arrives with the space, and nothing is trimmed, quoted or reworded. It goes
to your system clipboard and, in the same keystroke, out as an OSC 52
escape, so a `view` running over SSH puts the line on the clipboard of the
machine you are reading it on:

```vim
:View notifications
" > view: file /home/tj/my notes/plan v2.md is no longer readable
" y     -- that line, exactly, on your clipboard
```

If there is no system clipboard to reach, view says so once and the copy
still goes to its own registers and out over OSC 52.

`d` takes down the notice the selected entry belongs to, wherever the
entry sits in the history -- including an older wording of a notice that
has since re-worded itself. It does not delete the entry: the history is
the record of what was said, and that stays true whether or not the line
is still on screen. On a message from nvim, which has no notice standing
behind it, `d` does nothing.

## `:View`

The command is registered whatever you have turned off, so a feature is
always reachable even with no keys at all:

```vim
:View picker files
:View picker grep
:View tree toggle
```

It completes both arguments against every entry point this build has.
