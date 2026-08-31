//! A box-glyph answer the probe already has is not asked again by the guard.
//!
//! The CPR is the one probe answer a keyboard can also produce: `\x1b[1;2R`
//! is a cursor at row 1 column 2 and is byte-identical to tmux's `Shift-F3`
//! (`docs/terminal-probe-wire-capture.md`, "A keypress that is a CPR
//! reply"). What separates them is that the probe asked the question once,
//! so the bound has to hold across the handover from the probe to the
//! late-reply guard -- a guard that re-asks on every sweep reads a keypress
//! as an answer for as long as `PROBE_HARD_CAP`, and here that keypress
//! reports the opposite of what the terminal did.
//!
//! Descriptor 0 is process state, so this file holds one test.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;
use view_core::msg::Msg;
use view_tui::input::InputSource;
use view_tui::terminal::TermSizeCell;
use view_tui::tiers::{EnvHints, Probe, ProbeOutcome, PROBE_DEADLINE};

/// Capture E's answer, and no fence behind it: a terminal that is not
/// decoding UTF-8 puts the cursor in column 3, and the missing fence is what
/// arms the guard.
const COLUMN_THREE_NO_FENCE: &[u8] = b"\x1b[1;3R";

/// tmux's `Shift-F3`, arriving while the guard is armed. Column 2 is the
/// answer a UTF-8 terminal gives, which this terminal is not.
const SHIFT_F3: &[u8] = b"\x1b[1;2R";

#[test]
fn a_cpr_shaped_keypress_after_the_probes_own_answer_never_upgrades_the_session() {
    use std::os::fd::AsFd;

    // this test's failure mode is a block, not a wrong answer
    let _watchdog = view_test_support::watchdog();
    let (master, slave) = common::stdin_pty();

    rustix::io::write(&master, COLUMN_THREE_NO_FENCE).unwrap();
    assert!(
        common::wait_readable(slave.as_fd()),
        "the pty never delivered the box-glyph answer"
    );

    let mut queries = Vec::new();
    let probe = Probe::start(
        common::SlaveSource,
        &mut queries,
        PROBE_DEADLINE,
        &EnvHints::default(),
    )
    .unwrap();
    let outcome: ProbeOutcome = probe.finish(Duration::ZERO);
    assert!(
        !outcome.caps.unicode_boxes,
        "column 3 is the answer of a terminal that drew three cells"
    );
    assert!(outcome.cpr_seen, "the probe has its box-glyph answer");
    assert!(
        !outcome.fence_seen,
        "no fence arrived, so the guard has something to wait for"
    );

    let mut input = InputSource::open_after_probe(&outcome).unwrap();
    let size = TermSizeCell::default();
    assert!(input.still_listening(), "the guard must be armed");

    rustix::io::write(&master, SHIFT_F3).unwrap();
    assert!(
        common::wait_readable(slave.as_fd()),
        "the pty never delivered the keypress"
    );
    let mut seen = Vec::new();
    input.drain(&size, |msg| seen.push(msg));

    let upgraded: Vec<&Msg> = seen
        .iter()
        .filter(|msg| matches!(msg, Msg::CapsUpgraded(caps) if caps.unicode_boxes))
        .collect();
    assert!(
        upgraded.is_empty(),
        "the probe asked its box-glyph question once and the terminal \
         answered it; a keypress wearing the same six bytes must not \
         reverse that answer and hand the session a charset it cannot \
         draw: {seen:?}"
    );

    drop(input);
    // a master still open keeps this session's own descriptor 0 alive for
    // whatever the harness does next
    drop(master);
}
