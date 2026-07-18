//! Live-nvim proof that [`decode_redraw`] matches the real wire formats,
//! not just fixtures constructed by hand. Attaches to a real spawned nvim
//! with the full ext set ([`view_engine::handle::EngineHandle::ui_attach`])
//! and asserts its actual `redraw` traffic decodes to typed events instead
//! of falling through to `Unknown`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};
use view_engine::process::{Engine, EngineConfig};
use view_engine::ui_events::{decode_redraw, UiEvent};

#[test]
fn decodes_grid_line_and_flush_from_real_nvim_redraw() {
    let mut engine = Engine::spawn(EngineConfig::default()).unwrap();
    let notifications = engine.take_notifications().unwrap();
    engine.handle.ui_attach(80, 24).unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_grid_line = false;
    let mut saw_flush = false;
    while Instant::now() < deadline && !(saw_grid_line && saw_flush) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(note) = notifications.recv_timeout(remaining) else {
            break;
        };
        if note.method != "redraw" {
            continue;
        }
        for event in decode_redraw(&note.params) {
            match event {
                UiEvent::GridLine { .. } => saw_grid_line = true,
                UiEvent::Flush => saw_flush = true,
                _ => {}
            }
        }
    }

    assert!(
        saw_grid_line,
        "expected at least one decoded GridLine from real nvim redraw traffic \
         (decoder likely mismatches the live wire arity again)"
    );
    assert!(
        saw_flush,
        "expected at least one decoded Flush from real nvim redraw traffic"
    );
    // no manual detach/kill: Engine's Drop reaps the child
}

#[test]
fn decodes_mode_change_and_cmdline_show_from_real_nvim_redraw() {
    let mut engine = Engine::spawn(EngineConfig::default()).unwrap();
    let notifications = engine.take_notifications().unwrap();
    engine.handle.ui_attach(80, 24).unwrap();
    // entering cmdline mode is what proves the full ext set actually took:
    // without ext_cmdline, nvim paints ":" straight into the grid instead
    // of emitting mode_change + cmdline_show.
    engine.handle.input(":").unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_mode_change = false;
    let mut saw_cmdline_show = false;
    while Instant::now() < deadline && !(saw_mode_change && saw_cmdline_show) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(note) = notifications.recv_timeout(remaining) else {
            break;
        };
        if note.method != "redraw" {
            continue;
        }
        for event in decode_redraw(&note.params) {
            match event {
                UiEvent::ModeChange { .. } => saw_mode_change = true,
                UiEvent::CmdlineShow { .. } => saw_cmdline_show = true,
                _ => {}
            }
        }
    }

    assert!(
        saw_mode_change,
        "expected at least one decoded ModeChange after entering cmdline mode \
         (decoder likely mismatches the live wire arity again)"
    );
    assert!(
        saw_cmdline_show,
        "expected at least one decoded CmdlineShow after `nvim_input(\":\")`"
    );
}
