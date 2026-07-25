//! Pins the embedded engine's config isolation: `EngineSession` must spawn
//! nvim with `--clean`, so a host's user config can never leak into oracle
//! screens. Deterministic on any host: the test itself points
//! `XDG_CONFIG_HOME` at a committed fixture whose `init.lua` paints a
//! marker into the grid, so removing `--clean` fails here even on a machine
//! with no nvim config at all.
//!
//! Lives alone in this integration binary because it mutates process
//! environment: each Cargo integration test file is its own process, so the
//! mutation cannot race tests in other binaries, and being the only test
//! here means there is no second thread in this one to spawn a child while
//! the mutation lands or to inherit it afterwards. The lib's own tests share
//! a binary and therefore need the lock in `view_oracle`'s `testenv`
//! instead; that module is `#[cfg(test)]` and so is not reachable from here.
//! A second test added to this file would need the same treatment: it would
//! run on its own thread, and `EngineSession::spawn` below reads the
//! environment this one is rewriting.
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
