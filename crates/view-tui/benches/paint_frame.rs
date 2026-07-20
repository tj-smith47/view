//! Hot-path micro-bench: the full per-frame paint the runtime loop pays on
//! every redraw -- `render()`'s `Surface`, then `composite()` into a ratatui
//! frame, then ratatui's own buffer diff -- against a populated 120x40 grid.
//! `steady_state` mutates one cell per iteration (the echo scenario's
//! per-keystroke damage shape), so its number is the paint-side CPU cost a
//! single typed character pays end to end, minus real terminal write
//! syscalls (`TestBackend` absorbs those).

use criterion::{criterion_group, criterion_main, Criterion};
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use std::hint::black_box;
use view_core::grid::GridOp;
use view_core::model::Model;
use view_tui::paint::composite;

const WIDTH: u16 = 120;
const HEIGHT: u16 = 40;

/// A model whose grid looks like a real editing session: full-width text
/// rows under a handful of highlight ids, so `composite`'s per-cell style
/// resolution runs against a populated table rather than an all-default
/// fast path.
fn populated_model() -> Model {
    let mut model = Model::new();
    model.engine.grid.apply(GridOp::Resize {
        width: WIDTH,
        height: HEIGHT,
    });
    for row in 0..HEIGHT {
        let cells = (0..WIDTH)
            .map(|col| {
                let ch = char::from(b'a' + ((row + col) % 26) as u8);
                (ch.to_string(), u64::from((row + col) % 7), 1)
            })
            .collect();
        model.engine.grid.apply(GridOp::PutLine {
            row,
            col_start: 0,
            cells,
        });
    }
    model.content_painted = true;
    model
}

fn bench_paint_frame(c: &mut Criterion) {
    let mut model = populated_model();
    let backend = TestBackend::new(WIDTH, HEIGHT);
    let mut terminal = Terminal::new(backend).expect("test backend terminal");

    let surface = view_surface::render(&model);
    terminal
        .draw(|f| composite(&model, &surface, f))
        .expect("priming draw");

    let mut flip = false;
    c.bench_function("paint_frame_steady_state", |b| {
        b.iter(|| {
            // one changed cell per frame, alternating so no iteration is a
            // no-op diff
            flip = !flip;
            model.engine.grid.apply(GridOp::PutLine {
                row: 5,
                col_start: 5,
                cells: vec![((if flip { "x" } else { "y" }).to_string(), 0, 1)],
            });
            let surface = view_surface::render(&model);
            let frame = terminal
                .draw(|f| composite(black_box(&model), black_box(&surface), f))
                .expect("draw");
            black_box(frame.area);
        });
    });
}

criterion_group!(benches, bench_paint_frame);
criterion_main!(benches);
