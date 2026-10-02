# Performance

One section per moment you live through in the editor. Each says what you
feel, gives view and bare Neovim side by side, and then says what makes it
that number. The identifiers, the statistics and the machinery are in
[docs/benchmarking.md](benchmarking.md).

Every reading below was recorded on a shared Linux dev host, whose
`dev-linux` is the default class of this page.

## You open a project

You type `view ~/.config` and wait for the screen you can start working in:
the tree, the tabline and the statusline present and still.

| | view | Neovim | on |
|---|---|---|---|
| screen ready | 54.3 ms | 51.9 ms | your config (lazy.nvim, noice, nvim-notify), same host, same run |

Under your config view is 2.4 ms behind on screen ready, against a bar level
with Neovim, so this row is a bar view has not met, by 4.7%. At the worst launch
in a thousand under your config the two are within two milliseconds of each
other (71.7 ms against 70.0 ms). With no plugins at all screen ready is 7.9%
behind, which is a bench fixture reading.

What makes it that number: view paints its own shell, the chrome you see
before anything has loaded, in about 5 ms, and that frame is on screen
whether your config has zero plugins or forty. The rest is your `init.lua`.
view used to wait out your whole first screen before attaching to the engine
and taking the surfaces over, and then redraw everything that screen had
already painted; it now attaches while your `init.lua` is still running. What
is left of that gap is the screen the attach asks for, which travels to view
over the wire and is painted again by view, where Neovim's own terminal UI
reads it out of the same process. That attach is inside the engine's own
startup now, so the engine's "started" mark lands 2.63 ms later under view.

## You type

You press a key and the character appears.

| | view | Neovim | on |
|---|---|---|---|
| keypress to glyph, worst case in a thousand | 1.46 ms | 1.20 ms | your config, same host, same run |
| the same keypress, with view drawing the glyph it expects | 0.35 ms | 1.22 ms | your config, same host, same run |

At the median, under your config, keypress to glyph is 20% behind, against a
bar of 10%: another bar view has not met, and by 10% of the round trip.
Plugin-free, the same worst keystroke is 1.02 ms against 0.85 ms.

view does not have to wait for the engine to answer before it draws. It puts the
character it expects on screen and corrects it the moment the engine's redraw
arrives, so under your config the glyph is there in 0.35 ms at the worst of a
thousand keystrokes, beside Neovim's 1.22 ms to paint the same character. A
prediction answered all but one of the keystrokes measured. That is the reading,
not a network; the network case is the acceptance leg below, measured by
`scripts/acceptance/remote-rtt.sh`.

The network is what the prediction is for, and it is measured on its own:
an acceptance leg puts 0, 25, 100 and 300 ms of round trip in front of
view's engine on a plugin-free fixture and holds the predicted glyph to the
same bound at every tier (`scripts/acceptance/remote-rtt.sh`, which runs
only on a host declared quiet enough for it).

What makes it that number: the key leaves your terminal and reaches Neovim
in under a tenth of a millisecond, and the redraw that comes back reaches
your screen in under a tenth of a millisecond. Of the round trip between
them, more than half is spent inside Neovim itself.

## You scroll a big file

You hold a key down in a 100,000-line file and watch the text keep up.

| | view | Neovim | on |
|---|---|---|---|
| how stale the screen ever gets | 1.61 ms | 1.01 ms | your config, same host, same run |

Both are inside the 16 ms budget, which is one frame at 60 Hz, and view's
staleness is the larger of the two: 0.6 ms more of it. Plugin-free the same
staleness is 1.39 ms.

A plugin storm or a `:terminal` flood pouring output into the screen is the same
moment under load. Under your config the screen answers on a 16.95 ms cadence,
just past one frame, so this too is a bar view has not met, and Neovim answers
on a 17.35 ms one. On that same run the longest view goes without painting is
44.3 ms, and view drains the flood at the pace Neovim holds. Plugin-free the
cadence is 15.0 ms, inside the frame. Neovim refreshes a terminal buffer on a
fixed 10 ms timer, and neither side can paint more often than that timer plus
one redraw.

## You search a huge tree

You open the picker and type; the matches are under your fingers.

| | view | Neovim | on |
|---|---|---|---|
| keystroke to matching results, 100k entries | 4.8 ms | n/a | bench fixture, worst case in a thousand |
| first page of results, 1M-file tree | 2.6 ms | n/a | bench fixture, worst case in a thousand |

Neovim ships no picker, so this moment has no second column. The results
stream while the scan is still running, so the first page is there before
the walk finishes.

## The engine hangs

A plugin drives Neovim into a synchronous loop and the editor stops
answering. You get a banner naming the wedge:

```
nvim has not answered for 14s
<F5> restart   <C-c> interrupt
```

The seconds count up while the wedge stands. The count can trail the real
wait by up to 2 s. `<F5>` restarts the engine whenever the banner is up. The
restart reopens every file you had listed, each with the unsaved text its swap
file held, and leaves you in the one you were editing. Recovering unsaved
text needs swap files on, which is Neovim's default. A notice names each file
the restart recovered, and each file it reopened from disk because its
unsaved text had no swap file. Your window layout starts fresh. When the
connection itself is gone, the second line offers `<C-q>` to quit.

| | view | Neovim | on |
|---|---|---|---|
| hang to banner on screen | 11.7 s | n/a | bench fixture, worst case in a thousand |

Bare Neovim has no counterpart moment: when it wedges, nothing tells you.
The 11.7 s is the sum of a 10 s wedge threshold and one 2 s probe
interval, the earliest a probe fired just before the engine stopped
serving can possibly report back.

## Memory

view's own process holds 8.51 MB with no plugins loaded. view plus its
engine child under the 15-plugin stack is 27.96 MB, against 4.39 MB for bare
Neovim alone.

---

How these were measured, what each cell is called, and how a bar is
re-seated: [docs/benchmarking.md](benchmarking.md).
