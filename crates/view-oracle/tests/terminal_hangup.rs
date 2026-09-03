//! What a live `view` does when the terminal underneath it goes away
//! without a signal.
//!
//! A pty master closing delivers `SIGHUP` only to a session leader with
//! that pty as its controlling terminal. A driver that spawns `view` on a
//! pty slave and never calls `setsid` -- which is how several of this
//! tree's own live drivers spawn it -- leaves the editor holding a
//! descriptor that answers `POLLHUP` forever and `EIO` on every read, with
//! no signal to end it. So the pty here is rolled by hand and deliberately
//! left without the `setsid`/`TIOCSCTTY` pair `PtySession` performs: with
//! them the kernel's own `SIGHUP` would end the session and this file would
//! prove nothing.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(unix)]

mod common;

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use view_oracle::QueryPolicy;

/// How long the session is given to reach the point where its engine child
/// exists, which is what makes it a session with something to lose.
const STARTS: Duration = Duration::from_secs(20);

/// How long the session is given to leave once its terminal has gone.
///
/// The discriminator, not slack: the defect this pins is an unbounded spin,
/// so a run that fails here fails by never ending at all.
const LEAVES: Duration = Duration::from_secs(10);

/// How long the pty must stay silent before the session counts as settled,
/// before the host's load widens it.
const SETTLED: Duration = Duration::from_millis(500);

/// The status a shell reports for a process a `SIGHUP` ended, and the one
/// `view-core`'s `Msg::Terminated` arm produces.
const HANGUP_STATUS: i32 = 129;

/// The pid of the `nvim` `view` spawned, once it has one.
#[cfg(target_os = "linux")]
fn engine_child_of(pid: u32) -> Option<u32> {
    view_test_support::child_pids(pid)
        .into_iter()
        .find(|child| {
            std::fs::read_to_string(format!("/proc/{child}/comm"))
                .is_ok_and(|comm| comm.trim() == "nvim")
        })
}

/// Whether the OS still holds a process-table entry for `pid`.
#[cfg(target_os = "linux")]
fn still_running(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// A `view` session ends through its own teardown when the pty master
/// closes under it, and takes its engine with it.
///
/// Without that, `view` does not merely linger: crossterm's unix event
/// source retries a read that answers `EIO` forever, so the process spins at
/// 100% CPU with no terminal to paint to and nothing that will ever end it.
#[test]
fn a_session_whose_pty_master_closed_ends_instead_of_spinning() {
    let paths = common::ScratchPaths::new("terminal-hangup");
    let winsize = nix::pty::Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = nix::pty::openpty(Some(&winsize), None).expect("openpty for the hangup session");
    // `openpty(3)` hands back a master with `FD_CLOEXEC` clear, so a child
    // spawned after it inherits the master and holds the pty open against
    // this test's own close -- the hangup would never happen, and the
    // session under test would be asserted against a terminal that is still
    // there. nvim inherits it from view in turn, so the leak outlives even
    // the process the test is watching.
    nix::fcntl::fcntl(
        &pty.master,
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
    )
    .expect("keep the pty master out of the session's own descriptors");
    let mut cmd = std::process::Command::new(common::view_bin_path());
    view_oracle::make_hermetic(&mut cmd).expect("hermetic env for the hangup session");
    for var in [
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ] {
        cmd.env(var, common::xdg_home(&paths.isolated_home, var));
    }
    common::disable_native_features(&paths.isolated_home);
    let slave_dup = |slot: &str| {
        std::process::Stdio::from(
            nix::unistd::dup(&pty.slave)
                .unwrap_or_else(|err| panic!("dup pty slave for {slot}: {err}")),
        )
    };
    cmd.stdin(slave_dup("stdin"));
    cmd.stdout(slave_dup("stdout"));
    cmd.stderr(slave_dup("stderr"));
    // no setsid and no TIOCSCTTY on purpose: see this file's module doc
    let mut child = cmd.spawn().expect("spawn view on the hand-rolled pty");
    // the child owns its own dups; a copy left open here would keep the pty
    // from ever hanging up
    drop(pty.slave);

    let mut master = std::fs::File::from(pty.master);
    // non-blocking so one thread can both answer the session's capability
    // queries and watch for its engine, and so the master has no reader
    // thread holding a second descriptor open when it is time to close it
    nix::fcntl::fcntl(
        &master,
        nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
    )
    .expect("set the pty master non-blocking");
    let mut responder = view_oracle::QueryResponder::for_policy(QueryPolicy::AnswerFullTier);
    let mut screen = vt100::Parser::new(24, 80, 0);
    let mut written = 0_usize;
    let deadline = Instant::now() + view_test_support::host_deadline(STARTS);
    // hoisted out of the loop rather than scaled inside it: `host_deadline`
    // reads the host's load average, and the loop below spins on a
    // non-blocking read
    let settled = view_test_support::host_deadline(SETTLED);
    let mut quiet_since = Instant::now();
    let engine = loop {
        let mut buf = [0_u8; 4096];
        match master.read(&mut buf) {
            Ok(0) => panic!("the pty hung up before the session ever started"),
            Ok(read) => {
                let replies = responder.replies_for(&buf[..read]);
                if !replies.is_empty() {
                    // best effort: an unanswered query only costs the
                    // session a capability, and the terminal it would be
                    // resolved for is the one this test is about to remove
                    let _ = master.write_all(&replies);
                    let _ = master.flush();
                }
                screen.process(&buf[..read]);
                written += read;
                quiet_since = Instant::now();
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(err) => panic!("reading the pty master failed: {err}"),
        }
        // the engine's existence alone is too early to close anything on:
        // view spawns nvim and then paints "waiting for nvim..." until the
        // handshake lands, and a terminal taken away inside that window is
        // a startup failure rather than the hangup under test
        let up = written > 0
            && quiet_since.elapsed() >= settled
            && !screen.screen().contents().contains("waiting for nvim");
        if up {
            break engine_child_of(child.id());
        }
        assert!(
            Instant::now() < deadline,
            "the session never came up; screen:\n{}",
            screen.screen().contents()
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let rendered = screen.screen().contents();

    drop(master);

    let leave_by = Instant::now() + view_test_support::host_deadline(LEAVES);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait on the session") {
            break status;
        }
        assert!(
            Instant::now() < leave_by,
            "the session outlived the terminal it was painting to: a read on a \
             hung-up pty answers EIO forever, and crossterm's own event source \
             retries it rather than reporting it, so nothing ends the process"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(
        status.code(),
        Some(HANGUP_STATUS),
        "a hangup must leave through the same teardown a fatal signal takes, \
         which is what states the exit status; screen:\n{rendered}"
    );

    #[cfg(target_os = "linux")]
    {
        let engine = engine.expect(
            "a settled session must have an nvim child, or the reap below \
             asserts nothing",
        );
        let reaped_by = Instant::now() + view_test_support::host_deadline(LEAVES);
        while still_running(engine) {
            assert!(
                Instant::now() < reaped_by,
                "the session left its engine behind: pid {engine} is still in \
                 the process table"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = engine;
}
