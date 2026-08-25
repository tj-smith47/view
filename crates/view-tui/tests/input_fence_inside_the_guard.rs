//! The fence arriving *inside* the guard, with a keypress still arriving
//! behind it.
//!
//! The sibling of `input_fence_seen_guard_releases`, one read later: there
//! the probe already had the fence and the guard was armed on the tail
//! alone, here the fence lands while the guard owns the terminal. Both end
//! the same way -- the fence is the probe's last question, so nothing it
//! asked for can still be in flight -- but a fence is not a reason to hand
//! back a half-arrived keypress. Decoded at the fence, an `ESC [` is
//! dropped to nothing and its own final byte reaches the next read alone,
//! typing an arrow's `A` into the buffer as a literal key.
//!
//! Descriptor 0 is process state, so this file holds one test.
//!
//! One wall clock this cannot scale: both phases have to land inside
//! `PROBE_HARD_CAP` of arming the guard, or it expires mid-test. They are
//! writes and reads rather than sleeps, so the margin is three orders of
//! magnitude -- but a host that stalls this process for 400ms between them
//! fails the test without the code being wrong.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use view_core::model::TermCaps;
use view_core::msg::Msg;
use view_tui::input::InputSource;
use view_tui::terminal::TermSizeCell;

/// The DA1 fence and, behind it, an introducer with no final byte yet: a
/// live prefix of a cursor-key sequence and of several answer grammars.
const FENCE_THEN_INTRODUCER: &[u8] = b"\x1b[?1;2c\x1b[";

/// The byte that resolves it, making the run an arrow rather than an answer.
const ARROW_FINAL: &[u8] = b"A";

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
fn a_fence_reaching_the_guard_does_not_hand_back_the_keypress_behind_it() {
    use std::os::fd::AsFd;

    // this test's failure mode is a block, not a wrong answer
    let _watchdog = view_test_support::watchdog();
    let (master, slave) = common::stdin_pty();
    // no fence at the probe, so the guard is armed on the fence itself
    let mut input = InputSource::open_after_probe(TermCaps::default(), false, Vec::new()).unwrap();
    let size = TermSizeCell::default();

    let write = |bytes: &[u8]| {
        rustix::io::write(&master, bytes).unwrap();
        assert!(
            common::wait_readable(slave.as_fd()),
            "the pty never delivered {bytes:?}"
        );
    };

    write(FENCE_THEN_INTRODUCER);
    assert!(
        drained(&mut input, &size).is_empty(),
        "an introducer gets the read that finishes it, whatever arrived in \
         front of it"
    );
    assert!(
        input.still_listening(),
        "the fence is answered and the guard is owed nothing more, but the \
         run behind it is half a keypress: handing the terminal back here \
         costs its introducer"
    );

    write(ARROW_FINAL);
    assert_eq!(
        drained(&mut input, &size),
        vec!["<Up>"],
        "the run was the user's arrow, and the read that finishes it must \
         deliver the arrow rather than an `A`"
    );
    assert!(
        !input.still_listening(),
        "with the fence long answered and the tail resolved, the guard is \
         owed nothing and must have handed the terminal back rather than \
         holding the key path for the rest of the cap"
    );

    drop(input);
    // a master still open keeps this session's own descriptor 0 alive for
    // whatever the harness does next
    drop(master);
}
