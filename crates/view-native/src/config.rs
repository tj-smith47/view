//! The tables of `view.toml` this build reads: `[native]`, which native
//! features a user has turned off, and `[supervision]`, how far view may go
//! on its own to recover a failed engine.
//!
//! An absent or empty file is the full experience, so every resolution path
//! that finds nothing to read answers `all_enabled()` rather than failing.
//! The key set of `[native]` is the feature registry itself, never a second
//! list written out here, so a feature can never exist in the table and be
//! unspellable in config.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
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
/// what tells a user which is which. This struct is the machine-readable
/// half of that: `loaded_tables` in the tests renders it and takes the key
/// set, so adding a field here *is* the event the example pin fires on.
///
/// `Serialize` is derived for exactly that reason and for no runtime
/// purpose. Round-tripping a `Default` through `toml::Value` is the only
/// way this crate can enumerate its own table names without a second list
/// beside the struct, free to drift from it.
///
/// A field naming a `[table]` this loader reads must stay non-`Option` --
/// a `#[serde(default)]` concrete value, as `native` already is -- for the
/// derived pin to see it at all. `toml`'s serializer emits no key for a
/// `None`, so an `Option`-typed table would round-trip through
/// `toml::Value::try_from(ViewFile::default())` as an absent key: present in
/// the struct, invisible to `loaded_tables`, and free to drift from
/// `EXAMPLE_TOML` without either direction of this file's tests noticing.
#[derive(Debug, Default, Deserialize, Serialize)]
struct ViewFile {
    #[serde(default)]
    native: BTreeMap<String, bool>,
    #[serde(default)]
    supervision: SupervisionTable,
}

/// The `[supervision]` table's wire shape. Unknown keys are refused rather
/// than ignored, for the reason `[native]`'s key check states: a misspelled
/// switch that parses as "leave the default alone" reads to a user exactly
/// like a switch that worked.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SupervisionTable {
    #[serde(default = "yes")]
    auto_restart: bool,
}

/// `serde`'s `default` for [`SupervisionTable::auto_restart`], which is the
/// only field whose absent value is not `bool`'s own default.
fn yes() -> bool {
    true
}

impl Default for SupervisionTable {
    fn default() -> Self {
        Self {
            auto_restart: yes(),
        }
    }
}

/// How far view may go on its own to recover a failed engine.
///
/// One field, because it is the one genuine choice here: whether a
/// connection observed dead is respawned without asking. The thresholds and
/// the probe cadence behind the detection are internal constants, not knobs
/// -- a number the tool has already sized correctly is not a decision to
/// hand a user.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupervisionConfig {
    /// `false` surfaces a dead engine and waits for the busy modal's own
    /// `Restart`; the manual choice works either way.
    pub auto_restart: bool,
}

impl Default for SupervisionConfig {
    /// Automatic recovery on: the config-absent default, and what `--clean`
    /// resolves to.
    fn default() -> Self {
        Self { auto_restart: true }
    }
}

/// Everything `view.toml` resolves to for this build, parsed in one pass.
///
/// One read and one parse, not one per table: two loaders over the same file
/// would each own an error path for the same unreadable file, and a caller
/// would have to decide which of the two answers to believe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewConfig {
    /// The `[native]` table's resolved answers.
    pub native: NativeConfig,
    /// The `[supervision]` table's resolved answers.
    pub supervision: SupervisionConfig,
}

impl ViewConfig {
    /// Every default: the config-absent answer for every table.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            native: NativeConfig::all_enabled(),
            supervision: SupervisionConfig::default(),
        }
    }

    /// Parses every table this build reads out of one TOML document.
    ///
    /// # Errors
    ///
    /// Returns [`NativeConfigError`] on invalid TOML, a `[native]` key that
    /// names no feature, or an unknown key inside `[supervision]`.
    pub fn from_toml_str(s: &str) -> Result<Self, NativeConfigError> {
        // boxed: `toml::de::Error` is 128+ bytes on the msvc ABI, which
        // makes every `Result<_, NativeConfigError>` in this module a
        // large-error return there (`clippy::result_large_err`)
        let file: ViewFile = toml::from_str(s).map_err(|e| NativeConfigError::Toml(Box::new(e)))?;
        Ok(Self {
            native: NativeConfig::from_parsed(&file)?,
            supervision: SupervisionConfig {
                auto_restart: file.supervision.auto_restart,
            },
        })
    }

    /// Reads `view.toml` from `config_path`, or every default when there is
    /// no path to read or no file at it.
    ///
    /// # Errors
    ///
    /// Returns [`NativeConfigError`] when the file exists but cannot be read
    /// or parsed; the error names the path in both cases.
    pub fn load(config_path: Option<&Path>) -> Result<Self, NativeConfigError> {
        let Some(path) = config_path else {
            return Ok(Self::defaults());
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
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Self::defaults()),
            Err(source) => Err(NativeConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }
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
    ///
    /// # Errors
    ///
    /// Returns [`NativeConfigError`] on invalid TOML or a key that names no
    /// feature in the registry.
    pub fn from_toml_str(s: &str) -> Result<Self, NativeConfigError> {
        ViewConfig::from_toml_str(s).map(|cfg| cfg.native)
    }

    /// The `[native]` half of an already-parsed document.
    fn from_parsed(file: &ViewFile) -> Result<Self, NativeConfigError> {
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
    ///
    /// # Errors
    ///
    /// Returns [`NativeConfigError`] when the file exists but cannot be read
    /// or parsed; the error names the path in both cases.
    pub fn load(config_path: Option<&Path>) -> Result<Self, NativeConfigError> {
        ViewConfig::load(config_path).map(|cfg| cfg.native)
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
        source: Box<toml::de::Error>,
    },
    /// TOML given directly as a string is not valid, or a `[native]` value
    /// in it is not a boolean. No path: there is no file behind it.
    #[error(transparent)]
    Toml(#[from] Box<toml::de::Error>),
    /// A key inside `[native]` names no feature in the registry.
    #[error("unknown key `{key}` in [native]: no such native feature (known: {known})")]
    UnknownFeature {
        /// The key as the user spelled it.
        key: String,
        /// Every id the registry does know, comma separated.
        known: String,
    },
}

/// Every `Result` in this module carries `NativeConfigError` by value, so a
/// variant growing past `clippy::result_large_err`'s 128-byte threshold is a
/// lint failure rather than a review note -- and it fires per target ABI, so
/// the first host to see it can be one nobody builds on daily (msvc reads
/// `toml::de::Error` as 128+ bytes where linux-gnu does not).
const _: () = assert!(std::mem::size_of::<NativeConfigError>() <= 128);

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

    /// Every top-level table spec section 11 specifies `view.toml` to carry,
    /// in the order `view.toml.example` shows them.
    ///
    /// Hand-written, and unavoidably so: it is a transcription of the spec,
    /// which no build artifact carries. What is *not* hand-written is which
    /// of them this build reads -- see [`loaded_tables`].
    static SPECIFIED_TABLES: [&str; 5] = ["native", "ui", "engine", "supervision", "ai"];

    /// Every top-level table **this crate's** loader reads, taken from
    /// [`ViewFile`]'s own rendered shape rather than restated.
    ///
    /// Derived, so that adding a field to `ViewFile` is itself the event
    /// that makes `every_specified_table_is_documented_in_the_example` demand
    /// a live block in the example. A hand-maintained flag beside the struct
    /// would only fire when somebody remembered to flip it, which is not a
    /// forcing function at all.
    ///
    /// The reach is this crate and no further. A `[ui]` or `[ai]` table read
    /// by another crate's own struct is invisible from here, because nothing
    /// in the workspace enumerates config loaders across crate boundaries;
    /// such a loader landing without its example block going live is caught
    /// by review, not by this test.
    fn loaded_tables() -> BTreeSet<String> {
        toml::Value::try_from(ViewFile::default())
            .expect("the loader's own shape must render as TOML")
            .as_table()
            .map(|table| table.keys().cloned().collect())
            .unwrap_or_default()
    }

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
        let loaded = loaded_tables();
        for name in SPECIFIED_TABLES {
            let reads_it = loaded.contains(name);
            // one comparison for both failure modes: `None` is a table the
            // example never mentions, `Some(other)` is one whose block is
            // live when this build cannot read it, or commented out when it
            // can
            let commented = headers
                .iter()
                .find(|(header, _)| header == name)
                .map(|(_, commented)| *commented);
            assert_eq!(
                commented,
                Some(!reads_it),
                "[{name}] is {} by this build, so view.toml.example owes it a {} block",
                if reads_it { "read" } else { "not read" },
                if reads_it { "live" } else { "commented-out" }
            );
        }
    }

    #[test]
    fn the_example_documents_no_table_this_build_has_never_heard_of() {
        let known: BTreeSet<&str> = SPECIFIED_TABLES.into_iter().collect();
        for (name, _) in example_headers() {
            assert!(
                known.contains(name.as_str()),
                "view.toml.example shows [{name}], which is in no specified table"
            );
        }
    }

    #[test]
    fn every_table_this_crate_reads_is_a_specified_one() {
        // the other direction of the derive: a field added to `ViewFile`
        // under a name spec section 11 never specified is a loader for a
        // table no user was ever told about
        let specified: BTreeSet<&str> = SPECIFIED_TABLES.into_iter().collect();
        for name in loaded_tables() {
            assert!(
                specified.contains(name.as_str()),
                "this crate reads [{name}], which spec section 11 does not specify"
            );
        }
    }

    #[test]
    fn the_readmes_primary_instruction_actually_turns_the_feature_off() {
        // README's "copy the example, change `picker = true` to
        // `picker = false`", followed literally. The example's own parse is
        // pinned above; what is pinned here is that the edit a user is told
        // to make has the effect they were promised
        let edited = EXAMPLE_TOML.replace("picker = true", "picker = false");
        assert_ne!(edited, EXAMPLE_TOML, "the example must still ship the key");
        let cfg = NativeConfig::from_toml_str(&edited).expect("the edited example must parse");
        assert!(!cfg.enabled("picker"));
        for f in registry::features().iter().filter(|f| f.id != "picker") {
            assert!(cfg.enabled(f.id), "{} must stay on", f.id);
        }
    }

    #[test]
    fn the_dotted_off_switch_is_a_whole_config_on_its_own() {
        // README's alternative for a file written from scratch, and the
        // exact string the registry's `off_switch` hands to a notice
        let cfg = NativeConfig::from_toml_str("native.picker = false\n")
            .expect("the dotted form must be a legal config by itself");
        assert!(!cfg.enabled("picker"));
        assert!(cfg.enabled("tree"));
    }

    #[test]
    fn appending_the_dotted_form_under_the_example_table_is_refused() {
        // the shape the README must never recommend: appended after the
        // example's `[native]` header, the dotted key nests as
        // `native.native.picker`, whose value is a table where the loader
        // needs a bool. Refusing is what keeps a user from reading it as
        // "done" while `native.rs` falls back to all-enabled for the session
        let appended = format!("{EXAMPLE_TOML}\nnative.picker = false\n");
        let err = NativeConfig::from_toml_str(&appended)
            .expect_err("a nested [native] table is not a feature switch");
        assert!(
            matches!(err, NativeConfigError::Toml(_)),
            "expected a TOML type error, got: {err:?}"
        );
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
        let dir = view_test_support::ScratchDir::new("native-example").unwrap();
        let path = dir.join("view.toml");
        std::fs::write(&path, EXAMPLE_TOML).expect("the example must be writable");
        let loaded = NativeConfig::load(Some(&path));
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
        let dir = view_test_support::ScratchDir::new("native-broken").unwrap();
        let path = dir.join("view.toml");
        std::fs::write(&path, "[native\npicker = false\n").expect("a temp config must be writable");
        let err = NativeConfig::load(Some(&path)).expect_err("a broken config must not resolve");
        let msg = format!("{err}");
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
    #[test]
    fn an_absent_supervision_table_recovers_automatically() {
        assert!(ViewConfig::defaults().supervision.auto_restart);
        let cfg = ViewConfig::from_toml_str("[native]\npicker = false\n")
            .expect("a file with no supervision table must parse");
        assert!(
            cfg.supervision.auto_restart,
            "an unstated switch must keep the derived default, not bool's"
        );
        assert!(
            !cfg.native.enabled("picker"),
            "the other table still resolves"
        );
    }

    #[test]
    fn supervision_auto_restart_can_be_turned_off() {
        let cfg = ViewConfig::from_toml_str("[supervision]\nauto_restart = false\n")
            .expect("the documented switch must parse");
        assert!(!cfg.supervision.auto_restart);
        assert_eq!(
            cfg.native,
            NativeConfig::all_enabled(),
            "turning off automatic recovery must not touch any native feature"
        );
    }

    #[test]
    fn a_misspelled_supervision_key_is_refused_rather_than_ignored() {
        let err = ViewConfig::from_toml_str("[supervision]\nauto_restrat = false\n")
            .expect_err("a typo that silently keeps the default is unreadable to a user");
        assert!(
            format!("{err}").contains("auto_restrat"),
            "the error must name the offending key, got: {err}"
        );
    }

    #[test]
    fn a_non_bool_auto_restart_is_an_error() {
        let err = ViewConfig::from_toml_str("[supervision]\nauto_restart = \"yes\"\n")
            .expect_err("only a boolean switches automatic recovery");
        assert!(
            matches!(err, NativeConfigError::Toml(_)),
            "expected a TOML type error, got: {err:?}"
        );
    }

    #[test]
    fn the_example_configs_supervision_block_is_the_shipped_default() {
        let cfg = ViewConfig::from_toml_str(EXAMPLE_TOML).expect("the example must parse");
        assert_eq!(cfg.supervision, SupervisionConfig::default());
    }

    #[test]
    fn the_examples_supervision_switch_actually_turns_the_automatic_half_off() {
        // the example, edited the way its own comment tells a user to
        let edited = EXAMPLE_TOML.replace("auto_restart = true", "auto_restart = false");
        assert_ne!(edited, EXAMPLE_TOML, "the example must still ship the key");
        let cfg = ViewConfig::from_toml_str(&edited).expect("the edited example must parse");
        assert!(!cfg.supervision.auto_restart);
    }

    #[test]
    fn one_read_answers_every_table_from_disk() {
        let dir = view_test_support::ScratchDir::new("view-config-both").unwrap();
        let path = dir.join("view.toml");
        std::fs::write(
            &path,
            "[native]\ntree = false\n\n[supervision]\nauto_restart = false\n",
        )
        .expect("a temp config must be writable");
        let cfg = ViewConfig::load(Some(&path)).expect("both tables must load together");
        assert!(!cfg.native.enabled("tree"));
        assert!(!cfg.supervision.auto_restart);
    }

    #[test]
    fn no_path_and_no_file_are_both_every_default() {
        assert_eq!(
            ViewConfig::load(None).expect("no path must resolve"),
            ViewConfig::defaults()
        );
        let missing = Path::new(env!("CARGO_MANIFEST_DIR")).join("no-such-view.toml");
        assert_eq!(
            ViewConfig::load(Some(&missing)).expect("an absent file must resolve"),
            ViewConfig::defaults()
        );
    }
}
