#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::path::PathBuf;
use std::time::Duration;
use view_engine::process::{Engine, EngineConfig};
use view_engine::EngineError;

#[test]
fn spawns_and_handshakes_with_real_nvim() {
    let engine = Engine::spawn(EngineConfig::default()).unwrap();
    assert!(engine.api_info.channel_id >= 1);
    // floor from the spec: engine must be at least 0.11
    assert!(
        (engine.api_info.version_major, engine.api_info.version_minor) >= (0, 11),
        "nvim >= 0.11 required, found {}.{}",
        engine.api_info.version_major,
        engine.api_info.version_minor
    );
    let echoed = engine
        .handle
        .request("nvim_eval", vec![rmpv::Value::from("21 * 2")])
        .unwrap();
    assert_eq!(echoed.as_u64(), Some(42));
    // no manual kill: Engine's Drop impl kills and reaps the child, and it
    // runs even if an earlier assert above panics and unwinds through here
}

/// Reproduces the zombie found in review: `Engine::spawn` against a binary
/// that accepts `--embed` without erroring but never replies must not leak
/// the child when the handshake times out.
#[test]
fn handshake_failure_reaps_child() {
    let fixture = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/fake_hang_nvim.sh"
    ));
    let cfg = EngineConfig {
        nvim_bin: fixture.clone(),
        handshake_timeout: Duration::from_millis(500),
        ..EngineConfig::default()
    };

    // spawn() blocks for ~200ms waiting on the handshake; race a pgrep
    // against it on another thread to prove the fake process was actually
    // alive mid-handshake, not just absent for lack of ever starting. All
    // `cargo test` tests in this binary are threads in one process, so the
    // fake child's parent pid is our own; scoping pgrep to it (rather than
    // a bare name match) avoids colliding with unrelated `sleep` processes
    // elsewhere on the host.
    let spawn_thread = std::thread::spawn(move || Engine::spawn(cfg));
    std::thread::sleep(Duration::from_millis(50));
    let seen_alive = fake_child_alive();

    let result = spawn_thread.join().unwrap();
    let err = result.err();
    assert!(
        matches!(err, Some(EngineError::Timeout { .. })),
        "expected Some(EngineError::Timeout {{ .. }}), got {err:?}"
    );
    assert!(
        seen_alive,
        "fake nvim process was never observed running; test does not \
         exercise the reap path"
    );

    // proof of no zombie: by the time spawn() returned to us, ChildGuard's
    // Drop had already run (it is a local in the now-returned spawn()
    // stack frame), so kill()+wait() already happened. `exec sleep
    // infinity` in the fixture replaces the shell's image in place (same
    // pid, no grandchild), so Child::kill() targets the actual sleeping
    // process directly and Child::wait() fully reaps it -- this assertion
    // would fail for either a leaked zombie (pgrep still lists it) or a
    // leaked-but-still-running child.
    assert!(
        !fake_child_alive(),
        "fake nvim process still present after spawn() returned: not reaped"
    );
}

/// True if a `sleep infinity` process (the fixture's post-`exec` identity)
/// is currently a child of this test binary's process.
fn fake_child_alive() -> bool {
    std::process::Command::new("pgrep")
        .args([
            "-P",
            &std::process::id().to_string(),
            "-f",
            "sleep infinity",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
