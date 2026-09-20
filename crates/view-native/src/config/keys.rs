//! The key registry: every `view.toml` key any crate in this build reads,
//! as plain data.
//!
//! Four mechanisms need the same list -- the file parse, the environment
//! lookup, the flag mapping, and the doctor's provenance row -- and four
//! hand-maintained copies is the drift `[native]` already refuses by
//! resolving against the feature registry rather than against the file.
//! The `[native]` rows here are that same registry walked again, so a
//! feature this build ships can never be missing an environment name.

use std::sync::OnceLock;

use view_core::native::registry;

/// One config key: where it lives in the file, how a user overrides it,
/// and what an absent value means.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigKey {
    /// The `[table]` the key lives under.
    pub table: &'static str,
    /// The key's own name inside that table.
    pub key: &'static str,
    /// The long flag that overrides this key, when it has one. Most keys
    /// do not: `[native]`'s switches are reached through `--clean` as a
    /// whole table, and a flag per feature is surface with no caller.
    pub flag: Option<&'static str>,
    /// What an absent value resolves to, stated as the text a user would
    /// have written to get the same answer. `None` means the default is
    /// computed outside this crate, and the doctor prints the computed
    /// value beside the word derived.
    pub derived: Option<&'static str>,
}

/// Every key a user may set in `view.toml`, grouped by table:
/// `[ui]`, `[ui.surfaces.<surface>]`, `[engine]`, `[native]`, `[keys]`,
/// `[supervision]`, `[ai]`, `[ai.review]`.
///
/// The scope is the shipped example's own: every key that file documents
/// has a row here, and a row here is a key that file documents
/// (`the_registry_and_the_example_document_the_same_keys` compares the two
/// sets, so neither can grow alone).
///
/// The split between the two resolvers is a contract rather than an
/// accident: the rows whose table this crate parses are answered by
/// [`super::ResolvedConfig::rows`], and the `[ai]`/`[ai.review]` rows are
/// metadata only -- that table is parsed and resolved by the crate that
/// owns it, and the bin holds the two to each other, because `view-native`
/// and `view-ai` may not name each other's types.
#[must_use]
pub fn keys() -> &'static [ConfigKey] {
    static KEYS: OnceLock<Vec<ConfigKey>> = OnceLock::new();
    KEYS.get_or_init(|| {
        let mut rows = vec![
            ConfigKey {
                table: "ui",
                key: "tier",
                flag: Some("--tier"),
                derived: Some(super::AUTO),
            },
            ConfigKey {
                table: "ui",
                key: "theme",
                flag: Some("--theme"),
                derived: Some(super::AUTO),
            },
            ConfigKey {
                table: "ui",
                key: "panes",
                flag: Some("--panes"),
                // no text a user could write names the answer this key
                // derives: `auto` is the absence of a choice, and what it
                // resolves to is whatever the session's own environment
                // says, which is the value the report prints beside the
                // marker that decided it
                derived: None,
            },
            ConfigKey {
                table: "ui",
                key: "gaps",
                flag: None,
                derived: Some("true"),
            },
            ConfigKey {
                table: "ui.tokens",
                key: "accent",
                flag: None,
                derived: Some(super::AUTO),
            },
            ConfigKey {
                table: "ui.surfaces.tree",
                key: "placement",
                flag: None,
                derived: Some("overlay"),
            },
            ConfigKey {
                table: "ui.surfaces.tree",
                key: "anchor",
                flag: None,
                derived: Some("left"),
            },
            ConfigKey {
                table: "ui.surfaces.tree",
                key: "size",
                flag: None,
                derived: Some("30"),
            },
            ConfigKey {
                table: "engine",
                key: "nvim_bin",
                flag: Some("--nvim-bin"),
                derived: Some(super::BUNDLED),
            },
            ConfigKey {
                table: "engine",
                key: "appname",
                flag: Some("--appname"),
                // no text a user could write means "whatever NVIM_APPNAME
                // already names": an empty string is a profile named "",
                // not the inherited one
                derived: None,
            },
            ConfigKey {
                table: "engine",
                key: "single_grid",
                flag: Some("--single-grid"),
                derived: Some("false"),
            },
        ];
        // the feature registry walked rather than transcribed, the same
        // direction `[native]`'s own resolution walks: a feature this build
        // ships is a config key by construction
        rows.extend(registry::features().iter().map(|feature| ConfigKey {
            table: "native",
            key: feature.id,
            flag: None,
            // the pill's switch follows `[ui] panes`, so no text a user
            // could write names its absent answer: what it derives to
            // depends on the look this session resolved
            derived: if feature.id == "tabline" {
                None
            } else {
                Some(if feature.default_on { "true" } else { "false" })
            },
        }));
        rows.extend([
            ConfigKey {
                table: "native",
                key: "tree_width",
                flag: None,
                derived: Some("30"),
            },
            ConfigKey {
                table: "native",
                key: "tabline_shows",
                flag: None,
                derived: Some("tabs"),
            },
            ConfigKey {
                table: "keys",
                key: "sidebar_wider",
                flag: None,
                derived: Some("<S-Right>, <C-w>>"),
            },
            ConfigKey {
                table: "keys",
                key: "sidebar_narrower",
                flag: None,
                derived: Some("<S-Left>, <C-w><lt>"),
            },
            ConfigKey {
                table: "keys",
                key: "composer_newline",
                flag: None,
                derived: Some("<S-CR>, <M-CR>"),
            },
            ConfigKey {
                table: "supervision",
                key: "auto_restart",
                flag: None,
                derived: Some("true"),
            },
            ConfigKey {
                table: "ai",
                key: "enabled",
                flag: None,
                derived: Some("true"),
            },
            ConfigKey {
                table: "ai",
                key: "agent",
                flag: None,
                derived: Some("claude-code"),
            },
            ConfigKey {
                table: "ai",
                key: "panel_width",
                flag: None,
                derived: Some("30"),
            },
            ConfigKey {
                table: "ai.review",
                key: "open_target",
                flag: None,
                derived: Some("current"),
            },
        ]);
        rows
    })
}

/// The environment variable name for a key, derived from its own path so
/// the two can never disagree: `[native] picker` is `VIEW_NATIVE_PICKER`.
///
/// A nested table nests the same way the file spells it, with the dot that
/// separates its segments written as the underscore that separates every
/// other segment of the name: `[ai.review] open_target` is
/// `VIEW_AI_REVIEW_OPEN_TARGET`, exactly as if the table were spelled
/// `[ai_review]`. Nesting deeper adds a segment and nothing else.
///
/// The `VIEW_*` namespace carries members that are not config at all
/// (`VIEW_LOG`, and the harness's own names), so a resolver claims exactly
/// the names this function generates and reads no other.
#[must_use]
pub fn env_name(key: &ConfigKey) -> String {
    format!(
        "VIEW_{}_{}",
        key.table.replace('.', "_").to_uppercase(),
        key.key.to_uppercase()
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::collections::BTreeSet;

    use super::*;

    /// The shipped example, embedded for the reason the loader's own copy
    /// of this constant states: a moved example is a build failure rather
    /// than a runtime read error.
    const EXAMPLE_TOML: &str = include_str!("../../../../view.toml.example");

    /// Every `table.key` pair the shipped example documents, live blocks and
    /// commented-out ones alike -- a key a user is shown is a key a user
    /// will set, whether or not this build reads its table yet.
    ///
    /// A commented line is read exactly like a live one after its leading
    /// `#` comes off, which is what lets the `[ui]`/`[engine]` blocks count.
    /// Prose in the same comment column is told apart by shape rather than
    /// by position: only a line whose whole text before the first `=` is one
    /// bare identifier is a key, so `agent = ["mycli", "--acp"]` inside a
    /// sentence stays prose.
    fn example_keys() -> BTreeSet<(String, String)> {
        let mut table = String::new();
        let mut found = BTreeSet::new();
        for line in EXAMPLE_TOML.lines() {
            let trimmed = line.trim();
            let body = trimmed
                .strip_prefix('#')
                .map_or(trimmed, |rest| rest.trim_start());
            if let Some(name) = body.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
                table = name.to_string();
                continue;
            }
            let Some((key, _)) = body.split_once('=') else {
                continue;
            };
            let key = key.trim();
            if table.is_empty() || key.is_empty() {
                continue;
            }
            if key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                found.insert((table.clone(), key.to_string()));
            }
        }
        found
    }

    /// The registry and the shipped example are the same set of keys, in
    /// both directions and without a count written down anywhere: a key
    /// documented for a user with no row here has no environment name and no
    /// provenance row, and a row here the example never shows is a knob
    /// nobody can find.
    #[test]
    fn the_registry_and_the_example_document_the_same_keys() {
        let registered: BTreeSet<(String, String)> = keys()
            .iter()
            .map(|row| (row.table.to_string(), row.key.to_string()))
            .collect();
        assert_eq!(
            registered,
            example_keys(),
            "the registry and view.toml.example must document the same keys"
        );
    }

    #[test]
    fn the_rows_are_grouped_by_table() {
        let mut tables: Vec<&str> = keys().iter().map(|row| row.table).collect();
        tables.dedup();
        let mut once = tables.clone();
        once.sort_unstable();
        once.dedup();
        assert_eq!(
            tables.len(),
            once.len(),
            "a table's rows must be contiguous: {tables:?}"
        );
        assert_eq!(
            tables,
            vec![
                "ui",
                "ui.tokens",
                "ui.surfaces.tree",
                "engine",
                "native",
                "keys",
                "supervision",
                "ai",
                "ai.review"
            ],
            "the flag-bearing tables lead, then the file's own, then the sibling crate's"
        );
    }

    #[test]
    fn the_native_rows_are_the_feature_registry_itself() {
        let registered: Vec<&str> = keys()
            .iter()
            .filter(|row| row.table == "native")
            .map(|row| row.key)
            .collect();
        // the switches walked from the registry, then the `[native]` keys
        // that are not switches -- so a feature this build ships is a
        // config key by construction, and nothing else can slip in beside
        // them unnoticed
        let mut expected: Vec<&str> = registry::features().iter().map(|f| f.id).collect();
        expected.extend(["tree_width", "tabline_shows"]);
        assert_eq!(
            registered, expected,
            "a feature this build ships is a config key, in the registry's own order"
        );
    }

    #[test]
    fn env_name_is_derived_from_the_key_path() {
        let mut names = BTreeSet::new();
        for row in keys() {
            let name = env_name(row);
            assert!(
                names.insert(name.clone()),
                "{name} names two different keys"
            );
            assert!(name.starts_with("VIEW_"), "{name} is outside the namespace");
            assert!(
                name.contains(&row.table.replace('.', "_").to_uppercase())
                    && name.contains(&row.key.to_uppercase()),
                "{name} does not spell {}.{}",
                row.table,
                row.key
            );
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit()),
                "{name} is not an environment variable name"
            );
        }
        // the names spelled out elsewhere -- the plan's three, and the
        // nested table, whose dot is the one part of the derivation a shape
        // check cannot pin
        let spelled: Vec<String> = keys().iter().map(env_name).collect();
        for expected in [
            "VIEW_UI_TIER",
            "VIEW_NATIVE_PICKER",
            "VIEW_NATIVE_TREE_WIDTH",
            "VIEW_KEYS_SIDEBAR_WIDER",
            "VIEW_SUPERVISION_AUTO_RESTART",
            "VIEW_AI_PANEL_WIDTH",
            "VIEW_AI_REVIEW_OPEN_TARGET",
        ] {
            assert!(
                spelled.iter().any(|name| name == expected),
                "{expected} is not generated by any row: {spelled:?}"
            );
        }
    }

    #[test]
    fn only_the_keys_a_user_reaches_for_mid_session_carry_a_flag() {
        let flagged: Vec<Option<&str>> = keys().iter().map(|row| row.flag).collect();
        assert_eq!(
            flagged.iter().filter(|flag| flag.is_some()).count(),
            6,
            "six flags, and a row without one is a stated state: {flagged:?}"
        );
        for row in keys().iter().filter(|row| row.flag.is_some()) {
            assert!(
                row.table == "ui" || row.table == "engine",
                "[{}] {} carries a flag; `--clean` is [native]'s whole-table flag",
                row.table,
                row.key
            );
        }
    }
}
