//! `InputSource::has_buffered` against a real terminal descriptor holding a
//! capability reply in front of a keystroke.
//!
//! The shape under test is the one no fd readiness poll can describe: a
//! terminal answers a query after the startup prober has stopped listening,
//! the user types, and both land in one kernel read. crossterm parses the
//! whole read but hands out one event per poll -- and never hands out the
//! reply at all, since its public filter rejects it -- so a single poll
//! reports "nothing buffered" about a buffer holding a decoded key. The
//! runtime loop then sleeps on a descriptor whose queue it has itself just
//! emptied, and the keystroke waits for an unrelated later one.
//!
//! A pty this test owns, put on descriptor 0 before crossterm's
//! process-wide reader binds to anything, is what makes that reachable: the
//! reader resolves the same descriptor the source does, and one write into
//! the master reproduces the coalesced read exactly. Descriptor 0 is
//! process state, so this file deliberately holds one test -- the reader is
//! built once per process and cannot be pointed at a second terminal later.

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

    let (master, slave) = common::stdin_pty();

    // opened while the queue is empty, so crossterm's reader binds to this
    // terminal without consuming any of the burst written below
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

    assert!(
        input.has_buffered(),
        "the keystroke behind the dropped reply is decodable, so the gate \
         that decides whether the loop may sleep must say so -- answering \
         no here strands it until an unrelated later keystroke arrives"
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
