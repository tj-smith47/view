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
use view_engine::damage::storm::{fold_and_take, gen_sequence};

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

criterion_group!(benches, bench_damage_fold);
criterion_main!(benches);
