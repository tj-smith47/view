//! Proves the control arm's mechanism before any row depends on it: an
//! nvim `--remote-ui` client, hosted in the same pty the measured arms
//! use, draws a headless server's buffer and echoes typed input into it.
//!
//! Unix only. The arm exists to attribute a latency residual measured on
//! the two unix classes, and the socket handshake it relies on is a unix
//! socket path rather than a named pipe.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

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

/// The environment entry that turns a run of this binary into the
/// intermediate harness the reaping case kills.
const INTERMEDIATE: &str = "VIEW_BENCH_REMOTE_UI_INTERMEDIATE";

/// How long the intermediate harness is given to have a server of its own.
const SERVER_APPEARS: Duration = Duration::from_secs(30);

/// How long the server is given to leave once its harness has been killed.
///
/// The discriminator, not slack: an unreaped server does not leave at all.
const REAPED: Duration = Duration::from_secs(3);

/// The step between two looks at the process table.
const POLL: Duration = Duration::from_millis(10);

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

/// The bare-nvim spec both cases below start a server from, over a fixture
/// file written into `dir`.
fn bare_spec(nvim: PathBuf, dir: &ScratchDir) -> SpawnSpec {
    let file = dir.join("scratch.txt");
    std::fs::write(&file, "REMOTEUIFIXTURE\n").unwrap();
    SpawnSpec {
        program: nvim,
        args: vec![
            OsString::from("-u"),
            OsString::from("NONE"),
            OsString::from("-n"),
            file.into_os_string(),
        ],
        env: vec![(OsString::from("TERM"), OsString::from("xterm-256color"))],
        cwd: Some(dir.to_path_buf()),
        measured_program: None,
    }
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
    let bare = bare_spec(nvim, &dir);

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

/// Turns this binary into a harness that owns a live control server and then
/// stops, so the case below has something to kill outright.
///
/// A test rather than a fixture binary: the server it starts is the one the
/// rows use, spawned through the same call, and a second crate built to hold
/// that call would be proving its own copy of it. Inert unless the case
/// below re-execs this binary with `INTERMEDIATE` set, which is what keeps
/// an ordinary `cargo test` run from parking here.
#[test]
fn the_intermediate_parent() {
    if std::env::var_os(INTERMEDIATE).is_none() {
        return;
    }
    let Some(nvim) = nvim_bin() else {
        return;
    };
    let dir = scratch("orphan-intermediate");
    let bare = bare_spec(nvim, &dir);
    let _server = RemoteUiServer::start(&bare, dir.join("ui.sock")).expect("headless server");
    // parks here holding the server: the case below reads the child off
    // procfs and kills this process without ever writing a byte
    let mut byte = [0_u8; 1];
    let _ = std::io::Read::read(&mut std::io::stdin(), &mut byte);
}

/// A control server does not outlive the harness that started it, even when
/// that harness is killed outright and its `Drop` never runs.
///
/// The server has no pty to hang up and no controlling terminal, so nothing
/// about a dead harness reaches it on its own: a headless nvim left holding
/// its socket is a stray that lives until the host is rebooted.
#[cfg(target_os = "linux")]
#[test]
fn a_control_server_dies_with_a_harness_that_was_killed_outright() {
    if nvim_bin().is_none() {
        let reason = "no nvim on PATH or at $VIEW_NVIM_BIN";
        println!(
            "skipping a_control_server_dies_with_a_harness_that_was_killed_outright: {reason}"
        );
        if std::env::var("GITHUB_ACTIONS").is_ok_and(|v| v == "true") {
            println!("::warning::remote_ui reaping test skipped: {reason}");
        }
        return;
    }
    let mut harness = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["the_intermediate_parent", "--exact", "--nocapture"])
        .env(INTERMEDIATE, "1")
        // the park above reads this, so it stays open rather than answering
        // EOF the moment the case starts
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("re-exec this test binary as the intermediate harness");

    let deadline = std::time::Instant::now() + view_test_support::host_deadline(SERVER_APPEARS);
    let mut server = None;
    while std::time::Instant::now() < deadline {
        server = view_test_support::child_pids(harness.id())
            .into_iter()
            .find(|pid| {
                std::fs::read_to_string(format!("/proc/{pid}/comm"))
                    .is_ok_and(|comm| comm.trim() == "nvim")
            });
        if server.is_some() {
            break;
        }
        std::thread::sleep(POLL);
    }
    let Some(server) = server else {
        let _ = harness.kill();
        let _ = harness.wait();
        panic!("the intermediate harness never started a server to orphan");
    };

    harness
        .kill()
        .expect("kill the intermediate harness outright");
    harness.wait().expect("reap the intermediate harness");

    let gone = std::time::Instant::now() + view_test_support::host_deadline(REAPED);
    while std::path::Path::new(&format!("/proc/{server}")).exists() {
        assert!(
            std::time::Instant::now() < gone,
            "the control server {server} outlived the harness that started it, with no terminal \
             and no socket peer left to end it"
        );
        std::thread::sleep(POLL);
    }
}
