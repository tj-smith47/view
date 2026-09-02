//! The key registry and the two crates that resolve against it, held to
//! each other here because this is the only crate that may name both.
//!
//! `view-native` and `view-ai` are forbidden to each other in both
//! directions, so the registry carries the `[ai]` rows as metadata -- table,
//! key, environment name, derived default -- and `view-ai` resolves them.
//! Nothing inside either crate can notice the two drifting apart.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use view_native::config::{env_name, keys, resolve_with, Overrides, ViewConfig};

/// Every `VIEW_AI_*` name the registry generates is a name `view-ai`
/// actually reads.
#[test]
fn every_ai_registry_row_is_honored_by_view_ai() {
    let published = view_ai::AiConfig::env_names();
    let registered: Vec<String> = keys()
        .iter()
        .filter(|row| row.table == "ai")
        .map(env_name)
        .collect();
    assert_eq!(
        registered.len(),
        2,
        "the registry carries `[ai] enabled` and `[ai] agent`: {registered:?}"
    );
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

/// The cardinality contract the two halves split between them: thirteen
/// registry rows, eleven answered by `view-native`, two by `view-ai`.
#[test]
fn the_two_resolvers_answer_the_whole_registry_between_them() {
    let resolved = resolve_with(&ViewConfig::defaults(), &Overrides::default(), &|_| None);
    let native = resolved.rows().len();
    let ai = view_ai::AiConfig::env_names().len();
    assert_eq!(native, 11, "view-native answers eleven keys");
    assert_eq!(
        native + ai,
        keys().len(),
        "every registry row is answered by exactly one of the two crates"
    );
}

/// A derived default the registry states for an `[ai]` row is the one
/// `view-ai` resolves to with nothing set. The registry's text is what the
/// doctor prints beside the word derived, and a value that disagreed with
/// the crate that owns the table would be printing a lie.
#[test]
fn the_ai_rows_derived_defaults_are_what_view_ai_resolves() {
    let clean =
        view_ai::AiConfig::resolve_with(None, true, &|_| None).expect("no file, no failure path");
    for row in keys().iter().filter(|row| row.table == "ai") {
        let stated = row.derived.expect("an [ai] key states its own default");
        let resolved = match row.key {
            "enabled" => clean.enabled().to_string(),
            "agent" => match clean.agent_spec() {
                view_ai::AgentSpec::Id(id) => id.clone(),
                view_ai::AgentSpec::Command(words) => words.join(" "),
            },
            other => panic!("[ai] {other} has no reader in this cross-check"),
        };
        assert_eq!(
            resolved, stated,
            "[ai] {} derives {resolved}, and the registry says {stated}",
            row.key
        );
    }
}
