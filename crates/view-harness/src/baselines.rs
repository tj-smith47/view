//! The per-machine-class baseline file: recorded measurements the bench
//! gate compares against. TOML lives here (view-harness is the one
//! package sanctioned to parse it); `view-bench` itself stays serde-free
//! and only produces numbers.
//!
//! File shape (`crates/view-bench/baselines/<class>.toml`):
//!
//! ```toml
//! schema = 1
//! engine_pin = "v0.12.4"
//! machine_class = "dev-linux"
//!
//! [echo.minimal]
//! ratio_p99 = 1.21
//! paired_delta_p99_ms = 0.29
//! ```
//!
//! Every metric is lower-is-better, so one gate rule covers all cells:
//! a breach is a measured value above `baseline * headroom`, with the
//! headroom chosen per metric kind and machine class by [`gate_headroom`].
//!
//! Machine classes split into two gate policies, encoded in the class
//! name itself (see [`is_controlled_class`]): a `controlled-` prefixed
//! class runs on a dedicated, load-controlled box and additionally gates
//! the paired tail statistics; every other class records them ungated.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The one schema this loader understands.
pub const SUPPORTED_SCHEMA: u32 = 1;

/// Headroom for gated ratio metrics (view vs nvim from one interleaved
/// run). Medians are robust to ambient tail noise, and per-sample
/// alternation keeps both sides under the same ambient median shift:
/// measured echo ratio_p50 stayed inside a x1.07 band (1.51..1.62)
/// across host-load regimes whose absolute tails swung x300. 1.25
/// covers that band with margin while a real regression still breaches.
pub const RATIO_HEADROOM: f64 = 1.25;

/// Headroom for absolute metrics (ms/us/MB budgets). Absolute tails on a
/// shared dev host move with ambient load (taps p99 varied x1.5 between
/// quiet invocations), so the band is wider than for ratios; a breach
/// under heavy host load is loud and explainable, while a gross
/// regression (2x) still cannot hide inside it.
pub const ABSOLUTE_HEADROOM: f64 = 1.5;

/// Whether `class` names a dedicated, load-controlled bench runner. The
/// policy lives in the class name itself (a `controlled-` prefix) rather
/// than a separate metadata field so a baseline file can never disagree
/// with its own class about which gate policy applies: there is no
/// second value to drift, hand-edit, or forget when recording a fresh
/// class.
#[must_use]
pub fn is_controlled_class(class: &str) -> bool {
    class.starts_with("controlled-")
}

/// The gate policy for one metric on one machine class: `None` means
/// recorded for reference but never gated on this mechanism.
///
/// The paired tail statistics gate only on controlled classes. Both
/// earned the shared-class exemption by measurement of one unchanged
/// binary pair across host-load regimes:
/// - `paired_delta_p99_ms` tracked ambient load x149 (0.62ms..92.5ms);
///   its regression protection is duplicated by the ratio from the same
///   paired run.
/// - `ratio_p99` has a +/-50% ambient noise floor (invocation medians
///   1.05..1.95): shared tails are scheduler-dominated, and load
///   compresses the ratio toward 1.
///
/// On a dedicated runner those load regimes cannot occur, so the same
/// statistics gate there under the standard ratio headroom, and the spec
/// p99 budget gets a real bar instead of a reference number.
#[must_use]
pub fn gate_headroom(metric: &str, controlled: bool) -> Option<f64> {
    if metric == "paired_delta_p99_ms" || metric == "ratio_p99" {
        return controlled.then_some(RATIO_HEADROOM);
    }
    if metric.contains("ratio") {
        Some(RATIO_HEADROOM)
    } else {
        Some(ABSOLUTE_HEADROOM)
    }
}

/// Metric values for one `[scenario.fixture]` cell.
pub type CellMetrics = BTreeMap<String, f64>;

/// Errors loading, saving, or gating against a baseline file.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BaselineError {
    #[error("reading {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("writing {path}: {source}")]
    Write {
        path: String,
        source: std::io::Error,
    },
    #[error("parsing {path}: {source}")]
    Parse {
        path: String,
        source: Box<toml::de::Error>,
    },
    #[error("serializing baseline: {source}")]
    Serialize { source: toml::ser::Error },
    #[error("{path} declares schema {found}; this harness supports schema {SUPPORTED_SCHEMA}")]
    UnsupportedSchema { path: String, found: u32 },
    #[error(
        "baseline {path} was recorded against engine pin {recorded:?} but the current pin is \
         {current:?}; re-record the baseline before gating or recording single cells"
    )]
    PinDrift {
        path: String,
        recorded: String,
        current: String,
    },
    #[error(
        "baseline {path} declares machine_class {recorded:?} but this run named class \
         {current:?}; the gate policy is derived from the class, so the two must agree"
    )]
    ClassMismatch {
        path: String,
        recorded: String,
        current: String,
    },
    #[error("baseline {path} has no [{scenario}.{fixture}] cell to gate against")]
    MissingCell {
        path: String,
        scenario: String,
        fixture: String,
    },
}

/// One recorded machine class's baselines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselineFile {
    pub schema: u32,
    pub engine_pin: String,
    pub machine_class: String,
    /// scenario -> fixture -> metric -> recorded value.
    #[serde(flatten)]
    pub cells: BTreeMap<String, BTreeMap<String, CellMetrics>>,
}

impl BaselineFile {
    /// A fresh, empty baseline for `class` under `pin`.
    #[must_use]
    pub fn new(class: &str, pin: &str) -> Self {
        Self {
            schema: SUPPORTED_SCHEMA,
            engine_pin: pin.to_string(),
            machine_class: class.to_string(),
            cells: BTreeMap::new(),
        }
    }

    /// Inserts or replaces one cell's metrics.
    pub fn upsert_cell(&mut self, scenario: &str, fixture: &str, metrics: CellMetrics) {
        self.cells
            .entry(scenario.to_string())
            .or_default()
            .insert(fixture.to_string(), metrics);
    }

    /// The recorded metrics for one cell, if present.
    #[must_use]
    pub fn cell(&self, scenario: &str, fixture: &str) -> Option<&CellMetrics> {
        self.cells.get(scenario)?.get(fixture)
    }
}

/// One gate breach: a measured metric above its recorded bar.
#[derive(Debug, Clone, PartialEq)]
pub struct Breach {
    pub scenario: String,
    pub fixture: String,
    pub metric: String,
    pub measured: f64,
    pub recorded: f64,
    pub headroom: f64,
    pub bar: f64,
}

impl std::fmt::Display for Breach {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "GATE BREACH [{}.{}] {}: measured {:.4} > bar {:.4} (recorded {:.4} x headroom {})",
            self.scenario,
            self.fixture,
            self.metric,
            self.measured,
            self.bar,
            self.recorded,
            self.headroom
        )
    }
}

/// Compares one cell's measured metrics against the recorded baseline.
/// Only metrics present in the baseline gate (a newly added metric cannot
/// retroactively fail old baselines); every gated metric is
/// lower-is-better, with per-metric headroom from [`gate_headroom`].
/// Takes the machine class name, not a pre-derived flag, so no caller can
/// pair one class's cells with another class's gate policy.
#[must_use]
pub fn gate_cell(
    scenario: &str,
    fixture: &str,
    measured: &CellMetrics,
    recorded: &CellMetrics,
    class: &str,
) -> Vec<Breach> {
    let controlled = is_controlled_class(class);
    let mut breaches = Vec::new();
    for (metric, recorded_value) in recorded {
        let Some(&measured_value) = measured.get(metric) else {
            continue;
        };
        let Some(headroom) = gate_headroom(metric, controlled) else {
            continue;
        };
        let bar = recorded_value * headroom;
        if measured_value > bar {
            breaches.push(Breach {
                scenario: scenario.to_string(),
                fixture: fixture.to_string(),
                metric: metric.clone(),
                measured: measured_value,
                recorded: *recorded_value,
                headroom,
                bar,
            });
        }
    }
    breaches
}

/// Baseline cells that `measured` and `skipped` together leave
/// unchecked, in deterministic (sorted) order. The forward gate walks
/// measured cells only, so a cell that silently fell out of a
/// full-coverage run (dropped from the matrix, lost to a selection bug)
/// would otherwise stay green forever with its recorded bars never
/// re-tested; `skipped` carries the cells the run legitimately excluded
/// for platform reasons so they do not count as coverage gaps.
#[must_use]
pub fn uncovered_cells(
    baseline: &BaselineFile,
    measured: &[(String, String)],
    skipped: &[(String, String)],
) -> Vec<(String, String)> {
    let covered = |scenario: &str, fixture: &str| {
        measured
            .iter()
            .chain(skipped.iter())
            .any(|(s, f)| s == scenario && f == fixture)
    };
    let mut uncovered = Vec::new();
    for (scenario, fixtures) in &baseline.cells {
        for fixture in fixtures.keys() {
            if !covered(scenario, fixture) {
                uncovered.push((scenario.clone(), fixture.clone()));
            }
        }
    }
    uncovered
}

/// Loads and validates `path`.
///
/// # Errors
///
/// Returns [`BaselineError::Read`]/[`BaselineError::Parse`] on I/O or
/// TOML failures, and [`BaselineError::UnsupportedSchema`] on a schema
/// this harness does not understand.
pub fn load(path: &Path) -> Result<BaselineFile, BaselineError> {
    let display = path.display().to_string();
    let text = std::fs::read_to_string(path).map_err(|source| BaselineError::Read {
        path: display.clone(),
        source,
    })?;
    let file: BaselineFile = toml::from_str(&text).map_err(|source| BaselineError::Parse {
        path: display.clone(),
        source: Box::new(source),
    })?;
    if file.schema != SUPPORTED_SCHEMA {
        return Err(BaselineError::UnsupportedSchema {
            path: display,
            found: file.schema,
        });
    }
    Ok(file)
}

/// Serializes `file` to `path`, creating parent directories.
///
/// # Errors
///
/// Returns [`BaselineError::Serialize`]/[`BaselineError::Write`] on
/// serialization or I/O failures.
pub fn save(path: &Path, file: &BaselineFile) -> Result<(), BaselineError> {
    let display = path.display().to_string();
    let text =
        toml::to_string_pretty(file).map_err(|source| BaselineError::Serialize { source })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| BaselineError::Write {
            path: display.clone(),
            source,
        })?;
    }
    std::fs::write(path, text).map_err(|source| BaselineError::Write {
        path: display,
        source,
    })
}

/// Rejects gating or single-cell recording against a baseline recorded
/// under a different engine pin: numbers measured against one engine
/// version are not a regression bar for another.
///
/// # Errors
///
/// Returns [`BaselineError::PinDrift`] when the pins differ.
pub fn require_pin_match(
    file: &BaselineFile,
    current_pin: &str,
    path: &Path,
) -> Result<(), BaselineError> {
    if file.engine_pin != current_pin {
        return Err(BaselineError::PinDrift {
            path: path.display().to_string(),
            recorded: file.engine_pin.clone(),
            current: current_pin.to_string(),
        });
    }
    Ok(())
}

/// Rejects using a baseline file whose recorded `machine_class` differs
/// from the class this invocation named: the gate policy (controlled vs
/// shared) is derived from the class, so a mismatch would silently apply
/// one class's policy to another class's numbers.
///
/// # Errors
///
/// Returns [`BaselineError::ClassMismatch`] when the classes differ.
pub fn require_class_match(
    file: &BaselineFile,
    current_class: &str,
    path: &Path,
) -> Result<(), BaselineError> {
    if file.machine_class != current_class {
        return Err(BaselineError::ClassMismatch {
            path: path.display().to_string(),
            recorded: file.machine_class.clone(),
            current: current_class.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]
    use super::*;

    fn metrics(pairs: &[(&str, f64)]) -> CellMetrics {
        pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }

    #[test]
    fn round_trips_through_toml_with_scenario_fixture_tables() {
        let mut file = BaselineFile::new("dev-linux", "v0.12.4");
        file.upsert_cell(
            "echo",
            "minimal",
            metrics(&[("ratio_p99", 1.21), ("paired_delta_p99_ms", 0.29)]),
        );
        let text = toml::to_string_pretty(&file).unwrap();
        assert!(text.contains("[echo.minimal]"), "actual TOML:\n{text}");
        let parsed: BaselineFile = toml::from_str(&text).unwrap();
        assert_eq!(parsed.cell("echo", "minimal").unwrap()["ratio_p99"], 1.21);
        assert_eq!(parsed.machine_class, "dev-linux");
    }

    #[test]
    fn upsert_replaces_only_the_named_cell() {
        let mut file = BaselineFile::new("dev-linux", "v0.12.4");
        file.upsert_cell("echo", "minimal", metrics(&[("ratio_p99", 1.2)]));
        file.upsert_cell("echo", "heavy", metrics(&[("ratio_p99", 1.4)]));
        file.upsert_cell("echo", "minimal", metrics(&[("ratio_p99", 1.1)]));
        assert_eq!(file.cell("echo", "minimal").unwrap()["ratio_p99"], 1.1);
        assert_eq!(file.cell("echo", "heavy").unwrap()["ratio_p99"], 1.4);
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(s, f)| ((*s).to_string(), (*f).to_string()))
            .collect()
    }

    #[test]
    fn uncovered_cells_names_a_baseline_cell_the_run_never_touched() {
        let mut file = BaselineFile::new("dev-linux", "v0.12.4");
        file.upsert_cell("echo", "minimal", metrics(&[("ratio_p50", 1.5)]));
        file.upsert_cell("memory", "minimal", metrics(&[("pss_mb", 3.4)]));
        let uncovered = uncovered_cells(&file, &pairs(&[("echo", "minimal")]), &[]);
        assert_eq!(uncovered, pairs(&[("memory", "minimal")]));
    }

    #[test]
    fn uncovered_cells_accepts_a_platform_skipped_cell() {
        let mut file = BaselineFile::new("gh-macos", "v0.12.4");
        file.upsert_cell("echo", "minimal", metrics(&[("ratio_p50", 1.5)]));
        file.upsert_cell("memory", "minimal", metrics(&[("pss_mb", 3.4)]));
        let uncovered = uncovered_cells(
            &file,
            &pairs(&[("echo", "minimal")]),
            &pairs(&[("memory", "minimal")]),
        );
        assert!(
            uncovered.is_empty(),
            "a platform-skipped cell must not read as a coverage gap: {uncovered:?}"
        );
    }

    #[test]
    fn gate_passes_within_headroom_and_breaches_above_it() {
        let recorded = metrics(&[("ratio_p50", 1.0)]);
        let ok = gate_cell(
            "echo",
            "minimal",
            &metrics(&[("ratio_p50", 1.20)]),
            &recorded,
            "dev-linux",
        );
        assert!(ok.is_empty(), "1.20 sits inside 1.0 x {RATIO_HEADROOM}");
        let bad = gate_cell(
            "echo",
            "minimal",
            &metrics(&[("ratio_p50", 1.30)]),
            &recorded,
            "dev-linux",
        );
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].metric, "ratio_p50");
        assert_eq!(bad[0].headroom, RATIO_HEADROOM);
        let rendered = bad[0].to_string();
        assert!(
            rendered.contains("[echo.minimal]"),
            "breach must name the cell; actual: {rendered}"
        );
    }

    #[test]
    fn controlled_class_is_a_naming_convention() {
        assert!(is_controlled_class("controlled-linux-x86"));
        assert!(!is_controlled_class("dev-linux"));
        assert!(!is_controlled_class("shared-ci"));
        assert!(!is_controlled_class("linux-controlled"));
    }

    #[test]
    fn headroom_policy_maps_metric_kinds() {
        for controlled in [false, true] {
            assert_eq!(gate_headroom("ratio_p50", controlled), Some(RATIO_HEADROOM));
            assert_eq!(
                gate_headroom("drain_ratio", controlled),
                Some(RATIO_HEADROOM)
            );
            assert_eq!(
                gate_headroom("ratio_vs_nvim", controlled),
                Some(RATIO_HEADROOM)
            );
            assert_eq!(
                gate_headroom("staleness_p99_ms", controlled),
                Some(ABSOLUTE_HEADROOM)
            );
            assert_eq!(
                gate_headroom("view_p99_ms", controlled),
                Some(ABSOLUTE_HEADROOM)
            );
            assert_eq!(gate_headroom("pss_mb", controlled), Some(ABSOLUTE_HEADROOM));
        }
        assert_eq!(gate_headroom("paired_delta_p99_ms", false), None);
        assert_eq!(gate_headroom("ratio_p99", false), None);
        assert_eq!(
            gate_headroom("paired_delta_p99_ms", true),
            Some(RATIO_HEADROOM)
        );
        assert_eq!(gate_headroom("ratio_p99", true), Some(RATIO_HEADROOM));
    }

    #[test]
    fn tail_metrics_never_breach_on_a_shared_class() {
        let recorded = metrics(&[("paired_delta_p99_ms", 0.6), ("ratio_p99", 1.05)]);
        let measured = metrics(&[("paired_delta_p99_ms", 92.5), ("ratio_p99", 1.95)]);
        assert!(
            gate_cell("echo", "minimal", &measured, &recorded, "dev-linux").is_empty(),
            "reference-only metrics must not gate even far above their recorded values"
        );
    }

    #[test]
    fn tail_metrics_gate_on_a_controlled_class() {
        let recorded = metrics(&[("paired_delta_p99_ms", 0.6), ("ratio_p99", 1.05)]);
        let measured = metrics(&[("paired_delta_p99_ms", 92.5), ("ratio_p99", 1.95)]);
        let breaches = gate_cell("echo", "minimal", &measured, &recorded, "controlled-linux");
        assert_eq!(
            breaches.len(),
            2,
            "both tail statistics must gate on a controlled class; got {breaches:?}"
        );
        for breach in &breaches {
            assert_eq!(breach.headroom, RATIO_HEADROOM);
        }
        let within = metrics(&[("paired_delta_p99_ms", 0.7), ("ratio_p99", 1.2)]);
        assert!(gate_cell("echo", "minimal", &within, &recorded, "controlled-linux").is_empty());
    }

    #[test]
    fn gate_ignores_metrics_absent_from_the_baseline() {
        let recorded = metrics(&[("ratio_p50", 1.0)]);
        let measured = metrics(&[("ratio_p50", 0.9), ("new_metric", 99.0)]);
        assert!(gate_cell("echo", "minimal", &measured, &recorded, "dev-linux").is_empty());
    }

    #[test]
    fn class_mismatch_is_rejected() {
        let file = BaselineFile::new("dev-linux", "v0.12.4");
        let err = require_class_match(&file, "controlled-linux", Path::new("x.toml")).unwrap_err();
        assert!(matches!(err, BaselineError::ClassMismatch { .. }));
        assert!(require_class_match(&file, "dev-linux", Path::new("x.toml")).is_ok());
    }

    #[test]
    fn unsupported_schema_is_rejected_on_load() {
        let dir = std::env::temp_dir().join(format!("view-baselines-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad-schema.toml");
        std::fs::write(
            &path,
            "schema = 2\nengine_pin = \"v0.12.4\"\nmachine_class = \"dev-linux\"\n",
        )
        .unwrap();
        assert!(matches!(
            load(&path).unwrap_err(),
            BaselineError::UnsupportedSchema { found: 2, .. }
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pin_drift_is_rejected() {
        let file = BaselineFile::new("dev-linux", "v0.12.3");
        let err = require_pin_match(&file, "v0.12.4", Path::new("x.toml")).unwrap_err();
        assert!(matches!(err, BaselineError::PinDrift { .. }));
        let file = BaselineFile::new("dev-linux", "v0.12.4");
        assert!(require_pin_match(&file, "v0.12.4", Path::new("x.toml")).is_ok());
    }

    #[test]
    fn save_and_load_round_trip_on_disk() {
        let dir = std::env::temp_dir().join(format!("view-baselines-rt-{}", std::process::id()));
        let path = dir.join("dev-linux.toml");
        let mut file = BaselineFile::new("dev-linux", "v0.12.4");
        file.upsert_cell("first_paint", "heavy", metrics(&[("cold_ms", 38.0)]));
        save(&path, &file).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(
            loaded.cell("first_paint", "heavy").unwrap()["cold_ms"],
            38.0
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
