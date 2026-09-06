//! What a late-attaching child answers `nvim_list_uis()` with while its
//! whole startup runs and no UI has attached yet.
//!
//! The question is not academic and the empty answer is not harmless: a
//! plugin decides which surfaces to claim at the moment it sets itself up,
//! which on this spawn shape is entirely inside that window. lazy.nvim
//! reads this list to decide whether it is running headless, and noice
//! reads it to decide whether the session it is starting under has taken
//! the cmdline, the message area and the popup menu away from it.
//!
//! The answer is nvim's own terminal UI: the real size, `ext_linegrid` and
//! nothing else externalized. Startup then produces exactly the state
//! `nvim` itself produces, and what view externalizes is settled at the
//! attach instead, by taking the surfaces back off whoever claimed them.
//!
//! So a live child is asked directly, before anything attaches, rather than
//! the chunk being read for the strings it contains: what has to be true is
//! that nvim answers for a UI at all, and only nvim can be asked that.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use rmpv::Value;
use view_engine::process::{Engine, EngineConfig};

/// The one field each answer is read through, so a missing key and a `false`
/// one are told apart by the caller rather than by `as_bool`'s own default.
fn flag(ui: &Value, name: &str) -> Option<bool> {
    ui.as_map()?
        .iter()
        .find(|(key, _)| key.as_str() == Some(name))?
        .1
        .as_bool()
}

#[test]
fn a_startup_with_no_ui_yet_answers_as_nvims_own_terminal_ui_would() {
    let engine = Engine::spawn(EngineConfig::isolated().with_late_attach(120, 40)).unwrap();

    let uis = engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                Value::from("return vim.api.nvim_list_uis()"),
                Value::Array(vec![]),
            ],
        )
        .unwrap();
    let uis = uis.as_array().expect("nvim_list_uis answers with a list");
    assert_eq!(
        uis.len(),
        1,
        "a startup nobody has attached to must still name the UI on its way, got {uis:?}"
    );
    let ui = &uis[0];
    assert_eq!(flag(ui, "ext_linegrid"), Some(true), "got {ui:?}");
    assert_eq!(flag(ui, "rgb"), Some(true), "got {ui:?}");
    // spelled `false` rather than left absent: a plugin comparing against
    // `false` reads what nvim itself would have answered
    for surface in [
        "ext_cmdline",
        "ext_popupmenu",
        "ext_messages",
        "ext_tabline",
    ] {
        assert_eq!(flag(ui, surface), Some(false), "{surface}, got {ui:?}");
    }

    // and the geometry the config was armed with, which is what a plugin
    // laying itself out at setup reads
    let size = engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                Value::from(
                    "local ui = vim.api.nvim_list_uis()[1] return ui.width .. 'x' .. ui.height",
                ),
                Value::Array(vec![]),
            ],
        )
        .unwrap();
    assert_eq!(size.as_str(), Some("120x40"));
}

#[test]
fn a_real_ui_replaces_the_answer_the_startup_stood_in_with() {
    let engine = Engine::spawn(EngineConfig::isolated().with_late_attach(120, 40)).unwrap();
    engine
        .handle
        .ui_attach(80, 24, &["ext_linegrid", "ext_cmdline"])
        .unwrap();

    let uis = engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                Value::from("return vim.api.nvim_list_uis()"),
                Value::Array(vec![]),
            ],
        )
        .unwrap();
    let uis = uis.as_array().expect("nvim_list_uis answers with a list");
    assert_eq!(uis.len(), 1, "got {uis:?}");
    let ui = &uis[0];
    assert_eq!(
        flag(ui, "ext_cmdline"),
        Some(true),
        "the attached UI's own answer, not the stand-in's; got {ui:?}"
    );
}
