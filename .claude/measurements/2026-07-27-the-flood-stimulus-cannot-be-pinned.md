# The flood stimulus cannot be pinned from user space, so the row pairs instead

Date: 2026-07-27
Hosts: dev-linux (12-core VM) and mbp (macOS 26.2)
Method: `~/.claude/tmp/flood58/chunkprobe2.py` -- forks a pty, runs one
producer under `/bin/sh -c`, reads the master with a 1 MB buffer for 2.5 s and
reports how many bytes each read returned. No view, no nvim, no build.

## The remedy this was testing

The 2026-07-26 flood attribution found the two hosts chunk an identical byte
stream into the pty about 65x differently, and prescribed two pins. The first
(run the producer under a non-interactive shell) landed. The second was:

> **Chunk-size pin.** Because the kernels chunk 65x differently, the producer
> must write fixed-size blocks into the pty so the stimulus is comparable
> across hosts, or the cadence metric measures the OS buffer, not view.

That premise -- that the producer's write size reaches the reader -- is the
thing to test before building on it.

## What the two hosts do

| producer | dev-linux median | macOS median |
|---|---:|---:|
| `yes \| cat -n` | 194 B | 40 B |
| `yes \| cat -n \| dd obs=4096` | 161 B | 1024 B |
| `yes \| cat -n \| dd obs=65536` | 199 B | 1024 B |
| `stty -opost; yes \| cat -n` | **4095 B** | 40 B |
| `stty -opost; ... \| dd obs=1024` | 4095 B | 1024 B |
| `stty -opost; ... \| dd obs=512` | 3072 B | 1024 B |

Read across, not down. **Each host answers to a different knob, and neither
knob moves it onto the other's value.**

- On Linux the producer's write size is invisible: `dd obs=65536` delivers
  199 B. Output post-processing (`ONLCR`) is what forces the small chunks, and
  disabling it jumps delivery to 4095 B -- the kernel's tty output buffer.
- On macOS post-processing is irrelevant (`-opost` still delivers 40 B) while
  the producer's write size is decisive -- but only up to 1024 B, which is
  that kernel's own buffer. `obs=65536` and `obs=1024` both deliver 1024 B.

Asking for less does not work either: `obs=512` under `-opost` reads back
3072 B on Linux, because the kernel coalesces the producer's small writes into
whatever its buffer holds when the reader arrives.

**The delivered chunk size is the kernel's tty output buffer, a per-OS
constant. A producer cannot choose it, so the prescribed pin cannot exist.**

Throughput moves with it and is no steadier: 14.6 to 315.1 MB/s across these
variants on one host. There is no producer that makes two kernels present one
stimulus.

### The one knob the table does not test, and why it does not rescue the pin

Six points test the two knobs the original prescription implied (producer
block size, `-opost`), and n=6 does not by itself license "cannot be pinned":
a **paced** producer -- write a block, wait for the reader to drain it, write
the next -- *can* pin delivered chunk size on both kernels, because it never
lets the kernel coalesce.

That knob is unavailable to this row for a reason stronger than the table:
**pacing un-floods the flood.** The row exists to hold view to a coalescing
invariant under unbounded backpressure. A rate-limited producer supplies
bounded backpressure by construction, so it no longer exercises the invariant
under test. Any producer that pins the stimulus destroys the stimulus, which
is what makes the impossibility claim airtight *for this row* rather than for
pty writers in general.

### What is observed and what is derived

The byte values above are **probe-specific**: the probe reads the master with
a 1 MB buffer as fast as it can, while nvim reads on its own event-loop
cadence with its own buffer, and a slower reader lets the kernel coalesce
more. So 40 B and 194 B are observations about this probe, and the conclusion
that **the kernel, not the producer, owns the last hop into the reader** is
*derived* for nvim's actual reads, not observed on them. The derivation is
what the row rests on; the specific byte counts are not.

## What replaces it

The row's subject is a coalescing invariant: under a flood, view must not
stall painting. That invariant is expressible without a portable stimulus,
because both arms of a paired run meet whatever the local kernel does:

```
cadence_p99_ratio = view cadence p99 / nvim cadence p99   (same host, same run)
```

A coarser-chunking host lifts numerator and denominator together, so the
quotient survives what the millisecond number cannot -- the same property that
let `echo`'s `ratio_p50` move 1.7% while `view_p99_ms` from the same runs moved
7.4x across a 19x load range.

Two things this is careful about:

- **It is a ratio of two p99s, not the p99 of a paired ratio.** The two sides
  run in sequence over one window each; their gaps have no per-sample
  correspondence to pair. The metric name says which it is, so it cannot be
  read as the paired-sample statistic `ratio_p99` is.
- **The absolute `cadence_p99_ms` stays**, per class, against the spec's 16 ms.
  A flood must not stall paint on any host, and that promise is absolute. What
  is retired is its cross-class comparison, not the bar.

## What the cadence distribution actually is: a jitter tail, not a throttle

The p99 the row gates on cannot be read alone -- a coalescing failure and a
regular redraw cadence produce the same p99 from opposite distributions. The
reading was **pre-registered before the number existed**: p50 near 12-13 ms
means the p99 is the upper edge of a regular cadence, p50 near 16.4 ms means a
hard throttle. Anything between licenses neither.

Report-only run, `--scenario flood --fixture minimal --class dev-linux`, no
`--record`, no `--gate`. dev-linux, 12 cores, engine pin NVIM v0.12.4, three
trials. Host load average (1 min): **0.17 at start, 2.03 at end** -- the rise
is the run's own six pty sessions. Raw log: `~/.claude/tmp/t30-flood-p50.log`.

| trial | view p50 | view p90 | view p99 | nvim p50 | nvim p90 | nvim p99 | cadence ratio |
|---|---:|---:|---:|---:|---:|---:|---:|
| 1 | 12.24 | 13.68 | 15.52 | 12.22 | 13.86 | 15.97 | 0.972 |
| 2 | 12.24 | 13.72 | 15.38 | 12.13 | 13.50 | 14.86 | 1.035 |
| 3 | 12.21 | 13.59 | 15.13 | 12.16 | 13.52 | 14.84 | 1.020 |

**p50 12.24 ms: the jitter-tail reading, unambiguously.** The p99 sits 1.26x
its own p50 and the p90 sits between them at 13.68; a stall detaches its tail
from the bulk instead. Both sides paint at ~82 Hz and their p50s agree to
0.9%, which is what a shared upstream cadence looks like -- view's paint
stream is downstream of the same nvim the control side measures.

Two consequences worth carrying forward:

- **The 16 ms bar is not what this row failed.** At this host load the view
  side's gated `cadence_p99_ms` is **15.385 ms**, inside the spec 3.1 bar of
  16.0, where the earlier run at host load 3.9 read 16.429. The metric tracks
  ambient load; it did not measure a view defect at either value.
- **The ratio's own spread argues against gating it on a shared class.**
  Inside this single run, at near-constant load, `cadence_p99_ratio` moved
  0.972 to 1.035 -- a 6.3% spread. Echo's `ratio_p50` moved 1.70% across a
  19x load range, and that is the statistic whose load robustness was
  transplanted onto this one. A quotient of two tails taken over consecutive
  15-second windows has no per-sample pairing to cancel a spike that lands in
  one window and not the other, and the trial-to-trial spread here is
  consistent with that. The statistic is recorded and left ungated on shared
  classes until it has its own load-regime characterization.

What this run does **not** settle: it is one host, one afternoon. It licenses
the jitter-tail reading of the distribution shape, not a bar.

## Reproducing

```bash
python3 ~/.claude/tmp/flood58/chunkprobe2.py 'stty -opost; yes | cat -n'
ssh mbp 'zsh -lc "python3 /tmp/chunkprobe2.py \"stty -opost; yes | cat -n\""'
task bench -- --scenario flood --fixture minimal --class dev-linux
```
