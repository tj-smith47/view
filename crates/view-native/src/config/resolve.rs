//! The precedence chain spec section 11 states, answered in one place: a
//! command-line flag beats an environment variable, which beats the config
//! file, which beats the value view derives on its own.
//!
//! Every key answers through the same [`layer`] call, so no key can resolve
//! in a different order than its neighbour, and every answer carries the
//! layer it came from -- provenance is the half that makes a chain usable
//! for triage, since "the picker is off" and "your `view.toml` turns the
//! picker off" are different answers to the same question.

use std::path::PathBuf;

use view_core::model::Tier;
use view_core::native::registry;

use super::keys::{env_name, keys, ConfigKey};
use super::{NativeConfig, SupervisionConfig, ViewConfig};

/// Where a resolved value came from, in the precedence order the spec
/// states: a command-line flag beats an environment variable, which beats
/// the config file, which beats the value view derives on its own.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A flag on this invocation's command line.
    Flag,
    /// A `VIEW_*` variable the registry generates the name of.
    Env,
    /// The `view.toml` this session read.
    File,
    /// Nothing named it, so view answered for itself.
    Derived,
}

impl Source {
    /// The word a report prints for this layer.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Flag => "flag",
            Self::Env => "environment",
            Self::File => "config file",
            Self::Derived => "derived",
        }
    }
}

/// One resolved answer and the reason it is that answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved<T> {
    /// What the key resolved to.
    pub value: T,
    /// Which layer supplied it.
    pub source: Source,
}

/// Which rendering tier a user asked for. There is no `Auto` variant:
/// "auto" is the absence of a choice, carried as `None`, so this layer
/// never has to name a value it cannot compute -- only a caller holding a
/// terminal can probe one.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierChoice {
    /// Every capability this build draws with.
    Full,
    /// The middle tier.
    Standard,
    /// The floor every terminal can render.
    Basic,
}

impl TierChoice {
    /// The word a user writes for this tier, and the word a report prints.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Standard => "standard",
            Self::Basic => "basic",
        }
    }
}

impl From<TierChoice> for Tier {
    fn from(choice: TierChoice) -> Self {
        match choice {
            TierChoice::Full => Self::Full,
            TierChoice::Standard => Self::Standard,
            TierChoice::Basic => Self::Basic,
        }
    }
}

/// Every value a flag may override, taken from the parsed command line.
///
/// The five keys decision 6a names, and no more: `[native]`'s switches are
/// reached through `--clean` as a whole table, so a `None` flag on a
/// registry row is a stated state rather than a gap.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    /// `--tier`.
    pub tier: Option<TierChoice>,
    /// `--theme`.
    pub theme: Option<String>,
    /// `--nvim-bin`.
    pub nvim_bin: Option<PathBuf>,
    /// `--appname`.
    pub appname: Option<String>,
    /// `--single-grid`.
    pub single_grid: Option<bool>,
}

/// The `[ui]` table's resolved answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedUi {
    /// Which tier to render at, or `None` for the terminal's own answer.
    pub tier: Resolved<Option<TierChoice>>,
    /// Which colorscheme to ask nvim for, or `None` to derive the chrome
    /// from whatever the user's own config ended on.
    pub theme: Resolved<Option<String>>,
}

/// The `[engine]` table's resolved answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEngine {
    /// The editor to spawn, or `None` for the bundled layout beside this
    /// executable -- which only a caller that can look beside itself can
    /// resolve to a path.
    pub nvim_bin: Resolved<Option<PathBuf>>,
    /// The `NVIM_APPNAME` to set in the child, or `None` to inherit
    /// whatever the process already carries.
    pub appname: Resolved<Option<String>>,
    /// Whether to attach without `ext_multigrid`.
    pub single_grid: Resolved<bool>,
}

/// Every key this crate resolves, each with the reason it is that answer.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    /// The `[ui]` answers.
    pub ui: ResolvedUi,
    /// The `[engine]` answers.
    pub engine: ResolvedEngine,
    /// The tables this crate parses, with every layer applied: what a
    /// session attaches, binds keys and supervises from. Whole configs
    /// rather than a value per key, so a consumer that already takes a
    /// [`ViewConfig`] keeps taking one.
    pub tables: ViewConfig,
    /// Where each feature's switch came from, in `registry::features()`
    /// order.
    native: Vec<Source>,
    /// Where `[supervision] auto_restart` came from.
    supervision: Source,
}

/// Resolves every key against the process environment.
#[must_use]
pub fn resolve(file: &ViewConfig, flags: &Overrides) -> ResolvedConfig {
    resolve_with(file, flags, &|name| std::env::var(name).ok())
}

/// Resolves every key against a caller-supplied environment, so the
/// precedence chain is testable without mutating the process's own -- and
/// so a caller that must suppress the environment layer altogether
/// (`--clean`, whose whole question is "view or your config") passes one
/// that answers nothing.
#[must_use]
pub fn resolve_with(
    file: &ViewConfig,
    flags: &Overrides,
    env: &dyn Fn(&str) -> Option<String>,
) -> ResolvedConfig {
    let ui = ResolvedUi {
        // `[ui]` and `[engine]` are not tables `ViewFile` carries, so there
        // is nothing between the environment and the derived answer for
        // their keys; the flag and environment layers are the same code
        // every other key runs through
        tier: layer(
            flags.tier.map(Some),
            env_value(env, "ui", "tier").and_then(|value| parse_tier(&value)),
            None,
            None,
        ),
        theme: layer(
            flags.theme.as_deref().map(parse_theme),
            env_value(env, "ui", "theme").map(|value| parse_theme(&value)),
            None,
            None,
        ),
    };
    let engine = ResolvedEngine {
        nvim_bin: layer(
            flags.nvim_bin.clone().map(Some),
            env_value(env, "engine", "nvim_bin").map(|value| parse_nvim_bin(&value)),
            None,
            None,
        ),
        appname: layer(
            flags.appname.clone().map(Some),
            env_value(env, "engine", "appname").map(Some),
            None,
            None,
        ),
        single_grid: layer(
            flags.single_grid,
            env_value(env, "engine", "single_grid").and_then(|value| parse_bool(&value)),
            None,
            false,
        ),
    };
    let mut disabled = Vec::new();
    let mut native = Vec::with_capacity(registry::features().len());
    for feature in registry::features() {
        let switch = layer(
            None,
            env_value(env, "native", feature.id).and_then(|value| parse_bool(&value)),
            file.spells("native", feature.id)
                .then(|| !file.native.disabled.contains(&feature.id)),
            true,
        );
        if !switch.value {
            disabled.push(feature.id);
        }
        native.push(switch.source);
    }
    let auto_restart = layer(
        None,
        env_value(env, "supervision", "auto_restart").and_then(|value| parse_bool(&value)),
        file.spells("supervision", "auto_restart")
            .then_some(file.supervision.auto_restart),
        SupervisionConfig::default().auto_restart,
    );
    ResolvedConfig {
        ui,
        engine,
        tables: ViewConfig {
            native: NativeConfig {
                disabled,
                // a width and a key binding are the file's alone: neither is
                // a registry key, so neither has a layer above the file to
                // lose to
                tree_width: file.native.tree_width,
                tree_width_notice: file.native.tree_width_notice,
            },
            supervision: SupervisionConfig {
                auto_restart: auto_restart.value,
            },
            keys: file.keys.clone(),
            spelled: file.spelled.clone(),
        },
        native,
        supervision: auto_restart.source,
    }
}

impl ResolvedConfig {
    /// Every key, its resolved value rendered for display, and where it
    /// came from, in registry order. The doctor's config section is this
    /// walk; nothing re-derives the list.
    ///
    /// Eleven rows, not the registry's thirteen: the `[ai]` pair is parsed
    /// and resolved by the crate that owns that table, and a caller that
    /// can name both crates appends its answers to these.
    #[must_use]
    pub fn rows(&self) -> Vec<(&'static ConfigKey, String, Source)> {
        keys()
            .iter()
            .filter_map(|key| self.answer(key).map(|(value, source)| (key, value, source)))
            .collect()
    }

    /// One key's rendered value and layer, or `None` for a key another
    /// crate answers.
    fn answer(&self, key: &ConfigKey) -> Option<(String, Source)> {
        Some(match (key.table, key.key) {
            ("ui", "tier") => (
                self.ui
                    .tier
                    .value
                    .map_or("auto", TierChoice::label)
                    .to_string(),
                self.ui.tier.source,
            ),
            ("ui", "theme") => (
                self.ui.theme.value.clone().unwrap_or_else(|| "auto".into()),
                self.ui.theme.source,
            ),
            ("engine", "nvim_bin") => (
                self.engine
                    .nvim_bin
                    .value
                    .as_ref()
                    .map_or_else(|| "bundled".into(), |path| path.display().to_string()),
                self.engine.nvim_bin.source,
            ),
            ("engine", "appname") => (
                self.engine.appname.value.clone().unwrap_or_default(),
                self.engine.appname.source,
            ),
            ("engine", "single_grid") => (
                self.engine.single_grid.value.to_string(),
                self.engine.single_grid.source,
            ),
            ("native", id) => (
                (!self.tables.native.disabled.contains(&id)).to_string(),
                *self
                    .native
                    .get(registry::features().iter().position(|f| f.id == id)?)?,
            ),
            ("supervision", "auto_restart") => (
                self.tables.supervision.auto_restart.to_string(),
                self.supervision,
            ),
            _ => return None,
        })
    }
}

/// The chain itself, written once so no key can answer in a different
/// order than its neighbour.
fn layer<T>(flag: Option<T>, env: Option<T>, file: Option<T>, derived: T) -> Resolved<T> {
    if let Some(value) = flag {
        return Resolved {
            value,
            source: Source::Flag,
        };
    }
    if let Some(value) = env {
        return Resolved {
            value,
            source: Source::Env,
        };
    }
    if let Some(value) = file {
        return Resolved {
            value,
            source: Source::File,
        };
    }
    Resolved {
        value: derived,
        source: Source::Derived,
    }
}

/// What the environment says about one key, or `None` when it says
/// nothing.
///
/// The name is generated from the key's own registry row and never read
/// from anywhere else, which is what keeps the rest of the `VIEW_*`
/// namespace -- `VIEW_LOG`, and the harness's own names -- out of config
/// entirely. An empty value is no value: `VIEW_UI_THEME=` names no
/// colorscheme, and reading it as one would be a layer a user cannot
/// unset.
fn env_value(env: &dyn Fn(&str) -> Option<String>, table: &str, key: &str) -> Option<String> {
    let row = keys()
        .iter()
        .find(|row| row.table == table && row.key == key)?;
    let value = env(&env_name(row))?;
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// A switch's value, or `None` for text that is not one.
///
/// A value this cannot read falls through to the layer below rather than
/// failing the session: an environment view cannot parse is the same kind
/// of mistake as a mistyped `tree_width`, and neither is a reason to
/// refuse to open a file.
fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// A tier a user named, `Some(None)` for the word that names the absence
/// of a choice, and `None` for text that names no tier at all -- which
/// falls through to the layer below, for the reason [`parse_bool`] states.
fn parse_tier(value: &str) -> Option<Option<TierChoice>> {
    match value.to_ascii_lowercase().as_str() {
        "auto" => Some(None),
        "full" => Some(Some(TierChoice::Full)),
        "standard" => Some(Some(TierChoice::Standard)),
        "basic" => Some(Some(TierChoice::Basic)),
        _ => None,
    }
}

/// The colorscheme a value names, or `None` for the word that names the
/// absence of a choice. Any other text is a colorscheme name: nvim owns
/// the vocabulary, and a name this build has never heard of is answered by
/// the engine that has.
fn parse_theme(value: &str) -> Option<String> {
    (value != "auto").then(|| value.to_string())
}

/// The editor a value names, or `None` for the word that names the layout
/// beside this executable -- which is the same absence-of-a-choice the
/// `[ui]` keys spell `auto`.
fn parse_nvim_bin(value: &str) -> Option<PathBuf> {
    (value != "bundled").then(|| PathBuf::from(value))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// An environment that answers nothing, which is what `--clean` hands
    /// the resolver.
    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// An environment carrying a legal value for every key this crate
    /// resolves, so a test that must show a layer being *suppressed* has
    /// something to suppress.
    fn every_key_set(name: &str) -> Option<String> {
        keys()
            .iter()
            .find(|row| env_name(row) == name)
            .map(|row| env_fixture(row).to_string())
    }

    /// A legal environment value for one key, per its own type.
    fn env_fixture(row: &ConfigKey) -> &'static str {
        match (row.table, row.key) {
            ("ui", "tier") => "basic",
            ("ui", "theme") => "gruvbox",
            ("engine", "nvim_bin") => "/opt/nvim/bin/nvim",
            ("engine", "appname") => "work",
            _ => "false",
        }
    }

    /// One row's rendered value and layer, by key path.
    fn row(resolved: &ResolvedConfig, table: &str, key: &str) -> (String, Source) {
        resolved
            .rows()
            .into_iter()
            .find(|(row, _, _)| row.table == table && row.key == key)
            .map(|(_, value, source)| (value, source))
            .unwrap_or_else(|| panic!("[{table}] {key} has no resolved row"))
    }

    /// The chain's four layers, proven across the two keys that between
    /// them carry all four: `[ui] tier` is reachable by flag and by
    /// environment, and `[supervision] auto_restart` by environment and by
    /// file. Every adjacent pair of the chain therefore has a key that
    /// shows the higher layer winning. No single key spans all four,
    /// because `[ui]` is not a table `ViewFile` carries.
    ///
    /// Both halves of every arm are asserted -- the value *and* the
    /// `Source` -- since a chain that is right for the wrong reason
    /// answers with the right value from the wrong layer, and only the
    /// provenance can tell the two apart.
    #[test]
    fn flag_beats_env_beats_file_beats_derived() {
        let file = ViewConfig::from_toml_str("[supervision]\nauto_restart = true\n")
            .expect("the fixture must parse");
        let env = |name: &str| match name {
            "VIEW_UI_TIER" => Some("basic".to_string()),
            "VIEW_SUPERVISION_AUTO_RESTART" => Some("false".to_string()),
            _ => None,
        };
        let flags = Overrides {
            tier: Some(TierChoice::Full),
            ..Overrides::default()
        };

        let resolved = resolve_with(&file, &flags, &env);
        assert_eq!(
            resolved.ui.tier,
            Resolved {
                value: Some(TierChoice::Full),
                source: Source::Flag
            },
            "the flag outranks the environment"
        );
        assert_eq!(
            row(&resolved, "supervision", "auto_restart"),
            ("false".to_string(), Source::Env),
            "the environment outranks the file"
        );

        let resolved = resolve_with(&file, &Overrides::default(), &env);
        assert_eq!(
            resolved.ui.tier,
            Resolved {
                value: Some(TierChoice::Basic),
                source: Source::Env
            },
            "with no flag, the environment answers"
        );

        let resolved = resolve_with(&file, &Overrides::default(), &no_env);
        assert_eq!(
            row(&resolved, "supervision", "auto_restart"),
            ("true".to_string(), Source::File),
            "with no environment, the file answers -- and a key written at \
             its own default is still the file's answer"
        );
        assert_eq!(
            resolved.ui.tier,
            Resolved {
                value: None,
                source: Source::Derived
            },
            "with nothing named at all, the sentinel a caller probes for"
        );
        assert_eq!(
            row(
                &resolve_with(&ViewConfig::defaults(), &Overrides::default(), &no_env),
                "supervision",
                "auto_restart"
            ),
            ("true".to_string(), Source::Derived),
            "and an absent file leaves the same key derived"
        );
    }

    #[test]
    fn every_key_has_a_derived_default() {
        let resolved = resolve_with(&ViewConfig::defaults(), &Overrides::default(), &no_env);
        for (key, value, source) in resolved.rows() {
            assert_eq!(
                source,
                Source::Derived,
                "[{}] {} has no derived answer",
                key.table,
                key.key
            );
            assert!(
                !value.is_empty() || key.derived.is_none(),
                "[{}] {} renders nothing, yet claims a derived value of {:?}",
                key.table,
                key.key,
                key.derived
            );
        }
        // the `[ai]` rows are resolved a crate away, so what this crate can
        // hold them to is the registry's own statement of their defaults
        for key in keys().iter().filter(|key| key.table == "ai") {
            assert!(
                key.derived.is_some(),
                "[{}] {} is required, and no key may be",
                key.table,
                key.key
            );
        }
    }

    #[test]
    fn a_flagless_key_still_resolves_through_env_and_file() {
        let key = keys()
            .iter()
            .find(|key| key.table == "native" && key.key == "picker")
            .expect("the picker is a registry feature");
        assert_eq!(key.flag, None, "[native] switches are reached by --clean");

        let file = ViewConfig::from_toml_str("[native]\npicker = false\n")
            .expect("the fixture must parse");
        assert_eq!(
            row(
                &resolve_with(&file, &Overrides::default(), &no_env),
                "native",
                "picker"
            ),
            ("false".to_string(), Source::File),
            "a key with no flag still reads the file"
        );
        let env = |name: &str| (name == "VIEW_NATIVE_PICKER").then(|| "true".to_string());
        assert_eq!(
            row(
                &resolve_with(&file, &Overrides::default(), &env),
                "native",
                "picker"
            ),
            ("true".to_string(), Source::Env),
            "and still loses to the environment"
        );
    }

    #[test]
    fn native_and_supervision_keys_resolve_from_env() {
        let env = |name: &str| match name {
            "VIEW_NATIVE_PICKER" => Some("false".to_string()),
            "VIEW_SUPERVISION_AUTO_RESTART" => Some("false".to_string()),
            _ => None,
        };
        let resolved = resolve_with(&ViewConfig::defaults(), &Overrides::default(), &env);
        assert!(
            !resolved.tables.native.enabled("picker"),
            "VIEW_NATIVE_PICKER=false disables the picker with no file present"
        );
        assert!(
            resolved.tables.native.enabled("tree"),
            "one key's environment must not answer for another"
        );
        assert!(
            !resolved.tables.supervision.auto_restart,
            "VIEW_SUPERVISION_AUTO_RESTART=false surfaces a dead engine instead"
        );
    }

    /// Every key this crate resolves reads the name its own registry row
    /// generates, and no other. Walked rather than sampled: a lookup that
    /// misspells one table or key would otherwise resolve to the derived
    /// answer forever, with nothing to say so.
    #[test]
    fn every_resolved_key_reads_its_own_environment_name() {
        for key in keys().iter().filter(|key| key.table != "ai") {
            let name = env_name(key);
            let env = |asked: &str| (asked == name).then(|| env_fixture(key).to_string());
            let resolved = resolve_with(&ViewConfig::defaults(), &Overrides::default(), &env);
            let (value, source) = row(&resolved, key.table, key.key);
            assert_eq!(
                source,
                Source::Env,
                "{name} does not reach [{}] {}",
                key.table,
                key.key
            );
            assert_eq!(
                value,
                env_fixture(key),
                "{name} resolved to something other than what it said"
            );
        }
    }

    #[test]
    fn view_log_is_never_read_as_config() {
        let asked = std::cell::RefCell::new(Vec::new());
        let env = |name: &str| {
            asked.borrow_mut().push(name.to_string());
            // every VIEW_* name answers, so a resolver reading one it does
            // not own would resolve to it rather than to its own default
            Some("false".to_string())
        };
        let resolved = resolve_with(&ViewConfig::defaults(), &Overrides::default(), &env);
        let generated: Vec<String> = keys().iter().map(env_name).collect();
        for name in asked.borrow().iter() {
            assert!(
                generated.contains(name),
                "the resolver read {name}, which no registry row generates"
            );
        }
        for name in ["VIEW_LOG", "VIEW_LOG_FILE", "VIEW_HARNESS_TAP"] {
            assert!(
                !asked.borrow().iter().any(|read| read == name),
                "{name} is diagnostics, never config"
            );
        }
        assert!(
            !resolved.tables.native.enabled("picker"),
            "the names the registry does generate are still read"
        );
    }

    /// `--clean` asks whether view or a user's own config is at fault, and
    /// an answer that let the environment through would be answering a
    /// different question than the one the mode exists for. What
    /// the mode passes is an environment that says nothing; both halves
    /// are pinned here, so a suppression that stopped suppressing fails.
    #[test]
    fn clean_resolves_every_key_to_its_derived_default() {
        let file = ViewConfig::from_toml_str("[native]\npicker = false\n")
            .expect("the fixture must parse");
        let dirty = resolve_with(&file, &Overrides::default(), &every_key_set);
        assert!(
            dirty
                .rows()
                .iter()
                .all(|(_, _, source)| *source == Source::Env),
            "the fixture environment must reach every key: {:?}",
            dirty.rows()
        );

        // what `--clean` resolves: no config path at all, so the defaults
        // stand in for the file, and no environment layer either
        let clean = resolve_with(&ViewConfig::defaults(), &Overrides::default(), &no_env);
        for (key, _, source) in clean.rows() {
            assert_eq!(
                source,
                Source::Derived,
                "[{}] {} survived --clean",
                key.table,
                key.key
            );
        }
        assert!(clean.tables.native.enabled("picker"));
        assert!(clean.tables.supervision.auto_restart);
    }

    #[test]
    fn rows_answer_every_key_this_crate_resolves() {
        let resolved = resolve_with(&ViewConfig::defaults(), &Overrides::default(), &no_env);
        let answered: Vec<(&str, &str)> = resolved
            .rows()
            .into_iter()
            .map(|(key, _, _)| (key.table, key.key))
            .collect();
        let owed: Vec<(&str, &str)> = keys()
            .iter()
            .filter(|key| key.table != "ai")
            .map(|key| (key.table, key.key))
            .collect();
        assert_eq!(
            answered, owed,
            "every registry key but the two `view-ai` resolves owes a row here"
        );
        assert_eq!(answered.len(), 11, "eleven rows, thirteen keys");
    }

    #[test]
    fn a_value_the_environment_cannot_be_read_as_falls_through() {
        let file = ViewConfig::from_toml_str("[native]\npicker = false\n")
            .expect("the fixture must parse");
        let env = |name: &str| match name {
            "VIEW_NATIVE_PICKER" => Some("off".to_string()),
            "VIEW_UI_TIER" => Some("turbo".to_string()),
            _ => None,
        };
        let resolved = resolve_with(&file, &Overrides::default(), &env);
        assert_eq!(
            row(&resolved, "native", "picker"),
            ("false".to_string(), Source::File),
            "text that is not a switch leaves the file's answer standing"
        );
        assert_eq!(
            resolved.ui.tier.source,
            Source::Derived,
            "and text that names no tier leaves the sentinel"
        );
    }

    #[test]
    fn auto_and_bundled_are_the_absence_of_a_choice_at_every_layer() {
        let env = |name: &str| match name {
            "VIEW_UI_THEME" => Some("auto".to_string()),
            "VIEW_ENGINE_NVIM_BIN" => Some("bundled".to_string()),
            _ => None,
        };
        let resolved = resolve_with(&ViewConfig::defaults(), &Overrides::default(), &env);
        assert_eq!(
            resolved.ui.theme,
            Resolved {
                value: None,
                source: Source::Env
            },
            "a user who wrote `auto` still wrote something"
        );
        assert_eq!(
            resolved.engine.nvim_bin,
            Resolved {
                value: None,
                source: Source::Env
            },
            "and `bundled` is the same shape for the engine"
        );
        assert_eq!(row(&resolved, "ui", "theme").0, "auto");
        assert_eq!(row(&resolved, "engine", "nvim_bin").0, "bundled");
    }

    #[test]
    fn the_flag_layer_carries_every_key_that_has_a_flag() {
        let flags = Overrides {
            tier: Some(TierChoice::Standard),
            theme: Some("gruvbox".to_string()),
            nvim_bin: Some(PathBuf::from("/opt/nvim/bin/nvim")),
            appname: Some("work".to_string()),
            single_grid: Some(true),
        };
        let resolved = resolve_with(&ViewConfig::defaults(), &flags, &every_key_set);
        for key in keys().iter().filter(|key| key.flag.is_some()) {
            let (_, source) = row(&resolved, key.table, key.key);
            assert_eq!(
                source,
                Source::Flag,
                "{} does not reach [{}] {}",
                key.flag.unwrap_or("<none>"),
                key.table,
                key.key
            );
        }
        assert_eq!(resolved.engine.appname.value.as_deref(), Some("work"));
        assert_eq!(
            resolved.engine.nvim_bin.value,
            Some(PathBuf::from("/opt/nvim/bin/nvim"))
        );
        assert!(resolved.engine.single_grid.value);
    }

    #[test]
    fn a_tier_choice_is_the_terminals_own_tier() {
        for (choice, tier) in [
            (TierChoice::Full, Tier::Full),
            (TierChoice::Standard, Tier::Standard),
            (TierChoice::Basic, Tier::Basic),
        ] {
            assert_eq!(Tier::from(choice), tier, "{}", choice.label());
        }
    }
}
