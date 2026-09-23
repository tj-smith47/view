//! The key registry and the two crates that resolve against it, held to
//! each other here because this is the only crate that may name both.
//!
//! `view-native` and `view-ai` are forbidden to each other in both
//! directions, so the registry carries the `[ai]` rows as metadata -- table,
//! key, environment name, derived default -- and `view-ai` resolves them.
//! Nothing inside either crate can notice the two drifting apart.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use view_native::config::{env_name, keys, resolve_with, ConfigKey, Overrides, ViewConfig};

/// Whether a row belongs to the table `view-ai` owns, its nested
/// sub-tables included: `[ai.review]` is as much that crate's as `[ai]` is.
fn is_ai(row: &ConfigKey) -> bool {
    row.table == "ai" || row.table.starts_with("ai.")
}

/// Every `VIEW_AI_*` name the registry generates is a name `view-ai`
/// actually reads, and the two lists are the same length in both
/// directions -- neither crate may carry a key the other has not heard of.
#[test]
fn every_ai_registry_row_is_honored_by_view_ai() {
    let published = view_ai::AiConfig::env_names();
    let registered: Vec<String> = keys()
        .iter()
        .filter(|row| is_ai(row))
        .map(env_name)
        .collect();
    for name in &registered {
        assert!(
            published.contains(&name.as_str()),
            "the registry generates {name}, which view-ai reads under no name: {published:?}"
        );
    }
    assert_eq!(
        published.len(),
        registered.len(),
        "view-ai reads a name the registry does not carry: {published:?} against {registered:?}"
    );
}

/// The key paths themselves agree, not merely their environment names: a
/// row's `(table, key)` is what the doctor prints and what `AiConfig::rows`
/// answers under, and a rename on one side with a matching environment name
/// on the other would still print a key nobody can write.
#[test]
fn the_ai_registry_rows_and_view_ais_own_rows_are_the_same_keys() {
    let registered: Vec<(&str, &str)> = keys()
        .iter()
        .filter(|row| is_ai(row))
        .map(|row| (row.table, row.key))
        .collect();
    let answered: Vec<(&str, &str)> = view_ai::AiConfig::default()
        .rows()
        .into_iter()
        .map(|(table, key, _, _)| (table, key))
        .collect();
    assert_eq!(
        registered, answered,
        "the registry's `[ai]` rows and view-ai's own must be the same keys, in the same order"
    );
}

/// The split the two halves make of the registry: every row is answered by
/// exactly one of them, with no count written down on either side -- a key
/// added to the registry has to land in one resolver or the other before
/// this is green again.
///
/// `keys.desktop_modifier` is the one row neither crate answers:
/// [`view_native::config::ResolvedConfig::rows`]'s own doc states why --
/// its real value needs the terminal's own kitty keyboard protocol probe,
/// which neither resolver holds, so `view`'s own `caps_notice` prints it
/// beside this walk, outside it.
#[test]
fn the_two_resolvers_answer_the_whole_registry_between_them() {
    let resolved = resolve_with(&ViewConfig::defaults(), &Overrides::default(), &|_| None);
    let native: Vec<(&str, &str)> = resolved
        .rows()
        .into_iter()
        .map(|(key, _, _)| (key.table, key.key))
        .collect();
    let ai: Vec<(&str, &str)> = view_ai::AiConfig::default()
        .rows()
        .into_iter()
        .map(|(table, key, _, _)| (table, key))
        .collect();
    let together: Vec<(&str, &str)> = native.iter().chain(ai.iter()).copied().collect();
    let registered: Vec<(&str, &str)> = keys()
        .iter()
        .map(|row| (row.table, row.key))
        .filter(|row| *row != ("keys", "desktop_modifier"))
        .collect();
    assert_eq!(
        together, registered,
        "every registry row but keys.desktop_modifier is answered by exactly one of the two crates, in registry order"
    );
    assert!(
        native.iter().all(|row| !ai.contains(row)),
        "a key answered twice has two provenances and no single answer"
    );
}

/// A derived default the registry states for an `[ai]` row is the one
/// `view-ai` resolves to with nothing set. The registry's text is what the
/// doctor prints beside the word derived, and a value that disagreed with
/// the crate that owns the table would be printing a lie.
///
/// Walked off `AiConfig::rows` rather than matched key by key, so a key
/// added to that table joins this check without a second arm to write.
#[test]
fn the_ai_rows_derived_defaults_are_what_view_ai_resolves() {
    let clean =
        view_ai::AiConfig::resolve_with(None, true, &|_| None).expect("no file, no failure path");
    let derived: Vec<(&str, &str, String)> = clean
        .rows()
        .into_iter()
        .map(|(table, key, value, _)| (table, key, value))
        .collect();
    for row in keys().iter().filter(|row| is_ai(row)) {
        let stated = row.derived.expect("an [ai] key states its own default");
        let (_, _, resolved) = derived
            .iter()
            .find(|(table, key, _)| *table == row.table && *key == row.key)
            .unwrap_or_else(|| panic!("[{}] {} has no row in view-ai", row.table, row.key));
        assert_eq!(
            resolved, stated,
            "[{}] {} derives {resolved}, and the registry says {stated}",
            row.table, row.key
        );
    }
}
