# Wire capture: ConPTY under the harness

Captured live: one capture program, run on a real Windows host through ConPTY
and on this tree's Linux host through a unix pty, driving the same corpus
script against the same pinned engine. Source of truth for what a Windows
harness leg must do differently; which repaints ConPTY injects, how a resize
is reported, whether exit codes survive, what clock the host offers, and what a
per-child kill leaves behind.

The headline needs no softening. Through ConPTY, **nothing whatsoever reaches
the master until the master answers a cursor-position request**. Every figure
below sits downstream of that.

> Answered since 2026-09-06: `QueryPolicy`'s responder replies `\x1b[1;1R` to a
> bare `\x1b[6n` under every answering policy. The unix arm's stream below was
> captured before that (and before 6b89c29, which answered the OSC 11
> background query and the DSR behind it), so a re-capture that differs from
> the bytes published here is expected; that gap is not itself a finding. the
> `E1568` warning is gone from the unix stream, and the ConPTY arm is expected
> to stop hanging to its deadline. Neither re-capture has been run; the Windows
> half is a winserver run and the host is unreachable.

## Hosts and engine identity

| | ConPTY arm | unix pty arm |
|---|---|---|
| host | winserver (Windows, x86_64, 2 cpus) | dev-linux (x86_64, 12 cpus) |
| engine | `nvim-win64\bin\nvim.exe` | `/home/linuxbrew/.linuxbrew/bin/nvim` |
| reported | `NVIM v0.12.4` | `NVIM v0.12.4` |
| pin | `.engine-pin` `v0.12.4`, no drift | same |
| capture source | `fnv1a-64 7bf5482143b17c5f over 24475 bytes` | identical |

The source fingerprint is printed by the capture itself, so the two arms are
one program: describing them as two that merely resemble each other would be
wrong. It fingerprints the program as it ran: editing the script moves the
value on both arms together, and the figure asserts only that the arms agree; a
repeat of this particular hex is beside the point.

## Capture method

`scripts/acceptance/capture-conpty.ps1` carries a small Rust program between
sentinel comments, writes it into a scratch crate on the capture host, and runs
it. The crate depends on `view-oracle` by path and drives `PtySession`, so the
child gets the same hermetic environment every editor process in this tree gets
(all four standard-path roots, which on Windows is the difference between an
isolated child and the operator's own configuration). The unix arm extracts the
identical source with the awk commands in the script's own header.

```
$ scp scripts/acceptance/capture-conpty.ps1 winserver:capture-conpty.ps1
$ ssh winserver 'powershell -NoProfile -File "$HOME\capture-conpty.ps1"
    -Repo "$HOME\view-t2" -Nvim "$HOME\nvim-win64\bin\nvim.exe"
    -Out "$HOME\conpty-capture" -Cols 80 -Rows 24 -Label conpty'
```

What is driven: bare `nvim --clean` in an 80x24 pty, `TERM` pinned to
`xterm-256color` and `COLORTERM` to `truecolor`, capability queries answered as
a full-tier terminal would answer them (`QueryPolicy::AnswerFullTier`),
replaying `corpus/insert-basic.toml`'s script `ihello world<Esc>0x` as the
bytes `ihello world\x1b0x`. That entry is the shortest in the corpus, and the
corpus drivers send key notation over RPC while a pty takes bytes, so the
translation is by hand.

Artifacts, beside this file, each holding the full escaped and hex stream of
every segment plus both grid resolutions:

- `conpty-harness-wire-capture.conpty-80x24.txt`
- `conpty-harness-wire-capture.unixpty-80x24.txt`
- `conpty-harness-wire-capture.conpty-unanswered-80x24.txt`

## 1. ConPTY withholds every byte until a cursor-position request is answered

The first thing the ConPTY master sees is a bare four-byte request, and with
nothing answering it that is also the last thing it sees. From
`conpty-unanswered-80x24.txt`:

```
== startup stream
  painted:  false
  cpr answered: 0
  bytes:    4
  escaped:
    \x1b[6n
  hex:
    1b 5b 36 6e
```

Everything downstream of that in the run is empty: the replayed script
produces 0 bytes, the resize produces 0 bytes, the parsed screen has 0 lines,
`:cq 3` never exits, `:terminal` never spawns anything. The run spends every
deadline it has and reports nothing.

Answering `\x1b[6n` with `\x1b[1;1R` unblocks all of it. Same host, same size,
same script, from `conpty-80x24.txt`:

```
== startup stream
  painted:  true
  cpr answered: 1
  bytes:    5690
  escaped:
    \x1b[6n\x1b[?9001h\x1b[?1004h\x1b[m\x1b]0;C:\\Users\\Administrator\\
    nvim-win64\\bin\\nvim.exe\x07\x1b[?25h\x1b[?1049h\x1b[22;0;0t\x1b[2J
    \x1b[?2004h\x1b[?u\x1b[?25l\x1b[K\x0d\x0a\x1b[K\x0d\x0a...
```

The unix arm never asks at all (`cpr answered: 0` in both the answering and the
non-answering runs, with byte-identical figures either way), and its first
bytes are the engine's own probe batch:

```
== startup stream
  painted:  true
  cpr answered: 0
  bytes:    12513
  escaped:
    \x1b[?1049h\x1b[22;0;0t\x1b[?1h\x1b=\x1b[H\x1b[2J\x1b[?2004h\x1b[?69
    $p\x1b[?2026$p\x1b[?2027$p\x1b[?2031$p\x1b[?2048$p\x1b[0m\x1b[4:3m\x
    1bP$qm\x1b\\\x1b[?u\x1b[c\x1b]11;?\x07\x1b[5n...
```

**The request belongs to the pseudoconsole; the editor plays no part in it.**
The same pty running `cmd.exe /c echo hi` carries it too, and stalls on it
identically: with the request unanswered that session produces 4 bytes and
never exits; answered, it produces 65 bytes and exits 0. The unix control
(`/bin/sh -c echo hi`) produces 4 bytes (`hi` and a line break), carries no
request, and exits 0. So this is a property of the pseudoconsole, and it
applies to **every** child a Windows leg spawns, nvim included.

`QueryPolicy`'s responder answers eight queries: the device-attributes fence,
four optional capability queries (`\x1b[?2026$p`, `\x1b[?u`, the SGR readback,
the box-glyph probe), and three a real terminal answers whatever its
capabilities are; the OSC 11 background query, the DSR behind it, and since
2026-09-06 a bare `\x1b[6n`. The capture above was taken while that last one
went unanswered, which is what left a Windows pty leg hanging until whatever
deadline it was given and reporting an empty screen.

## 2. What the parser resolves from each stream

With the handshake answered, the two streams differ in almost every byte and
resolve to **the same cells** and **a different `contents()` **.

Cells, compared row by row across the full grid at both checkpoints:

| checkpoint | rows | cell grids equal |
|---|---|---|
| after the script (80x24) | 24 vs 24 | yes, byte for byte |
| after the resize (100x30) | 30 vs 30 | yes, byte for byte |

`contents()`; the string every screen assertion in this tree reads through
(`PtySession::screen`, `wait_for`, `wait_for_screen`); does not agree:

| checkpoint | ConPTY | unix pty |
|---|---|---|
| after the script (80x24) | `contents lines: 1` | `contents lines: 24` |
| after the resize (100x30) | `contents lines: 23` | `contents lines: 29` |
| after the script (100x30 run) | `contents lines: 1` | `contents lines: 30` |

The ConPTY screen resolves to one logical line holding every row end to end:

```
    |ello world                        ...        ~   ...   [No Name] [+]
```

against the unix arm's 24 separate rows starting `|ello world`. The mechanism
is visible in the streams: ConPTY repaints by absolute cursor positioning and
pads each row through its final column, and `vt100` marks a row written through
its last column as wrapped and omits the newline after it. The engine on a unix
pty ends rows with `\x0d\x0a` and does not fill the final column.

So the falsifiable answer is a concrete ConPTY-only divergence: **the identical
script resolves to the identical cell grid and to a different parsed text**. An
assertion that reads cells is safe on Windows. An assertion that reads
`contents()` line structure; a needle spanning a row boundary, a line count, a
row-indexed split; is not.

## 3. Resize

ConPTY announces the resize on the wire before repainting, with a sequence the
engine never writes. First bytes of the resize segment, 80x24 to 100x30:

```
ConPTY:  \x1b[?25l\x1b[8;30;100t\x1b[m\x1b[38;2;224;226;234m...
unix:    \x1b[?2026h\x1b(B\x1b[m\x1b[38;2;224;226;234m\x1b[48;2;20;22;27m
         \x1b[H\x1b[2J\x1b[22B...
```

`\x1b[8;<rows>;<cols>t` occurs exactly once in the whole ConPTY capture, in the
resize segment, with the new size in it (`\x1b[8;36;120t` in the 100x30 run,
resized to 120x36). It never occurs in the unix capture at all. An
engine-driven redraw of the same screen (`ctrl-l`) starts
`\x1b[m\x1b[?25l\x1b[2J...` on ConPTY and carries no window report.

**The resize repaint is therefore distinguishable from an engine-driven redraw,
by the `\x1b[8;<rows>;<cols>t` prefix and by nothing else.** Byte volume alone
does not separate them reliably, and latency does not separate them at all:

| | ConPTY resize | ConPTY ctrl-l | unix resize | unix ctrl-l |
|---|---|---|---|---|
| bytes (80x24 run) | 3541 | 504 | 1212 | 522 |
| bytes (100x30 run) | 4917 | 580 | 1408 | 598 |
| first byte (80x24) | 1.22 ms | 7.10 ms | 1.01 ms | 0.90 ms |
| last byte (80x24) | 11.76 ms | 15.21 ms | 1.01 ms | 0.90 ms |
| first byte (100x30) | 1.62 ms | 1.11 ms | 1.19 ms | 0.97 ms |
| last byte (100x30) | 18.29 ms | 20.26 ms | 1.19 ms | 0.97 ms |

The last row is the figure a sampling boundary has to respect. On a unix pty
the whole repaint lands inside one 200us sample: first byte and last byte are
the same instant. Under ConPTY the same repaint arrives in several chunks
spread over 11 to 21 ms, whether it was provoked by a resize or by a redraw. A
Windows sample that ends at "the first byte of the frame" measures something 10
to 19 ms shorter than one that ends at the last.

## 4. Exit codes

`:cq 3` through ConPTY, observed at the parent:

```
== exit code
  sent:        :cq 3<CR>
  exit_code(): 3
  success():   false
```

Identical on the unix arm. Exit-code propagation holds on Windows, so spec
section 5.6's contract is met there. The one caveat is upstream: with the
cursor-position request unanswered the child never reaches `:cq` at all, and
`wait_for_exit` reports the child never exited.

## 5. Clock

`Instant`, measured as the smallest non-zero difference between consecutive
readings over 200000 rounds (an upper bound on the true tick, since the loop's
own cost sits inside it):

| | ConPTY host | unix host |
|---|---|---|
| smallest non-zero | 100 ns | 18 ns |
| identical consecutive pairs | 141606 of 200000 | 0 of 200000 |
| mean pair cost | 32 ns | 20 ns |

Windows resolves to exactly 100 ns and reports the same value for around three
quarters of consecutive reads, which is what a 100 ns tick read faster than it
advances looks like. That is four orders of magnitude below the
millisecond-scale boundaries the non-tap bench cells measure (`echo`, `scroll`,
`first_paint`, `flood`, `picker`), so the constraint on Windows is the ConPTY
delivery spread in section 3; the clock plays no part.

## 6. Teardown

`:terminal` in the pty child, then a kill, then the process table:

| | ConPTY arm | unix arm |
|---|---|---|
| descendants observed | `nvim.exe`, `conhost.exe`, `powershell.exe` | `nvim`, `bash` |
| kill lever | per-child (`portable_pty::Child::kill`) | process group (`nix::sys::signal::killpg`) |
| editor status after kill | `code: 1, signal: None` | `code: 1, signal: Some("Killed")` |
| survivors | none | none |

The unix process-group kill is compiled out on Windows, and the per-child kill
still left nothing behind: the grandchildren were gone before the master was
closed. Windows teardown needs no group-kill equivalent on this evidence. The
check carries real weight and does real work: it enumerates the editor's
descendants by pid from the live process table before the kill and re-reads
that table after, and reaps by pid anything still standing.

## 7. Disconfirm

Every recorded figure moves when the input moves. Re-running the whole capture
at 100x30 (resizing to 120x36) changes the streams and the numbers on both
arms:

| figure | 80x24 run | 100x30 run |
|---|---|---|
| unix startup bytes | 12513 | 14018 |
| ConPTY resize bytes | 3541 | 4917 |
| unix resize bytes | 1212 | 1408 |
| ConPTY window report | `\x1b[8;30;100t` | `\x1b[8;36;120t` |
| unix contents lines after the script | 24 | 30 |

One figure absent from that table: the ConPTY startup byte count
does not move with the size, because it does not hold still at all. Across runs
of one configuration it read 5690 and 5690 at 80x24 and 7880 and 4865 at
100x30, since what conhost has flushed by the time the settle window closes
varies. A byte-count assertion over a ConPTY startup stream carries no
stability; the resize and redraw segments, provoked and bounded by the capture,
hold steady.

And the central result has its own control: the same ConPTY capture with the
cursor-position request left unanswered reports zeros everywhere (section 1),
while the same unix capture with the request left unanswered is byte-identical
to the answering run. A capture that reported the same result either way would
not have been reading the stream.

## 8. A harness gap this capture had to route around

`PtySession::record_raw_output` promises that the bytes a later `wait_for`
parses are recorded ("call this before `screen`, `with_screen`, `wait_for` or
any other method that drains"). They are not. `PtySession::wait_until` pulls
chunks off the channel straight into `self.parser` and never touches
`self.raw`; only `drain_available` records. Any chunk consumed by a blocking
wait is absent from `raw_output`.

Measured here: recording turned on, then a `wait_for` on the startup needle,
then `raw_output` returned 0 bytes for a session that had painted a full
screen. The capture answers this by waiting through `with_screen` and
`raw_output`, which do record.

The consumers in the tree today are the OSC 52 clipboard tests and
`history_copy`, which record and then `wait_for` before searching the raw
stream for an escape. They can only lose an escape that shares a chunk with the
thing being waited for, and they fail loudly when they do, so the shape
surfaces as a flake; a silent pass never happens here. It is a real defect in
the recorder's contract all the same.

## What the harness must do differently on Windows

| Concern | Unix behavior | Required on Windows |
|---|---|---|
| Startup handshake | child paints unprompted | master must answer `\x1b[6n` with `\x1b[<row>;<col>R` before any child output arrives, for **every** child, editor or not |
| Query responder | `QueryPolicy` answers DA1, `?2026$p`, `?u`, SGR, the box-glyph probe, OSC 11, DSR and a bare `\x1b[6n` | the cursor-position answer is what a ConPTY leg needs; without it every leg hangs to its deadline and reports an empty screen |
| Screen assertions | `contents()` is row-structured | read cells (`with_screen`, `cell(row, col)`); `contents()` collapses full-width rows into one wrapped line |
| Row structure | rows end `\x0d\x0a` | rows are padded through the final column by absolute positioning |
| Resize detection | no size report on the wire | `\x1b[8;<rows>;<cols>t` prefixes the resize repaint and marks it apart from an engine redraw |
| Sample boundaries | a repaint lands in one chunk under 200 us | a repaint is spread over 11 to 21 ms in several chunks; a boundary must state which end it takes, and a startup byte count is not stable enough to assert on |
| Exit codes | `:cq 3` gives 3 | identical, no accommodation needed |
| Clock | 18 ns tick | 100 ns tick, most consecutive reads identical; adequate for millisecond cells |
| Teardown | process-group kill | per-child kill suffices; no group-kill equivalent needed |
| Extra startup traffic | none | `\x1b[?9001h` (win32 input mode), `\x1b[?1004h` (focus reporting) and an OSC 0 title carrying the binary's full path, none of which the engine wrote |
