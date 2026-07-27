# The paint path's cost is cache residency, and every hot micro-bench understates it 7x

Date: 2026-07-27
Class: dev-linux (12-core VM, quiet)
Bench: `cargo bench -p view-tui --bench paint_frame`

## The discrepancy

The tapped production run puts the two paint stages view owns at
**28.4 us p50**:

```
area-resolved->composed    13.8 us p50   (damage resolution + composite)
composed->flush-start      14.6 us p50   (backend diff + escape encode)
```

`paint_frame_steady_state_crossterm` runs that same work, over the same
120x40 grid, with one cell changed and a real `CrosstermBackend` doing real
escape encoding. It measures **2.9 us**. Tap overhead does not explain a
10x gap: it is 0.288 us p50 / 1.329 us p99 against a 5 us bar.

## The test

`paint_frame_cold/steady_state_crossterm_cold` is the identical frame with
one change: a 10 ms sleep before each one, and only the frame itself timed.
10 ms is what the `echo` row paces at, and a human types slower still.

| variant | timed span | cost |
|---|---|---|
| `steady_state_crossterm` (back to back) | apply_grid + render + damage + compose + emit | **2.94 us** |
| `steady_state_crossterm_cold` (10 ms gap) | render + damage + compose + emit | **21.27 us** (CI 20.0-23.1) |

**7.2x, from idle alone.** The cold variant measures strictly *less* work
than the hot one, because `apply_grid` sits outside its timed span, so the
ratio is if anything understated.

The residue against production's 28.4 us is the work the bench does not
model: the mouse-capture and sync-bracket queueing, cursor positioning and
shape, the `Rc<RefCell>` frame buffer, and a real damage set that is
several rows rather than one.

## What this means, and what it does not

**It is the same lesson as the input path, one level down.** There the cost
of a thread hop turned out to be a function of how deeply the receiving core
had parked. Here the cost of pure CPU work turns out to be a function of how
long the caches, branch predictors and page mappings have been left alone.
Both are invisible to a benchmark that loops, and steady typing presents
neither loop.

**Every criterion micro-bench in this repo has this defect.**
`grid_apply`, `update_key`, `damage_fold`, `render_frame` and the three hot
`paint_frame` functions all measure a state the editor never occupies. They
remain valid as *relative* instruments -- a change that makes the hot number
worse made the code worse -- and they are wrong as *absolute* costs. The
damage-clipping work quoted `paint_frame_steady_state` as its "after"
number; that number is real, and it is about 7x optimistic as a description
of what a keystroke pays.

**The lever this identifies is not a tuning one.** The cost is proportional
to memory touched per frame, not to instructions executed, so the way to
reduce it is to touch less: today `view_surface::render` builds a fresh
full-screen `Surface` on every frame even when a single cell changed. That
is a deliberate property of the Elm-style runtime, not an oversight, and
trading it for incremental rendering is an architectural decision rather
than an optimization. It is written up as a pitch in `.claude/HANDOFF.md`
section 5.8 rather than taken unilaterally.

## Reproducing

```bash
cargo bench -p view-tui --bench paint_frame
task bench -- --scenario echo_path --fixture minimal --class dev-linux
```
