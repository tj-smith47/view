//! The paired campaign that keeps the read-side heartbeat off the latency
//! rows: the same two rows measured twice, once against the build the bench
//! matrix measures that row with and once against the same build with no
//! prober in its engine at all (`bench-no-heartbeat`), with the difference
//! held to what this machine class has published about the statistic's own
//! run-to-run spread.
//!
//! Why a test rather than a budget row. A budget bounds a number; the claim
//! here is about a *difference* between two binaries measured back to back,
//! which no single recorded value can express -- a regression that added
//! half a millisecond to every keystroke would still record inside its own
//! bar the day after it landed, because the ratchet compares this run to
//! the last recording rather than to the counterfactual.
//!
//! Why a compiled-out prober rather than a paused one. The costs at issue
//! are a probe on the outbox, a reply through the reader thread and one
//! more arm on the runtime loop's dispatch, but also the thread itself
//! waking on its interval. Pausing removes the first three and keeps the
//! last, so it cannot answer the question the row exists to answer.
//!
//! Ignored by default and driven by `task heartbeat-ab`, which builds the
//! binaries it needs first: two arms times each build a row measures --
//! tap-instrumented for the internal-boundary row, prediction-free for the
//! echo row -- each in its own target directory so no build can overwrite
//! another's.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::Command;

use view_harness::baselines::{declared_headroom, load_headroom, Headroom};
use view_harness::builds::{view_bin_flag, NOSPEC_VIEW_BIN, TAPS_VIEW_BIN};
use view_harness::fixture::workspace_root;

/// The rows this campaign measures, in run order.
///
/// A function rather than a list built inside the measuring test, so the
/// wiring pins below assert against the same rows the campaign drives
/// instead of a second copy that could drift from them.
fn campaign_rows() -> Vec<Row> {
    let mut rows = vec![Row {
        scenario: "echo",
        fixture: "minimal",
        metric: "ratio_p50",
        build: Build::NoSpeculate,
    }];
    // the tap channel is a unix mechanism, so the input-path row and the
    // instrumented build it reads exist only there
    if cfg!(unix) {
        rows.push(Row {
            scenario: "input_path",
            fixture: "minimal",
            metric: "key_to_rpc_p99_us",
            build: Build::Taps,
        });
    }
    rows
}

/// Sampling for one arm of one row.
///
/// Under the protocol minimum a recorded number is held to, deliberately:
/// nothing here is recorded or gated, both arms are measured under the
/// identical protocol minutes apart, and the quantity being read is a
/// difference between them rather than either one's absolute value. The
/// full 1000-sample protocol on four arms would price this campaign in
/// half-hours and buy resolution the tolerance below does not use.
const SAMPLES: usize = 400;
const WARMUP: usize = 40;
const TRIALS: usize = 3;

/// One row of the campaign: which cell to measure, which statistic to read
/// out of it, and which build of view the row measures.
struct Row {
    scenario: &'static str,
    fixture: &'static str,
    metric: &'static str,
    build: Build,
}

/// The build each row measures, in both of this campaign's arms.
///
/// Neither row measures the plain binary. The bench matrix gives a row the
/// build its own boundary needs, and a campaign that measured some other
/// one would be reading a number the matrix never reports.
#[derive(Clone, Copy)]
enum Build {
    /// The tap-instrumented build, which the internal-boundary rows
    /// measure: their intervals are read off the tap channel.
    Taps,
    /// The arm that predicts nothing, which the echo rows measure. Their
    /// boundary is the typed glyph reaching the screen, and a build that
    /// predicts puts it there before the engine answers -- so on the plain
    /// binary both of this campaign's arms would time the same painted
    /// prediction and the comparison would report the prober costing
    /// nothing from an apparatus that never measured the round trip.
    NoSpeculate,
}

impl Build {
    /// The target directories `task heartbeat-ab` fills for this build:
    /// prober armed, prober compiled out.
    fn dirs(self) -> (&'static str, &'static str) {
        match self {
            Build::Taps => ("target/taps", "target/taps-no-heartbeat"),
            Build::NoSpeculate => ("target/nospec", "target/nospec-no-heartbeat"),
        }
    }

    /// The bench flag that names this build's binary. Each row family takes
    /// the flag for the build it measures, and the bench binary refuses a
    /// run whose named binary a selected row would not read.
    fn flag(self) -> &'static str {
        match self {
            Build::Taps => TAPS_VIEW_BIN,
            Build::NoSpeculate => NOSPEC_VIEW_BIN,
        }
    }
}

/// The build directories `task heartbeat-ab` fills, by arm.
struct Arms {
    /// The prober armed.
    armed: PathBuf,
    /// The same code with the prober compiled out.
    bare: PathBuf,
}

fn arms(build: Build) -> Arms {
    let root = workspace_root();
    let (armed, bare) = build.dirs();
    Arms {
        armed: root.join(armed).join("release").join(view_bin_name()),
        bare: root.join(bare).join("release").join(view_bin_name()),
    }
}

fn view_bin_name() -> &'static str {
    if cfg!(windows) {
        "view.exe"
    } else {
        "view"
    }
}

fn bench_bin() -> PathBuf {
    workspace_root()
        .join("target")
        .join("release")
        .join(if cfg!(windows) { "bench.exe" } else { "bench" })
}

/// Whether this run is on a GitHub runner, by the same variable the bench
/// binary reads to decide whether a skip needs a checks-page annotation.
fn under_gha() -> bool {
    std::env::var("GITHUB_ACTIONS").is_ok_and(|value| value == "true")
}

/// This host's own class name. The bench binary refuses a class naming
/// another platform, and nothing here records or gates, so the campaign
/// names the host it is on rather than asking an operator to.
///
/// Platform is all it knows, so on a shared runner this would answer
/// `dev-linux` for a machine that is nothing of the sort -- which is why
/// the campaign refuses to run there at all rather than borrowing a class
/// name (see [`under_gha`] at its entry).
fn class() -> String {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    };
    format!("dev-{os}")
}

/// Runs one cell against one binary and returns the gated statistic the
/// aggregation trailer names.
fn measure(row: &Row, view_bin: &Path) -> f64 {
    let bench = bench_bin();
    assert!(
        bench.exists(),
        "no bench binary at {}; run this campaign through `task heartbeat-ab`, which builds \
         every binary it measures",
        bench.display()
    );
    assert!(
        view_bin.exists(),
        "no view binary at {}; run this campaign through `task heartbeat-ab`, which builds \
         both arms into their own target directories",
        view_bin.display()
    );
    let mut cmd = Command::new(&bench);
    cmd.current_dir(workspace_root())
        .arg("--scenario")
        .arg(row.scenario)
        .arg("--fixture")
        .arg(row.fixture)
        .arg("--class")
        .arg(class())
        .arg("--samples")
        .arg(SAMPLES.to_string())
        .arg("--warmup")
        .arg(WARMUP.to_string())
        .arg("--trials")
        .arg(TRIALS.to_string());
    cmd.arg(row.build.flag()).arg(view_bin);
    let out = cmd.output().expect("running the bench binary");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{}/{} against {} exited {:?}\n{stdout}\n{}",
        row.scenario,
        row.fixture,
        view_bin.display(),
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    gated_value(&stdout, row.metric).unwrap_or_else(|| {
        panic!(
            "{}/{} against {} printed no gated {} line:\n{stdout}",
            row.scenario,
            row.fixture,
            view_bin.display(),
            row.metric
        )
    })
}

/// The value on the aggregation trailer for `metric`, the line
/// `view_bench::report::aggregate_line` writes.
fn gated_value(stdout: &str, metric: &str) -> Option<f64> {
    let needle = format!("gated {metric} ");
    stdout.lines().rev().find_map(|line| {
        let tail = line.split(&needle).nth(1)?;
        tail.split_whitespace().next()?.parse().ok()
    })
}

/// The spread this class has published for `metric`, as a multiplier.
///
/// The tolerance is a measured property of the host rather than a number
/// chosen here: a difference smaller than what one unchanged binary pair
/// already moves between runs is not a difference this campaign can see. A
/// class that has published nothing for the statistic fails the campaign
/// instead of falling back to a default, because a fallback would report
/// "no measurable delta" from an apparatus nobody has shown can measure
/// one.
fn tolerance(scenario: &str, metric: &str) -> f64 {
    let path = workspace_root()
        .join("crates")
        .join("view-bench")
        .join("baselines")
        .join(format!("{}.headroom.toml", class()));
    let table = load_headroom(&path, &class()).expect("loading the class headroom sidecar");
    match declared_headroom(&table, scenario, metric) {
        Some(Headroom::Proportional(factor)) => factor,
        other => panic!(
            "class {} publishes no proportional spread for {scenario}.{metric} ({other:?}); \
             characterize the statistic on this class before reading a paired difference \
             against it",
            class()
        ),
    }
}

/// Both arms of a build have to be two different binaries, or the campaign
/// measures one binary twice and reports the prober costing nothing from an
/// apparatus that never compared anything. Cheap enough to run in the
/// per-commit gate, where the measuring test below never runs.
#[test]
fn each_build_measures_two_distinct_binaries() {
    for build in [Build::Taps, Build::NoSpeculate] {
        let arms = arms(build);
        assert_ne!(
            arms.armed,
            arms.bare,
            "both arms of {:?} resolve to the same binary",
            build.dirs()
        );
        let (armed_dir, bare_dir) = build.dirs();
        assert_ne!(armed_dir, bare_dir);
    }
}

/// Each row has to be driven with the flag the bench binary reads that
/// row's build from. A row driven with any other flag hands the bench
/// binary a path no selected row reads: since the flag check was extended
/// to every `--*-view-bin`, that run is refused rather than silently
/// measuring the default build under both arms -- but the refusal arrives
/// half an hour into a campaign, and this arrives in the gate.
#[test]
fn every_row_is_driven_with_the_flag_its_scenario_reads() {
    let rows = campaign_rows();
    assert!(!rows.is_empty(), "the campaign drives no rows at all");
    for row in &rows {
        assert_eq!(
            view_bin_flag(row.scenario),
            Some(row.build.flag()),
            "{} is driven with {}, which is not the flag its build is named by",
            row.scenario,
            row.build.flag()
        );
    }
}

#[test]
#[ignore = "drives four release binaries through two full bench rows; run via task heartbeat-ab"]
fn the_heartbeat_prober_costs_nothing_this_class_can_measure() {
    // a runner is not the dev machine whose name `class()` would answer for
    // it: the tolerance below and the baselines the bench arms compare
    // against are both properties of the host that published them, and no
    // gh class has published a spread for these two statistics at all. So
    // the campaign says what it did not measure instead of consuming
    // dev-linux's headroom on a machine that never earned it -- announced
    // on the checks page, because a silent skip reads as a verified claim.
    if under_gha() {
        let reason = "no gh class publishes a resolvable spread for these rows, and the dev \
                      class's spread describes a different machine";
        println!("skipping the_heartbeat_prober_costs_nothing_this_class_can_measure: {reason}");
        println!("::warning::heartbeat armed-vs-absent campaign skipped: {reason}");
        return;
    }
    let rows = campaign_rows();

    let mut report = Vec::new();
    let mut breaches = Vec::new();
    for row in &rows {
        let arms = arms(row.build);
        // armed first, bare second, back to back on one host state: a
        // campaign that measured every armed arm and then every bare one
        // would put the whole cell loop between the two halves of each
        // comparison
        let armed = measure(row, &arms.armed);
        let bare = measure(row, &arms.bare);
        let tolerance = tolerance(row.scenario, row.metric);
        let ratio = armed / bare;
        let line = format!(
            "{}.{} {}: armed {armed:.4}, prober absent {bare:.4}, armed/absent {ratio:.4} \
             (this class resolves {tolerance:.4})",
            row.scenario, row.fixture, row.metric
        );
        println!("{line}");
        report.push(line.clone());
        if ratio > tolerance {
            breaches.push(line);
        }
    }
    assert!(
        breaches.is_empty(),
        "the heartbeat prober moved a latency row past this class's own spread:\n{}\nfull \
         campaign:\n{}",
        breaches.join("\n"),
        report.join("\n")
    );
}
