//! Noticing that the read side has stopped answering.
//!
//! The write side's own watch ([`crate::stall`]) sees a backlog that stops
//! draining, which is the failure of a peer that has stopped *reading*. It
//! is blind to the opposite failure: a peer still draining view's output
//! while answering none of it. Nothing else in the connection notices that
//! one either -- a wedged engine emits no redraws, so the very traffic a
//! runtime loop would otherwise be woken by is exactly the traffic that
//! stops.
//!
//! The probe that closes the gap is `nvim_get_mode`, which the pinned
//! engine answers on receipt even while its main loop is blocked waiting
//! for a key (see [`crate::nvim_api`]'s `get_mode`), so an engine parked at
//! a hit-enter prompt still reads as alive while one hung inside a
//! synchronous call does not.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::handle::{EngineError, EngineHandle};

/// How long a probe may go unacknowledged, while the connection is still
/// open, before the engine counts as wedged rather than merely slow to
/// answer this one probe.
///
/// Twice the five seconds this crate already allows a healthy engine for a
/// single round trip, by the same reasoning
/// [`WRITER_STALL_THRESHOLD`](crate::stall::WRITER_STALL_THRESHOLD) applies
/// to the write side -- the pattern, deliberately not the constant: a
/// wedged read side and a wedged write side are different failures needing
/// different recoveries, and one shared number would tie the two verdicts
/// together for no reason beyond their happening to agree today.
pub const HEARTBEAT_WEDGE_THRESHOLD: Duration = Duration::from_secs(10);

/// How often the background prober issues a fresh `nvim_get_mode` probe.
/// Four probes fit inside [`HEARTBEAT_WEDGE_THRESHOLD`] before a verdict
/// can flip, so one dropped or slow reply does not read as a false wedge.
pub const HEARTBEAT_PROBE_INTERVAL: Duration = Duration::from_secs(2);

/// What one [`HeartbeatWatch::observe`] call reports about the read side.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// The engine answered inside the threshold, or no probe has had time
    /// to go stale yet.
    Alive,
    /// No probe has been answered for longer than the threshold while the
    /// connection itself is still open. The engine process is there and its
    /// pipe is healthy; what has stopped is the answering, which the probe
    /// this watch rides is chosen to keep working through the states that
    /// are not failures (see the module docs).
    Wedged,
    /// The connection is closed. Distinct from [`Wedged`](Self::Wedged): a
    /// wedged engine may still recover on its own, a dead one never will,
    /// so the recovery each verdict calls for is different.
    Dead,
}

/// The state the prober thread and the runtime loop genuinely share.
///
/// Two relaxed atomics and nothing else, so the loop's side of the exchange
/// is a load rather than a lock: the one thing an observer must never do is
/// wait on the connection, since being unable to answer is precisely the
/// condition it is asking about.
#[derive(Debug, Default)]
struct Probe {
    sent_generation: AtomicU64,
    paused: AtomicBool,
}

/// Issues the periodic probe whose answers [`HeartbeatWatch`] folds.
///
/// A separate type from the watch, and the only one carrying a method that
/// sends anything, so "the paint loop never awaits RPC" holds by
/// construction rather than by discipline: the half the loop owns has no
/// send on it to call. Cheap to clone; every clone drives the same probe
/// state.
#[derive(Debug, Clone)]
pub struct HeartbeatProber {
    probe: Arc<Probe>,
}

impl HeartbeatProber {
    /// Issues one fresh probe, tagged with a generation one higher than the
    /// last. A no-op returning `Ok(())` while the watch is paused (see
    /// [`HeartbeatWatch::pause`]).
    ///
    /// Ticks are not acknowledgement-gated: a probe goes out on schedule
    /// whether or not the previous one was ever answered, so the answer
    /// that ends a wedge arrives on the next tick rather than waiting for a
    /// reply that is never coming.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` on the same terms as
    /// [`EngineHandle::request_heartbeat`]: the connection is already
    /// closed, or its writer thread has exited.
    pub fn tick(&self, handle: &EngineHandle) -> Result<(), EngineError> {
        if self.probe.paused.load(Ordering::Relaxed) {
            return Ok(());
        }
        // bumped before the send, never after: a generation that was
        // allocated but whose request failed to queue must still never be
        // handed to a second probe, or one answer would resolve two
        let generation = self.probe.sent_generation.fetch_add(1, Ordering::Relaxed) + 1;
        handle.request_heartbeat(generation)
    }
}

/// Watches one connection's read side across successive observations.
///
/// The window is anchored on the last *answer*, not on the last probe sent,
/// and that is the only arrangement that can report a wedge at all: probes
/// go out on a fixed cadence regardless of whether anything answers them,
/// so a window restarted by each send would never grow past one tick
/// interval however long the engine stayed silent. Anchoring on answers
/// measures exactly the time in which the engine was asked and did not
/// reply.
///
/// `sent_generation` is a relaxed atomic because the prober thread writes
/// it and the observing thread reads it -- the one genuine cross-thread
/// edge in this design. The acknowledgement side needs no atomic at all:
/// [`record_ack`](Self::record_ack) and [`observe`](Self::observe) are both
/// called from the runtime loop's own thread, the first from its message
/// dispatch and the second from its pass over the frame.
#[derive(Debug)]
pub struct HeartbeatWatch {
    threshold: Duration,
    probe: Arc<Probe>,
    /// The newest generation the engine has answered for.
    acked_generation: u64,
    /// When the current unanswered window opened: the last answer, or the
    /// point this watch was created or resumed.
    since: Instant,
}

impl Default for HeartbeatWatch {
    fn default() -> Self {
        Self::new(HEARTBEAT_WEDGE_THRESHOLD)
    }
}

impl HeartbeatWatch {
    /// A watch that reports a wedge after `threshold` of probes going
    /// unanswered. Production uses [`Default`], which supplies
    /// [`HEARTBEAT_WEDGE_THRESHOLD`]; an explicit threshold exists so a
    /// test can drive the same predicate without spending the real one.
    #[must_use]
    pub fn new(threshold: Duration) -> Self {
        Self {
            threshold,
            probe: Arc::new(Probe::default()),
            acked_generation: 0,
            since: Instant::now(),
        }
    }

    /// The send half of this watch, for the thread that owns the cadence.
    #[must_use]
    pub fn prober(&self) -> HeartbeatProber {
        HeartbeatProber {
            probe: Arc::clone(&self.probe),
        }
    }

    /// Records that the engine answered the probe tagged `generation`.
    ///
    /// A reply older than one already recorded is folded in without moving
    /// the acknowledged generation backwards, which is what the generation
    /// tag is for: replies can arrive out of order, and an older one
    /// arriving late is still proof the engine answered just now.
    pub fn record_ack(&mut self, generation: u64) {
        self.record_ack_at(generation, Instant::now());
    }

    /// Folds the connection's read side into a verdict.
    ///
    /// Never blocks and never sends: one relaxed load, one comparison and
    /// no allocation, so a paint loop can afford to ask on every pass --
    /// which it must, since a wedged engine emits no redraws to wake anyone
    /// and this reports only about the moments it is called. The send lives
    /// on [`HeartbeatProber::tick`]; this call cannot violate "the paint
    /// loop never awaits RPC" whatever the connection is doing.
    ///
    /// `connection_closed` is the caller's reading of
    /// [`EngineHandle::is_closed`], passed in rather than taken here so
    /// this fold stays answerable without a connection at all.
    #[must_use]
    pub fn observe(&self, connection_closed: bool) -> Liveness {
        if !connection_closed && !self.outstanding() {
            // the steady state -- a reply is back within a round trip of
            // the probe that asked for it, so the overwhelming majority of
            // passes find nothing owed -- and so the one that has to cost
            // the least: two relaxed loads, no clock read
            return Liveness::Alive;
        }
        self.fold(connection_closed, Instant::now())
    }

    /// How long a caller may wait for its own next wakeup before this watch
    /// would have something new to say, or `None` when it would not.
    ///
    /// `None` means "wait as long as you like": with nothing outstanding
    /// there is no silence to time, so no wakeup this watch asked for could
    /// tell the caller anything. A duration is returned only while a probe
    /// is unanswered, which is the one window in which a caller that never
    /// woke again would sit on an unreported wedge -- and while the engine
    /// is healthy its own answer arrives first and wakes the caller before
    /// this deadline ever expires, so the wakeup is paid for only by a
    /// connection that has actually gone quiet.
    ///
    /// Never zero, since a zero wait is a spin. Past the threshold the
    /// window has already said what it had to say, and its remaining job --
    /// noticing that the engine started answering again -- is served by
    /// looking again one threshold later.
    #[must_use]
    pub fn poll_deadline(&self) -> Option<Duration> {
        if !self.outstanding() {
            return None;
        }
        self.deadline_at(Instant::now())
    }

    /// Stops [`HeartbeatProber::tick`] from issuing any *new* probe.
    ///
    /// Does not guarantee that no reply arrives afterwards: probes already
    /// dispatched by prior ticks are still in flight and the peer still
    /// answers them. Never rewinds the sent generation.
    pub fn pause(&self) {
        self.probe.paused.store(true, Ordering::Relaxed);
    }

    /// Clears the paused flag and re-anchors the unanswered window at now,
    /// so a pause is never counted as silence the engine is responsible
    /// for.
    ///
    /// Deliberately leaves the sent and acknowledged generations exactly
    /// where [`pause`](Self::pause) found them, so the pair stays
    /// monotonic-forward across the cycle: a late reply to a probe sent
    /// before the pause raises the acknowledged generation to at most its
    /// pre-pause value, strictly below any generation a post-resume tick
    /// produces, so it can never satisfy a later wedge check and mask a
    /// genuine hang. It does re-anchor the window, which is correct rather
    /// than lenient: an answer, however old the question, is the engine
    /// demonstrably answering at that moment.
    pub fn resume(&mut self) {
        self.resume_at(Instant::now());
    }

    /// [`record_ack`](Self::record_ack) against an exact clock.
    fn record_ack_at(&mut self, generation: u64, now: Instant) {
        if generation > self.acked_generation {
            self.acked_generation = generation;
        }
        self.since = now;
    }

    /// [`resume`](Self::resume) against an exact clock.
    fn resume_at(&mut self, now: Instant) {
        self.probe.paused.store(false, Ordering::Relaxed);
        self.since = now;
    }

    /// [`poll_deadline`](Self::poll_deadline) against an exact clock.
    fn deadline_at(&self, now: Instant) -> Option<Duration> {
        if !self.outstanding() {
            return None;
        }
        let remaining = self
            .threshold
            .saturating_sub(now.duration_since(self.since));
        Some(if remaining.is_zero() {
            self.threshold
        } else {
            remaining
        })
    }

    /// Whether a probe has been sent that no reply has yet accounted for.
    fn outstanding(&self) -> bool {
        self.probe.sent_generation.load(Ordering::Relaxed) > self.acked_generation
    }

    /// [`observe`](Self::observe) against an exact clock, so the predicate
    /// is provable against an exact elapsed time instead of a scheduler's.
    fn fold(&self, connection_closed: bool, now: Instant) -> Liveness {
        if connection_closed {
            // outranks every timing question below: a probe still waiting
            // on a connection that is gone is waiting on nothing, and the
            // patience the threshold buys is patience for an engine that
            // might yet answer
            return Liveness::Dead;
        }
        if !self.outstanding() {
            // nothing is owed an answer, so nothing is being withheld
            // however long the connection has been quiet
            return Liveness::Alive;
        }
        // saturating: `Instant::duration_since` yields zero for an earlier
        // `now`, so a clock reading out of order can only under-report a
        // silence, never invent one
        if now.duration_since(self.since) >= self.threshold {
            Liveness::Wedged
        } else {
            Liveness::Alive
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const THRESHOLD: Duration = Duration::from_secs(10);

    /// A watch and its prober over a connection whose peer neither answers
    /// nor hangs up, so a probe genuinely goes unanswered rather than
    /// failing to send. The returned guards keep that connection open for
    /// the test's duration.
    fn watched_connection() -> (
        HeartbeatWatch,
        HeartbeatProber,
        EngineHandle,
        (
            std::sync::mpsc::Sender<()>,
            std::sync::mpsc::Receiver<crate::handle::EngineNotification>,
        ),
    ) {
        let (source, keep_open) = crate::test_peer::IdleSource::new();
        let (handle, notifs) = EngineHandle::start(source, std::io::sink());
        let watch = HeartbeatWatch::new(THRESHOLD);
        let prober = watch.prober();
        (watch, prober, handle, (keep_open, notifs))
    }

    #[test]
    fn a_probe_no_one_answers_reads_as_wedged_once_past_the_threshold() {
        let (watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.since;
        prober.tick(&handle).unwrap();
        assert_eq!(watch.fold(false, t0), Liveness::Alive);
        assert_eq!(watch.fold(false, t0 + THRESHOLD / 2), Liveness::Alive);
        assert_eq!(watch.fold(false, t0 + THRESHOLD), Liveness::Wedged);
    }

    #[test]
    fn a_connection_no_one_has_probed_never_reads_as_wedged() {
        let (watch, _prober, _handle, _guards) = watched_connection();
        let t0 = watch.since;
        assert_eq!(watch.fold(false, t0), Liveness::Alive);
        assert_eq!(watch.fold(false, t0 + THRESHOLD * 100), Liveness::Alive);
    }

    #[test]
    fn an_engine_answering_between_probes_never_reads_as_wedged() {
        let (mut watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.since;
        for n in 1..=20 {
            let now = t0 + THRESHOLD / 2 * n;
            prober.tick(&handle).unwrap();
            assert_eq!(
                watch.fold(false, now),
                Liveness::Alive,
                "answer {n} read as a wedge"
            );
            watch.record_ack_at(u64::from(n), now);
        }
    }

    #[test]
    fn the_wedge_clears_as_soon_as_one_probe_is_answered() {
        let (mut watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.since;
        prober.tick(&handle).unwrap();
        assert_eq!(watch.fold(false, t0 + THRESHOLD), Liveness::Wedged);
        watch.record_ack_at(1, t0 + THRESHOLD);
        assert_eq!(watch.fold(false, t0 + THRESHOLD), Liveness::Alive);
    }

    /// The whole reason the probe carries a generation: an answer that
    /// arrives after a newer one must not rewind what has been answered,
    /// or the newer probe would be owed an answer it already received and
    /// the connection would time itself out while healthy.
    #[test]
    fn a_reply_arriving_out_of_order_never_rewinds_what_has_been_answered() {
        let (mut watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.since;
        prober.tick(&handle).unwrap();
        prober.tick(&handle).unwrap();
        watch.record_ack_at(2, t0);
        watch.record_ack_at(1, t0);
        assert_eq!(watch.fold(false, t0 + THRESHOLD * 10), Liveness::Alive);
    }

    /// The second stall is timed from the answer that ended the first, not
    /// from the first observation after it: the gap between two
    /// observations belongs to the silence it precedes.
    #[test]
    fn a_second_silence_is_timed_from_the_answer_that_ended_the_first() {
        let (mut watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.since;
        prober.tick(&handle).unwrap();
        assert_eq!(watch.fold(false, t0 + THRESHOLD), Liveness::Wedged);
        let recovered = t0 + THRESHOLD;
        watch.record_ack_at(1, recovered);
        prober.tick(&handle).unwrap();
        assert_eq!(
            watch.fold(false, recovered + THRESHOLD - Duration::from_millis(1)),
            Liveness::Alive
        );
        assert_eq!(watch.fold(false, recovered + THRESHOLD), Liveness::Wedged);
    }

    #[test]
    fn nothing_outstanding_asks_for_no_wakeup_at_all() {
        let (mut watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.since;
        assert_eq!(watch.deadline_at(t0), None);
        prober.tick(&handle).unwrap();
        assert_eq!(watch.deadline_at(t0), Some(THRESHOLD));
        assert_eq!(
            watch.deadline_at(t0 + THRESHOLD / 4),
            Some(THRESHOLD / 4 * 3)
        );
        watch.record_ack_at(1, t0);
        assert_eq!(watch.deadline_at(t0), None);
    }

    /// Never zero once the verdict has flipped: a zero wait is a spin, and
    /// the window's remaining job -- noticing the engine answering again --
    /// is served by looking one threshold later.
    #[test]
    fn a_confirmed_wedge_keeps_asking_so_the_recovery_is_seen_without_a_keystroke() {
        let (watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.since;
        prober.tick(&handle).unwrap();
        assert_eq!(watch.deadline_at(t0 + THRESHOLD), Some(THRESHOLD));
        assert_eq!(watch.deadline_at(t0 + THRESHOLD * 10), Some(THRESHOLD));
    }

    /// A closed connection is not a slow one, and no amount of patience
    /// changes it: the verdict lands on the first observation, with no
    /// probe ever issued and no time elapsed, because the recovery it calls
    /// for is a different one and waiting only delays it.
    #[test]
    fn a_closed_connection_reads_as_dead_with_nothing_probed_and_no_time_elapsed() {
        let (mut watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.since;
        assert_eq!(watch.fold(true, t0), Liveness::Dead);
        prober.tick(&handle).unwrap();
        assert_eq!(watch.fold(true, t0), Liveness::Dead);
        watch.record_ack_at(1, t0);
        assert_eq!(watch.fold(true, t0), Liveness::Dead);
        assert_eq!(watch.fold(true, t0 + THRESHOLD * 10), Liveness::Dead);
    }

    /// The whole chain in one pass, since every link between the tick and
    /// the verdict is a place the generation can be lost: the probe goes
    /// out, a peer answers it, the reader thread decodes and routes the
    /// acknowledgement, and the generation that comes off the sink is the
    /// one that closes the window. An answer carrying any other generation
    /// leaves the probe outstanding and this reads as a wedge.
    #[test]
    fn an_answered_probe_routes_the_whole_way_back_into_the_watch() {
        let (peer_read, our_write) = std::io::pipe().unwrap();
        let (our_read, mut peer_write) = std::io::pipe().unwrap();
        let pump = crate::damage::PumpShared::new();
        let handle = EngineHandle::start_pumped(
            our_read,
            our_write,
            Arc::clone(&pump),
            #[cfg(any(unix, windows))]
            None,
        );
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let _dpump = pump.attach_sink(tx);

        let mut watch = HeartbeatWatch::new(THRESHOLD);
        watch.prober().tick(&handle).unwrap();

        let mut r = std::io::BufReader::new(peer_read);
        let value = rmpv::decode::read_value(&mut r).unwrap();
        let crate::rpc::RpcMessage::Request { msgid, method, .. } =
            crate::rpc::RpcMessage::from_value(value).unwrap()
        else {
            unreachable!("expected a Request");
        };
        assert_eq!(method, "nvim_get_mode");
        let reply = crate::rpc::RpcMessage::Response {
            msgid,
            error: rmpv::Value::Nil,
            result: rmpv::Value::Map(vec![
                (rmpv::Value::from("mode"), rmpv::Value::from("n")),
                (rmpv::Value::from("blocking"), rmpv::Value::from(false)),
            ]),
        };
        rmpv::encode::write_value(&mut peer_write, &reply.to_value()).unwrap();
        std::io::Write::flush(&mut peer_write).unwrap();

        let msg = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let view_core::msg::Msg::HeartbeatReply { generation } = msg else {
            unreachable!("expected HeartbeatReply, got {msg:?}");
        };
        watch.record_ack(generation);
        assert_eq!(
            watch.fold(false, watch.since + THRESHOLD * 10),
            Liveness::Alive
        );
    }

    #[test]
    fn a_paused_watch_issues_no_further_probe_however_often_it_is_ticked() {
        let (mut watch, prober, handle, _guards) = watched_connection();
        prober.tick(&handle).unwrap();
        watch.pause();
        for _ in 0..5 {
            prober.tick(&handle).unwrap();
        }
        assert_eq!(watch.probe.sent_generation.load(Ordering::Relaxed), 1);
        watch.resume_at(watch.since);
        prober.tick(&handle).unwrap();
        assert_eq!(watch.probe.sent_generation.load(Ordering::Relaxed), 2);
    }

    /// Both halves together, because either alone proves the wrong thing: a
    /// fold that swallowed the late reply would pass the first assertion by
    /// being blind, and one that let it stand in for a post-resume answer
    /// would pass nothing after it. The pair is what shows the reply is
    /// harmless *and* that a genuine hang right after it is still caught.
    #[test]
    fn a_reply_landing_after_a_resume_is_harmless_without_masking_the_next_hang() {
        let (mut watch, prober, handle, _guards) = watched_connection();
        prober.tick(&handle).unwrap();
        watch.pause();
        let resumed = watch.since + THRESHOLD * 3;
        watch.resume_at(resumed);

        // the pre-pause probe's reply, arriving only now
        watch.record_ack_at(1, resumed);
        assert_eq!(watch.fold(false, resumed), Liveness::Alive);

        // and from that same point, a probe nothing answers
        prober.tick(&handle).unwrap();
        assert_eq!(
            watch.fold(false, resumed + THRESHOLD - Duration::from_millis(1)),
            Liveness::Alive
        );
        assert_eq!(watch.fold(false, resumed + THRESHOLD), Liveness::Wedged);
    }

    /// The pause is not counted against the engine: time spent with the
    /// prober deliberately silenced is time nobody asked it anything.
    #[test]
    fn a_pause_longer_than_the_threshold_is_not_charged_to_the_engine() {
        let (mut watch, prober, handle, _guards) = watched_connection();
        prober.tick(&handle).unwrap();
        watch.pause();
        let resumed = watch.since + THRESHOLD * 10;
        watch.resume_at(resumed);
        assert_eq!(watch.fold(false, resumed), Liveness::Alive);
    }

    #[test]
    fn the_default_threshold_is_the_documented_one() {
        assert_eq!(
            HeartbeatWatch::default().threshold,
            HEARTBEAT_WEDGE_THRESHOLD
        );
    }
}
