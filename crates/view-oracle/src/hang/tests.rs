//! The falsifiable half of the reproduced hang schedules.
//!
//! Every assertion here is bounded, and the bound is a shipped constant
//! rather than a number chosen to make a test pass: a schedule that stopped
//! being detected, or started being detected late, fails on the same
//! arithmetic the affordance's own contract is written in.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use view_core::model::OverlayKind;
use view_core::msg::{Effect, Msg};
use view_core::native::supervision::{
    SinceStamp, WedgeKind, ENGINE_BUSY_MODAL_THRESHOLD, RESTART_NOTATION,
};
use view_core::update::update;
use view_engine::heartbeat::{Liveness, HEARTBEAT_PROBE_INTERVAL};
use view_engine::process::EngineConfig;

use super::{
    detection_deadline, observation_slack, restart_bound, run_schedule, slack_scale, HangRun,
    HangSchedule, HangSession, DETECTION_BOUND, FOLD_INTERVAL, SHUTDOWN, WEDGE_LOOP,
};
use crate::{snapshot, OracleError, Probe};

/// How many folds a run must have completed for its survival claim to mean
/// anything. A harness that hung alongside the engine would report a
/// handful; a healthy one at the fold cadence reports orders of magnitude
/// more over any of the bounds below.
const SURVIVAL_FOLDS: u64 = 100;

/// A session over an isolated engine, spawned the way every schedule here
/// spawns one.
fn session() -> HangSession {
    HangSession::spawn(EngineConfig::isolated().with_shutdown_timeout(SHUTDOWN))
        .expect("a pinned nvim must spawn")
}

/// A `Probe` view of a session, so the shared state snapshot the
/// differential oracle compares with can be taken of one.
struct SessionProbe<'a>(&'a HangSession);

impl Probe for SessionProbe<'_> {
    fn eval_str(&mut self, expr: &str) -> Result<String, OracleError> {
        self.0.engine().handle.eval_str(expr).map_err(Into::into)
    }

    fn get_mode(&mut self) -> Result<(String, bool), OracleError> {
        self.0.engine().handle.get_mode().map_err(Into::into)
    }

    fn input(&mut self, notation: &str) -> Result<(), OracleError> {
        self.0.engine().handle.input(notation).map_err(Into::into)
    }
}

// -- the wire fact both other schedules rest on -------------------------

/// The one property that makes a wedge distinguishable from a busy editor
/// at all: nvim answers `nvim_get_mode` on receipt even while its main loop
/// is blocked waiting for a key, so an engine deliberately parked in a
/// key-wait keeps reading `Alive` while one hung inside a synchronous call
/// does not.
///
/// Re-verified live rather than carried forward as documentation, because
/// every other assertion in this file is vacuous if it stops holding: a
/// pinned engine that deferred the probe in a key-wait would make the
/// heartbeat report a wedge for every `r`, `f` or hit-enter prompt a user
/// ever sits at.
#[test]
fn nvim_answers_the_liveness_probe_while_blocked_waiting_for_a_key() {
    let mut session = session();
    let fired = session.fire(HangSchedule::BlockedOnKey).unwrap();

    // nvim's own statement that its main loop is blocked, delivered by the
    // very call a blocked main loop is supposed to be unable to serve
    let mut blocked_reports = 0_u32;
    let until = Instant::now() + detection_deadline();
    while Instant::now() < until {
        let (_, blocking) = session
            .engine()
            .handle
            .get_mode()
            .expect("the pinned engine must answer nvim_get_mode while blocked on a key");
        if blocking {
            blocked_reports += 1;
        }
        assert_eq!(
            session.fold(),
            None,
            "a key-wait was classified as a wedge after {:?}",
            fired.elapsed()
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(
        blocked_reports > 0,
        "the engine never reported itself blocked, so this proves nothing about \
         a probe answered during a block"
    );
    assert!(
        session.model().overlays().is_empty(),
        "a key-wait raised the engine-busy modal"
    );
    assert!(
        session.banner().is_none(),
        "a key-wait raised the sticky banner: {:?}",
        session.banner()
    );
}

/// The other half of the same fact, and the reason the read-side schedule
/// is detectable: a synchronous Lua loop pumps nothing, so the fast probe
/// goes unanswered there where a key-wait answers it.
#[test]
fn a_synchronous_lua_loop_leaves_the_liveness_probe_unanswered() {
    let mut session = session();
    let fired = session.fire(HangSchedule::ReadSideWedge).unwrap();
    let bound = detection_deadline();
    let deadline = fired + bound;
    while session.engine().handle.get_mode().is_ok() {
        assert!(
            Instant::now() < deadline,
            "the engine kept answering for {bound:?} after being told to run a \
             {WEDGE_LOOP:?} loop, so it never entered it"
        );
    }
}

/// The wedge loop really does end on a clock, with nothing killing it.
///
/// The bound is the module's own safety net, and a net nobody ever drops a
/// weight into is a decoration: a harness killed outright runs no teardown,
/// so a loop that could not end itself would leave an nvim spinning a core
/// on the host until reboot. It is also easy to write one that cannot --
/// `vim.uv.now` reports libuv's loop-cached time, which does not advance
/// inside a loop that reaches no loop iteration, so a budget written
/// against it never expires -- and nothing else here would notice, since
/// every other schedule kills its engine anyway.
#[test]
fn a_bounded_wedge_loop_ends_on_its_own_with_nothing_killing_it() {
    let mut session = session();
    // longer than the engine's own reply timeout, so the call that proves
    // the engine wedged has time to time out inside the budget rather than
    // straddling its end
    let budget = HEARTBEAT_PROBE_INTERVAL * 3;
    let fired = session.fire_bounded_wedge(budget).unwrap();

    while session.engine().handle.get_mode().is_ok() {
        assert!(
            fired.elapsed() < budget,
            "the engine never entered the loop, so its ending proves nothing"
        );
    }

    while fired.elapsed() < budget + observation_slack() {
        let _ = session.fold();
        std::thread::sleep(FOLD_INTERVAL);
    }
    session.engine().handle.get_mode().unwrap_or_else(|err| {
        panic!(
            "a loop given a {budget:?} budget was still running {:?} after it \
             fired ({err}), so its bound counts a clock that does not advance \
             inside it",
            fired.elapsed()
        )
    });
}

// -- the two hang schedules ---------------------------------------------

/// A read-side wedge reaches `Wedged` inside the shipped bound, and the
/// harness is demonstrably still folding while it does.
#[test]
fn a_read_side_wedge_is_reported_wedged_within_the_probe_and_threshold_bound() {
    let report = run_schedule(HangRun::new(HangSchedule::ReadSideWedge)).unwrap();

    let deadline = detection_deadline();
    let detected = report.detected_after.unwrap_or_else(|| {
        panic!(
            "a synchronous Lua loop was never reported as a wedge within {deadline:?}: {report:?}"
        )
    });
    assert!(
        detected <= deadline,
        "the wedge took {detected:?} to notice, past the {DETECTION_BOUND:?} the \
         probe interval and wedge threshold allow (plus what a poller costs to \
         read it)"
    );
    assert_eq!(report.wedge, Some(WedgeKind::ReadSide));
    assert_eq!(report.banner.as_deref(), Some(WedgeKind::ReadSide.notice()));
    assert!(
        report.folds >= SURVIVAL_FOLDS,
        "the observing side completed only {} folds while the engine was wedged, \
         so it did not survive the wedge it was watching",
        report.folds
    );
    assert!(
        report.restarted_after.is_none(),
        "a read-side wedge may resolve itself; nothing may restart the engine \
         behind the user's back for one"
    );
}

/// The banner comes first and the modal only after the patience a
/// possibly-self-resolving condition is worth.
#[test]
fn a_read_side_wedge_escalates_to_the_modal_only_past_the_modal_threshold() {
    let report = run_schedule(HangRun::new(HangSchedule::ReadSideWedge).escalating()).unwrap();

    assert_eq!(report.banner.as_deref(), Some(WedgeKind::ReadSide.notice()));
    assert_eq!(
        report.offered,
        Some(WedgeKind::ReadSide),
        "a wedge held past {ENGINE_BUSY_MODAL_THRESHOLD:?} never escalated into \
         the modal: {report:?}"
    );
    assert!(
        !report.unattended,
        "an open connection that may still answer must never be restarted \
         unattended"
    );

    // the modal prints a duration the fold hands it, so a readout that
    // stopped agreeing with the oracle's own clock is a readout that stopped
    // being true -- the cheap half of the cross-check, with the loop's own
    // fold pinned separately in `crates/view/tests/supervision_live.rs`
    let readout = report
        .offered_readout
        .unwrap_or_else(|| panic!("an open modal carried no readout: {report:?}"));
    let observed = report
        .wedged_for
        .unwrap_or_else(|| panic!("an open modal with nothing observed behind it: {report:?}"));
    assert!(
        readout >= ENGINE_BUSY_MODAL_THRESHOLD.as_secs(),
        "the modal opened showing {readout}s, less than the \
         {ENGINE_BUSY_MODAL_THRESHOLD:?} that opens it"
    );
    let shown = Duration::from_secs(readout);
    let drift = shown
        .saturating_sub(observed)
        .max(observed.saturating_sub(shown));
    assert!(
        // one whole second on top, because the readout is truncated seconds
        // of the duration the oracle measures in full
        drift <= observation_slack() + Duration::from_secs(1),
        "the modal showed {readout}s for a wedge the oracle had been reading \
         for {observed:?}"
    );
}

/// A killed child reaches `Dead` inside the same bound, which it must beat
/// comfortably: a closed connection is observable the moment the reader
/// hits EOF rather than after any silence has to accumulate.
#[test]
fn a_killed_engine_is_reported_dead_within_the_probe_and_threshold_bound() {
    let report = run_schedule(HangRun::new(HangSchedule::DeadConnection)).unwrap();

    let detected = report
        .detected_after
        .unwrap_or_else(|| panic!("a killed engine was never reported dead: {report:?}"));
    let deadline = detection_deadline();
    assert!(
        detected <= deadline,
        "the death took {detected:?} to notice, past {deadline:?}"
    );
    assert!(
        detected < DETECTION_BOUND,
        "a closed connection is observable the moment its reader hits EOF, so \
         {detected:?} means the verdict waited out a silence it had no reason to"
    );
    assert_eq!(report.verdict, Liveness::Dead);
    assert_eq!(report.wedge, Some(WedgeKind::Dead));
}

// -- the tier's own tripwire --------------------------------------------

/// De-wire the heartbeat and the read-side schedule must stop detecting
/// anything.
///
/// A detector nobody has ever seen fail is not evidence that the thing it
/// detects is happening; it may be a detector that reports the same verdict
/// whatever the engine does. Pausing the prober removes the one signal a
/// read-side wedge is visible through, and this test fails if the schedule
/// goes on reporting `Wedged` without it.
///
/// Only the read side. The dead schedule is *not* falsifiable this way:
/// its verdict rides `EngineHandle::is_closed`, so a killed engine reads
/// `Dead` with the prober paused, stopped or never started -- which is a
/// fact about where each verdict comes from rather than a gap in this test.
#[test]
fn a_read_side_wedge_goes_unnoticed_once_the_heartbeat_is_de_wired() {
    let mut session = session();
    // one whole interval of folding first, so every probe already issued is
    // answered and acknowledged while the engine is still healthy: pausing
    // leaves outstanding probes outstanding, and one still aging would time
    // out on its own and detect a wedge the paused watch never saw
    session.fold_for(HEARTBEAT_PROBE_INTERVAL);
    session.pause_heartbeat();
    session.fold_for(HEARTBEAT_PROBE_INTERVAL / 4);

    let fired = session.fire(HangSchedule::ReadSideWedge).unwrap();
    let detected = session.await_liveness(Liveness::Wedged, fired, detection_deadline());

    // read after the wait rather than before it, so proving the engine
    // stopped answering costs the wait nothing: without this the whole test
    // would also pass over a schedule that silently stopped wedging
    // anything, which is the one way a tripwire can rust shut
    assert!(
        session.engine().handle.get_mode().is_err(),
        "the engine was still answering, so nothing was wedged and detecting \
         nothing says nothing about the heartbeat"
    );

    assert_eq!(
        detected, None,
        "a wedge was reported {detected:?} after firing with the prober paused, \
         so these schedules do not detect through the heartbeat and would pass \
         over a de-wired one"
    );
    assert!(
        session.banner().is_none(),
        "a de-wired heartbeat raised the sticky banner anyway: {:?}",
        session.banner()
    );
    assert!(
        session.model().overlays().is_empty(),
        "a de-wired heartbeat escalated to the modal anyway"
    );
}

// -- recovery, both sides of the switch ---------------------------------

/// The whole unattended path end to end: a real crash, a restart nobody had
/// to answer for, a replacement that answers within the handshake bound, and
/// the unsaved edit back out of nvim's own swap file.
#[test]
fn an_unattended_session_replaces_a_dead_engine_and_recovers_its_swap() {
    let report = run_schedule(HangRun::new(HangSchedule::DeadConnection)).unwrap();

    assert!(
        report.unattended,
        "automatic recovery is on, so a dead engine must be replaced without \
         asking: {report:?}"
    );
    assert_eq!(
        report.offered, None,
        "nothing may be asked of a user whose engine is already being replaced"
    );
    let restarted = report
        .restarted_after
        .unwrap_or_else(|| panic!("no restart happened: {report:?}"));
    let bound = restart_bound();
    assert!(
        restarted <= bound,
        "the replacement answered nvim_get_mode only after {restarted:?}, past \
         the {bound:?} a reaping, a spawn, an attach and a probe are allowed"
    );
    assert_eq!(
        report.recovered_line.as_deref(),
        Some("never written to disk"),
        "the replacement came up on the file as it is on disk, discarding what \
         the swap held"
    );
    assert_eq!(
        report.replacement_verdict,
        Some(Liveness::Alive),
        "the replacement is not answering the same watch that condemned its \
         predecessor: {report:?}"
    );
    assert!(
        report.folds >= SURVIVAL_FOLDS,
        "the observing side completed only {} folds",
        report.folds
    );
}

/// The same crash with the user's switch off: nothing respawns on its own,
/// the modal stays up, and the replacement happens only once the modal's own
/// `Restart` key is pressed.
#[test]
fn an_attended_session_waits_for_the_restart_choice_before_replacing_anything() {
    let report = run_schedule(HangRun::new(HangSchedule::DeadConnection).attended()).unwrap();

    assert!(
        !report.unattended,
        "auto_restart = false must never respawn behind the user's back: {report:?}"
    );
    assert_eq!(
        report.offered,
        Some(WedgeKind::Dead),
        "a dead engine with recovery turned off must leave the modal open: {report:?}"
    );
    let restarted = report.restarted_after.unwrap_or_else(|| {
        panic!("the modal's own Restart choice brought nothing back: {report:?}")
    });
    let bound = restart_bound();
    assert!(
        restarted <= bound,
        "the replacement answered only after {restarted:?}, past {bound:?}"
    );
    assert_eq!(
        report.recovered_line.as_deref(),
        Some("never written to disk"),
        "the manually chosen restart recovered nothing"
    );
    assert_eq!(report.replacement_verdict, Some(Liveness::Alive));
}

/// The modal must not close on its own while the condition behind it is
/// still true: with recovery off, a dead connection stays offered for as
/// long as the session goes on folding it.
#[test]
fn an_attended_dead_engines_modal_stays_open_across_every_later_fold() {
    let mut session = session();
    session.model.supervision.auto_restart = false;
    let fired = session.fire(HangSchedule::DeadConnection).unwrap();
    session
        .await_liveness(Liveness::Dead, fired, detection_deadline())
        .expect("a killed engine must read dead");
    assert_eq!(
        session.model().engine_busy().map(|open| open.kind),
        Some(WedgeKind::Dead)
    );

    session.fold_for(HEARTBEAT_PROBE_INTERVAL * 2);
    assert!(
        !session.take_restart_request(),
        "a later fold asked for a restart nobody chose"
    );
    assert_eq!(
        session.model().engine_busy().map(|open| open.kind),
        Some(WedgeKind::Dead),
        "the modal closed itself while the connection it offers to replace is \
         still gone"
    );
    assert!(
        session.press(RESTART_NOTATION),
        "the modal's own Restart key must ask for a replacement"
    );
}

// -- non-interference ---------------------------------------------------

/// The modal is raised by view noticing something, never by the user asking
/// for it, and the thing it notices is very often an operation that
/// finishes. Raising it, escalating it and dismissing it must therefore
/// leave nvim's own state -- buffer text, cursor, mode, registers, marks --
/// exactly as it was, against a live engine that was never wedged at all.
///
/// Same shape as the native features' own non-interference proof
/// (`view-harness/tests/native_interference.rs`), with the one difference
/// that matters here: this overlay is opened by a `Msg` the runtime folds,
/// not by a keystroke, so the drift it could cause would never be traceable
/// to anything the user did.
#[test]
fn raising_and_dismissing_the_engine_busy_overlay_leaves_the_engine_untouched() {
    let session = session();
    let mut model = view_core::model::Model::with_term_size(80, 24);

    // something in every field a drift could show up in: an empty session
    // would let an overlay that clobbered a mark pass for having had no mark
    // to clobber
    session
        .engine()
        .handle
        .feed_keys("ihello world<Esc>0mayy")
        .unwrap();
    session.engine().handle.eval_str("1").unwrap();

    let before = snapshot(&mut SessionProbe(&session)).unwrap();

    for observed_for in [
        Duration::ZERO,
        ENGINE_BUSY_MODAL_THRESHOLD,
        ENGINE_BUSY_MODAL_THRESHOLD + Duration::from_secs(5),
    ] {
        let effects = update(
            &mut model,
            Msg::EngineLiveness {
                wedge: Some(WedgeKind::ReadSide),
                observed_for,
            },
        );
        assert!(
            effects.iter().all(|e| !matches!(e, Effect::Rpc(_))),
            "the supervision fold sent something to the engine: {effects:?}"
        );
    }
    assert!(
        matches!(
            model.overlays().last().map(|o| &o.kind),
            Some(OverlayKind::EngineBusy(_))
        ),
        "the escalation never opened the modal, so this proves nothing about \
         one being open"
    );
    assert!(
        model
            .engine_busy()
            .is_some_and(|open| open.since.readout()
                >= SinceStamp::new(ENGINE_BUSY_MODAL_THRESHOLD).readout()),
        "the modal's readout did not carry the duration it was raised for"
    );

    // dismissed the way a user dismisses it, and the `<Esc>` that does it
    // still goes on to the engine: answering an annunciator nobody asked for
    // may not cost a keystroke
    let effects = update(
        &mut model,
        Msg::Key(view_core::msg::Key {
            notation: "<Esc>".into(),
        }),
    );
    for effect in effects {
        if let Effect::Rpc(view_core::msg::RpcCall::Input { notation }) = effect {
            session.engine().handle.input(&notation).unwrap();
        }
    }
    assert!(model.overlays().is_empty(), "Dismiss must close the modal");

    let after = snapshot(&mut SessionProbe(&session)).unwrap();
    assert_eq!(
        before, after,
        "raising, escalating and dismissing the engine-busy overlay moved the \
         engine's own state"
    );
}

// -- the report's own pass/fail reading ---------------------------------

/// A report shaped like the run that produced it, with the timing left to
/// the caller: every predicate below turns on that one field.
fn report(schedule: HangSchedule, detected: Option<Duration>) -> super::HangReport {
    super::HangReport {
        schedule,
        verdict: schedule.expected(),
        wedge: None,
        detected_after: detected,
        folds: 1,
        banner: None,
        offered: None,
        offered_readout: None,
        wedged_for: None,
        unattended: false,
        restarted_after: None,
        recovered_line: None,
        replacement_verdict: None,
    }
}

#[test]
fn a_hang_reaching_its_verdict_inside_the_deadline_reads_as_detected() {
    let ok = report(HangSchedule::ReadSideWedge, Some(detection_deadline()));
    assert!(ok.is_success());
    assert!(ok.report_line().contains("DETECTED"));
}

/// The two ways a hang schedule stops being evidence: never detected at
/// all, and detected only past the bound the affordance promises.
#[test]
fn a_hang_that_missed_its_bound_reads_as_missed() {
    for detected in [None, Some(detection_deadline() + Duration::from_millis(1))] {
        let missed = report(HangSchedule::ReadSideWedge, detected);
        assert!(
            !missed.is_success(),
            "{detected:?} must not read as detected"
        );
        assert!(missed.report_line().contains("MISSED"));
    }
}

/// The control is the one schedule a short run fails: it proves its verdict
/// by holding it, so anything shorter than the bound a wedge would have
/// taken proves nothing about a key-wait not being one.
#[test]
fn a_control_held_for_less_than_the_wedge_bound_reads_as_missed() {
    assert!(report(HangSchedule::BlockedOnKey, Some(DETECTION_BOUND)).is_success());
    assert!(!report(
        HangSchedule::BlockedOnKey,
        Some(DETECTION_BOUND - Duration::from_millis(1))
    )
    .is_success());
}

/// A restart that produced an engine the same watch does not read as alive
/// is a failed recovery however promptly the death was noticed.
#[test]
fn a_replacement_that_is_not_alive_fails_its_schedule() {
    let mut ok = report(
        HangSchedule::DeadConnection,
        Some(Duration::from_millis(50)),
    );
    ok.replacement_verdict = Some(Liveness::Alive);
    assert!(ok.is_success());

    let mut wedged = ok.clone();
    wedged.replacement_verdict = Some(Liveness::Wedged);
    assert!(!wedged.is_success());
}

/// The host knob widens the observation allowance and nothing else, and
/// every way of writing it wrong leaves the shipped bound alone rather than
/// loosening it.
#[test]
fn the_slack_scale_widens_only_what_a_host_costs_and_never_by_accident() {
    assert_eq!(slack_scale(Some("4")), 4);
    assert_eq!(slack_scale(Some(" 4 ")), 4);
    for wrong in [
        None,
        Some(""),
        Some("0"),
        Some("-2"),
        Some("2.5"),
        Some("x"),
    ] {
        assert_eq!(slack_scale(wrong), 1, "{wrong:?} must not move the bound");
    }
    assert_eq!(detection_deadline(), DETECTION_BOUND + observation_slack());
}
