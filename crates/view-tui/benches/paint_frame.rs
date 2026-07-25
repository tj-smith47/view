//! Hot-path micro-bench: the per-frame paint the runtime loop pays on every
//! redraw -- `render()`'s `Surface`, a composite into the persistent shadow,
//! then the shadow's diff of what changed against what the terminal shows --
//! against a populated 120x40 grid.
//!
//! Three functions bracket the damage-clipping lever:
//!
//! - `paint_frame_full_recomposite` recomposites every cell and diffs the
//!   whole grid each frame (the pre-clipping behavior), the "before" number.
//! - `paint_frame_steady_state` mutates one cell per iteration (the echo
//!   scenario's per-keystroke damage shape) and composites only the damaged
//!   row, the "after" number for a single typed character.
//! - `paint_frame_steady_state_crossterm` runs the clipped path over a real
//!   `CrosstermBackend` so its escape-encoding cost is included.
//!
//! All three absorb terminal write syscalls into their backend.

#![allow(clippy::expect_used)]

use criterion::{criterion_group, criterion_main, Criterion};
use ratatui::backend::{Backend, TestBackend};
use ratatui::layout::Rect;
use std::hint::black_box;
use view_core::grid::GridOp;
use view_core::model::Model;
use view_tui::paint::{overlay_rows, Damage, Shadow};

const WIDTH: u16 = 120;
const HEIGHT: u16 = 40;

/// A model whose grid looks like a real editing session: full-width text
/// rows under a handful of highlight ids, so the compositor's per-cell style
/// resolution runs against a populated table rather than an all-default
/// fast path.
fn populated_model() -> Model {
    let mut model = Model::new();
    model.engine.apply_grid(GridOp::Resize {
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
        model.engine.apply_grid(GridOp::PutLine {
            row,
            col_start: 0,
            cells,
        });
    }
    model.content_painted = true;
    model
}

/// One production-shaped frame: composite `damage` into the shadow, emit the
/// cells that changed against what the terminal shows, then promote the
/// composed frame -- exactly the sequence `Term::draw_surface` runs.
fn emit_frame<B: Backend>(
    backend: &mut B,
    shadow: &mut Shadow,
    model: &Model,
    surface: &view_surface::Surface,
    damage: &Damage,
) {
    shadow.compose(model, surface, damage);
    let _ = shadow.emit_updates(backend);
    shadow.commit();
}

/// A shadow sized for the bench grid with one full frame already painted, so
/// every measured iteration starts from a populated terminal rather than a
/// blank one.
fn primed_shadow<B: Backend>(backend: &mut B, model: &mut Model) -> (Shadow, Vec<u16>) {
    let mut shadow = Shadow::new();
    shadow.resize(Rect::new(0, 0, WIDTH, HEIGHT));
    let _ = model.take_paint_damage();
    let surface = view_surface::render(model);
    let overlay = overlay_rows(&surface);
    emit_frame(backend, &mut shadow, model, &surface, &Damage::full());
    (shadow, overlay)
}

/// The "before" reference: recomposite and diff every cell each frame, the
/// whole-grid cost the clipping lever removes.
fn bench_paint_frame_full(c: &mut Criterion) {
    let mut model = populated_model();
    let mut backend = TestBackend::new(WIDTH, HEIGHT);
    let (mut shadow, _) = primed_shadow(&mut backend, &mut model);

    let mut flip = false;
    c.bench_function("paint_frame_full_recomposite", |b| {
        b.iter(|| {
            flip = !flip;
            model.engine.apply_grid(GridOp::PutLine {
                row: 5,
                col_start: 5,
                cells: vec![((if flip { "x" } else { "y" }).to_string(), 0, 1)],
            });
            let _ = model.take_paint_damage();
            let surface = view_surface::render(&model);
            emit_frame(
                black_box(&mut backend),
                black_box(&mut shadow),
                &model,
                &surface,
                &Damage::full(),
            );
        });
    });
}

/// The "after" number: one changed cell per frame, composited and diffed
/// through the same row-clipped `Damage` the runtime builds.
fn bench_paint_frame(c: &mut Criterion) {
    let mut model = populated_model();
    let mut backend = TestBackend::new(WIDTH, HEIGHT);
    let (mut shadow, mut prev_overlay) = primed_shadow(&mut backend, &mut model);

    let mut flip = false;
    c.bench_function("paint_frame_steady_state", |b| {
        b.iter(|| {
            // one changed cell per frame, alternating so no iteration is a
            // no-op diff
            flip = !flip;
            model.engine.apply_grid(GridOp::PutLine {
                row: 5,
                col_start: 5,
                cells: vec![((if flip { "x" } else { "y" }).to_string(), 0, 1)],
            });
            let grid_damage = model.take_paint_damage();
            let surface = view_surface::render(&model);
            let cur_overlay = overlay_rows(&surface);
            let damage = Damage::from_frame(
                &grid_damage,
                model.chrome_rows(),
                &prev_overlay,
                &cur_overlay,
                false,
            );
            prev_overlay = cur_overlay;
            emit_frame(
                black_box(&mut backend),
                black_box(&mut shadow),
                &model,
                &surface,
                &damage,
            );
        });
    });
}

/// Same clipped steady-state frame, but through `CrosstermBackend` over an
/// in-memory writer: includes the real escape-sequence generation the
/// production path pays, still minus terminal write syscalls, so the
/// difference against `paint_frame_steady_state` isolates crossterm's
/// per-frame encoding cost.
fn bench_paint_frame_crossterm(c: &mut Criterion) {
    let mut model = populated_model();
    let mut backend = ratatui::backend::CrosstermBackend::new(Vec::<u8>::new());
    let (mut shadow, mut prev_overlay) = primed_shadow(&mut backend, &mut model);

    let mut flip = false;
    c.bench_function("paint_frame_steady_state_crossterm", |b| {
        b.iter(|| {
            flip = !flip;
            model.engine.apply_grid(GridOp::PutLine {
                row: 5,
                col_start: 5,
                cells: vec![((if flip { "x" } else { "y" }).to_string(), 0, 1)],
            });
            let grid_damage = model.take_paint_damage();
            let surface = view_surface::render(&model);
            let cur_overlay = overlay_rows(&surface);
            let damage = Damage::from_frame(
                &grid_damage,
                model.chrome_rows(),
                &prev_overlay,
                &cur_overlay,
                false,
            );
            prev_overlay = cur_overlay;
            emit_frame(
                black_box(&mut backend),
                black_box(&mut shadow),
                &model,
                &surface,
                &damage,
            );
        });
    });
}

criterion_group!(
    benches,
    bench_paint_frame_full,
    bench_paint_frame,
    bench_paint_frame_crossterm
);
criterion_main!(benches);
