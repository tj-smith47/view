# The echo gap is not the price of being an out-of-process UI

Date: 2026-07-26
Class: dev-linux (12-core VM, host load 0.25-0.56 throughout)
Engine: the pinned v0.12.4

## Question

The echo row measures view ~1.22x bare nvim at steady insert-mode typing.
Three explanations were adopted and falsified by earlier measurement (a
thread-hop floor, the pty transport, the instrumentation itself). The
standing hypothesis, written into `budgets.toml` as the reason its
shortfall was accepted, was that the residual is inherent to being an
out-of-process UI at all: view's TUI is a separate process reaching an
engine over msgpack-rpc, and bare nvim's is not.

That hypothesis is testable, because nvim ships its own out-of-process
UI. `nvim --server <sock> --remote-ui` attaches nvim's own C TUI to a
headless nvim over the same UI protocol view speaks. Comparing that arm
to bare nvim varies only the UI's *location*; comparing view to bare nvim
varies location *and* implementation together. The difference between the
two ratios is the implementation's share.

## Result

Both ratios are measured against a bare-nvim arm interleaved in the same
run, so each cancels host drift before the two are compared.

| fixture | view vs nvim | nvim remote-ui vs nvim | gap that is location |
|---|---|---|---|
| minimal | 1.224 | 1.038 | 17% of it |
| heavy | 1.238 | 1.018 | 8% of it |

Being an out-of-process UI costs 2 to 4 percent. view costs 22 to 24
percent. **The hypothesis is refuted: four fifths or more of the echo gap
is view's own, not the protocol's.**

## The confound that was checked and killed

The control run measured its bare-nvim arm at p50 0.665ms; the gate run
earlier the same day measured the same arm at 0.534ms. A between-run
offset like that compresses ratios toward 1.0, because a shared additive
term appears in both numerator and denominator, so it could have
manufactured the control's 1.038 on its own.

`echo/minimal` was therefore re-run immediately after the control, on the
same host state:

```
gate run:      nvim p50 0.534ms   view p50 0.659ms   ratio_p50 1.223
recheck:       nvim p50 0.669ms   view p50 0.816ms   ratio_p50 1.224
control:       nvim p50 0.665ms   r-ui p50 0.691ms   ratio_p50 1.038
```

The bare arm moved 24 percent between the two view runs and the ratio
moved by one part in a thousand. The ratios are comparable; the control's
1.038 is not an artifact of load.

Arithmetic bound on the same point: applying the control run's +0.13ms
offset to the gate run's view arm gives (0.659+0.13)/(0.534+0.13) = 1.188.
Compression can move 1.224 to 1.188. It cannot move it to 1.038.

## Where the gap actually goes

With the protocol explanation gone, the `echo_path` decomposition
attributes the remainder. On the minimal fixture the chain resolved for
all 3000 samples, no sample held a repeated round tag, and the stage sum
sat within 0.2% of the measured total, so these percentiles are an
attribution rather than an estimate.

| stage | p50 us |
|---|---|
| pty -> key-decoded | 98.4 |
| key-decoded -> loop-wake | 60.8 |
| loop-wake -> rpc-handoff | 13.3 |
| rpc-handoff -> rpc-written | 42.5 |
| rpc-written -> redraw-parsed | 462.6 |
| redraw-parsed -> loop-wake | 24.4 |
| loop-wake -> draw-start | 11.1 |
| draw-start -> frame-prepared | 1.0 |
| frame-prepared -> area-resolved | 0.4 |
| area-resolved -> composed | 16.3 |
| composed -> flush-start | 18.4 |
| flush-start -> term-written | 12.1 |
| term-written -> glyph-seen | 45.7 |
| **view total** | **805.2** |
| **bare nvim total** | **641.7** |

463 of view's 805 microseconds are spent inside the engine, and 46 in the
terminal and its parser, both of which bare nvim pays too. The ~299
microseconds that are view's own split into an input path of 215 (pty read
to RPC write) and a paint path of 84 (redraw parsed to terminal write)
against a measured gap of 163.5. No single stage dominates, so there is no
one hot spot to delete.

The heavy fixture's decomposition is **not** usable as an attribution: 2 of
its 3000 samples held a repeated chain tag, and the row's own guard states
that the stages between the RPC write and the terminal write are
understated whenever that reads non-zero. Its ratio (`gated ratio_p50`
1.218) is consistent with the minimal fixture's, but its per-stage numbers
are not quoted anywhere.

## What this changes

- The `echo.ratio_p50` shortfalls in `crates/view-bench/budgets.toml` keep
  their accepted values, but their stated reason was wrong and is
  corrected: the residual is view's to answer for.
- Any release claim that view's typing overhead is what an out-of-process
  editor UI costs is unsupported and must not be made.
- The attribution moves inside view, to the `echo_path` stage
  decomposition.

## Reproducing

```bash
task bench -- --scenario echo         --fixture minimal --class dev-linux
task bench -- --scenario echo_control --fixture minimal --class dev-linux
```
