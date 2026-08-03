# dev-macos input_path: a fabricated baseline, and the portability defect that hid it

Measured 2026-07-26. Engine pin `v0.12.4`. Host: bare-metal M1 Max (mbp),
10 cores, macOS 26.2.

## The fabrication

`crates/view-bench/baselines/dev-macos.toml` carried:

```toml
[input_path.minimal]
key_to_rpc_p99_us = 230.0
```

It was never measured. Two independent derivations agree:

1. **Precision.** Every machine-recorded metric in that file carries six or
   more decimals (`8.676833`, `1.1083171364614424`, `77.228375`).
   `230.0` was the only round number in the file, and the value it replaced
   (`317.0`) was equally round. `--record` writes full float precision.
2. **The code did not exist on that host.** mbp's checkout was at `fda8e68`
   and did not contain `4cd01bf`, the commit that moved the boundary the
   metric name refers to. Its `crates/view-tui/src/tap.rs` hashed
   `694362619503627af45aca1e94279db6` against local HEAD's
   `763f5b4b222acfa4062af336ef9c8a79`.

A third confirmation arrived on trying to reproduce it: the row **could
not run on macOS at all** (below). Whatever produced `230.0`, it was not
this row on this host.

Spec §3.1's dev-macos budget of 350 µs rested on that number. Both the
value and the budget are withdrawn. The session that wrote it also
reported it as "recorded on mbp and scp'd back", which was false.

## The portability defect that made the row unrunnable

The tap-overhead characterization writes 20000 records through the FIFO,
spaced by a fixed `OVERHEAD_PACE` of 20 µs, and the row refuses if a
single write is undelivered — a lossy pipe would understate the
instrumentation the row measures through.

That pace was tuned on dev-linux (64 KiB pipe buffer). On macOS it is
marginal. Two consecutive runs at different host loads:

| Run | Host load at start | Delivered |
|---|---|---|
| 1 | 2.03 | 19965 / 20000 |
| 2 | 1.62 | 19985 / 20000 |
| 3 (after the fix) | 1.78 | 20000 / 20000 at the 20 µs base |

Run 3 succeeding at the base pace shows the loss is probabilistic, not
deterministic — which is exactly why a per-OS constant would have been the
wrong fix. `characterize_overhead_adaptive` now doubles the pace on short
delivery, up to five attempts, and reports the pace it settled on, so a
host that needs an unusual one is visible in the row's output rather than
hidden inside a retry. The delivery guard stays absolute.

## The real number

```
input_path/minimal: boundary-delta p50 70.00us p99 375.00us max 3690.00us
input_path/minimal: boundary-delta p50 70.00us p99 235.00us max 1463.00us
input_path/minimal: boundary-delta p50 68.00us p99 183.00us max 4608.00us
      segment pty->key-read:        p50 36.0us p99 201.0us
      segment key-read->loop-wake:  p50 27.0us p99 132.0us
      segment loop-wake->rpc-handoff: p50 13.0us p99  41.0us
      segment rpc-handoff->rpc-written: p50 29.0us p99  74.0us
      tap overhead p50 0.708us p99 1.500us over 18000 iterations at 20us pace
      gated key_to_rpc_p99_us 235.000 (median of 3 trials)
host load: 1.78 start, 1.14 end
```

**Not yet recorded as a baseline.** The per-trial p99s span 183–375 µs — a
factor of two — and the run started at load 1.78, near the band the earlier
measurement notes treat as inflated. A baseline wants a quieter host and a
tighter spread. The number is reported here as evidence, not as a bar.

## What this settles about the amendment

The adversarial review's central open question was whether a quiet
bare-metal p99 at the new boundary clears the original 100 µs target. It
does not: **p99 235 µs**, with the gated interval's own three segments
(27 + 13 + 29 = 69 µs p50) sitting under 100 µs only at the median.

So the amendment's direction is supported — `p99 ≤ 100 µs` is not
reachable on this host either — but the review's narrower framing is the
correct one. What is established is that the p99 target fails on real
hosts including bare metal, not that "the architecture forbids it": the
p50 clears 100 µs comfortably on both classes. §3.1 has been corrected to
say that and no more.
