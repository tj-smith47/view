//! Noticing that the write side has stopped moving.
//!
//! The outbox queue is unbounded and stays that way. Every message on it is
//! either a msgpack `Request` the peer must answer or a notification whose
//! order is load-bearing, so there is nothing on it that can be dropped:
//! discarding a `Request` leaves nvim's response loop waiting on a reply
//! that will never be sent, and discarding a notification silently rewrites
//! the buffer. The queue therefore never refuses work, and a peer that has
//! stopped reading is answered by saying so rather than by shedding load.

use std::time::{Duration, Instant};

use crate::handle::EngineHandle;

/// How long the writer thread may go without delivering a message, while
/// messages are still queued for it, before the connection counts as
/// wedged.
///
/// Twice the five seconds this crate already allows a healthy engine for a
/// single round trip (`nvim_api`'s attach, eval and get-mode timeouts and
/// `EngineConfig::handshake_timeout` all sit at five seconds). One round
/// trip is the longest a responsive nvim is ever expected to keep view
/// waiting, so any threshold below it would call a merely busy peer wedged.
/// The doubling additionally covers the one legitimately long single write:
/// a multi-megabyte paste is one `write_all`, and it counts as delivered
/// only once the whole message is out, so a peer draining it steadily shows
/// no motion until it finishes. Much beyond that and the notice arrives
/// after the operator has stopped connecting it to whatever they just did,
/// which is the only moment the information is worth anything.
pub const WRITER_STALL_THRESHOLD: Duration = Duration::from_secs(10);

/// Watches one connection's write side across successive observations and
/// answers whether it is wedged: messages queued for the writer thread, and
/// not one of them delivered for longer than the threshold.
///
/// The elapsed time is measured here, between two observations, rather than
/// stamped by the writer thread itself, and that is the only arrangement
/// that can tell a wedge from an idle connection. A wall-clock stamp the
/// writer takes says when it last delivered something, which after a quiet
/// minute is a minute ago whether or not anything is wrong; the instant the
/// next message is queued, that stale stamp reads as a minute-long stall
/// that never happened, and the queueing side may not grow a single
/// instruction to refresh it -- it is the caller's send path, the one this
/// engine's latency is measured on. Anchoring the window at the first
/// observation that finds work queued and the delivered count unmoved
/// measures only time in which the writer demonstrably had something to
/// write and did not.
#[derive(Debug)]
pub struct OutboxStallWatch {
    threshold: Duration,
    /// The delivered count as of the previous observation.
    delivered: u64,
    /// When the current unbroken stall was first observed, or `None` when
    /// the last observation found the queue empty or the count moved.
    since: Option<Instant>,
}

impl Default for OutboxStallWatch {
    fn default() -> Self {
        Self::new(WRITER_STALL_THRESHOLD)
    }
}

impl OutboxStallWatch {
    /// A watch that reports a wedge after `threshold` of queued-but-unmoving
    /// output. Production uses [`Default`], which supplies
    /// [`WRITER_STALL_THRESHOLD`]; an explicit threshold exists so a test
    /// can drive the same predicate without spending the real one.
    #[must_use]
    pub fn new(threshold: Duration) -> Self {
        Self {
            threshold,
            delivered: 0,
            since: None,
        }
    }

    /// Folds one reading of `handle`'s write side into the watch and
    /// answers whether that connection is wedged right now.
    ///
    /// Two relaxed atomic loads, one comparison and no allocation, so a
    /// paint loop can afford to ask on every pass -- which it must, since
    /// this reports only about the moments it is called and a wedged engine
    /// emits no redraws to wake anyone. It never touches the connection's
    /// lock and never blocks, wedged or not.
    #[must_use]
    pub fn observe(&mut self, handle: &EngineHandle) -> bool {
        let (queued, delivered) = handle.write_progress();
        self.fold(queued, delivered, Instant::now())
    }

    /// [`observe`](Self::observe) with both readings supplied, so the
    /// predicate is provable against an exact queue depth, an exact
    /// delivered count and an exact clock instead of a scheduler's.
    fn fold(&mut self, queued: usize, delivered: u64, now: Instant) -> bool {
        let moved = delivered != self.delivered;
        self.delivered = delivered;
        if queued == 0 {
            // nothing owed, so nothing is being withheld however long the
            // writer has been quiet; the next message queued starts its own
            // window from the observation that first sees it
            self.since = None;
            return false;
        }
        if moved {
            // a delivery restarts the window at the observation that saw
            // it, rather than at the one after: the gap between two
            // observations belongs to the stall it precedes, and dropping
            // it would let a lazily-observed connection under-report
            self.since = Some(now);
            return false;
        }
        let since = *self.since.get_or_insert(now);
        // saturating: `Instant::duration_since` yields zero for an earlier
        // `now`, so a clock reading out of order can only under-report a
        // stall, never invent one
        now.duration_since(since) >= self.threshold
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const THRESHOLD: Duration = Duration::from_secs(10);

    fn watch() -> OutboxStallWatch {
        OutboxStallWatch::new(THRESHOLD)
    }

    #[test]
    fn a_queue_whose_writer_never_delivers_reads_as_wedged_once_past_the_threshold() {
        let mut w = watch();
        let t0 = Instant::now();
        assert!(!w.fold(1, 0, t0));
        assert!(!w.fold(1, 0, t0 + THRESHOLD / 2));
        assert!(w.fold(1, 0, t0 + THRESHOLD));
        assert!(w.fold(3, 0, t0 + THRESHOLD * 10));
    }

    #[test]
    fn an_empty_queue_never_reads_as_wedged_however_long_the_writer_has_been_idle() {
        let mut w = watch();
        let t0 = Instant::now();
        assert!(!w.fold(0, 7, t0));
        assert!(!w.fold(0, 7, t0 + THRESHOLD * 100));
    }

    #[test]
    fn a_writer_delivering_between_observations_never_reads_as_wedged() {
        let mut w = watch();
        let t0 = Instant::now();
        for n in 1..=20 {
            let now = t0 + THRESHOLD / 2 * n;
            assert!(
                !w.fold(4, u64::from(n), now),
                "delivery {n} read as a wedge"
            );
        }
    }

    #[test]
    fn a_queue_appearing_after_a_long_idle_is_timed_from_when_it_appeared() {
        // the false positive the tick-side clock exists to prevent: a
        // writer idle for an hour has not stalled, and the message just
        // handed to it deserves the full threshold of its own
        let mut w = watch();
        let t0 = Instant::now();
        assert!(!w.fold(0, 1, t0));
        assert!(!w.fold(1, 1, t0 + Duration::from_secs(3600)));
        assert!(!w.fold(
            1,
            1,
            t0 + Duration::from_secs(3600) + THRESHOLD - Duration::from_millis(1)
        ));
        assert!(w.fold(1, 1, t0 + Duration::from_secs(3600) + THRESHOLD));
    }

    #[test]
    fn the_wedge_clears_as_soon_as_one_message_is_delivered() {
        let mut w = watch();
        let t0 = Instant::now();
        assert!(!w.fold(2, 0, t0));
        assert!(w.fold(2, 0, t0 + THRESHOLD));
        assert!(!w.fold(2, 1, t0 + THRESHOLD));
    }

    #[test]
    fn a_second_stall_is_timed_from_the_delivery_that_ended_the_first() {
        let mut w = watch();
        let t0 = Instant::now();
        assert!(!w.fold(2, 0, t0));
        assert!(w.fold(2, 0, t0 + THRESHOLD));
        let recovered = t0 + THRESHOLD;
        assert!(!w.fold(2, 1, recovered));
        assert!(!w.fold(2, 1, recovered + THRESHOLD / 2));
        assert!(!w.fold(2, 1, recovered + THRESHOLD - Duration::from_millis(1)));
        assert!(w.fold(2, 1, recovered + THRESHOLD));
    }

    #[test]
    fn the_default_threshold_is_the_documented_one() {
        assert_eq!(
            OutboxStallWatch::default().threshold,
            WRITER_STALL_THRESHOLD
        );
    }
}
