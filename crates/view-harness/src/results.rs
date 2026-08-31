//! The compat run's output shape: `compat/results.json`, one [`ScenarioResult`]
//! per scenario file the `compat` subcommand drove. A committed library type
//! (not an ad hoc `serde_json::json!` blob assembled in `bin/oracle.rs`) so
//! the evidence-page renderer ([`crate::page`]) deserializes the exact
//! shape a run produced instead of re-deriving it from field-name guesses.
//!
//! The field set mirrors the design spec's own compat-evidence row schema:
//! "plugin, version, engine pin, scenario, state, result, date".

use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// One scenario's outcome. `plugin_version` is best-effort: populated when
/// the scenario's fixture has a `lazy-lock.json` naming `plugin` at a
/// pinned commit, `None` for a fixture-less or no-lockfile fixture (the
/// `minimal` fixture, or a scenario whose plugin the lockfile does not
/// name).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScenarioResult {
    pub scenario_path: String,
    pub plugin: String,
    pub plugin_version: Option<String>,
    pub class: String,
    pub fixture: Option<String>,
    pub state: String,
    pub engine_pin: String,
    pub status: ScenarioStatus,
    /// `None` when `status` is [`ScenarioStatus::Skipped`] (no step ever
    /// ran), naming the failing step's index otherwise only when `status`
    /// is [`ScenarioStatus::Failed`].
    pub failing_step: Option<usize>,
    pub steps_total: usize,
    pub detail: Option<String>,
    pub elapsed_ms: u128,
    /// `YYYY-MM-DD`, the day the run occurred -- the compat-evidence page's
    /// staleness rule ("every engine-pin bump re-runs the matrix and
    /// re-dates the page") keys off this, not a full timestamp.
    pub date: String,
}

/// `YYYY-MM-DD` for the current instant, in UTC. Hand-rolled rather than a
/// `chrono`/`time` dependency: this is the only date computation anywhere
/// in the workspace, for one report-row stamp
/// ([`view_harness::results::ScenarioResult::date`]).
pub fn today_date_string() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant's days-since-epoch -> proleptic Gregorian civil date
/// algorithm (public domain: <http://howardhinnant.github.io/date_algorithms.html>),
/// pinned by [`civil_from_days_matches_known_dates`] against independently
/// computed reference values rather than trusted from transcription alone.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// A scenario's terminal outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ScenarioStatus {
    Ok,
    Failed,
    /// A fixture-less scenario with `$VIEW_DAILY_CONFIG` unset (the
    /// maintainer's standing daily-config scenario), reported
    /// SKIPPED-with-notice rather than failing a CI run that has no daily
    /// config to test against.
    Skipped,
}

/// The full `compat/results.json` document: every scenario this run drove,
/// in the order they ran.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResultsFile {
    pub results: Vec<ScenarioResult>,
}

/// Errors writing or reading `compat/results.json`.
#[derive(Debug, Error)]
pub enum ResultsError {
    #[error("failed to read results file {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to serialize compat results")]
    Serialize(#[from] serde_json::Error),
}

/// Writes `results` to `path` as pretty-printed JSON.
///
/// # Errors
///
/// Returns [`ResultsError::Serialize`] if `results` cannot be serialized, or
/// [`ResultsError::Io`] if `path` cannot be written.
pub fn write_results(path: &Path, results: &ResultsFile) -> Result<(), ResultsError> {
    let text = serde_json::to_string_pretty(results)?;
    std::fs::write(path, text).map_err(|source| ResultsError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Reads and parses `path` as a [`ResultsFile`] -- the inverse of
/// [`write_results`], for a consumer (the `page` subcommand) that reads a
/// prior run's results back rather than re-running the whole compat suite.
///
/// # Errors
///
/// Returns [`ResultsError::Io`] if `path` cannot be read, or
/// [`ResultsError::Serialize`] if its content is not a valid [`ResultsFile`].
pub fn load_results(path: &Path) -> Result<ResultsFile, ResultsError> {
    let text = std::fs::read_to_string(path).map_err(|source| ResultsError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(serde_json::from_str(&text)?)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use view_test_support::ScratchDir;

    fn sample() -> ResultsFile {
        ResultsFile {
            results: vec![ScenarioResult {
                scenario_path: "compat/scenarios/lualine.toml".to_string(),
                plugin: "lualine".to_string(),
                plugin_version: Some("abc1234".to_string()),
                class: "ui-owning".to_string(),
                fixture: Some("heavy".to_string()),
                state: "present".to_string(),
                engine_pin: "v0.12.4".to_string(),
                status: ScenarioStatus::Ok,
                failing_step: None,
                steps_total: 4,
                detail: None,
                elapsed_ms: 2100,
                date: "2026-07-19".to_string(),
            }],
        }
    }

    #[test]
    fn results_round_trip_through_write_and_load() {
        let dir = ScratchDir::new("harness-results-write-round-trip").unwrap();
        let path = dir.join("results.json");

        write_results(&path, &sample()).expect("write_results failed");
        let loaded = load_results(&path).expect("written results must parse back");

        assert_eq!(loaded.results.len(), 1);
        assert_eq!(loaded.results[0].plugin, "lualine");
        assert_eq!(loaded.results[0].status, ScenarioStatus::Ok);
    }

    /// Reference values independently computed via Python's
    /// `datetime.date` (`epoch + timedelta(days=N)`), not transcribed from
    /// the Hinnant algorithm's own worked examples -- an independent
    /// derivation path, per this codebase's own re-derive-don't-recognize
    /// standard, catches a transcription bug a self-referential check
    /// would not.
    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(1), (1970, 1, 2));
        assert_eq!(civil_from_days(365), (1971, 1, 1));
        assert_eq!(civil_from_days(366), (1971, 1, 2));
        assert_eq!(civil_from_days(1000), (1972, 9, 27));
        assert_eq!(civil_from_days(19570), (2023, 8, 1));
        assert_eq!(civil_from_days(20653), (2026, 7, 19));
    }

    #[test]
    fn status_serializes_as_screaming_snake_case() {
        let text = serde_json::to_string(&ScenarioStatus::Skipped).expect("serialize failed");
        assert_eq!(text, "\"SKIPPED\"");
    }
}
