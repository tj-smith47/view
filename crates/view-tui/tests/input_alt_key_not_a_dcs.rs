//! `InputSource::open_guarded` against `ESC P`, which is a keypress.
//!
//! The guard only ever arms on a session whose probe never saw its fence,
//! which is also a session where the kitty keyboard protocol was never
//! pushed -- so Alt-prefixed keys arrive as `ESC <char>` and `Alt+Shift+P`
//! is `ESC P`, byte for byte the opening of the batch's DCS answer. Reading
//! it as an answer would hold it, and everything typed behind it, until the
//! cap, then discard the lot.
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

use view_core::msg::Msg;
use view_tui::input::InputSource;
use view_tui::terminal::TermSizeCell;

#[test]
fn an_alt_prefixed_p_is_typed_through_rather_than_read_as_the_probes_answer() {
    use std::os::fd::AsFd;

    // this test's failure mode is a block, not a wrong answer
    let _watchdog = view_test_support::watchdog();
    let (master, slave) = common::stdin_pty();
    let mut input = InputSource::open_guarded().unwrap();
    let size = TermSizeCell::default();

    let drained = |input: &mut InputSource| {
        let mut keys = Vec::new();
        input.drain(&size, |msg| {
            if let Msg::Key(key) = msg {
                keys.push(key.notation);
            }
        });
        keys
    };
    let write = |bytes: &[u8]| {
        rustix::io::write(&master, bytes).unwrap();
        assert!(
            common::wait_readable(slave.as_fd()),
            "the pty never delivered {bytes:?}"
        );
    };

    write(b"a");
    assert_eq!(drained(&mut input), vec!["a"]);

    write(b"\x1bP");
    assert_eq!(
        drained(&mut input),
        vec!["<Esc>", "P"],
        "two bytes are not the batch's DCS answer, which opens `ESC P 0/1 $ \
         r`; what was typed is an Escape and a `P`"
    );

    write(b"hello");
    assert_eq!(
        drained(&mut input),
        vec!["h", "e", "l", "l", "o"],
        "nothing may be held behind a keypress mistaken for an answer"
    );

    drop(input);
    // see the sibling tests: a master still open keeps this session's own
    // descriptor 0 alive for whatever the harness does next
    drop(master);
}
