//! A SIGWINCH answered while the late-reply guard still owns the terminal.
//!
//! The guard keeps crossterm off the descriptor, so the resize crossterm
//! would have reported is one the drain has to resolve for itself, from the
//! fd it already holds. What is under test is that the shape it publishes
//! is the terminal's real one: an ioctl on the wrong descriptor, or a
//! `tput` fallback reading `$LINES`/`$COLUMNS`, answers with a shape this
//! pty never had and the first frames after a resize paint to it.
//!
//! A pty this test owns, on descriptor 0 before any reader binds to it, is
//! what makes the shape knowable; descriptor 0 is process state, so this
//! file deliberately holds one test.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use view_core::model::TermCaps;
use view_core::msg::Msg;
use view_tui::input::InputSource;
use view_tui::terminal::TermSizeCell;

#[test]
fn a_resize_under_the_guard_reports_the_shape_the_terminal_actually_has() {
    // this test's failure mode is a block, not a wrong answer
    let _watchdog = view_test_support::watchdog();
    let (_master, slave) = common::stdin_pty();

    // a shape no default matches, so a fallback that guesses one cannot
    // pass by coincidence
    let shape = rustix::termios::Winsize {
        ws_row: 37,
        ws_col: 113,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    rustix::termios::tcsetwinsize(&slave, shape).unwrap();

    // armed: no DA1 fence, so the terminal may still owe a reply and
    // crossterm may not read
    let mut input = InputSource::open_after_probe(TermCaps::default(), false, Vec::new()).unwrap();

    // the signal a resize arrives as. The hook registered above writes the
    // self-pipe from the handler, on this thread, so the byte is queued by
    // the time this returns
    signal_hook::low_level::raise(signal_hook::consts::SIGWINCH).unwrap();

    let size = TermSizeCell::default();
    let mut drained = Vec::new();
    input.drain(&size, |msg| drained.push(msg));
    assert!(
        matches!(
            drained.as_slice(),
            [Msg::Resized {
                width: 113,
                height: 37
            }]
        ),
        "the guarded drain must answer the signal with this pty's own \
         shape: {drained:?}"
    );
    assert_eq!(
        size.take(),
        Some((113, 37)),
        "the shape must be published before the message is handed over, or \
         a frame painted behind it addresses the terminal's old size"
    );
}
