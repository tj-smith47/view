//! A capability answer cut in half by the probe's settle, not by a read.
//!
//! The settle takes no wait, so the seam between the probe and the guard
//! sits at the first probe window rather than at the hard cap -- which on
//! an ssh hop is exactly where a reply is mid-flight. The head of that
//! reply is in the probe's buffer and the tail arrives on the input path,
//! so unless the head crosses the handover the guard scans a tail with no
//! head: `1$y` is no answer's grammar, and the session both loses the
//! capability and types three keys into the buffer.
//!
//! Descriptor 0 is process state, so this file holds one test.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;
use view_core::msg::Msg;
use view_tui::input::InputSource;
use view_tui::terminal::TermSizeCell;
use view_tui::tiers::{Probe, PROBE_DEADLINE};

/// As much of a DECRPM answer as the segment before the settle carried.
const REPLY_HEAD: &[u8] = b"\x1b[?2026;";

/// The rest of it, arriving once the guard owns the terminal.
const REPLY_TAIL: &[u8] = b"1$y";

#[test]
fn the_head_of_an_answer_still_arriving_at_the_settle_crosses_the_handover() {
    use std::os::fd::AsFd;

    // this test's failure mode is a block, not a wrong answer
    let _watchdog = view_test_support::watchdog();
    let (master, slave) = common::stdin_pty();

    rustix::io::write(&master, REPLY_HEAD).unwrap();
    assert!(
        common::wait_readable(slave.as_fd()),
        "the pty never delivered the head of the reply"
    );

    let mut queries = Vec::new();
    let probe = Probe::start(common::SlaveSource, &mut queries, PROBE_DEADLINE, None).unwrap();
    let outcome = probe.finish(Duration::ZERO);
    assert!(
        !outcome.fence_seen,
        "this file's subject is the fence that never came; a probe reporting \
         one has changed what is under test"
    );
    assert!(
        !outcome.caps.sync,
        "half an answer resolves nothing: {:?}",
        outcome.caps
    );
    assert!(
        outcome.residue.is_empty(),
        "the terminal's own bytes are not the user's: {:?}",
        outcome.residue
    );
    assert_eq!(
        outcome.partial_reply, REPLY_HEAD,
        "the live prefix of the answer must leave the probe with the outcome"
    );

    let mut input =
        InputSource::open_after_probe(outcome.caps, outcome.fence_seen, outcome.partial_reply)
            .unwrap();
    let size = TermSizeCell::default();
    let mut settled = Vec::new();
    input.drain(&size, |msg| settled.push(msg));
    assert!(
        settled.is_empty(),
        "a head with no tail yet resolves nothing and types nothing: {settled:?}"
    );

    rustix::io::write(&master, REPLY_TAIL).unwrap();
    assert!(
        common::wait_readable(slave.as_fd()),
        "the pty never delivered the tail of the reply"
    );
    let mut after = Vec::new();
    input.drain(&size, |msg| after.push(msg));
    assert!(
        matches!(after.as_slice(), [Msg::CapsUpgraded(caps)] if caps.sync),
        "the tail must complete the answer the probe half-heard, reaching the \
         session as the capability it reports rather than as `1`, `$`, `y`: \
         {after:?}"
    );

    rustix::io::write(&master, b"b").unwrap();
    assert!(
        common::wait_readable(slave.as_fd()),
        "the pty never delivered the write after the reply"
    );
    let mut resumed = Vec::new();
    input.drain(&size, |msg| resumed.push(msg));
    assert!(
        matches!(resumed.as_slice(), [Msg::Key(key)] if key.notation == "b"),
        "input after the completed answer must still arrive: {resumed:?}"
    );

    drop(input);
    // a master still open keeps this session's own descriptor 0 alive for
    // whatever the harness does next
    drop(master);
}
