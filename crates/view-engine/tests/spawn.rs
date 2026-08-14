#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::path::PathBuf;
use std::time::Duration;
use view_engine::process::{Engine, EngineConfig};
use view_engine::EngineError;

mod common;

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
#[test]
fn handshake_failure_reaps_child() {
    let marker = marker_path();
    let _ = std::fs::remove_file(&marker);
    let cfg = EngineConfig::default()
        .with_nvim_bin(env!("CARGO_BIN_EXE_view-engine-hang-fixture"))
        .with_env("VIEW_ENGINE_HANG_MARKER", &marker)
        // long enough that a host's own process startup fits inside it: a
        // debug binary on macOS pays dyld and codesign validation before its
        // main runs at all, which has been measured in this tree at over
        // half a second, and a handshake that gave up first would kill the
        // fixture before it could ever report itself
        .with_handshake_timeout(Duration::from_secs(2));

    // spawn() blocks waiting on a handshake that never comes; watch for the
    // fixture's marker on this thread meanwhile, to prove the fake process
    // was actually alive mid-handshake rather than absent for never having
    // started -- without which the reap assertion below passes vacuously.
    // The watch runs for exactly as long as spawn() itself does, rather than
    // for a constant fraction of its timeout: how long fork and exec take is
    // a property of the host, so any constant here races the very startup it
    // means to observe, and loses that race whenever the machine is busy.
    let spawn_thread = std::thread::spawn(move || Engine::spawn(cfg));
    let read_marker = || {
        std::fs::read_to_string(&marker)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
    };
    let mut fixture_pid = None;
    while !spawn_thread.is_finished() {
        fixture_pid = read_marker();
        if fixture_pid.is_some() {
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
    // a last read for the fixture that wrote its marker in the same instant
    // spawn() gave up: the file outlives the process it names, so finding it
    // here still proves the process ran
    let fixture_pid = fixture_pid.or_else(read_marker).expect(
        "fake nvim process never wrote its marker, so it was never observed \
         running; the test does not exercise the reap path",
    );

    // proof of no zombie: by the time spawn() returned, ChildGuard's Drop
    // had already run (it is a local in the now-returned spawn() stack
    // frame), so kill()+wait() already happened. The fixture is a single
    // process with no grandchild, so Child::kill() targets the actual
    // blocked process directly and Child::wait() fully reaps it -- this
    // assertion fails for either a leaked zombie (still in the process
    // table) or a leaked-but-still-running child.
    assert!(
        !common::pid_in_process_table(fixture_pid),
        "fake nvim pid {fixture_pid} still in the process table after \
         spawn() returned: not reaped"
    );
    let _ = std::fs::remove_file(&marker);
}

/// Where the hang fixture reports its pid: under the build tree, never the
/// system temp dir, which is world-writable and would let an unrelated
/// process pre-create this predictable path as a symlink.
fn marker_path() -> PathBuf {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.pop(); // crates/
    root.pop(); // workspace root
    let dir = root.join("target").join("view-engine-spawn");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!("hang-marker-{}", std::process::id()))
}
