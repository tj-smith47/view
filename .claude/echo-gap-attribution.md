# Where the echo gap lives

Measured attribution of the `echo/minimal` paired ratio into named stages,
on both measured classes. Supersedes the subtraction-derived estimate in
`.superpowers/sdd/t12-attribution.md`, whose `W -> R` figure was never
measured directly.

Every number here was measured on the commit named in the run-conditions
table, on the host it is filed under, with the host load it was taken at
recorded beside it. Where an earlier version of this document made a claim
that a later measurement killed, the claim is gone rather than softened;
the killed claims are listed under "What earlier readings of this got
wrong". Every delta is reported beside the spread of the same measurement
repeated without a source change, because a delta smaller than that spread
is not a result.

Instrumentation: the `echo_path/minimal` report-only bench cell walks one
keystroke's whole round trip through the `bench-taps` build as the ordered
tag chain `K U S W R U B P G C F T`, anchored at the harness timestamp
taken immediately before the key byte reaches the pty master and closed by
the harness observing the echoed glyph. Bare nvim is paired into the same
run through the same pairing math the gated echo row uses.

```
$ task bench -- --scenario echo_path --fixture minimal --class dev-linux \
       --samples 1000 --warmup 100 --trials 3
```

## Where the gap lives, in one paragraph

The echo gap lives in **view's own plumbing on either side of the RPC
seam, not in the seam itself**: the stages that have no counterpart in
bare nvim's architecture account for **+150 us of a +165.8 us gap on
dev-linux and +343 us of a +437 us gap on dev-macos**, while the stages
that do have a counterpart come to -8.2 us on dev-linux (below this
project's 3% reporting floor, so not a finding either way) and +66 us
against view on dev-macos, with a reported residual of +23.9 and +28 us
from percentile non-additivity. The `--embed` protocol boundary is
specifically **not** the answer, because bare nvim v0.12.4 is itself a
two-process msgpack-RPC architecture — a UI client and an editor server
exchanging `nvim_input` and `redraw` over two unidirectional pipes, one
frame at a time, even under `-u NONE` — so it pays a structurally
identical boundary. Within the view-only stages the single largest named
cost, and the only stage whose bare-nvim counterpart is exactly zero, is
the **per-frame terminal size query**: one `self.inner.size()?` in
`draw_surface` that crossterm serves with `open("/dev/tty")` +
`ioctl(TIOCGWINSZ)` + `close`, costing **22.5 us p50 on dev-linux and
190 us p50 on dev-macos** per painted frame against bare nvim's **0**
calls per frame; building view with that one statement replaced by the
resize-tracked size the model already holds collapses the stage to 0.6 and
5 us — 18x and 92x the stage's own baseline-to-baseline spread. That
collapse is the firm result and three separate parties have reproduced it.
The further claim that removing the call **reverses the host inversion**
in `ratio_p50` is weaker, and its provenance must travel with it: it is
established on **dev-macos only**, over three interleaved
baseline/reverted pairs whose arms separate by 11x their own spread, from
a single campaign no independent party has repeated. On **dev-linux it is
not established**. The campaign's own dev-linux arms separated by 0.022,
but an independent attempt to reproduce that pair returned steps of
**opposite sign** (-0.023 and +0.020), both inside the arm spread, arms
fully overlapping. That attempt ran at roughly 8x the campaign's host
load, so it does not refute the campaign's numbers — but it establishes
something more useful than either: ambient load alone moves this host's
`ratio_p50` by ~0.06, **larger than the 0.048 effect under test**, so
dev-linux cannot resolve an effect of this size and a null result there is
uninterpretable. Read the dev-linux leg as absent evidence, not as
supporting evidence. The rest of the view-only total is the
four channel handoffs between view's threads (117 us linux / 127 us
macos), which are the only work shared with the `input_path` row.

**What this does not explain, stated explicitly:** it does not explain the
tail on either host — the whole attribution is p50, and at p99 the
inversion does not exist at all (dev-macos `ratio_p99` 0.943, view better
than bare nvim); it does not explain the **+66 us / 6.1% dev-macos bracket
excess**, which is the largest unexplained quantity here and sits on the
boundary side of the one host every headline claim rests on; it does not
settle spec 3.1's presumption **in either direction**, and proposes no
amendment to it; it does not explain why bare nvim is itself 2x slower on
the M1 Max (1076 vs 531 us), why the size call costs more inside view's
process than in a minimal one, or why the dev-macos view total drops 36 us
more than the stage that was removed.

It also leaves the inversion-reversal **single-sourced**. The dev-macos
campaign met a success criterion an adversarial party registered before
the measurement existed, which is the strongest evidence shape available
here — but it remains one campaign, on the one host every headline claim
rests on, and the attempt to corroborate it on the other host instead
found that host incapable of resolving the effect. Anything built on the
reversal should carry that. Nothing in this document does: the shippable
finding is the size query's own cost, which stands on the collapse alone
and does not need the reversal to be true.

## Chain integrity, and what guards it

The walker takes the earliest match at or after the previous match. That
rule disambiguates the two `U` (loop wake) appearances, and four tests
pin it. It does **not**, by itself, keep a keystroke from pairing with a
redraw round it did not cause: a spurious complete round inside the sample
window collapses every stage between the RPC write and the terminal write
and parks their time in the closing stage, while both wake counters still
read clean. That failure mode is now counted directly.

Every chain tag except `U` is expected exactly once per sample window, so
each is counted over the window and a repeat is reported. `U` appears
twice by design and keeps its per-bracket counters. Both are pinned by
tests (grep `a_spurious_redraw_round_ahead_of_the_real_one_is_flagged` and
`every_single_round_tag_is_a_chain_tag_and_no_wake_is_one` in
`crates/view-bench/src/scenarios/taps.rs`), and the injected-round test is
sabotage-verified: neutering the counter turns it red.

**Every decomposition run below reports zero on all ten counters, on both
hosts, over 3000 samples each.** No number in this document comes from a
window that held more than one redraw round.

## Run conditions

Every measurement in this document, with the host load it was taken under.

| | dev-linux | dev-macos |
|---|---|---|
| host | KVM guest, 12 vCPU | Apple M1 Max, 10 core, bare metal |
| commit | `fda8e68` | `fda8e68` |
| engine pin | v0.12.4 | v0.12.4 |
| headline decomposition, load start -> end | 0.59 -> 0.32 | 1.50 -> 0.71 |
| headline decomposition, CPU idle start -> end | not sampled | 84.7% -> 87.7% |
| headline decomposition, null-pair calibration | 1.0089 (dev 1.0089) | 1.0089 (dev 1.0089) |
| baseline decomposition replicates (see below) | 3 more, load 1.31-1.55 | 1 more, load 1.90 |
| replicate view total spread | 719.5 / 720.2 / 727.9 vs headline 696.8 | 1507 vs headline 1513 |
| replicate size-query stage spread | 21.3 / 21.7 / 21.7 vs headline 22.5 | 192 vs headline 190 |
| normal-build `echo` gate, load / calibration | 0.32 -> 0.33 / 1.0089 | 1.40 -> 1.33 / 1.0208 |
| reverted decomposition, load / calibration | 0.45 -> 0.23 / 1.0340 | 1.18 -> 1.49 / 0.9833 |
| reverted normal-build `echo`, load / calibration | 0.44 -> 0.18 / 0.9725 | 1.46 -> 1.47 / 0.9794 |
| interleaved normal-build campaign, pairs / load range | 2 pairs, 0.31-0.51 | 3 pairs, 1.38-2.27 |
| interleaved campaign, per-run calibrations | 0.9159 - 1.0987 | 0.9657 - 1.0227 |
| paint micro-bench, load start -> end | 0.08 -> 0.73 | 1.86 -> 1.44 |
| standalone size-query probe, load start -> end | 0.35 -> 0.46 | 1.37 -> 1.56 |
| bare-nvim syscall trace, load start -> end | 0.25 -> 0.34 | not obtainable (see below) |
| tap overhead p50 / p99 | 0.279 / 0.698 us | 0.708 / 1.542 us |
| pty floor control, p50 / p99, n=500 | 31.9 / 49.4 us | 28 / 55 us |
| calibration floor | 1.15 | 1.15 |
| samples per stage | 3000 (3 trials x 1000) | 3000 |

The diagnostic cell now takes its own null-pair calibration in the run
that produces its ratios, rather than borrowing one from a different run
at a different load.

### What a repeat of the same run costs, on each host

Every delta in this document is reported beside the spread of the same
measurement repeated with no source change, because a delta smaller than
that spread is not a result.

**dev-linux, four baseline decomposition runs of identical source:**

| run | load start -> end | calibration | size-query stage | view total | resolved |
|---|---|---|---|---|---|
| headline | 0.59 -> 0.32 | 1.0089 | 22.5 | 696.8 | 3000/3000 |
| replicate 1 | 1.31 -> 1.80 | 1.0263 | 21.7 | 720.2 | 2929/3000 |
| replicate 2 | 1.55 -> 1.41 | 0.9680 | 21.7 | 727.9 | 2996/3000 |
| replicate 3 | 1.41 -> 0.82 | 0.9766 | 21.3 | 719.5 | 2991/3000 |

Spread across the four: **view total 31.1 us (4.5%)**, size-query stage
**1.2 us**. The three replicates were taken at a higher load than the
headline run, so part of that 4.5% is load rather than run-to-run noise;
even among the three replicates alone, taken back to back, the total
spans 8.4 us while the stage spans 0.4 us. Redraw-round multiplicity was
**0 on every tag on all four runs**. The replicates carry a few unresolved
chains (4 to 71 of 3000) where the headline run carried none; unresolved
samples are dropped from the stage percentiles and counted, never
redistributed.

**dev-macos, two baseline decomposition runs:** load 1.50 and 1.90, size
query 190 vs 192, view total 1513 vs 1507, nvim total 1076 vs 1075 —
**6 us (0.4%) apart on the total**.

The two hosts' repeat spreads are therefore very different quantities:
31 us (or 8 us back-to-back) on dev-linux against 6 us on dev-macos, and
each delta below is weighed against its own host's figure.

### dev-macos host load: what it does and does not rule out

dev-macos is the user's laptop, in daily use, and its ambient one-minute
load average floor over this session was **1.3 to 1.5**; "load below 0.5"
was never reachable. That is not a limitation on the conclusions, and the
measurements say why rather than leaving it open:

| measured on dev-macos | value |
|---|---|
| `sysctl hw.ncpu` / `hw.physicalcpu` | 10 / 10 |
| one-minute load average | 1.42 |
| `top -l 2` CPU idle | 87.4% |
| largest single process | under 12% CPU |

Load average is an un-normalized count of runnable threads, not a
utilization figure. 1.42 runnable on 10 cores is **14% utilization**, and
the independently measured 87.4% idle says 12.7% busy — the two agree.
Together they **rule CPU contention out**: there was no queue for a core
at any point in these runs, so no measurement here was taken under
contention and no error bar needs widening for it.

Two further reasons the load does not reach the conclusions. The two
baseline runs at load 1.50 and 1.90 agree to 0.4% on every headline
number, so the effect of a 27% load change on this row is smaller than
the numbers being compared. And the statistic every claim below rests on
is a **paired** one: view and bare nvim are sampled alternately inside the
same run under the same ambient load, so load common to both sides divides
out of the ratio. Where two builds are compared, the builds are
interleaved run by run as well, so drift cannot align with one arm.

What the load does leave open is the tail, not the median: an ambient
burst lands in p99 and this document draws no conclusion from dev-macos
p99 values.

## Per-stage decomposition

`pays` names what bare nvim spends on the equivalent work. `bracket`
stages have a measured bare-nvim counterpart; `view-only` stages have no
counterpart in bare nvim's architecture at all. The classification is
justified stage by stage under "Bare-nvim comparison" below, and it
differs from an earlier version of this document: `size-probed->composed`
and `composed->flush-start` were `view-only` there, and are `bracket`
here, because bare nvim's UI process was measured doing exactly that job.

### dev-linux, load 0.59, calibration 1.0089

| stage | pays | p50 us | p99 us | samples |
|---|---|---|---|---|
| pty->key-decoded | bracket | 86.3 | 147.1 | 3000 |
| key-decoded->loop-wake | view-only | 50.8 | 92.8 | 3000 |
| loop-wake->rpc-handoff | view-only | 13.8 | 30.0 | 3000 |
| rpc-handoff->rpc-written | view-only | 32.2 | 76.7 | 3000 |
| **rpc-written->redraw-parsed** | bracket | **363.6** | 554.9 | 3000 |
| redraw-parsed->loop-wake | view-only | 20.5 | 58.0 | 3000 |
| loop-wake->draw-start | view-only | 8.9 | 23.7 | 3000 |
| draw-start->frame-prepared | view-only | 1.3 | 5.4 | 3000 |
| **frame-prepared->size-probed** | view-only | **22.5** | 43.1 | 3000 |
| size-probed->composed | bracket | 13.3 | 26.4 | 3000 |
| composed->flush-start | bracket | 13.0 | 25.6 | 3000 |
| flush-start->term-written | bracket | 9.2 | 19.6 | 3000 |
| term-written->glyph-seen | bracket | 37.4 | 93.3 | 3000 |
| sum of stages | | 672.9 | | |
| TOTAL view t0->glyph | | 696.8 | 992.1 | 3000 |
| TOTAL nvim t0->glyph | | 531.0 | 773.1 | 3000 |
| **residual (total - sum)** | | **23.9 (3.4%)** | | |

### dev-macos, load 1.50, calibration 1.0089

| stage | pays | p50 us | p99 us | samples |
|---|---|---|---|---|
| pty->key-decoded | bracket | 43 | 183 | 3000 |
| key-decoded->loop-wake | view-only | 44 | 135 | 3000 |
| loop-wake->rpc-handoff | view-only | 21 | 52 | 3000 |
| rpc-handoff->rpc-written | view-only | 34 | 88 | 3000 |
| **rpc-written->redraw-parsed** | bracket | **978** | 4579 | 3000 |
| redraw-parsed->loop-wake | view-only | 28 | 80 | 3000 |
| loop-wake->draw-start | view-only | 20 | 38 | 3000 |
| draw-start->frame-prepared | view-only | 6 | 8 | 3000 |
| **frame-prepared->size-probed** | view-only | **190** | 485 | 3000 |
| size-probed->composed | bracket | 37 | 71 | 3000 |
| composed->flush-start | bracket | 39 | 70 | 3000 |
| flush-start->term-written | bracket | 16 | 28 | 3000 |
| term-written->glyph-seen | bracket | 29 | 727 | 3000 |
| sum of stages | | 1485 | | |
| TOTAL view t0->glyph | | 1513 | 6851 | 3000 |
| TOTAL nvim t0->glyph | | 1076 | 6094 | 3000 |
| **residual (total - sum)** | | **28 (1.9%)** | | |

All figures microseconds. In both headline runs every stage resolved on
3000 of 3000 measured view samples, and the totals are pooled over the
same 3000; the dev-linux replicates listed under "What a repeat of the
same run costs" resolved 2929 to 2996 of 3000, with the unresolved
samples counted and dropped rather than redistributed.
`macOS CLOCK_MONOTONIC` quantizes to 1 us, which is why every dev-macos
figure is whole.

The residual is what percentiles not adding produces: each stage's p50 is
its own median, and the medians of 13 stages do not sum to the median of
their sum. It is reported, never distributed across the stages.

## Self-consistency check

This is an arithmetic identity, not evidence. `residual = total - sum of
stages` and `view-only + bracket = sum of stages` by definition, so the
columns close for **any** assignment of stages to `bracket` and
`view-only`. Its only value is catching a transcription error.

```
                                  dev-linux     dev-macos
  view total                        696.8         1513
  nvim total                        531.0         1076
  measured gap                     +165.8         +437

  view-only stages                 +150.0         +343
  bracket - nvim total               -8.2          +66
  residual                          +23.9          +28
                                   ------        ------
                                   +165.7         +437
```

Nothing follows from the fact that it closes. What the bracket line means
is argued from measurement below, not from this table.

## Bare-nvim comparison, per stage

The comparison method that works is tracing the pinned engine's syscalls
while it is driven through real frames in a pty. Doing that produced the
finding this whole comparison rests on.

**Bare nvim v0.12.4 is itself a two-process msgpack-RPC architecture.** A
30-frame trace on dev-linux (`strace -f -tt -T -e
trace=read,write,writev,readv,ioctl`) shows a UI client process and an
editor server process exchanging `nvim_input` and `redraw` over **two
unidirectional pipes**, one frame at a time:

```
client  read(0, "x")                      keystroke off the pty
client  write(12, "...nvim_input...x")    msgpack encode, out over pipe A
server  read(10, "...nvim_input...x")
server  write(11, "...redraw...grid_line")  process, encode the redraw
client  read(13, "...redraw...grid_line")   in over pipe B
client  writev(18, [ESC[?25l, "xhello...", ESC[?25h])   the frame
```

The transport is two pipes and not a socketpair, and `/proc` says so
directly rather than by inference. Driving `nvim -u NONE -n` under a pty and
reading both processes' fd links:

```
client (pid N)    fd 12 -> pipe:[113237560]      fd 13 -> pipe:[113237561]
server (pid N+1)  fd 10 -> pipe:[113237560]      fd 11 -> pipe:[113237561]
     nvim --embed -u NONE -n
```

Two distinct pipe inodes, each appearing once on each side, carrying opposite
directions. A socketpair would show one `socket:[...]` inode on both sides.
This is also the second independent confirmation of the two-process fact, and
it holds under `-u NONE`, the most minimal invocation there is: it is not
config-dependent and not a thread.

The same topology holds on dev-macos: a bare `nvim` in a terminal is two
processes there too (parent UI client, child server), confirmed by `ps`.

So the `--embed` protocol boundary is **not an architecture view pays for
and bare nvim escapes**. Bare nvim pays a structurally identical one. That
is a stronger statement than any bracket arithmetic, and it does not
depend on percentile addition.

The per-frame trace also settles the single most important stage exactly:

| trace fact | count |
|---|---|
| `TIOCGWINSZ` calls over the whole session | **1** |
| `TIOCGWINSZ` calls after the first keystroke, over 30 frames | **0** |

Bare nvim queries the terminal size once, at startup, and never again per
frame. view queries it on every painted frame.

### Per-stage table

Strace inflates every traced syscall and every gap between two of them, so
the bare-nvim times below are upper bounds and are comparable to one
another, not to view's untraced figures at face value. The count of
`TIOCGWINSZ` calls is exact regardless. dev-linux figures.

| view stage(s) | p50 us | bare nvim counterpart | nvim p50 us | status |
|---|---|---|---|---|
| pty->key-decoded + the three input handoffs | 183.1 | client `read(0)` end -> `write(rpc)` start | 97 | measured |
| rpc-written->redraw-parsed | 363.6 | both pipe crossings + server process/encode | 722 | measured |
| redraw-parsed->loop-wake .. composed->flush-start | 79.5 | client `read(redraw)` end -> `writev(frame)` start | 42 | measured |
| of which frame-prepared->size-probed | 22.5 | per-frame `TIOCGWINSZ` | **0** | measured, exact |
| flush-start->term-written | 9.2 | `writev` duration | 25 | measured |
| term-written->glyph-seen | 37.4 | the same harness parse runs on both sides | identical | identical by construction |
| key-decoded->loop-wake, loop-wake->rpc-handoff, rpc-handoff->rpc-written, redraw-parsed->loop-wake, loop-wake->draw-start | | channel handoffs between view's threads | structurally absent | bare nvim's client is single-threaded on this path; it has no channel to cross, so there is no quantity to measure rather than one that went unmeasured |

### Why no multiplier is quoted from these numbers

An earlier version of this document normalized both sides to shares of
their own chain and claimed the result was "immune to the inflation
factor", then quoted a bolded multiplier off it. **That is wrong in
principle**, and the reason matters more than the number:

- ptrace overhead is charged **per syscall boundary** and is roughly
  constant per boundary — not proportional to the true time of the stage
  the boundary sits in. Normalization cancels a *proportional* factor. It
  does not cancel a per-event one.
- The four nvim stages do not hold equal numbers of boundaries. They hold
  **1, ~4, 1 and 1**. So the overhead lands overwhelmingly on the second
  stage, inflating its share and deflating the other three's by
  construction.
- The overhead is measurable and large: the traced chain sums to 886 us
  against 531 us for the same round trip measured untraced by the harness,
  so 355 us over ~7 boundaries is **~51 us per boundary** — comparable to
  or larger than three of the four stages.
- It cannot be corrected out either. Deducting 51 us per boundary from
  nvim's stage times (97, 722, 42, 25) gives **46, 519, -9, -26**: two of
  the four stages go negative. A correction that produces negative
  durations is not a correction, and no share table computed from this
  trace can be trusted to a multiplier.
- Compounding all of it, the two columns are not the same kind of
  measurement: view's shares come from an **untraced** run and nvim's from
  a **ptrace-perturbed** one.

The table is therefore given as an indication of direction and nothing
else. No multiplier is quoted from it, and no conclusion in this document
rests on it.

| job | view share (untraced) | bare nvim share (traced, inflated) |
|---|---|---|
| pty keystroke -> RPC bytes out | 28.8% | 10.9% |
| RPC out -> redraw parsed | 57.2% | 81.5% |
| redraw parsed -> frame bytes ready | 12.5% | 4.7% |
| frame bytes -> written | 1.4% | 2.8% |

The direction — view spending a larger share of its round trip on both
ends of the RPC seam and a smaller share on the seam itself — is what the
per-stage table already shows without normalizing anything, and it is
consistent with the one exact number in the comparison: bare nvim's
per-frame `TIOCGWINSZ` count of zero. The *size* of the difference is not
established by this trace and is not claimed.

Not obtainable: the same trace on dev-macos. `dtruss` requires SIP to be
disabled on that host, which it is not. The architectural fact was
confirmed there by process topology instead; the per-stage timings were
not measured on dev-macos and no dev-macos number in this document depends
on them.

### What the bracket comparison says now

With `size-probed->composed` and `composed->flush-start` moved into the
bracket, where the trace shows bare nvim's client doing the same job:

| | dev-linux | dev-macos |
|---|---|---|
| view bracket stages | 522.8 | 1142 |
| bare nvim whole round trip | 531.0 | 1076 |
| difference | -8.2 (1.5% of nvim's total) | **+66 (6.1%)** |

On dev-linux the difference is below this project's 3% cross-binary
reporting floor and is not a finding in either direction. On dev-macos it
is **against** view by 6.1%.

**There is no "embedding is cheaper" claim in this document.** The earlier
version made one; it was an artifact of charging view's compose and encode
to view alone while crediting nvim's to the bracket. Corrected, the linux
advantage disappears into noise and the macos one reverses.

## The host inversion, and the reversion that proves it

dev-macos measures a worse p50 ratio than dev-linux despite being bare
metal. One stage accounts for it: `frame-prepared->size-probed`, which is
the single statement `self.inner.size()?` in `draw_surface` (grep
`crate::tap::TAG_SIZE_PROBED` in `crates/view-tui/src/terminal.rs`).
crossterm implements that as `open("/dev/tty")` + `ioctl(TIOCGWINSZ)` +
`close`, on every call (grep `window_size` in crossterm's
`src/terminal/sys/unix.rs`); view calls it once per painted frame.

That was previously argued by subtracting the stage from both totals. It
is now **measured by reversion**: a build in which the statement is
replaced by the resize-tracked size view already holds on the model,

```rust
// crates/view-tui/src/terminal.rs, in draw_surface
- let size = self.inner.size()?;
+ let size = ratatui::layout::Size::new(model.term_width, model.term_height);
```

was run through the same cells on both hosts. The patch was applied,
measured, and reverted; the tree carries no part of it.

### The stage collapses. That is the evidence; the total is not.

Instrumented build, decomposition cell. Each delta is given beside the
spread of the same measurement repeated without a source change, from
"What a repeat of the same run costs" above.

| | dev-linux | dev-macos |
|---|---|---|
| size-query stage, baseline -> reverted | 22.5 -> **0.6** | 190 -> **5** |
| **stage collapse** | **-21.9** | **-185** |
| baseline-to-baseline spread of that stage | 1.2 | 2 |
| collapse as a multiple of the spread | **18x** | **92x** |
| view total, baseline -> reverted | 696.8 -> 675.8 | 1513 -> 1292 |
| view total drop | -21.0 | -221 |
| baseline-to-baseline spread of the total | **31.1** (8.4 back-to-back) | **6** |
| total drop as a multiple of the spread | **0.7x** | 37x |
| nvim total, baseline -> reverted | 531.0 -> 526.1 | 1076 -> 1079 |

**Read the stage row, not the total row.** The stage collapse is 18x and
92x its own replicate spread on the two hosts and is the claim this
document stakes. The total row is a different matter on each host:

- **dev-linux: the total proves nothing.** The -21.0 us total drop is
  *smaller* than the 31.1 us spread between four baseline runs of
  identical source. Even against the tightest available figure — 8.4 us
  between three back-to-back replicates — one baseline/reverted pair does
  not separate the effect from drift at the total level, and -21.0 on
  696.8 is 3.0%, exactly at this project's cross-binary reporting floor,
  which applies because baseline and reverted are separately compiled
  binaries. The stage-level collapse is not in that position: it is a
  97% change on a two-tag span whose only content is the statement being
  removed.
- **dev-macos: the total drops 36 us MORE than the stage, and that
  overshoot is unexplained.** -221 total against -185 stage. It is **6x
  that host's 6 us baseline-to-baseline total spread**, so it is not
  run-to-run drift; but it is 2.4% of the total, *below* the 3%
  cross-binary floor that applies to any comparison of two separately
  compiled binaries, so codegen layout is a sufficient explanation and
  nothing distinguishes it from one. It is not evidence for anything and
  no mechanism is proposed for it. Recorded under "What this does not
  explain" and left open. The claim on this host is the -185 stage
  collapse, which is 92x the stage's own replicate spread and 12.2% of
  the round trip.

The paired control barely moved across the same pairs: nvim total -4.9 us
(linux) and +3 us (macos), under 1% either way. On dev-macos that rules
host drift out as the explanation for the -221. On dev-linux it does not
rescue the total-level number, which is inside its own replicate spread
whatever the control did.

**This settles the 146 us that an earlier version of this document
reported as unattributed on dev-macos.** That figure came from a
standalone probe reproducing only a third of the stage; the concern was
that the other two thirds might be co-located scheduling time that
removing the call would not recover. It is not: removing the call
collapses the stage from 190 us to 5 us.

### The counterfactual, in the quantity the spec records, replicated and interleaved

The reversal was first taken on one baseline/reverted pair per host. One
pair cannot distinguish an effect from drift, so it was repeated as an
**interleaved replicate campaign**: the two builds alternate run by run
(baseline, reverted, baseline, reverted, …) so that any drift over the
campaign cannot align with one arm, and each run takes its own null-pair
calibration. Normal build, gated `echo/minimal`, 1000 samples x 3 trials
per run.

**dev-macos — three interleaved pairs, six runs, in the order shown:**

| # | arm | load start -> end | calibration | `ratio_p50` | calibrated |
|---|---|---|---|---|---|
| 1 | baseline | 1.76 -> 1.82 | 1.0056 | 1.333 | 1.3257 |
| 2 | reverted | 1.84 -> 2.04 | 0.9864 | 1.141 | 1.1567 |
| 3 | baseline | 1.88 -> 1.53 | 0.9924 | 1.345 | 1.3553 |
| 4 | reverted | 1.48 -> 1.38 | 0.9657 | 1.135 | 1.1753 |
| 5 | baseline | 1.91 -> 2.12 | 1.0227 | 1.350 | 1.3200 |
| 6 | reverted | 2.27 -> 2.20 | 0.9993 | 1.130 | 1.1308 |

| dev-macos, raw `ratio_p50` | value |
|---|---|
| baseline arm | 1.333, 1.345, 1.350 — **spread 0.017 (1.3%)** |
| reverted arm | 1.130, 1.135, 1.141 — **spread 0.011 (0.8%)** |
| per-pair step | -0.192, -0.210, -0.220 |
| separation between the arms | 0.192, i.e. **11x the wider arm's spread** |

The two arms do not overlap, and every one of the three pairs steps the
same way. Calibrated the separation narrows but survives: baseline
1.3200-1.3553, reverted 1.1308-1.1753, a 0.145 gap against a worst-case
arm spread of 0.045 (**3.2x**).

**dev-linux — two interleaved pairs, four runs, in the order shown:**

| # | arm | load start -> end | calibration | `ratio_p50` | calibrated |
|---|---|---|---|---|---|
| 1 | baseline | 0.33 -> 0.47 | 1.0619 | 1.298 | 1.2223 |
| 2 | reverted | 0.47 -> 0.31 | 0.9807 | 1.250 | 1.2746 |
| 3 | baseline | 0.37 -> 0.47 | 0.9159 | 1.272 | 1.3888 |
| 4 | reverted | 0.51 -> 0.45 | 1.0987 | 1.223 | 1.1131 |

| dev-linux, raw `ratio_p50` | value |
|---|---|
| baseline arm | 1.272, 1.298 — spread 0.026 (2.0%) |
| reverted arm | 1.223, 1.250 — spread 0.027 (2.2%) |
| per-pair step | **-0.048, -0.049** |
| separation between the arms | 0.022, i.e. **0.8x** the arm spread |

dev-linux behaves differently and the difference is instructive. The two
arms barely separate — 0.022 against an arm spread of 0.026 — yet the
*paired* step is -0.048 and -0.049, agreeing to 0.001. That is what
interleaving buys: both arms drift together between pairs, and the
within-pair difference is stable even though the absolute level is not.
The earlier non-interleaved dev-linux runs give the same step size
(baseline 1.281 / 1.285, reverted 1.238 / 1.238), so six dev-linux runs
now put the raw effect at -0.045 to -0.049, or **3.7%** of the ratio.
Calibrated, dev-linux is unusable here and says so: the calibrations
taken beside these four runs span 0.9159 to 1.0987 (20%), which is five
times the effect, and dividing by them flips the sign of the step within
pair 1. Nothing on dev-linux should be read from the calibrated column.

**The host inversion, and whether it reverses.** Both hosts measured this
session on the normal build, raw:

| | dev-linux | dev-macos | verdict |
|---|---|---|---|
| baseline `ratio_p50`, arm range (median) | 1.272-1.298 (1.285) | 1.333-1.350 (1.345) | dev-macos **worse**, +4.7% on the medians (+2.7% to +6.1% at the range extremes); ranges do not overlap, separated by 0.035 = 1.3x the linux arm spread |
| reverted `ratio_p50`, arm range (median) | 1.223-1.250 (1.237) | 1.130-1.141 (1.135) | dev-macos **better**, -8.2% on the medians (-6.7% to -9.6% at the extremes); ranges do not overlap, separated by 0.082 = 3x the linux arm spread |

**The inversion reverses**, and now with per-arm spreads on both hosts
rather than one pair. The two legs are not equally strong and the weaker
one is named: the reverted leg separates by 3x the widest arm spread, the
baseline leg by only 1.3x. So "with the size query removed, dev-macos
measures better than dev-linux" is the firmer half of the statement, and
"with it present, dev-macos measures worse" is the half that a noisier
dev-linux could erase.

What would have overturned this and did not: any pair stepping the wrong
way, arms overlapping on dev-macos, or a per-pair step on dev-macos
smaller than the 0.017 arm spread. None occurred over six dev-macos runs
and four dev-linux runs.

Two limits stay attached to this result. Baseline and reverted are
**separately compiled binaries**, so this project's 3% cross-binary floor
applies to the step within each pair: dev-macos clears it by a factor of
five (**14.4% / 15.6% / 16.3%** over the three pairs), dev-linux only just
(**3.7% / 3.9%**). And the reversal is a statement about `ratio_p50` only — see
"The inversion is a p50 phenomenon" below.

The same comparison on the instrumented build agrees in direction:
calibrated 1.3255 (linux) and 1.3768 (macos) baseline, 1.2427 and 1.2133
reverted. That build is not replicated per arm and is not what the claim
rests on; it is reported as agreeing, not as evidence.

### A caveat on the calibration, with the evidence for it

Dividing by the null-pair calibration is required and is done above, but
on this evidence the calibration is a **noisier estimator than the
quantity it corrects**, and on dev-linux it consumes the entire effect.
Four dev-linux normal-build runs, taken before the interleaved campaign
above and agreeing with it on every raw figure:

| run | load | calibration | `ratio_p50` |
|---|---|---|---|
| baseline | 0.22 | 0.9233 | 1.281 |
| baseline | 0.32 | 1.0089 | 1.285 |
| reverted | 0.22 | 1.0857 | 1.238 |
| reverted | 0.44 | 0.9725 | 1.238 |

The two baseline ratios agree to 0.3% and the two reverted ratios agree to
0.0%, while the calibrations taken beside them span 0.9233 to 1.0857
(17.6%). The interleaved campaign reproduced that independently: its four
dev-linux calibrations span 0.9159 to 1.0987 (20.0%) beside raw ratios
whose paired step agrees to 0.001. The calibration is one 200-sample single-trial null pair; the
ratio is a 3-trial median of 1000-sample paired runs. On dev-linux the
size query is 3.2% of view's total, so dividing by a +/-9% estimator turns
a reproducible -3.7% into nothing. On dev-macos the calibrated effect
is 14.3%, far outside that spread, and survives easily.

The honest reading: the calibration's designed job is to **refuse** a
noisy host (deviation above 1.15), which it does. It is not precise enough
to be a correction factor at this effect size on dev-linux. Both the raw
and the calibrated columns are given above; they agree on the direction
and on the reversal.

### The inversion is a p50 phenomenon, and only that

The echo row records four metrics, three of them tail metrics. Measured
this session, with the size query fully present:

| | dev-linux | dev-macos |
|---|---|---|
| `ratio_p50` | 1.285 | 1.362 |
| `ratio_p99` | 1.224 | **0.943** |

At p99, dev-macos is already ahead, and by a wide margin -- view's p99
there is *better* than bare nvim's. The recorded baselines say the same
(`ratio_p99` 1.307 linux, 1.194 macos). Everything this document says
about the host inversion is **about the median only**. Whatever produces
the tails on both hosts is a different question that this decomposition
does not answer, and it is not a question the size query is involved in.

## Corroborating measurements

Each was run to falsify a hypothesis rather than confirm one.

**The paint stage is not compositing CPU.** If it were, the paint
micro-bench -- which measures exactly that CPU -- would be several times
worse on dev-macos. It is not:

| | dev-linux (load 0.08) | dev-macos (load 1.86) | ratio |
|---|---|---|---|
| `paint_frame_steady_state_crossterm` | 2.848 us | 3.322 us | 1.17 |
| `paint_frame_full_recomposite` | 75.7 us | 81.3 us | 1.07 |

**It is not the frame preamble either.** `draw-start->frame-prepared`
measures 1.3 us (linux) and 6 us (macos); the cost is inside the one call.

**The syscall triple is most of it, independently.** A standalone C probe
outside the repo runs crossterm's exact three syscalls in a process with
no view, no nvim and no taps:

| | hot loop | 10 ms cadence | in-situ stage |
|---|---|---|---|
| dev-linux (load 0.35) | 0.84 us | 17.5 us | 22.5 us |
| dev-macos (load 1.37) | 17.0 us | 107.0 us | 190 us |

The cold-cadence probe accounts for 78% of the dev-linux stage and 56% of
the dev-macos one. The remainder is a property of the calling process, not
of the stage's attribution: the reversion above recovers the whole stage
on both hosts either way.

**The falsified hop theory stays falsified.** The cross-thread wake
microtest measures 4.4 us (linux) and 1.2 us (macos) and predicts the
in-situ handoffs should be ~3.6x cheaper on dev-macos. Measured in situ
they are near equal: 96.8 us (linux) vs 99 us (macos) for the three
input-side handoffs. The microtest does not measure the same event, which
is why no floor should ever have been derived from it. No hop-cost story
appears anywhere in this attribution.

## Instrumentation cost, and what it forbids

The decomposition runs the `bench-taps` build; the gated `echo` row runs
the normal build. Measured on both hosts, this session, at the loads in
the run-conditions table:

| | dev-linux | dev-macos |
|---|---|---|
| normal-build view p50, baseline | 689 us | 1485 us |
| taps-build view p50, baseline | 696.8 us | 1513 us |
| **instrumentation inflation, view side** | **+1.1%** | **+1.9%** |
| normal-build nvim p50 over the same runs | 549 us | 1090 us |
| taps-build nvim p50 over the same runs | 531.0 us | 1076 us |
| direct tap cost (12 records x measured p50 overhead) | 3.3 us (0.5%) | 8.5 us (0.6%) |

The nvim side carries no instrumentation and still moved 1-3% between the
same runs, and the two builds are separately compiled, which this
project's noise model puts at a 3% codegen-layout floor. The view-side
inflation this session is +1.1% and +1.9% -- close enough between hosts
that no host-dependent correction is warranted, which was not true of an
earlier reading that measured +6.4% and +3.9% and then compared taps-build
ratios across hosts without correcting.

**Consequence, and it is honoured throughout: no taps-build sum may be
equated with a normal-build number.** Concretely, the gap in each build:

| | dev-linux | dev-macos |
|---|---|---|
| taps-build gap (view total - nvim total) | 165.8 us | 437 us |
| **normal-build gap (view p50 - nvim p50)** | **140 us** | **395 us** |
| taps-build gap is high by | 18% | 11% |

Every per-stage percentage in this document is a percentage of the
**taps-build** total and is labelled as such where it appears. The
normal-build gap above is the number to reason against for anything the
gated row will be judged on.

## Does this share a cause with the `input_path` gap?

Partly, and the shared part is the smaller part of each. All figures in
this section are taps-build figures on both sides, so they are
commensurate with each other; they are not commensurate with a
normal-build budget.

`input_path` is `t0 -> W`, which is the first four stages: 183.1 us p50 on
dev-linux (86.3 + 50.8 + 13.8 + 32.2). That row's own gated statistic is a
p99 against a p99 budget; these are p50s, so no budget comparison is drawn
from them here. An earlier version of this document set this p50 against
that p99 budget, which compares two different statistics.

- **Shared:** the three handoffs `key-decoded->loop-wake`,
  `loop-wake->rpc-handoff` and `rpc-handoff->rpc-written`, 96.8 us on
  dev-linux. That is 53% of `input_path` and 65% of the echo gap's
  view-only total. Work here moves both rows.
- **`input_path` only:** `pty->key-decoded`, 86.3 us, the other 47% of
  that row. It moves the echo gap far less, because bare nvim pays to
  receive the keystroke too and the stage sits inside a bracket that is
  within noise of nvim's total on dev-linux.
- **echo only:** the view-only stages from `redraw-parsed->loop-wake`
  onward, 53.2 us on dev-linux and 244 us on dev-macos, which
  `input_path` never crosses. The whole host inversion lives here.

On dev-macos the two rows barely overlap: `input_path`'s stages come to
142 us while the echo gap's view-only total is 343 us, 190 of which is a
stage `input_path` does not contain.

## What this says about the spec's section 3.1 presumption

Spec 3.1 records the cause as open and presumes the residual lives in the
RPC/UI-protocol process boundary. The corrected measurement does **not**
settle that presumption in either direction, and this document proposes no
amendment.

- Against the presumption: bare nvim runs the same two-process msgpack-RPC
  architecture, so the boundary is not a cost view pays and bare nvim
  escapes; and on dev-linux the bracket containing it is within 1.5% of
  bare nvim's whole round trip, below the reporting floor.
- For the presumption: on dev-macos that same bracket is 6.1% **against**
  view, which is a real excess on the boundary side and is not explained
  here.
- Independent of both: the single largest named cost on dev-macos, and the
  one measured to reverse the host inversion, sits on the paint side, well
  after the boundary.

The presumption stands as unproven either way. Amending the spec is the
coordinator's and the user's call, not this document's.

## Candidates, with measured cost (not fixed here)

1. **The per-frame terminal size query**, 22.5 us (linux) / 190 us (macos)
   p50 per painted frame -- 3.2% and 12.6% of the **taps-build** round
   trip. Bare nvim makes the call once at startup and zero times per
   frame; view already receives `Event::Resize` on its input thread and
   turns it into `Msg::Resized` (grep `Event::Resize` in
   `crates/view-tui/src/terminal.rs`), and `model.term_width` /
   `model.term_height` already carry the result, so the size is a value
   view holds and re-derives per frame from a syscall triple. Largest
   single candidate on both hosts, the only one measured to reverse the
   inversion, and the only stage with a bare-nvim comparison of exactly
   zero.
2. **The four channel handoffs**, 117.3 us (linux) / 127 us (macos)
   pooled. `key-decoded->loop-wake` (50.8 / 44) and
   `rpc-handoff->rpc-written` (32.2 / 34) are the two largest. They are
   the bulk of what makes view's pty-to-RPC share 28.8% of its round trip
   against bare nvim's 10.9%. They do **not** track the cross-thread wake
   microtest, which is an order of magnitude smaller and inverted between
   the hosts.
3. **The dev-macos bracket excess, +66 us (6.1% of bare nvim's whole round
   trip)**, and it is a candidate only in the sense that it is a measured
   cost with no owner yet. After the corrected classification, the stages
   that have a bare-nvim counterpart come to 1142 us on dev-macos against
   bare nvim's 1076 us. On dev-linux the same comparison is -8.2 us, below
   the 3% reporting floor, so the excess exists on one host only. It is the
   **largest unexplained item on the host this document's headline claims
   rest on**, and it sits on the boundary side rather than the paint side.

   One arithmetic hint exists and is worth recording with its assumption
   attached. Bare nvim's own round trip is 2.03x slower on dev-macos
   (1076 vs 531). *If* that host factor applied uniformly to view's bracket
   stages — an assumption nothing here verifies — the scaled dev-linux
   bracket would be 1059 us against the measured 1142, and the excess would
   decompose as `rpc-written->redraw-parsed` **+241**, partly offset by
   `pty->key-decoded` **-132** and `term-written->glyph-seen` **-47**. That
   points at the RPC span, which is what spec 3.1 presumes; it is not
   evidence for the presumption, because a uniform host factor is exactly
   what a two-host comparison cannot assume. Testing it needs per-stage
   bare-nvim timings on dev-macos, which `dtruss` cannot supply on that host
   without SIP disabled. Until then the excess cannot be attacked, and it is
   the reason spec 3.1's presumption is reported unproven rather than
   falsified.
4. **crossterm's key read and decode**, the part of `pty->key-decoded`
   above the 31.9 us pty floor on dev-linux. Moves `input_path`; moves the
   echo gap much less.

## What this does not explain

- **The dev-macos bracket excess: +66 us, 6.1% of bare nvim's whole round
  trip.** After the corrected classification the stages that have a
  bare-nvim counterpart come to 1142 us on dev-macos against bare nvim's
  1076 us. dev-linux shows -8.2 us for the same comparison, below the 3%
  floor, so this is a one-host effect. It is the largest single unexplained
  quantity in this document and it is on the host every headline claim here
  rests on. Nothing in this decomposition localizes it: the only available
  decomposition of it assumes a uniform host factor, which is precisely
  what cannot be assumed (see candidate 3). Per-stage bare-nvim timings on
  dev-macos would settle it and are not obtainable without disabling SIP.
- **Why the dev-macos view total drops 36 us more than the stage it
  removes** (-221 against -185). That overshoot is 6x the host's 6 us
  baseline-to-baseline spread, so it is not run-to-run drift, but at 2.4%
  of the total it is below the 3% cross-binary floor that applies between
  two separately compiled binaries, so codegen layout is a sufficient
  explanation and nothing here distinguishes it from one. No mechanism is
  claimed for it.
- **The tail.** This attribution is p50 throughout. At p99 the host
  inversion does not exist -- dev-macos measures `ratio_p99` 0.943, better
  than bare nvim -- and `term-written->glyph-seen` reaches 727 us p99 on
  dev-macos against a 29 us p50. That stage is harness observation, and
  the multiplicity counters now make that more than an assertion: a
  spurious redraw round is exactly the mechanism that would park real view
  time in that stage, and it is counted zero on all 3000 samples. What
  produces the tails on either host is unmeasured.
- **Why bare nvim is itself twice as slow on the M1 Max** (1076 us vs
  531 us). That is a property of the baseline, not of view, but it sets
  the denominator every macOS ratio is divided by.
- **Why the size query costs more inside view's process than in a minimal
  one** (56% of the stage reproduced by the standalone probe on
  dev-macos). Localizing it needs a probe reproducing view's process
  shape. It does not affect the attribution or the candidate: the
  reversion recovers the whole stage either way.
- **Per-stage bare-nvim timings on dev-macos.** `dtruss` requires SIP
  disabled there. Not measured; nothing here depends on them.

## What earlier readings of this got wrong

Recorded so the same claims are not re-derived.

- **"Embedding is 29.7 us and 14 us cheaper than bare nvim."** Withdrawn.
  It charged view's compose and encode to view alone while crediting bare
  nvim's to the bracket. Corrected, the linux figure is -8.2 us (1.5%,
  below the reporting floor) and the macos figure is +66 us against view.
- **"Two facts fall out of [the reconciliation table]."** Withdrawn.
  The table is an arithmetic identity that closes for any classification.
- **"1.291 / 1.245, the ratio without the size query."** Withdrawn as a
  method: those were `(total - stage) / nvim` arithmetic, not measurement,
  and they invoked percentile non-additivity to excuse the residual while
  relying on percentile additivity to compute themselves. The reversion
  above replaces them with a measured build.
- **"146 us of the dev-macos size-query stage is unattributed."**
  Withdrawn. The reversion recovers the whole stage.
- **The headline dev-macos table taken at load 1.94 with a quieter run
  relegated to a parenthetical.** Corrected: the headline is now the
  quietest run achievable on that host, and the two runs are reported side
  by side because they agree within 0.4%.
- **"view spends roughly 2.6x bare nvim's share of the round trip on both
  ends of the RPC seam," read off shares said to be "immune to the
  inflation factor."** Both withdrawn. ptrace overhead is per syscall
  boundary and roughly constant per boundary, so normalizing to shares does
  not cancel it, and the four nvim stages hold unequal boundary counts
  (1, ~4, 1, 1). No multiplier is quotable from that trace; see "Why no
  multiplier is quoted from these numbers".
- **"The view total drops by the stage's magnitude on both hosts."**
  Withdrawn as evidence. On dev-linux the total-level drop (-21.0 us) is
  smaller than the spread between four baseline runs of identical source
  (31.1 us), so it establishes nothing; on dev-macos it over-delivers by
  36 us against the stage. The stage-level collapse is the evidence and
  is stated as such.
- **"and slightly more"** as the description of the dev-macos overshoot.
  Withdrawn: the number is 36 us, it is 6x that host's baseline-to-baseline
  spread, and it is named as unexplained rather than absorbed.
- **"a socketpair"** as bare nvim's internal transport. Wrong. `/proc`
  shows two distinct `pipe:` inodes, one per direction, on both processes:
  two unidirectional pipes.
- **"Load average on this host is therefore not a CPU-contention measure"
  and "this is a residual limitation."** Both were weaker than the
  evidence. 1.42 runnable on 10 cores with 87.4% idle measured beside it
  is 14% utilization; the two figures agree and together they **rule out**
  CPU contention rather than leaving a limitation open.
- **"The nvim side is an independent drift monitor."** Withdrawn. Bare
  nvim runs on the same host, interleaved sample-by-sample with an
  instrumented view emitting 12 FIFO writes per frame. It is a *paired
  control*, which is what makes the ratio meaningful, and it is not
  independent of view's run.

## Should the instrumentation stay in the tree? Yes

- **Off by default, by construction.** `bench-taps` appears in no
  `default` feature list. The tap *module itself* is `#[cfg(feature =
  "bench-taps")]` in both `view-tui` and `view-engine`, so an unguarded
  call site does not merely add cost -- it fails to compile.
- **That moat is now checked by a test in the tree** rather than by
  whoever last ran `cargo check` by hand (grep
  `no_tap_reaches_a_default_build` in
  `crates/view-bench/src/scenarios/taps.rs`). It walks every measured
  crate's sources and asserts three things the compiler moat rests on:
  every tap call site is compiled out of a default build, every tap module
  declaration is feature-gated, and no `default` feature set reaches
  `bench-taps`. Sabotage-verified against all four failure shapes --
  unguarding a direct call site, unguarding an enclosing block, ungating a
  module, and adding `default = ["bench-taps"]` -- each turns it red, and
  restoring turns it green.
- **The harness-side rows cannot leak into a measured build at all**: they
  live in `view-bench` and `view-harness`, which no shipping binary links.
- **Chain integrity is tested, sabotage-verified, and self-reported per
  run**: unresolved chains, ambiguous loop wakes, and per-tag redraw-round
  multiplicity are counted and printed, and the run warns loudly rather
  than publishing percentiles from a window that held two rounds.
