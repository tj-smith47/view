//! `Engine::restart` against a real nvim: a child killed out of band is
//! replaced by a live one, an unsaved edit comes back out of nvim's own swap
//! file, and neither teardown leaves a process behind.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use std::path::{Path, PathBuf};
use std::time::Duration;
use view_engine::process::{Engine, EngineConfig};

mod common;

/// A config with no user configuration, no plugins, no shada and no swap
/// file, matching every other live test here. Built per call rather than
/// cloned: an `EngineConfig` is consumed by the spawn it describes, and a
/// restart is a second spawn.
fn cfg() -> EngineConfig {
    EngineConfig::isolated().with_shutdown_timeout(Duration::from_millis(500))
}

/// A scratch directory under the build tree -- never the system temp dir,
/// which is world-writable -- holding both the file a recovery test edits
/// and the state directory nvim writes that file's swap into.
fn scratch(name: &str) -> PathBuf {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.pop();
    root.pop();
    let dir = root.join("target").join("view-engine-restart").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A config editing `file` with swap files enabled and every one of them
/// written under `dir`, so a recovery reads the swap this test made rather
/// than anything the host happens to hold.
fn editing(dir: &Path, file: &Path) -> EngineConfig {
    EngineConfig::default()
        .with_arg("--clean")
        .with_env("XDG_STATE_HOME", dir.join("state"))
        .with_arg(file)
        .with_shutdown_timeout(Duration::from_millis(500))
}

/// Kills `pid` without going through `Engine`, so the teardown a restart
/// performs meets a child that is already gone -- the shape a real crash
/// leaves behind, which the ordinary `Drop` path never produces.
///
/// Per platform for the same reason `common::pid_in_process_table` is: the
/// crash this simulates has to be a real one on the host actually running
/// the test, and neither platform's tool exists on the other.
#[cfg(unix)]
fn kill_out_of_band(pid: u32) {
    let killed = std::process::Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .expect("kill must run for an out-of-band crash to be simulable");
    assert!(killed.success(), "kill -KILL {pid} failed: {killed:?}");
}

#[cfg(windows)]
fn kill_out_of_band(pid: u32) {
    let killed = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .status()
        .expect("taskkill must run for an out-of-band crash to be simulable");
    assert!(
        killed.success(),
        "taskkill /F /PID {pid} failed: {killed:?}"
    );
}

/// Waits until `pid` has left the process table, or fails: a `kill` that has
/// been sent is not yet a process that has died, and an assertion made
/// against a still-running child would prove nothing about the crash path.
fn wait_until_gone(pid: u32) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if !common::pid_in_process_table(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !common::pid_in_process_table(pid),
        "pid {pid} still in the process table 5s after SIGKILL"
    );
}

/// Whether the engine is at a prompt nobody can answer. `nvim_get_mode` is
/// answered on receipt even then (that is the whole reason the liveness
/// probe rides it), so a live reply is not on its own proof of a usable
/// editor -- this is.
fn blocking(engine: &Engine) -> bool {
    let mode = engine
        .handle
        .request_timeout("nvim_get_mode", vec![], Duration::from_secs(5))
        .expect("a live engine answers nvim_get_mode");
    mode.as_map()
        .and_then(|entries| {
            entries
                .iter()
                .find(|(key, _)| key.as_str() == Some("blocking"))
        })
        .and_then(|(_, value)| value.as_bool())
        .expect("nvim_get_mode always reports blocking")
}

#[test]
fn a_restart_after_a_crash_yields_a_live_engine() {
    let engine = Engine::spawn(cfg()).unwrap();
    let dead_pid = engine.pid();
    kill_out_of_band(dead_pid);

    let engine = engine
        .restart(cfg())
        .expect("a crashed engine must restart");

    assert_ne!(
        engine.pid(),
        dead_pid,
        "restart reused the dead child's pid"
    );
    let mode = engine.handle.request("nvim_get_mode", vec![]).unwrap();
    assert!(
        mode.as_map().is_some(),
        "the restarted engine answered nvim_get_mode with {mode:?}"
    );
}

/// The whole point of the flag, end to end: an edit that was never written
/// to disk comes back from nvim's own swap file, and nothing else does. This
/// is the recovery guarantee the restart claims, stated as a test -- no
/// view-side copy of the text exists to produce it from.
#[test]
fn a_restart_recovers_the_unsaved_edit_its_predecessor_left_in_swap() {
    let dir = scratch("recovers-swap");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "what is on disk\n").unwrap();

    let engine = Engine::spawn(editing(&dir, &file)).unwrap();
    // --embed holds startup until a UI attaches, so the file is not even
    // loaded (and has no swap file) before this
    engine.handle.ui_attach(80, 24).unwrap();
    engine
        .handle
        .request_timeout(
            "nvim_buf_set_lines",
            vec![
                0.into(),
                0.into(),
                (-1).into(),
                false.into(),
                rmpv::Value::Array(vec!["never written to disk".into()]),
            ],
            Duration::from_secs(5),
        )
        .unwrap();
    // nvim flushes the swap on its own schedule ('updatetime'/'updatecount'),
    // which is exactly the pacing that bounds what a real crash recovers;
    // `:preserve` is nvim's own way to ask for that flush now, so this test
    // measures the recovery rather than the flush timer
    engine
        .handle
        .request_timeout(
            "nvim_command",
            vec!["preserve".into()],
            Duration::from_secs(5),
        )
        .unwrap();
    kill_out_of_band(engine.pid());

    let engine = engine
        .restart(editing(&dir, &file))
        .expect("a crashed engine must restart");
    engine.handle.ui_attach(80, 24).unwrap();

    let lines = engine
        .handle
        .request_timeout(
            "nvim_buf_get_lines",
            vec![0.into(), 0.into(), (-1).into(), false.into()],
            Duration::from_secs(5),
        )
        .unwrap();
    assert_eq!(
        lines
            .as_array()
            .and_then(|l| l.first())
            .and_then(|l| l.as_str()),
        Some("never written to disk"),
        "the restart did not recover the swap file's contents: {lines:?}"
    );
    assert!(
        !blocking(&engine),
        "the recovered engine is parked at a prompt nobody can answer"
    );

    assert!(
        spawned_with(&engine, "-r"),
        "the recovering child carries no -r: {:?}",
        engine.command_line()
    );
    assert_os_agrees(&engine);
}

/// The falsifier for the flag's condition, and the reason it has one: `-r`
/// with no file to act on lists every swap file it can find, parks the
/// engine at the prompt that acknowledges the list, and exits. A restart
/// with nothing to recover must therefore carry no flag at all, and must
/// still be an editor once a UI attaches.
#[test]
fn a_restart_with_no_file_to_recover_carries_no_flag_and_stays_usable() {
    let engine = Engine::spawn(cfg()).unwrap();
    kill_out_of_band(engine.pid());
    let engine = engine
        .restart(cfg())
        .expect("a crashed engine must restart");

    assert!(
        !spawned_with(&engine, "-r"),
        "a restart with no file to recover must not ask nvim to list swap \
         files: {:?}",
        engine.command_line()
    );
    assert_os_agrees(&engine);

    engine.handle.ui_attach(80, 24).unwrap();
    assert!(
        !blocking(&engine),
        "the restarted engine is parked at a prompt nobody can answer"
    );
    let sum = engine
        .handle
        .request_timeout(
            "nvim_eval",
            vec![rmpv::Value::from("6 * 7")],
            Duration::from_secs(5),
        )
        .unwrap();
    assert_eq!(
        sum.as_u64(),
        Some(42),
        "the restarted engine cannot evaluate"
    );
}

/// Two crashes in a row, the second arriving immediately after the recovery
/// from the first: every child this test starts is reaped, so no zombie
/// survives either teardown.
#[test]
fn a_second_crash_restarts_again_and_leaves_no_zombie() {
    let engine = Engine::spawn(cfg()).unwrap();
    let first = engine.pid();
    kill_out_of_band(first);
    let engine = engine.restart(cfg()).expect("the first restart must work");

    let second = engine.pid();
    kill_out_of_band(second);
    let engine = engine.restart(cfg()).expect("the second restart must work");

    let third = engine.pid();
    assert!(
        first != second && second != third && first != third,
        "three spawns must be three processes: {first}, {second}, {third}"
    );
    engine.handle.request("nvim_get_mode", vec![]).unwrap();
    drop(engine);

    for pid in [first, second, third] {
        wait_until_gone(pid);
    }
}

/// A restart whose replacement cannot start at all reports the failure
/// rather than handing back an engine that is not there. The old child is
/// already gone by then, which is why the caller owes the user a report
/// instead of a silent retry.
#[test]
fn a_restart_that_cannot_come_back_up_reports_it() {
    let engine = Engine::spawn(cfg()).unwrap();
    let dead_pid = engine.pid();
    kill_out_of_band(dead_pid);

    let failed = engine
        .restart(cfg().with_nvim_bin("view-no-such-nvim-binary"))
        .err();

    assert!(
        matches!(failed, Some(view_engine::EngineError::Io(_))),
        "a restart that cannot spawn must report it, got {failed:?}"
    );
    wait_until_gone(dead_pid);
}

/// A wedge is a fact about the engine that was there, never about the one
/// that replaced it. Carrying the old watch across the restart would carry
/// its unanswered probes and their accumulated silence with it, and the
/// replacement -- healthy, answering, seconds old -- would read as wedged on
/// the very first observation, re-raising the banner and the modal it just
/// recovered from.
#[test]
fn a_restart_never_inherits_the_dead_engines_wedge() {
    let (tx, _rx) = std::sync::mpsc::sync_channel(64);
    let mut engine = Engine::spawn(cfg()).unwrap();
    let _ = engine.start_pump(tx.clone());
    // a probe that is genuinely outstanding: the engine's reply reaches the
    // pump's sink, and nothing in this test folds it back through
    // `record_ack`, so the watch keeps waiting for it exactly as a loop
    // stalled past the threshold would
    engine.heartbeat.prober().tick(&engine.handle).unwrap();
    let wedged_at = std::time::Instant::now() + view_engine::HEARTBEAT_WEDGE_THRESHOLD;
    assert_eq!(
        engine.heartbeat.observe_at(false, wedged_at),
        view_engine::Liveness::Wedged,
        "the test never reached the wedge it means to restart out of"
    );

    kill_out_of_band(engine.pid());
    let mut engine = engine.restart(cfg()).expect("a wedged engine must restart");
    let _ = engine.start_pump(tx);

    assert_eq!(
        engine.heartbeat.observe_at(false, wedged_at),
        view_engine::Liveness::Alive,
        "the restarted engine inherited its predecessor's wedge"
    );
    assert_eq!(
        engine.heartbeat.poll_deadline(),
        None,
        "the restarted engine owes an answer to a probe it was never sent"
    );
}

/// Whether the engine's child was handed `flag`, according to the command
/// line the spawn itself recorded.
///
/// The authority on every platform: `Engine::command_line` is read off the
/// `Command` that was spawned, so an assertion made against it is an
/// assertion about what the child actually received rather than about a
/// rule re-derived from the config a second time.
fn spawned_with(engine: &Engine, flag: &str) -> bool {
    engine.command_line().iter().any(|arg| arg == flag)
}

/// Cross-checks the recorded command line against the one the OS reports for
/// the running child, so a recording that stopped matching reality is caught
/// rather than believed. Linux only: `/proc` is the one place a spawned
/// process's argv is readable without cooperation from the process.
#[cfg(target_os = "linux")]
fn assert_os_agrees(engine: &Engine) {
    let pid = engine.pid();
    let raw = std::fs::read(format!("/proc/{pid}/cmdline"))
        .unwrap_or_else(|e| panic!("/proc/{pid}/cmdline unreadable: {e}"));
    assert!(
        !raw.is_empty(),
        "pid {pid} has an empty command line: it is a zombie, not a running engine"
    );
    let os: Vec<String> = raw
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect();
    let recorded: Vec<String> = engine
        .command_line()
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        os, recorded,
        "the OS reports a command line the spawn did not record"
    );
}

/// The cross-check is a Linux-only extra, never the assertion itself: every
/// test above asserts through [`spawned_with`], which answers on every
/// platform.
#[cfg(not(target_os = "linux"))]
fn assert_os_agrees(_engine: &Engine) {}
