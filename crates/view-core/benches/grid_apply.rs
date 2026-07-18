//! Hot-path micro-bench: `Grid::apply` under a full-frame `PutLine` sweep,
//! the same shape of traffic one `grid_line`-heavy redraw batch produces
//! (a full-screen repaint, e.g. `:e` on a new file or a scrollback-filling
//! paste). Recorded, not gated: the gate lives with the phase that consumes
//! this number as a regression budget.

use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;
use view_core::grid::{Grid, GridOp};

const WIDTH: u16 = 120;
const HEIGHT: u16 = 40;

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

criterion_group!(benches, bench_grid_apply);
criterion_main!(benches);
