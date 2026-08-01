//! Hot-path micro-bench: `render()` building a full `Surface` from a
//! populated `Model` -- grid content plus every overlay layer (tabline,
//! cmdline, messages, popupmenu) live at once, the worst-case layer count
//! a single frame pays for. Recorded, not gated: the gate lives with the
//! phase that consumes this number as a regression budget.

use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;
use std::time::{Duration, Instant};
use view_core::events::{PmItem, TabEntry, TabHandle, UiEvent};
use view_core::grid::GridOp;
use view_core::model::Model;
use view_core::msg::Msg;
use view_core::update::update;

/// Idle between renders in the cold variant. `.claude/measurements/
/// 2026-07-27-the-paint-path-is-cold-cache-not-instructions.md` found the
/// paint path's per-frame cost tracks cache residency rather than
/// instruction count; the `echo` bench row paces keystrokes at this
/// interval and a human types slower still, so this is the gap the editor
/// actually presents to a hot loop rather than a pathological one.
const KEYSTROKE_GAP: Duration = Duration::from_millis(10);

fn full_model() -> Model {
    let mut model = Model::new();
    model.engine.apply_grid(GridOp::Resize {
        width: 120,
        height: 40,
    });
    model.engine.apply_grid(GridOp::PutLine {
        row: 0,
        col_start: 0,
        cells: vec![("x".to_string(), 0, 120)],
    });

    let _ = update(
        &mut model,
        Msg::Redraw(vec![
            UiEvent::TablineUpdate {
                current: TabHandle(1),
                tabs: vec![
                    TabEntry {
                        tab: TabHandle(1),
                        name: "one.rs".into(),
                    },
                    TabEntry {
                        tab: TabHandle(2),
                        name: "two.rs".into(),
                    },
                ],
            },
            UiEvent::CmdlineShow {
                content: vec![(0, "wq".to_string())],
                pos: 2,
                firstc: ":".to_string(),
                prompt: String::new(),
                indent: 0,
                level: 1,
            },
            UiEvent::MsgShow {
                kind: "echomsg".to_string(),
                content: vec![(0, "benchmark message".to_string())],
                replace_last: false,
            },
            UiEvent::PopupmenuShow {
                items: vec![
                    PmItem {
                        word: "foo".into(),
                        kind: "Function".into(),
                        menu: String::new(),
                        info: String::new(),
                    },
                    PmItem {
                        word: "bar".into(),
                        kind: "Variable".into(),
                        menu: String::new(),
                        info: String::new(),
                    },
                ],
                selected: 0,
                row: 1,
                col: 0,
                grid: 0,
            },
        ]),
    );
    model
}

fn bench_render_frame(c: &mut Criterion) {
    let model = full_model();

    c.bench_function("render_frame_full_model", |b| {
        b.iter(|| {
            let surface = view_surface::render(black_box(&model));
            black_box(surface);
        });
    });
}

/// The cold counterpart: the identical render, but with a keystroke
/// interval of idle before each one and only the render itself timed.
/// Follows `paint_frame.rs`'s cold pattern (an `iter_custom` loop bounded
/// by sample count rather than throughput).
fn bench_render_frame_cold(c: &mut Criterion) {
    let model = full_model();

    let mut group = c.benchmark_group("render_frame_cold");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(12));
    group.bench_function("full_model_cold", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                std::thread::sleep(KEYSTROKE_GAP);
                let started = Instant::now();
                let surface = view_surface::render(black_box(&model));
                black_box(surface);
                elapsed += started.elapsed();
            }
            elapsed
        });
    });
    group.finish();
}

criterion_group!(benches, bench_render_frame, bench_render_frame_cold);
criterion_main!(benches);
