//! Engine lifecycle/shutdown tests: `Drop` must never deadlock even with a
//! request in flight, and the graceful-shutdown mechanism must actually
//! take the graceful path with a responsive child and fall back to
//! `SIGKILL` with an unresponsive one, read off the `ShutdownPath` the code
//! records at the branch it takes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;
use view_core::msg::Msg;
use view_engine::process::{Engine, EngineConfig, ShutdownPath};

mod common;

/// Kills `pid` unconditionally, in each platform's own spelling.
///
/// Windows is covered rather than left a no-op: this is what a failing test
/// uses to avoid orphaning a wedged editor, and a shared Windows host
/// collects those just as readily as a unix one.
fn kill_pid(pid: u32) {
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .status();
    }
    #[cfg(not(any(unix, windows)))]
    let _ = pid;
}

/// Kills the child it holds unless [`disarm`](Self::disarm)ed first, so a
/// test that fails while its child is wedged does not leave that child
/// running: the failure arrives precisely because nothing in the test can
/// reach a shutdown that never returned, and a shared machine collects one
/// orphaned editor per such failure.
///
/// Disarmed on every path that has already reaped the child, never left to
/// fire on a pid the test is done with: a reaped pid can be reused, and
/// signalling one is signalling whichever unrelated process now holds it.
struct KillOnDrop(Option<u32>);

impl KillOnDrop {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            kill_pid(pid);
        }
    }
}

#[test]
fn drop_with_request_in_flight_does_not_deadlock_and_reaps_child() {
    let engine = Engine::spawn(EngineConfig::isolated()).unwrap();
    let pid = engine.pid();
    // the drop under test happens on another thread, so the child outlives
    // a deadlock there: nothing left holds the Engine whose own Drop would
    // reap it
    let mut orphan_guard = KillOnDrop(Some(pid));
    let handle = engine.handle.clone();

    // nvim's event loop is single-threaded: a synchronous sleep keeps this
    // request genuinely in flight (nvim has not yet replied) across the
    // window in which the Engine is dropped underneath it
    let in_flight = std::thread::spawn(move || {
        let _ = handle.request("nvim_command", vec![rmpv::Value::from("sleep 200m")]);
    });
    std::thread::sleep(Duration::from_millis(20));

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(engine);
        let _ = tx.send(());
    });
    assert!(
        rx.recv_timeout(common::rpc_deadline()).is_ok(),
        "Engine::drop deadlocked with a request in flight"
    );
    // the drop returned, so the child is already reaped and its pid is no
    // longer this test's to signal
    orphan_guard.disarm();
    let _ = in_flight.join();

    assert!(
        !common::pid_in_process_table(pid),
        "child pid {pid} still in the process table after Engine dropped: \
         not reaped"
    );
}

/// How long a test here waits for nvim to act on `qa!` before calling it
/// wedged. Sized as a liveness bound rather than a performance one: a child
/// that has not run at all in a minute is not a descheduled child, whatever
/// the host's load.
const GRACEFUL_EXIT_LIVENESS_BOUND: Duration = Duration::from_secs(60);

/// The engine deadline the tests of responsive shutdown configure,
/// deliberately far past [`GRACEFUL_EXIT_LIVENESS_BOUND`] so it cannot
/// expire while any of them is still watching.
///
/// Which branch a responsive child drives the code down is what those tests
/// assert, so a deadline able to fire mid-test would make the answer a
/// function of host load. No budget value avoids that: one tight enough to
/// mean anything on a quiet host force-kills a genuinely responsive child on
/// a loaded one. Large but finite because `graceful_kill` turns it into an
/// `Instant` deadline, and an effectively infinite duration overflows that
/// addition rather than meaning "never".
const GRACEFUL_DEADLINE_PAST_THE_TEST: Duration = Duration::from_secs(300);

/// Runs `work` against the child `pid` on its own thread and hands back
/// what it produced, failing the test if nothing arrives within
/// [`GRACEFUL_EXIT_LIVENESS_BOUND`] and killing `pid` on the way out.
///
/// Keeps the wait under the test's own control rather than the engine's.
/// An engine deadline able to expire mid-test picks the graceful-vs-forced
/// branch by host load, and reports the result as the wrong branch; an
/// expiry here can only mean the child never exited at all, and says that.
///
/// `work` consumes or borrows the `Engine` on the worker thread, so on the
/// expiry path nothing left in the test owns the child: no `Drop` will run
/// for it, and the very condition being reported is that the shutdown it
/// was handed to never returned.
fn within_liveness_bound<T: Send + 'static>(
    subject: &str,
    pid: u32,
    work: impl FnOnce() -> T + Send + 'static,
) -> T {
    let mut orphan_guard = KillOnDrop(Some(pid));
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    let wedged = format!(
        "{subject} produced nothing within {GRACEFUL_EXIT_LIVENESS_BOUND:?}: nvim never exited \
         after qa!, so it is wedged rather than merely slow to be scheduled. The child was \
         killed on the way out of this failure, since nothing here can reach a shutdown that \
         never returned"
    );
    let produced = rx
        .recv_timeout(GRACEFUL_EXIT_LIVENESS_BOUND)
        .expect(&wedged);
    // work returned, so it reaped the child itself and the pid is no
    // longer this test's to signal
    orphan_guard.disarm();
    let _ = worker.join();
    produced
}

#[test]
fn shutdown_exits_gracefully_without_sigkill_when_responsive() {
    let cfg = EngineConfig::isolated().with_shutdown_timeout(GRACEFUL_DEADLINE_PAST_THE_TEST);
    let engine = Engine::spawn(cfg).unwrap();
    // named by pid so a failure below points at the process it concerns,
    // not just at the fact that one existed
    let pid = engine.pid();
    let subject = format!("shutdown of nvim pid {pid}");
    let outcome = within_liveness_bound(&subject, pid, move || engine.shutdown()).unwrap();

    assert_eq!(
        outcome.path,
        ShutdownPath::Graceful,
        "a responsive nvim was force-killed rather than left to exit on its \
         own; status {:?}",
        outcome.status
    );
    // which signal ended a process is a unix-only fact; the path assertion
    // above is the cross-platform half and stays ungated
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        assert!(
            outcome.status.signal().is_none(),
            "the graceful path ran but the child died by signal {:?}: it went \
             down of something other than the qa! it was asked to perform",
            outcome.status.signal()
        );
    }
}

#[test]
fn shutdown_force_kills_when_unresponsive_within_timeout() {
    let cfg = EngineConfig::isolated().with_shutdown_timeout(Duration::from_millis(50));
    let engine = Engine::spawn(cfg).unwrap();

    // `:sleep` alone does not work here: Neovim's sleep implementation
    // still pumps its event loop (and processes the qa! notification)
    // while waiting. `system()` runs the shell command synchronously via a
    // blocking wait on the main thread, which genuinely stalls nvim's event
    // loop, so qa! cannot be processed until it returns; the forced-kill
    // fallback must fire well before this ~3s shell command finishes.
    // request_timeout itself returns quickly regardless, bounded by its own
    // timeout on the write phase, since the response can never arrive while
    // nvim is blocked.
    //
    // The command is spelled per platform because nvim runs it through
    // `&shell`: `sleep` is not a thing cmd.exe can run, and a stall command
    // that silently fails to stall would leave a responsive child taking the
    // graceful path and failing this test for the wrong reason.
    let stall = if cfg!(windows) {
        "system('ping -n 4 127.0.0.1')"
    } else {
        "system('sleep 3')"
    };
    let _ = engine.handle.request_timeout(
        "nvim_eval",
        vec![rmpv::Value::from(stall)],
        Duration::from_millis(10),
    );

    let outcome = engine.shutdown().unwrap();
    assert_eq!(
        outcome.path,
        ShutdownPath::Forced,
        "a stalled nvim was reported as exiting on its own; status {:?}",
        outcome.status
    );
    // as above: SIGKILL is the unix spelling of the forced path, while the
    // path assertion itself holds on every platform
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        assert_eq!(
            outcome.status.signal(),
            Some(9),
            "the forced path ran but the child did not die of SIGKILL; status {:?}",
            outcome.status
        );
    }
}

/// Drains `msgs` until the reader thread's terminal `Msg::EngineStopped`
/// arrives, returning whether it did inside
/// [`GRACEFUL_EXIT_LIVENESS_BOUND`].
///
/// Draining rather than reading one message: the same sink carries redraw
/// and request traffic, and a `qa!` produces some of it on the way out, so
/// the terminal signal is not necessarily the first thing to arrive.
fn wait_for_engine_stopped(msgs: &std::sync::mpsc::Receiver<Msg>) -> bool {
    let deadline = std::time::Instant::now() + GRACEFUL_EXIT_LIVENESS_BOUND;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match msgs.recv_timeout(left) {
            Ok(Msg::EngineStopped { .. }) => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
}

/// `wait_exit` is what the runtime loop calls in response to
/// `Msg::EngineStopped` (the reader thread already saw the connection end);
/// it must still resolve a real exit status via the same graceful-kill
/// machinery `shutdown`/`Drop` use, not hang or fabricate one.
#[test]
fn wait_exit_resolves_a_normally_exiting_child() {
    let cfg = EngineConfig::isolated().with_shutdown_timeout(GRACEFUL_DEADLINE_PAST_THE_TEST);
    let mut engine = Engine::spawn(cfg).unwrap();
    let (sink, msgs) = std::sync::mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(sink);

    // ask nvim to exit on its own, independent of wait_exit's own qa!, so
    // this proves wait_exit correctly observes a child that is already
    // exiting rather than being the sole cause of the exit
    engine
        .handle
        .notify("nvim_command", vec![rmpv::Value::from("qa!")])
        .unwrap();

    // that the exit happened is then read off the connection rather than
    // assumed after a sleep: EngineStopped is what the reader thread routes
    // the instant nvim's stream ends, and is the very signal the runtime
    // loop calls wait_exit in response to. A request cannot stand in for it,
    // because an RPC-issued qa! only sets nvim exiting and lets the messages
    // already queued behind it be answered on the way out
    assert!(
        wait_for_engine_stopped(&msgs),
        "nvim's channel never ended after the qa! it was sent, so the child \
         was not already exiting when wait_exit ran"
    );

    let pid = engine.pid();
    let subject = format!("wait_exit on nvim pid {pid}");
    let info = within_liveness_bound(&subject, pid, move || engine.wait_exit());
    assert_eq!(
        info.code,
        Some(0),
        "expected a clean exit code, got {info:?}"
    );
    assert!(!info.by_signal);
}

/// The seam a failed write asks through: it must report on the child, never
/// end it.
///
/// The bug this pins had the runtime resolve every failed write by calling
/// `stop_report`, whose `wait_exit` opens with `qa!`. Against a live nvim
/// that is not a diagnosis, it is a cause of death: nvim runs `VimLeavePre`,
/// the exit comes back announced and clean, and the caller reads a transient
/// write error as the user's own `:q` and leaves 0 with nothing on screen to
/// say why.
#[test]
fn a_live_child_reports_no_stop_and_is_not_asked_to_quit() {
    let mut engine = Engine::spawn(EngineConfig::isolated()).unwrap();
    let pid = engine.pid();
    let mut orphan_guard = KillOnDrop(Some(pid));
    engine.handle.ui_attach(80, 24).unwrap();

    assert!(
        engine.stop_report_if_exited().is_none(),
        "a running child has no stop to report, and saying otherwise is what \
         turns a write error into an exit"
    );

    // the connection still answers, so nothing was sent that would make it
    // stop answering: an engine handed `qa!` is inside VimLeavePre or gone
    assert_eq!(
        engine.handle.eval_str("1 + 1").unwrap(),
        "2",
        "the engine stopped answering, so the observation ended the session \
         it was supposed to be observing"
    );
    assert!(
        engine.stop_report_if_exited().is_none(),
        "asking twice must not be what kills it either"
    );

    drop(engine);
    orphan_guard.disarm();
}

/// The other half of the same seam: a child that is on its way out must be
/// reported as a stop even though `try_wait` has not caught it yet.
///
/// The window is real -- nvim closes the embed channel, then writes shada
/// and runs `VimLeave` before the process actually goes -- and a write
/// losing its pipe inside it is the user's own `:q` arriving a hair ahead of
/// the reader's `Msg::EngineStopped`. Calling that "still running" hands it
/// to supervision, which on the shipped default respawns the editor the user
/// just closed.
#[test]
fn a_child_whose_connection_closed_is_reported_as_a_stop_not_a_broken_write() {
    let mut engine = Engine::spawn(EngineConfig::isolated()).unwrap();
    let pid = engine.pid();
    let mut orphan_guard = KillOnDrop(Some(pid));
    engine.handle.ui_attach(80, 24).unwrap();

    // nvim's own exit, not a teardown of view's: `command` never answers
    // because the process leaves mid-request, so its Err is the barrier that
    // says the exit has genuinely started
    assert!(engine.handle.command("qa!").is_err());

    let (exit, _) = engine
        .stop_report_if_exited()
        .expect("a connection that has closed is a stop, whatever try_wait has caught up with");
    assert_eq!(exit.code, Some(0), "nvim's own status, got {exit:?}");
    orphan_guard.disarm();
}

/// A restart's teardown never asks a live child to quit.
///
/// `:qa!` unloads nvim's buffers normally, which deletes their swap files --
/// the very files the restart exists to bring the user's unsaved work back
/// from. Killing the child leaves them on disk, and the exit status is the
/// evidence of which of the two happened: a graceful `qa!` leaves 0, a
/// signal leaves no code at all.
#[cfg(unix)]
#[test]
fn a_restart_teardown_kills_the_child_rather_than_asking_it_to_quit() {
    use std::os::unix::process::ExitStatusExt;

    let mut engine = Engine::spawn(EngineConfig::isolated()).unwrap();
    let mut orphan_guard = KillOnDrop(Some(engine.pid()));
    engine.handle.ui_attach(80, 24).unwrap();

    let status = engine.kill_exit().unwrap();
    orphan_guard.disarm();
    assert_eq!(
        status.signal(),
        Some(9),
        "the child left on its own terms, so the teardown asked it to quit and took the \
         user's swap files with it: {status:?}"
    );
}

#[cfg(unix)]
#[test]
fn wait_exit_maps_external_signal_kill_to_128_plus_signal() {
    let mut engine = Engine::spawn(EngineConfig::isolated()).unwrap();
    let pid = engine.pid();
    std::process::Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let info = engine.wait_exit();
    assert_eq!(info.code, Some(137), "expected 128+SIGKILL, got {info:?}");
    assert!(info.by_signal);
}
