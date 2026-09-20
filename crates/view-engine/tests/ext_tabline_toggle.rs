//! Live-nvim proof that an `ext_*` surface can be taken from the engine and
//! handed back while a session runs, which is what lets a look flip bring
//! the pill or give row 0 back to nvim without a restart.
//!
//! Nothing in view's own code says whether nvim honours an `ext_*` option
//! after `nvim_ui_attach`. The answer is the engine's, and the only way to
//! read it is to ask a running one.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::mpsc::{self};
use std::time::Instant;

use view_core::msg::Msg;
use view_engine::damage::DamagePump;
use view_engine::process::{Engine, EngineConfig};
use view_engine::ui_events::UiEvent;

/// The tab count each `tabline_update` carries before `deadline`, and
/// whether row 0 of the grid carries nvim's own tab line.
///
/// Both readings come off one drain because they answer the same question
/// from the two sides nvim can answer it: it either sends the row as an
/// event or draws it into the grid, never both.
fn drain(rx: &mpsc::Receiver<Msg>, pump: &DamagePump, deadline: Instant) -> (Vec<usize>, bool) {
    let mut tablines = Vec::new();
    let mut drawn = false;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(Msg::RedrawReady) = rx.recv_timeout(remaining) else {
            break;
        };
        for event in pump.take_damage() {
            match event {
                UiEvent::TablineUpdate { tabs, .. } => tablines.push(tabs.len()),
                UiEvent::GridLine { row: 0, .. } => drawn = true,
                _ => {}
            }
        }
    }
    (tablines, drawn)
}

#[test]
fn ext_tabline_is_taken_and_handed_back_while_the_session_runs() {
    let mut engine = Engine::spawn(EngineConfig::isolated()).unwrap();
    let (tx, rx) = mpsc::sync_channel(256);
    let (pump, _cutover) = engine.start_pump(tx);
    engine
        .handle
        .ui_attach(80, 24, &view_engine::ui_ext_options_shipped())
        .unwrap();
    let lua = |chunk: &str| {
        engine
            .handle
            .request(
                "nvim_exec_lua",
                vec![rmpv::Value::from(chunk), rmpv::Value::Array(Vec::new())],
            )
            .unwrap();
    };
    let settle = || Instant::now() + common::rpc_deadline();

    lua("vim.cmd('tabnew')");
    let (before, drawn_before) = drain(&rx, &pump, settle());
    assert!(
        before.is_empty(),
        "the attach asked for no tab line and nvim sent {before:?}"
    );
    assert!(
        drawn_before,
        "nvim drew nothing into row 0 while it owned the tab line"
    );

    // back to one tabpage, so the take is read on the session a flip is
    // likeliest to happen on: nothing about the tabs changed, and the row
    // the pill is about to draw has names only if nvim sends them for the
    // option set on its own
    lua("vim.cmd('tabonly')");
    let _ = drain(&rx, &pump, settle());

    engine.handle.set_ui_ext("ext_tabline", true).unwrap();
    let (taken, _) = drain(&rx, &pump, settle());
    assert_eq!(
        taken.first().copied(),
        Some(1),
        "setting the option on a live UI with one tabpage sent {taken:?}, so \
         a flip to tiles would paint a pill with no names on it"
    );

    engine.handle.set_ui_ext("ext_tabline", false).unwrap();
    lua("vim.cmd('tabnew')");
    let (given_back, drawn_after) = drain(&rx, &pump, settle());
    assert!(
        given_back.is_empty(),
        "nvim kept sending tabline_update after the option was unset: {given_back:?}"
    );
    assert!(
        drawn_after,
        "nvim took the tab line back and drew nothing into row 0"
    );
}
