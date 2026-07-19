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
//! a breach is a measured value above `baseline * GATE_HEADROOM`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The one schema this loader understands.
pub const SUPPORTED_SCHEMA: u32 = 1;

/// Multiplier applied to every recorded baseline value before comparison.
/// The gated statistics are already medians over repeated interleaved
/// trials, but even the median retains run-to-run spread on a shared dev
/// host; the headroom absorbs that spread while still failing on a real
/// regression well below the doctored-baseline detection bar.
pub const GATE_HEADROOM: f64 = 1.15;

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
    pub bar: f64,
}

impl std::fmt::Display for Breach {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "GATE BREACH [{}.{}] {}: measured {:.4} > bar {:.4} (recorded {:.4} x headroom {GATE_HEADROOM})",
            self.scenario, self.fixture, self.metric, self.measured, self.bar, self.recorded
        )
    }
}

/// Compares one cell's measured metrics against the recorded baseline.
/// Only metrics present in the baseline gate (a newly added metric cannot
/// retroactively fail old baselines); every gated metric is
/// lower-is-better.
#[must_use]
pub fn gate_cell(
    scenario: &str,
    fixture: &str,
    measured: &CellMetrics,
    recorded: &CellMetrics,
) -> Vec<Breach> {
    let mut breaches = Vec::new();
    for (metric, recorded_value) in recorded {
        let Some(&measured_value) = measured.get(metric) else {
            continue;
        };
        let bar = recorded_value * GATE_HEADROOM;
        if measured_value > bar {
            breaches.push(Breach {
                scenario: scenario.to_string(),
                fixture: fixture.to_string(),
                metric: metric.clone(),
                measured: measured_value,
                recorded: *recorded_value,
                bar,
            });
        }
    }
    breaches
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

    #[test]
    fn gate_passes_within_headroom_and_breaches_above_it() {
        let recorded = metrics(&[("ratio_p99", 1.0)]);
        let ok = gate_cell(
            "echo",
            "minimal",
            &metrics(&[("ratio_p99", 1.10)]),
            &recorded,
        );
        assert!(ok.is_empty(), "1.10 sits inside 1.0 x {GATE_HEADROOM}");
        let bad = gate_cell(
            "echo",
            "minimal",
            &metrics(&[("ratio_p99", 1.20)]),
            &recorded,
        );
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].metric, "ratio_p99");
        let rendered = bad[0].to_string();
        assert!(
            rendered.contains("[echo.minimal]"),
            "breach must name the cell; actual: {rendered}"
        );
    }

    #[test]
    fn gate_ignores_metrics_absent_from_the_baseline() {
        let recorded = metrics(&[("ratio_p99", 1.0)]);
        let measured = metrics(&[("ratio_p99", 0.9), ("new_metric", 99.0)]);
        assert!(gate_cell("echo", "minimal", &measured, &recorded).is_empty());
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
