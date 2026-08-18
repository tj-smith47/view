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
        Ok(Self {
            enabled: file.ai.enabled,
            agent: resolve_agent(file.ai.agent)?,
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
        WireAgentSpec::Id(id) => Ok(AgentSpec::Id(id)),
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
}

impl Default for WireAiTable {
    fn default() -> Self {
        Self {
            enabled: wire_enabled_default(),
            agent: wire_agent_default(),
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
    #![allow(clippy::unwrap_used, clippy::expect_used)]

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

    #[test]
    fn the_example_ai_block_round_trips() {
        const EXAMPLE_TOML: &str = include_str!("../../../view.toml.example");
        // the parse-to-defaults assertion below is satisfied by an absent
        // `[ai]` table too, so it alone cannot prove the block is live --
        // this pins presence first
        let doc: toml::Value =
            toml::from_str(EXAMPLE_TOML).expect("the shipped example must be valid TOML");
        assert!(
            doc.get("ai").is_some(),
            "the shipped example must carry a live [ai] table, not just a commented-out stub"
        );
        let cfg = AiConfig::from_toml_str(EXAMPLE_TOML)
            .expect("view.toml.example's [ai] block must parse");
        assert_eq!(cfg, AiConfig::default());
    }
}
