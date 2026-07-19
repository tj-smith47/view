//! The compat scenario TOML schema and loader: each entry names a plugin
//! under test, its config-reconciliation class, the fixture (if any) it runs
//! against, and a fixed, declarative list of steps -- the same "durable,
//! reviewable artifact instead of a hardcoded Rust test" rationale
//! `view_harness::corpus` documents for the differential oracle's own
//! corpus, applied to the compat harness.
//!
//! Mirrors `corpus`'s own layering deliberately: a private `Raw*` shape
//! (`#[serde(deny_unknown_fields)]`, parsed straight off `toml::from_str`)
//! validated into a public, already-typed result -- here, [`ScenarioFile`],
//! whose `class`/`state`/`steps` fields are `view_oracle::compat` types
//! rather than this crate's own, since `view-oracle` is the crate that
//! knows how to *drive* a step and must stay serde-free (see that crate's
//! `compat` module docs). `schema = 1` is the only version accepted, for
//! the same reason `corpus`'s loader pins one: a schema bump implies a
//! shape change this loader has not been taught to read.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;
use view_oracle::compat::{resolve_send_keys, CompatError, PluginClass, ScenarioState, Step};

/// The only `schema` value this loader accepts today.
const SUPPORTED_SCHEMA: u32 = 1;

/// Default timeout for a `wait_for`/`wait_for_cell` step that omits its own
/// `timeout_ms`. Matches `corpus::DEFAULT_QUIESCE_DEADLINE_MS`: the same
/// "generous enough for a slow run, tight enough that a genuinely wedged
/// session fails promptly" reasoning applies to a compat step's wait as it
/// does to the differential oracle's own quiesce deadline.
pub const DEFAULT_STEP_TIMEOUT_MS: u64 = 5_000;

/// The wire shape of a scenario TOML file, deserialized directly by `toml`
/// before any validation. Kept separate from [`ScenarioFile`] for the same
/// reason `corpus::RawEntry` is kept separate from `CorpusEntry`: a
/// missing- or unknown-field error should surface from `serde`/`toml`
/// itself, in their own words, not be re-derived by hand here.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScenario {
    schema: u32,
    plugin: String,
    class: String,
    fixture: Option<String>,
    state: String,
    #[serde(default)]
    cold_bootstrap: bool,
    steps: Vec<RawStep>,
}

/// One step's wire shape. Exactly one of `send` / `wait_for` /
/// `wait_for_cell` / `assert_absent` / `probe` must be set -- [`validate_step`]
/// enforces that, since `serde`'s own `deny_unknown_fields` only catches a
/// field name it has never heard of, not a legal field used on the wrong
/// variant (e.g. `expect` with no `probe`, or two action fields at once).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStep {
    send: Option<String>,
    wait_for: Option<String>,
    wait_for_cell: Option<RawCellTarget>,
    assert_absent: Option<String>,
    probe: Option<String>,
    wait_for_probe: Option<String>,
    expect: Option<String>,
    timeout_ms: Option<u64>,
}

/// `wait_for_cell`'s inline-table shape: `{ row = 23, col = 0, expected = ":" }`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCellTarget {
    row: u16,
    col: u16,
    expected: String,
}

/// One validated, defaults-applied scenario, ready for the `compat`
/// subcommand to drive: everything [`view_oracle::compat::CompatSession`]
/// needs (`class`/`state`/`steps` already `view-oracle` types) plus the
/// bookkeeping fields (`plugin`, `fixture`, `cold_bootstrap`) the runner's
/// own fixture resolution and report line need.
#[derive(Debug, Clone)]
pub struct ScenarioFile {
    pub plugin: String,
    pub class: PluginClass,
    /// Names a subdirectory of `compat/fixtures/`, or `None` for a
    /// fixture-less scenario (the maintainer's `$VIEW_DAILY_CONFIG`, whose
    /// `init.lua` the harness does not own and so cannot rely on carrying
    /// its own `serverstart` call -- see `CompatSession::prime_probe_channel`).
    pub fixture: Option<String>,
    pub state: ScenarioState,
    /// Forces the heavy fixture's plugin cache to a run-unique, guaranteed-
    /// empty key instead of its normal lockfile-hash key, so this scenario
    /// always drives a full network bootstrap rather than ever reusing a
    /// prior warm cache. `false` for every ordinary scenario.
    pub cold_bootstrap: bool,
    pub steps: Vec<Step>,
}

/// Errors loading or validating a scenario entry.
#[derive(Debug, Error)]
pub enum ScenarioError {
    /// The scenario file could not be read from disk.
    #[error("failed to read scenario file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The file's content is not valid TOML for the [`RawScenario`] shape --
    /// covers both a malformed document and a `deny_unknown_fields`
    /// rejection of an unrecognized key.
    #[error("failed to parse scenario TOML")]
    Toml(#[from] toml::de::Error),
    /// `schema` named a version this loader has not been taught to read.
    #[error("unsupported scenario schema {0} (only schema = 1 is recognized)")]
    UnsupportedSchema(u32),
    /// `class` did not name one of the design spec's three
    /// config-reconciliation classes.
    #[error("unknown class {0:?} (expected one of \"semantic\", \"ui-adjacent\", \"ui-owning\")")]
    UnknownClass(String),
    /// `state` named a value other than `"present"`. The design spec's
    /// other two states (superseded, native-without-plugin) need
    /// supersession machinery the engine does not have yet -- see
    /// [`ScenarioState`]'s own doc comment.
    #[error("unsupported state {0:?} (only \"present\" is currently recognized)")]
    UnsupportedState(String),
    /// A step set zero, or more than one, of its mutually exclusive action
    /// fields (`send` / `wait_for` / `wait_for_cell` / `assert_absent` /
    /// `probe` / `wait_for_probe`).
    #[error("step {index} must set exactly one of send/wait_for/wait_for_cell/assert_absent/probe/wait_for_probe, found {found}")]
    AmbiguousStep { index: usize, found: usize },
    /// A `probe`/`wait_for_probe` step is missing its required `expect`
    /// field.
    #[error("step {index} is a probe step but has no expect field")]
    ProbeMissingExpect { index: usize },
    /// `expect` was set on a step whose action is not `probe`.
    #[error("step {index} sets expect but is not a probe step")]
    ExpectWithoutProbe { index: usize },
    /// `timeout_ms` was set on a step whose action is none of `wait_for`,
    /// `wait_for_cell`, or `wait_for_probe` (the only three steps that poll
    /// toward a deadline).
    #[error(
        "step {index} sets timeout_ms but is not a wait_for/wait_for_cell/wait_for_probe step"
    )]
    TimeoutOnNonWaitingStep { index: usize },
    /// A `send` step's key text contains a `<...>` token this translator
    /// does not know how to turn into real keypress bytes -- caught here,
    /// at load time, rather than first discovered mid-run: see
    /// [`resolve_send_keys`]'s own doc comment for which tokens are
    /// recognized.
    #[error("step {index}: {source}")]
    UnsupportedKeyNotation {
        index: usize,
        #[source]
        source: CompatError,
    },
}

/// Validates and converts one [`RawStep`] into a [`Step`], applying
/// [`DEFAULT_STEP_TIMEOUT_MS`] to a `wait_for`/`wait_for_cell`/`wait_for_probe`
/// step that omitted `timeout_ms`. `index` is only used to identify the
/// offending step in a returned [`ScenarioError`].
fn validate_step(raw: RawStep, index: usize) -> Result<Step, ScenarioError> {
    let action_count = [
        raw.send.is_some(),
        raw.wait_for.is_some(),
        raw.wait_for_cell.is_some(),
        raw.assert_absent.is_some(),
        raw.probe.is_some(),
        raw.wait_for_probe.is_some(),
    ]
    .into_iter()
    .filter(|set| *set)
    .count();
    if action_count != 1 {
        return Err(ScenarioError::AmbiguousStep {
            index,
            found: action_count,
        });
    }

    let is_waiting =
        raw.wait_for.is_some() || raw.wait_for_cell.is_some() || raw.wait_for_probe.is_some();
    if raw.timeout_ms.is_some() && !is_waiting {
        return Err(ScenarioError::TimeoutOnNonWaitingStep { index });
    }
    if raw.probe.is_none() && raw.wait_for_probe.is_none() && raw.expect.is_some() {
        return Err(ScenarioError::ExpectWithoutProbe { index });
    }

    let timeout = Duration::from_millis(raw.timeout_ms.unwrap_or(DEFAULT_STEP_TIMEOUT_MS));

    if let Some(keys) = raw.send {
        resolve_send_keys(&keys)
            .map_err(|source| ScenarioError::UnsupportedKeyNotation { index, source })?;
        return Ok(Step::Send(keys));
    }
    if let Some(needle) = raw.wait_for {
        return Ok(Step::WaitFor { needle, timeout });
    }
    if let Some(cell) = raw.wait_for_cell {
        return Ok(Step::WaitForCell {
            row: cell.row,
            col: cell.col,
            expected: cell.expected,
            timeout,
        });
    }
    if let Some(needle) = raw.assert_absent {
        return Ok(Step::AssertAbsent(needle));
    }
    if let Some(expr) = raw.probe {
        let Some(expect) = raw.expect else {
            return Err(ScenarioError::ProbeMissingExpect { index });
        };
        return Ok(Step::Probe { expr, expect });
    }
    if let Some(expr) = raw.wait_for_probe {
        let Some(expect) = raw.expect else {
            return Err(ScenarioError::ProbeMissingExpect { index });
        };
        return Ok(Step::WaitForProbe {
            expr,
            expect,
            timeout,
        });
    }
    // action_count == 1 already ruled out every other combination, so
    // exactly one of the six arms above always matches; unreachable is not
    // available in lib code (workspace lints deny it), but this arm can
    // never actually run since the six `is_some()` checks above are the
    // same six options this if-chain now walks in the same order.
    Err(ScenarioError::AmbiguousStep { index, found: 0 })
}

fn validate_class(raw: &str) -> Result<PluginClass, ScenarioError> {
    match raw {
        "semantic" => Ok(PluginClass::Semantic),
        "ui-adjacent" => Ok(PluginClass::UiAdjacent),
        "ui-owning" => Ok(PluginClass::UiOwning),
        other => Err(ScenarioError::UnknownClass(other.to_string())),
    }
}

fn validate_state(raw: &str) -> Result<ScenarioState, ScenarioError> {
    match raw {
        "present" => Ok(ScenarioState::Present),
        other => Err(ScenarioError::UnsupportedState(other.to_string())),
    }
}

/// Parses and validates one scenario from its raw TOML text.
///
/// # Errors
///
/// Returns [`ScenarioError::Toml`] on malformed TOML or an unrecognized
/// field, [`ScenarioError::UnsupportedSchema`] if `schema` is not
/// [`SUPPORTED_SCHEMA`], [`ScenarioError::UnknownClass`]/[`ScenarioError::UnsupportedState`]
/// if `class`/`state` do not name a recognized value, or any of the
/// per-step errors [`validate_step`] can raise.
pub fn parse(raw_toml: &str) -> Result<ScenarioFile, ScenarioError> {
    let raw: RawScenario = toml::from_str(raw_toml)?;
    if raw.schema != SUPPORTED_SCHEMA {
        return Err(ScenarioError::UnsupportedSchema(raw.schema));
    }
    let class = validate_class(&raw.class)?;
    let state = validate_state(&raw.state)?;
    let steps = raw
        .steps
        .into_iter()
        .enumerate()
        .map(|(index, step)| validate_step(step, index))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(ScenarioFile {
        plugin: raw.plugin,
        class,
        fixture: raw.fixture,
        state,
        cold_bootstrap: raw.cold_bootstrap,
        steps,
    })
}

/// Reads `path` from disk and [`parse`]s it.
///
/// # Errors
///
/// Returns [`ScenarioError::Io`] if `path` cannot be read, or any error
/// [`parse`] returns.
pub fn load_file(path: &Path) -> Result<ScenarioFile, ScenarioError> {
    let raw = std::fs::read_to_string(path).map_err(|source| ScenarioError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse(&raw)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const VALID: &str = r#"
schema = 1
plugin = "lualine"
class = "ui-owning"
fixture = "heavy"
state = "present"
steps = [
  { send = "ihello<Esc>" },
  { wait_for = "hello", timeout_ms = 5000 },
  { assert_absent = "E5108" },
  { probe = "luaeval('lualine ~= nil')", expect = "true" },
]
"#;

    #[test]
    fn valid_scenario_parses() {
        let scenario = parse(VALID).expect("VALID must parse as a scenario");
        assert_eq!(scenario.plugin, "lualine");
        assert_eq!(scenario.class, PluginClass::UiOwning);
        assert_eq!(scenario.fixture.as_deref(), Some("heavy"));
        assert_eq!(scenario.state, ScenarioState::Present);
        assert!(!scenario.cold_bootstrap);
        assert_eq!(scenario.steps.len(), 4);
        assert_eq!(scenario.steps[0], Step::Send("ihello<Esc>".to_string()));
        assert_eq!(
            scenario.steps[1],
            Step::WaitFor {
                needle: "hello".to_string(),
                timeout: Duration::from_millis(5000),
            }
        );
        assert_eq!(scenario.steps[2], Step::AssertAbsent("E5108".to_string()));
        assert_eq!(
            scenario.steps[3],
            Step::Probe {
                expr: "luaeval('lualine ~= nil')".to_string(),
                expect: "true".to_string(),
            }
        );
    }

    #[test]
    fn fixture_is_optional_for_a_fixture_less_scenario() {
        let toml = VALID.replace("fixture = \"heavy\"\n", "");
        let scenario = parse(&toml).expect("a scenario with no fixture field must parse");
        assert_eq!(scenario.fixture, None);
    }

    #[test]
    fn wait_for_cell_step_parses() {
        let toml = VALID.replace(
            "{ assert_absent = \"E5108\" },",
            "{ wait_for_cell = { row = 23, col = 0, expected = \":\" }, timeout_ms = 1000 },",
        );
        let scenario = parse(&toml).expect("wait_for_cell step must parse");
        assert_eq!(
            scenario.steps[2],
            Step::WaitForCell {
                row: 23,
                col: 0,
                expected: ":".to_string(),
                timeout: Duration::from_millis(1000),
            }
        );
    }

    #[test]
    fn wait_for_step_without_timeout_ms_gets_the_default() {
        let toml = VALID.replace(", timeout_ms = 5000", "");
        let scenario = parse(&toml).expect("must parse without an explicit timeout_ms");
        assert_eq!(
            scenario.steps[1],
            Step::WaitFor {
                needle: "hello".to_string(),
                timeout: Duration::from_millis(DEFAULT_STEP_TIMEOUT_MS),
            }
        );
    }

    #[test]
    fn unknown_field_is_rejected_not_ignored() {
        let toml = format!("{VALID}\nbogus_field = \"nope\"\n");
        let err = parse(&toml).expect_err("an unknown field must be a hard error");
        assert!(
            matches!(err, ScenarioError::Toml(_)),
            "expected a Toml error for an unknown field, got {err:?}"
        );
    }

    #[test]
    fn unknown_schema_is_rejected() {
        let toml = VALID.replace("schema = 1", "schema = 2");
        let err = parse(&toml).expect_err("an unrecognized schema must be a hard error");
        assert!(
            matches!(err, ScenarioError::UnsupportedSchema(2)),
            "expected UnsupportedSchema(2), got {err:?}"
        );
    }

    #[test]
    fn unknown_class_is_rejected() {
        let toml = VALID.replace(r#"class = "ui-owning""#, r#"class = "bogus""#);
        let err = parse(&toml).expect_err("an unrecognized class must be a hard error");
        assert!(
            matches!(err, ScenarioError::UnknownClass(ref s) if s == "bogus"),
            "expected UnknownClass(\"bogus\"), got {err:?}"
        );
    }

    #[test]
    fn unknown_state_is_rejected() {
        let toml = VALID.replace(r#"state = "present""#, r#"state = "superseded""#);
        let err = parse(&toml).expect_err("an unrecognized state must be a hard error");
        assert!(
            matches!(err, ScenarioError::UnsupportedState(ref s) if s == "superseded"),
            "expected UnsupportedState(\"superseded\"), got {err:?}"
        );
    }

    #[test]
    fn a_step_with_two_action_fields_is_rejected() {
        let toml = VALID.replace(
            "{ assert_absent = \"E5108\" },",
            "{ assert_absent = \"E5108\", wait_for = \"x\" },",
        );
        let err = parse(&toml).expect_err("a step with two action fields must be a hard error");
        assert!(
            matches!(err, ScenarioError::AmbiguousStep { found: 2, .. }),
            "expected AmbiguousStep{{found: 2}}, got {err:?}"
        );
    }

    #[test]
    fn a_step_with_no_action_field_is_rejected() {
        let toml = VALID.replace("{ assert_absent = \"E5108\" },", "{ timeout_ms = 100 },");
        let err = parse(&toml).expect_err("a step with no action field must be a hard error");
        assert!(
            matches!(err, ScenarioError::TimeoutOnNonWaitingStep { .. })
                || matches!(err, ScenarioError::AmbiguousStep { found: 0, .. }),
            "expected an ambiguous/no-action step error, got {err:?}"
        );
    }

    #[test]
    fn a_probe_step_without_expect_is_rejected() {
        let toml = VALID.replace(
            "{ probe = \"luaeval('lualine ~= nil')\", expect = \"true\" },",
            "{ probe = \"luaeval('lualine ~= nil')\" },",
        );
        let err = parse(&toml).expect_err("a probe step without expect must be a hard error");
        assert!(
            matches!(err, ScenarioError::ProbeMissingExpect { .. }),
            "expected ProbeMissingExpect, got {err:?}"
        );
    }

    #[test]
    fn expect_without_probe_is_rejected() {
        let toml = VALID.replace(
            "{ assert_absent = \"E5108\" },",
            "{ assert_absent = \"E5108\", expect = \"true\" },",
        );
        let err = parse(&toml).expect_err("expect without probe must be a hard error");
        assert!(
            matches!(err, ScenarioError::ExpectWithoutProbe { .. })
                || matches!(err, ScenarioError::AmbiguousStep { .. }),
            "expected ExpectWithoutProbe or AmbiguousStep, got {err:?}"
        );
    }

    #[test]
    fn timeout_ms_on_a_send_step_is_rejected() {
        let toml = VALID.replace(
            "{ send = \"ihello<Esc>\" },",
            "{ send = \"ihello<Esc>\", timeout_ms = 100 },",
        );
        let err = parse(&toml).expect_err("timeout_ms on a send step must be a hard error");
        assert!(
            matches!(err, ScenarioError::TimeoutOnNonWaitingStep { index: 0 }),
            "expected TimeoutOnNonWaitingStep{{index: 0}}, got {err:?}"
        );
    }

    #[test]
    fn a_send_step_with_supported_key_notation_parses() {
        let toml = VALID.replace(
            "{ send = \"ihello<Esc>\" },",
            "{ send = \"ihello<Esc><C-w>\" },",
        );
        let scenario = parse(&toml).expect("a send step using only supported notation must parse");
        assert_eq!(
            scenario.steps[0],
            Step::Send("ihello<Esc><C-w>".to_string())
        );
    }

    #[test]
    fn a_send_step_with_unsupported_key_notation_is_rejected_at_load_time() {
        let toml = VALID.replace("{ send = \"ihello<Esc>\" },", "{ send = \"<C-Up>\" },");
        let err = parse(&toml).expect_err(
            "a send step whose key text contains a notation-shaped but \
             untranslatable token must be a hard error at load time, not \
             a silent literal-text scenario that only fails at drive time",
        );
        assert!(
            matches!(err, ScenarioError::UnsupportedKeyNotation { index: 0, .. }),
            "expected UnsupportedKeyNotation{{index: 0}}, got {err:?}"
        );
    }

    #[test]
    fn missing_required_field_is_rejected() {
        let toml = r#"
schema = 1
class = "semantic"
state = "present"
steps = []
"#;
        let err = parse(toml).expect_err("a missing required field (plugin) must be a hard error");
        assert!(
            matches!(err, ScenarioError::Toml(_)),
            "expected a Toml error for a missing required field, got {err:?}"
        );
    }
}
