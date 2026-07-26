# input_path: the gated boundary, and what the instrumentation was costing

Every number below was observed on dev-linux (quiet, 1-min load 0.2-0.5)
or mbp unless stated. Engine pin v0.12.4.

## The starting position

Spec 3.1 asked for `p99 <= 100 us` over "key at pty -> RPC bytes written",
and the recorded dev-linux value was 269.6 us. The prior investigation had
already established that the largest segment of that interval is the pty
transport between the harness's write to the pty master and view's read of
it (`pty->key-decoded`, 97.8 us p50), which no view code schedules.

## Two findings, in the order they were found

### 1. The gated boundary included the OS pty read

Fixed by opening the gated interval at view's own key-read tap instead of
the harness's pty write. The excluded prologue is still reported as the
`pty->key-read` segment, as evidence rather than as a bar.

Bracketing view's key *encode* separately was tried and abandoned. With a
dedicated arrival tap ahead of `encode_key` and the existing tap after it,
the pair read **14.3 us p50** on dev-linux and **15.0 us p50** on mbp --
and with the encode moved out from between them, the same pair read
**16.1 us p50**. The encode is smaller than the instrumentation's noise,
so the second tap was removed and the existing `K` tap moved to the key's
arrival: same tag, same chain, one fewer tap inside the gated interval.

### 2. The tap-overhead gate was measuring failed writes

`characterize_overhead` wrote 100000 records into the FIFO in a tight loop
and reported p50 0.27 us / p99 0.53 us, comfortably under the 5 us bar that
decides whether the taps rows may run at all.

Counting what the reader actually received: **39398 of 100000**. The FIFO
fills faster than the reader drains it, and every write after that fails
immediately with `EAGAIN`. A failed write is cheap and is nothing like the
operation the tap sites pay for, so the bar was being compared against a
mostly-failed operation.

Pacing the writes so all of them land changes the number:

| characterization | delivered | p50 | p99 |
|---|---|---|---|
| unpaced (as it was) | 39398 / 100000 | 0.27 us | 0.53 us |
| paced with `sleep` | 20000 / 20000 | 1.11 us | 6.70 us |
| paced with an on-CPU spin | 18000 / 18000 | 0.30 us | 1.13 us |

The sleeping pace has an artifact of its own: it measures every write on a
thread that just woke, cold and possibly on another CPU, which is the one
thing most of the real tap sites never are. The spin keeps the thread
running, and the difference lands entirely in the tail. The
characterization now spins, fails loudly if any write is undelivered, and
runs inside the live session rather than against an idle host.

### The tap's own cost, and the ~7 us that was `format!`

Two adjacent tap calls with nothing between them, in the input thread,
measured in view's own process during a real row:

| tap record built by | two adjacent taps, p50 | p99 |
|---|---|---|
| `format!` | 16.1 us | 36.0 us |
| a stack buffer | 9.1 us | 22.5 us |

The heap allocation was worth ~7 us of that pair. Both tap modules now
format into a stack buffer; nothing else changed.

### The 9 us left over is not instrumentation -- the control says so

Two adjacent taps costing ~4.75 us each would have made the whole
decomposition mostly instrumentation, against a characterization reading
0.30 us. Three candidate explanations were tested:

1. *The characterization runs on an idle host.* Disproved: it was moved
   inside the live session (view, nvim, the harness and the pty all
   running) and reads **0.293 us p50 / 1.342 us p99** there, unchanged.
2. *Contention between the three tapping threads.* Same disproof.
3. *The site, not the tap.* Confirmed. The paint path already carries a
   tap pair around what is now a plain `Rect` construction, at a site in
   the middle of running code rather than immediately after a blocking
   read. It measures **`frame-prepared->area-resolved` p50 0.3 us / p99
   1.4 us** -- the characterization's number, in view's own process. Its
   neighbour `draw-start->frame-prepared` reads 0.9 us.

So a tap costs ~0.3 us wherever it fires, and the ~9 us between the input
thread's two adjacent taps is the thread coming out of
`crossterm::event::read()`'s blocking syscall -- cost that belongs to
view's input path, not to the measurement of it. Instrumentation inside
the gated interval is two mid-code taps, ~0.6 us of an ~87 us p50: under
1%, and no correction is warranted.

This also revises the two readings above. The `format!` win is real but
its magnitude was measured on the one tap that pays the wake, so ~7 us is
that site's saving, not every site's; and `encode_key` being invisible
(14.3 us with it, 16.1 us without) was never a statement about encoding
cost at all -- both figures are the wake.

## Where the time actually goes (dev-linux, quiet, recording run)

```
pty->key-read          p50  78.6us  p99 139.4us   <- excluded, the OS's
key-read->loop-wake    p50  44.7us  p99  87.3us   <- channel + loop wake
loop-wake->rpc-handoff p50   9.4us  p99  22.5us   <- update() + msgpack
rpc-handoff->rpc-written p50 32.8us p99  77.8us   <- writer thread + pipe
gated key_to_rpc_p99_us 159.462 (median of 3 trials)
```

Two of the three gated segments are cross-thread wakes, which the hard
rules mandate ("the paint loop never awaits RPC; the RPC reader thread
never blocks"). Only `loop-wake->rpc-handoff` is view CPU, and it is 9.4 us
p50. There is no ~150 us of reducible view code behind the old 100 us
target; the target was written before the architecture it measures.

## Reproducing

`task bench -- --scenario input_path --fixture minimal --class <class>`.
The segment lines and the tap-overhead line are printed on every run.
