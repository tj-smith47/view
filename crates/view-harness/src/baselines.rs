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

/// The platforms a machine-class name may claim, spelled as
/// [`std::env::consts::OS`] spells them so the two can be compared
/// directly.
const PLATFORM_TOKENS: [&str; 3] = ["linux", "macos", "windows"];

/// The platform `class` claims, or `None` when it claims none or claims
/// two. The token is matched as a hyphen-delimited segment (`dev-linux`,
/// `gh-macos`, `controlled-linux-x86`), never as a substring, so a
/// machine name that merely contains a platform word cannot pass for a
/// declaration.
#[must_use]
pub fn class_platform(class: &str) -> Option<&'static str> {
    let mut claimed: Option<&'static str> = None;
    for segment in class.split('-') {
        let Some(token) = PLATFORM_TOKENS.iter().find(|token| **token == segment) else {
            continue;
        };
        if claimed.is_some_and(|first| first != *token) {
            return None;
        }
        claimed = Some(token);
    }
    claimed
}

/// Rejects running under a class whose platform is not this binary's
/// host platform.
///
/// Rows are measured per-platform under per-platform metric names, and
/// the machine class is what selects the baseline file. Both sides of
/// [`require_class_match`] are hand-supplied (the CLI argument and the
/// file's own field), so they can agree with each other while both being
/// wrong about the host; the host platform is the one value in the check
/// that cannot be typed in. Refusing here is what stops one platform's
/// recorded numbers from becoming another platform's bars.
///
/// # Errors
///
/// Returns [`BaselineError::ClassPlatformUnnamed`] when the class names
/// no single platform, and [`BaselineError::HostPlatformMismatch`] when
/// it names one this binary is not running on.
pub fn require_host_platform(class: &str) -> Result<(), BaselineError> {
    let host = std::env::consts::OS;
    let Some(named) = class_platform(class) else {
        return Err(BaselineError::ClassPlatformUnnamed {
            class: class.to_string(),
            tokens: PLATFORM_TOKENS.join(", "),
        });
    };
    if named != host {
        return Err(BaselineError::HostPlatformMismatch {
            class: class.to_string(),
            named: named.to_string(),
            host: host.to_string(),
        });
    }
    Ok(())
}

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

/// One measured cell as the record path carries it: scenario, fixture, and
/// the metrics produced for that pair.
pub type MeasuredCell = (String, String, CellMetrics);

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
    #[error(
        "machine class {class:?} names no platform; a class name must carry exactly one of \
         [{tokens}] as a hyphen-delimited segment, because baselines are per-platform and a \
         class that will not say which platform it belongs to cannot be checked against the host"
    )]
    ClassPlatformUnnamed { class: String, tokens: String },
    #[error(
        "machine class {class:?} names platform {named:?} but this binary runs on {host:?}; rows \
         are measured per-platform under per-platform metric names, so one platform's recorded \
         numbers are not another's bars"
    )]
    HostPlatformMismatch {
        class: String,
        named: String,
        host: String,
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

/// Recorded metrics of one cell that the run produced no value for, in
/// deterministic (sorted) order.
///
/// [`gate_cell`] walks only the metrics both sides hold, so a cell that
/// still runs but stops producing one of its recorded numbers (a renamed
/// metric, a platform whose row measures a different quantity under a
/// different name) would otherwise report green with that recorded bar
/// silently untested. This is [`uncovered_cells`] one level down: cell
/// coverage proves the row ran, metric coverage proves it produced what
/// the baseline gates.
#[must_use]
pub fn unmeasured_metrics(measured: &CellMetrics, recorded: &CellMetrics) -> Vec<String> {
    recorded
        .keys()
        .filter(|metric| !measured.contains_key(*metric))
        .cloned()
        .collect()
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

/// What ratcheting one measured metric against the recorded baseline did.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum RatchetOutcome {
    /// The metric was not in the baseline; the measured value is recorded as-is.
    New { metric: String, value: f64 },
    /// The measured value beat the recorded bar; the bar moves down to it.
    Improved { metric: String, old: f64, new: f64 },
    /// The measured value did not beat the recorded bar, so the bar is
    /// held. `regression_masked` is set when the measured value also
    /// exceeds the gated bar (`recorded * headroom`): the record is
    /// refusing to lower a bar the measurement would have breached, which
    /// is a regression signal the operator must see, not noise the ratchet
    /// should silently absorb.
    Held {
        metric: String,
        recorded: f64,
        measured: f64,
        regression_masked: bool,
    },
}

/// Ratchets one cell's `measured` metrics against the `existing` recorded
/// cell (if any), so a recorded bar moves only in the improving (lower)
/// direction. Every metric is lower-is-better (see the module invariant),
/// so the ratchet keeps `min(recorded, measured)` per metric; a metric the
/// baseline never held is recorded as-is. `controlled` selects the headroom
/// used to flag a masked regression via [`gate_headroom`].
///
/// The returned cell carries exactly the measured metric keys: an existing
/// metric the run did not remeasure is not carried forward, matching the
/// full-matrix record's from-scratch hygiene.
///
/// Ratcheting to `min` pins an absolute metric's bar to its best-ever quiet
/// run, so the gated ceiling becomes `min * ABSOLUTE_HEADROOM`. On a shared
/// class whose quiet absolute variance is itself ~1.5x (see
/// [`ABSOLUTE_HEADROOM`]) that ceiling sits near the top of the honest band,
/// so a normal quiet run can breach; the load-controlled classes it matters
/// for do not carry that variance. Sizing a noise-aware floor into the
/// downward move (only ratchet down past the host's measured resolution) is
/// the refinement that closes this, and it needs the per-host floor that is
/// not yet measured; until then the min-ratchet is the faithful "only
/// improves" rule and the shared-class breach is the documented
/// loud-breach-then-rerun regime.
#[must_use]
pub fn ratchet_cell(
    existing: Option<&CellMetrics>,
    measured: &CellMetrics,
    controlled: bool,
) -> (CellMetrics, Vec<RatchetOutcome>) {
    let mut cell = CellMetrics::new();
    let mut outcomes = Vec::new();
    for (metric, &value) in measured {
        match existing.and_then(|existing| existing.get(metric)) {
            None => {
                cell.insert(metric.clone(), value);
                outcomes.push(RatchetOutcome::New {
                    metric: metric.clone(),
                    value,
                });
            }
            // a non-finite or non-positive measurement is not a real
            // improvement: every recorded metric is a positive latency, ratio,
            // or size, and lowering the bar to 0.0 or NaN would make the gate
            // breach on every later honest measurement
            Some(&recorded) if value.is_finite() && value > 0.0 && value < recorded => {
                cell.insert(metric.clone(), value);
                outcomes.push(RatchetOutcome::Improved {
                    metric: metric.clone(),
                    old: recorded,
                    new: value,
                });
            }
            Some(&recorded) => {
                cell.insert(metric.clone(), recorded);
                let regression_masked = gate_headroom(metric, controlled)
                    .is_some_and(|headroom| value > recorded * headroom);
                outcomes.push(RatchetOutcome::Held {
                    metric: metric.clone(),
                    recorded,
                    measured: value,
                    regression_masked,
                });
            }
        }
    }
    (cell, outcomes)
}

/// Which cells a record touches, which decides how the recorded file is
/// assembled around the ratchet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecordMode {
    /// The whole matrix was measured. The file is rebuilt from the measured
    /// cells so a cell that left the matrix does not survive with a stale
    /// bar, and an existing file that cannot be a ratchet reference (a
    /// different pin or class) is discarded rather than blocking the record.
    FullMatrix,
    /// A single cell was measured. The existing file's other cells are
    /// preserved untouched; only the measured cell is ratcheted in.
    SingleCell,
}

/// What one cell's ratchet did during a record.
#[derive(Debug, Clone, PartialEq)]
pub struct CellRatchet {
    pub scenario: String,
    pub fixture: String,
    pub outcomes: Vec<RatchetOutcome>,
}

/// The file to save and a per-cell account of what the ratchet did.
#[derive(Debug, Clone)]
pub struct RecordPlan {
    pub file: BaselineFile,
    pub cells: Vec<CellRatchet>,
    /// Set when a full-matrix record found an existing baseline it could
    /// not ratchet against (different pin or class) and recorded fresh
    /// instead; the operator is told why the bars did not ratchet.
    pub reset_reason: Option<String>,
}

/// Assembles the baseline to record from `measured`, ratcheting each cell
/// against `existing` when that file is a valid reference for it (same
/// engine `pin` and `class`; see [`ratchet_cell`] for the per-metric rule).
///
/// The two record modes differ only in how the surrounding file is built:
/// [`RecordMode::FullMatrix`] starts fresh so stale cells fall away and an
/// incomparable existing file is set aside (recorded in `reset_reason`);
/// [`RecordMode::SingleCell`] preserves the existing file's other cells.
///
/// Precondition for [`RecordMode::SingleCell`]: when `existing` is
/// `Some`, it must already be pin- and class-matched by the caller (via
/// [`require_pin_match`]/[`require_class_match`]), because a single-cell
/// edit of a file that fails those checks would silently invalidate it.
#[must_use]
pub fn plan_record(
    existing: Option<BaselineFile>,
    mode: RecordMode,
    class: &str,
    pin: &str,
    measured: &[MeasuredCell],
) -> RecordPlan {
    let controlled = is_controlled_class(class);
    let comparable = existing
        .as_ref()
        .is_some_and(|file| file.engine_pin == pin && file.machine_class == class);

    let reset_reason = match &existing {
        Some(file) if !comparable => Some(format!(
            "existing baseline is pin {:?} class {:?}, not pin {pin:?} class {class:?}; recorded \
             fresh, no ratchet",
            file.engine_pin, file.machine_class
        )),
        _ => None,
    };

    // A single-cell record edits one cell of the existing file, so it starts
    // from that file to keep the others -- but only when the file is
    // comparable; cloning an incomparable one would save its cells (measured
    // under a different engine or class) relabeled under this run's pin. A
    // full-matrix record always rebuilds the file so a cell that left the
    // matrix cannot survive with a stale bar.
    let mut file = match (mode, &existing) {
        (RecordMode::SingleCell, Some(existing)) if comparable => existing.clone(),
        _ => BaselineFile::new(class, pin),
    };

    // The ratchet reference is the existing file only when it is comparable;
    // an incomparable file's numbers were taken against a different engine or
    // gate policy and are not a bar this run's numbers may be held to.
    let reference = comparable.then_some(existing.as_ref()).flatten();

    let mut cells = Vec::new();
    for (scenario, fixture, metrics) in measured {
        let existing_cell = reference.and_then(|file| file.cell(scenario, fixture));
        let (ratcheted, outcomes) = ratchet_cell(existing_cell, metrics, controlled);
        file.upsert_cell(scenario, fixture, ratcheted);
        cells.push(CellRatchet {
            scenario: scenario.clone(),
            fixture: fixture.clone(),
            outcomes,
        });
    }

    RecordPlan {
        file,
        cells,
        reset_reason,
    }
}

impl RecordPlan {
    /// The number of held metrics whose measurement would have breached the
    /// gate: a ratcheted record kept the better bar, but each of these is a
    /// regression the operator must see rather than a bar that merely failed
    /// to improve.
    #[must_use]
    pub fn masked_regressions(&self) -> usize {
        self.cells
            .iter()
            .flat_map(|cell| &cell.outcomes)
            .filter(|outcome| {
                matches!(
                    outcome,
                    RatchetOutcome::Held {
                        regression_masked: true,
                        ..
                    }
                )
            })
            .count()
    }

    /// Human-readable lines summarizing what the record did, for `target`
    /// (the baseline path). Carries the reset note (if any), one line per
    /// metric ratcheted, the count recorded, and a trailing warning when any
    /// held bar hid a regression, so the operator never reads "recorded N
    /// cells" as "the bars all moved."
    #[must_use]
    pub fn report_lines(&self, target: &str) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(reason) = &self.reset_reason {
            lines.push(format!("      {reason}"));
        }
        for cell in &self.cells {
            let (scenario, fixture) = (&cell.scenario, &cell.fixture);
            for outcome in &cell.outcomes {
                lines.push(match outcome {
                    RatchetOutcome::Improved { metric, old, new } => {
                        format!(
                            "      improved {scenario}.{fixture} {metric}: {old:.4} -> {new:.4}"
                        )
                    }
                    RatchetOutcome::New { metric, value } => {
                        format!("      new {scenario}.{fixture} {metric}: {value:.4}")
                    }
                    RatchetOutcome::Held {
                        metric,
                        recorded,
                        measured,
                        regression_masked: false,
                    } => format!(
                        "      held {scenario}.{fixture} {metric}: recorded {recorded:.4} kept \
                         (measured {measured:.4} did not improve it)"
                    ),
                    RatchetOutcome::Held {
                        metric,
                        recorded,
                        measured,
                        regression_masked: true,
                    } => format!(
                        "      REGRESSION MASKED {scenario}.{fixture} {metric}: measured \
                         {measured:.4} would breach the recorded bar {recorded:.4}, held to keep \
                         the ratchet; a later --gate will breach on it, so investigate first"
                    ),
                });
            }
        }
        lines.push(format!(
            "recorded {} cell(s) into {target}",
            self.cells.len()
        ));
        let masked = self.masked_regressions();
        if masked > 0 {
            lines.push(format!(
                "      WARNING: {masked} held metric(s) would have breached the gate; the record \
                 kept the better bar but the measurement shows a regression"
            ));
        }
        lines
    }
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
        // a baseline holding a cell whose scenario this platform has no
        // measurement for, which is the shape the skip list exists for
        let mut file = BaselineFile::new("gh-windows", "v0.12.4");
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
    fn class_platform_reads_one_hyphen_delimited_token() {
        assert_eq!(class_platform("dev-linux"), Some("linux"));
        assert_eq!(class_platform("gh-macos"), Some("macos"));
        assert_eq!(class_platform("controlled-linux-x86"), Some("linux"));
        assert_eq!(class_platform("gh-windows"), Some("windows"));
        assert_eq!(class_platform("dev-linuxish"), None);
        assert_eq!(class_platform("bench-box-1"), None);
        assert_eq!(class_platform("dev-linux-macos"), None);
    }

    #[test]
    fn a_class_naming_another_platform_is_refused_on_this_host() {
        let host = std::env::consts::OS;
        assert!(
            require_host_platform(&format!("dev-{host}")).is_ok(),
            "the host's own platform must be accepted"
        );
        let other = if host == "linux" { "macos" } else { "linux" };
        let err = require_host_platform(&format!("gh-{other}")).unwrap_err();
        assert!(
            matches!(err, BaselineError::HostPlatformMismatch { .. }),
            "a foreign platform's class must be refused, got {err}"
        );
        let err = require_host_platform("bench-box-1").unwrap_err();
        assert!(matches!(err, BaselineError::ClassPlatformUnnamed { .. }));
    }

    #[test]
    fn a_recorded_metric_the_run_never_measured_is_named() {
        let recorded = metrics(&[("pss_mb", 3.4)]);
        let measured = metrics(&[("phys_footprint_mb", 41.0)]);
        assert!(
            gate_cell("memory", "minimal", &measured, &recorded, "dev-linux").is_empty(),
            "a differently named metric cannot breach, which is why coverage must catch it"
        );
        assert_eq!(unmeasured_metrics(&measured, &recorded), vec!["pss_mb"]);
        assert!(unmeasured_metrics(&recorded, &recorded).is_empty());
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
                gate_headroom("pace_ratio", controlled),
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

    fn outcome_for<'a>(outcomes: &'a [RatchetOutcome], name: &str) -> &'a RatchetOutcome {
        outcomes
            .iter()
            .find(|o| match o {
                RatchetOutcome::New { metric, .. }
                | RatchetOutcome::Improved { metric, .. }
                | RatchetOutcome::Held { metric, .. } => metric == name,
            })
            .expect("outcome present for the named metric")
    }

    #[test]
    fn ratchet_holds_the_bar_when_the_measurement_regresses() {
        let existing = metrics(&[("ratio_p50", 1.20)]);
        let measured = metrics(&[("ratio_p50", 1.35)]);
        let (cell, outcomes) = ratchet_cell(Some(&existing), &measured, false);
        assert_eq!(
            cell["ratio_p50"], 1.20,
            "a worse measurement must not raise the recorded bar"
        );
        assert_eq!(
            outcome_for(&outcomes, "ratio_p50"),
            &RatchetOutcome::Held {
                metric: "ratio_p50".to_string(),
                recorded: 1.20,
                measured: 1.35,
                regression_masked: false,
            }
        );
    }

    #[test]
    fn ratchet_lowers_the_bar_when_the_measurement_improves() {
        let existing = metrics(&[("ratio_p50", 1.35)]);
        let measured = metrics(&[("ratio_p50", 1.20)]);
        let (cell, outcomes) = ratchet_cell(Some(&existing), &measured, false);
        assert_eq!(
            cell["ratio_p50"], 1.20,
            "a better measurement must lower the recorded bar to it"
        );
        assert_eq!(
            outcome_for(&outcomes, "ratio_p50"),
            &RatchetOutcome::Improved {
                metric: "ratio_p50".to_string(),
                old: 1.35,
                new: 1.20,
            }
        );
    }

    #[test]
    fn ratchet_records_a_metric_the_baseline_never_held() {
        let existing = metrics(&[("ratio_p50", 1.20)]);
        let measured = metrics(&[("ratio_p50", 1.15), ("view_p99_ms", 2.0)]);
        let (cell, outcomes) = ratchet_cell(Some(&existing), &measured, false);
        assert_eq!(cell["view_p99_ms"], 2.0);
        assert_eq!(
            outcome_for(&outcomes, "view_p99_ms"),
            &RatchetOutcome::New {
                metric: "view_p99_ms".to_string(),
                value: 2.0,
            }
        );
    }

    #[test]
    fn ratchet_with_no_existing_cell_records_every_metric_as_new() {
        let measured = metrics(&[("ratio_p50", 1.20), ("view_p99_ms", 2.0)]);
        let (cell, outcomes) = ratchet_cell(None, &measured, false);
        assert_eq!(cell, measured);
        assert!(outcomes
            .iter()
            .all(|o| matches!(o, RatchetOutcome::New { .. })));
        assert_eq!(outcomes.len(), 2);
    }

    #[test]
    fn ratchet_flags_a_held_value_that_exceeds_the_gated_bar_as_a_masked_regression() {
        // recorded 1.0, headroom 1.25 -> gated bar 1.25; a measurement of
        // 1.40 both fails to improve AND would breach the gate, so holding
        // the bar hides a real regression the operator must be told about.
        let existing = metrics(&[("ratio_p50", 1.0)]);
        let measured = metrics(&[("ratio_p50", 1.40)]);
        let (cell, outcomes) = ratchet_cell(Some(&existing), &measured, false);
        assert_eq!(cell["ratio_p50"], 1.0);
        assert_eq!(
            outcome_for(&outcomes, "ratio_p50"),
            &RatchetOutcome::Held {
                metric: "ratio_p50".to_string(),
                recorded: 1.0,
                measured: 1.40,
                regression_masked: true,
            }
        );
    }

    #[test]
    fn ratchet_does_not_carry_forward_a_metric_the_run_did_not_remeasure() {
        let existing = metrics(&[("ratio_p50", 1.20), ("stale_metric", 9.0)]);
        let measured = metrics(&[("ratio_p50", 1.15)]);
        let (cell, _outcomes) = ratchet_cell(Some(&existing), &measured, false);
        assert!(
            !cell.contains_key("stale_metric"),
            "an existing metric the run did not remeasure must not survive: {cell:?}"
        );
    }

    #[test]
    fn ratchet_flags_masked_regression_only_for_gated_metrics() {
        // ratio_p99 is ungated on a shared class (gate_headroom None), so a
        // held-worse value there is not a masked gate breach: there is no
        // bar for it to have breached.
        let existing = metrics(&[("ratio_p99", 1.0)]);
        let measured = metrics(&[("ratio_p99", 5.0)]);
        let (_cell, outcomes) = ratchet_cell(Some(&existing), &measured, false);
        assert_eq!(
            outcome_for(&outcomes, "ratio_p99"),
            &RatchetOutcome::Held {
                metric: "ratio_p99".to_string(),
                recorded: 1.0,
                measured: 5.0,
                regression_masked: false,
            }
        );
    }

    fn measured_cell(scenario: &str, fixture: &str, pairs: &[(&str, f64)]) -> MeasuredCell {
        (scenario.to_string(), fixture.to_string(), metrics(pairs))
    }

    type CellSpec<'a> = (&'a str, &'a str, &'a [(&'a str, f64)]);

    fn baseline_with(pin: &str, class: &str, cells: &[CellSpec]) -> BaselineFile {
        let mut file = BaselineFile::new(class, pin);
        for (scenario, fixture, pairs) in cells {
            file.upsert_cell(scenario, fixture, metrics(pairs));
        }
        file
    }

    #[test]
    fn full_matrix_record_ratchets_each_cell_against_a_comparable_baseline() {
        let existing = baseline_with(
            "v0.12.4",
            "dev-linux",
            &[("echo", "minimal", &[("ratio_p50", 1.30)])],
        );
        let measured = vec![measured_cell("echo", "minimal", &[("ratio_p50", 1.20)])];
        let plan = plan_record(
            Some(existing),
            RecordMode::FullMatrix,
            "dev-linux",
            "v0.12.4",
            &measured,
        );
        assert_eq!(
            plan.file.cell("echo", "minimal").unwrap()["ratio_p50"],
            1.20
        );
        assert!(plan.reset_reason.is_none());
    }

    #[test]
    fn full_matrix_record_drops_a_cell_no_longer_measured() {
        let existing = baseline_with(
            "v0.12.4",
            "dev-linux",
            &[
                ("echo", "minimal", &[("ratio_p50", 1.30)]),
                ("scroll", "minimal", &[("ratio_p50", 1.70)]),
            ],
        );
        let measured = vec![measured_cell("echo", "minimal", &[("ratio_p50", 1.25)])];
        let plan = plan_record(
            Some(existing),
            RecordMode::FullMatrix,
            "dev-linux",
            "v0.12.4",
            &measured,
        );
        assert!(
            plan.file.cell("scroll", "minimal").is_none(),
            "a full-matrix record must not carry a cell the run no longer measures"
        );
    }

    #[test]
    fn full_matrix_record_ignores_a_pin_drifted_baseline_and_records_fresh() {
        let existing = baseline_with(
            "v0.12.3",
            "dev-linux",
            &[("echo", "minimal", &[("ratio_p50", 0.5)])],
        );
        let measured = vec![measured_cell("echo", "minimal", &[("ratio_p50", 1.20)])];
        let plan = plan_record(
            Some(existing),
            RecordMode::FullMatrix,
            "dev-linux",
            "v0.12.4",
            &measured,
        );
        assert_eq!(
            plan.file.cell("echo", "minimal").unwrap()["ratio_p50"],
            1.20,
            "a baseline under another pin must not ratchet the fresh measurement down to its own number"
        );
        assert!(plan.reset_reason.is_some());
        assert!(matches!(
            plan.cells[0].outcomes[0],
            RatchetOutcome::New { .. }
        ));
    }

    #[test]
    fn full_matrix_record_with_no_existing_file_records_fresh_without_a_reset_reason() {
        let measured = vec![measured_cell("echo", "minimal", &[("ratio_p50", 1.20)])];
        let plan = plan_record(
            None,
            RecordMode::FullMatrix,
            "dev-linux",
            "v0.12.4",
            &measured,
        );
        assert_eq!(
            plan.file.cell("echo", "minimal").unwrap()["ratio_p50"],
            1.20
        );
        assert!(
            plan.reset_reason.is_none(),
            "recording the first baseline is not a reset"
        );
    }

    #[test]
    fn single_cell_record_preserves_the_other_cells_and_ratchets_the_measured_one() {
        let existing = baseline_with(
            "v0.12.4",
            "dev-linux",
            &[
                ("echo", "minimal", &[("ratio_p50", 1.30)]),
                ("scroll", "minimal", &[("ratio_p50", 1.70)]),
            ],
        );
        let measured = vec![measured_cell("echo", "minimal", &[("ratio_p50", 1.20)])];
        let plan = plan_record(
            Some(existing),
            RecordMode::SingleCell,
            "dev-linux",
            "v0.12.4",
            &measured,
        );
        assert_eq!(
            plan.file.cell("scroll", "minimal").unwrap()["ratio_p50"],
            1.70,
            "a single-cell record must leave the other cells untouched"
        );
        assert_eq!(
            plan.file.cell("echo", "minimal").unwrap()["ratio_p50"],
            1.20
        );
    }

    #[test]
    fn report_names_an_improved_metric_and_counts_no_masked_regression() {
        let existing = baseline_with(
            "v0.12.4",
            "dev-linux",
            &[("echo", "minimal", &[("ratio_p50", 1.30)])],
        );
        let measured = vec![measured_cell("echo", "minimal", &[("ratio_p50", 1.20)])];
        let plan = plan_record(
            Some(existing),
            RecordMode::FullMatrix,
            "dev-linux",
            "v0.12.4",
            &measured,
        );
        assert_eq!(plan.masked_regressions(), 0);
        let lines = plan.report_lines("dev-linux.toml");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("improved") && l.contains("ratio_p50")),
            "an improved metric must be named in the report: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("recorded 1 cell(s) into dev-linux.toml")),
            "the report must state where and how many cells were recorded: {lines:?}"
        );
    }

    #[test]
    fn report_flags_a_masked_regression_and_a_trailing_warning() {
        // recorded 1.0, headroom 1.25; measuring 1.40 does not improve and
        // would breach the gate, so the held bar hides a regression.
        let existing = baseline_with(
            "v0.12.4",
            "dev-linux",
            &[("echo", "minimal", &[("ratio_p50", 1.0)])],
        );
        let measured = vec![measured_cell("echo", "minimal", &[("ratio_p50", 1.40)])];
        let plan = plan_record(
            Some(existing),
            RecordMode::FullMatrix,
            "dev-linux",
            "v0.12.4",
            &measured,
        );
        assert_eq!(plan.masked_regressions(), 1);
        let lines = plan.report_lines("dev-linux.toml");
        assert!(
            lines.iter().any(|l| l.contains("REGRESSION MASKED")),
            "a masked regression must be called out: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.starts_with("      WARNING:")),
            "a trailing warning must summarize the masked regression: {lines:?}"
        );
    }

    #[test]
    fn ratchet_does_not_lower_a_bar_to_a_nonpositive_measurement() {
        // a 0.0 (or negative) measurement would install a 0.0 bar, and the
        // gate bar 0.0 * headroom = 0.0 breaches on every later positive
        // measurement forever; a metric that cannot physically be <= 0 must
        // never ratchet the bar there.
        let existing = metrics(&[("view_p99_ms", 2.0)]);
        let (cell, _) = ratchet_cell(Some(&existing), &metrics(&[("view_p99_ms", 0.0)]), false);
        assert_eq!(
            cell["view_p99_ms"], 2.0,
            "a 0.0 measurement must not install a 0.0 bar"
        );
    }

    #[test]
    fn ratchet_does_not_lower_a_bar_to_a_nonfinite_measurement() {
        let existing = metrics(&[("view_p99_ms", 2.0)]);
        let (cell, _) = ratchet_cell(
            Some(&existing),
            &metrics(&[("view_p99_ms", f64::NAN)]),
            false,
        );
        assert_eq!(
            cell["view_p99_ms"], 2.0,
            "a non-finite measurement must not disturb the recorded bar"
        );
    }

    #[test]
    fn ratchet_flags_masked_regression_for_a_tail_metric_on_a_controlled_class() {
        // ratio_p99 is gated ONLY on controlled classes; there a held-worse
        // value that would breach must be flagged, exactly opposite to the
        // shared-class case tested above.
        let existing = metrics(&[("ratio_p99", 1.0)]);
        let (_cell, outcomes) =
            ratchet_cell(Some(&existing), &metrics(&[("ratio_p99", 1.40)]), true);
        assert_eq!(
            outcome_for(&outcomes, "ratio_p99"),
            &RatchetOutcome::Held {
                metric: "ratio_p99".to_string(),
                recorded: 1.0,
                measured: 1.40,
                regression_masked: true,
            }
        );
    }

    #[test]
    fn plan_record_derives_controlled_from_the_class_for_the_masked_flag() {
        let existing = baseline_with(
            "v0.12.4",
            "controlled-linux",
            &[("echo", "minimal", &[("ratio_p99", 1.0)])],
        );
        let measured = vec![measured_cell("echo", "minimal", &[("ratio_p99", 1.40)])];
        let plan = plan_record(
            Some(existing),
            RecordMode::FullMatrix,
            "controlled-linux",
            "v0.12.4",
            &measured,
        );
        assert_eq!(
            plan.masked_regressions(),
            1,
            "a controlled class must flag a tail-metric regression a shared class would not"
        );
    }

    #[test]
    fn full_matrix_record_holds_a_bar_the_measurement_did_not_beat() {
        let existing = baseline_with(
            "v0.12.4",
            "dev-linux",
            &[("echo", "minimal", &[("ratio_p50", 1.20)])],
        );
        let measured = vec![measured_cell("echo", "minimal", &[("ratio_p50", 1.35)])];
        let plan = plan_record(
            Some(existing),
            RecordMode::FullMatrix,
            "dev-linux",
            "v0.12.4",
            &measured,
        );
        assert_eq!(
            plan.file.cell("echo", "minimal").unwrap()["ratio_p50"],
            1.20,
            "a worse full-matrix remeasure must hold the better recorded bar"
        );
    }

    #[test]
    fn full_matrix_record_ignores_a_class_mismatched_baseline() {
        let existing = baseline_with(
            "v0.12.4",
            "controlled-linux",
            &[("echo", "minimal", &[("ratio_p50", 0.5)])],
        );
        let measured = vec![measured_cell("echo", "minimal", &[("ratio_p50", 1.20)])];
        let plan = plan_record(
            Some(existing),
            RecordMode::FullMatrix,
            "dev-linux",
            "v0.12.4",
            &measured,
        );
        assert_eq!(
            plan.file.cell("echo", "minimal").unwrap()["ratio_p50"],
            1.20
        );
        assert_eq!(plan.file.machine_class, "dev-linux");
        assert!(
            plan.reset_reason.is_some(),
            "a class-mismatched baseline is not a ratchet reference"
        );
    }

    #[test]
    fn single_cell_record_against_an_incomparable_existing_records_fresh_not_a_stale_clone() {
        // the caller normally errors before here, but plan_record must not
        // silently save the old pin's cells relabeled under the new run: the
        // saved file must carry this run's pin and drop cells measured under
        // the old engine.
        let existing = baseline_with(
            "v0.12.3",
            "dev-linux",
            &[
                ("echo", "minimal", &[("ratio_p50", 0.5)]),
                ("scroll", "minimal", &[("ratio_p50", 1.7)]),
            ],
        );
        let measured = vec![measured_cell("echo", "minimal", &[("ratio_p50", 1.20)])];
        let plan = plan_record(
            Some(existing),
            RecordMode::SingleCell,
            "dev-linux",
            "v0.12.4",
            &measured,
        );
        assert_eq!(
            plan.file.engine_pin, "v0.12.4",
            "the saved file must carry the run's pin, not the drifted one"
        );
        assert!(
            plan.file.cell("scroll", "minimal").is_none(),
            "cells measured under the old pin must not survive relabeled under the new one"
        );
        assert_eq!(
            plan.file.cell("echo", "minimal").unwrap()["ratio_p50"],
            1.20,
            "the fresh measurement, not the drifted 0.5, must be recorded"
        );
        assert!(plan.reset_reason.is_some());
    }
}
