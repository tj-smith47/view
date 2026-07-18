//! Hot-path micro-bench: `update()` dispatching a single key through
//! `Msg::Key`, the per-keystroke cost the paint loop pays on every input
//! event. `<C-x>` (already-encoded `nvim_input` notation, matching what
//! `view_tui::keys::encode_key` produces) exercises the ordinary
//! engine-focused dispatch path. Recorded, not gated: the gate lives with
//! the phase that consumes this number as a regression budget.

use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;
use view_core::model::Model;
use view_core::msg::{Key, Msg};
use view_core::update::update;

fn bench_update_key(c: &mut Criterion) {
    let mut model = Model::new();

    c.bench_function("update_key_dispatch_ctrl_x", |b| {
        b.iter(|| {
            let effects = update(
                &mut model,
                Msg::Key(Key {
                    notation: black_box("<C-x>".to_string()),
                }),
            );
            black_box(effects);
        });
    });
}

criterion_group!(benches, bench_update_key);
criterion_main!(benches);
