//! `InputSource::open_listening` against a real terminal that answers
//! the startup capability probe after the probe has stopped listening.
//!
//! The shape under test is what an ssh hop makes ordinary: the probe writes
//! its batch, the replies are a network round trip behind, and the terminal
//! is handed to crossterm before the DA1 fence ever lands. A DECRPM answer
//! (`ESC [ ? 2026 ; 1 $ y`) reaching crossterm's parser resolves neither to
//! an event nor to an error, so it stays in that parser's buffer and every
//! later byte is appended to it -- the reply does not just arrive as
//! garbage, it swallows the keystrokes behind it. The guard reads those
//! bytes first, keeps them off the key path, and reports what they
//! answered: this burst is a full-tier terminal, and the session that
//! painted its first frames at `Basic` has to hear so.
//!
//! A pty this test owns, put on descriptor 0 before crossterm's
//! process-wide reader binds to anything, is what makes that reachable.
//! Descriptor 0 is process state, so this file deliberately holds one test.

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

/// The whole batch a modern terminal owes, arriving in one burst long after
/// the probe gave up, with a keystroke typed right behind it: the DECRPM
/// answer, the kitty flags, the DECRQSS truecolor readback (a DCS, which
/// crossterm decodes into a run of literal keys), the DA1 fence, then `a`.
const LATE_BATCH_THEN_KEY: &[u8] =
    b"\x1b[?2026;1$y\x1b[?1u\x1bP1$r0;48;2;1;2;3m\x1b\\\x1b[?62;1;6ca";

#[test]
fn a_reply_arriving_after_the_probe_gave_up_never_reaches_the_engine() {
    use std::os::fd::AsFd;

    // this test's failure mode is a block, not a wrong answer
    let _watchdog = view_test_support::watchdog();
    let (master, slave) = common::stdin_pty();

    // opened while the queue is empty, so crossterm's reader binds to this
    // terminal without consuming any of the burst written below
    // exactly what `main` opens for a probe whose fence never came
    let mut input = InputSource::open_listening(TermCaps::default(), Vec::new()).unwrap();

    rustix::io::write(&master, LATE_BATCH_THEN_KEY).unwrap();
    assert!(
        common::wait_readable(slave.as_fd()),
        "the pty never delivered the bytes written into its master"
    );

    let size = TermSizeCell::default();
    let mut drained = Vec::new();
    input.drain(&size, |msg| drained.push(msg));
    assert!(
        matches!(
            drained.as_slice(),
            [Msg::CapsUpgraded(caps), Msg::Key(key)]
                if *caps == TermCaps::from_probe(true, true, true) && key.notation == "a"
        ),
        "the burst must leave the key path carrying one keystroke and no \
         literal `P`, `$`, `r` or digit from the DCS answer, and must reach \
         the loop as the full tier every one of its answers proves -- the \
         upgrade the probe stopped waiting for: {drained:?}"
    );

    // the keystroke after the guard has handed the terminal back: it must
    // arrive through crossterm normally, which it cannot do if the late
    // DECRPM answer is still sitting in that parser's buffer collecting
    // every byte behind it
    rustix::io::write(&master, b"b").unwrap();
    assert!(
        common::wait_readable(slave.as_fd()),
        "the pty never delivered the second write"
    );
    let mut after = Vec::new();
    input.drain(&size, |msg| after.push(msg));
    assert!(
        matches!(after.as_slice(), [Msg::Key(key)] if key.notation == "b"),
        "input after the swept burst must decode normally: {after:?}"
    );

    drop(input);
    // the poll deadline is unbounded when nothing is ready, and a master
    // still open leaves this session's own descriptor 0 alive for whatever
    // the harness does next
    drop(master);
}
