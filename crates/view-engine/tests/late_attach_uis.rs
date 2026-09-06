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

/// Every key one entry of `nvim_list_uis()` carries, sorted, so two answers
/// are compared as sets rather than in whatever order the map was built.
fn keys(ui: &Value) -> Vec<String> {
    let mut names: Vec<String> = ui
        .as_map()
        .into_iter()
        .flatten()
        .filter_map(|(key, _)| key.as_str().map(str::to_owned))
        .collect();
    names.sort();
    names
}

/// `nvim_list_uis()` as the child answers it right now.
fn list_uis(engine: &Engine) -> Vec<Value> {
    engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                Value::from("return vim.api.nvim_list_uis()"),
                Value::Array(vec![]),
            ],
        )
        .unwrap()
        .as_array()
        .expect("nvim_list_uis answers with a list")
        .clone()
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

/// The stand-in's key set is nvim's own, read off the UI that later
/// attaches to the same child rather than written down here.
///
/// A key the table forgets is invisible to every assertion about the keys
/// it does carry, and it is not harmless: a plugin reads `term_name` to
/// decide what its terminal can draw and `stdout_tty` to decide whether it
/// has one at all, and `nil` is not an answer nvim ever gives for those.
/// nvim builds every entry of this list from one shape, so the UI attached
/// on the next line is the authority for what that shape is.
#[test]
fn the_stand_in_carries_every_key_nvim_reports_for_a_real_ui() {
    let engine = Engine::spawn(EngineConfig::isolated().with_late_attach(120, 40)).unwrap();
    let pending = list_uis(&engine);
    assert_eq!(pending.len(), 1, "got {pending:?}");
    let pending = keys(&pending[0]);

    engine.handle.ui_attach(120, 40, &["ext_linegrid"]).unwrap();
    let attached = list_uis(&engine);
    assert_eq!(attached.len(), 1, "got {attached:?}");

    assert_eq!(
        pending,
        keys(&attached[0]),
        "the answer a plugin reads during startup must carry the same keys \
         nvim gives the UI that replaces it"
    );
}

/// The values a plugin branches on during startup are the terminal UI's,
/// not a headless child's.
#[test]
fn the_stand_in_answers_as_a_session_with_a_terminal() {
    let engine = Engine::spawn(
        EngineConfig::isolated()
            .with_late_attach(120, 40)
            .with_env("TERM", "xterm-256color"),
    )
    .unwrap();
    let uis = list_uis(&engine);
    let ui = &uis[0];
    for flagged in ["stdin_tty", "stdout_tty", "ext_termcolors"] {
        assert_eq!(flag(ui, flagged), Some(true), "{flagged}, got {ui:?}");
    }
    for unflagged in ["ext_hlstate", "ext_wildmenu"] {
        assert_eq!(flag(ui, unflagged), Some(false), "{unflagged}, got {ui:?}");
    }
    let named = ui
        .as_map()
        .expect("a ui entry is a map")
        .iter()
        .find(|(key, _)| key.as_str() == Some("term_name"))
        .map(|(_, value)| value.clone());
    assert_eq!(
        named.as_ref().and_then(rmpv::Value::as_str),
        Some("xterm-256color"),
        "term_name is the child's own $TERM; got {ui:?}"
    );
}
