//! Engine lifecycle/shutdown tests: `Drop` must never deadlock even with a
//! request in flight, and the graceful-shutdown mechanism must actually
//! take the graceful path with a responsive child and fall back to
//! `SIGKILL` with an unresponsive one, observable via the exit status's
//! signal vs. its code.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;
use view_engine::process::{Engine, EngineConfig};

/// True if a process with the given pid still exists, checked via `kill
/// -0` rather than a name-based `pgrep`, since the pid is known exactly
/// and unambiguously identifies the child (unlike matching by binary name,
/// which risks colliding with unrelated processes on a shared host).
///
/// Unix-only: `kill -0` has no Windows equivalent, and shelling out to a
/// nonexistent `kill` binary would make callers' liveness checks silently
/// pass via `unwrap_or(false)` instead of actually observing the process.
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn drop_with_request_in_flight_does_not_deadlock_and_reaps_child() {
    let engine = Engine::spawn(EngineConfig::default()).unwrap();
    #[cfg(unix)]
    let pid = engine.pid();
    let handle = engine.handle.clone();

    // nvim's event loop is single-threaded: a synchronous sleep keeps this
    // request genuinely in flight (nvim has not yet replied) for the window
    // where we drop the Engine underneath it
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
        rx.recv_timeout(Duration::from_secs(3)).is_ok(),
        "Engine::drop deadlocked with a request in flight"
    );
    let _ = in_flight.join();

    // liveness re-check is unix-only (see pid_alive); the deadlock check
    // above already exercises Drop's non-blocking behavior on every
    // platform, so the reap proof is the only part narrowed here
    #[cfg(unix)]
    assert!(
        !pid_alive(pid),
        "child pid {pid} still alive after Engine dropped: not reaped"
    );
}

#[cfg(unix)]
#[test]
fn shutdown_exits_gracefully_without_sigkill_when_responsive() {
    use std::os::unix::process::ExitStatusExt as _;

    let engine = Engine::spawn(EngineConfig::default()).unwrap();
    let status = engine.shutdown().unwrap();
    assert!(
        status.signal().is_none(),
        "expected a graceful qa! exit, got signal {:?}",
        status.signal()
    );
}

#[cfg(unix)]
#[test]
fn shutdown_force_kills_when_unresponsive_within_timeout() {
    use std::os::unix::process::ExitStatusExt as _;

    let cfg = EngineConfig {
        shutdown_timeout: Duration::from_millis(50),
        ..EngineConfig::default()
    };
    let engine = Engine::spawn(cfg).unwrap();

    // `:sleep` alone does not work here: Neovim's sleep implementation
    // still pumps its event loop (and processes our qa! notification)
    // while waiting. `system()` runs the shell command synchronously via a
    // blocking waitpid on the main thread, which genuinely stalls nvim's
    // event loop, so qa! cannot be processed until it returns; the SIGKILL
    // fallback must fire well before this 3s shell command finishes.
    // request_timeout itself returns quickly regardless, bounded by its own
    // timeout on the write phase, since the response can never arrive while
    // nvim is blocked.
    let _ = engine.handle.request_timeout(
        "nvim_eval",
        vec![rmpv::Value::from("system('sleep 3')")],
        Duration::from_millis(10),
    );

    let status = engine.shutdown().unwrap();
    assert_eq!(
        status.signal(),
        Some(9),
        "expected SIGKILL (signal 9), got {status:?}"
    );
}
