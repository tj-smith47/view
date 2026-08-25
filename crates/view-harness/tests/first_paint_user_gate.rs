//! What the login-shaped `user` fixture is for: a bar that moves when a
//! real config's attach window gets worse.
//!
//! Two halves. One is policy -- which metric a stalled login breaches and
//! on which classes -- and it is asserted over the public gate surface on
//! numbers written here, because policy is the same whatever the row
//! records. The other is the binding between the stall the fixture plants
//! and the bar it has to clear, which is a property of the recorded value
//! and is read from the baselines themselves.
//!
//! The stall is proven to load by `tests/user_fixture.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use view_harness::baselines::{
    self, gate_cell, CellId, CellMetrics, Headroom, HeadroomTable, MeasuredCell, ABSOLUTE_HEADROOM,
};
use view_harness::fixture::{workspace_root, USER_FIXTURE, USER_FIXTURE_STALL_MS};

/// The metric a login's own slowness lands in.
const MARKER: &str = "marker_cold_ms";

/// A recorded cold marker this stall still fires against, used by the
/// policy half. Not a stand-in for the real value: the half that cares
/// what the real value is reads it, below.
const SYNTHETIC_MARKER_MS: f64 = 96.0;

/// The largest recorded cold marker [`USER_FIXTURE_STALL_MS`] can still
/// breach: the absolute headroom lets a cell grow by
/// `ABSOLUTE_HEADROOM - 1` of itself before it is a breach, so a stall
/// smaller than that share of the recorded value fires nothing at all.
fn largest_marker_the_stall_can_breach() -> f64 {
    USER_FIXTURE_STALL_MS as f64 / (ABSOLUTE_HEADROOM - 1.0)
}

fn cell() -> CellId {
    CellId::new("first_paint", USER_FIXTURE)
}

/// The row's own recorded shape for this fixture, in the order the report
/// prints it.
fn recorded(marker_ms: f64) -> CellMetrics {
    [
        ("shell_visible_cold_ms", 3.9),
        (MARKER, marker_ms),
        ("marker_ratio_p50", 0.42),
        ("marker_ratio_p99", 0.55),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

fn measured(metrics: CellMetrics) -> MeasuredCell {
    MeasuredCell {
        id: cell(),
        metrics,
    }
}

/// Every class baseline in the repo, as `(class, file)`, skipping the
/// sidecars that are not baselines (`*.headroom.toml`) and the partial
/// fixture a recorder test writes.
fn class_baselines() -> Vec<(String, PathBuf)> {
    let dir = workspace_root()
        .join("crates")
        .join("view-bench")
        .join("baselines");
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("the baselines directory must be readable") {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.ends_with(".toml") || name.contains(".headroom.") || name.contains(".partial.") {
            continue;
        }
        found.push((name.trim_end_matches(".toml").to_string(), path));
    }
    assert!(
        !found.is_empty(),
        "no class baseline was found to read, so this walk would pass without reading anything"
    );
    found
}

/// A stall inside the login lengthens the window between the startup shell
/// and the file's own content: `marker_cold_ms` grows, while
/// `shell_visible_cold_ms` (painted before the engine child is even
/// spawned) does not, and the paired ratios move with both arms rather
/// than with view alone.
#[test]
fn a_stalled_login_breaches_the_cold_marker_on_a_controlled_class_alone() {
    assert!(
        SYNTHETIC_MARKER_MS < largest_marker_the_stall_can_breach(),
        "the numbers this policy test runs on have to be a case the stall really fires on, \
         or it would be asserting the gate's silence rather than its verdict"
    );
    let recorded = recorded(SYNTHETIC_MARKER_MS);
    let mut stalled = recorded.clone();
    *stalled.get_mut(MARKER).unwrap() += USER_FIXTURE_STALL_MS as f64;
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
        vec![MARKER],
        "the stall must fire the cold marker and nothing else; got {breaches:?}"
    );
    assert_eq!(breaches[0].fixture, USER_FIXTURE);
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

/// The other half, and the one the policy test cannot make: that the stall
/// the fixture actually plants is still large enough to breach the bar the
/// recorded row sets.
///
/// Read from the baselines rather than written here, so the day a class
/// records a login slower than [`largest_marker_the_stall_can_breach`] is
/// the day this fails -- at that point the knob proves nothing, and a
/// green test saying otherwise is worse than no test.
///
/// Before any class records the cell there is no bar to bind to, and the
/// loud failure is the gate's own: a `--gate` run on a class whose
/// baseline lacks the cell reports it as uncovered and exits on it. That
/// mechanism is what this asserts in the meantime, so the pre-record state
/// is pinned to a real refusal rather than to this test's silence.
#[test]
fn the_planted_stall_clears_the_bar_of_every_class_that_records_the_login() {
    let ceiling = largest_marker_the_stall_can_breach();
    let mut recorded_anywhere = 0_usize;
    for (class, path) in class_baselines() {
        let baseline = baselines::load(&path).unwrap_or_else(|err| {
            panic!("class baseline {} must parse: {err}", path.display());
        });
        let Some(metrics) = baseline.cell(&cell()) else {
            assert!(
                !baselines::unrecorded_cells(&baseline, &[measured(recorded(1.0))]).is_empty(),
                "{class} records no first_paint.{USER_FIXTURE} cell, and a gate run against it \
                 no longer reports the cell as uncovered either -- nothing would fail loudly \
                 before the recording lands"
            );
            continue;
        };
        let marker = *metrics.get(MARKER).unwrap_or_else(|| {
            panic!("{class} records first_paint.{USER_FIXTURE} without {MARKER}")
        });
        recorded_anywhere += 1;
        assert!(
            marker < ceiling,
            "{class} records a cold marker of {marker:.1} ms for the login, and \
             {USER_FIXTURE_STALL_MS} ms of deliberate stall no longer reaches its \
             {:.1} ms bar ({ABSOLUTE_HEADROOM}x). The knob proves nothing at that size: raise \
             USER_FIXTURE_STALL_MS past {:.1} ms, or the fixture's own slowdown is decoration",
            marker * ABSOLUTE_HEADROOM,
            marker * (ABSOLUTE_HEADROOM - 1.0)
        );
    }
    println!("{recorded_anywhere} class baseline(s) record first_paint.{USER_FIXTURE}");
}
