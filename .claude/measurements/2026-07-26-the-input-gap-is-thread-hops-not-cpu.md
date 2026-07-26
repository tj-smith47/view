# The typing gap is thread hops waking idle cores, and it is sized

Date: 2026-07-26
Class: dev-linux (12-core VM, quiet)
Bench: `cargo bench -p view-core --bench input_handoff`

## What this corrects

`2026-07-26-input-path-boundary-and-tap-cost.md` closed with:

> Two of the three gated segments are cross-thread wakes, which the hard
> rules mandate. Only `loop-wake->rpc-handoff` is view CPU, and it is 9.4 us
> p50. There is no ~150 us of reducible view code behind the old 100 us
> target.

The first half is right and the conclusion drawn from it is wrong. The
segments are indeed wakes rather than view CPU. But "it is a wake, and the
rules mandate a wake" was treated as "the cost is fixed", and it is not:
the cost is charged **per hop**, and how many hops the input path takes is
a design variable, not a rule.

## The measurement

`SyncSender<Keystroke>` at the production capacity, a receiver that is
genuinely parked (not spinning), one keystroke at a time. The swept
variable is how long the receiver has been idle before the send, because a
core idle for milliseconds sits in a deeper sleep state than one idle for
microseconds and someone pays to bring it back.

| idle gap | 1 hop p50 | 2 hops p50 | cost of hop 2 |
|---|---|---|---|
| 50 us | 7.78 | 14.83 | +7.1 |
| 200 us | 8.84 | 19.16 | +10.3 |
| 1 ms | 20.56 | 41.87 | +21.3 |
| 10 ms | 40.04 | 71.25 | +31.2 |

Two things fall out.

**A hop's cost is a function of idle depth, not of the channel.** The same
primitive costs 7.8 us or 40 us depending only on how long the far side
has been asleep. The echo row paces at 10 ms and a human types slower than
that, so the 10 ms row is the case the product actually meets. The
"1.2-4.4 us cross-thread wake" figure quoted in spec 3.1 is from a
different measurement and does not describe this one.

**Hops are additive.** A second hop costs a second wake, at 78 to 96
percent of the first across the sweep. They do not overlap.

## Against the tapped production segments

```
key-decoded->loop-wake     60.8 us p50   (1 hop + thread cold off crossterm's read + encode)
rpc-handoff->rpc-written   42.5 us p50   (1 hop + the write syscall)
                          ------
                          103.3 us of a 163.5 us view-vs-nvim gap
```

The bench puts a bare 10 ms-idle hop at 40 us, so both segments are
substantially one hop each plus their own small work. That accounts for
the two segments without appealing to any view CPU, and it matches the
independent finding that view CPU on this path is 9.4 us p50.

## Why nvim's own remote UI does not pay this

The `echo_control` row measures nvim's own out-of-process TUI at 1.015
against bare nvim where view measures 1.354. That client reads its pty and
writes its socket **on one thread**: one wake, at the pty read. view takes
two further hops after that read, and each one wakes a core that has been
idle for a keystroke interval.

This is the whole shape of the gap. It is not Rust versus C, not msgpack,
not compositing, and not the protocol.

## What is reducible, and what is not

The input thread to runtime loop hop is **not** reducible: a key must be
dispatched through the model, which may consume it for view's own UI
rather than forward it, and the model lives on the loop.

The runtime loop to RPC writer thread hop **is** a candidate, worth ~31 to
42 us p50 of a 163.5 us gap (20 to 26 percent of the whole typing
deficit). It cannot simply be deleted: the writer thread exists so that a
wedged engine stalls a background thread instead of the paint loop, which
is what "the paint loop never awaits RPC" protects. Any inline fast path
has to preserve two invariants at once:

- **Ordering.** Messages must reach nvim in the order they were produced.
  An inline write racing a message already queued for the writer thread
  would reorder keystrokes, which corrupts the buffer.
- **Non-blocking.** The paint loop must not be able to block on a full
  pipe.

Both are satisfiable: take the writer lock, write inline only when the
queue is empty (so nothing can be overtaken), write non-blocking, and push
any unwritten remainder back for the writer thread to finish. Ordering
then follows from lock acquisition order exactly as it followed from
channel order before.

## Reproducing

```bash
cargo bench -p view-core --bench input_handoff
task bench -- --scenario echo_path --fixture minimal --class dev-linux
```
