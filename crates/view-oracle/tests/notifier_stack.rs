//! A notifier's complaint about view holding `vim.notify` never reaches the
//! terminal, wherever the notifier stacked it: at launch, after a
//! supervised restart and after a branch, each of which starts an engine
//! that sources the config again.
//!
//! The planted config stacks three boxes down the top-right corner and a
//! taller complaint below them, outside the message area. Once that
//! complaint is gone it opens a second complaint at full width in one
//! step, wider than half the screen, beside a progress box at the bottom
//! right that updates and has to stay on screen.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use view_oracle::PtySession;

const COLS: u16 = 100;
const ROWS: u16 = 30;

/// What one wait may take on an idle host, before the load this run started
/// under widens it.
const BUDGET: Duration = Duration::from_secs(20);

/// The first lines of the two complaint boxes. On the terminal either is a
/// frame carrying a complaint view takes down.
const COMPLAINTS: [&str; 2] = ["STACKCOMPLAINT", "STACKWIDE"];

/// The fixture's line once view closed the full-width complaint, which the
/// fixture opens only after view closed the stacked one, followed by the
/// engine's pid.
const TAKEN: &str = "WIDETAKEN";

/// The fixture's lines once its own timeout closed a complaint box view
/// left standing, each followed by the engine's pid.
const EXPIRED: [&str; 2] = ["STACKEXPIRED", "WIDEEXPIRED"];

/// The progress box's last update, followed by the engine's pid.
const PROGRESS: &str = "STACKPROGRESS 5/5";

fn wrote(stream: &[u8], needle: &str) -> bool {
    stream
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

/// The `view` under test on its `[native]` defaults, which draw the
/// messages, with the recorder a branch needs, recording every byte from
/// its spawn.
fn view_session(home: &std::path::Path) -> PtySession {
    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    common::isolate_xdg_first_launch(&mut cmd, home);
    cmd.env("VIEW_DVR_ENABLED", "true");
    let mut session = PtySession::spawn_configured(cmd, COLS, ROWS)
        .expect("PtySession::spawn_configured against the editor under test");
    session.record_raw_output();
    session
}

/// The pid of an `nvim` the session under test spawned other than `old`,
/// which a killed engine stays listed as until it is reaped.
fn engine_child_of(pid: u32, old: Option<u32>) -> Option<u32> {
    view_test_support::child_pids(pid)
        .into_iter()
        .find(|child| {
            Some(*child) != old
                && std::fs::read_to_string(format!("/proc/{child}/comm"))
                    .is_ok_and(|comm| comm.trim() == "nvim")
        })
}

/// Waits for an engine other than `old` under the session, and answers its
/// pid.
fn next_engine(session: &mut PtySession, old: Option<u32>) -> u32 {
    let pid = session.pid().expect("the session under test has a pid");
    let deadline = std::time::Instant::now() + view_test_support::host_deadline(BUDGET);
    loop {
        if let Some(engine) = engine_child_of(pid, old) {
            return engine;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no engine replaced {old:?}; screen:\n{}",
            session.screen()
        );
        // drains for the interval; the predicate is never the answer here
        let _ = session.wait_for_screen(Duration::from_millis(50), |_| false);
    }
}

/// Waits until the screen carries one of `lines`, each followed by
/// engine `engine`'s pid.
///
/// Read off the screen, since a frame writes only the cells that changed
/// and a marker shares its opening with the line it replaces.
fn on_screen(session: &mut PtySession, engine: u32, lines: &[&str], at: &str) {
    let wanted: Vec<String> = lines
        .iter()
        .map(|line| format!("{line} {engine}"))
        .collect();
    let deadline = std::time::Instant::now() + view_test_support::host_deadline(BUDGET);
    while !session.wait_for_screen(Duration::from_millis(50), |screen| {
        let contents = screen.contents();
        wanted.iter().any(|line| contents.contains(line))
    }) {
        assert!(
            std::time::Instant::now() < deadline,
            "{at}: never on screen: {wanted:?}; screen:\n{}",
            session.screen()
        );
    }
}

/// Waits until engine `engine`'s fixture says how its last complaint
/// ended.
fn reported(session: &mut PtySession, engine: u32, at: &str) {
    on_screen(session, engine, &[TAKEN, EXPIRED[0], EXPIRED[1]], at);
}

/// Waits until engine `engine`'s fixture says how its complaints ended,
/// and asserts view took both without writing a frame that carried
/// either, and left the progress box on screen.
fn complaint_taken(session: &mut PtySession, from: usize, engine: u32, at: &str) {
    reported(session, engine, at);
    let taken = format!("{TAKEN} {engine}");
    let written = session.raw_output()[from..].to_vec();
    for complaint in COMPLAINTS {
        assert!(
            !wrote(&written, complaint),
            "{at}: view painted {complaint}; screen:\n{}",
            session.screen()
        );
    }
    assert!(
        session.screen().contains(&taken),
        "{at}: view left a complaint standing; screen:\n{}",
        session.screen()
    );
    on_screen(session, engine, &[PROGRESS], at);
}

/// Runs one session to the fixture's report and out again, so the theme
/// cache exists and the measured session paints no first-launch notice over
/// the corner the stack fills.
fn warm_the_home(home: &std::path::Path) {
    let mut session = view_session(home);
    let engine = next_engine(&mut session, None);
    reported(&mut session, engine, "warming");
    session
        .send(b"\x1b:qa!\r")
        .expect("the warming pty accepts the quit");
    let _ = session.wait_for_exit(view_test_support::host_deadline(BUDGET));
}

/// Ends `pid` the way a crashed engine ends: only a pid this test read off
/// the session it spawned.
fn kill(pid: u32) {
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(pid).expect("a pid read from /proc fits an i32")),
        nix::sys::signal::Signal::SIGKILL,
    );
}

/// Disconfirm: recognising the complaint by the message area's rect alone
/// paints the fourth box at launch and after each replacement, and a
/// corner held only while it sits inside the corner column paints the
/// full-width one.
#[test]
fn a_stacked_complaint_is_taken_at_launch_after_a_restart_and_after_a_branch() {
    let paths = common::ScratchPaths::new("notifier-stack");
    common::plant_nvim_config(&paths.isolated_home, "notifier-stack");
    warm_the_home(&paths.isolated_home);

    let mut session = view_session(&paths.isolated_home);
    let launched = next_engine(&mut session, None);
    complaint_taken(&mut session, 0, launched, "launch");

    let from = session.raw_output().len();
    kill(launched);
    let restarted = next_engine(&mut session, Some(launched));
    complaint_taken(&mut session, from, restarted, "restart");

    session
        .send(b"\x1b:View dvr scrub\r")
        .expect("the pty accepts the scrub");
    assert!(
        session.wait_for("b branch", view_test_support::host_deadline(BUDGET)),
        "the scrub never opened; screen:\n{}",
        session.screen()
    );
    session.send(b"b").expect("the pty accepts the branch");
    assert!(
        session.wait_for(
            "Branch from frame",
            view_test_support::host_deadline(BUDGET)
        ),
        "the branch confirm never opened; screen:\n{}",
        session.screen()
    );
    let from = session.raw_output().len();
    session.send(b"y").expect("the pty accepts the answer");
    let branched = next_engine(&mut session, Some(restarted));
    complaint_taken(&mut session, from, branched, "branch");

    session
        .send(b"\x1b:qa!\r")
        .expect("the pty accepts the quit");
    let _ = session.wait_for_exit(view_test_support::host_deadline(BUDGET));
}
