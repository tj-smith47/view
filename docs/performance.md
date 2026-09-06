# Performance

One section per moment you actually live through in the editor. Each says
what you feel, gives view and bare Neovim side by side on the same host in
the same run, and then says what makes it that number.

Nothing here is a number view took because it was easy to take. A segment
inside the input path, the frame view paints before your config has run, the
resident footprint -- those explain the moments below and never stand in for
one, so none of them appears as a headline. Everything they add up to is on
this page; the identifiers, the statistics and the machinery are in
[docs/benchmarking.md](benchmarking.md).

Two rules this page keeps, because breaking either is how benchmark pages
start lying: a moment measured under a config nobody runs is reported as not
yet measured, never filled in from a bench fixture; and a comparison is
paired -- view and bare Neovim launched in the same run on the same host,
samples interleaved -- or it is not a comparison.

## You open a project

You type `view ~/.config` and wait for the screen you can start working in:
the tree, the tabline and the statusline present and still.

| | view | Neovim | on |
|---|---|---|---|
| screen ready | not yet recorded | not yet recorded | your config (lazy.nvim, noice, nvim-notify), same host, same run |

What is known so far, and what it is not: with no plugins at all, that
screen arrives in 16.1 ms under view against 14.7 ms under Neovim -- view
1.4 ms behind, on a config nobody runs. Under a login-shaped config the
file's first line lands at 58.7 ms against 54.2 ms. Neither is the row
above, and neither is written into it.

What makes it that number: view paints its own shell -- the chrome you see
before anything has loaded -- in about 4 ms, and that frame is on screen
whether your config has zero plugins or forty. The rest is your `init.lua`,
which view does not make slower: the embedded engine reaches its own
"started" mark within 0.7 ms of the same engine under Neovim's own terminal
UI. The 1.4 ms above is view's attach and takeover, which happen after your
`VimEnter` runs rather than before it.

## You type

You press a key and the character appears.

| | view | Neovim | on |
|---|---|---|---|
| keypress to glyph, worst case in a thousand | not yet recorded | not yet recorded | your config, same host, same run |
| with the engine on the far side of a network | not yet recorded | not yet recorded | your config, same host, same run |

Under a plugin-free config, view's worst keystroke in a thousand takes
0.73 ms and Neovim's takes 0.67 ms; at the median view is about 13% behind.
Both are far under the ~10 ms where a person begins to notice a key lagging
their finger, which is why the gap is tracked rather than felt.

When the engine runs on another machine, view can show the character before
the round trip is back -- a predicted glyph, corrected the moment the engine
answers. Measured plugin-free, that puts the character on screen at 0.39x
the time bare Neovim takes locally, and it is the one place view is
decisively ahead rather than close.

What makes it that number: the key leaves your terminal and reaches Neovim
in under a tenth of a millisecond, and the redraw that comes back reaches
your screen in under a tenth of a millisecond. Of the round trip between
them, more than half is spent inside Neovim itself, where view's code does
not run.

## You scroll a big file

You hold a key down in a 100,000-line file and watch the text keep up.

| | view | Neovim | on |
|---|---|---|---|
| how stale the screen ever gets | not yet recorded | not yet recorded | your config, same host, same run |

Plugin-free, the screen is never more than 1.07 ms behind your input, and
with the 15-plugin bench stack never more than 1.23 ms -- against a 16 ms
budget, which is one frame at 60 Hz. Paired, view is 1.6 to 1.9x Neovim's
figure on the same run: both numbers are a fraction of a frame, so the ratio
is a bar view has not met rather than a lag you can see.

A plugin storm or a `:terminal` flood pouring output into the screen is the
same moment under load, and there the screen answers on a 14.6 ms cadence --
still inside one frame -- while keeping pace with the flood exactly.

## You search a huge tree

You open the picker and type; the matches are under your fingers.

| | view | Neovim | on |
|---|---|---|---|
| keystroke to matching results, 100k entries | 4.7 ms | n/a | bench fixture, worst case in a thousand |
| first page of results, 1M-file tree | 5.0 ms | n/a | bench fixture, worst case in a thousand |

Neovim ships no picker to pair against, so this moment has no second column
-- what makes it realistic is the size of the tree, not the plugin set. The
results stream while the scan is still running: there is no wait for a walk
to finish before the first page appears.

## The engine hangs

A plugin drives Neovim into a synchronous loop and the editor stops
answering. You get a banner naming the wedge instead of a frozen screen.

| | view | Neovim | on |
|---|---|---|---|
| hang to banner on screen | 11.6 s | n/a | bench fixture, worst case in a thousand |

Bare Neovim has no counterpart moment: when it wedges, nothing tells you.
The 11.6 s is not a target to admire, it is the sum of a 10 s wedge
threshold and one 2 s probe interval -- the earliest a probe fired just
before the engine stopped serving can possibly report back.

## Memory

view's own process holds 4.96 MB with no plugins loaded. It embeds a real
Neovim, so the honest number is the pair: view plus its engine child under
the 15-plugin stack is 27.96 MB, against 4.39 MB for bare Neovim alone.
view can never be smaller than the Neovim it embeds, and no row here says
otherwise.

---

How these were measured, what each cell is called, which of them are
diagnostics, and how a bar is re-seated: [docs/benchmarking.md](benchmarking.md).
