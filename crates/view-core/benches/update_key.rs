//! Hot-path micro-bench: `update()` dispatching a single key through
//! `Msg::Key`, the per-keystroke cost the paint loop pays on every input
//! event. `<C-x>` (already-encoded `nvim_input` notation, matching what
//! `view_tui::keys::encode_key` produces) exercises the ordinary
//! engine-focused dispatch path. Recorded, not gated: the gate lives with
//! the phase that consumes this number as a regression budget.

use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;
use std::time::{Duration, Instant};
use view_core::model::Model;
use view_core::msg::{Key, Msg};
use view_core::update::update;

/// Idle between dispatches in the cold variant. `.claude/measurements/
/// 2026-07-27-the-paint-path-is-cold-cache-not-instructions.md` found the
/// paint path's per-frame cost tracks cache residency rather than
/// instruction count; the `echo` bench row paces keystrokes at this
/// interval and a human types slower still, so this is the gap the editor
/// actually presents to a hot loop rather than a pathological one.
const KEYSTROKE_GAP: Duration = Duration::from_millis(10);

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

/// The cold counterpart: the identical dispatch, but with a keystroke
/// interval of idle before each one and only the dispatch itself timed.
/// Follows `paint_frame.rs`'s cold pattern (an `iter_custom` loop bounded
/// by sample count rather than throughput).
fn bench_update_key_cold(c: &mut Criterion) {
    let mut model = Model::new();

    let mut group = c.benchmark_group("update_key_cold");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(12));
    group.bench_function("dispatch_ctrl_x_cold", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                std::thread::sleep(KEYSTROKE_GAP);
                let started = Instant::now();
                let effects = update(
                    &mut model,
                    Msg::Key(Key {
                        notation: black_box("<C-x>".to_string()),
                    }),
                );
                black_box(effects);
                elapsed += started.elapsed();
            }
            elapsed
        });
    });
    group.finish();
}

criterion_group!(benches, bench_update_key, bench_update_key_cold);
criterion_main!(benches);
