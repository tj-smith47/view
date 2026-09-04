//! What happens to an engine child when the process that owns it dies
//! without running a destructor.
//!
//! `Engine`'s `Drop` covers every exit view chooses and none of the ones it
//! does not: a `SIGKILL`, a harness timeout or a restarted session leaves
//! the child an orphan. A healthy `nvim --embed` still notices its RPC pipe
//! close and exits on its own, so the only child that can prove anything
//! here is one wedged past noticing -- which is exactly the shape found
//! reparented to init at 100% CPU for days.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

/// Set on the re-executed copy of this binary that plays the parent being
/// killed. Absent in an ordinary suite run, where
/// [`the_intermediate_parent`] is a no-op.
const INTERMEDIATE: &str = "VIEW_ENGINE_ORPHAN_INTERMEDIATE";

/// The line the intermediate writes to stderr once its engine is wedged.
const PID_MARKER: &str = "orphan-reaping engine pid ";

/// How long the wedged engine keeps spinning if nothing kills it.
///
/// A bound rather than a wait: the assertions below are decided inside
/// [`REAPED`], and this only puts a ceiling on what a failing run can leave
/// behind. It is the discriminator too -- an engine that outlives its parent
/// on stdin EOF alone would not go until this expires, an order of magnitude
/// past the window the reap is asserted in.
const BUSY_NANOS: u64 = 30_000_000_000;

/// How long the orphan is given to leave the process table once its parent
/// is gone.
const REAPED: std::time::Duration = std::time::Duration::from_secs(3);

/// The parent half of the pin, run in a re-executed copy of this binary so
/// there is a process to kill that is not the test harness itself.
///
/// Wedges its engine in a synchronous Lua loop -- one that pumps no event
/// loop, so the child reads no RPC, notices no closed pipe and ignores
/// `SIGTERM` -- reports the child's pid, and parks on a read of its own
/// standard input that only its parent can end.
#[test]
fn the_intermediate_parent() {
    use std::io::{Read, Write};

    if std::env::var_os(INTERMEDIATE).is_none() {
        return;
    }
    let engine =
        view_engine::process::Engine::spawn(view_engine::process::EngineConfig::isolated())
            .unwrap();
    engine
        .handle
        .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS)
        .unwrap();
    // typed rather than requested: a blocking request for work that never
    // returns could not be waited on and then reported
    engine
        .handle
        .input(&format!(
            ":lua local t=vim.uv.hrtime() while {BUSY_NANOS} - (vim.uv.hrtime()-t) > 0 do end<CR>"
        ))
        .unwrap();
    // the engine's own silence is the readiness condition: one still reading
    // the command line answers this promptly, and one inside the loop
    // answers it not at all
    let deadline = common::rpc_poll_deadline_for(30);
    while engine.handle.get_mode().is_ok() {
        assert!(
            std::time::Instant::now() < deadline,
            "the engine kept answering after being told to spin, so it never wedged"
        );
    }
    let mut stderr = std::io::stderr();
    writeln!(stderr, "{PID_MARKER}{}", engine.pid()).unwrap();
    stderr.flush().unwrap();
    let mut byte = [0_u8; 1];
    let _ = std::io::stdin().read(&mut byte);
}

/// A wedged engine dies with the parent that owns it, even when that parent
/// dies by `SIGKILL` and runs no destructor at all.
///
/// Linux only: `PR_SET_PDEATHSIG` is what covers this, and no equivalent is
/// armed on the other two platforms (see `arm_parent_death`).
#[test]
fn a_wedged_engine_dies_with_a_parent_that_was_killed_outright() {
    #[cfg(not(target_os = "linux"))]
    {
        view_test_support::announce_skip(
            "a_wedged_engine_dies_with_a_parent_that_was_killed_outright",
            "no parent-death signal is armed off Linux",
        );
    }
    #[cfg(target_os = "linux")]
    {
        use std::io::BufRead;

        let mut parent = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["the_intermediate_parent", "--exact", "--nocapture"])
            .env(INTERMEDIATE, "1")
            // held open for the whole test: the intermediate parks on a read of
            // this pipe, so the only thing that ends it is the kill below
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let reader = std::io::BufReader::new(parent.stderr.take().unwrap());
        let engine_pid = reader
            .lines()
            .map_while(Result::ok)
            .find_map(|line| {
                line.strip_prefix(PID_MARKER)
                    .and_then(|pid| pid.trim().parse::<u32>().ok())
            })
            .expect("the intermediate must report the pid of the engine it wedged");
        assert!(
            common::pid_in_process_table(engine_pid),
            "the engine was already gone before its parent was \
         killed, so nothing below is evidence about the kill"
        );

        parent.kill().unwrap();
        parent.wait().unwrap();

        let deadline = std::time::Instant::now() + view_test_support::host_deadline(REAPED);
        while common::pid_in_process_table(engine_pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "the engine outlived the parent that owned it: a child \
             wedged in synchronous Lua reads no closed pipe and ignores \
             SIGTERM, so without a parent-death signal it spins until \
             something kills it"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

/// An engine spawned from a thread that then exits keeps running.
///
/// The parent-death signal names the *thread* that forked the child, not the
/// process, so arming it on whichever thread happened to call `spawn` kills
/// the engine the moment that thread returns -- which is what view's own
/// background attach thread does. That regression surfaced as a live session
/// painting `E305` from the dead child's swap file, a failure whose name
/// says nothing about parent-death signals; this one says it.
#[test]
fn an_engine_spawned_from_a_thread_outlives_that_thread() {
    #[cfg(not(target_os = "linux"))]
    {
        view_test_support::announce_skip(
            "an_engine_spawned_from_a_thread_outlives_that_thread",
            "no parent-death signal is armed off Linux, so no thread owns one",
        );
        return;
    }
    let engine = std::thread::spawn(|| {
        view_engine::process::Engine::spawn(view_engine::process::EngineConfig::isolated())
    })
    .join()
    .expect("the spawning thread must not panic")
    .expect("the engine starts");

    assert!(
        engine.handle.get_mode().is_ok(),
        "the engine stopped answering once the thread that spawned it \
         exited, so its parent-death signal was armed against that thread \
         rather than the process"
    );
}
