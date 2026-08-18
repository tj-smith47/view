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
/// No `serde` derive here: the wire shape lives in [`WireAgentSpec`] so this
/// resolved, public type never carries a deserialization contract as part
/// of its API.
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
    /// runnable (an empty or whitespace-only id, or a command with no
    /// program).
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
    /// Returns [`AiConfigError`] when the file exists but cannot be read or
    /// parsed; the error names the path in both cases.
    pub fn load(config_path: Option<&Path>) -> Result<Self, AiConfigError> {
        let Some(path) = config_path else {
            return Ok(Self::default());
        };
        match std::fs::read_to_string(path) {
            // a parse failure that came from a file names that file: a bare
            // line/column is not actionable when a user has more than one
            // config in play, and the read failure arm below already
            // answers with the path
            Ok(s) => Self::from_toml_str(&s).map_err(|e| match e {
                AiConfigError::Toml(source) => AiConfigError::ParseFile {
                    path: path.to_path_buf(),
                    source,
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
/// A `Command([])` has no program to run and an `Id("")` (or a
/// whitespace-only id) names no adapter; both would otherwise surface only
/// as a spawn failure downstream, with no line in `view.toml` to blame.
/// Refusing here, at parse time, keeps the diagnostic where the mistake is.
fn resolve_agent(wire: WireAgentSpec) -> Result<AgentSpec, AiConfigError> {
    match wire {
        WireAgentSpec::Id(id) if id.trim().is_empty() => Err(AiConfigError::EmptyAgent {
            detail: "agent id must not be empty or whitespace-only",
        }),
        WireAgentSpec::Id(id) => Ok(AgentSpec::Id(id)),
        WireAgentSpec::Command(words) if words.is_empty() => Err(AiConfigError::EmptyAgent {
            detail: "agent command must name a program, e.g. agent = [\"mycli\", \"--acp\"]",
        }),
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

impl<'de> Deserialize<'de> for WireAgentSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct AgentVisitor;

        impl<'de> serde::de::Visitor<'de> for AgentVisitor {
            type Value = WireAgentSpec;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(
                    "[ai] agent to be a string id (agent = \"claude-code\") or an array \
                     command (agent = [\"mycli\", \"--acp\"])",
                )
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
                while let Some(word) = seq.next_element::<String>()? {
                    words.push(word);
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
    /// or whitespace-only id, or a command array with no program.
    #[error("[ai] agent is empty: {detail}")]
    EmptyAgent {
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
