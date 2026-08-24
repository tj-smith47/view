//! The tables of `view.toml` this build reads: `[native]`, which native
//! features a user has turned off, `[supervision]`, how far view may go
//! on its own to recover a failed engine, and `[keys]`, which keys resize
//! the sidebars.
//!
//! An absent or empty file is the full experience, so every resolution path
//! that finds nothing to read answers `all_enabled()` rather than failing.
//! The key set of `[native]` is the feature registry itself, never a second
//! list written out here, so a feature can never exist in the table and be
//! unspellable in config.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use view_core::native::geometry;
use view_core::native::keys::{Action, Direction, KeyBindings};
use view_core::native::registry;

/// A resolved on/off answer for every feature in the registry.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeConfig {
    disabled: Vec<&'static str>,
    tree_width: u16,
    tree_width_notice: Option<&'static str>,
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
    native: NativeTable,
    #[serde(default)]
    supervision: SupervisionTable,
    #[serde(default)]
    keys: KeysTable,
}

/// The `[native]` table's wire shape: the feature switches, whose key set is
/// the registry itself, plus the one key in that table that is not a
/// feature.
///
/// [`Deserialize`] is hand-written rather than derived, and the reason is
/// the error a user reads. A derive would need `#[serde(flatten)]` to hold
/// a key set the registry owns, and `flatten` buffers the whole table
/// through serde's `Content` before typing any value -- which costs every
/// switch in the table its line and column: `notifications = "off"` stops
/// pointing at the value and starts pointing at the `[native]` header,
/// leaving a user to bisect their own file. Reading the map key by key
/// hands each value straight to `toml`'s own deserializer, which spans it.
///
/// `deny_unknown_fields` has no equivalent here and needs none: a key that
/// spells no feature is refused by [`NativeConfig::from_parsed`]'s registry
/// check (bool-valued) or by the visitor below (anything else), and both
/// name the key.
#[derive(Debug, Serialize)]
struct NativeTable {
    tree_width: u16,
    /// Never part of the file's own shape -- a resolution detail this
    /// struct carries to its reader -- so it stays out of the rendered
    /// TOML the example's drift guard reads.
    #[serde(skip_serializing)]
    tree_width_notice: Option<&'static str>,
    #[serde(flatten)]
    features: BTreeMap<String, bool>,
}

impl Default for NativeTable {
    fn default() -> Self {
        Self {
            tree_width: geometry::DEFAULT_PANEL_WIDTH_PCT,
            tree_width_notice: None,
            features: BTreeMap::new(),
        }
    }
}

/// The one `[native]` key that is not a feature switch.
const TREE_WIDTH_KEY: &str = "tree_width";

/// What a `tree_width` that is not a whole number is answered with.
const TREE_WIDTH_NOTICE: &str = "view: [native] tree_width must be a whole number of percent -- \
     the file tree opens at its default width this run";

/// The width the tree opens at, and the notice a value that is not a whole
/// number owes the user.
///
/// A width never fails the table, for the reason `[ai] panel_width`'s own
/// resolution states: a broken `[native]` reverts every feature switch in
/// the file to its default for the run, which is far more than a mistyped
/// percentage asked for. An integer resolves clamped however far outside
/// the range it is written; anything else opens at the default and says
/// so.
fn resolve_tree_width(value: &toml::Value) -> (u16, Option<&'static str>) {
    match value.as_integer() {
        Some(pct) => (geometry::clamp_panel_width(pct), None),
        None => (geometry::DEFAULT_PANEL_WIDTH_PCT, Some(TREE_WIDTH_NOTICE)),
    }
}

impl<'de> Deserialize<'de> for NativeTable {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(NativeTableVisitor)
    }
}

struct NativeTableVisitor;

impl<'de> serde::de::Visitor<'de> for NativeTableVisitor {
    type Value = NativeTable;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a table of feature switches, each a boolean, optionally with tree_width")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let mut table = NativeTable::default();
        while let Some(key) = map.next_key::<String>()? {
            if key == TREE_WIDTH_KEY {
                let (width, notice) = resolve_tree_width(&map.next_value::<toml::Value>()?);
                table.tree_width = width;
                table.tree_width_notice = notice;
                continue;
            }
            match map.next_value::<bool>() {
                Ok(on) => {
                    table.features.insert(key, on);
                }
                // A registry name given something other than a switch keeps
                // serde's own error, spanned to the offending value.
                Err(e) if registry::is_feature(&key) => return Err(e),
                // A name the registry never heard of is a spelling mistake,
                // not a type mistake, and its value's span points at the
                // half the user did not get wrong -- so this one names the
                // key instead, the way the bool-valued case already does
                // through `from_parsed`. `tree_widht = 30` is the shape
                // this exists for.
                Err(_) => {
                    return Err(serde::de::Error::custom(format!(
                        "[native] {key} names no feature and is not {TREE_WIDTH_KEY}; \
                         expected a boolean for one of: {}",
                        known_ids()
                    )))
                }
            }
        }
        Ok(table)
    }
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

/// The `[keys]` table's wire shape: one entry per rebindable action, each
/// either a key notation or a list of them. Unknown keys are refused rather
/// than ignored, for the reason `[supervision]`'s own check states.
///
/// The values stay untyped because a spelling this build cannot match must
/// not fail the table (see [`resolve_key_bindings`]), and a typed field
/// would make one a parse error instead.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct KeysTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sidebar_wider: Option<toml::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sidebar_narrower: Option<toml::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    composer_newline: Option<toml::Value>,
}

/// What a `sidebar_wider` naming no key this build can match is answered
/// with; `sidebar_narrower` has its own so the notice names the action the
/// user has to go fix.
const SIDEBAR_WIDER_NOTICE: &str =
    "view: [keys] sidebar_wider must be key notations spelled as nvim spells them, case \
     included (\"<C-w>>\", \"<S-Right>\"), at most two keys each -- the sidebars widen on \
     their default keys this run";

/// The narrowing half of [`SIDEBAR_WIDER_NOTICE`].
const SIDEBAR_NARROWER_NOTICE: &str =
    "view: [keys] sidebar_narrower must be key notations spelled as nvim spells them, case \
     included (\"<C-w><\", \"<S-Left>\"), at most two keys each -- the sidebars narrow on \
     their default keys this run";

/// The composer's own, on the same terms as [`SIDEBAR_WIDER_NOTICE`].
const COMPOSER_NEWLINE_NOTICE: &str =
    "view: [keys] composer_newline must be key notations spelled as nvim spells them, case \
     included (\"<S-CR>\", \"<M-CR>\"), at most two keys each -- the composer breaks a line \
     on its default keys this run";

/// The bindings every rebindable action answers to, and the notice each
/// action whose value could not be read owes the user.
///
/// A key never fails the table, for the reason `[native] tree_width`'s own
/// resolution states, and each action falls back on its own: a mistyped
/// `sidebar_wider` leaves a perfectly good `sidebar_narrower` alone rather
/// than reverting both to defaults the user asked to replace.
fn resolve_key_bindings(table: &KeysTable) -> (KeyBindings, Vec<&'static str>) {
    let mut keys = KeyBindings::default();
    let mut notices = Vec::new();
    for (value, action, notice) in [
        (
            &table.sidebar_wider,
            Action::Resize(Direction::Wider),
            SIDEBAR_WIDER_NOTICE,
        ),
        (
            &table.sidebar_narrower,
            Action::Resize(Direction::Narrower),
            SIDEBAR_NARROWER_NOTICE,
        ),
        (
            &table.composer_newline,
            Action::ComposerNewline,
            COMPOSER_NEWLINE_NOTICE,
        ),
    ] {
        let Some(value) = value.as_ref() else {
            continue;
        };
        let spellings = match value {
            toml::Value::String(one) => Some(vec![one.clone()]),
            toml::Value::Array(many) => many
                .iter()
                .map(|entry| entry.as_str().map(str::to_string))
                .collect::<Option<Vec<String>>>(),
            _ => None,
        };
        if !spellings.is_some_and(|spellings| keys.rebind(action, &spellings)) {
            notices.push(notice);
        }
    }
    (keys, notices)
}

/// Which keys perform which action.
#[must_use]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeysConfig {
    bindings: KeyBindings,
    notices: Vec<&'static str>,
}

impl KeysConfig {
    /// The keys each action answers to, defaults included for every action
    /// `[keys]` left alone.
    #[must_use]
    pub fn bindings(&self) -> &KeyBindings {
        &self.bindings
    }

    /// One notice per action whose value named no key, or empty when the
    /// table was absent or usable. See [`resolve_key_bindings`] for why such
    /// a value is a notice rather than the parse error it looks like.
    #[must_use]
    pub fn notices(&self) -> &[&'static str] {
        &self.notices
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
    /// The `[keys]` table's resolved answers.
    pub keys: KeysConfig,
}

impl ViewConfig {
    /// Every default: the config-absent answer for every table.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            native: NativeConfig::all_enabled(),
            supervision: SupervisionConfig::default(),
            keys: KeysConfig::default(),
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
        let (bindings, notices) = resolve_key_bindings(&file.keys);
        Ok(Self {
            native: NativeConfig::from_parsed(&file)?,
            supervision: SupervisionConfig {
                auto_restart: file.supervision.auto_restart,
            },
            keys: KeysConfig { bindings, notices },
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
            tree_width: geometry::DEFAULT_PANEL_WIDTH_PCT,
            tree_width_notice: None,
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
        for key in file.native.features.keys() {
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
            .filter(|f| !file.native.features.get(f.id).copied().unwrap_or(true))
            .map(|f| f.id)
            .collect();
        Ok(Self {
            disabled,
            // already resolved and clamped where the value was read, so the
            // number here is the number the tree opens at
            tree_width: file.native.tree_width,
            tree_width_notice: file.native.tree_width_notice,
        })
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
    /// feature, and is not
    /// [`view_core::native::mappings::is_reachable_feature`]'s one other
    /// case -- a feature whose own enabled state lives outside `[native]`
    /// entirely, so `disabled` structurally can never carry a switch for it
    /// -- either is not enabled: nothing in the build can act on it.
    ///
    /// The second case matters for `register_plan` ([`crate::mappings`]):
    /// such a feature's default key must still be registered from here, or
    /// `is_feature` alone would read the structural absence of a `[native]`
    /// switch as "disabled" and silently drop a key `default_maps` and
    /// `:View`'s own completion both still advertise as live. Whether that
    /// feature's key is *itself* also gated on its own real enabled bit is a
    /// question this method has no way to answer -- it knows only that
    /// `[native]` is not where that bit lives.
    #[must_use]
    pub fn enabled(&self, id: &str) -> bool {
        view_core::native::mappings::is_reachable_feature(id) && !self.disabled.contains(&id)
    }

    /// The share of the terminal width the tree sidebar opens at, in
    /// percent, already clamped to the range the resize keys work in
    /// ([`geometry::clamp_panel_width`]) -- a `view.toml` asking for 5, 95
    /// or -5 opens at the nearest end rather than refusing to start the
    /// editor.
    #[must_use]
    pub fn tree_width(&self) -> u16 {
        self.tree_width
    }

    /// What a `tree_width` that is not a whole number owes the user, or
    /// `None` when the key was absent or usable. See [`resolve_tree_width`]
    /// for why such a value is a notice rather than the parse error it
    /// looks like.
    #[must_use]
    pub fn tree_width_notice(&self) -> Option<&'static str> {
        self.tree_width_notice
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
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::collections::BTreeSet;

    use super::*;
    use view_core::native::keys::Resolved;

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
    static SPECIFIED_TABLES: [&str; 7] = [
        "native",
        "keys",
        "ui",
        "engine",
        "supervision",
        "ai",
        "ai.review",
    ];

    /// Tables `SPECIFIED_TABLES` names that this crate's own `ViewFile`
    /// never reads, by design, but that a sibling crate's own loader does.
    /// `every_specified_table_is_documented_in_the_example` folds this into
    /// `reads_it` so the shipped example can honestly go live the moment a
    /// real reader exists anywhere in the workspace, not only inside this
    /// crate. Without it, `reads_it` silently means "read by view-native"
    /// rather than "read by this build". A table lands here in the same
    /// commit that gives it a sibling reader, never a step ahead of that
    /// reader landing -- the same atomicity the fs capability flags are
    /// held to.
    static SIBLING_READ_TABLES: [&str; 2] = ["ai", "ai.review"];

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
        let in_example: BTreeSet<&str> = file.native.features.keys().map(String::as_str).collect();
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
            let reads_it = loaded.contains(name) || SIBLING_READ_TABLES.contains(&name);
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
        // the shape the README must never recommend: appended inside the
        // example's `[native]` block itself, the dotted key nests as
        // `native.native.picker`, whose value is a table where the loader
        // needs a bool. Refusing is what keeps a user from reading it as
        // "done" while `native.rs` falls back to all-enabled for the
        // session. Spliced in right after `[native]`'s own last key rather
        // than appended at the file's end: which table a trailing dotted
        // key nests under depends on whichever `[table]` header the file
        // happened to open last, and that is not `[native]`'s own concern
        // to pin.
        let appended = EXAMPLE_TOML.replacen(
            "palette = true\n",
            "palette = true\nnative.picker = false\n",
            1,
        );
        let err = NativeConfig::from_toml_str(&appended)
            .expect_err("a nested [native] table is not a feature switch");
        assert!(
            matches!(err, NativeConfigError::Toml(_)),
            "expected a TOML type error, got: {err:?}"
        );
        // `matches!` alone cannot tell this error from any other TOML
        // error, which is exactly how this test's predecessor passed for
        // the wrong reason: pin the message so a future reordering that
        // changes which table the trailing key lands in cannot go unnoticed
        assert!(
            err.to_string().contains("expected a boolean"),
            "the refusal must be the nested-table type error, got: {err}"
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
    fn an_array_agent_form_in_ai_is_ignored_by_native() {
        // `[ai]`'s `agent` key takes either a string or an array of
        // strings (view-ai's `AgentSpec`); `ViewFile` never grows a field
        // for `[ai]` at all, so the array shape must be exactly as
        // invisible to this loader as the string shape already is
        let cfg = NativeConfig::from_toml_str(
            "[native]\npicker = false\n\n[ai]\nenabled = true\nagent = [\"mycli\", \"--acp\"]\n",
        )
        .expect("[ai]'s array-form agent must not fail the native loader");
        assert!(!cfg.enabled("picker"));
        assert!(cfg.enabled("tree"));
    }

    #[test]
    fn tree_width_round_trips_beside_the_feature_switches() {
        let cfg = NativeConfig::from_toml_str("[native]\npicker = false\ntree_width = 45\n")
            .expect("a width and a switch must share the table");
        assert_eq!(cfg.tree_width(), 45);
        assert!(!cfg.enabled("picker"), "the switch beside it still works");
        assert!(cfg.enabled("tree"));
        assert_eq!(
            NativeConfig::from_toml_str("[native]\ntree = true\n")
                .expect("a table with no width must parse")
                .tree_width(),
            geometry::DEFAULT_PANEL_WIDTH_PCT
        );
    }

    #[test]
    fn a_tree_width_outside_the_range_is_clamped_rather_than_refused() {
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
            let cfg = NativeConfig::from_toml_str(&format!("[native]\ntree_width = {written}\n"))
                .expect("an out-of-range width must not keep the editor from starting");
            assert_eq!(cfg.tree_width(), resolved, "tree_width = {written}");
            assert_eq!(cfg.tree_width_notice(), None, "tree_width = {written}");
        }
    }

    /// A width that cannot be read as a number must not revert every
    /// feature switch in the file to its default for the run, which is what
    /// failing the table does.
    #[test]
    fn a_tree_width_that_is_not_a_number_keeps_the_table_and_says_so() {
        for written in ["\"wide\"", "3.5", "true", "[30]", "{ pct = 30 }"] {
            let cfg = NativeConfig::from_toml_str(&format!(
                "[native]\npicker = false\ntree_width = {written}\n"
            ))
            .unwrap_or_else(|e| panic!("tree_width = {written} must not fail the table: {e}"));
            assert!(
                !cfg.enabled("picker"),
                "tree_width = {written} must not revert the switches beside it"
            );
            assert_eq!(cfg.tree_width(), geometry::DEFAULT_PANEL_WIDTH_PCT);
            let notice = cfg
                .tree_width_notice()
                .unwrap_or_else(|| panic!("tree_width = {written} owes the user a notice"));
            assert!(
                notice.contains("tree_width"),
                "the notice must name the key: {notice}"
            );
        }
    }

    /// The span a user needs to find the mistake in a forty-line file. The
    /// switches are read key by key rather than through `#[serde(flatten)]`
    /// precisely so this points at the value and not at the table header.
    #[test]
    fn a_non_bool_switch_is_reported_at_the_line_and_column_it_sits_on() {
        let err = NativeConfig::from_toml_str("[native]\npicker = true\nnotifications = \"off\"\n")
            .expect_err("only booleans switch a feature");
        let msg = format!("{err}");
        assert!(
            msg.contains("line 3") && msg.contains("column 17"),
            "the error must point at the value, not at [native]: {msg}"
        );
        assert!(msg.contains("expected a boolean"), "{msg}");
    }

    /// A misspelled `tree_width` reads as a feature switch given a number,
    /// so the message names the key rather than leaving the reader with a
    /// type error about a boolean they never mentioned.
    #[test]
    fn a_misspelled_width_key_is_refused_by_name() {
        let err = NativeConfig::from_toml_str("[native]\ntree_widht = 30\n")
            .expect_err("a key that is neither a feature nor tree_width must be refused");
        let msg = format!("{err}");
        assert!(
            msg.contains("tree_widht"),
            "the error must name the key: {msg}"
        );
        for f in registry::features() {
            assert!(msg.contains(f.id), "{} missing from the error: {msg}", f.id);
        }
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
    fn an_absent_keys_table_is_the_shipped_defaults() {
        let cfg = ViewConfig::from_toml_str("[native]\npicker = false\n")
            .expect("a file with no keys table must parse");
        assert_eq!(cfg.keys, KeysConfig::default());
        assert!(cfg.keys.notices().is_empty());
        assert_eq!(
            cfg.keys.bindings().resolve(None, "<S-Right>"),
            Some(Resolved::Act(Action::Resize(Direction::Wider))),
            "the shifted arrow still widens"
        );
        assert_eq!(
            cfg.keys.bindings().resolve(Some("<C-w>"), ">"),
            Some(Resolved::Act(Action::Resize(Direction::Wider))),
            "and so does nvim's own chord"
        );
    }

    #[test]
    fn one_rebound_action_leaves_the_other_on_its_defaults() {
        let cfg = ViewConfig::from_toml_str("[keys]\nsidebar_wider = \"<M-.>\"\n")
            .expect("a single notation must parse");
        assert!(cfg.keys.notices().is_empty());
        let keys = cfg.keys.bindings();
        assert_eq!(
            keys.resolve(None, "<M-.>"),
            Some(Resolved::Act(Action::Resize(Direction::Wider)))
        );
        assert_eq!(keys.resolve(None, "<S-Right>"), None, "replaced, not added");
        assert_eq!(
            keys.resolve(None, "<S-Left>"),
            Some(Resolved::Act(Action::Resize(Direction::Narrower))),
            "the action nobody named keeps its defaults"
        );
    }

    /// Every action `[keys]` carries, and the default key each one answers
    /// to. Walked by the tests below rather than named one at a time, so an
    /// action added to [`KeysTable`] without a row here fails the
    /// crosscheck instead of shipping untested.
    const KEYS_ACTIONS: [(&str, &str); 3] = [
        ("sidebar_wider", "<S-Right>"),
        ("sidebar_narrower", "<S-Left>"),
        ("composer_newline", "<M-CR>"),
    ];

    /// The population the walk above has to cover: every field of the
    /// `[keys]` table, read off the serialized shape rather than restated.
    #[test]
    fn the_walked_actions_are_exactly_the_keys_table() {
        let all = toml::Value::try_from(KeysTable {
            sidebar_wider: Some("<S-Right>".into()),
            sidebar_narrower: Some("<S-Left>".into()),
            composer_newline: Some("<M-CR>".into()),
        })
        .expect("the keys table serializes");
        let fields: BTreeSet<&str> = all
            .as_table()
            .map(|t| t.keys().map(String::as_str).collect())
            .unwrap_or_default();
        let walked: BTreeSet<&str> = KEYS_ACTIONS.iter().map(|(field, _)| *field).collect();
        assert_eq!(
            fields, walked,
            "every [keys] action must be walked by the tests below"
        );

        let example: ViewFile = toml::from_str(EXAMPLE_TOML).expect("the example parses");
        let shown = toml::Value::try_from(example.keys).expect("the keys table serializes");
        let shown: BTreeSet<&str> = shown
            .as_table()
            .map(|t| t.keys().map(String::as_str).collect())
            .unwrap_or_default();
        assert_eq!(
            shown, walked,
            "and every one of them is shown in view.toml.example"
        );
    }

    /// A key that names nothing must not revert the feature switches in the
    /// file, nor the actions beside it, which is what failing the table
    /// does. Every action, so the fallback is the table's rule and not one
    /// action's.
    #[test]
    fn a_key_this_build_cannot_match_keeps_the_table_and_says_so() {
        for (field, _) in KEYS_ACTIONS {
            for written in [
                "30",
                "true",
                "[\"<C-w>\", 3]",
                "{ key = \"<C-w>>\" }",
                "\"abc\"",
            ] {
                let cfg = ViewConfig::from_toml_str(&format!(
                    "[native]\npicker = false\n\n[keys]\n{field} = {written}\n"
                ))
                .unwrap_or_else(|e| panic!("{field} = {written} must not fail the table: {e}"));
                assert!(
                    !cfg.native.enabled("picker"),
                    "{field} = {written} must not revert the switches beside it"
                );
                for (other, default_key) in KEYS_ACTIONS {
                    assert!(
                        cfg.keys.bindings().resolve(None, default_key).is_some(),
                        "{field} = {written} left {other} without its default {default_key}"
                    );
                }
                let notices = cfg.keys.notices();
                assert_eq!(notices.len(), 1, "{field} = {written}: {notices:?}");
                assert!(
                    notices[0].contains(field),
                    "the notice must name the key: {}",
                    notices[0]
                );
            }
        }
    }

    /// The rebind path over the same population: each action takes the key
    /// its own entry names and leaves every other action alone.
    #[test]
    fn every_action_rebinds_on_its_own() {
        for (field, _) in KEYS_ACTIONS {
            let cfg = ViewConfig::from_toml_str(&format!("[keys]\n{field} = \"<M-.>\"\n"))
                .unwrap_or_else(|e| panic!("{field} = \"<M-.>\" must parse: {e}"));
            assert!(
                cfg.keys.notices().is_empty(),
                "{field}: {:?}",
                cfg.keys.notices()
            );
            assert!(
                cfg.keys.bindings().resolve(None, "<M-.>").is_some(),
                "{field} does not answer the key it was given"
            );
            for (other, default_key) in KEYS_ACTIONS {
                assert_eq!(
                    cfg.keys.bindings().resolve(None, default_key).is_some(),
                    other != field,
                    "{field} rebound: {other}'s default {default_key} is wrong"
                );
            }
        }
    }

    #[test]
    fn a_misspelled_keys_entry_is_refused_by_name() {
        let err = ViewConfig::from_toml_str("[keys]\nsidebar_widr = \"<C-w>>\"\n")
            .expect_err("a key that names no action must be refused");
        assert!(
            format!("{err}").contains("sidebar_widr"),
            "the error must name the offending key, got: {err}"
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

    /// The example spells both defaults out rather than leaving the table
    /// empty, so a user copying it keeps exactly what an untouched build
    /// gives them -- including the bare `<` the encoder itself spells
    /// `<lt>`.
    #[test]
    fn the_example_configs_keys_block_is_the_shipped_default() {
        let cfg = ViewConfig::from_toml_str(EXAMPLE_TOML).expect("the example must parse");
        assert_eq!(cfg.keys, KeysConfig::default());
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
