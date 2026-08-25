//! `InputSource::open_guarded` against a terminal whose reply lands whole,
//! in its own read, after the user has already typed.
//!
//! The ordering the two sibling tests do not reach, and the likely one on a
//! slow link: the first sweep finds only the keystroke, because the reply is
//! still in flight, and the whole answer lands in the read after it. A guard
//! that treated the keystroke as proof the terminal was done would be gone
//! by then, and the whole answer would decode into literal keys.
//!
//! Descriptor 0 is process state, so this file holds one test.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use view_core::msg::Msg;
use view_tui::input::InputSource;
use view_tui::terminal::TermSizeCell;

/// A DECRPM answer whole, as one read delivers it.
const WHOLE_LATE_REPLY: &[u8] = b"\x1b[?2026;1$y";

#[test]
fn a_reply_that_lands_whole_after_the_first_keystroke_is_still_dropped() {
    use std::os::fd::AsFd;

    let (master, slave) = common::stdin_pty();
    let mut input = InputSource::open_guarded().unwrap();

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
        after.is_empty(),
        "a keystroke ahead of it does not make the terminal's own answer \
         typeable: {after:?}"
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
