//! Reproduces a write-phase hang: a peer that never reads its stdin,
//! combined with a payload larger than the OS pipe buffer, used to block
//! `request_timeout` inside the `write()` syscall itself, past its own
//! timeout. The fix moves writes onto a dedicated writer thread fed by a
//! channel, so the calling thread's `recv_timeout` bounds the whole call
//! regardless of how long the write takes.
//!
//! Cross-platform: the fixture is a compiled binary, and a peer that
//! never drains its stdin blocks a large write on every platform this
//! builds for.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use view_engine::{EngineError, EngineHandle};

#[test]
fn request_timeout_bounds_write_phase_against_wedged_peer() {
    // the hang fixture never touches stdin or stdout and blocks until
    // killed: exactly the "peer never reads stdin" shape this test needs,
    // reused rather than duplicated as a second fixture.
    let mut child = Command::new(env!("CARGO_BIN_EXE_view-engine-hang-fixture"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let stdin = child.stdin.take().unwrap();
    let (handle, _notifications) = EngineHandle::start(stdout, stdin);

    // well over the ~64 KiB default pipe buffer, so the writer thread's
    // write() call is guaranteed to block on this fixture, which never
    // drains its stdin
    let huge = rmpv::Value::from("x".repeat(256 * 1024));
    let timeout = Duration::from_millis(200);
    // the timeout is the engine's to honour; only the slack around it is
    // the host's, so the bound is the one plus a scaled share of the other
    let slack = Duration::from_millis(1800);
    let start = Instant::now();
    let result = handle.request_timeout("nvim_eval", vec![huge], timeout);
    let elapsed = start.elapsed();

    assert!(
        matches!(result, Err(EngineError::Timeout { .. })),
        "expected Timeout, got {result:?}"
    );
    assert!(
        elapsed < timeout + view_test_support::host_deadline(slack),
        "request_timeout took {elapsed:?} against a {timeout:?} timeout plus \
         {slack:?} of host-scaled slack; the write phase is leaking outside \
         the timeout again"
    );

    let _ = child.kill();
    let _ = child.wait();
}
