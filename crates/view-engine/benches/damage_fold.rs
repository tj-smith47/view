//! Hot-path micro-bench: the damage compactor's fold+take pair
//! (`PumpShared::fold_redraw` / `DamagePump::take_damage`'s underlying
//! logic) against the same deterministic seeded "redraw storm" fixture
//! `damage.rs`'s own property tests generate against
//! (`compaction_preserves_final_grid_and_non_grid_subsequence`). Requires
//! the `bench-support` feature: the fold hot path and its generator are
//! crate-private otherwise (see `view_engine::damage::storm`'s module doc
//! comment for why). Recorded, not gated: the gate lives with the phase
//! that consumes this number as a regression budget.

use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;
use std::time::{Duration, Instant};
use view_engine::damage::storm::{fold_and_take, gen_sequence};

/// Idle between folds in the cold variant. `.claude/measurements/
/// 2026-07-27-the-paint-path-is-cold-cache-not-instructions.md` found the
/// paint path's per-frame cost tracks cache residency rather than
/// instruction count; the `echo` bench row paces keystrokes at this
/// interval and a human types slower still, so this is the gap the editor
/// actually presents to a hot loop rather than a pathological one.
const KEYSTROKE_GAP: Duration = Duration::from_millis(10);

fn bench_damage_fold(c: &mut Criterion) {
    // seed 0, a fixed 500-event storm: large enough to amortize fixed
    // per-call overhead, small enough to keep one bench iteration well
    // under a frame budget
    let storm = gen_sequence(0, 500);

    c.bench_function("damage_fold_storm_500_events", |b| {
        b.iter(|| {
            let compacted = fold_and_take(black_box(storm.clone()));
            black_box(compacted);
        });
    });
}

/// The cold counterpart: the identical fold, but with a keystroke interval
/// of idle before each one and only the fold itself timed. Follows
/// `paint_frame.rs`'s cold pattern (an `iter_custom` loop bounded by
/// sample count rather than throughput).
fn bench_damage_fold_cold(c: &mut Criterion) {
    let storm = gen_sequence(0, 500);

    let mut group = c.benchmark_group("damage_fold_cold");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(12));
    group.bench_function("storm_500_events_cold", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                std::thread::sleep(KEYSTROKE_GAP);
                let batch = storm.clone();
                let started = Instant::now();
                let compacted = fold_and_take(black_box(batch));
                black_box(compacted);
                elapsed += started.elapsed();
            }
            elapsed
        });
    });
    group.finish();
}

criterion_group!(benches, bench_damage_fold, bench_damage_fold_cold);
criterion_main!(benches);
