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

/// Every key any crate in this build reads, in the order the shipped
/// example lists them: `[ui]`, `[engine]`, `[native]`, `[supervision]`,
/// `[ai]`.
///
/// Thirteen rows, and the count is a contract rather than an accident:
/// eleven of them are resolved by this crate ([`super::ResolvedConfig::rows`]
/// is that walk), and the two `[ai]` rows are metadata only -- that table
/// is parsed and resolved by the crate that owns it, and the bin holds the
/// two to each other, because `view-native` and `view-ai` may not name each
/// other's types.
#[must_use]
pub fn keys() -> &'static [ConfigKey] {
    static KEYS: OnceLock<Vec<ConfigKey>> = OnceLock::new();
    KEYS.get_or_init(|| {
        let mut rows = vec![
            ConfigKey {
                table: "ui",
                key: "tier",
                flag: Some("--tier"),
                derived: Some("auto"),
            },
            ConfigKey {
                table: "ui",
                key: "theme",
                flag: Some("--theme"),
                derived: Some("auto"),
            },
            ConfigKey {
                table: "engine",
                key: "nvim_bin",
                flag: Some("--nvim-bin"),
                derived: Some("bundled"),
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
            derived: Some("true"),
        }));
        rows.push(ConfigKey {
            table: "supervision",
            key: "auto_restart",
            flag: None,
            derived: Some("true"),
        });
        rows.push(ConfigKey {
            table: "ai",
            key: "enabled",
            flag: None,
            derived: Some("true"),
        });
        rows.push(ConfigKey {
            table: "ai",
            key: "agent",
            flag: None,
            derived: Some("claude-code"),
        });
        rows
    })
}

/// The environment variable name for a key, derived from its own path so
/// the two can never disagree: `[native] picker` is `VIEW_NATIVE_PICKER`.
///
/// The `VIEW_*` namespace carries members that are not config at all
/// (`VIEW_LOG`, and the harness's own names), so a resolver claims exactly
/// the names this function generates and reads no other.
#[must_use]
pub fn env_name(key: &ConfigKey) -> String {
    format!(
        "VIEW_{}_{}",
        key.table.to_uppercase(),
        key.key.to_uppercase()
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn the_registry_carries_every_key_the_example_ships() {
        assert_eq!(keys().len(), 13, "thirteen keys, five tables");
        let mut tables: Vec<&str> = keys().iter().map(|row| row.table).collect();
        tables.dedup();
        assert_eq!(
            tables,
            vec!["ui", "engine", "native", "supervision", "ai"],
            "the rows run in the order the shipped example lists the tables"
        );
    }

    #[test]
    fn the_native_rows_are_the_feature_registry_itself() {
        let registered: Vec<&str> = keys()
            .iter()
            .filter(|row| row.table == "native")
            .map(|row| row.key)
            .collect();
        let features: Vec<&str> = registry::features().iter().map(|f| f.id).collect();
        assert_eq!(
            registered, features,
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
                name.contains(&row.table.to_uppercase()) && name.contains(&row.key.to_uppercase()),
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
        // the three the plan names outright, so a rewrite of the derivation
        // that still passes the shape checks above cannot rename them
        let spelled: Vec<String> = keys().iter().map(env_name).collect();
        for expected in [
            "VIEW_UI_TIER",
            "VIEW_NATIVE_PICKER",
            "VIEW_SUPERVISION_AUTO_RESTART",
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
            5,
            "five flags, and a row without one is a stated state: {flagged:?}"
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
