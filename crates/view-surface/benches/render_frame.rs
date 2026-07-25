//! Hot-path micro-bench: `render()` building a full `Surface` from a
//! populated `Model` -- grid content plus every overlay layer (tabline,
//! cmdline, messages, popupmenu) live at once, the worst-case layer count
//! a single frame pays for. Recorded, not gated: the gate lives with the
//! phase that consumes this number as a regression budget.

use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;
use view_core::events::{PmItem, TabEntry, TabHandle, UiEvent};
use view_core::grid::GridOp;
use view_core::model::Model;
use view_core::msg::Msg;
use view_core::update::update;

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

criterion_group!(benches, bench_render_frame);
criterion_main!(benches);
