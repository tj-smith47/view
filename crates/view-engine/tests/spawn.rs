#![allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::time::Duration;
use view_engine::process::{Engine, EngineConfig};
#[cfg(unix)]
use view_engine::EngineError;

#[test]
fn spawns_and_handshakes_with_real_nvim() {
    let engine = Engine::spawn(EngineConfig::isolated()).unwrap();
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

/// `Engine::spawn` against a binary that accepts `--embed` without erroring
/// but never replies must not leak the child when the handshake times out.
///
/// Unix-only: the fixture is a shell script, which Windows `CreateProcess`
/// cannot exec directly, and the liveness check below shells out to
/// `pgrep`, which has no Windows equivalent.
#[cfg(unix)]
#[test]
fn handshake_failure_reaps_child() {
    let fixture = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/fake_hang_nvim.sh"
    ));
    let cfg = EngineConfig::default()
        .with_nvim_bin(fixture.clone())
        .with_handshake_timeout(Duration::from_millis(500));

    // spawn() blocks for ~500ms waiting on the handshake; race a pgrep
    // against it on another thread to prove the fake process was actually
    // alive mid-handshake, not just absent for lack of ever starting. All
    // `cargo test` tests in this binary are threads in one process, so the
    // fake child's parent pid is our own; scoping pgrep to it (rather than
    // a bare name match) avoids colliding with unrelated `sleep` processes
    // elsewhere on the host. The probe repeats until the fixture is seen
    // or the handshake window is nearly spent: how long fork, exec of the
    // shell, and its exec of sleep take varies by host and platform, so a
    // single probe at a fixed offset races the very startup it means to
    // observe.
    let spawn_thread = std::thread::spawn(move || Engine::spawn(cfg));
    let deadline = std::time::Instant::now() + Duration::from_millis(400);
    let mut seen_alive = false;
    while std::time::Instant::now() < deadline {
        if fake_child_alive() {
            seen_alive = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

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
    // stack frame), so kill()+wait() already happened. `exec sleep 100000`
    // in the fixture replaces the shell's image in place (same pid, no
    // grandchild), so Child::kill() targets the actual sleeping process
    // directly and Child::wait() fully reaps it -- this assertion would
    // fail for either a leaked zombie (pgrep still lists it) or a
    // leaked-but-still-running child.
    assert!(
        !fake_child_alive(),
        "fake nvim process still present after spawn() returned: not reaped"
    );
}

/// True if a `sleep 100000` process (the fixture's post-`exec` identity) is
/// currently a child of this test binary's process.
#[cfg(unix)]
fn fake_child_alive() -> bool {
    std::process::Command::new("pgrep")
        .args(["-P", &std::process::id().to_string(), "-f", "sleep 100000"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
