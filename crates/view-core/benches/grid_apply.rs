//! Hot-path micro-bench: `Grid::apply` under a full-frame `PutLine` sweep,
//! the same shape of traffic one `grid_line`-heavy redraw batch produces
//! (a full-screen repaint, e.g. `:e` on a new file or a scrollback-filling
//! paste). Recorded, not gated: the gate lives with the phase that consumes
//! this number as a regression budget.

use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;
use std::time::{Duration, Instant};
use view_core::grid::{Grid, GridOp};

const WIDTH: u16 = 120;
const HEIGHT: u16 = 40;

/// Idle between applies in the cold variants. `.claude/measurements/
/// 2026-07-27-the-paint-path-is-cold-cache-not-instructions.md` found the
/// paint path's per-frame cost tracks cache residency rather than
/// instruction count; the `echo` bench row paces keystrokes at this
/// interval and a human types slower still, so this is the gap the editor
/// actually presents to a hot loop rather than a pathological one.
const KEYSTROKE_GAP: Duration = Duration::from_millis(10);

fn full_frame_cells() -> Vec<(String, u64, u64)> {
    // one long run per row, matching how a real terminal-width redraw
    // batch typically arrives (a handful of runs per line, not one cell at
    // a time)
    vec![("x".to_string(), 1, u64::from(WIDTH))]
}

fn bench_grid_apply(c: &mut Criterion) {
    let mut grid = Grid::new();
    grid.apply(GridOp::Resize {
        width: WIDTH,
        height: HEIGHT,
    });
    let cells = full_frame_cells();

    c.bench_function("grid_apply_full_frame_put_line", |b| {
        b.iter(|| {
            for row in 0..HEIGHT {
                grid.apply(GridOp::PutLine {
                    row,
                    col_start: 0,
                    cells: black_box(cells.clone()),
                });
            }
            black_box(&grid);
        });
    });

    c.bench_function("grid_apply_scroll_full_width", |b| {
        b.iter(|| {
            grid.apply(GridOp::Scroll {
                top: 0,
                bot: HEIGHT,
                left: 0,
                right: WIDTH,
                rows: black_box(1),
            });
        });
    });
}

/// The cold counterpart of both functions above: the identical apply, but
/// with a keystroke interval of idle before each one and only the apply
/// itself timed. Follows `paint_frame.rs`'s cold pattern (an
/// `iter_custom` loop bounded by sample count rather than throughput).
fn bench_grid_apply_cold(c: &mut Criterion) {
    let mut grid = Grid::new();
    grid.apply(GridOp::Resize {
        width: WIDTH,
        height: HEIGHT,
    });
    let cells = full_frame_cells();

    let mut group = c.benchmark_group("grid_apply_cold");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(12));
    group.bench_function("full_frame_put_line_cold", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                std::thread::sleep(KEYSTROKE_GAP);
                let started = Instant::now();
                for row in 0..HEIGHT {
                    grid.apply(GridOp::PutLine {
                        row,
                        col_start: 0,
                        cells: black_box(cells.clone()),
                    });
                }
                black_box(&grid);
                elapsed += started.elapsed();
            }
            elapsed
        });
    });
    group.bench_function("scroll_full_width_cold", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                std::thread::sleep(KEYSTROKE_GAP);
                let started = Instant::now();
                grid.apply(GridOp::Scroll {
                    top: 0,
                    bot: HEIGHT,
                    left: 0,
                    right: WIDTH,
                    rows: black_box(1),
                });
                elapsed += started.elapsed();
            }
            elapsed
        });
    });
    group.finish();
}

criterion_group!(benches, bench_grid_apply, bench_grid_apply_cold);
criterion_main!(benches);
