//! What the login-shaped `user` fixture is for: a bar that moves when a
//! real config's attach window gets worse.
//!
//! Over the public gate surface with synthetic numbers, not a measurement.
//! The stall the fixture can plant is proven to load by
//! `tests/user_fixture.rs`; what is proven here is the other half, that a
//! run carrying it comes back as a breach of exactly one metric on exactly
//! the classes that gate cold starts.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use view_harness::baselines::{
    gate_cell, CellId, CellMetrics, Headroom, HeadroomTable, MeasuredCell, ABSOLUTE_HEADROOM,
};

/// The row's own recorded shape for this fixture, in the order the report
/// prints it. Illustrative rather than harvested: no baseline holds a
/// `first_paint.user` cell until a record run writes one, and what this
/// pins is the policy over such a cell, not its values.
fn recorded() -> CellMetrics {
    [
        ("shell_visible_cold_ms", 3.9),
        ("marker_cold_ms", 96.0),
        ("marker_ratio_p50", 0.42),
        ("marker_ratio_p99", 0.55),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

fn measured(metrics: CellMetrics) -> MeasuredCell {
    MeasuredCell {
        id: CellId::new("first_paint", "user"),
        metrics,
    }
}

/// A stall inside the login lengthens the window between the startup shell
/// and the file's own content: `marker_cold_ms` grows, while
/// `shell_visible_cold_ms` (painted before the engine child is even
/// spawned) does not, and the paired ratios move with both arms rather
/// than with view alone.
///
/// The stall has to clear the absolute headroom to be the thing that
/// fires. 50 ms over a recorded 96 ms does, against a 144 ms bar; a login
/// that records much slower than that needs a longer one, which is a
/// property of the recorded value rather than of this policy.
#[test]
fn a_stalled_login_breaches_the_cold_marker_on_a_controlled_class_alone() {
    let recorded = recorded();
    let mut stalled = recorded.clone();
    *stalled.get_mut("marker_cold_ms").unwrap() += 50.0;
    let table = HeadroomTable::new();

    let breaches = gate_cell(
        &measured(stalled.clone()),
        &recorded,
        "controlled-linux",
        &table,
    );
    assert_eq!(
        breaches
            .iter()
            .map(|b| b.metric.as_str())
            .collect::<Vec<_>>(),
        vec!["marker_cold_ms"],
        "the stall must fire the cold marker and nothing else; got {breaches:?}"
    );
    assert_eq!(breaches[0].fixture, "user");
    assert_eq!(
        breaches[0].headroom,
        Headroom::Proportional(ABSOLUTE_HEADROOM)
    );

    assert!(
        gate_cell(&measured(stalled), &recorded, "dev-linux", &table).is_empty(),
        "a cold absolute is recorded on a shared class and gated on a controlled one"
    );
    assert!(
        gate_cell(
            &measured(recorded.clone()),
            &recorded,
            "controlled-linux",
            &table
        )
        .is_empty(),
        "the login as it ships must sit inside its own recorded bars"
    );
}
