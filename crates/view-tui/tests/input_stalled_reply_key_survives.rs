//! `InputSource::open_listening` against a private-mode answer the terminal
//! stopped halfway through, with the user typing onto the end of it.
//!
//! `ESC [ ? 2026` sitting in the guard's buffer takes the next byte as its
//! final one, and the bytes that end a private-mode CSI include `c`, `u`
//! and `y` -- change, undo and yank. Read as answers they fabricate a DA1
//! fence (which drops the guard, re-opening the very leak it exists to
//! close), a kitty terminal, or nothing at all while eating the key. Only
//! the grammars decide here: the stalled run is the terminal's and goes,
//! the byte that ended it is the user's and stays.
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

#[test]
fn a_key_typed_onto_a_stalled_answer_arrives_and_answers_nothing() {
    use std::os::fd::AsFd;

    // this test's failure mode is a block, not a wrong answer
    let _watchdog = view_test_support::watchdog();
    let (master, slave) = common::stdin_pty();
    let mut input = InputSource::open_listening(TermCaps::default()).unwrap();
    let size = TermSizeCell::default();

    let write = |bytes: &[u8]| {
        rustix::io::write(&master, bytes).unwrap();
        assert!(
            common::wait_readable(slave.as_fd()),
            "the pty never delivered {bytes:?}"
        );
    };
    let drained = |input: &mut InputSource| {
        let mut keys = Vec::new();
        input.drain(&size, |msg| {
            if let Msg::Key(key) = msg {
                keys.push(key.notation);
            }
        });
        keys
    };

    for key in ["c", "u", "y", "h", ":"] {
        write(b"\x1b[?2026");
        assert!(
            drained(&mut input).is_empty(),
            "a half-arrived answer is nobody's keystroke yet"
        );
        write(key.as_bytes());
        assert_eq!(
            drained(&mut input),
            vec![key.to_string()],
            "the key that terminated the stalled answer is the user's"
        );
    }

    // the proof that no `c` was read as the DA1 fence: the guard is still
    // armed, so an answer arriving now is still kept off the decoder
    write(b"\x1b[?2026;1$y");
    assert!(
        drained(&mut input).is_empty(),
        "a fence fabricated from a keypress would have dropped the guard, \
         and this answer would have reached crossterm"
    );

    drop(input);
    // see the sibling tests: a master still open keeps this session's own
    // descriptor 0 alive for whatever the harness does next
    drop(master);
}
