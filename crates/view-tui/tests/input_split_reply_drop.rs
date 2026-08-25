//! `InputSource::open_guarded` against a terminal whose late reply arrives
//! in pieces, with a keystroke ahead of it.
//!
//! Segmentation is what an ssh hop adds to the shape the sibling
//! `input_late_reply_drop` test covers: the reply is not one burst but
//! whatever fits the segment that carried it, and a user typing while the
//! probe's second window is still open puts a real keystroke in front of a
//! half-delivered answer. Forwarding that keystroke must not end the guard,
//! because the rest of the answer is still owed and crossterm's parser
//! turns it into literal keys.
//!
//! Descriptor 0 is process state, so this file holds one test, and the
//! shape it drives cannot share a process with the sibling's -- that one
//! ends with the guard disarmed by the fence.

//!
//! One wall clock this cannot scale: every phase has to land inside
//! `PROBE_HARD_CAP` of arming the guard, or the guard expires mid-test.
//! The phases are writes and reads rather than sleeps, so the margin is
//! three orders of magnitude -- but a host that stalls this process for
//! 400ms between two of them fails the test without the code being wrong.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use view_core::msg::Msg;
use view_tui::input::InputSource;
use view_tui::terminal::TermSizeCell;

/// A typed `a` and as much of a DECRPM answer as the first segment carried.
const KEY_THEN_PARTIAL_REPLY: &[u8] = b"a\x1b[?2026";

/// The rest of that answer, arriving after the keystroke has been forwarded.
const REPLY_TAIL: &[u8] = b";1$y";

#[test]
fn a_keystroke_ahead_of_a_split_reply_does_not_hand_the_rest_of_it_to_crossterm() {
    use std::os::fd::AsFd;

    // this test's failure mode is a block, not a wrong answer
    let _watchdog = view_test_support::watchdog();
    let (master, slave) = common::stdin_pty();
    let mut input = InputSource::open_guarded().unwrap();

    rustix::io::write(&master, KEY_THEN_PARTIAL_REPLY).unwrap();
    assert!(
        common::wait_readable(slave.as_fd()),
        "the pty never delivered the first segment"
    );

    let size = TermSizeCell::default();
    let mut typed = Vec::new();
    input.drain(&size, |msg| typed.push(msg));
    assert!(
        matches!(typed.as_slice(), [Msg::Key(key)] if key.notation == "a"),
        "the keystroke ahead of the reply must reach the engine on its own: \
         {typed:?}"
    );

    rustix::io::write(&master, REPLY_TAIL).unwrap();
    assert!(
        common::wait_readable(slave.as_fd()),
        "the pty never delivered the reply's tail"
    );
    let mut after = Vec::new();
    input.drain(&size, |msg| after.push(msg));
    assert!(
        after.is_empty(),
        "the tail of the split reply must be swept, not decoded into \
         `;`, `1`, `$` and `y` keystrokes: {after:?}"
    );

    // the guard must have handed the terminal back working, not wedged
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
    // see the sibling test: a master still open keeps this session's own
    // descriptor 0 alive for whatever the harness does next
    drop(master);
}
