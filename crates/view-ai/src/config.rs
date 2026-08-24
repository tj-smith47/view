//! The `[ai]` table of `view.toml`: whether the agent panel and ACP client
//! are turned on, and which agent this build speaks to.
//!
//! `view-native`'s own `ViewFile` never grows a field for this table -- this
//! crate owns its own config so the dependency direction stays core <-
//! surface <- {native, ai} instead of `[ai]` reaching back through native to
//! be read. An absent or empty `[ai]` table is the full experience, matching
//! `[native]`'s own config-absent convention: `AiConfig::default()` is
//! agent-on with the one adapter this build knows how to provision on its
//! own.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use view_core::msg::ReviewOpenTarget;
use view_core::native::geometry;

/// Which agent an `[ai]` table names: a known adapter by id, or an
/// arbitrary command line for one this build has no adapter for.
///
/// No `serde` derive here: the wire shape lives in a private wire type, so
/// this resolved, public type never carries a deserialization contract as
/// part of its API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSpec {
    /// A known adapter id, resolved to a provisioned binary elsewhere.
    Id(String),
    /// An arbitrary command line, run as given with no provisioning.
    Command(Vec<String>),
}

/// Resolved `[ai]` config: whether the agent panel and ACP client are on,
/// and which agent to speak to.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiConfig {
    enabled: bool,
    agent: AgentSpec,
    panel_width: u16,
    panel_width_notice: Option<&'static str>,
    review_open_target: ReviewOpenTarget,
    review_open_target_notice: Option<&'static str>,
}

impl AiConfig {
    /// The config-absent answer: agent on, speaking to `claude-code` -- the
    /// one adapter this build knows how to auto-provision.
    // an inherent `default`, not only the `Default` impl below, so a caller
    // that never needs the trait (most call sites here) does not have to
    // spell `<AiConfig as Default>::default()` or pull the trait into scope
    #[allow(clippy::should_implement_trait)]
    pub fn default() -> Self {
        Self {
            enabled: true,
            agent: AgentSpec::Id("claude-code".to_string()),
            panel_width: geometry::DEFAULT_PANEL_WIDTH_PCT,
            panel_width_notice: None,
            review_open_target: ReviewOpenTarget::Current,
            review_open_target_notice: None,
        }
    }

    /// Parses an `[ai]` table out of one TOML document. A document with no
    /// `[ai]` table at all resolves to [`AiConfig::default`].
    ///
    /// # Errors
    ///
    /// Returns [`AiConfigError`] on invalid TOML, an `[ai]` value that does
    /// not match its expected shape, or an `agent` that names nothing
    /// runnable (an empty or whitespace-only id, or a command whose first
    /// element -- the program -- is empty, whitespace-only, or absent).
    pub fn from_toml_str(s: &str) -> Result<Self, AiConfigError> {
        // boxed for the same reason `NativeConfigError::Toml` is:
        // `toml::de::Error` is 128+ bytes on the msvc ABI, which makes an
        // unboxed `Result<_, AiConfigError>` a large-error return there
        let file: ConfigFile = toml::from_str(s).map_err(|e| AiConfigError::Toml(Box::new(e)))?;
        let (panel_width, panel_width_notice) = resolve_panel_width(file.ai.panel_width);
        let (review_open_target, review_open_target_notice) =
            resolve_open_target(file.ai.review.open_target);
        Ok(Self {
            enabled: file.ai.enabled,
            agent: resolve_agent(file.ai.agent)?,
            panel_width,
            panel_width_notice,
            review_open_target,
            review_open_target_notice,
        })
    }

    /// Reads `view.toml` from `config_path`, or [`AiConfig::default`] when
    /// there is no path to read or no file at it.
    ///
    /// # Errors
    ///
    /// Returns [`AiConfigError`] when the file exists but cannot be read,
    /// cannot be parsed, or names an `agent` with nothing runnable in it;
    /// the error names the path in every case.
    pub fn load(config_path: Option<&Path>) -> Result<Self, AiConfigError> {
        let Some(path) = config_path else {
            return Ok(Self::default());
        };
        match std::fs::read_to_string(path) {
            // a failure that came from a file names that file: a bare
            // line/column, or a detail string with no file attached, is not
            // actionable when a user has more than one config in play, and
            // the read failure arm below already answers with the path
            Ok(s) => Self::from_toml_str(&s).map_err(|e| match e {
                AiConfigError::Toml(source) => AiConfigError::ParseFile {
                    path: path.to_path_buf(),
                    source,
                },
                AiConfigError::EmptyAgent { detail, .. } => AiConfigError::EmptyAgent {
                    path: Some(path.to_path_buf()),
                    detail,
                },
                other => other,
            }),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(AiConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Whether the agent panel and ACP client are on.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Which agent this config resolves to.
    #[must_use]
    pub fn agent_spec(&self) -> &AgentSpec {
        &self.agent
    }

    /// The share of the terminal width the panel opens at, in percent,
    /// already clamped to the range the resize keys work in
    /// ([`view_core::native::geometry::clamp_panel_width`]) -- a `view.toml`
    /// asking for 5, 95 or -5 opens at the nearest end rather than refusing
    /// to start the editor.
    #[must_use]
    pub fn panel_width(&self) -> u16 {
        self.panel_width
    }

    /// What a `panel_width` that is not a whole number owes the user, or
    /// `None` when the key was absent or usable. See
    /// [`resolve_panel_width`] for why such a value is a notice rather than
    /// the parse error it looks like.
    #[must_use]
    pub fn panel_width_notice(&self) -> Option<&'static str> {
        self.panel_width_notice
    }

    /// Where a review shows a file no window already has open. See
    /// [`ReviewOpenTarget`] for why the default is the current window.
    #[must_use]
    pub fn review_open_target(&self) -> ReviewOpenTarget {
        self.review_open_target
    }

    /// What an `[ai.review] open_target` naming neither target owes the
    /// user, or `None` when the key was absent or usable -- a notice
    /// rather than a parse error, for the reason [`resolve_panel_width`]
    /// states.
    #[must_use]
    pub fn review_open_target_notice(&self) -> Option<&'static str> {
        self.review_open_target_notice
    }
}

impl Default for AiConfig {
    /// Delegates to the inherent [`AiConfig::default`]: the trait impl
    /// exists so this type composes with generic code that requires
    /// `Default`, without giving up the inherent method the interface
    /// names directly.
    fn default() -> Self {
        Self::default()
    }
}

/// What a `panel_width` that is not a whole number is answered with.
const PANEL_WIDTH_NOTICE: &str = "view: [ai] panel_width must be a whole number of percent -- \
     the agent panel opens at its default width this run";

/// The width the panel opens at, and the notice a value that is not a
/// whole number owes the user.
///
/// A width never fails the table, whatever is written for it. `[ai]`'s own
/// error path is fail-closed by design -- a table that cannot be read
/// turns the agent off for the whole run (see `seed_ai_enabled` in the bin
/// crate) -- and a mistyped percentage must not be the thing that disables
/// the panel. An integer resolves clamped however far outside the range it
/// is written; anything else (a float, a string, `40%`) opens at the
/// default and says so.
fn resolve_panel_width(value: Option<toml::Value>) -> (u16, Option<&'static str>) {
    match value {
        None => (geometry::DEFAULT_PANEL_WIDTH_PCT, None),
        Some(toml::Value::Integer(pct)) => (geometry::clamp_panel_width(pct), None),
        Some(_) => (geometry::DEFAULT_PANEL_WIDTH_PCT, Some(PANEL_WIDTH_NOTICE)),
    }
}

/// What an `open_target` naming neither target is answered with.
const OPEN_TARGET_NOTICE: &str =
    "view: [ai.review] open_target must be \"current\" or \"split\" -- \
     a review opens an unopened file in the current window this run";

/// Where a review opens a file no window has, and the notice an
/// unrecognized value owes the user. Never fails the table, for the reason
/// [`resolve_panel_width`] states.
fn resolve_open_target(value: Option<toml::Value>) -> (ReviewOpenTarget, Option<&'static str>) {
    match value.as_ref().and_then(toml::Value::as_str) {
        None if value.is_none() => (ReviewOpenTarget::Current, None),
        Some("current") => (ReviewOpenTarget::Current, None),
        Some("split") => (ReviewOpenTarget::Split, None),
        _ => (ReviewOpenTarget::Current, Some(OPEN_TARGET_NOTICE)),
    }
}

/// Turns a parsed [`WireAgentSpec`] into the public [`AgentSpec`], refusing
/// one that names nothing runnable.
///
/// A `Command([])` has no program to run, a `Command` whose first element
/// (the program) is empty or whitespace-only is equally unrunnable, and an
/// `Id("")` (or a whitespace-only id) names no adapter; all three would
/// otherwise surface only as a spawn failure downstream, with no line in
/// `view.toml` to blame. Refusing here, at parse time, keeps the
/// diagnostic where the mistake is.
///
/// A blank *later* element (`agent = ["mycli", ""]`) is deliberately left
/// alone: it is a command line with an empty argument, which is still
/// runnable and is `mycli`'s own business to accept or reject -- only the
/// program name in position zero is this loader's concern.
fn resolve_agent(wire: WireAgentSpec) -> Result<AgentSpec, AiConfigError> {
    match wire {
        WireAgentSpec::Id(id) if id.trim().is_empty() => Err(AiConfigError::EmptyAgent {
            path: None,
            detail: "agent id must not be empty or whitespace-only",
        }),
        // stored trimmed so an id validated as non-blank is also the exact
        // string later lookups and spawns see; " claude-code " resolving to
        // a padded id would fail adapter resolution with no config line to
        // blame, the same downstream-blame problem the blank check exists for
        WireAgentSpec::Id(id) => Ok(AgentSpec::Id(id.trim().to_string())),
        WireAgentSpec::Command(words) if words.first().is_none_or(|w| w.trim().is_empty()) => {
            Err(AiConfigError::EmptyAgent {
                path: None,
                detail: "agent command must name a program, e.g. agent = [\"mycli\", \"--acp\"]",
            })
        }
        WireAgentSpec::Command(words) => Ok(AgentSpec::Command(words)),
    }
}

/// The shape of `view.toml` this loader reads: one table, `[ai]`, with
/// every other top-level table ignored the same way `view-native`'s own
/// `ViewFile` ignores `[ai]`.
#[derive(Debug, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    ai: WireAiTable,
}

/// The `[ai.review]` sub-table's wire shape: how a review presents itself.
/// Its own table rather than a flat `review_open_target` key, so the
/// review's later settings have a place to land that is already the shape
/// a reader expects.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireReviewTable {
    /// Left as whatever was written, for the reason
    /// [`resolve_panel_width`] gives: a review setting must never be the
    /// thing that turns the agent off for a run.
    #[serde(default)]
    open_target: Option<toml::Value>,
}

/// The `[ai]` table's wire shape. Unknown keys are refused rather than
/// ignored, for the reason `[native]`'s key check states: a misspelled
/// switch that parses as "leave the default alone" reads to a user exactly
/// like a switch that worked.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireAiTable {
    #[serde(default = "wire_enabled_default")]
    enabled: bool,
    #[serde(default = "wire_agent_default")]
    agent: WireAgentSpec,
    /// Left as whatever was written, not typed as a number here: see
    /// [`resolve_panel_width`] for why a width is never allowed to fail
    /// this table.
    #[serde(default)]
    panel_width: Option<toml::Value>,
    #[serde(default)]
    review: WireReviewTable,
}

impl Default for WireAiTable {
    fn default() -> Self {
        Self {
            enabled: wire_enabled_default(),
            agent: wire_agent_default(),
            panel_width: None,
            review: WireReviewTable::default(),
        }
    }
}

/// `serde`'s `default` for [`WireAiTable::enabled`].
fn wire_enabled_default() -> bool {
    true
}

/// `serde`'s `default` for [`WireAiTable::agent`].
fn wire_agent_default() -> WireAgentSpec {
    WireAgentSpec::Id("claude-code".to_string())
}

/// The wire form of `agent`. Kept private and separate from the public
/// [`AgentSpec`] it resolves to (via [`resolve_agent`]) so `serde` is an
/// implementation detail of this module, never part of `AgentSpec`'s own
/// API -- and so this type, not `AgentSpec`, is the one place a
/// deserialization contract has to be honored.
///
/// `Deserialize` is hand-written rather than derived with
/// `#[serde(untagged)]`: an untagged enum's own error on a bad value names
/// the enum type and the words "untagged enum", neither of which a
/// `view.toml` author has ever seen. The [`Visitor`](serde::de::Visitor)
/// below answers in the two shapes the table actually accepts instead.
#[derive(Debug)]
enum WireAgentSpec {
    Id(String),
    Command(Vec<String>),
}

/// The two legal `agent` shapes, phrased for a config-file author rather
/// than for a Rust reader. Shared by the top-level `expecting` text (a
/// bad scalar or container) and the per-element wrap in `visit_seq` (a bad
/// word inside an otherwise-legal array), so both paths name the same two
/// shapes in the same words instead of drifting apart.
const AGENT_SHAPE_HINT: &str =
    "a string id (agent = \"claude-code\") or an array command (agent = [\"mycli\", \"--acp\"])";

impl<'de> Deserialize<'de> for WireAgentSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct AgentVisitor;

        impl<'de> serde::de::Visitor<'de> for AgentVisitor {
            type Value = WireAgentSpec;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "[ai] agent to be {AGENT_SHAPE_HINT}")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(WireAgentSpec::Id(v.to_string()))
            }

            fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(WireAgentSpec::Id(v))
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut words = Vec::new();
                loop {
                    // a per-element type mismatch (`agent = [1, 2]`) is
                    // useful on its own -- it names the offending element --
                    // but on its own it never says what `agent` as a whole
                    // is allowed to be; the wrap appends that without
                    // discarding the element-level detail
                    match seq.next_element::<String>() {
                        Ok(Some(word)) => words.push(word),
                        Ok(None) => break,
                        Err(e) => {
                            return Err(serde::de::Error::custom(format!(
                                "{e} ([ai] agent must be {AGENT_SHAPE_HINT})"
                            )));
                        }
                    }
                }
                Ok(WireAgentSpec::Command(words))
            }
        }

        deserializer.deserialize_any(AgentVisitor)
    }
}

/// Every `Result` in this module carries `AiConfigError` by value, so a
/// variant growing past `clippy::result_large_err`'s 128-byte threshold is
/// a lint failure rather than a review note -- and it fires per target ABI,
/// the same reason `view-native`'s own config module pins its error's size.
const _: () = assert!(std::mem::size_of::<AiConfigError>() <= 128);

/// Everything that can go wrong resolving the `[ai]` table.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum AiConfigError {
    /// The config file exists but could not be read.
    #[error("could not read config file {path}: {source}")]
    Read {
        /// The path that failed to read.
        path: PathBuf,
        /// The underlying filesystem error.
        source: std::io::Error,
    },
    /// A config file exists and was read, but is not valid TOML, or an
    /// `[ai]` value in it does not match its expected shape.
    #[error("could not parse config file {path}: {source}")]
    ParseFile {
        /// The path whose contents failed to parse.
        path: PathBuf,
        /// The underlying TOML error, with its line and column.
        source: Box<toml::de::Error>,
    },
    /// TOML given directly as a string is not valid, or an `[ai]` value in
    /// it does not match its expected shape. No path: there is no file
    /// behind it.
    #[error(transparent)]
    Toml(#[from] Box<toml::de::Error>),
    /// `agent` parsed to a legal shape but named nothing runnable: an empty
    /// or whitespace-only id, or a command array with no program (empty, or
    /// whose first element is empty or whitespace-only).
    #[error(
        "[ai] agent is empty: {detail}{}",
        path.as_ref()
            .map_or_else(String::new, |p| format!(" (in {})", p.display()))
    )]
    EmptyAgent {
        /// The file this value was read from, when there was one; `None`
        /// for TOML given directly as a string.
        path: Option<PathBuf>,
        /// What was empty, and what a legal value looks like instead.
        detail: &'static str,
    },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn from_toml_str_reads_the_enabled_and_agent_id_keys() {
        let cfg = AiConfig::from_toml_str(
            r#"
[ai]
enabled = true
agent = "claude-code"
"#,
        )
        .expect("a well-formed [ai] table must parse");
        assert!(cfg.enabled());
        assert_eq!(cfg.agent_spec(), &AgentSpec::Id("claude-code".into()));
    }

    #[test]
    fn an_agent_array_resolves_a_command_spec() {
        let cfg = AiConfig::from_toml_str("[ai]\nagent = [\"mycli\", \"--acp\"]\n")
            .expect("an array agent value must parse");
        assert_eq!(
            cfg.agent_spec(),
            &AgentSpec::Command(vec!["mycli".into(), "--acp".into()])
        );
    }

    #[test]
    fn an_absent_ai_table_resolves_the_derived_default() {
        let cfg = AiConfig::from_toml_str("").expect("an empty document must parse");
        assert_eq!(cfg, AiConfig::default());
        // the inherent `default` and the `Default` trait impl must agree --
        // see the rationale on `impl Default for AiConfig` for why this is
        // not automatic
        assert_eq!(cfg, <AiConfig as Default>::default());
    }

    #[test]
    fn no_path_and_no_file_are_both_the_derived_default() {
        assert_eq!(
            AiConfig::load(None).expect("no config path must resolve"),
            AiConfig::default()
        );
        let missing = std::env::temp_dir().join("view-ai-config-does-not-exist.toml");
        assert_eq!(
            AiConfig::load(Some(&missing)).expect("a missing file must resolve"),
            AiConfig::default()
        );
    }

    #[test]
    fn panel_width_round_trips_and_defaults_to_the_shared_sidebar_width() {
        let cfg = AiConfig::from_toml_str("[ai]\npanel_width = 45\n")
            .expect("a panel_width must parse beside the other keys");
        assert_eq!(cfg.panel_width(), 45);
        assert!(cfg.enabled(), "one key written leaves the rest defaulted");
        assert_eq!(
            AiConfig::from_toml_str("[ai]\nenabled = true\n")
                .expect("an [ai] table with no width must parse")
                .panel_width(),
            geometry::DEFAULT_PANEL_WIDTH_PCT
        );
    }

    #[test]
    fn a_panel_width_outside_the_range_is_clamped_rather_than_refused() {
        for (written, resolved) in [
            (0, geometry::MIN_PANEL_WIDTH_PCT),
            (5, geometry::MIN_PANEL_WIDTH_PCT),
            (95, geometry::MAX_PANEL_WIDTH_PCT),
            (65535, geometry::MAX_PANEL_WIDTH_PCT),
            // the two a `u16` field refused at the deserializer, before any
            // clamp could run
            (-5, geometry::MIN_PANEL_WIDTH_PCT),
            (-1_000_000, geometry::MIN_PANEL_WIDTH_PCT),
            (1_000_000, geometry::MAX_PANEL_WIDTH_PCT),
        ] {
            let cfg = AiConfig::from_toml_str(&format!("[ai]\npanel_width = {written}\n"))
                .expect("an out-of-range width must not keep the editor from starting");
            assert_eq!(cfg.panel_width(), resolved, "panel_width = {written}");
            assert_eq!(cfg.panel_width_notice(), None, "panel_width = {written}");
        }
    }

    /// The failure this exists to prevent: `[ai]`'s error path turns the
    /// agent off for the whole run, so a width that cannot be read as a
    /// number must resolve, not fail.
    #[test]
    fn a_panel_width_that_is_not_a_number_keeps_the_panel_and_says_so() {
        for written in ["\"wide\"", "3.5", "true", "[30]", "{ pct = 30 }"] {
            let cfg = AiConfig::from_toml_str(&format!("[ai]\npanel_width = {written}\n"))
                .unwrap_or_else(|e| panic!("panel_width = {written} must not fail the table: {e}"));
            assert!(
                cfg.enabled(),
                "panel_width = {written} must never disable the agent panel"
            );
            assert_eq!(cfg.panel_width(), geometry::DEFAULT_PANEL_WIDTH_PCT);
            let notice = cfg
                .panel_width_notice()
                .unwrap_or_else(|| panic!("panel_width = {written} owes the user a notice"));
            assert!(
                notice.contains("panel_width"),
                "the notice must name the key: {notice}"
            );
        }
    }

    #[test]
    fn an_unknown_key_in_ai_is_an_error() {
        let err = AiConfig::from_toml_str("[ai]\nauth = true\n")
            .expect_err("only enabled and agent are legal keys");
        assert!(
            matches!(err, AiConfigError::Toml(_)),
            "expected a TOML type error, got: {err}"
        );
    }

    #[test]
    fn a_malformed_agent_value_names_the_two_legal_shapes() {
        let err = AiConfig::from_toml_str("[ai]\nagent = 3\n")
            .expect_err("a bare number is neither legal agent shape");
        let msg = err.to_string();
        assert!(
            msg.contains("string id") && msg.contains("array"),
            "the error must name both legal shapes, got: {msg}"
        );
        assert!(
            !msg.contains("untagged") && !msg.contains("AgentSpec"),
            "the error must never surface serde's own type names, got: {msg}"
        );
    }

    #[test]
    fn an_empty_agent_command_is_refused() {
        let err = AiConfig::from_toml_str("[ai]\nagent = []\n")
            .expect_err("a command with no program cannot run");
        assert!(
            matches!(err, AiConfigError::EmptyAgent { .. }),
            "expected an empty-agent error, got: {err}"
        );
    }

    #[test]
    fn an_empty_agent_id_is_refused() {
        let err = AiConfig::from_toml_str("[ai]\nagent = \"\"\n")
            .expect_err("an empty id names no adapter");
        assert!(
            matches!(err, AiConfigError::EmptyAgent { .. }),
            "expected an empty-agent error, got: {err}"
        );
    }

    #[test]
    fn a_whitespace_only_agent_id_is_refused() {
        let err = AiConfig::from_toml_str("[ai]\nagent = \"   \"\n")
            .expect_err("a whitespace-only id names no adapter");
        assert!(
            matches!(err, AiConfigError::EmptyAgent { .. }),
            "expected an empty-agent error, got: {err}"
        );
    }

    #[test]
    fn a_padded_agent_id_resolves_trimmed() {
        let cfg = AiConfig::from_toml_str("[ai]\nagent = \" claude-code \"\n")
            .expect("a padded id is valid once trimmed");
        assert_eq!(cfg.agent_spec(), &AgentSpec::Id("claude-code".into()));
    }

    #[test]
    fn a_command_with_a_blank_program_name_is_refused() {
        let err = AiConfig::from_toml_str("[ai]\nagent = [\"\"]\n")
            .expect_err("an empty program name names nothing runnable");
        assert!(
            matches!(err, AiConfigError::EmptyAgent { .. }),
            "expected an empty-agent error, got: {err}"
        );
        let err = AiConfig::from_toml_str("[ai]\nagent = [\"   \", \"--acp\"]\n")
            .expect_err("a whitespace-only program name is equally unrunnable");
        assert!(
            matches!(err, AiConfigError::EmptyAgent { .. }),
            "expected an empty-agent error, got: {err}"
        );
    }

    #[test]
    fn a_blank_later_argument_is_left_to_the_program_to_reject() {
        // deliberate: only the program name (element zero) is this
        // loader's concern -- a blank later argument is still a runnable
        // command line, and accepting or rejecting it is `mycli`'s call
        let cfg = AiConfig::from_toml_str("[ai]\nagent = [\"mycli\", \"\"]\n")
            .expect("a blank later argument must not be refused here");
        assert_eq!(
            cfg.agent_spec(),
            &AgentSpec::Command(vec!["mycli".into(), String::new()])
        );
    }

    #[test]
    fn a_malformed_agent_array_element_names_the_two_legal_shapes() {
        let err = AiConfig::from_toml_str("[ai]\nagent = [1, 2]\n")
            .expect_err("an integer is not a legal command word");
        let msg = err.to_string();
        assert!(
            msg.contains("string id") && msg.contains("array"),
            "the error must name both legal shapes, got: {msg}"
        );
    }

    #[test]
    fn a_file_backed_empty_agent_names_its_path() {
        let dir =
            std::env::temp_dir().join(format!("view-ai-empty-agent-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir must be creatable");
        let path = dir.join("view.toml");
        std::fs::write(&path, "[ai]\nagent = \"\"\n").expect("temp file must be writable");

        let err = AiConfig::load(Some(&path)).expect_err("an empty agent id must be refused");
        assert!(
            matches!(err, AiConfigError::EmptyAgent { .. }),
            "expected an empty-agent error, got: {err}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains(&path.display().to_string()),
            "a file-backed error must name the file it came from, got: {msg}"
        );

        std::fs::remove_dir_all(&dir).expect("temp dir must be removable");
    }

    /// A key no wire table will ever accept, used to make one refuse and
    /// list what it does accept.
    const PROBE_KEY: &str = "zzz_not_a_key";

    /// Every field the wire table at `path` accepts, read out of serde's
    /// own `deny_unknown_fields` refusal rather than copied: the structs
    /// are the only list of these there is, and a second hand-written one
    /// is what goes stale the day a field is added.
    fn wire_fields(path: &str) -> Vec<String> {
        let refusal = toml::from_str::<ConfigFile>(&format!("[{path}]\n{PROBE_KEY} = 0\n"))
            .err()
            .map_or_else(
                || panic!("[{path}] must refuse an unknown key, and no longer does"),
                |err| err.to_string(),
            );
        // "expected one of `a`, `b`" for a table with several fields,
        // "expected `a`" for one with a single field -- both start here
        let listed = refusal.split_once("expected ").map_or_else(
            || panic!("serde's refusal no longer lists the fields it accepts: {refusal}"),
            |(_, listed)| listed.to_string(),
        );
        listed
            .split('`')
            .skip(1)
            .step_by(2)
            .map(str::to_owned)
            .collect()
    }

    /// Asserts `doc` sets every field the table at `path` accepts, and
    /// recurses into each one that is itself a table.
    fn assert_example_sets_every_field(doc: &toml::Value, path: &str) {
        let fields = wire_fields(path);
        assert!(
            !fields.is_empty(),
            "[{path}] accepts no fields, so this walk proves nothing"
        );
        for field in fields {
            let full = format!("{path}.{field}");
            let mut node = doc;
            for key in full.split('.') {
                node = node.get(key).unwrap_or_else(|| {
                    panic!(
                        "view.toml.example must spell {full}: every field the loader \
                         reads is a field the shipped example documents"
                    )
                });
            }
            if node.is_table() {
                assert_example_sets_every_field(doc, &full);
            }
        }
    }

    #[test]
    fn the_example_ai_block_round_trips_and_names_every_field() {
        const EXAMPLE_TOML: &str = include_str!("../../../view.toml.example");
        // the parse-to-defaults assertion below is satisfied by an absent
        // `[ai]` table too, so it alone cannot prove the block is live --
        // this pins presence first
        let doc: toml::Value =
            toml::from_str(EXAMPLE_TOML).expect("the shipped example must be valid TOML");
        // destructured rather than field-accessed: a second table this
        // loader learns to read stops this test compiling until the walk
        // below is told to cover it
        let ConfigFile { ai: _ } =
            toml::from_str(EXAMPLE_TOML).expect("the shipped example must parse as the wire shape");
        assert_example_sets_every_field(&doc, "ai");
        let cfg = AiConfig::from_toml_str(EXAMPLE_TOML)
            .expect("view.toml.example's [ai] block must parse");
        assert_eq!(cfg, AiConfig::default());
    }
}
