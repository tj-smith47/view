//! `InputSource::open_after_probe` against a terminal whose reply lands whole,
//! in its own read, after the user has already typed.
//!
//! The ordering the two sibling tests do not reach, and the likely one on a
//! slow link: the first sweep finds only the keystroke, because the reply is
//! still in flight, and the whole answer lands in the read after it. A guard
//! that treated the keystroke as proof the terminal was done would be gone
//! by then, and the whole answer would decode into literal keys instead of
//! upgrading the session that is already running at the tier it settled for.
//!
//! Descriptor 0 is process state, so this file holds one test.

//!
//! One wall clock this cannot scale: every phase has to land inside
//! `PROBE_HARD_CAP` of arming the guard, or the guard expires mid-test.
//! The phases are writes and reads rather than sleeps, so the margin is
//! three orders of magnitude -- but a host that stalls this process for
//! 400ms between two of them fails the test without the code being wrong.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use view_core::model::TermCaps;
use view_core::msg::Msg;
use view_tui::input::InputSource;
use view_tui::terminal::TermSizeCell;

/// A DECRPM answer whole, as one read delivers it.
const WHOLE_LATE_REPLY: &[u8] = b"\x1b[?2026;1$y";

#[test]
fn a_reply_that_lands_whole_after_the_first_keystroke_still_answers_the_probe() {
    use std::os::fd::AsFd;

    // this test's failure mode is a block, not a wrong answer
    let _watchdog = view_test_support::watchdog();
    let (master, slave) = common::stdin_pty();
    let mut input = InputSource::open_after_probe(TermCaps::default(), false, Vec::new()).unwrap();

    rustix::io::write(&master, b"a").unwrap();
    assert!(
        common::wait_readable(slave.as_fd()),
        "the pty never delivered the keystroke"
    );

    let size = TermSizeCell::default();
    let mut typed = Vec::new();
    input.drain(&size, |msg| typed.push(msg));
    assert!(
        matches!(typed.as_slice(), [Msg::Key(key)] if key.notation == "a"),
        "the keystroke must reach the engine on its own: {typed:?}"
    );

    rustix::io::write(&master, WHOLE_LATE_REPLY).unwrap();
    assert!(
        common::wait_readable(slave.as_fd()),
        "the pty never delivered the reply"
    );
    let mut after = Vec::new();
    input.drain(&size, |msg| after.push(msg));
    assert!(
        matches!(after.as_slice(), [Msg::CapsUpgraded(caps)] if caps.sync),
        "a keystroke ahead of it makes the terminal's own answer neither \
         typeable nor lost: the reply leaves the key path carrying nothing, \
         and the capability it reports reaches the session: {after:?}"
    );

    rustix::io::write(&master, b"b").unwrap();
    assert!(
        common::wait_readable(slave.as_fd()),
        "the pty never delivered the write after the reply"
    );
    let mut resumed = Vec::new();
    input.drain(&size, |msg| resumed.push(msg));
    assert!(
        matches!(resumed.as_slice(), [Msg::Key(key)] if key.notation == "b"),
        "input after the swept reply must still arrive: {resumed:?}"
    );

    drop(input);
    // see the sibling tests: a master still open keeps this session's own
    // descriptor 0 alive for whatever the harness does next
    drop(master);
}
