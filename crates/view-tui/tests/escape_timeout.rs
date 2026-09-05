//! What bounds a key code the terminal has only half delivered: nvim's own
//! `ttimeoutlen`, relayed from the engine, and the reading the reader takes
//! of the bytes it is holding once that wait is out.
//!
//! The wait is the whole reason a bare `ESC` is not a keystroke the moment
//! it lands -- the byte that would make it an Alt chord may be in the read
//! that has not happened yet -- and the forced reading is the reason it
//! ever becomes one at all.
//!
//! Descriptor 0 is process state, so this file holds one test.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use view_core::msg::Msg;
use view_tui::input::InputSource;
use view_tui::terminal::TermSizeCell;

/// The wait armed for the phases that need a non-zero one. Short, because
/// the only sleep below overshoots it on purpose and a host that stalls can
/// only overshoot it further.
const WAIT: Duration = Duration::from_millis(100);

#[test]
fn a_half_arrived_key_code_waits_the_engines_own_timing_and_then_is_read() {
    use std::os::fd::AsFd;

    // this test's failure mode is a block, not a wrong answer
    let _watchdog = view_test_support::watchdog();
    let (master, slave) = common::stdin_pty();
    let mut input = InputSource::open().unwrap();
    let size = TermSizeCell::default();
    input.set_escape_timeout(WAIT);

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
        msgs.iter()
            .filter_map(|msg| match msg {
                Msg::Key(key) => Some(key.notation.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };

    // the wait is re-armed by every read that leaves the run unfinished, as
    // the engine re-arms its own timer (`tui/input.c`): a sequence trickling
    // in a byte at a time gets the whole wait for each of them, rather than
    // being read as literal bytes because its first byte is old. Asserted on
    // the deadline rather than by sleeping past one, so a stalled host
    // cannot make the two waits look like one
    write(b"\x1b");
    assert!(
        drained(&mut input).is_empty(),
        "an Escape alone may still become an Alt chord"
    );
    let opened = input
        .next_deadline()
        .expect("a run waiting for its tail is what the deadline is for");
    write(b"[");
    assert!(
        drained(&mut input).is_empty(),
        "an introducer alone may still become an arrow"
    );
    let restarted = input
        .next_deadline()
        .expect("the run is still unfinished, so it still has a deadline");
    assert!(
        restarted > opened,
        "the read that carried a byte must restart the wait"
    );

    // past the wait the held run is read as what its bytes spell, which for
    // `ESC [` is termkey's own answer -- the reading under which those bytes
    // reach the buffer at all
    std::thread::sleep(WAIT * 3);
    assert_eq!(
        drained(&mut input),
        vec!["<M-[>"],
        "a run out of time is read rather than held for a byte that is not coming"
    );

    // `ttimeout` off, and a negative `ttimeoutlen`, both reach here as a
    // zero-length wait: the engine forces on the same pass that read the
    // bytes, so nothing may be left pending for a later drain
    input.set_escape_timeout(Duration::ZERO);
    write(b"\x1bO");
    assert_eq!(
        drained(&mut input),
        vec!["<M-O>"],
        "a zero wait is forced in the drain that read it, not in the next one"
    );
    write(b"\x1b");
    assert_eq!(
        drained(&mut input),
        vec!["<Esc>"],
        "a bare Escape with nothing behind it is the Escape key"
    );
    assert!(
        input.next_deadline().is_none(),
        "nothing may be left waiting once the wait is zero"
    );

    // an `ESC` sharing a read with a paste opener is a keystroke of its
    // own. Folded into the opener it would leave a run this wait flushes --
    // at zero, in this very drain -- and the pasted body would reach the
    // buffer as normal-mode commands
    write(b"\x1b\x1b[200~body");
    assert_eq!(
        drained(&mut input),
        vec!["<Esc>"],
        "a paste behind a stray Escape must not be flushed as keystrokes"
    );
    assert!(
        input.next_deadline().is_none(),
        "a paste is bounded by its closer, not by the escape timeout"
    );
}
