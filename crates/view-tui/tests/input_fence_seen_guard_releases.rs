//! The guard armed on a tail alone hands the terminal back when that tail
//! resolves, rather than idling out the cap.
//!
//! A terminal that answered out of order leaves a half-delivered run behind
//! a fence that already arrived, and the run has to be held for the read
//! that finishes it. Nothing else is owed, though: the fence is the probe's
//! last question. Once the run resolves, keeping the guard would route
//! every keystroke for the rest of `PROBE_HARD_CAP` through the residue
//! decoder in exchange for a reply that cannot come.
//!
//! Descriptor 0 is process state, so this file holds one test.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;
use view_core::msg::Msg;
use view_tui::input::InputSource;
use view_tui::terminal::TermSizeCell;
use view_tui::tiers::{EnvHints, Probe, PROBE_DEADLINE};

/// The DA1 fence, then an introducer with no final byte yet: a live prefix
/// of a cursor-key sequence, and of several answer grammars besides.
const FENCE_THEN_INTRODUCER: &[u8] = b"\x1b[?1;2c\x1b[";

/// The byte that resolves it, making the run an arrow rather than an answer.
const ARROW_FINAL: &[u8] = b"A";

#[test]
fn a_guard_armed_on_a_tail_alone_releases_when_that_tail_resolves() {
    use std::os::fd::AsFd;

    // this test's failure mode is a block, not a wrong answer
    let _watchdog = view_test_support::watchdog();
    let (master, slave) = common::stdin_pty();

    rustix::io::write(&master, FENCE_THEN_INTRODUCER).unwrap();
    assert!(
        common::wait_readable(slave.as_fd()),
        "the pty never delivered the fence and the introducer"
    );

    let mut queries = Vec::new();
    let probe = Probe::start(
        common::SlaveSource,
        &mut queries,
        PROBE_DEADLINE,
        &EnvHints::default(),
    )
    .unwrap();
    let outcome = probe.finish(Duration::ZERO);
    assert!(
        outcome.fence_seen,
        "the fence is in the same segment as the introducer; the probe must have read it"
    );
    assert_eq!(
        outcome.partial_reply, b"\x1b[",
        "an introducer is a live prefix of an answer grammar, so the scan \
         must stop on it rather than call it residue"
    );

    let mut input =
        InputSource::open_after_probe(outcome.caps, outcome.fence_seen, outcome.partial_reply)
            .unwrap();
    let size = TermSizeCell::default();
    assert!(
        input.still_listening(),
        "a tail the settle cut in half is a run the guard has to finish, \
         whatever the fence did"
    );

    rustix::io::write(&master, ARROW_FINAL).unwrap();
    assert!(
        common::wait_readable(slave.as_fd()),
        "the pty never delivered the byte that finishes the sequence"
    );
    let mut resolved = Vec::new();
    input.drain(&size, |msg| resolved.push(msg));
    assert!(
        matches!(resolved.as_slice(), [Msg::Key(key)] if key.notation == "<Up>"),
        "the run was the user's arrow, and the read that finishes it must \
         deliver the arrow rather than an `A`: {resolved:?}"
    );

    assert!(
        !input.still_listening(),
        "with its fence long answered and its tail resolved, the guard is \
         owed nothing and must have handed the terminal back rather than \
         holding the key path for the rest of the cap"
    );

    drop(input);
    // a master still open keeps this session's own descriptor 0 alive for
    // whatever the harness does next
    drop(master);
}
