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
        .with_handshake_timeout(view_test_support::host_deadline(Duration::from_secs(2)));

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

/// A terminal the engine's own geometry refuses is a startup path a user
/// reaches -- a pty nothing has sized reports 0x0, a real terminal reports
/// it for the first instant of a session still negotiating its size, and a
/// narrow split reports a positive width under nvim's minimum -- and the
/// geometry `--cmd` is where such a reading is fatal rather than merely
/// small: it opens with `vim.o.columns`, nvim refuses anything under 12
/// with `E594` and anything under 3 lines with `E593`, and the aborted
/// chunk registers none of what follows it, the `VimEnter` hook the attach
/// waits on included. The child then stays alive and never says it started,
/// which is what the session's attach deadline expires against.
///
/// The size is taken through `grid_target_for` rather than written down
/// here, because that is the one arithmetic both the spawn and the attach
/// read: a floor that lived only at this call site would leave the attach
/// asking for the refused reading instead.
///
/// `vim.g.view` and the `VimEnter` hook are what tell a chunk that ran from
/// one that aborted -- the statement after the geometry line, and the
/// chunk's last. The geometry assertion is the pin on the answer's *value*:
/// it is asked at readings whose answers are not nvim's own 80x24 default
/// (5x40 is clamped per axis to 12x40, 100x1 to 100x3), so a spawn that took
/// the default by aborting cannot satisfy it either.
///
/// Both axes of `view_core::model::ENGINE_MIN_SIZE` are walked against the
/// engine itself, and that is the only place either value is proven rather
/// than compared against itself: 100x1 spawns the child at 3 lines, one
/// under which the chunk aborts on `E593`, so a rows minimum lowered to 2
/// fails here and nowhere else.
#[test]
fn a_spawn_sized_from_a_refused_geometry_runs_its_whole_startup_chunk() {
    for reading in [(0, 0), (5, 40), (100, 1)] {
        let (width, height) = view_core::model::grid_target_for(reading, 0, false);
        let engine =
            Engine::spawn(EngineConfig::isolated().with_late_attach(width, height)).unwrap();

        let read = |lua: &str| {
            engine
                .handle
                .request(
                    "nvim_exec_lua",
                    vec![rmpv::Value::from(lua), rmpv::Value::Array(vec![])],
                )
                .unwrap()
        };
        assert_eq!(
            read("return vim.g.view").as_u64(),
            Some(1),
            "the startup chunk aborted at its geometry line for {reading:?}"
        );
        assert!(
            read("return #vim.api.nvim_get_autocmds({ event = 'VimEnter' })")
                .as_u64()
                .is_some_and(|hooks| hooks >= 1),
            "the VimEnter hook the attach waits on was never registered for {reading:?}"
        );
        assert_eq!(
            read("return { vim.o.columns, vim.o.lines }")
                .as_array()
                .and_then(|size| Some((size.first()?.as_u64()?, size.get(1)?.as_u64()?))),
            Some((u64::from(width), u64::from(height))),
            "the child lays out at the size the spawn named for {reading:?}"
        );
    }
}
