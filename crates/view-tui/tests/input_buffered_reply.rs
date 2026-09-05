//! `InputSource` against a real terminal descriptor holding a capability
//! reply in front of a keystroke.
//!
//! The shape under test is one read carrying both: a terminal answers a
//! query after the startup prober has stopped listening, the user types,
//! and the two land together. What must come out of it is the keystroke
//! and nothing else -- the reply consumed, the key behind it delivered,
//! and the loop never told to sleep on a keystroke it has not handed over.
//!
//! A pty this test owns, put on descriptor 0, is what makes that
//! reachable: one write into the master reproduces the coalesced read
//! exactly. Descriptor 0 is process state, so this file deliberately holds
//! one test.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use view_core::msg::Msg;
use view_tui::input::InputSource;
use view_tui::terminal::TermSizeCell;

/// A DA1 answer (`ESC [ ? 62 ; 1 ; 6 c`) immediately followed by one
/// keystroke: crossterm parses the first into `PrimaryDeviceAttributes`,
/// which its public event filter drops, and the second into a key it would
/// hand over if it ever got that far.
const REPLY_THEN_KEY: &[u8] = b"\x1b[?62;1;6ca";

#[test]
fn has_buffered_reports_a_key_parsed_behind_a_dropped_capability_reply() {
    use std::os::fd::AsFd;

    // this test's failure mode is a block, not a wrong answer
    let _watchdog = view_test_support::watchdog();
    let (master, slave) = common::stdin_pty();

    // opened while the queue is empty, so nothing of the burst written
    // below is consumed before the assertions about it
    let mut input = InputSource::open().unwrap();
    assert!(
        !input.has_buffered(),
        "a terminal nothing has been written to must report nothing buffered"
    );

    rustix::io::write(&master, REPLY_THEN_KEY).unwrap();
    assert!(
        common::wait_readable(slave.as_fd()),
        "the pty never delivered the bytes written into its master"
    );

    // the gate says nothing is held here, and that is the whole answer:
    // every byte this source reads is decoded in the call that reads it,
    // so there is no userspace buffer for the descriptor's own readiness
    // to be wrong about, and the loop wakes on the fd that is still ready
    assert!(
        !input.has_buffered(),
        "a source that reports something held must be holding it for a \
         caller to collect; nothing here is"
    );

    let size = TermSizeCell::default();
    let mut drained = Vec::new();
    input.drain(&size, |msg| drained.push(msg));
    assert!(
        matches!(drained.as_slice(), [Msg::Key(key)] if key.notation == "a"),
        "the drain that follows a positive answer must produce that same \
         keystroke, not a different event: {drained:?}"
    );

    report_empty_queue_cost(&mut input);

    drop(input);
    // the poll deadline is unbounded when nothing is ready, and a master
    // still open leaves this session's own descriptor 0 alive for whatever
    // the harness does next
    drop(master);
}

/// Times `input`'s empty-queue answer over the call count in
/// `VIEW_HAS_BUFFERED_ITERS`, printing it and doing nothing at all when that
/// variable is unset.
///
/// What the runtime loop pays for this gate is one empty-queue answer per
/// entry into a readiness wait, and the terminal it has to be measured
/// against is a real one -- the same pty this test already owns and has
/// just drained, so the measurement runs where the assertions above hold
/// rather than on a host that happens to have a spare tty:
///
/// ```text
/// VIEW_HAS_BUFFERED_ITERS=200000 cargo test -p view-tui --release \
///     --test input_buffered_reply -- --nocapture
/// ```
fn report_empty_queue_cost(input: &mut InputSource) {
    let Some(iters) = std::env::var("VIEW_HAS_BUFFERED_ITERS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|iters| *iters > 0)
    else {
        return;
    };
    let mut answered = 0_u32;
    let start = std::time::Instant::now();
    for _ in 0..iters {
        if input.has_buffered() {
            answered += 1;
        }
    }
    let elapsed = start.elapsed();
    println!(
        "has_buffered empty-queue: {iters} calls in {elapsed:?}, {} ns/call, {answered} answered true",
        elapsed.as_nanos() / u128::from(iters)
    );
}
