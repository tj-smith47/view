//! The `[native]` table of `view.toml`: which native features a user has
//! turned off.
//!
//! An absent or empty file is the full experience, so every resolution path
//! that finds nothing to read answers `all_enabled()` rather than failing.
//! The key set is the feature registry itself, never a second list written
//! out here, so a feature can never exist in the table and be unspellable
//! in config.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use view_core::native::registry;

/// A resolved on/off answer for every feature in the registry.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeConfig {
    disabled: Vec<&'static str>,
}

/// The shape of `view.toml` this crate reads. Unknown top-level tables are
/// ignored rather than rejected: `[ui]`, `[engine]` and `[ai]` belong to
/// other subsystems, and a loader that failed on a sibling's table would
/// make every new table a breaking change here.
#[derive(Debug, Default, Deserialize)]
struct ViewFile {
    #[serde(default)]
    native: BTreeMap<String, bool>,
}

impl NativeConfig {
    /// Every feature on: the config-absent default, and what `--clean`
    /// resolves to.
    pub fn all_enabled() -> Self {
        Self {
            disabled: Vec::new(),
        }
    }

    /// Parses a `[native]` table. Unknown keys are an error, not a no-op.
    pub fn from_toml_str(s: &str) -> Result<Self, NativeConfigError> {
        let file: ViewFile = toml::from_str(s)?;
        // serde's `deny_unknown_fields` cannot express this key set: the
        // legal keys are the registry's rows, and a struct field per
        // feature would be a second copy of the table free to drift from
        // it. Cross-checking the parsed keys against the registry keeps one
        // source of truth and still refuses a typo instead of reading it as
        // "that feature stays on".
        for key in file.native.keys() {
            if !registry::is_feature(key) {
                return Err(NativeConfigError::UnknownFeature {
                    key: key.clone(),
                    known: known_ids(),
                });
            }
        }
        // resolution walks the registry, not the file, so an absent key is
        // an enabled feature by construction
        let disabled = registry::features()
            .iter()
            .filter(|f| !file.native.get(f.id).copied().unwrap_or(true))
            .map(|f| f.id)
            .collect();
        Ok(Self { disabled })
    }

    /// Reads `view.toml` from `config_path`, or `all_enabled()` when there
    /// is no path to read or no file at it. An absent file is the full
    /// experience; an unparseable one is an error the CLI reports.
    pub fn load(config_path: Option<&Path>) -> Result<Self, NativeConfigError> {
        let Some(path) = config_path else {
            return Ok(Self::all_enabled());
        };
        match std::fs::read_to_string(path) {
            Ok(s) => Self::from_toml_str(&s),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Self::all_enabled()),
            Err(source) => Err(NativeConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Whether the feature named `id` is enabled. An id that names no
    /// feature is not enabled: nothing in the build can act on it.
    #[must_use]
    pub fn enabled(&self, id: &str) -> bool {
        registry::is_feature(id) && !self.disabled.contains(&id)
    }
}

/// Every registry id, comma separated, for an error message that shows a
/// user what they could have written instead.
fn known_ids() -> String {
    registry::features()
        .iter()
        .map(|f| f.id)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Everything that can go wrong resolving the `[native]` table.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum NativeConfigError {
    /// The config file exists but could not be read.
    #[error("could not read config file {path}: {source}")]
    Read {
        /// The path that failed to read.
        path: PathBuf,
        /// The underlying filesystem error.
        source: std::io::Error,
    },
    /// The file is not valid TOML, or a `[native]` value is not a boolean.
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    /// A key inside `[native]` names no feature in the registry.
    #[error("unknown key `{key}` in [native]: no such native feature (known: {known})")]
    UnknownFeature {
        /// The key as the user spelled it.
        key: String,
        /// Every id the registry does know, comma separated.
        known: String,
    },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The shipped example config, read from the workspace root.
    fn example_toml() -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../view.toml.example");
        std::fs::read_to_string(path).expect("view.toml.example must be readable")
    }

    #[test]
    fn every_registry_feature_is_keyed_in_the_example_config() {
        let example = example_toml();
        let file: ViewFile = toml::from_str(&example).expect("view.toml.example must parse");
        let missing: Vec<&str> = registry::features()
            .iter()
            .map(|f| f.id)
            .filter(|id| !file.native.contains_key(*id))
            .collect();
        assert!(
            missing.is_empty(),
            "view.toml.example has no [native] key for: {missing:?}"
        );
    }

    #[test]
    fn every_example_key_is_a_registry_feature() {
        let cfg = NativeConfig::from_toml_str(&example_toml())
            .expect("view.toml.example must name only real features");
        for f in registry::features() {
            assert!(cfg.enabled(f.id), "{} must be enabled by the example", f.id);
        }
    }

    #[test]
    fn unknown_key_is_rejected_naming_it() {
        let err = NativeConfig::from_toml_str("[native]\npickr = false\n")
            .expect_err("a misspelled feature key must be an error, not a silent no-op");
        assert!(
            format!("{err}").contains("pickr"),
            "the error must name the offending key, got: {err}"
        );
    }

    #[test]
    fn unknown_key_error_lists_what_a_user_could_have_written() {
        let err = NativeConfig::from_toml_str("[native]\npickr = false\n")
            .expect_err("a misspelled feature key must be an error");
        let msg = format!("{err}");
        for f in registry::features() {
            assert!(msg.contains(f.id), "{} missing from the error: {msg}", f.id);
        }
    }

    #[test]
    fn disabling_one_feature_leaves_the_others_on() {
        let cfg = NativeConfig::from_toml_str("[native]\nstatusline = false\n")
            .expect("a known key must parse");
        assert!(!cfg.enabled("statusline"));
        for f in registry::features().iter().filter(|f| f.id != "statusline") {
            assert!(cfg.enabled(f.id), "{} must stay on", f.id);
        }
    }

    #[test]
    fn tables_owned_by_other_subsystems_are_ignored() {
        let cfg = NativeConfig::from_toml_str(
            "[ui]\ntier = \"auto\"\n\n[native]\npicker = false\n\n[ai]\nenabled = true\n",
        )
        .expect("sibling tables must not fail the native loader");
        assert!(!cfg.enabled("picker"));
        assert!(cfg.enabled("tree"));
    }

    #[test]
    fn an_empty_table_is_every_feature_on() {
        let cfg = NativeConfig::from_toml_str("[native]\n").expect("an empty table must parse");
        assert_eq!(cfg, NativeConfig::all_enabled());
    }

    #[test]
    fn a_non_bool_value_is_an_error() {
        let err = NativeConfig::from_toml_str("[native]\npicker = \"yes\"\n")
            .expect_err("only booleans switch a feature");
        assert!(
            matches!(err, NativeConfigError::Toml(_)),
            "expected a TOML type error, got: {err}"
        );
    }

    #[test]
    fn no_path_and_no_file_are_both_the_full_experience() {
        assert_eq!(
            NativeConfig::load(None).expect("no config path must resolve"),
            NativeConfig::all_enabled()
        );
        let missing = Path::new(env!("CARGO_MANIFEST_DIR")).join("no-such-view.toml");
        assert_eq!(
            NativeConfig::load(Some(&missing)).expect("an absent file must resolve"),
            NativeConfig::all_enabled()
        );
    }

    #[test]
    fn the_example_config_loads_from_disk() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../view.toml.example");
        let cfg = NativeConfig::load(Some(&path)).expect("the example must load");
        assert_eq!(cfg, NativeConfig::all_enabled());
    }

    #[test]
    fn an_unreadable_file_is_reported_with_its_path() {
        // a directory is the portable stand-in for "exists but is not a
        // readable file": every platform refuses to read one, with a kind
        // that is never NotFound
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let err = NativeConfig::load(Some(dir)).expect_err("a directory is not a config file");
        assert!(
            format!("{err}").contains("view-native"),
            "the error must name the path it failed on, got: {err}"
        );
    }

    #[test]
    fn an_unknown_id_is_never_enabled() {
        let cfg = NativeConfig::all_enabled();
        assert!(!cfg.enabled("pickr"));
        assert!(!cfg.enabled(""));
    }
}
