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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;
use view_oracle::compat::{
    parse_state_name, resolve_send_keys, state_name, CompatError, PluginClass, ScenarioState,
    SendConfirm, Step,
};

/// The four state names a [`PluginClass::UiOwning`] scenario must declare
/// between them (order fixed for a stable, reviewable error message; not a
/// requirement on the file's own `[[states]]` ordering).
///
/// `"unaccommodated"` is the load-bearing one: the other three all run a
/// configuration already adapted to the engine, so a suite carrying only
/// them cannot fail on a migration defect at all. A fixture accommodation
/// with no paired unaccommodated state is a suppressed defect, and this
/// array is what makes the pairing a load-time failure rather than a
/// reviewer's habit.
const REQUIRED_UI_OWNING_STATES: [&str; 4] =
    ["unaccommodated", "superseded", "deferred", "native-only"];

/// The one scenario file stem `cold_bootstrap = true` is honored for. Tying
/// the exemption to a filename rather than trusting the flag on its face
/// closes the loophole a bare boolean leaves open: any `ui-owning` scenario
/// could otherwise set `cold_bootstrap = true` to dodge
/// [`REQUIRED_UI_OWNING_STATES`] coverage with no structural cost. A
/// filesystem directory cannot hold two entries with the same stem, so at
/// most one file can ever claim this exemption, and claiming it means
/// replacing `compat/scenarios/cold-bootstrap.toml` itself -- an
/// unmistakably reviewable act, not a quiet flag flip.
const COLD_BOOTSTRAP_STEM: &str = "cold-bootstrap";

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
    #[serde(default)]
    cold_bootstrap: bool,
    states: Vec<RawState>,
}

/// One `[[states]]` entry's wire shape: the reconciliation state's own name
/// (`"present"` / `"unaccommodated"` / `"superseded"` / `"deferred"` /
/// `"native-only"`), an optional `[native]` override, an optional per-state
/// fixture override (a `native-only` state commonly swaps to a plugin-free
/// fixture), whether this state wants the fixture's accommodations, and this
/// state's own steps.
///
/// `accommodations` is a field rather than a property read off the state's
/// name because keying it on `name == "unaccommodated"` would leave exactly
/// one state per scenario able to prove anything about an unadjusted config
/// -- and `deferred`, the state that carries the remedy a conflict notice
/// points a user at, is the one that most needs to prove the remedy works on
/// a config nobody pre-adjusted.
///
/// `native` is `Option` rather than a plain, default-empty map because the
/// two absent cases mean different things and the runner needs to tell
/// them apart: a `present` state that omits `native` entirely leaves its
/// fixture's own committed `view/view.toml` untouched (see
/// `bin/oracle.rs`'s `run_scenario`), while every other state that omits it
/// still materializes the empty-table default -- a bare `[native]` header,
/// matching `NativeConfig`'s own "absent table means every feature stays
/// on" default, exactly as it always has for `superseded`/`native-only`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawState {
    name: String,
    native: Option<BTreeMap<String, bool>>,
    fixture: Option<String>,
    #[serde(default = "accommodations_default")]
    accommodations: bool,
    steps: Vec<RawStep>,
}

/// `true`: every state that says nothing keeps the fixture's accommodations,
/// so the states that predate the field are unchanged by its arrival.
fn accommodations_default() -> bool {
    true
}

/// One step's wire shape. Exactly one of `send` / `wait_for` /
/// `wait_for_cell` / `assert_absent` / `assert_cell_not` / `probe` must be
/// set -- [`validate_step`] enforces that, since `serde`'s own
/// `deny_unknown_fields` only catches a field name it has never heard of,
/// not a legal field used on the wrong variant (e.g. `expect` with no
/// `probe`, or two action fields at once). `confirm_probe`/`confirm_expect`
/// are `send`'s own pair, named apart from `probe`/`expect` so a step can
/// never be ambiguous between "this step is a probe" and "this send step
/// also carries a confirmation probe".
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStep {
    send: Option<String>,
    wait_for: Option<String>,
    wait_for_cell: Option<RawCellTarget>,
    assert_absent: Option<String>,
    assert_cell_not: Option<RawCellNotTarget>,
    probe: Option<String>,
    wait_for_probe: Option<String>,
    expect: Option<String>,
    timeout_ms: Option<u64>,
    confirm_probe: Option<String>,
    confirm_expect: Option<String>,
}

/// `wait_for_cell`'s inline-table shape: `{ row = 23, col = 0, expected = ":" }`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCellTarget {
    row: u16,
    col: u16,
    expected: String,
}

/// `assert_cell_not`'s inline-table shape: `{ row = 29, col = 85, glyph = "" }`.
/// A distinct shape from [`RawCellTarget`] (`glyph` rather than `expected`)
/// so a step reads as the negative assertion it is rather than a
/// same-named field silently meaning "must equal" in one step and "must not
/// equal" in the other.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCellNotTarget {
    row: u16,
    col: u16,
    glyph: String,
}

/// One validated, defaults-applied scenario, ready for the `compat`
/// subcommand to drive: the bookkeeping fields (`plugin`, `fixture`,
/// `cold_bootstrap`) the runner's own fixture resolution and report line
/// need, plus every state this scenario declares.
#[derive(Debug, Clone)]
pub struct ScenarioFile {
    pub plugin: String,
    pub class: PluginClass,
    /// Names a subdirectory of `compat/fixtures/`, or `None` for a
    /// fixture-less scenario (the maintainer's `$VIEW_DAILY_CONFIG`, whose
    /// `init.lua` the harness does not own and so cannot rely on carrying
    /// its own `serverstart` call -- see `CompatSession::prime_probe_channel`).
    /// A state's own [`ScenarioStateEntry::fixture`] overrides this default
    /// when set.
    pub fixture: Option<String>,
    /// Forces the heavy fixture's plugin cache to a run-unique, guaranteed-
    /// empty key instead of its normal lockfile-hash key, so this scenario
    /// always drives a full network bootstrap rather than ever reusing a
    /// prior warm cache. `false` for every ordinary scenario.
    pub cold_bootstrap: bool,
    pub states: Vec<ScenarioStateEntry>,
}

/// One validated `[[states]]` entry: everything [`view_oracle::compat::CompatSession`]
/// needs to drive this state (`name`/`steps` already `view-oracle` types)
/// plus the `native`/`fixture` overrides the runner materializes into a
/// hermetic `view.toml` and spawns `view --config` against. See
/// [`RawState`]'s doc comment for what `native: None` means for a
/// `present` state versus every other one.
#[derive(Debug, Clone)]
pub struct ScenarioStateEntry {
    pub name: ScenarioState,
    pub native: Option<BTreeMap<String, bool>>,
    pub fixture: Option<String>,
    /// Whether this state runs the fixture's accommodations (`true` unless
    /// the file says otherwise). See [`RawState`] for why this is a field
    /// and not a property of the state's name.
    pub accommodations: bool,
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
    /// A `[[states]]` entry's `name` did not match one of the five
    /// recognized reconciliation states -- see [`parse_state_name`].
    #[error("unsupported state {0:?} (expected one of \"present\", \"unaccommodated\", \"superseded\", \"deferred\", \"native-only\")")]
    UnsupportedState(String),
    /// A scenario declared zero `[[states]]` entries: there is nothing for
    /// the runner to drive.
    #[error("scenario declares no [[states]] entries")]
    NoStates,
    /// Two `[[states]]` entries in the same file named the same state,
    /// leaving the runner unable to tell which one a report line describes.
    #[error("state {name:?} is declared more than once")]
    DuplicateStateName { name: String },
    /// A `ui-owning` scenario (one whose plugin the engine can supersede,
    /// and that is not exempted by `cold_bootstrap`) declared fewer than
    /// the four states [`REQUIRED_UI_OWNING_STATES`] names: a ui-owning
    /// plugin's coverage is incomplete until
    /// unaccommodated/superseded/deferred/native-only are all asserted.
    #[error(
        "ui-owning scenario is missing required states: {missing:?} \
         (needs unaccommodated, superseded, deferred, native-only)"
    )]
    IncompleteUiOwningStates { missing: Vec<String> },
    /// `cold_bootstrap = true` was set on a scenario file whose stem is not
    /// [`COLD_BOOTSTRAP_STEM`]: the exemption from the three-state
    /// completeness gate is reserved for that one file, so no other
    /// `ui-owning` scenario can self-exempt.
    #[error(
        "cold_bootstrap = true is reserved for the scenario file named \
         \"cold-bootstrap.toml\"; rename this file to opt in, or drop \
         cold_bootstrap and declare the required ui-owning states instead"
    )]
    UnauthorizedColdBootstrap,
    /// A step set zero, or more than one, of its mutually exclusive action
    /// fields (`send` / `wait_for` / `wait_for_cell` / `assert_absent` /
    /// `assert_cell_not` / `probe` / `wait_for_probe`).
    #[error("step {index} must set exactly one of send/wait_for/wait_for_cell/assert_absent/assert_cell_not/probe/wait_for_probe, found {found}")]
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
    /// `confirm_probe`/`confirm_expect` was set on a step whose action is
    /// not `send` -- they exist only to pair with a `send` step's own
    /// delivery confirmation.
    #[error("step {index} sets confirm_probe/confirm_expect but is not a send step")]
    ConfirmWithoutSend { index: usize },
    /// A `send` step set `confirm_probe` without its required
    /// `confirm_expect` partner, or vice versa -- both name one
    /// confirmation together and neither means anything alone.
    #[error("step {index} sets confirm_probe/confirm_expect but not both")]
    ConfirmMissingPartner { index: usize },
    /// A `send` step's key text opens with an invocation of nvim's
    /// `:silent` ex command (see [`starts_with_silent_command`] for exactly
    /// which spellings that covers), the codebase's own idiom for a send
    /// that produces no screen delta, but sets no `confirm_probe`:
    /// unconfirmed, `Step::Send`'s screen-based wait can never observe this
    /// step's effect, so it would burn its own full deadline on every run
    /// with nothing in the scenario file explaining why. Add
    /// `confirm_probe`/`confirm_expect`, or drop `:silent` so the command
    /// produces a visible delta the default path can confirm.
    #[error(
        "step {index} sends a :silent[!] command (full spelling or nvim's own \
         sil/sile/silen abbreviations) with no confirm_probe; add \
         confirm_probe/confirm_expect or drop :silent"
    )]
    SilentSendWithoutConfirm { index: usize },
}

/// True if `keys` opens with an invocation of nvim's `:silent` ex command --
/// the idiom [`ScenarioError::SilentSendWithoutConfirm`] exists to catch.
/// Recognizes the full spelling and nvim's own shortest-to-longest
/// abbreviations (`sil`, `sile`, `silen`, `silent`), each with or without
/// the `!` bang variant, with any amount of whitespace after the leading
/// `:` (nvim's own command-line parser skips it there). Confirmed live
/// against the pinned nvim (`getcompletion("sil", "command")` returns
/// exactly `["silent"]`; `getcompletion("si", "command")` also matches
/// `sign`/`simalt`) -- `sil` is nvim's own shortest unambiguous
/// abbreviation for this command, so no shorter prefix belongs in this set:
/// a scenario typing `:si` would not actually invoke `:silent` either, it
/// would hit nvim's own ambiguous-command error.
///
/// Deliberately does not scan for `:silent` anywhere inside `keys`, only at
/// its very start: a `Send` step's text before any leading `:` is real
/// keystrokes (e.g. a literal insertion of the word "silent"), not command
/// syntax, and matching them as this idiom would be a false positive this
/// function must not produce.
fn starts_with_silent_command(keys: &str) -> bool {
    let Some(after_colon) = keys.strip_prefix(':') else {
        return false;
    };
    let after_colon = after_colon.trim_start();
    ["silent", "silen", "sile", "sil"].iter().any(|form| {
        after_colon.strip_prefix(form).is_some_and(|rest| {
            rest.is_empty() || rest.starts_with('!') || rest.starts_with(char::is_whitespace)
        })
    })
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
        raw.assert_cell_not.is_some(),
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
    if raw.send.is_none() && (raw.confirm_probe.is_some() || raw.confirm_expect.is_some()) {
        return Err(ScenarioError::ConfirmWithoutSend { index });
    }
    if raw.confirm_probe.is_some() != raw.confirm_expect.is_some() {
        return Err(ScenarioError::ConfirmMissingPartner { index });
    }

    let timeout = Duration::from_millis(raw.timeout_ms.unwrap_or(DEFAULT_STEP_TIMEOUT_MS));

    if let Some(keys) = raw.send {
        resolve_send_keys(&keys)
            .map_err(|source| ScenarioError::UnsupportedKeyNotation { index, source })?;
        let confirm = match (raw.confirm_probe, raw.confirm_expect) {
            (Some(expr), Some(expect)) => Some(SendConfirm { expr, expect }),
            _ => None,
        };
        if confirm.is_none() && starts_with_silent_command(&keys) {
            return Err(ScenarioError::SilentSendWithoutConfirm { index });
        }
        return Ok(Step::Send { keys, confirm });
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
    if let Some(cell) = raw.assert_cell_not {
        return Ok(Step::AssertCellNot {
            row: cell.row,
            col: cell.col,
            glyph: cell.glyph,
        });
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
    // exactly one of the seven arms above always matches; unreachable is not
    // available in lib code (workspace lints deny it), but this arm can
    // never actually run since the seven `is_some()` checks above are the
    // same seven options this if-chain now walks in the same order.
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
    parse_state_name(raw).ok_or_else(|| ScenarioError::UnsupportedState(raw.to_string()))
}

/// Validates one `[[states]]` entry's raw shape into its typed form,
/// including every step it carries.
fn validate_state_entry(raw: RawState) -> Result<ScenarioStateEntry, ScenarioError> {
    let name = validate_state(&raw.name)?;
    let steps = raw
        .steps
        .into_iter()
        .enumerate()
        .map(|(index, step)| validate_step(step, index))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ScenarioStateEntry {
        name,
        native: raw.native,
        fixture: raw.fixture,
        accommodations: raw.accommodations,
        steps,
    })
}

/// Rejects `cold_bootstrap = true` on any file but [`COLD_BOOTSTRAP_STEM`].
/// `source_stem` is `None` for [`parse`]'s raw-string entry point, which
/// carries no filename at all -- so a scenario parsed directly from a
/// string literal (every unit test in this module, and any future caller
/// with no file on disk) can never claim the exemption either, the same
/// strict default an unrecognized stem gets.
fn check_cold_bootstrap_authorization(
    cold_bootstrap: bool,
    source_stem: Option<&str>,
) -> Result<(), ScenarioError> {
    if cold_bootstrap && source_stem != Some(COLD_BOOTSTRAP_STEM) {
        return Err(ScenarioError::UnauthorizedColdBootstrap);
    }
    Ok(())
}

/// Checks the scenario-wide invariants over the full `states` list that no
/// single entry's own validation can see: at least one state exists, no two
/// states share a name, and a `ui-owning` scenario (unless exempted by
/// `cold_bootstrap`, whose network-bound bootstrap cost is already paid once
/// and should not triple for orthogonal supersession-state coverage)
/// declares all four of [`REQUIRED_UI_OWNING_STATES`]. Callers must run
/// [`check_cold_bootstrap_authorization`] first: this function trusts
/// `cold_bootstrap` at face value, the same way it always has.
fn validate_state_completeness(
    class: PluginClass,
    cold_bootstrap: bool,
    states: &[ScenarioStateEntry],
) -> Result<(), ScenarioError> {
    if states.is_empty() {
        return Err(ScenarioError::NoStates);
    }
    let mut seen: BTreeSet<&'static str> = BTreeSet::new();
    for state in states {
        let name = state_name(state.name);
        if !seen.insert(name) {
            return Err(ScenarioError::DuplicateStateName {
                name: name.to_string(),
            });
        }
    }
    if class == PluginClass::UiOwning && !cold_bootstrap {
        let missing: Vec<String> = REQUIRED_UI_OWNING_STATES
            .iter()
            .filter(|required| !seen.contains(*required))
            .map(|s| (*s).to_string())
            .collect();
        if !missing.is_empty() {
            return Err(ScenarioError::IncompleteUiOwningStates { missing });
        }
    }
    Ok(())
}

/// Parses and validates one scenario from its raw TOML text, `source_stem`
/// being the filename stem [`check_cold_bootstrap_authorization`] checks a
/// `cold_bootstrap = true` flag against.
fn parse_from(raw_toml: &str, source_stem: Option<&str>) -> Result<ScenarioFile, ScenarioError> {
    let raw: RawScenario = toml::from_str(raw_toml)?;
    if raw.schema != SUPPORTED_SCHEMA {
        return Err(ScenarioError::UnsupportedSchema(raw.schema));
    }
    let class = validate_class(&raw.class)?;
    let states = raw
        .states
        .into_iter()
        .map(validate_state_entry)
        .collect::<Result<Vec<_>, _>>()?;
    check_cold_bootstrap_authorization(raw.cold_bootstrap, source_stem)?;
    validate_state_completeness(class, raw.cold_bootstrap, &states)?;

    Ok(ScenarioFile {
        plugin: raw.plugin,
        class,
        fixture: raw.fixture,
        cold_bootstrap: raw.cold_bootstrap,
        states,
    })
}

/// Parses and validates one scenario from its raw TOML text.
///
/// Carries no filename, so `cold_bootstrap = true` is always rejected here
/// -- see [`check_cold_bootstrap_authorization`]. A scenario that
/// legitimately needs the exemption is only ever read through
/// [`load_file`].
///
/// # Errors
///
/// Returns [`ScenarioError::Toml`] on malformed TOML or an unrecognized
/// field, [`ScenarioError::UnsupportedSchema`] if `schema` is not
/// [`SUPPORTED_SCHEMA`], [`ScenarioError::UnknownClass`]/[`ScenarioError::UnsupportedState`]
/// if `class`/a state's `name` do not name a recognized value,
/// [`ScenarioError::NoStates`]/[`ScenarioError::DuplicateStateName`]/[`ScenarioError::IncompleteUiOwningStates`]/[`ScenarioError::UnauthorizedColdBootstrap`]
/// if the `states` list itself is invalid, or any of the per-step errors
/// [`validate_step`] can raise.
pub fn parse(raw_toml: &str) -> Result<ScenarioFile, ScenarioError> {
    parse_from(raw_toml, None)
}

/// Reads `path` from disk and parses it, the same as [`parse`] but with
/// `path`'s own file stem available to authorize `cold_bootstrap = true`
/// (see [`check_cold_bootstrap_authorization`]).
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
    let stem = path.file_stem().and_then(|s| s.to_str());
    parse_from(&raw, stem)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    const VALID: &str = r#"
schema = 1
plugin = "lualine"
class = "semantic"
fixture = "heavy"

[[states]]
name = "present"
steps = [
  { send = "ihello<Esc>" },
  { wait_for = "hello", timeout_ms = 5000 },
  { assert_absent = "E5108" },
  { probe = "luaeval('lualine ~= nil')", expect = "true" },
]
"#;

    /// A minimal `ui-owning` scenario declaring all four required states,
    /// for the completeness-enforcement tests below.
    const VALID_UI_OWNING: &str = r#"
schema = 1
plugin = "lualine"
class = "ui-owning"

[[states]]
name = "unaccommodated"
accommodations = false
native = {}
steps = [ { wait_for_cell = { row = 29, col = 1, expected = "N" } } ]

[[states]]
name = "superseded"
native = {}
steps = [ { wait_for_cell = { row = 29, col = 1, expected = "N" } } ]

[[states]]
name = "deferred"
native = { statusline = false }
steps = [ { wait_for_cell = { row = 29, col = 85, expected = "" } } ]

[[states]]
name = "native-only"
fixture = "minimal"
steps = []
"#;

    #[test]
    fn valid_scenario_parses() {
        let scenario = parse(VALID).expect("VALID must parse as a scenario");
        assert_eq!(scenario.plugin, "lualine");
        assert_eq!(scenario.class, PluginClass::Semantic);
        assert_eq!(scenario.fixture.as_deref(), Some("heavy"));
        assert!(!scenario.cold_bootstrap);
        assert_eq!(scenario.states.len(), 1);
        assert_eq!(scenario.states[0].name, ScenarioState::Present);
        assert_eq!(scenario.states[0].steps.len(), 4);
        assert_eq!(
            scenario.states[0].steps[0],
            Step::Send {
                keys: "ihello<Esc>".to_string(),
                confirm: None,
            }
        );
        assert_eq!(
            scenario.states[0].steps[1],
            Step::WaitFor {
                needle: "hello".to_string(),
                timeout: Duration::from_millis(5000),
            }
        );
        assert_eq!(
            scenario.states[0].steps[2],
            Step::AssertAbsent("E5108".to_string())
        );
        assert_eq!(
            scenario.states[0].steps[3],
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
            scenario.states[0].steps[2],
            Step::WaitForCell {
                row: 23,
                col: 0,
                expected: ":".to_string(),
                timeout: Duration::from_millis(1000),
            }
        );
    }

    #[test]
    fn assert_cell_not_step_parses() {
        let toml = VALID.replace(
            "{ assert_absent = \"E5108\" },",
            "{ assert_cell_not = { row = 29, col = 85, glyph = \"\" } },",
        );
        let scenario = parse(&toml).expect("assert_cell_not step must parse");
        assert_eq!(
            scenario.states[0].steps[2],
            Step::AssertCellNot {
                row: 29,
                col: 85,
                glyph: String::new(),
            }
        );
    }

    #[test]
    fn wait_for_step_without_timeout_ms_gets_the_default() {
        let toml = VALID.replace(", timeout_ms = 5000", "");
        let scenario = parse(&toml).expect("must parse without an explicit timeout_ms");
        assert_eq!(
            scenario.states[0].steps[1],
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
        let toml = VALID.replace(r#"class = "semantic""#, r#"class = "bogus""#);
        let err = parse(&toml).expect_err("an unrecognized class must be a hard error");
        assert!(
            matches!(err, ScenarioError::UnknownClass(ref s) if s == "bogus"),
            "expected UnknownClass(\"bogus\"), got {err:?}"
        );
    }

    #[test]
    fn unknown_state_is_rejected() {
        let toml = VALID.replace(r#"name = "present""#, r#"name = "nonexistent""#);
        let err = parse(&toml).expect_err("an unrecognized state name must be a hard error");
        assert!(
            matches!(err, ScenarioError::UnsupportedState(ref s) if s == "nonexistent"),
            "expected UnsupportedState(\"nonexistent\"), got {err:?}"
        );
    }

    #[test]
    fn a_scenario_with_no_states_is_rejected() {
        let toml = r#"
schema = 1
plugin = "lualine"
class = "semantic"
states = []
"#;
        let err =
            parse(toml).expect_err("a scenario with no [[states]] entries must be a hard error");
        assert!(
            matches!(err, ScenarioError::NoStates),
            "expected NoStates, got {err:?}"
        );
    }

    #[test]
    fn two_states_sharing_a_name_are_rejected() {
        let toml = format!(
            "{}\n[[states]]\nname = \"present\"\nsteps = []\n",
            VALID.trim_end()
        );
        let err = parse(&toml).expect_err("two states sharing a name must be a hard error");
        assert!(
            matches!(err, ScenarioError::DuplicateStateName { ref name } if name == "present"),
            "expected DuplicateStateName{{\"present\"}}, got {err:?}"
        );
    }

    #[test]
    fn a_ui_owning_scenario_with_all_four_required_states_parses() {
        let scenario =
            parse(VALID_UI_OWNING).expect("a ui-owning scenario with 4 states must parse");
        assert_eq!(scenario.states.len(), 4);
    }

    #[test]
    fn a_ui_owning_scenario_missing_a_required_state_is_rejected() {
        // Drop the "native-only" state, leaving the other three -- the
        // completeness check this whole schema exists to enforce.
        let (three_state_toml, _) = VALID_UI_OWNING
            .split_once("\n[[states]]\nname = \"native-only\"")
            .expect("VALID_UI_OWNING must contain a native-only state to split off");
        let err = parse(three_state_toml)
            .expect_err("a ui-owning scenario declaring fewer than 4 states must be a hard error");
        assert!(
            matches!(
                err,
                ScenarioError::IncompleteUiOwningStates { ref missing }
                    if missing == &vec!["native-only".to_string()]
            ),
            "expected IncompleteUiOwningStates{{[\"native-only\"]}}, got {err:?}"
        );
    }

    #[test]
    fn a_ui_owning_scenario_missing_its_unaccommodated_state_is_rejected() {
        // The one state that can fail on a migration defect: without this
        // arm the loader would accept the three adjusted-config states as
        // complete coverage, which is the finding this schema closes.
        let (head, rest) = VALID_UI_OWNING
            .split_once("[[states]]\nname = \"unaccommodated\"")
            .expect("VALID_UI_OWNING must contain an unaccommodated state to split off");
        let (_, tail) = rest
            .split_once("[[states]]")
            .expect("a further state must follow the unaccommodated one");
        let toml = format!("{head}[[states]]{tail}");
        let err = parse(&toml)
            .expect_err("a ui-owning scenario with no unaccommodated state must fail to load");
        assert!(
            matches!(
                err,
                ScenarioError::IncompleteUiOwningStates { ref missing }
                    if missing == &vec!["unaccommodated".to_string()]
            ),
            "expected IncompleteUiOwningStates{{[\"unaccommodated\"]}}, got {err:?}"
        );
    }

    #[test]
    fn a_state_that_says_nothing_about_accommodations_keeps_them() {
        let scenario = parse(VALID).expect("VALID must parse as a scenario");
        assert!(
            scenario.states[0].accommodations,
            "a state omitting the field must default to keeping the fixture's accommodations"
        );
    }

    #[test]
    fn a_state_can_decline_accommodations_under_any_name() {
        let scenario = parse(VALID_UI_OWNING).expect("VALID_UI_OWNING must parse");
        let by_name = |want: ScenarioState| {
            scenario
                .states
                .iter()
                .find(|state| state.name == want)
                .expect("VALID_UI_OWNING declares every required state")
        };
        assert!(!by_name(ScenarioState::Unaccommodated).accommodations);
        assert!(by_name(ScenarioState::Deferred).accommodations);

        let deferred_unadjusted = VALID_UI_OWNING.replace(
            "name = \"deferred\"\nnative = { statusline = false }",
            "name = \"deferred\"\naccommodations = false\nnative = { statusline = false }",
        );
        let scenario =
            parse(&deferred_unadjusted).expect("deferred may decline accommodations too");
        assert!(
            !scenario
                .states
                .iter()
                .find(|state| state.name == ScenarioState::Deferred)
                .expect("the deferred state must survive the rewrite")
                .accommodations
        );
    }

    /// Walks the real `compat/scenarios/` directory rather than a hand-built
    /// fixture: the rule has to hold for the files that actually run, and a
    /// scenario added later joins this walk by existing.
    #[test]
    fn every_ui_owning_scenario_declares_an_unaccommodated_state() {
        let dir = crate::fixture::workspace_root()
            .join("compat")
            .join("scenarios");
        let mut checked = 0_usize;
        for entry in std::fs::read_dir(&dir).expect("compat/scenarios must be readable") {
            let path = entry.expect("a readable directory entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let scenario = load_file(&path).unwrap_or_else(|err| {
                panic!("{} must load: {err}", path.display());
            });
            if scenario.class != PluginClass::UiOwning || scenario.cold_bootstrap {
                continue;
            }
            let state = scenario
                .states
                .iter()
                .find(|state| state.name == ScenarioState::Unaccommodated)
                .unwrap_or_else(|| {
                    panic!("{} declares no unaccommodated state", path.display());
                });
            assert!(
                !state.accommodations,
                "{}'s unaccommodated state must set accommodations = false",
                path.display()
            );
            // A state that opted out of the accommodations but turned a
            // native feature off would be the same suppression in a new
            // place: the defaults case is only honest under the defaults.
            if let Some(native) = &state.native {
                let disabled: Vec<&String> = native
                    .iter()
                    .filter(|(_, on)| !**on)
                    .map(|(key, _)| key)
                    .collect();
                assert!(
                    disabled.is_empty(),
                    "{}'s unaccommodated state turns {disabled:?} off; it must run view's own \
                     defaults",
                    path.display()
                );
            }
            checked += 1;
        }
        assert!(
            checked > 0,
            "no ui-owning scenario was reached in {} -- the walk found nothing to enforce",
            dir.display()
        );
    }

    #[test]
    fn a_ui_owning_scenario_cannot_self_exempt_via_cold_bootstrap() {
        // Drop the "native-only" state (as above) but also set
        // cold_bootstrap = true: without the stem check, this would dodge
        // IncompleteUiOwningStates entirely. parse()'s source_stem is
        // always None, so no file can claim the exemption through it.
        let (partial_toml, _) = VALID_UI_OWNING
            .split_once("\n[[states]]\nname = \"native-only\"")
            .expect("VALID_UI_OWNING must contain a native-only state to split off");
        // cold_bootstrap must precede the first [[states]] table -- TOML
        // attaches a bare top-level key to the nearest preceding
        // array-of-tables entry, not to the document root, if it trails one.
        let toml = partial_toml.replacen(
            "class = \"ui-owning\"",
            "class = \"ui-owning\"\ncold_bootstrap = true",
            1,
        );
        let err = parse(&toml).expect_err(
            "cold_bootstrap = true must not let a ui-owning scenario dodge the completeness gate \
             through parse(), which never carries a file stem",
        );
        assert!(
            matches!(err, ScenarioError::UnauthorizedColdBootstrap),
            "expected UnauthorizedColdBootstrap, got {err:?}"
        );
    }

    #[test]
    fn cold_bootstrap_is_authorized_only_for_the_cold_bootstrap_stem() {
        let (partial_toml, _) = VALID_UI_OWNING
            .split_once("\n[[states]]\nname = \"native-only\"")
            .expect("VALID_UI_OWNING must contain a native-only state to split off");
        // cold_bootstrap must precede the first [[states]] table -- TOML
        // attaches a bare top-level key to the nearest preceding
        // array-of-tables entry, not to the document root, if it trails one.
        let toml = partial_toml.replacen(
            "class = \"ui-owning\"",
            "class = \"ui-owning\"\ncold_bootstrap = true",
            1,
        );

        let err = parse_from(&toml, Some("daily-config"))
            .expect_err("an unrelated file stem must not authorize cold_bootstrap = true");
        assert!(
            matches!(err, ScenarioError::UnauthorizedColdBootstrap),
            "expected UnauthorizedColdBootstrap for stem \"daily-config\", got {err:?}"
        );

        let scenario = parse_from(&toml, Some(COLD_BOOTSTRAP_STEM)).expect(
            "the cold-bootstrap-named file must be authorized to set cold_bootstrap = true",
        );
        assert!(scenario.cold_bootstrap);
        assert_eq!(
            scenario.states.len(),
            3,
            "authorization must exempt from IncompleteUiOwningStates, not just from the flag check"
        );
    }

    #[test]
    fn a_semantic_scenario_with_one_state_is_not_held_to_the_ui_owning_completeness_bar() {
        // VALID itself is class = "semantic" with exactly one state; it must
        // not be rejected for lacking superseded/deferred/native-only.
        parse(VALID).expect("a semantic scenario needs only >= 1 state, not all 3 named ones");
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
            scenario.states[0].steps[0],
            Step::Send {
                keys: "ihello<Esc><C-w>".to_string(),
                confirm: None,
            }
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

[[states]]
name = "present"
steps = []
"#;
        let err = parse(toml).expect_err("a missing required field (plugin) must be a hard error");
        assert!(
            matches!(err, ScenarioError::Toml(_)),
            "expected a Toml error for a missing required field, got {err:?}"
        );
    }

    #[test]
    fn a_send_step_with_a_paired_confirm_probe_parses() {
        let toml = VALID.replace(
            "{ send = \"ihello<Esc>\" },",
            "{ send = \"ihello<Esc>\", confirm_probe = \"mode()\", confirm_expect = \"n\" },",
        );
        let scenario = parse(&toml).expect("a send step with a paired confirm probe must parse");
        assert_eq!(
            scenario.states[0].steps[0],
            Step::Send {
                keys: "ihello<Esc>".to_string(),
                confirm: Some(SendConfirm {
                    expr: "mode()".to_string(),
                    expect: "n".to_string(),
                }),
            }
        );
    }

    #[test]
    fn confirm_probe_without_confirm_expect_is_rejected() {
        let toml = VALID.replace(
            "{ send = \"ihello<Esc>\" },",
            "{ send = \"ihello<Esc>\", confirm_probe = \"mode()\" },",
        );
        let err = parse(&toml)
            .expect_err("confirm_probe with no confirm_expect partner must be a hard error");
        assert!(
            matches!(err, ScenarioError::ConfirmMissingPartner { index: 0 }),
            "expected ConfirmMissingPartner{{index: 0}}, got {err:?}"
        );
    }

    #[test]
    fn confirm_expect_without_confirm_probe_is_rejected() {
        let toml = VALID.replace(
            "{ send = \"ihello<Esc>\" },",
            "{ send = \"ihello<Esc>\", confirm_expect = \"n\" },",
        );
        let err = parse(&toml)
            .expect_err("confirm_expect with no confirm_probe partner must be a hard error");
        assert!(
            matches!(err, ScenarioError::ConfirmMissingPartner { index: 0 }),
            "expected ConfirmMissingPartner{{index: 0}}, got {err:?}"
        );
    }

    #[test]
    fn confirm_probe_on_a_non_send_step_is_rejected() {
        let toml = VALID.replace(
            "{ assert_absent = \"E5108\" },",
            "{ assert_absent = \"E5108\", confirm_probe = \"mode()\", confirm_expect = \"n\" },",
        );
        let err = parse(&toml).expect_err("confirm_probe on a non-send step must be a hard error");
        assert!(
            matches!(err, ScenarioError::ConfirmWithoutSend { index: 2 }),
            "expected ConfirmWithoutSend{{index: 2}}, got {err:?}"
        );
    }

    #[test]
    fn a_silent_send_with_no_confirm_probe_is_rejected() {
        let toml = VALID.replace(
            "{ send = \"ihello<Esc>\" },",
            "{ send = \":silent cd /tmp<CR>\" },",
        );
        let err = parse(&toml).expect_err(
            "a :silent send step with no confirm_probe must be a hard error, since its \
             screen-based delivery wait can never observe a :silent command's effect",
        );
        assert!(
            matches!(err, ScenarioError::SilentSendWithoutConfirm { index: 0 }),
            "expected SilentSendWithoutConfirm{{index: 0}}, got {err:?}"
        );
    }

    #[test]
    fn a_silent_send_with_a_confirm_probe_parses() {
        let toml = VALID.replace(
            "{ send = \"ihello<Esc>\" },",
            "{ send = \":silent cd /tmp<CR>\", confirm_probe = \"getcwd()\", confirm_expect = \"/tmp\" },",
        );
        let scenario = parse(&toml).expect("a :silent send step with a confirm_probe must parse");
        assert_eq!(
            scenario.states[0].steps[0],
            Step::Send {
                keys: ":silent cd /tmp<CR>".to_string(),
                confirm: Some(SendConfirm {
                    expr: "getcwd()".to_string(),
                    expect: "/tmp".to_string(),
                }),
            }
        );
    }

    #[test]
    fn starts_with_silent_command_catches_every_recognized_spelling() {
        for keys in [
            ":silent cd /tmp<CR>",
            ":silent!cd /tmp<CR>",
            ":silent! cd /tmp<CR>",
            ":silen cd /tmp<CR>",
            ":sile cd /tmp<CR>",
            ":sil cd /tmp<CR>",
            ":sil!cd /tmp<CR>",
            ": silent cd /tmp<CR>",
            ":  silent cd /tmp<CR>",
            ":silent",
        ] {
            assert!(
                starts_with_silent_command(keys),
                "expected {keys:?} to be recognized as a :silent-family command"
            );
        }
    }

    #[test]
    fn starts_with_silent_command_does_not_false_positive() {
        for keys in [
            ":sign place 1 line=1 name=x",
            ":simalt ~x",
            ":si cd /tmp<CR>",
            "ihello silent world<Esc>",
            "isilent<Esc>",
            ":silentx cd /tmp<CR>",
            ":silence cd /tmp<CR>",
            "cd /tmp<CR>",
        ] {
            assert!(
                !starts_with_silent_command(keys),
                "expected {keys:?} to NOT be recognized as a :silent-family command"
            );
        }
    }

    #[test]
    fn a_silent_bang_send_with_no_confirm_probe_is_rejected() {
        let toml = VALID.replace(
            "{ send = \"ihello<Esc>\" },",
            "{ send = \":silent! cd /tmp<CR>\" },",
        );
        let err = parse(&toml).expect_err(
            "a :silent! send step with no confirm_probe must be a hard error, matching the \
             bare :silent case",
        );
        assert!(
            matches!(err, ScenarioError::SilentSendWithoutConfirm { index: 0 }),
            "expected SilentSendWithoutConfirm{{index: 0}}, got {err:?}"
        );
    }

    #[test]
    fn a_sil_abbreviation_send_with_no_confirm_probe_is_rejected() {
        let toml = VALID.replace(
            "{ send = \"ihello<Esc>\" },",
            "{ send = \":sil cd /tmp<CR>\" },",
        );
        let err = parse(&toml).expect_err(
            "a :sil send step with no confirm_probe must be a hard error, matching the \
             full :silent spelling",
        );
        assert!(
            matches!(err, ScenarioError::SilentSendWithoutConfirm { index: 0 }),
            "expected SilentSendWithoutConfirm{{index: 0}}, got {err:?}"
        );
    }
}
