# Collapsing the runtime-loop-to-writer-thread hop

Date: 2026-07-27
Class: dev-linux (12-core VM)
Code: `crates/view-engine/src/outbox.rs`

## What was changed and why it was safe to change

`2026-07-26-the-input-gap-is-thread-hops-not-cpu.md` sized the
runtime-loop-to-writer-thread hop at ~31 to 42 us p50 and named it the one
hop on the input path that is not architecturally mandated. The writer
thread exists so that a wedged engine stalls a background thread rather
than the paint loop, which is what "the paint loop never awaits RPC"
protects. That purpose is served by never *blocking* on the pipe, not by
always *deferring* to another thread.

So the send path now writes inline when it can prove both:

- **The write cannot block.** POSIX makes a pipe write of at most
  `PIPE_BUF` atomic, and a poll reporting `POLLOUT` means at least that
  much room is free. Both halves are needed: the poll alone would permit a
  partial write that then blocks on the remainder. Messages above the
  inline ceiling, and every non-unix build, take the thread unchanged.
- **Nothing can be overtaken.** A message already handed to the writer
  thread has not reached the pipe yet, so writing a later one inline would
  reorder it. `handed_off` counts outstanding thread-bound messages; the
  inline path is taken only at zero, and the counter is decremented under
  the same lock the inline path tests it under.

Ordering therefore follows from lock acquisition order exactly as it
previously followed from channel order.

## The test that proves the ordering gate, and the one that did not

The first real-pipe ordering test passed **with the `handed_off` gate
removed**, which makes it worthless as a proof. A single-threaded sender
feeding a fast reader closes the race window before it opens: the pipe
never backs up, so the thread path is never taken, so there is nothing for
an inline write to overtake. It was measuring an empty queue.

`a_backlogged_pipe_keeps_order_while_both_paths_are_live` fixes both
halves. The reader sleeps 50 us per 64-byte read so the pipe stays
backlogged, and 200 000 messages are sent so the 64 KB pipe buffer is
genuinely exhausted (at 20 000 the whole run fit inside the buffer and the
test reported `inline 20000, threaded 0`). It asserts both paths ran
before it asserts byte-exact order.

Disconfirming run, gate deleted:

```
the outbox reordered messages: inline 99835, threaded 100165
```

Gate restored: passes. The test can reintroduce the bug on demand, which
is what separates it from a test written to pass.

## What it bought

Segment, from the tapped `echo_path` row (load-independent, both arms of
the same run):

```
rpc-handoff->rpc-written    42.5 us p50   ->   14.1 us p50
```

The residual ~14 us is the poll syscall, the mutex, and the write itself.
An unsafe throwaway that skipped the poll reached 10.2 us, so the safety
check costs about 4 us of the 28 saved.

Back-to-back paired A/B on the same host state, `echo/minimal`:

| leg | view p50 | nvim p50 | gap | ratio_p50 |
|---|---|---|---|---|
| before (`HEAD~1` engine) | 0.660 ms | 0.544 ms | 116 us | 1.224 |
| after | 0.632 ms | 0.543 ms | **89 us** | **1.161** |

The bare-nvim arm is unchanged between legs (0.544 vs 0.543), which is what
rules out the load difference between the two legs doing the work: ratio
compression needs a shared additive offset, and there is none here. The
27 us the gap lost matches the 28 us the segment lost.

## Recorded

Re-recorded on a quiet host, both null-pair brackets clean:

| cell | metric | was | now |
|---|---|---|---|
| echo.minimal | ratio_p50 | 1.3538 | 1.1719 |
| echo.minimal | ratio_p99 | 1.3075 | 1.0917 |
| echo.minimal | view_p99_ms | 1.0186 | 0.9043 |
| echo.heavy | ratio_p50 | 1.2441 | 1.1838 |
| echo.heavy | ratio_p99 | 1.1423 | 1.0103 |
| echo.heavy | view_p99_ms | 1.9059 | 1.7799 |
| input_path.minimal | key_to_rpc_p99_us | 154.749 | 117.739 |

Only `echo/minimal` has a before/after A/B behind it. `echo.heavy` and
`input_path.minimal` are one post-change run each against a previously
recorded value from a different run, so their deltas carry between-run
variance that the minimal figure does not.

The two `dev-macos` ratio shortfalls predate this change and have not been
re-measured; task 21 owns that.

## A defect the re-recording exposed

Gating the freshly written ledger against a fresh measurement failed
immediately:

```
BUDGET FAIL [echo.minimal] ratio_p50: 1.176 against spec 1.100
  is worse than the accepted shortfall 1.172
```

0.35% apart. `Verdict::Widened` compared the next measurement to the
`accepted` value with zero tolerance, but an accepted value is one sample
of a noisy statistic, so every listed shortfall had roughly even odds of
failing any given run. The shortfall ceiling is now the one the baseline
ratchet already grants that metric on that class, and a metric the class
does not gate at all gets no ceiling here either. Both directions are
asserted, and the shipped ledger passes.

## Reproducing

```bash
task bench -- --scenario echo --fixture minimal --class dev-linux
task bench -- --scenario echo_path --fixture minimal --class dev-linux
cargo test -p view-engine outbox
```
