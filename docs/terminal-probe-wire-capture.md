# Wire capture: the terminal capability probe's queries and replies

Captured live against real emulators per "capture, never recall." Source of
truth for every escape sequence `crates/view-tui/src/tiers.rs` writes at
startup and for every reply shape `scan_replies` decodes, so no wire string
in the detection path rests on recall.

The wire strings this doc traces:

| Constant | Bytes | Traced by |
|---|---|---|
| `QUERY_SYNC` | `\x1b[?2026$p` | every capture below |
| `QUERY_KITTY` | `\x1b[?u` | every capture below |
| `QUERY_TRUECOLOR` | `\x1b[48;2;1;2;3m\x1bP$qm\x1b\\\x1b[0m` | every capture below |
| `QUERY_DA1_FENCE` | `\x1b[c` | every capture below |
| `QUERY_BOX_GLYPH` | `\r` `╭` `\x1b[6n` `\r\x1b[K` | every capture below |

Captured before any code sent it: the batch above was the question the
`unicode_boxes` capability was owed, and `QUERY_BOX_GLYPH` is that question
as `Probe::start` now writes it.
`writes_the_query_batch_before_reading_any_reply` asserts the whole batch
against the `sent:` line below, byte for byte, so a query that drifts from
these captures fails rather than decoding answers to a question nobody
asked.

## Capture method

`scripts/capture-terminal-probe.sh` writes the batch and dumps what came
back, hex and escaped, beside the terminal's identity. It takes the
terminal into raw mode *before* the queries go out (canonical mode holds a
reply that carries no newline, which is every reply here) and bounds the read
with `VMIN=0`/`VTIME`, so a terminal that answers nothing ends the window on
the timer rather than blocking. It sources nothing and builds nothing,
because it is copied to hosts that have no clone of this repo.

```
$ sh scripts/capture-terminal-probe.sh probe ~/probe-out.txt
$ sh scripts/capture-terminal-probe.sh keys  ~/probe-key.txt
```

The batch is written in one `write`, in `Probe::start`'s order, with the
box-glyph probe inserted ahead of the DA1 fence:

```
\x1b[?2026$p \x1b[?u \x1b[48;2;1;2;3m\x1bP$qm\x1b\\\x1b[0m \r╭\x1b[6n\r\x1b[K \x1b[c
```

The emulators were reached headlessly:

| Emulator | How it was driven |
|---|---|
| kitty 0.45.0 | `xvfb-run -a kitty --config NONE -o close_on_child_death=yes -- sh -c '<harness>'` |
| tmux 3.6 | the same kitty, running `tmux -f /dev/null new-session '<harness>'` |
| GNU screen 4.09.01 | the same kitty, running `screen -c <rc> sh -c '<harness>'` |
| mbp shell | the same kitty, running `ssh -tt mbp "zsh -lc 'sh ~/viewcap.sh …'"` |
| Windows ConPTY | `ssh -tt winserver 'powershell -NoProfile -File probe.ps1'` |

Keypresses were produced by the emulator's own key encoder, never typed by
hand and never spelled out: `kitten @ send-key shift+f3` for kitty,
`tmux send-keys S-F3` for tmux. What the harness read is what the emulator
would have sent a real finger.

Windows has no `sh`; its capture used the equivalent PowerShell, reproduced
verbatim in the Windows section below.

## Host and terminal matrix

| # | Host | Terminal | `TERM` | `COLORTERM` |
|---|---|---|---|---|
| A | dev-linux (`apps`, Linux 7.0.0-30) | kitty 0.45.0 | `xterm-kitty` | `truecolor` |
| B | dev-linux | tmux 3.6 inside A | `tmux-256color` | `truecolor` |
| C | dev-linux | tmux 3.6 inside A, `terminal-features ",xterm-kitty:RGB"` | `tmux-256color` | `truecolor` |
| D | dev-linux | GNU screen 4.09.01 inside A, UTF-8 | `screen` | `truecolor` |
| E | dev-linux | GNU screen 4.09.01 inside A, `defutf8 off`, `LANG=C` | `screen` | `truecolor` |
| F | mbp (Darwin 25.2.0, macOS 26.2) | kitty 0.45.0 over `ssh -tt` | `xterm-kitty` | *unset* |
| G | mbp | tmux 3.6a inside F | `tmux-256color` | `truecolor` |
| H | winserver (Windows Server 2025, 10.0.26100) | ConPTY / `conhost.exe` over OpenSSH | `xterm-256color` | *unset* |

Recorded unavailable, never assumed:

| Terminal | Why |
|---|---|
| xterm on dev-linux | not installed (`command -v xterm` empty); its modified-function-key encoding is covered by B/G, which emit the identical bytes |
| Terminal.app on mbp | no interactive Aqua session to drive. `osascript -e 'tell application "Terminal" to count windows'` returns `31:44: execution error: Terminal got an error: AppleEvent timed out. (-1712)` |
| Termius from the iPad | no interactive iPad session reachable from a headless run. F is the same shape: an `ssh` login whose client does not forward `COLORTERM` |
| Windows Terminal (`wt.exe`) | no interactive desktop session on winserver; `WT_SESSION` unset. H captures the ConPTY underneath it, which is the layer that answers |

## A. kitty 0.45.0, dev-linux

```
sent:     \x1b[?2026$p\x1b[?u\x1b[48;2;1;2;3m\x1bP$qm\x1b\\\x1b[0m\r\xe2\x95\xad\x1b[6n\r\x1b[K\x1b[c
received: \x1b[?2026;2$y\x1b[?0u\x1bP1$r0;48:2:1:2:3m\x1b\\\x1b[1;2R\x1b[?62;52;c
hex:      1b 5b 3f 32 30 32 36 3b 32 24 79 1b 5b 3f 30 75 1b 50 31 24 72 30 3b
          34 38 3a 32 3a 31 3a 32 3a 33 6d 1b 5c 1b 5b 31 3b 32 52 1b 5b 3f 36
          32 3b 35 32 3b 63
```

Read as:

| Reply | Bytes | Means |
|---|---|---|
| DECRPM | `\x1b[?2026;2$y` | `Pd=2026`, `Pm=2` (set, currently reset) -- `is_sync_supported` accepts 1 and 2, so `sync = true` |
| kitty flags | `\x1b[?0u` | flags 0 -- the protocol is present and nothing is pushed yet; `kitty_kbd = true` |
| DECRQSS | `\x1bP1$r0;48:2:1:2:3m\x1b\\` | valid (`1$r`), whole SGR state echoed, **colon** separators, triple preserved -- `truecolor = true` |
| CPR | `\x1b[1;2R` | cursor at row 1, column 2: `╭` advanced exactly one cell |
| DA1 | `\x1b[?62;52;c` | class 62, and it is the last reply on the wire |

## B and C. tmux 3.6 inside kitty, dev-linux

Identical bytes with and without `set -as terminal-features ",xterm-kitty:RGB"`:

```
sent:     (as above)
received: \x1bP0$r\x1b\\\x1b[1;2R\x1b[?1;2;4c
hex:      1b 50 30 24 72 1b 5c 1b 5b 31 3b 32 52 1b 5b 3f 31 3b 32 3b 34 63
```

Read as:

| Reply | Bytes | Means |
|---|---|---|
| DECRPM | *absent* | `sync = false` |
| kitty flags | *absent* | `kitty_kbd = false` |
| DECRQSS | `\x1bP0$r\x1b\\` | `0$r` -- the terminal calls the request invalid. `truecolor_from_decrqss` requires the `1$r` prefix, so the probe answers nothing here |
| CPR | `\x1b[1;2R` | `╭` advanced one cell |
| DA1 | `\x1b[?1;2;4c` | class 1, last on the wire |

tmux declines the truecolor readback whether or not it is told the outer
terminal is RGB-capable, and it renders 24-bit color in both cases. Inside
tmux the probe cannot establish truecolor and `COLORTERM` is the only signal
left, which is why `0$r` is read as *no answer* rather than as an answer of
"no": the hint decides only where the probe was silent, and a declined
request is silence about the color. An explicit answer -- `1$r` echoing a
quantized setting -- is a different fact and outranks the hint.

## D. GNU screen 4.09.01 inside kitty, UTF-8

```
received: \x1b[1;2R\x1b[?1;2c
hex:      1b 5b 31 3b 32 52 1b 5b 3f 31 3b 32 63
```

screen ignores DECRQSS entirely: no DCS comes back at all, not even a `0$r`
rejection. It also answers neither DECRPM nor the kitty query. Only CPR and
the DA1 fence arrive, and the fence is still last.

## E. GNU screen, `defutf8 off`, `LANG=C`

```
received: \x1b[1;3R\x1b[?1;2c
hex:      1b 5b 31 3b 33 52 1b 5b 3f 31 3b 32 63
```

The one capture whose CPR column differs. The same `\r` `╭` `\x1b[6n`
sequence lands the cursor at **column 3** instead of column 2: a terminal not
in UTF-8 mode does not draw one box-drawing cell for those three bytes, and
the column it reports says so. This is the discrimination the box-glyph
probe exists for, and it is the only signal in this whole matrix that
separates E from D -- `TERM`, `COLORTERM` and every other reply are identical
between them.

### What D and E prove, and what they do not

The box-glyph probe is proven here on **one axis only: the locale**. E is a
terminal taken out of UTF-8 mode, and that is the whole of the difference
between the two captures. Every other emulator in this matrix, on all three
operating systems, advanced `╭` by exactly one cell while in UTF-8 mode, so
the population contains exactly one positive and one negative and they differ
by `defutf8`/`LANG` alone.

Unobserved, and not to be claimed on this evidence:

- a UTF-8 terminal whose **font** lacks the glyph. None was captured; a
  tofu box or a fallback glyph may still advance one cell, in which case the
  CPR column says "fine" about a frame the user cannot read.
- a terminal whose **wcwidth** disagrees, drawing the glyph double-width. No
  capture here reports a column past 3.
- the emulators recorded unavailable above (Terminal.app, Windows Terminal,
  Termius), none of which contributed a box-glyph column.

So the probe discriminates -- it is not a query every terminal answers alike
-- but what it is *shown* to detect is a terminal not decoding UTF-8, not the
general question "can this terminal draw a rounded border." A consumer that
treats a column of 2 as proof the border will render is extrapolating past
this capture set.

## F. mbp over ssh, terminal is kitty on dev-linux

```
received: \x1b[?2026;2$y\x1b[?0u\x1bP1$r0;48:2:1:2:3m\x1b\\\x1b[1;2R\x1b[?62;52;c
```

Byte-identical to A, from a shell on a different machine and a different
operating system, with **`COLORTERM` unset**: `ssh` does not forward it. The
readback is the only thing on this login that knows the terminal renders
24-bit color, which is exactly the case `QUERY_TRUECOLOR` was added for.

## G. tmux 3.6a on macOS inside F

```
received: \x1bP0$r\x1b\\\x1b[1;2R\x1b[?1;2;4c
```

Byte-identical to B, and the mirror image of F: here `COLORTERM=truecolor` is
set (tmux inherits the login environment) while the terminal answers that it
did *not* keep the 24-bit background. Environment and readback disagree in
opposite directions on the same machine, one hop apart.

## H. Windows ConPTY over OpenSSH, winserver

PowerShell has no `stty`, so the equivalent harness reads decoded console
keys against a quiet-window stopwatch:

```powershell
$E = [char]27
$batch = $E+'[?2026$p' + $E+'[?u' + $E+'[48;2;1;2;3m' + $E+'P$qm' + $E+'\' +
         $E+'[0m' + "`r" + [char]0x256D + $E+'[6n' + "`r" + $E+'[K' + $E+'[c'
[Console]::Out.Write($batch); [Console]::Out.Flush()
$bytes = New-Object System.Collections.Generic.List[int]
$sw = [Diagnostics.Stopwatch]::StartNew()
while ($sw.ElapsedMilliseconds -lt 1500) {
  if ([Console]::KeyAvailable) {
    $k = [Console]::ReadKey($true); $bytes.Add([int]$k.KeyChar); $sw.Restart()
  }
}
```

```
received: \x1b[?2026;0$y\x1bP1$r0;48:2::1:2:3m\x1b\\\x1b[7;2R\x1b[?61;6;7;21;22;23;24;28;32;42c
hex:      1b 5b 3f 32 30 32 36 3b 30 24 79 1b 50 31 24 72 30 3b 34 38 3a 32 3a
          3a 31 3a 32 3a 33 6d 1b 5c 1b 5b 37 3b 32 52 1b 5b 3f 36 31 3b 36 3b
          37 3b 32 31 3b 32 32 3b 32 33 3b 32 34 3b 32 38 3b 33 32 3b 34 32 63
```

Read as:

| Reply | Bytes | Means |
|---|---|---|
| DECRPM | `\x1b[?2026;0$y` | `Pm=0` -- the mode is *not recognized*. `is_sync_supported` accepts only 1 and 2, so `sync = false`, correctly |
| kitty flags | *absent* | `\x1b[?u` is not understood; ConPTY passed it through to the screen, where it printed as `[?u` |
| DECRQSS | `\x1bP1$r0;48:2::1:2:3m\x1b\\` | valid, colon-separated, and carrying an **empty colour-space-id field**: `48:2::1:2:3`, the ITU-T T.416 spelling |
| CPR | `\x1b[7;2R` | row 7 (the harness had already printed seven lines), column 2 -- `╭` advanced one cell |
| DA1 | `\x1b[?61;...;42c` | class 61, last on the wire |

`48:2::1:2:3` is the separator detail a plan cannot invent, and it is not the
form the shipped parser accepts. `truecolor_from_decrqss` splits on `;` and
`:` and looks for the five-element run `48, 2, 1, 2, 3`; this reply splits to
`0, 48, 2, "", 1, 2, 3`, whose every five-element window carries the empty
field, so a terminal that *does* render 24-bit color is read as one that does
not. Windows sets no `COLORTERM` either, so nothing else recovers it. Any
consumer of this capture owes the empty-field spelling an accepted form.

## What a terminal that does not support this answers

Four distinct negatives are on the wire above, and they are not
interchangeable:

| Shape | Seen on | What it proves |
|---|---|---|
| no reply at all | DECRPM and kitty on B/C/D/E; kitty on H | the query was swallowed. Proves nothing about the capability, only that the terminal will not say |
| `\x1bP0$r\x1b\\` | DECRQSS on B/C/G | an explicit "invalid request": the terminal parsed the readback and declined it |
| `\x1b[?2026;0$y` | DECRPM on H | an explicit "mode not recognized", `Pm=0`, distinct from `Pm=1`/`Pm=2` |
| a *different* CPR column | E versus D | the query was answered and the answer differs: the glyph did not occupy one cell |

The probe discriminates in this population: A/F answer DECRQSS affirmatively
with the triple intact, B/C/G answer it negatively, D/E ignore it, and the
box-glyph column separates E from D where nothing else does. A capture set in
which every terminal answered alike would not be evidence, and this one is
not that. The box-glyph half of that claim is bounded to the locale axis --
see "What D and E prove, and what they do not" above for what stays
unobserved.

## The DA1 fence answers last

Every capture ends with the DA1 reply, on all five terminals and all three
operating systems, including the two where most of the batch went
unanswered. The early-break on `da1` is safe against this population: nothing
arrives behind the fence.

## A keypress that is a CPR reply

The residue contract rests on "a keyboard cannot produce `\x1b[?`", which
holds for the private-CSI and DCS arms. CPR has no such protection: its
grammar `\x1b[Pr;PcR` is also what a modified `F3` looks like. Captured
side by side:

| Source | Bytes | Emulator |
|---|---|---|
| CPR, cursor at row 1 column 2 | `\x1b[1;2R` | A, B, C, D, F, G |
| `Shift-F3` | `\x1b[1;2R` | B (dev-linux tmux), G (mbp tmux) |
| `Ctrl-F3` | `\x1b[1;5R` | B, G |
| `F3` unmodified | `\x1bOR` | A, B, G |
| `Shift-F3` | `\x1b[13;2~` | A (kitty) |
| `Ctrl-F3` | `\x1b[13;5~` | A (kitty) |

`Shift-F3` under tmux and a CPR reply from a cursor on row 1 column 2 are the
same six bytes. Not similar: identical, on both hosts, verbatim. No parser
can separate them from the byte stream alone.

What does separate them is where they arrive. Two facts from these captures
bound the ambiguity:

- The collision needs `Pr` to be the modifier-encoding row, which is always
  `1`. H's CPR at `\x1b[7;2R` collides with nothing, because no function key
  encodes row 7. The ambiguity is confined to a cursor parked on row 1.
- kitty does not encode modified `F3` in the CPR grammar at all
  (`\x1b[13;2~`), so the collision is a property of the terminal's key
  encoding, not of CPR.

The honest reading for anything decoding this: a `\x1b[1;PcR` arriving inside
a probe window that asked a CPR question is the CPR answer; the same bytes
outside that window are a keypress. An absolute claim that a keyboard cannot
produce this shape is false, and these captures are why.

## What the captures oblige

1. `48:2::1:2:3` must parse as truecolor. The empty colour-space-id field is
   what a current Windows ConPTY sends, and the `truecolor_from_decrqss`
   shipped when this was captured rejected it. It now drops empty fields
   before matching, and capture H's reply is a row of that function's table
   test.
2. The box-glyph probe's signal is the CPR **column**, not its presence: D
   and E both answer, and only the column differs. What it is shown to
   detect is a terminal not decoding UTF-8; a font gap and a double-width
   wcwidth are unobserved here, so neither may be claimed from a column of
   2.
3. `Pm=0` on DECRPM is a real answer meaning unsupported, not a missing one.
   Treating "no reply" and `;0$y` alike loses the distinction H provides.
4. Nothing arrives after the DA1 fence, so it remains usable as one.
5. `\x1b[1;2R` is not unambiguously a CPR reply. Anything asserting it is
   contradicts B and G.
