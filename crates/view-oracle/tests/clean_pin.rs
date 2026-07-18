//! Pins the embedded engine's config isolation: `EngineSession` must spawn
//! nvim with `--clean`, so a host's user config can never leak into oracle
//! screens. Deterministic on any host: the test itself points
//! `XDG_CONFIG_HOME` at a committed fixture whose `init.lua` paints a
//! marker into the grid, so removing `--clean` fails here even on a machine
//! with no nvim config at all.
//!
//! Lives alone in this integration binary because it mutates process
//! environment: each Cargo integration test file is its own process, so the
//! mutation cannot race tests in other binaries.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;
use view_oracle::EngineSession;

#[test]
fn engine_session_ignores_an_intrusive_user_config() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("xdg-config");
    assert!(
        fixture.join("nvim").join("init.lua").is_file(),
        "fixture config missing; the pin would pass vacuously"
    );
    std::env::set_var("XDG_CONFIG_HOME", &fixture);

    let mut session = EngineSession::spawn(40, 6).expect("EngineSession::spawn against real nvim");
    while session.pump_until_flush(Duration::from_millis(500)) {}

    let screen = session.screen_text();
    assert!(
        !screen.contains("CONFIG-LEAKED"),
        "user config was sourced by the embedded engine; screen:\n{screen}"
    );
}
