//! Live-nvim proof that [`decode_redraw`] matches the real `ext_linegrid`
//! wire format, not just fixtures constructed by hand. Attaches to a real
//! spawned nvim and asserts its actual `redraw` traffic decodes to typed
//! events instead of falling through to `Unknown`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use rmpv::Value;
use std::time::{Duration, Instant};
use view_engine::process::{Engine, EngineConfig};
use view_engine::ui_events::{decode_redraw, UiEvent};

#[test]
fn decodes_grid_line_and_flush_from_real_nvim_redraw() {
    let engine = Engine::spawn(EngineConfig::default()).unwrap();

    let options = Value::Map(vec![(Value::from("ext_linegrid"), Value::from(true))]);
    engine
        .handle
        .request_timeout(
            "nvim_ui_attach",
            vec![Value::from(80), Value::from(24), options],
            Duration::from_secs(5),
        )
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_grid_line = false;
    let mut saw_flush = false;
    while Instant::now() < deadline && !(saw_grid_line && saw_flush) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(note) = engine.notifications.recv_timeout(remaining) else {
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
