//! Pins the embedded engine's config isolation: an `EngineSession` child must
//! source no user config the host points it at, so a developer's own
//! `init.lua` can never leak into oracle screens. Deterministic on any host:
//! the test itself points `XDG_CONFIG_HOME` at a committed fixture whose
//! `init.lua` paints a marker into the grid, so a session that sources user
//! config fails here even on a machine with no nvim config at all.
//!
//! Two layers stand between that plant and the child, and either one alone
//! would satisfy the assertion: `--clean`, which discards user config, and
//! the hermetic environment sweep, which drops `XDG_CONFIG_HOME` outright
//! because no allowlist in `view_engine::env` names it. The control below is
//! what keeps the pin from being a statement about neither: it runs the same
//! probe against an engine with neither layer, and fails if the fixture never
//! painted anything, if the probe cannot see what it paints, or if the plant
//! never reached a child in the first place.
//!
//! Lives alone in this integration binary because it mutates process
//! environment: each Cargo integration test file is its own process, so the
//! mutation cannot race tests in other binaries, and being the only test
//! here means there is no second thread in this one to spawn a child while
//! the mutation lands or to inherit it afterwards. The lib's own tests share
//! a binary and therefore need the lock in `view_oracle`'s `testenv`
//! instead; that module is `#[cfg(test)]` and so is not reachable from here.
//! A second test added to this file would need the same treatment: it would
//! run on its own thread, and both spawns below read the environment this one
//! is rewriting.

// sole test in this binary, and its spawns run on the same thread after
// the plant, so there is no second reader for a guard to order it against
#![allow(clippy::disallowed_methods)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use view_engine::process::{Engine, EngineConfig};
use view_oracle::EngineSession;

/// What the fixture's `init.lua` writes into the buffer it opens.
const MARKER: &str = "CONFIG-LEAKED";

/// Reports every line of every buffer the child holds, which is where a
/// sourced fixture leaves its mark.
///
/// Identical on both arms below, and read through the engine rather than off
/// a rendered screen so the control needs no UI attached: an engine started
/// without `--clean` may emit a startup message, and a message parks the
/// shutdown `qa!` in `wait_return` with no UI to answer it.
const BUFFER_PROBE: &str =
    r#"join(map(nvim_list_bufs(), 'join(nvim_buf_get_lines(v:val, 0, -1, v:false), "")'), "|")"#;

/// A scratch directory under the build tree, never the system temp dir,
/// which is world-writable under a guessable name.
fn scratch(name: &str) -> PathBuf {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.pop(); // crates/
    root.pop(); // workspace root
    let dir = root.join("target").join("view-clean-pin").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// An engine with neither of the two layers the pin is about: no `--clean`,
/// and no hermetic plan. It keeps only the isolation that is not under test
/// -- the other three `XDG_*_HOME` directories and the two system-wide
/// search paths -- so that the one thing it sources is the planted fixture,
/// on a developer machine with a full plugin set as much as on a bare CI
/// runner.
fn leaky_config(empty: &Path) -> EngineConfig {
    let homes = scratch("homes");
    EngineConfig::default()
        .with_arg("-n")
        .with_env("XDG_DATA_HOME", homes.join("data"))
        .with_env("XDG_STATE_HOME", homes.join("state"))
        .with_env("XDG_CACHE_HOME", homes.join("cache"))
        .with_env("XDG_CONFIG_DIRS", empty)
        .with_env("XDG_DATA_DIRS", empty)
}

#[test]
fn engine_session_ignores_an_intrusive_user_config() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("xdg-config");
    assert!(
        fixture.join("nvim").join("init.lua").is_file(),
        "fixture config missing; the pin would pass vacuously"
    );
    let empty = view_engine::env::prepare_empty_search_path().unwrap();
    std::env::set_var("XDG_CONFIG_HOME", &fixture);

    // the control, first: without it a clean screen below would read the same
    // against a fixture that painted nothing, a probe that could never see it,
    // and a plant that never reached any child at all
    let leaky = Engine::spawn(leaky_config(&empty)).expect("an unguarded engine failed to run");
    // `--embed` holds startup until a UI attaches, so a child probed before
    // this call has sourced no configuration at all and reports the same
    // empty buffer list a fully isolated one would
    leaky.handle.ui_attach(40, 6).unwrap();
    // the attach only releases startup; sourcing happens on the child's own
    // schedule from there, and a single probe would race it
    let deadline = Instant::now() + view_test_support::host_deadline(Duration::from_secs(10));
    let mut leaked = leaky.handle.eval_str(BUFFER_PROBE).unwrap();
    while !leaked.contains(MARKER) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
        leaked = leaky.handle.eval_str(BUFFER_PROBE).unwrap();
    }
    assert!(
        leaked.contains(MARKER),
        "the planted config reached no child even unguarded, so this test can \
         prove nothing about a guarded one; buffers: {leaked}"
    );
    drop(leaky);

    let mut session = EngineSession::spawn(40, 6).expect("EngineSession::spawn against real nvim");
    while session
        .pump_until_flush(Duration::from_millis(500))
        .expect("draining startup flushes")
    {}

    let buffers = session.eval_str(BUFFER_PROBE).unwrap();
    assert!(
        !buffers.contains(MARKER),
        "user config was sourced by the embedded engine; buffers: {buffers}"
    );
    let screen = session.screen_text();
    assert!(
        !screen.contains(MARKER),
        "user config reached the decoded screen; screen:\n{screen}"
    );
}
