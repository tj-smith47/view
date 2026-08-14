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
//!
//! # What a verdict measures
//!
//! Exactly one thing, and a notice quoting it should claim no more: how
//! long the oldest probe still owed an answer has gone unanswered, as
//! observed from the thread that folds the verdict. Two consequences
//! follow, and both are deliberate.
//!
//! An engine that answers every probe, but always later than the threshold,
//! reads wedged for as long as that lasts rather than reading alive because
//! something arrived. The window advances to the oldest probe still
//! outstanding as answers land, so an answer never restarts it while an
//! older question is still open: an engine replying thirty seconds after
//! being asked is not a working editor, and the verdict says so.
//!
//! An observing thread stalled for longer than the threshold reads wedged
//! against a perfectly healthy engine, because acknowledgements are
//! recorded by that same thread: its replies are sitting in its own queue,
//! unfolded, and the verdict clears on the pass that drains them. That is a
//! true statement about the engine path as its consumer experiences it --
//! an editor whose loop is stuck is unresponsive whoever is at fault -- and
//! it is deliberately not a statement about the engine process alone.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use view_core::native::supervision::WedgeKind;

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

/// How many probe send times the shared state keeps.
///
/// A fold anchors on the oldest probe still outstanding, so it needs that
/// probe's send time to still be on record: twelve times as many as the
/// [`HEARTBEAT_WEDGE_THRESHOLD`] / [`HEARTBEAT_PROBE_INTERVAL`] ratio needs
/// at the shipped constants, and 512 bytes of shared state at that size. A
/// run outliving the log falls back to the oldest send time still held,
/// which is newer than the one overwritten and so under-reports the silence
/// -- the direction that delays a verdict rather than inventing one.
const SEND_LOG: u64 = 64;

// tied at compile time rather than by comment: shortening the probe interval
// shortens the ring in wall-clock terms, and a ring narrower than the
// threshold would leave the fold clamping against engines that are merely
// slow instead of measuring them
const _: () = assert!(
    SEND_LOG as u128 * HEARTBEAT_PROBE_INTERVAL.as_millis()
        >= 2 * HEARTBEAT_WEDGE_THRESHOLD.as_millis(),
    "SEND_LOG probe intervals must span at least twice HEARTBEAT_WEDGE_THRESHOLD"
);

/// How many times a fold re-reads the shared state before giving up on
/// getting a reading that no tick moved underneath it.
///
/// One retry would do at the prober's cadence -- a second collision needs
/// two ticks inside the handful of loads below, which is 4 s of probes in
/// the width of one function -- and three is the same reasoning with a
/// margin, at three relaxed loads apiece.
const ANCHOR_READ_ATTEMPTS: usize = 3;

/// How soon a caller told `Alive` by a raced anchor read is asked to look
/// again.
///
/// Short on purpose, and never a spin: reaching it costs
/// [`ANCHOR_READ_ATTEMPTS`] consecutive interleavings with a prober that
/// ticks once every [`HEARTBEAT_PROBE_INTERVAL`], and the tick that caused
/// the race has finished by the time the caller comes back. It is short
/// enough that a genuine wedge sitting under the raced reading reaches its
/// notice at the same time it otherwise would.
const UNSETTLED_RETRY: Duration = Duration::from_millis(1);

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

/// The state the prober thread and the observing thread genuinely share.
///
/// Atomics and nothing else, so the observer's side of the exchange is a
/// load rather than a lock: the one thing it must never do is wait on the
/// connection, since being unable to answer is precisely the condition it
/// is asking about.
#[derive(Debug)]
struct Probe {
    /// The monotonic base the send times are expressed against. `Instant`
    /// is not an atomic and nanoseconds since a fixed origin is, which is
    /// the whole reason this field exists.
    origin: Instant,
    /// The newest generation a probe has been issued for.
    sent_generation: AtomicU64,
    /// The newest generation the engine has answered for.
    acked_generation: AtomicU64,
    /// Send times in nanoseconds since `origin`, indexed by generation
    /// modulo [`SEND_LOG`].
    sent_at: [AtomicU64; SEND_LOG as usize],
    /// When the cadence last did anything, in nanoseconds since `origin`:
    /// the moment a probe went out, or the moment the watch was armed
    /// before the first one ever did.
    ///
    /// Relaxed at both ends, unlike the send log beside it, because nothing
    /// is published through it and no verdict is read from it. It dates a
    /// deadline for a probe that has not been sent yet
    /// ([`HeartbeatWatch::poll_deadline`]), so a reading a millisecond
    /// stale arms a wakeup a millisecond off, on a pass that then re-reads
    /// the generations this is not part of.
    cadence_at: AtomicU64,
    paused: AtomicBool,
}

impl Probe {
    /// Starts paused, so a connection nobody is observing yet is never
    /// charged for the silence -- see [`HeartbeatWatch::resume`].
    fn new() -> Self {
        Self {
            origin: Instant::now(),
            sent_generation: AtomicU64::new(0),
            acked_generation: AtomicU64::new(0),
            sent_at: std::array::from_fn(|_| AtomicU64::new(0)),
            cadence_at: AtomicU64::new(0),
            paused: AtomicBool::new(true),
        }
    }

    fn slot(generation: u64) -> usize {
        // the cast is exact for any remainder of SEND_LOG, which is far
        // below usize::MAX on every target this builds for
        (generation % SEND_LOG) as usize
    }

    /// `now` as nanoseconds since `origin`, saturating at both ends: a
    /// clock reading before the origin yields zero rather than wrapping,
    /// and an uptime past `u64`'s range (some 584 years) pins to the top.
    fn stamp(&self, now: Instant) -> u64 {
        u64::try_from(now.saturating_duration_since(self.origin).as_nanos()).unwrap_or(u64::MAX)
    }

    /// The oldest generation still owed an answer, or `None` when none is.
    ///
    /// Two loads and a comparison, with no clock read, so the steady state
    /// -- a reply back within a round trip of the probe that asked for it,
    /// which is the overwhelming majority of observations -- costs an
    /// observer nothing more than this.
    fn oldest_outstanding(&self) -> Option<u64> {
        let sent = self.sent_generation.load(Ordering::Acquire);
        let acked = self.acked_generation.load(Ordering::Relaxed);
        if sent <= acked {
            return None;
        }
        // clamped to what the log still holds: past that the true oldest
        // has been overwritten, and reading a newer generation's send time
        // shortens the silence rather than inventing one (see `SEND_LOG`)
        Some(Self::anchor_generation(sent, acked))
    }

    /// Which generation's send time anchors the window, given a `sent`/`acked`
    /// pair already read.
    fn anchor_generation(sent: u64, acked: u64) -> u64 {
        (acked + 1).max(sent.saturating_sub(SEND_LOG - 1))
    }

    /// The anchor's send time in nanoseconds, read against the `sent`
    /// generation the caller already observed, or `None` when a tick landed
    /// after that observation and the reading cannot be trusted.
    ///
    /// The check is not paranoia about a stale value; it is the one
    /// interleaving that can invert a verdict. Once the window is saturated
    /// the clamp above points at generation `sent - 63`, whose slot is the
    /// very slot the next tick (`sent + 1`) writes -- the two differ by
    /// exactly [`SEND_LOG`]. A tick landing between the caller's generation
    /// load and this stamp load would therefore hand back the *new* probe's
    /// send time for the *oldest* probe's slot, and an engine silent for
    /// hours would read as answered a moment ago. Re-reading the generation
    /// catches exactly that, and costs one acquire load on the path where a
    /// probe is already outstanding.
    fn anchor_nanos(&self, sent: u64, acked: u64) -> Option<u64> {
        let opened =
            self.sent_at[Self::slot(Self::anchor_generation(sent, acked))].load(Ordering::Relaxed);
        (self.sent_generation.load(Ordering::Acquire) == sent).then_some(opened)
    }

    /// How long the oldest probe still owed an answer has been outstanding
    /// at `now`.
    fn outstanding_for(&self, now: Instant) -> Outstanding {
        for _ in 0..ANCHOR_READ_ATTEMPTS {
            let sent = self.sent_generation.load(Ordering::Acquire);
            let acked = self.acked_generation.load(Ordering::Relaxed);
            if sent <= acked {
                return Outstanding::Nothing;
            }
            if let Some(opened) = self.anchor_nanos(sent, acked) {
                // saturating: a send stamped after `now` (a caller folding
                // against an earlier instant than the tick it is asking
                // about) yields zero rather than wrapping to an instant
                // wedge
                return Outstanding::For(Duration::from_nanos(
                    self.stamp(now).saturating_sub(opened),
                ));
            }
        }
        Outstanding::Unsettled
    }
}

/// What one anchor read found, kept apart from the duration it usually
/// yields because the third case is not a measurement at all.
///
/// A reading that raced a tick on every attempt has no send time behind it,
/// so it is evidence of nothing except that another look is owed. Folding
/// it in as a very long silence -- the shape that cannot hide a wedge --
/// makes the deadline arithmetic right and the verdict wrong: an engine
/// answering perfectly reads wedged for the one pass that raced, which is
/// long enough to raise a notice and repaint. The two consumers want
/// opposite things from it, so it is handed to them unresolved and each
/// decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outstanding {
    /// Every probe issued has been answered.
    Nothing,
    /// The oldest probe still owed an answer has been waiting this long.
    For(Duration),
    /// Every anchor read raced a tick; no send time here can be trusted.
    Unsettled,
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
    /// [`HeartbeatWatch::pause`]), on an open connection only.
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
    /// closed, or its writer thread has exited. A paused prober reports
    /// that too -- a caller that retires its thread on the first refusal
    /// would otherwise hold its handle for the process's lifetime whenever
    /// the connection died inside a pause.
    pub fn tick(&self, handle: &EngineHandle) -> Result<(), EngineError> {
        self.tick_at(handle, Instant::now())
    }

    /// [`tick`](Self::tick) against an exact clock.
    fn tick_at(&self, handle: &EngineHandle, now: Instant) -> Result<(), EngineError> {
        if handle.is_closed() {
            return Err(EngineError::Closed);
        }
        if self.probe.paused.load(Ordering::Acquire) {
            return Ok(());
        }
        let generation = self.probe.sent_generation.load(Ordering::Relaxed) + 1;
        let sent_at = self.probe.stamp(now);
        self.probe.sent_at[Probe::slot(generation)].store(sent_at, Ordering::Relaxed);
        // the same stamp, kept where a reader that finds nothing
        // outstanding can still date the cadence from it: that reader is
        // asking when the *next* probe is owed, and this tick is what the
        // answer counts forward from
        self.probe.cadence_at.store(sent_at, Ordering::Relaxed);
        // published after its send time is stamped, and with a release, so
        // an observer that sees this generation outstanding is guaranteed
        // the time it went out rather than whatever the slot held a lap
        // ago. `fetch_max` rather than a store keeps the generation
        // monotonic if this is ever reached concurrently, which is strictly
        // damage control and not concurrency support: two probers reading
        // the same generation would send two probes tagged alike, and one
        // answer would resolve both. What actually holds is the invariant
        // that there is one prober per connection -- `Engine::spawn` calls
        // `spawn_prober` exactly once and nothing else ticks a live
        // connection.
        self.probe
            .sent_generation
            .fetch_max(generation, Ordering::Release);
        // sent after the bump, never before: a generation handed to the
        // wire before it is on record could be answered before the observer
        // could tell it was ever owed
        handle.request_heartbeat(generation)
    }
}

/// Watches one connection's read side across successive observations.
///
/// The window is anchored on the send time of the oldest probe still owed
/// an answer, which is the only anchor that measures what the module doc
/// says this measures. Anchoring on the last probe *sent* could never
/// report a wedge at all, since probes go out on a fixed cadence regardless
/// of whether anything answers them. Anchoring on the last answer
/// *received* reports one, but forgives an engine that answers everything
/// far too late: each reply would restart the window while older questions
/// were still open.
///
/// Every field the two threads share lives behind atomics rather than a
/// lock, because the observing thread is asking whether the connection can
/// answer and so must never be able to wait on it. The `&mut self` on
/// [`record_ack`](Self::record_ack) and [`resume`](Self::resume) is an
/// exclusivity claim, not a borrow the implementation needs: both fold
/// acknowledgement state, and one owner doing that keeps the fold's
/// single-writer discipline checkable at compile time.
#[derive(Debug)]
pub struct HeartbeatWatch {
    threshold: Duration,
    probe: Arc<Probe>,
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
    ///
    /// Starts paused: [`resume`](Self::resume) arms it, and until then
    /// [`HeartbeatProber::tick`] issues nothing.
    #[must_use]
    pub fn new(threshold: Duration) -> Self {
        Self {
            threshold,
            probe: Arc::new(Probe::new()),
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
        if generation > self.probe.acked_generation.load(Ordering::Relaxed) {
            self.probe
                .acked_generation
                .store(generation, Ordering::Release);
        }
    }

    /// Folds the connection's read side into a verdict.
    ///
    /// Never blocks and never sends: two atomic loads and a comparison in
    /// the steady state, with no clock read and no allocation, so a paint
    /// loop can afford to ask on every pass -- which it must, since a
    /// wedged engine emits no redraws to wake anyone and this reports only
    /// about the moments it is called. The send lives on
    /// [`HeartbeatProber::tick`]; this call cannot violate "the paint loop
    /// never awaits RPC" whatever the connection is doing.
    ///
    /// `connection_closed` is the caller's reading of
    /// [`EngineHandle::is_closed`], passed in rather than taken here so
    /// this fold stays answerable without a connection at all.
    #[must_use]
    pub fn observe(&self, connection_closed: bool) -> Liveness {
        if !connection_closed && self.probe.oldest_outstanding().is_none() {
            return Liveness::Alive;
        }
        self.observe_at(connection_closed, Instant::now())
    }

    /// [`observe`](Self::observe) against an exact clock, so a caller can
    /// prove the predicate against an instant it chooses rather than
    /// against whatever the scheduler hands it.
    #[must_use]
    pub fn observe_at(&self, connection_closed: bool, now: Instant) -> Liveness {
        if connection_closed {
            // outranks every timing question below: a probe still waiting
            // on a connection that is gone is waiting on nothing, and the
            // patience the threshold buys is patience for an engine that
            // might yet answer
            return Liveness::Dead;
        }
        Self::verdict(self.probe.outstanding_for(now), self.threshold)
    }

    /// How long a caller may wait for its own next wakeup before this watch
    /// would have something new to say, or `None` when it would not.
    ///
    /// `None` means "wait as long as you like", and only a paused watch is
    /// ever told it: with the cadence stopped no probe is coming, so no
    /// silence can begin that a wakeup could report on.
    ///
    /// An armed watch always names a deadline, including while every probe
    /// it has issued is answered. The window it exists to detect opens
    /// between two passes of the caller's loop -- an engine wedges while
    /// nobody is typing, the next probe goes out into it, and nothing
    /// answers -- and a caller that slept unbounded on "nothing is
    /// outstanding right now" would sleep straight through it: a wedged
    /// engine emits no redraws, so the traffic that would otherwise wake
    /// the caller is exactly the traffic that stops.
    ///
    /// While a probe is outstanding the deadline times that probe's own
    /// silence. While none is, it is the same arithmetic against the probe
    /// that has not gone out yet: the next one is owed within one
    /// [`HEARTBEAT_PROBE_INTERVAL`] of whatever the cadence last did, and
    /// the earliest it could cross the threshold is one threshold after
    /// that. A healthy engine never reaches that instant, because its own
    /// answer wakes the caller first and every pass recomputes this from
    /// the tick that answer belongs to, so the wakeup is still paid for
    /// only by a connection that has actually gone quiet.
    ///
    /// Never zero, since a zero wait is a spin. Past the threshold the
    /// window has already said what it had to say, and its remaining job --
    /// noticing that the engine started answering again -- is served by
    /// looking again one threshold later.
    ///
    /// Costs one clock read and two relaxed loads on an idle pass with a
    /// live cadence. There is no answer to give without a clock: the
    /// question is when an instant in the future arrives, and a loop that
    /// declined to read one here would be declining to arm the deadline at
    /// all.
    #[must_use]
    pub fn poll_deadline(&self) -> Option<Duration> {
        self.deadline_with(Instant::now)
    }

    /// [`poll_deadline`](Self::poll_deadline) with the clock supplied as a
    /// source rather than a value, so a test can prove which questions reach
    /// for it.
    fn deadline_with(&self, clock: impl FnOnce() -> Instant) -> Option<Duration> {
        if self.probe.oldest_outstanding().is_none() {
            return self.prospective_deadline(clock);
        }
        Self::deadline(self.probe.outstanding_for(clock()), self.threshold)
    }

    /// How long the caller may wait while no probe is outstanding at all.
    ///
    /// The cadence's own anchor stands in for the send time the window
    /// would otherwise be measured from: one interval past it is the latest
    /// the next probe can go out, and a wedge starting the moment this is
    /// called cannot be provable before that probe has then gone unanswered
    /// for a threshold. Anchoring on `now` instead would arm the same
    /// deadline against a cadence that already ticked, and the pass that
    /// woke would find the probe younger than it expected.
    fn prospective_deadline(&self, clock: impl FnOnce() -> Instant) -> Option<Duration> {
        if self.probe.paused.load(Ordering::Acquire) {
            return None;
        }
        let now = Duration::from_nanos(self.probe.stamp(clock()));
        let owed = Duration::from_nanos(self.probe.cadence_at.load(Ordering::Relaxed))
            + HEARTBEAT_PROBE_INTERVAL;
        let overdue = now.saturating_sub(owed);
        if overdue.is_zero() {
            return Some(owed.saturating_sub(now) + self.threshold);
        }
        // the anchor is older than the cadence should ever leave it, which
        // is a prober that has retired (its connection refused a tick)
        // rather than one running late. Same arithmetic as an outstanding
        // probe past its threshold: look again one threshold later, never
        // sooner and never in a spin
        Self::deadline(Outstanding::For(overdue), self.threshold)
    }

    /// Stops [`HeartbeatProber::tick`] from issuing any *new* probe.
    ///
    /// Does not guarantee that no reply arrives afterwards: probes already
    /// dispatched by prior ticks are still in flight and the peer still
    /// answers them. Never rewinds the sent generation.
    pub fn pause(&self) {
        self.probe.paused.store(true, Ordering::Relaxed);
    }

    /// Arms the prober, forgiving every probe still outstanding.
    ///
    /// A pause is the observer's silence, never the engine's: time in which
    /// nobody was folding answers cannot be charged to the side that may
    /// well have been giving them. Forgiveness is what re-anchors the
    /// window here, since the window is anchored on outstanding probes and
    /// after this call there are none.
    ///
    /// Deliberately leaves the sent generation exactly where
    /// [`pause`](Self::pause) found it, so the pair stays monotonic-forward
    /// across the cycle: a late reply to a probe sent before the pause
    /// raises the acknowledged generation to at most its pre-pause value,
    /// strictly below any generation a post-resume tick produces, so it can
    /// never satisfy a later wedge check and mask a genuine hang.
    pub fn resume(&mut self) {
        self.resume_at(Instant::now());
    }

    /// [`resume`](Self::resume) against an exact clock.
    fn resume_at(&mut self, now: Instant) {
        let sent = self.probe.sent_generation.load(Ordering::Acquire);
        self.probe.acked_generation.store(sent, Ordering::Relaxed);
        // the arming is itself a cadence event, and the only one there is
        // until the first tick lands: a probe is owed one interval from
        // here, so the window before that tick is dated rather than
        // unbounded
        self.probe
            .cadence_at
            .store(self.probe.stamp(now), Ordering::Relaxed);
        // released after the forgiveness above, so the first tick to see
        // this connection armed also sees the acknowledgement that came
        // with the arming rather than a run it should not reopen
        self.probe.paused.store(false, Ordering::Release);
    }

    /// What one anchor reading says about the engine.
    fn verdict(silence: Outstanding, threshold: Duration) -> Liveness {
        match silence {
            // nothing is owed an answer, so nothing is being withheld
            // however long the connection has been quiet
            Outstanding::Nothing => Liveness::Alive,
            Outstanding::For(silence) if silence >= threshold => Liveness::Wedged,
            Outstanding::For(_) => Liveness::Alive,
            // a verdict is a statement about the engine and there is no
            // reading behind this one to make a statement from. A real
            // wedge underneath it keeps its latency: the deadline the same
            // reading arms is shorter than any wakeup the caller was
            // otherwise waiting on, so the settled answer lands within
            // `UNSETTLED_RETRY` of the pass that raced
            Outstanding::Unsettled => Liveness::Alive,
        }
    }

    /// How long the caller of that same reading may wait before asking again.
    fn deadline(silence: Outstanding, threshold: Duration) -> Option<Duration> {
        let silence = match silence {
            Outstanding::Nothing => return None,
            Outstanding::For(silence) => silence,
            // the one reading that must not be slept on: the caller was
            // told `Alive` by a reading that measured nothing, so the
            // re-read owed to it is the whole content of this deadline
            Outstanding::Unsettled => return Some(UNSETTLED_RETRY),
        };
        let remaining = threshold.saturating_sub(silence);
        Some(if remaining.is_zero() {
            threshold
        } else {
            remaining
        })
    }

    /// [`poll_deadline`](Self::poll_deadline) against an exact clock.
    #[cfg(test)]
    fn deadline_at(&self, now: Instant) -> Option<Duration> {
        self.deadline_with(|| now)
    }
}

/// Which wedge, if any, the two watches over one connection see between
/// them: the write side's own backlog reading ([`crate::stall`]), this
/// module's read-side verdict, and the caller's resolution of whether a
/// stop was a death.
///
/// A closed connection outranks every timing question below it, the same
/// ordering [`HeartbeatWatch::observe_at`] applies for the same reason: a
/// probe still waiting on a connection that is gone is waiting on nothing.
///
/// Between the two open-connection failures the write side's verdict wins,
/// because it is the one that can be the root cause of the other. Every
/// probe the read side is waiting on left through the same outbox, so a
/// writer that has stopped delivering is enough on its own to make the read
/// side report a wedge -- while a writer that is still delivering rules the
/// write side out entirely, whatever the read side says. Taking the read
/// side first would report the consequence and hide the cause, and it would
/// do so on the overwhelming majority of real stalls, since the two
/// thresholds are equal and both sides cross them together.
///
/// `lost` is the caller's own resolution of the stop, never the
/// connection's closed flag on its own: an engine that closed because its
/// user typed `:q` reaches this classification looking exactly like one
/// that crashed, and supervising that would offer -- or, unattended,
/// perform -- a restart of the editor they just closed. Only a resolved
/// exit status can tell the two apart
/// ([`SupervisionState::note_engine_stop`](view_core::native::supervision::SupervisionState::note_engine_stop)),
/// so a closed connection with `lost` still unresolved yields no wedge at
/// all rather than pre-empting the facts that resolution carries.
///
/// Lives here rather than beside the loop that folds it so the oracle's
/// reproduced hang schedules classify a verdict through the same function
/// production does: a second copy would let the two agree with each other
/// while both drifted from the runtime.
#[must_use]
pub fn wedge_kind(write_stalled: bool, read: Liveness, lost: bool) -> Option<WedgeKind> {
    match read {
        Liveness::Dead if lost => Some(WedgeKind::Dead),
        // closed, and not yet resolved: the reader's own `Msg::EngineStopped`
        // is already on its way (sent with a blocking send, so it cannot be
        // dropped), and it carries the facts this verdict must not pre-empt
        Liveness::Dead => None,
        _ if write_stalled => Some(WedgeKind::WriteSide),
        Liveness::Wedged => Some(WedgeKind::ReadSide),
        // `Liveness` is `#[non_exhaustive]`, so this arm also catches a
        // verdict a later engine build might add: with the write side moving
        // and no verdict this build understands, there is nothing to report
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const THRESHOLD: Duration = Duration::from_secs(10);
    /// The shipped cadence itself rather than a copy of its current value:
    /// the timelines below drive the interval production runs at, and the
    /// prospective deadline is built from this exact constant, so a test
    /// asserting against a second spelling of it would agree with the
    /// deadline only until one of the two moved.
    const INTERVAL: Duration = HEARTBEAT_PROBE_INTERVAL;

    /// An armed watch and its prober over a connection whose peer neither
    /// answers nor hangs up, so a probe genuinely goes unanswered rather
    /// than failing to send. The returned guards keep that connection open
    /// for the test's duration.
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
        let mut watch = HeartbeatWatch::new(THRESHOLD);
        // armed at the origin every timeline below is expressed against, so
        // the cadence anchor a deadline is dated from is `t0` exactly
        // rather than however long the spawn above took
        let origin = watch.probe.origin;
        watch.resume_at(origin);
        let prober = watch.prober();
        (watch, prober, handle, (keep_open, notifs))
    }

    /// An armed watch over a connection whose peer is already gone, with
    /// the reader thread's close-and-drain proven to have run.
    fn lost_connection() -> (HeartbeatWatch, HeartbeatProber, EngineHandle) {
        let (peer_read, our_write) = std::io::pipe().unwrap();
        let (our_read, peer_write) = std::io::pipe().unwrap();
        let pump = crate::damage::PumpShared::new();
        let handle = EngineHandle::start_pumped(
            our_read,
            our_write,
            Arc::clone(&pump),
            #[cfg(any(unix, windows))]
            None,
        );
        drop(peer_write);
        drop(peer_read);
        let err = handle.request("nvim_get_mode", vec![]).unwrap_err();
        assert!(matches!(err, EngineError::Closed), "{err:?}");
        let mut watch = HeartbeatWatch::new(THRESHOLD);
        let origin = watch.probe.origin;
        watch.resume_at(origin);
        let prober = watch.prober();
        (watch, prober, handle)
    }

    #[test]
    fn a_probe_no_one_answers_reads_as_wedged_once_past_the_threshold() {
        let (watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.probe.origin;
        prober.tick_at(&handle, t0).unwrap();
        assert_eq!(watch.observe_at(false, t0), Liveness::Alive);
        assert_eq!(watch.observe_at(false, t0 + THRESHOLD / 2), Liveness::Alive);
        assert_eq!(watch.observe_at(false, t0 + THRESHOLD), Liveness::Wedged);
    }

    #[test]
    fn a_connection_no_one_has_probed_never_reads_as_wedged() {
        let (watch, _prober, _handle, _guards) = watched_connection();
        let t0 = watch.probe.origin;
        assert_eq!(watch.observe_at(false, t0), Liveness::Alive);
        assert_eq!(
            watch.observe_at(false, t0 + THRESHOLD * 100),
            Liveness::Alive
        );
    }

    #[test]
    fn an_engine_answering_between_probes_never_reads_as_wedged() {
        let (mut watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.probe.origin;
        for n in 1..=20 {
            let now = t0 + THRESHOLD / 2 * n;
            prober.tick_at(&handle, now).unwrap();
            assert_eq!(
                watch.observe_at(false, now),
                Liveness::Alive,
                "answer {n} read as a wedge"
            );
            watch.record_ack(u64::from(n));
        }
    }

    /// The anchor tracks the oldest question still open, not the newest
    /// answer: an engine four probes behind but answering each one inside
    /// the threshold is slow, not wedged, however long the backlog runs.
    #[test]
    fn an_engine_answering_inside_the_threshold_stays_alive_under_a_standing_backlog() {
        let (mut watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.probe.origin;
        // four intervals of lag: every probe answered, none of them late
        let lag = 4;
        for n in 1..=40u32 {
            let now = t0 + INTERVAL * n;
            prober.tick_at(&handle, now).unwrap();
            if let Some(answered) = n.checked_sub(lag).filter(|a| *a >= 1) {
                watch.record_ack(u64::from(answered));
            }
            assert_eq!(
                watch.observe_at(false, now),
                Liveness::Alive,
                "probe {n} read as a wedge against an engine answering every one of them \
                 in {}s",
                (INTERVAL * lag).as_secs()
            );
        }
    }

    /// The failure the answer-anchored window forgives and this one does
    /// not: an engine answering every probe thirty seconds late is not a
    /// working editor, however steadily the answers arrive.
    #[test]
    fn an_engine_answering_every_probe_far_too_late_never_reads_as_alive_again() {
        let (mut watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.probe.origin;
        // fifteen intervals of lag: three times the threshold
        let lag = 15;
        for n in 1..=40u32 {
            let now = t0 + INTERVAL * n;
            prober.tick_at(&handle, now).unwrap();
            if let Some(answered) = n.checked_sub(lag).filter(|a| *a >= 1) {
                watch.record_ack(u64::from(answered));
            }
            if now >= t0 + INTERVAL + THRESHOLD {
                assert_eq!(
                    watch.observe_at(false, now),
                    Liveness::Wedged,
                    "probe {n} read as alive against an engine {}s behind",
                    (INTERVAL * lag).as_secs()
                );
            }
        }
    }

    #[test]
    fn the_wedge_clears_as_soon_as_one_probe_is_answered() {
        let (mut watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.probe.origin;
        prober.tick_at(&handle, t0).unwrap();
        assert_eq!(watch.observe_at(false, t0 + THRESHOLD), Liveness::Wedged);
        watch.record_ack(1);
        assert_eq!(watch.observe_at(false, t0 + THRESHOLD), Liveness::Alive);
    }

    /// The whole reason the probe carries a generation: an answer that
    /// arrives after a newer one must not rewind what has been answered,
    /// or the newer probe would be owed an answer it already received and
    /// the connection would time itself out while healthy.
    #[test]
    fn a_reply_arriving_out_of_order_never_rewinds_what_has_been_answered() {
        let (mut watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.probe.origin;
        prober.tick_at(&handle, t0).unwrap();
        prober.tick_at(&handle, t0).unwrap();
        watch.record_ack(2);
        watch.record_ack(1);
        assert_eq!(
            watch.observe_at(false, t0 + THRESHOLD * 10),
            Liveness::Alive
        );
    }

    /// The second silence is timed from the probe that opened it, not from
    /// the answer that ended the first: the two windows are separate
    /// questions and the gap between them belongs to neither.
    #[test]
    fn a_second_silence_is_timed_from_the_probe_that_opened_it() {
        let (mut watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.probe.origin;
        prober.tick_at(&handle, t0).unwrap();
        assert_eq!(watch.observe_at(false, t0 + THRESHOLD), Liveness::Wedged);
        let recovered = t0 + THRESHOLD;
        watch.record_ack(1);
        prober.tick_at(&handle, recovered).unwrap();
        assert_eq!(
            watch.observe_at(false, recovered + THRESHOLD - Duration::from_millis(1)),
            Liveness::Alive
        );
        assert_eq!(
            watch.observe_at(false, recovered + THRESHOLD),
            Liveness::Wedged
        );
    }

    /// An answered connection is not one that can be slept on: the probe
    /// that has not gone out yet is the one a wedge starting now would go
    /// unanswered, so the wakeup an armed watch asks for is dated from the
    /// cadence rather than declined.
    #[test]
    fn an_answered_connection_asks_to_look_again_when_the_next_probe_could_be_late() {
        let (mut watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.probe.origin;
        assert_eq!(watch.deadline_at(t0), Some(INTERVAL + THRESHOLD));
        prober.tick_at(&handle, t0).unwrap();
        assert_eq!(watch.deadline_at(t0), Some(THRESHOLD));
        assert_eq!(
            watch.deadline_at(t0 + THRESHOLD / 4),
            Some(THRESHOLD / 4 * 3)
        );
        watch.record_ack(1);
        // the window that answer closed is replaced by the next one, dated
        // from the tick rather than from the answer: the cadence is what
        // decides when the engine is next asked anything
        assert_eq!(watch.deadline_at(t0), Some(INTERVAL + THRESHOLD));
        assert_eq!(
            watch.deadline_at(t0 + INTERVAL / 2),
            Some(INTERVAL / 2 + THRESHOLD)
        );
    }

    /// The one state with genuinely nothing to say: no probe is coming, so
    /// no silence can begin, so no wakeup could report on one.
    #[test]
    fn a_paused_watch_asks_for_no_wakeup_at_all() {
        let (mut watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.probe.origin;
        prober.tick_at(&handle, t0).unwrap();
        watch.pause();
        // a pause is not forgiveness: the probe already on the wire is
        // still owed an answer, and the window timing it is unaffected
        assert_eq!(watch.deadline_at(t0), Some(THRESHOLD));
        watch.record_ack(1);
        assert_eq!(watch.deadline_at(t0), None);
    }

    /// The window before the first tick: a watch armed and then left alone
    /// is owed a probe one interval later, and dates its deadline from the
    /// arming because there is no send time yet to date it from.
    #[test]
    fn arming_anchors_the_window_before_the_first_tick_lands() {
        let (mut watch, _prober, _handle, _guards) = watched_connection();
        let armed = watch.probe.origin + THRESHOLD * 3;
        watch.resume_at(armed);
        assert_eq!(watch.deadline_at(armed), Some(INTERVAL + THRESHOLD));
        assert_eq!(watch.deadline_at(armed + INTERVAL), Some(THRESHOLD));
    }

    /// A cadence that stopped without the watch being paused -- a prober
    /// retired by a connection that refused its tick -- leaves an anchor
    /// older than any live cadence produces. The answer is the same one a
    /// crossed window gives, and never a spin.
    #[test]
    fn a_cadence_that_stopped_keeps_asking_rather_than_spinning() {
        let (watch, _prober, _handle, _guards) = watched_connection();
        let t0 = watch.probe.origin;
        for (late, expected) in [
            (INTERVAL + THRESHOLD / 2, THRESHOLD / 2),
            (INTERVAL + THRESHOLD, THRESHOLD),
            (THRESHOLD * 100, THRESHOLD),
        ] {
            let asked = watch.deadline_at(t0 + late);
            assert_eq!(asked, Some(expected), "{late:?} after the anchor");
            assert!(
                asked.is_some_and(|wait| !wait.is_zero()),
                "a zero wait is a spin"
            );
        }
    }

    /// A loop asks this every pass, so what it costs is a shipped number. A
    /// paused watch answers from the prober's own memory; an armed one pays
    /// one reading whether or not a probe is outstanding, because either
    /// answer is an instant in the future and no clock means no deadline.
    #[test]
    fn an_armed_watch_pays_one_clock_read_a_pass_and_a_paused_one_pays_none() {
        let (mut watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.probe.origin;
        let reads = std::cell::Cell::new(0_u32);
        let clock = |at: Instant| {
            let reads = &reads;
            move || {
                reads.set(reads.get() + 1);
                at
            }
        };

        watch.pause();
        assert_eq!(watch.deadline_with(clock(t0)), None);
        assert_eq!(
            reads.get(),
            0,
            "a paused watch paid for a reading its answer cannot use"
        );

        watch.resume_at(t0);
        assert_eq!(watch.deadline_with(clock(t0)), Some(INTERVAL + THRESHOLD));
        assert_eq!(
            reads.get(),
            1,
            "an armed watch must date the probe it is still owed"
        );

        prober.tick_at(&handle, t0).unwrap();
        assert_eq!(watch.deadline_with(clock(t0)), Some(THRESHOLD));
        assert_eq!(
            reads.get(),
            2,
            "an outstanding probe must time its own window"
        );
    }

    /// Never zero once the verdict has flipped: a zero wait is a spin, and
    /// the window's remaining job -- noticing the engine answering again --
    /// is served by looking one threshold later.
    #[test]
    fn a_confirmed_wedge_keeps_asking_so_the_recovery_is_seen_without_a_keystroke() {
        let (watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.probe.origin;
        prober.tick_at(&handle, t0).unwrap();
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
        let t0 = watch.probe.origin;
        // through the public seam, which is the caller's only reading of a
        // connection that closed with nothing owed: the timing path below
        // would report `Alive` for exactly this state
        assert_eq!(watch.observe(true), Liveness::Dead);
        assert_eq!(watch.observe(false), Liveness::Alive);
        assert_eq!(watch.observe_at(true, t0), Liveness::Dead);
        prober.tick_at(&handle, t0).unwrap();
        assert_eq!(watch.observe_at(true, t0), Liveness::Dead);
        watch.record_ack(1);
        assert_eq!(watch.observe_at(true, t0), Liveness::Dead);
        assert_eq!(watch.observe_at(true, t0 + THRESHOLD * 10), Liveness::Dead);
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
        watch.resume();
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
            watch.observe_at(false, watch.probe.origin + THRESHOLD * 10),
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
        watch.resume();
        prober.tick(&handle).unwrap();
        assert_eq!(watch.probe.sent_generation.load(Ordering::Relaxed), 2);
    }

    /// The paused prober still answers the one question its caller retires
    /// on. A thread that ticks until the connection refuses would otherwise
    /// outlive every connection that died inside a pause, holding an
    /// engine handle clone for the process's lifetime.
    #[test]
    fn a_paused_prober_still_refuses_a_connection_that_is_gone() {
        let (watch, prober, handle) = lost_connection();
        watch.pause();
        let err = prober.tick(&handle).unwrap_err();
        assert!(matches!(err, EngineError::Closed), "{err:?}");
        // and no probe was issued on the way to that refusal
        assert_eq!(watch.probe.sent_generation.load(Ordering::Relaxed), 0);
    }

    /// Both halves together, because either alone proves the wrong thing: a
    /// fold that swallowed the late reply would pass the first assertion by
    /// being blind, and one that let it stand in for a post-resume answer
    /// would pass nothing after it. The pair is what shows the reply is
    /// harmless *and* that a genuine hang right after it is still caught.
    #[test]
    fn a_reply_landing_after_a_resume_is_harmless_without_masking_the_next_hang() {
        let (mut watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.probe.origin;
        prober.tick_at(&handle, t0).unwrap();
        watch.pause();
        let resumed = t0 + THRESHOLD * 3;
        watch.resume();

        // the pre-pause probe's reply, arriving only now
        watch.record_ack(1);
        assert_eq!(watch.observe_at(false, resumed), Liveness::Alive);

        // and from that same point, a probe nothing answers
        prober.tick_at(&handle, resumed).unwrap();
        assert_eq!(
            watch.observe_at(false, resumed + THRESHOLD - Duration::from_millis(1)),
            Liveness::Alive
        );
        assert_eq!(
            watch.observe_at(false, resumed + THRESHOLD),
            Liveness::Wedged
        );
    }

    /// The pause is not counted against the engine: time spent with the
    /// prober deliberately silenced is time nobody asked it anything.
    #[test]
    fn a_pause_longer_than_the_threshold_is_not_charged_to_the_engine() {
        let (mut watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.probe.origin;
        prober.tick_at(&handle, t0).unwrap();
        watch.pause();
        let resumed = t0 + THRESHOLD * 10;
        watch.resume();
        assert_eq!(watch.observe_at(false, resumed), Liveness::Alive);
    }

    /// A silence outliving the send log still reads as one: the fallback
    /// anchor is the oldest send the log still holds, which is a real probe
    /// and a real send time, so the verdict stays a statement about the
    /// engine rather than an index into an overwritten slot.
    #[test]
    fn a_silence_outliving_the_send_log_still_reads_as_a_wedge() {
        let (watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.probe.origin;
        let probes = u32::try_from(SEND_LOG).unwrap() * 2;
        for n in 1..=probes {
            prober.tick_at(&handle, t0 + INTERVAL * n).unwrap();
        }
        let last = t0 + INTERVAL * probes;
        assert_eq!(watch.observe_at(false, last), Liveness::Wedged);
        // the oldest generation still on record, not the oldest ever sent
        let oldest = watch.probe.oldest_outstanding().unwrap();
        assert_eq!(oldest, u64::from(probes) - SEND_LOG + 1);
    }

    /// The one interleaving that can invert a verdict, recreated exactly: an
    /// observer reads the generation pair, a tick lands, and the slot the
    /// observer was about to read is the slot that tick just overwrote --
    /// they alias whenever the window is saturated, because the two
    /// generations differ by exactly `SEND_LOG`. Reading it anyway would
    /// hand an engine silent for two minutes the send time of a probe
    /// issued this instant.
    #[test]
    fn a_tick_landing_mid_read_invalidates_the_anchor_it_would_have_overwritten() {
        let (watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.probe.origin;
        let saturating = u32::try_from(SEND_LOG).unwrap();
        for n in 1..=saturating {
            prober.tick_at(&handle, t0 + INTERVAL * n).unwrap();
        }

        // the pair an observer reads before it goes looking in the log
        let sent = watch.probe.sent_generation.load(Ordering::Relaxed);
        let acked = watch.probe.acked_generation.load(Ordering::Relaxed);
        let anchor = Probe::anchor_generation(sent, acked);
        assert_eq!(anchor, sent - (SEND_LOG - 1));
        assert_eq!(
            Probe::slot(sent + 1),
            Probe::slot(anchor),
            "the next tick has to alias the anchor's slot or this test pins nothing"
        );
        let anchored_at = watch.probe.anchor_nanos(sent, acked);
        assert_eq!(
            anchored_at,
            Some(u64::try_from(INTERVAL.as_nanos()).unwrap()),
            "an unraced read must still yield the anchor's own send time"
        );

        // and now the tick, landing between that pair and the stamp read
        prober
            .tick_at(&handle, t0 + INTERVAL * (saturating + 1))
            .unwrap();
        assert_eq!(
            watch.probe.anchor_nanos(sent, acked),
            None,
            "a stamp read across a tick was accepted: a saturated window would \
             report the new probe's send time as the old one's and read Alive"
        );
    }

    /// The retry is what makes the invalidation above harmless: a fold that
    /// re-reads finds a settled pair and the verdict stands.
    #[test]
    fn a_saturated_window_reads_wedged_through_the_tick_that_aliases_its_anchor() {
        let (watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.probe.origin;
        let probes = u32::try_from(SEND_LOG).unwrap();
        for n in 1..=probes {
            prober.tick_at(&handle, t0 + INTERVAL * n).unwrap();
        }
        let landed = t0 + INTERVAL * (probes + 1);
        prober.tick_at(&handle, landed).unwrap();
        assert_eq!(watch.observe_at(false, landed), Liveness::Wedged);
        assert_eq!(watch.deadline_at(landed), Some(THRESHOLD));
    }

    /// The raced reading's whole contract, stated where it can be stated
    /// exactly: it says nothing about the engine, and it says the caller
    /// owes another look almost immediately. The pair matters together --
    /// a verdict of `Alive` on its own would postpone a real wedge's
    /// notice by whatever the caller was otherwise going to sleep for.
    #[test]
    fn a_raced_reading_raises_no_verdict_and_asks_to_be_read_again_at_once() {
        assert_eq!(
            HeartbeatWatch::verdict(Outstanding::Unsettled, THRESHOLD),
            Liveness::Alive,
            "a reading that measured nothing was allowed to accuse the engine"
        );
        assert_eq!(
            HeartbeatWatch::deadline(Outstanding::Unsettled, THRESHOLD),
            Some(UNSETTLED_RETRY)
        );
        assert!(
            UNSETTLED_RETRY < THRESHOLD,
            "the re-read must land far inside the window it is standing in for"
        );
        // and the settled readings are untouched by the split
        assert_eq!(
            HeartbeatWatch::verdict(Outstanding::Nothing, THRESHOLD),
            Liveness::Alive
        );
        assert_eq!(
            HeartbeatWatch::deadline(Outstanding::Nothing, THRESHOLD),
            None
        );
        assert_eq!(
            HeartbeatWatch::verdict(Outstanding::For(THRESHOLD), THRESHOLD),
            Liveness::Wedged
        );
    }

    /// The same thing through the seam a runtime loop actually calls,
    /// against the interleaving that produces it: a prober advancing
    /// generations as fast as it can while an observer folds verdicts at an
    /// instant no settled reading could call wedged. Every `Wedged` here is
    /// a raced reading escaping as a verdict, and one is enough to flash a
    /// notice at a user whose engine is fine.
    #[test]
    fn an_observer_racing_the_prober_never_reads_a_verdict_the_clock_denies() {
        let (watch, prober, handle, _guards) = watched_connection();
        let t0 = watch.probe.origin;
        let ticks = 4_000_u32;
        let ticker = std::thread::spawn(move || {
            for n in 1..=ticks {
                // stamped inside the window on purpose: no settled reading
                // taken at `t0 + INTERVAL` can be `Wedged`, so the verdict
                // under test has only the race to come from
                prober.tick_at(&handle, t0).unwrap();
                std::hint::black_box(n);
            }
        });
        let mut observations = 0_u32;
        while !ticker.is_finished() {
            assert_eq!(
                watch.observe_at(false, t0 + INTERVAL),
                Liveness::Alive,
                "a reading that raced the prober was reported as a wedge"
            );
            observations += 1;
        }
        ticker.join().unwrap();
        assert!(observations > 0, "the observer never got to run");
    }

    #[test]
    fn the_default_threshold_is_the_documented_one() {
        assert_eq!(
            HeartbeatWatch::default().threshold,
            HEARTBEAT_WEDGE_THRESHOLD
        );
    }

    /// A fresh watch probes nothing until its owner arms it, so probes
    /// answered before anyone was folding replies are never charged to the
    /// engine (see [`HeartbeatWatch::resume`]).
    #[test]
    fn a_watch_nobody_has_armed_issues_no_probe() {
        let (source, _keep_open) = crate::test_peer::IdleSource::new();
        let (handle, _notifs) = EngineHandle::start(source, std::io::sink());
        let watch = HeartbeatWatch::new(THRESHOLD);
        let prober = watch.prober();
        for _ in 0..5 {
            prober.tick(&handle).unwrap();
        }
        assert_eq!(watch.probe.sent_generation.load(Ordering::Relaxed), 0);
        assert_eq!(watch.observe(false), Liveness::Alive);
    }

    /// The write side's precedence is a precedence, not a preference: a
    /// writer that is demonstrably moving cannot be the wedge, so the read
    /// side's verdict stands on its own.
    #[test]
    fn a_moving_writer_leaves_the_read_sides_verdict_alone() {
        assert_eq!(
            wedge_kind(false, Liveness::Wedged, false),
            Some(WedgeKind::ReadSide)
        );
        assert_eq!(
            wedge_kind(true, Liveness::Alive, false),
            Some(WedgeKind::WriteSide)
        );
        assert_eq!(wedge_kind(false, Liveness::Alive, false), None);
        // and a closed connection whose stop the caller has resolved
        // outranks both, since neither side can recover one
        assert_eq!(
            wedge_kind(true, Liveness::Dead, true),
            Some(WedgeKind::Dead)
        );
    }

    /// The verdict that would respawn an editor its user just closed. A
    /// connection closing is not evidence of anything on its own: `:q` and a
    /// crash both close it, and only a resolved exit status tells them
    /// apart. Until that resolution lands, this reports nothing at all
    /// rather than guessing.
    #[test]
    fn a_closed_connection_is_no_verdict_until_its_stop_has_been_resolved() {
        assert_eq!(
            wedge_kind(false, Liveness::Dead, false),
            None,
            "an unresolved stop must not surface as a wedge to recover from"
        );
        assert_eq!(
            wedge_kind(true, Liveness::Dead, false),
            None,
            "not even with output stranded in the outbox: the write side \
             cannot outrank a connection whose stop is still being resolved"
        );
    }
}
