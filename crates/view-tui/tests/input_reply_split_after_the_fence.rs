//! A capability answer split by the settle on a terminal whose DA1 fence
//! has already arrived.
//!
//! The fence is asked last, so a fence that arrived normally proves nothing
//! is still in flight and the input path needs no guard. A terminal that
//! answers out of order breaks that: the fence lands, and behind it a reply
//! the probe was still reading is cut in half by the settle. Nothing about
//! the damage changes -- `1$y` handed to crossterm's parser is three keys
//! typed into the buffer and a capability lost for the session -- so the
//! tail decides whether the guard arms, exactly as a missing fence does.
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

/// The DA1 fence, then as much of a DECRPM answer as the segment carried.
const FENCE_THEN_REPLY_HEAD: &[u8] = b"\x1b[?1;2c\x1b[?2026;";

/// The rest of that answer, arriving once the guard owns the terminal.
const REPLY_TAIL: &[u8] = b"1$y";

#[test]
fn a_tail_left_behind_an_answered_fence_still_arms_the_guard() {
    use std::os::fd::AsFd;

    // this test's failure mode is a block, not a wrong answer
    let _watchdog = view_test_support::watchdog();
    let (master, slave) = common::stdin_pty();

    rustix::io::write(&master, FENCE_THEN_REPLY_HEAD).unwrap();
    assert!(
        common::wait_readable(slave.as_fd()),
        "the pty never delivered the fence and the head of the reply"
    );

    let mut queries = Vec::new();
    let probe = Probe::start(common::SlaveSource, &mut queries, PROBE_DEADLINE, None).unwrap();
    let outcome = probe.finish(Duration::ZERO);
    assert!(
        outcome.fence_seen,
        "the fence is in the same segment as the head; the probe must have read it"
    );
    assert!(
        !outcome.caps.sync,
        "half an answer resolves nothing: {:?}",
        outcome.caps
    );
    assert_eq!(
        outcome.partial_reply, b"\x1b[?2026;",
        "an answered fence does not make the tail behind it the user's"
    );

    let mut input =
        InputSource::open_after_probe(outcome.caps, outcome.fence_seen, outcome.partial_reply)
            .unwrap();
    let size = TermSizeCell::default();

    rustix::io::write(&master, REPLY_TAIL).unwrap();
    assert!(
        common::wait_readable(slave.as_fd()),
        "the pty never delivered the tail of the reply"
    );
    let mut after = Vec::new();
    input.drain(&size, |msg| after.push(msg));
    assert!(
        matches!(after.as_slice(), [Msg::CapsUpgraded(caps)] if caps.sync),
        "a fence that arrived is not permission to drop what is still \
         arriving behind it: the tail must complete the answer rather than \
         reach the parser as `1`, `$`, `y`: {after:?}"
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
