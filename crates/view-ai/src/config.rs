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
use view_core::config::{
    discarded_env, Source, BOOL_EXPECTED, OPEN_TARGET_EXPECTED, WIDTH_EXPECTED,
};
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
#[non_exhaustive]
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiConfig {
    enabled: bool,
    agent: AgentSpec,
    panel_width: u16,
    panel_width_notice: Option<&'static str>,
    review_open_target: ReviewOpenTarget,
    review_open_target_notice: Option<&'static str>,
    /// Where each key's answer came from, in [`AI_KEYS`] order.
    sources: [Source; AI_KEYS.len()],
    /// One line per environment value this crate could not read, on the
    /// same terms the sibling resolver states: a discarded value is never
    /// an error and never silent.
    notices: Vec<String>,
}

/// The keys of this table, as the key registry spells their paths, in the
/// order that registry lists them. The registry lives a crate away and may
/// not be named from here, so the bin holds the two lists to each other.
pub const AI_KEYS: [(&str, &str); 4] = [
    ("ai", "enabled"),
    ("ai", "agent"),
    ("ai", "panel_width"),
    ("ai.review", "open_target"),
];

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
            agent: AgentSpec::Id(DEFAULT_AGENT_ID.to_string()),
            panel_width: geometry::DEFAULT_PANEL_WIDTH_PCT,
            panel_width_notice: None,
            review_open_target: ReviewOpenTarget::Current,
            review_open_target_notice: None,
            sources: [Source::Derived; AI_KEYS.len()],
            notices: Vec::new(),
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
        // a key left out and a key written at its own default resolve to
        // the same answer, and only one of them is the file's -- the same
        // distinction `view-native`'s own loader draws, for the same reason
        let panel_width_spelled = file.ai.panel_width.is_some();
        let surface_size_spelled = file.ui.surfaces.agent.size.is_some();
        let spelled = [
            file.ai.enabled.is_some(),
            file.ai.agent.is_some(),
            panel_width_spelled || surface_size_spelled,
            file.ai.review.open_target.is_some(),
        ];
        let (panel_width, panel_width_notice) = resolve_panel_width(file.ai.panel_width);
        let (surface_size, surface_size_notice) =
            resolve_surface_agent_size(file.ui.surfaces.agent.size);
        let mut notices = Vec::new();
        if let Some(notice) = surface_size_notice {
            notices.push(notice.to_string());
        }
        // one notice whenever the older key is spelled at all, whether or
        // not the surfaces table also names it -- the same rule
        // `view-native`'s own tree alias holds itself to (`config::
        // surfaces::alias`), so `[native] tree_width` and `[ai]
        // panel_width` tell a user the same thing under the same
        // condition rather than one warning on both keys written and the
        // other staying silent on the older key alone
        if panel_width_spelled {
            notices.push(view_core::config::alias_notice(
                "ai",
                "panel_width",
                "ui.surfaces.agent",
                "size",
            ));
        }
        let (review_open_target, review_open_target_notice) =
            resolve_open_target(file.ai.review.open_target);
        Ok(Self {
            enabled: file.ai.enabled.unwrap_or(true),
            agent: match file.ai.agent {
                Some(wire) => resolve_agent(wire)?,
                None => AgentSpec::Id(DEFAULT_AGENT_ID.to_string()),
            },
            panel_width: surface_size.unwrap_or(panel_width),
            panel_width_notice,
            review_open_target,
            review_open_target_notice,
            sources: spelled.map(|written| {
                if written {
                    Source::File
                } else {
                    Source::Derived
                }
            }),
            notices,
        })
    }

    /// The **file layer alone**: `view.toml` at `config_path`, or
    /// [`AiConfig::default`] when there is no path to read or no file at it.
    ///
    /// Not the answer a session runs on. The environment sits above this
    /// layer, and `--clean` suppresses it entirely, so a caller resolving
    /// config for a session calls [`AiConfig::resolve`] and this only
    /// through it. This stays public for the callers that genuinely want
    /// the file's own answer -- a doctor showing what a config file says,
    /// separately from what the session resolved.
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

    /// Every layer `[ai]` answers to, in the order spec section 11 states:
    /// the environment over the file at `config_path`, over the defaults.
    /// `[ai]` has no flags, so the chain starts one layer down from
    /// `view-native`'s.
    ///
    /// `clean` is view's triage mode, and skips both the file and the
    /// environment: a mode that answered "view or your config" for the keys
    /// the sibling crate resolves while letting `VIEW_AI_AGENT` through
    /// would be answering a different question here.
    ///
    /// # Errors
    ///
    /// Returns [`AiConfigError`] for anything [`AiConfig::load`] does; an
    /// environment value this crate cannot read is never an error, for the
    /// reason [`resolve_panel_width`] states about a mistyped width.
    pub fn resolve(config_path: Option<&Path>, clean: bool) -> Result<Self, AiConfigError> {
        Self::resolve_with(config_path, clean, &|name| std::env::var(name).ok())
    }

    /// [`AiConfig::resolve`] against a caller-supplied environment, so the
    /// chain is testable without mutating the process's own.
    ///
    /// # Errors
    ///
    /// The same as [`AiConfig::resolve`].
    pub fn resolve_with(
        config_path: Option<&Path>,
        clean: bool,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, AiConfigError> {
        if clean {
            return Ok(Self::default());
        }
        let mut resolved = Self::load(config_path)?;
        if let Some(value) = env_value(env, ENABLED_ENV) {
            resolved.apply_env(0, parse_bool(&value), &value, BOOL_EXPECTED, |cfg, on| {
                cfg.enabled = on;
            });
        }
        if let Some(value) = env_value(env, AGENT_ENV) {
            // no shape an id can fail: an environment carries no word
            // boundaries, so every non-empty value is one adapter id
            resolved.apply_env(1, Some(value.clone()), &value, "", |cfg, id| {
                cfg.agent = AgentSpec::Id(id);
            });
        }
        if let Some(value) = env_value(env, PANEL_WIDTH_ENV) {
            let width = value.parse::<i64>().ok().map(geometry::clamp_panel_width);
            resolved.apply_env(2, width, &value, WIDTH_EXPECTED, |cfg, pct| {
                cfg.panel_width = pct;
            });
        }
        if let Some(value) = env_value(env, OPEN_TARGET_ENV) {
            // the file layer answers an unreadable target with `current`
            // and a notice, because a review setting must never be what
            // turns the agent off; the environment layer instead leaves the
            // file's own answer standing, which is what every other
            // discarded environment value in this build does
            resolved.apply_env(
                3,
                parse_open_target(&value),
                &value,
                OPEN_TARGET_EXPECTED,
                |cfg, target| {
                    cfg.review_open_target = target;
                },
            );
        }
        Ok(resolved)
    }

    /// One key's environment layer: applied and marked when the value read,
    /// noticed and left alone when it did not.
    fn apply_env<T>(
        &mut self,
        index: usize,
        parsed: Option<T>,
        raw: &str,
        expected: &str,
        apply: impl Fn(&mut Self, T),
    ) {
        let (table, key) = AI_KEYS[index];
        match parsed {
            Some(value) => {
                apply(self, value);
                self.sources[index] = Source::Env;
            }
            None => self
                .notices
                .push(discarded_env(ENV_NAMES[index], raw, expected, table, key)),
        }
    }

    /// The `VIEW_*` names this crate reads, for the crate that holds the
    /// key registry and this loader to each other. `view-native` carries
    /// the `[ai]` rows as metadata and may not call this crate, so the two
    /// meet in the bin, which is the only crate that can name both.
    #[must_use]
    pub fn env_names() -> [&'static str; AI_KEYS.len()] {
        ENV_NAMES
    }

    /// Every `[ai]` key, its resolved value rendered for display, and the
    /// layer that answered it -- the rows the sibling crate's own walk
    /// cannot produce, in the order the key registry lists them.
    #[must_use]
    pub fn rows(&self) -> Vec<(&'static str, &'static str, String, Source)> {
        AI_KEYS
            .into_iter()
            .enumerate()
            .map(|(index, (table, key))| {
                let value = match index {
                    0 => self.enabled.to_string(),
                    1 => match &self.agent {
                        AgentSpec::Id(id) => id.clone(),
                        AgentSpec::Command(words) => words.join(" "),
                    },
                    2 => self.panel_width.to_string(),
                    _ => match self.review_open_target {
                        ReviewOpenTarget::Current => "current".to_string(),
                        ReviewOpenTarget::Split => "split".to_string(),
                    },
                };
                (table, key, value, self.sources[index])
            })
            .collect()
    }

    /// One line per environment value this session could not read.
    #[must_use]
    pub fn notices(&self) -> &[String] {
        &self.notices
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

/// The environment name for `[ai] enabled`, spelled the way the key
/// registry derives it from the key's own path.
const ENABLED_ENV: &str = "VIEW_AI_ENABLED";

/// The environment name for `[ai] agent`. A value here names an adapter
/// id; the array form -- a command line with its own arguments -- is the
/// file's, since an environment variable carries no word boundaries this
/// crate could read one from without inventing a quoting rule.
const AGENT_ENV: &str = "VIEW_AI_AGENT";

/// The environment name for `[ai] panel_width`.
const PANEL_WIDTH_ENV: &str = "VIEW_AI_PANEL_WIDTH";

/// The environment name for `[ai.review] open_target`. A nested table nests
/// in the name the same way it nests in the file: the dot between its
/// segments is written as the underscore that separates every other segment.
const OPEN_TARGET_ENV: &str = "VIEW_AI_REVIEW_OPEN_TARGET";

/// Every name above, in [`AI_KEYS`] order.
const ENV_NAMES: [&str; AI_KEYS.len()] = [ENABLED_ENV, AGENT_ENV, PANEL_WIDTH_ENV, OPEN_TARGET_ENV];

/// The one adapter this build knows how to auto-provision, and what an
/// absent `agent` resolves to.
const DEFAULT_AGENT_ID: &str = "claude-code";

/// What the environment says about one key, or `None` when it says
/// nothing. An empty value is no value, so a variable set to nothing
/// leaves the file's answer standing rather than becoming an agent id
/// nothing can spawn.
fn env_value(env: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    let value = env(name)?;
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// A switch's value, or `None` for text that is not one -- which falls
/// through to the layer below rather than failing the session, the same
/// way a mistyped `panel_width` does.
fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// What a `panel_width` that is not a whole number is answered with.
const PANEL_WIDTH_NOTICE: &str = "view: [ai] panel_width must be a whole number of percent. \
     The agent panel opens at its default width this run";

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

/// What a `[ui.surfaces.agent] size` that is not a whole number of percent
/// is answered with.
const SURFACE_AGENT_SIZE_NOTICE: &str = "view: [ui.surfaces.agent] size must be a whole number \
     of percent. The agent panel opens at its default width this run";

/// `[ui.surfaces.agent] size`, the newer key `panel_width` has become: read
/// the same way [`resolve_panel_width`] reads the older one, and answered
/// with `None` rather than a default when it is absent, so the caller can
/// tell "not written" from "written at the shared default" and fall back to
/// the older key.
fn resolve_surface_agent_size(value: Option<toml::Value>) -> (Option<u16>, Option<&'static str>) {
    match value {
        None => (None, None),
        Some(toml::Value::Integer(pct)) => (Some(geometry::clamp_panel_width(pct)), None),
        Some(_) => (None, Some(SURFACE_AGENT_SIZE_NOTICE)),
    }
}

/// What an `open_target` naming neither target is answered with.
const OPEN_TARGET_NOTICE: &str = "view: [ai.review] open_target must be \"current\" or \"split\". \
     A review opens an unopened file in the current window this run";

/// Where a review opens a file no window has, and the notice an
/// unrecognized value owes the user. Never fails the table, for the reason
/// [`resolve_panel_width`] states.
fn resolve_open_target(value: Option<toml::Value>) -> (ReviewOpenTarget, Option<&'static str>) {
    match value.as_ref().and_then(toml::Value::as_str) {
        None if value.is_none() => (ReviewOpenTarget::Current, None),
        Some(name) => match parse_open_target(name) {
            Some(target) => (target, None),
            None => (ReviewOpenTarget::Current, Some(OPEN_TARGET_NOTICE)),
        },
        _ => (ReviewOpenTarget::Current, Some(OPEN_TARGET_NOTICE)),
    }
}

/// The target a value names, or `None` for text naming neither. The one
/// vocabulary both the file and the environment read, so the two can never
/// accept different words for the same key.
fn parse_open_target(value: &str) -> Option<ReviewOpenTarget> {
    match value {
        "current" => Some(ReviewOpenTarget::Current),
        "split" => Some(ReviewOpenTarget::Split),
        _ => None,
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

/// The shape of `view.toml` this loader reads: `[ai]` in full, plus the one
/// field `[ui.surfaces.agent]` shares with it (`size`, `panel_width`'s newer
/// name) -- every other `[ui]` key, and every other top-level table, is
/// ignored the same way `view-native`'s own `ViewFile` ignores `[ai]`. Not
/// `deny_unknown_fields` anywhere in this shape: this crate does not own
/// `[ui]`, so a key `view-native` reads there is not this parser's to
/// refuse.
#[derive(Debug, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    ai: WireAiTable,
    #[serde(default)]
    ui: WireUiTable,
}

/// The `[ui]` table's wire shape, as far as this crate reads it.
#[derive(Debug, Default, Deserialize)]
struct WireUiTable {
    #[serde(default)]
    surfaces: WireUiSurfacesTable,
}

/// The `[ui.surfaces]` table's wire shape, as far as this crate reads it:
/// the agent panel's own sub-table, and nothing of any sibling surface's.
#[derive(Debug, Default, Deserialize)]
struct WireUiSurfacesTable {
    #[serde(default)]
    agent: WireUiSurfaceSizeTable,
}

/// One surface's table, as far as this crate reads it: `size` alone,
/// mirroring `[ai] panel_width`'s own untyped `toml::Value` for the reason
/// [`resolve_panel_width`] gives -- a percentage written `30.0` must not
/// refuse the whole document.
#[derive(Debug, Default, Deserialize)]
struct WireUiSurfaceSizeTable {
    #[serde(default)]
    size: Option<toml::Value>,
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
///
/// Every field is optional rather than defaulted, so a document that
/// spelled a key at its own default and one that left it out stay
/// distinguishable past the parse: they resolve to the same answer, and
/// only one of them is the *file's* answer.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireAiTable {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    agent: Option<WireAgentSpec>,
    /// Left as whatever was written, not typed as a number here: see
    /// [`resolve_panel_width`] for why a width is never allowed to fail
    /// this table.
    #[serde(default)]
    panel_width: Option<toml::Value>,
    #[serde(default)]
    review: WireReviewTable,
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
    use view_test_support::ScratchDir;

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
    fn the_panel_width_alias_is_read_from_the_surfaces_table() {
        // the newer key alone: nothing to warn about, since the older key
        // was never spelled
        let cfg = AiConfig::from_toml_str("[ui.surfaces.agent]\nsize = 45\n")
            .expect("[ui.surfaces.agent] must parse beside [ai]");
        assert_eq!(
            cfg.panel_width(),
            45,
            "panel_width must read the surfaces table's own size"
        );
        assert!(cfg.notices().is_empty(), "{:?}", cfg.notices());

        // both keys: the surfaces table wins, and the user is told why the
        // older key's own 20 did not answer
        let cfg =
            AiConfig::from_toml_str("[ai]\npanel_width = 20\n\n[ui.surfaces.agent]\nsize = 45\n")
                .expect("both keys must parse together");
        assert_eq!(
            cfg.panel_width(),
            45,
            "the surfaces table outranks the alias"
        );
        assert_eq!(
            cfg.notices(),
            [view_core::config::alias_notice(
                "ai",
                "panel_width",
                "ui.surfaces.agent",
                "size"
            )],
            "the user was not told which key answered"
        );
    }

    /// `[native] tree_width` alone tells the user it now has a
    /// newer name (`config::surfaces::alias` in `view-native`), and `[ai]
    /// panel_width` owes the same notice under the same condition --
    /// spelled at all, not only spelled alongside the surfaces table.
    #[test]
    fn the_panel_width_alias_notices_even_when_spelled_alone() {
        let cfg = AiConfig::from_toml_str("[ai]\npanel_width = 20\n")
            .expect("[ai] panel_width alone must parse");
        assert_eq!(cfg.panel_width(), 20, "the older key still answers alone");
        assert_eq!(
            cfg.notices(),
            [view_core::config::alias_notice(
                "ai",
                "panel_width",
                "ui.surfaces.agent",
                "size"
            )],
            "the older key's own name change owes a notice the moment it is \
             spelled, the same rule [native] tree_width holds itself to"
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
        let dir = ScratchDir::new("ai-empty-agent-probe").expect("temp dir must be creatable");
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
        // `ui` is not walked below the way `ai` is: `[ui]` is not this
        // crate's table (`view-native` owns its shape and its own example
        // walk). The one field this loader borrows from it,
        // `[ui.surfaces.agent] size`, is real but left commented out in the
        // shipped example: `[ai] panel_width` is the field this walk forces
        // live, spelled the same way the example spells `[native]
        // tree_width` beside `[ui.surfaces.tree] size`, so the alias
        // notice below is expected, not refused
        let ConfigFile { ai: _, ui: _ } =
            toml::from_str(EXAMPLE_TOML).expect("the shipped example must parse as the wire shape");
        assert_example_sets_every_field(&doc, "ai");
        let cfg = AiConfig::from_toml_str(EXAMPLE_TOML)
            .expect("view.toml.example's [ai] block must parse");
        // the example spells every key at the value it already defaults to,
        // which is two claims rather than one: the answers are the derived
        // answers, *and* every one of them is the file's own -- the
        // difference between "your config set this" and "view did", which
        // is the whole reason the parse records a spelling at all
        let spelled: Vec<String> = cfg.rows().into_iter().map(|(_, _, v, _)| v).collect();
        let derived: Vec<String> = AiConfig::default()
            .rows()
            .into_iter()
            .map(|(_, _, v, _)| v)
            .collect();
        assert_eq!(spelled, derived, "the example must ship the derived answer");
        for (table, key, _, source) in cfg.rows() {
            assert_eq!(
                source,
                Source::File,
                "the example spells [{table}] {key}, so the file answered it"
            );
        }
        assert_eq!(
            cfg.notices(),
            [view_core::config::alias_notice(
                "ai",
                "panel_width",
                "ui.surfaces.agent",
                "size"
            )],
            "the shipped example spells the older key, which owes its own \
             alias notice on every start, the same as [native] tree_width \
             does: {:?}",
            cfg.notices()
        );
    }

    /// An environment naming every key this crate reads, so a test about a
    /// layer being applied and a test about it being suppressed can drive
    /// the same one.
    fn every_key_set(name: &str) -> Option<String> {
        match name {
            ENABLED_ENV => Some("false".to_string()),
            AGENT_ENV => Some("mycli".to_string()),
            PANEL_WIDTH_ENV => Some("45".to_string()),
            OPEN_TARGET_ENV => Some("split".to_string()),
            _ => None,
        }
    }

    #[test]
    fn the_ai_keys_resolve_from_the_environment_over_the_file() {
        let dir = view_test_support::ScratchDir::new("ai-env-layer").expect("a scratch dir");
        let path = dir.join("view.toml");
        std::fs::write(
            &path,
            "[ai]\nenabled = true\nagent = \"claude-code\"\n\n\
             [ui.surfaces.agent]\nsize = 20\n\n\
             [ai.review]\nopen_target = \"current\"\n",
        )
        .expect("the fixture must be written");
        let resolved = AiConfig::resolve_with(Some(&path), false, &every_key_set)
            .expect("the fixture must resolve");
        assert!(
            !resolved.enabled(),
            "VIEW_AI_ENABLED=false outranks an `enabled = true` in the file"
        );
        assert_eq!(
            resolved.agent_spec(),
            &AgentSpec::Id("mycli".into()),
            "VIEW_AI_AGENT names the adapter"
        );
        assert_eq!(
            resolved.panel_width(),
            45,
            "VIEW_AI_PANEL_WIDTH outranks 20"
        );
        assert_eq!(
            resolved.review_open_target(),
            ReviewOpenTarget::Split,
            "VIEW_AI_REVIEW_OPEN_TARGET outranks the file's own target"
        );
        // the value and the reason, since a chain that is right for the
        // wrong reason answers correctly from the wrong layer
        for (table, key, _, source) in resolved.rows() {
            assert_eq!(
                source,
                Source::Env,
                "[{table}] {key} answered from somewhere other than the environment"
            );
        }
        assert!(resolved.notices().is_empty(), "{:?}", resolved.notices());
    }

    /// An environment value this crate cannot read leaves the file's answer
    /// standing -- and says so. A layer that silently stopped applying is
    /// the one shape a user has nothing to debug with.
    #[test]
    fn an_environment_value_this_crate_cannot_read_is_noticed_not_swallowed() {
        let dir = view_test_support::ScratchDir::new("ai-env-notice").expect("a scratch dir");
        let path = dir.join("view.toml");
        std::fs::write(&path, "[ai]\nenabled = false\npanel_width = 20\n")
            .expect("the fixture must be written");
        let env = |name: &str| match name {
            ENABLED_ENV => Some("off".to_string()),
            PANEL_WIDTH_ENV => Some("40%".to_string()),
            OPEN_TARGET_ENV => Some("floating".to_string()),
            _ => None,
        };
        let resolved =
            AiConfig::resolve_with(Some(&path), false, &env).expect("the fixture must resolve");
        assert!(!resolved.enabled(), "the file's answer still stands");
        assert_eq!(resolved.panel_width(), 20, "and so does the file's width");
        assert_eq!(
            resolved.review_open_target(),
            ReviewOpenTarget::Current,
            "and the derived target, which no layer named"
        );
        let notices = resolved.notices().join("\n");
        for name in [ENABLED_ENV, PANEL_WIDTH_ENV, OPEN_TARGET_ENV] {
            assert!(notices.contains(name), "{name} was discarded in silence");
        }
        // enabled and panel_width the file's, agent and open_target view's
        // own: a discarded environment value must not read as an answer
        let owed = [Source::File, Source::Derived, Source::File, Source::Derived];
        for ((table, key, _, source), expected) in resolved.rows().into_iter().zip(owed) {
            assert_eq!(
                source, expected,
                "[{table}] {key} answered from the wrong layer"
            );
        }
    }

    #[test]
    fn every_published_env_name_is_one_this_crate_reads() {
        // a name published for the registry cross-check but read by
        // nothing would pass that check while doing nothing at all
        for name in AiConfig::env_names() {
            let env = |asked: &str| (asked == name).then(|| every_key_set(name)).flatten();
            let resolved =
                AiConfig::resolve_with(None, false, &env).expect("no file, no failure path");
            assert_ne!(
                resolved,
                AiConfig::default(),
                "{name} reaches nothing this crate resolves"
            );
        }
    }

    #[test]
    fn clean_resolves_the_ai_keys_to_their_derived_defaults() {
        let dir = view_test_support::ScratchDir::new("ai-clean").expect("a scratch dir");
        let path = dir.join("view.toml");
        std::fs::write(&path, "[ai]\nenabled = false\nagent = \"mycli\"\n")
            .expect("the fixture must be written");
        // both layers present and both suppressed: `--clean` asks whether
        // view or a user's own configuration is at fault, and an answer
        // that let either through would answer something else
        let resolved = AiConfig::resolve_with(Some(&path), true, &every_key_set)
            .expect("a clean session reads no file at all");
        assert_eq!(
            resolved,
            AiConfig::default(),
            "--clean is the derived default for every [ai] key"
        );
    }
}
