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
///
/// The flip side of ignoring them is that a table nothing reads yet parses
/// exactly like a table nobody will ever read, so the shipped example is
/// what tells a user which is which; `TABLES` in this module's tests pins
/// that listing and fails the moment a table gains a loader without the
/// example following.
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
            // a parse failure that came from a file names that file: a bare
            // line/column is not actionable when a user has more than one
            // config in play, and the read failure one arm below already
            // answers with the path
            Ok(s) => Self::from_toml_str(&s).map_err(|e| match e {
                NativeConfigError::Toml(source) => NativeConfigError::ParseFile {
                    path: path.to_path_buf(),
                    source,
                },
                other => other,
            }),
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
    /// A config file exists and was read, but is not valid TOML, or a
    /// `[native]` value in it is not a boolean.
    #[error("could not parse config file {path}: {source}")]
    ParseFile {
        /// The path whose contents failed to parse.
        path: PathBuf,
        /// The underlying TOML error, with its line and column.
        source: toml::de::Error,
    },
    /// TOML given directly as a string is not valid, or a `[native]` value
    /// in it is not a boolean. No path: there is no file behind it.
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

    use std::collections::BTreeSet;

    use super::*;

    /// The shipped example config, embedded at compile time rather than read
    /// through a `../..` walk from `CARGO_MANIFEST_DIR`: a moved or renamed
    /// example is then a build failure at the one site that names it,
    /// instead of a runtime read error in whichever test happened to run
    /// first.
    const EXAMPLE_TOML: &str = include_str!("../../../view.toml.example");

    /// One top-level table `view.toml` is specified to carry, and whether
    /// this build has a loader for it.
    ///
    /// [`ViewFile`] ignores unknown tables by design, so a table nothing
    /// reads yet parses exactly like a table nobody will ever read.
    /// Enumerating them here is what makes the example grow: a loaded table
    /// owes a live block, an unloaded one owes a commented block saying so,
    /// and flipping `loaded` when a loader lands fails until the example's
    /// block is uncommented.
    struct ConfigTable {
        /// The table's name as `view.toml` spells it, without brackets.
        name: &'static str,
        /// Whether a loader in this build reads the table.
        loaded: bool,
    }

    /// Every top-level table of `view.toml`, in the order the example lists
    /// them.
    static TABLES: [ConfigTable; 4] = [
        ConfigTable {
            name: "ui",
            loaded: false,
        },
        ConfigTable {
            name: "engine",
            loaded: false,
        },
        ConfigTable {
            name: "native",
            loaded: true,
        },
        ConfigTable {
            name: "ai",
            loaded: false,
        },
    ];

    /// Every `[table]` header in `EXAMPLE_TOML`, as `(name, commented)`.
    /// Lines are trimmed of an optional leading `#` so a documented-only
    /// table and a live one are found by the same pass.
    fn example_headers() -> Vec<(String, bool)> {
        EXAMPLE_TOML
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                let (body, commented) = trimmed
                    .strip_prefix('#')
                    .map_or((trimmed, false), |rest| (rest.trim(), true));
                let name = body.strip_prefix('[')?.strip_suffix(']')?;
                Some((name.to_string(), commented))
            })
            .collect()
    }

    #[test]
    fn the_example_config_keys_are_exactly_the_registry_ids() {
        let file: ViewFile = toml::from_str(EXAMPLE_TOML).expect("view.toml.example must parse");
        let in_example: BTreeSet<&str> = file.native.keys().map(String::as_str).collect();
        let in_registry: BTreeSet<&str> = registry::features().iter().map(|f| f.id).collect();
        assert_eq!(
            in_example, in_registry,
            "the example's [native] keys and the registry's ids must be the same set"
        );
    }

    #[test]
    fn every_specified_table_is_documented_in_the_example() {
        let headers = example_headers();
        for table in &TABLES {
            // one comparison for both failure modes: `None` is a table the
            // example never mentions, `Some(other)` is one whose block is
            // live when this build cannot read it, or commented out when it
            // can
            let commented = headers
                .iter()
                .find(|(name, _)| name == table.name)
                .map(|(_, commented)| *commented);
            assert_eq!(
                commented,
                Some(!table.loaded),
                "[{}] is {} by this build, so view.toml.example owes it a {} block",
                table.name,
                if table.loaded { "read" } else { "not read" },
                if table.loaded {
                    "live"
                } else {
                    "commented-out"
                }
            );
        }
    }

    #[test]
    fn the_example_documents_no_table_this_build_has_never_heard_of() {
        let known: BTreeSet<&str> = TABLES.iter().map(|t| t.name).collect();
        for (name, _) in example_headers() {
            assert!(
                known.contains(name.as_str()),
                "view.toml.example shows [{name}], which is in no specified table"
            );
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
        // written out and read back rather than loaded from the workspace
        // copy: the read path under test is `load`, and pointing it at a
        // scratch file keeps the assertion independent of where the example
        // sits relative to this crate
        let dir = std::env::temp_dir().join(format!("view-native-example-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temp dir must be creatable");
        let path = dir.join("view.toml");
        std::fs::write(&path, EXAMPLE_TOML).expect("the example must be writable");
        let loaded = NativeConfig::load(Some(&path));
        std::fs::remove_dir_all(&dir).expect("the temp dir must be removable");
        assert_eq!(
            loaded.expect("the example must load"),
            NativeConfig::all_enabled()
        );
    }

    #[test]
    fn an_unreadable_file_is_reported_with_its_path() {
        // a directory is the portable stand-in for "exists but is not a
        // readable file": every platform refuses to read one, with a kind
        // that is never NotFound
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let err = NativeConfig::load(Some(dir)).expect_err("a directory is not a config file");
        assert!(
            format!("{err}").contains(&dir.display().to_string()),
            "the error must name the path it failed on, got: {err}"
        );
    }

    #[test]
    fn an_unparseable_file_is_reported_with_its_path() {
        let dir = std::env::temp_dir().join(format!("view-native-broken-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temp dir must be creatable");
        let path = dir.join("view.toml");
        std::fs::write(&path, "[native\npicker = false\n").expect("a temp config must be writable");
        let err = NativeConfig::load(Some(&path)).expect_err("a broken config must not resolve");
        let msg = format!("{err}");
        std::fs::remove_dir_all(&dir).expect("the temp dir must be removable");
        assert!(
            matches!(err, NativeConfigError::ParseFile { .. }),
            "expected a file parse error, got: {err:?}"
        );
        assert!(
            msg.contains(&path.display().to_string()),
            "the parse error must name the file it failed on, got: {msg}"
        );
    }

    #[test]
    fn an_unknown_id_is_never_enabled() {
        let cfg = NativeConfig::all_enabled();
        assert!(!cfg.enabled("pickr"));
        assert!(!cfg.enabled(""));
    }
}
