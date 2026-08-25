//! Proves the control arm's mechanism before any row depends on it: an
//! nvim `--remote-ui` client, hosted in the same pty the measured arms
//! use, draws a headless server's buffer and echoes typed input into it.
//!
//! Unix only. The arm exists to attribute a latency residual measured on
//! the two unix classes, and the socket handshake it relies on is a unix
//! socket path rather than a named pipe.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use view_bench::boundaries::screen_holds;
use view_bench::remote_ui::RemoteUiServer;
use view_bench::session::{BenchSession, SettleBound, SpawnSpec};
use view_test_support::ScratchDir;

/// How long the client is given to go quiet after spawning, and the quiet
/// window it has to hold, before the host's own contention is accounted
/// for. The quiet window is the code's own constant and does not move; the
/// rest of the bound is a pty spawning nvim twice and is the host's.
const SETTLE_BOUND: Duration = Duration::from_secs(30);
const QUIET: Duration = Duration::from_millis(500);

/// How long a keystroke is given to come back on the client's screen,
/// likewise before scaling: all of it is a round trip through two live
/// processes.
const TYPED_BOUND: Duration = Duration::from_secs(10);

/// Resolves the engine the rest of the harness measures, skipping rather
/// than failing when it is absent: this is a mechanism proof, not a
/// dependency check, and `task ci` already verifies the pin separately.
fn nvim_bin() -> Option<PathBuf> {
    let bin =
        std::env::var_os("VIEW_NVIM_BIN").map_or_else(|| PathBuf::from("nvim"), PathBuf::from);
    std::process::Command::new(&bin)
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|_| bin)
}

/// A scratch root for one case, removed by the guard it is returned as on
/// every exit path -- a failing assertion included.
fn scratch(tag: &str) -> ScratchDir {
    ScratchDir::new(&format!("remote-ui-{tag}")).unwrap()
}

#[test]
fn a_remote_ui_client_draws_the_headless_servers_buffer_and_echoes_typing() {
    let Some(nvim) = nvim_bin() else {
        // `cargo test` swallows stdout/stderr for a passing test, so a
        // plain eprintln here is indistinguishable from an actual pass in
        // the default run's output -- add the same `::warning::` workflow
        // command `view-harness`'s bench binary uses for platform-skipped
        // cells (see `skip_announcements` in `view-harness/src/bin/bench.rs`)
        // so a skip on CI still surfaces on the checks page instead of
        // silently reading as a verified mechanism proof.
        let reason = "no nvim on PATH or at $VIEW_NVIM_BIN";
        println!("skipping a_remote_ui_client_draws_the_headless_servers_buffer_and_echoes_typing: {reason}");
        if std::env::var("GITHUB_ACTIONS").is_ok_and(|v| v == "true") {
            println!("::warning::remote_ui mechanism test skipped: {reason}");
        }
        return;
    };
    let dir = scratch("echo");
    let file = dir.join("scratch.txt");
    std::fs::write(&file, "REMOTEUIFIXTURE\n").unwrap();

    let bare = SpawnSpec {
        program: nvim,
        args: vec![
            OsString::from("-u"),
            OsString::from("NONE"),
            OsString::from("-n"),
            file.clone().into_os_string(),
        ],
        env: vec![(OsString::from("TERM"), OsString::from("xterm-256color"))],
        cwd: Some(dir.to_path_buf()),
    };

    let server = RemoteUiServer::start(&bare, dir.join("ui.sock")).expect("headless server");
    let mut client = BenchSession::spawn(&server.client_spec(&bare)).expect("remote ui client");
    assert!(
        client.settle(SettleBound {
            quiet: QUIET,
            deadline: view_test_support::HostBudget::new(QUIET, SETTLE_BOUND.saturating_sub(QUIET))
                .total(),
        }),
        "the client never went quiet; screen:\n{}",
        client.screen_text()
    );

    assert!(
        client.with_screen(|screen| screen_holds(screen, "REMOTEUIFIXTURE")),
        "the client drew no buffer the server holds, so the arm would be measuring an empty \
         frame rather than a round trip; screen:\n{}",
        client.screen_text()
    );

    client.send(b"ozREMOTEUITYPED").unwrap();
    let deadline = std::time::Instant::now() + view_test_support::host_deadline(TYPED_BOUND);
    let mut seen = false;
    while std::time::Instant::now() < deadline {
        if client.with_screen(|screen| screen_holds(screen, "REMOTEUITYPED")) {
            seen = true;
            break;
        }
        std::thread::yield_now();
    }
    assert!(
        seen,
        "typing into the client never reached the screen, so the arm cannot time a keypress; \
         screen:\n{}",
        client.screen_text()
    );

    client.shutdown();
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}
