//! `InputSource::open_after_probe` against an `ESC [` that arrives before the
//! user has typed anything at all.
//!
//! The guard's grace is not something a keystroke switches on: an
//! introducer is equally an answer's and an arrow key's from the moment the
//! guard arms, and the burst behind one that turns out to be neither must
//! still arrive. Two bytes are the whole cost.
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
fn an_introducer_ahead_of_the_first_keystroke_costs_only_its_two_bytes() {
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
    let drained = |input: &mut InputSource| {
        let mut keys = Vec::new();
        input.drain(&size, |msg| {
            if let Msg::Key(key) = msg {
                keys.push(key.notation);
            }
        });
        keys
    };

    write(b"\x1b[");
    assert!(
        drained(&mut input).is_empty(),
        "the read after it decides whether these two bytes are an answer"
    );

    write(b"hello");
    assert_eq!(
        drained(&mut input),
        vec!["h", "e", "l", "l", "o"],
        "the burst behind an introducer that became nothing is the user's"
    );

    drop(input);
    // see the sibling tests: a master still open keeps this session's own
    // descriptor 0 alive for whatever the harness does next
    drop(master);
}
