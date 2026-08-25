//! `InputSource::open_after_probe` against a reply split at its introducer,
//! after the user has already typed.
//!
//! `ESC [` is the one thing on this fd that is equally the terminal's and
//! the user's: the opening of a private-mode answer, and the opening of
//! every arrow key. Once a keystroke has come through, those two bytes get
//! one more read to prove themselves an answer. Both outcomes are driven
//! here -- the read that completes them, and the read that puts ordinary
//! typing behind them, which must arrive whole rather than being discarded
//! with the introducer it followed.
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

/// What the drain returned, as plain key notations.
fn drained(input: &mut InputSource, size: &TermSizeCell) -> Vec<String> {
    let mut keys = Vec::new();
    input.drain(size, |msg| {
        if let Msg::Key(key) = msg {
            keys.push(key.notation);
        }
    });
    keys
}

#[test]
fn an_introducer_split_from_its_reply_costs_neither_the_reply_nor_the_typing() {
    use std::os::fd::AsFd;

    // this test's failure mode is a block, not a wrong answer
    let _watchdog = view_test_support::watchdog();
    let (master, slave) = common::stdin_pty();
    let mut input = InputSource::open_after_probe(TermCaps::default(), false, Vec::new()).unwrap();
    let size = TermSizeCell::default();

    let write = |bytes: &[u8]| {
        rustix::io::write(&master, bytes).unwrap();
        assert!(
            common::wait_readable(slave.as_fd()),
            "the pty never delivered {bytes:?}"
        );
    };

    write(b"a");
    assert_eq!(
        drained(&mut input, &size),
        vec!["a"],
        "the keystroke that ends the hold must still reach the engine"
    );

    write(b"\x1b[");
    assert!(
        drained(&mut input, &size).is_empty(),
        "an introducer gets one read to become a reply, so nothing is \
         decoded from it yet"
    );

    write(b"?2026;1$y");
    assert!(
        drained(&mut input, &size).is_empty(),
        "the read that completes the reply must drop the whole of it, not \
         type the nine bytes that finished it"
    );

    write(b"\x1b[");
    assert!(
        drained(&mut input, &size).is_empty(),
        "the same introducer, this time in front of typing"
    );

    write(b"hello");
    assert_eq!(
        drained(&mut input, &size),
        vec!["h", "e", "l", "l", "o"],
        "an introducer the next read did not complete costs those two bytes \
         and nothing else: the keys behind it are the user's"
    );

    drop(input);
    // see the sibling tests: a master still open keeps this session's own
    // descriptor 0 alive for whatever the harness does next
    drop(master);
}
