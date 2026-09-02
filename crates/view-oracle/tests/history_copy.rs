//! Black-box proof of the message history's copy key inside a real `view`
//! process on a pty: the line a user scrolled to leaves for the terminal as
//! an OSC 52 escape *and* lands in view's own `"+` register, and a host with
//! no reachable system clipboard is told so once rather than once per copy.
//!
//! Neither claim is reachable from an `Effect`-layer test. The first is
//! about bytes on a terminal and about what a second keystroke (`"+p`) reads
//! back out of the worker's own state; the second is about a notice raised
//! by a worker thread whose reachability depends on the process's
//! environment. `PtySession::spawn_configured` strips `DISPLAY` and
//! `WAYLAND_DISPLAY` from every spawn in this tree (see
//! `clipboard_roundtrip.rs`'s module doc for the sweep and the one variable
//! that opts out of it), so a session spawned here is headless by
//! construction and the second claim is deterministic rather than a
//! property of whoever's desktop the suite ran on -- on Linux, where a
//! display variable is what reachability hangs on; the headless row states
//! its own narrower gate.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::{Duration, Instant};

/// The line the copy is about: a path with spaces in it, which is the shape
/// every "helpful" trim, quote or "copied 1 line" rewrite breaks. Distinct
/// from every other fixture marker in this suite so a raw-output search
/// cannot cross-match another session.
const TARGET: &str = "view-history-copy /home/tj/my notes/plan v2.md is gone";

/// How many messages are echoed after `TARGET`, so that reaching it in the
/// newest-first history means scrolling well past the rows the overlay's
/// first frame drew (~9 on an 80x24 terminal).
const FILLER: usize = 30;

/// The part of `TARGET` that survives the overlay's column truncation, so
/// the same needle works on a history row and in a pasted buffer line.
const TARGET_NEEDLE: &str = "my notes/plan";

/// nvim's default leader, which the isolated home never overrides.
const LEADER: &str = "\\";

/// The window a key's repaint is waited out in: long enough for a frame on
/// a loaded host, short enough that a key that changed nothing fails here
/// rather than stalling the suite.
const SETTLE: Duration = Duration::from_secs(5);

/// As much of the unreachable-clipboard notice as fits on one 80-column
/// toast row without risking a wrap mid-phrase. The full wording is pinned
/// in `view-core`'s own `an_unreachable_system_clipboard_notices_once`;
/// what this file is about is how many times it appears.
///
/// Gated with its only consumer, the linux-only headless row: elsewhere it
/// is dead code, and `-D warnings` makes that a build break.
#[cfg(target_os = "linux")]
const NOTICE_OPENING: &str = "view: no system";

fn wait_for_bytes(session: &mut view_oracle::PtySession, needle: &[u8], timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if session
            .raw_output()
            .windows(needle.len())
            .any(|w| w == needle)
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A headless `view` on a pty with the full native experience on, holding
/// `TARGET` and `FILLER` later messages in its history.
fn session_with_history(label: &str) -> (view_oracle::PtySession, common::ScratchPaths) {
    let bin = common::view_bin_path();
    let paths = common::ScratchPaths::new(label);
    let mut cmd = portable_pty::CommandBuilder::new(&bin);
    common::isolate_xdg_first_launch(&mut cmd, &paths.isolated_home);
    let mut session = view_oracle::PtySession::spawn_configured(cmd, 80, 24)
        .unwrap_or_else(|err| panic!("PtySession::spawn_configured against {bin:?}: {err}"));
    session.record_raw_output();
    assert!(
        session.wait_for("~", Duration::from_secs(10)),
        "view never painted its startup shell; screen:\n{}",
        session.screen()
    );

    session
        .send(format!(":echomsg \"{TARGET}\"\r").as_bytes())
        .expect("the echo command must reach the session");
    assert!(
        session.wait_for(TARGET_NEEDLE, Duration::from_secs(10)),
        "the target message never reached the toast stack; screen:\n{}",
        session.screen()
    );
    session
        .send(format!(":lua for i = 1, {FILLER} do print('history filler ' .. i) end\r").as_bytes())
        .expect("the filler command must reach the session");
    assert!(
        session.wait_for(&format!("history filler {FILLER}"), Duration::from_secs(10)),
        "the filler messages never arrived; screen:\n{}",
        session.screen()
    );
    (session, paths)
}

/// Opens the history and moves the selection down onto `TARGET`, which sits
/// `FILLER` rows below the top of a newest-first list.
///
/// Every key here is driven as the bytes a terminal actually sends, which
/// is the point of doing it on a pty: `gg` is two `g` events and reaches
/// the overlay only if the prefix is held between them. A test that injects
/// a single `"gg"` notation proves nothing about the key a user presses.
fn scroll_to_target(session: &mut view_oracle::PtySession) {
    session
        .send(format!("{LEADER}fm").as_bytes())
        .expect("the history mapping must reach the session");
    assert!(
        session.wait_for("Messages", Duration::from_secs(10)),
        "<leader>fm never opened the message history; screen:\n{}",
        session.screen()
    );

    // the newest entry, which is where the overlay opens and the one row
    // this session can name without knowing which notices its own startup
    // raised behind the fixture
    let newest = format!("> history filler {FILLER}");
    assert!(
        session.wait_for(&newest, SETTLE),
        "the history did not open on the newest entry; screen:\n{}",
        session.screen()
    );

    session
        .send(b"G")
        .expect("the scroll keys must reach the session");
    assert!(
        session.wait_for_screen(SETTLE, |screen| !screen.contents().contains(&newest)),
        "`G` left the selection on the newest entry; screen:\n{}",
        session.screen()
    );
    session
        .send(b"gg")
        .expect("the scroll keys must reach the session");
    assert!(
        session.wait_for(&newest, SETTLE),
        "two `g` presses did not bring the selection back to the top; screen:\n{}",
        session.screen()
    );

    session
        .send("j".repeat(FILLER).as_bytes())
        .expect("the scroll keys must reach the session");
    assert!(
        session.wait_for("> view-history-copy", Duration::from_secs(10)),
        "the target row never scrolled into the overlay; screen:\n{}",
        session.screen()
    );
}

/// Both halves of one copy, from a row the first frame never showed: the
/// escape the terminal a remote session is read on receives, and the local
/// register the same session pastes from. Dropping either effect from
/// `update::surfaces`'s copy arm leaves one of these two assertions with
/// nothing to find.
#[test]
fn a_copied_history_line_reaches_both_the_terminal_and_the_local_register() {
    let (mut session, _paths) = session_with_history("history-copy");
    scroll_to_target(&mut session);
    session
        .send(b"y")
        .expect("the copy key must reach the session");

    let expected = view_core::osc52::clipboard_escape('+', TARGET);
    assert!(
        wait_for_bytes(&mut session, expected.as_bytes(), Duration::from_secs(10)),
        "no OSC 52 escape carrying the selected line reached the terminal. Raw \
         output:\n{}",
        String::from_utf8_lossy(session.raw_output())
    );

    // and the local half, read back through view's own clipboard provider:
    // a `"+p` answers from the system clipboard when there is one and from
    // the worker's shadow register when there is not, and only the write
    // this same keypress performed can put the line in either
    session
        .send(b"\x1bo\x1b\"+p")
        .expect("the paste keys must reach the session");
    assert!(
        session.wait_for(TARGET_NEEDLE, Duration::from_secs(10)),
        "`\"+p` did not paste the copied line back, so the local write never \
         happened; screen:\n{}",
        session.screen()
    );
}

/// A headless host is told once, not once per copy: the worker reports every
/// unreachable write and the model's family dedupe is what turns a session's
/// worth of them into one line.
///
/// `target_os = "linux"`, not `unix`: headless-by-construction rests on
/// stripping `DISPLAY`/`WAYLAND_DISPLAY`, an X11/Wayland fact. macOS has no
/// display variable to strip -- `NSPasteboard` answers regardless, and on a
/// session without a full window server it sometimes accepts the connection
/// and refuses the write, a fault `write_system` deliberately keeps to a
/// `VIEW_LOG` line rather than this notice (crates/view/src/clipboard.rs).
/// The once-per-session dedupe itself stays pinned everywhere by
/// `view-core`'s `an_unreachable_system_clipboard_notices_once`.
#[test]
#[cfg(target_os = "linux")]
fn a_headless_session_notices_an_unreachable_clipboard_once() {
    let (mut session, _paths) = session_with_history("history-copy-headless");
    scroll_to_target(&mut session);
    session
        .send(b"yy")
        .expect("both copy keys must reach the session");
    assert!(
        session.wait_for(NOTICE_OPENING, Duration::from_secs(10)),
        "a copy with no reachable clipboard must say so; screen:\n{}",
        session.screen()
    );
    // an absence, so a bounded window rather than a single look: a second
    // notice would arrive as its own toast row a repaint after the first
    let doubled = session.wait_for_screen(
        view_test_support::host_deadline(Duration::from_millis(750)),
        |screen| screen.contents().matches(NOTICE_OPENING).count() >= 2,
    );
    assert!(
        !doubled,
        "two copies must leave one standing notice, not two; screen:\n{}",
        session.screen()
    );
}
