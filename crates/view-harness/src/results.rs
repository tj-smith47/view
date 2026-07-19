//! The compat run's output shape: `compat/results.json`, one [`ScenarioResult`]
//! per scenario file the `compat` subcommand drove. A committed library type
//! (not an ad hoc `serde_json::json!` blob assembled in `bin/oracle.rs`) so
//! a future matrix-filling and evidence-page-rendering consumer can
//! deserialize the exact shape a run produced instead of re-deriving it
//! from field-name guesses.
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

/// A scenario's terminal outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ScenarioStatus {
    Ok,
    Failed,
    /// A fixture-less scenario with `$VIEW_DAILY_CONFIG` unset (the
    /// maintainer's standing daily-config scenario, reported
    /// SKIPPED-with-notice per the design brief rather than failing a CI
    /// run that has no daily config to test against).
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
/// [`write_results`], for a future consumer that reads a prior run's
/// results back rather than re-running the whole compat suite.
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
        let dir = std::env::temp_dir().join(format!(
            "view-harness-results-write-{}-round-trip",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("failed to create scratch dir");
        let path = dir.join("results.json");

        write_results(&path, &sample()).expect("write_results failed");
        let loaded = load_results(&path).expect("written results must parse back");

        assert_eq!(loaded.results.len(), 1);
        assert_eq!(loaded.results[0].plugin, "lualine");
        assert_eq!(loaded.results[0].status, ScenarioStatus::Ok);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_serializes_as_screaming_snake_case() {
        let text = serde_json::to_string(&ScenarioStatus::Skipped).expect("serialize failed");
        assert_eq!(text, "\"SKIPPED\"");
    }
}
