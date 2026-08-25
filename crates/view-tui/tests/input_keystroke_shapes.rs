//! Every shape a keyboard puts on this fd, driven through the armed
//! late-reply guard.
//!
//! The guard reads before crossterm does, so for as long as it is armed it
//! owns the decode of everything typed -- not just of the terminal's
//! answers. A shape it cannot name is a keystroke lost or, worse, a
//! sequence's tail typed into the buffer as the letters it spells. Each
//! phase below is one keyboard shape, and each must arrive in the drain
//! that follows the read carrying it.
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
fn every_keyboard_shape_survives_the_guard_that_reads_it_first() {
    use std::os::fd::AsFd;

    // this test's failure mode is a block, not a wrong answer
    let _watchdog = view_test_support::watchdog();
    let (master, slave) = common::stdin_pty();
    let mut input = InputSource::open_guarded().unwrap();
    let size = TermSizeCell::default();

    let write = |bytes: &[u8]| {
        rustix::io::write(&master, bytes).unwrap();
        assert!(
            common::wait_readable(slave.as_fd()),
            "the pty never delivered {bytes:?}"
        );
    };
    let drained = |input: &mut InputSource| {
        let mut msgs = Vec::new();
        input.drain(&size, |msg| msgs.push(msg));
        msgs
    };
    let notations = |msgs: &[Msg]| {
        msgs.iter()
            .filter_map(|msg| match msg {
                Msg::Key(key) => Some(key.notation.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };

    for (bytes, expected, shape) in [
        (b"\x1b".as_slice(), vec!["<Esc>"], "a bare Escape"),
        (b"\x1bx".as_slice(), vec!["<Esc>", "x"], "Alt-prefixed"),
        (b"\x1b[A".as_slice(), vec!["<Up>"], "a CSI arrow"),
        (b"\x1bOA".as_slice(), vec!["<Up>"], "an SS3 arrow"),
        (b"\x1b[27u".as_slice(), vec!["<Esc>"], "the kitty form"),
        (
            b"\x1b[27;5;13~".as_slice(),
            vec!["<C-CR>"],
            "the modifyOtherKeys form",
        ),
    ] {
        write(bytes);
        assert_eq!(
            notations(&drained(&mut input)),
            expected,
            "{shape} must arrive in the drain that reads it"
        );
    }

    // split across two reads: the introducer waits for the byte that names
    // it rather than being dropped, which would leave the `A` to arrive
    // alone and append a literal letter in normal mode
    write(b"\x1b[");
    assert!(
        drained(&mut input).is_empty(),
        "half an arrow key is not a keystroke yet"
    );
    write(b"A");
    assert_eq!(
        notations(&drained(&mut input)),
        vec!["<Up>"],
        "the read that completes a split arrow decodes the arrow"
    );
    write(b"\x1bO");
    assert!(
        drained(&mut input).is_empty(),
        "SS3 gets the grace CSI gets: its final byte is one read away"
    );
    write(b"B");
    assert_eq!(
        notations(&drained(&mut input)),
        vec!["<Down>"],
        "a split SS3 arrow decodes the arrow too"
    );

    write(b"\x1b[200~two words\x1b[201~");
    let pasted = drained(&mut input);
    assert!(
        matches!(pasted.as_slice(), [Msg::Paste(text)] if text == "two words"),
        "a bracketed paste is one paste, not its text typed as commands: \
         {pasted:?}"
    );

    // the guard is still armed through all of it: nothing above was read as
    // one of the four answers, so the fence has still not arrived
    write(b"\x1b[?2026;1$y");
    assert!(
        drained(&mut input).is_empty(),
        "a keystroke read as an answer would have disarmed the guard"
    );

    drop(input);
    // see the sibling tests: a master still open keeps this session's own
    // descriptor 0 alive for whatever the harness does next
    drop(master);
}
