//! The runtime's half of display-only speculative echo: the session clock
//! every prediction is stamped from, and the loop's three call sites for the
//! folds `view-core` owns.
//!
//! Nothing is decided here. [`fold_engine_call`], [`fold_redraw`] and
//! [`fold_expiry`] classify a call, a batch and a pass, and this module's
//! wrappers do one thing each: read this session's clock and hand the fold a
//! stamp. That split is what lets the differential battery drive the same
//! folds the loop drives instead of a second opinion about them.
//!
//! The three call sites are the design, though. [`reconcile_speculation`]
//! runs only where a redraw batch is in hand, since that is the only thing a
//! prediction can be judged against, while [`expire_speculation`] runs on
//! every loop pass whether or not the engine said anything -- a call
//! reachable only when a redraw arrives is provably unreachable during a
//! total redraw stall, which is the exact condition the age bound exists to
//! survive. [`next_expiry`] is the third piece of that same bound: a per-pass
//! check keeps its promise only if the loop takes a pass, so the wait itself
//! has to be told when the next prediction comes due.
//!
//! Nothing here sends anything. A prediction is a glyph painted over the
//! authoritative grid until that grid says otherwise; the keystroke that
//! produced it reaches nvim by the path it always did.

use std::time::Duration;

use view_core::events::UiEvent;
use view_core::model::Model;
use view_core::msg::RpcCall;
use view_core::native::speculate::{
    fold_engine_call, fold_expiry, fold_redraw, SpecStamp, SPECULATION_MAX_AGE,
};

/// The fixed origin every [`SpecStamp`] in one session is measured from.
///
/// A stamp is a point on a monotonic timeline, and two of them mean nothing
/// together unless both were taken from the same origin (see [`SpecStamp`]).
/// Holding that origin in one session-scoped value is what makes a second one
/// unrepresentable: nothing here reads a clock any other way, so a call site
/// cannot quietly invent an origin of its own and stamp every prediction as
/// freshly made -- which would leave [`expire_speculation`] with nothing it
/// could ever find stale.
#[derive(Debug, Clone, Copy)]
pub struct SpeculationClock {
    origin: std::time::Instant,
}

impl Default for SpeculationClock {
    fn default() -> Self {
        Self {
            origin: std::time::Instant::now(),
        }
    }
}

impl SpeculationClock {
    /// A clock whose origin is `origin`.
    ///
    /// A session takes its origin from [`Default`] and never names one; this
    /// exists so a test can model the passage of `SPECULATION_MAX_AGE` by
    /// moving an origin back rather than by sleeping through it. Under the
    /// arm that predicts nothing there is no prediction to age, so no test
    /// there names an origin at all.
    #[cfg(all(test, not(feature = "bench-no-speculate")))]
    #[must_use]
    pub fn started_at(origin: std::time::Instant) -> Self {
        Self { origin }
    }

    /// Now, as elapsed time since this session's origin.
    #[must_use]
    pub fn now(self) -> SpecStamp {
        SpecStamp::new(self.origin.elapsed())
    }
}

/// Judges every pending prediction against the redraw batch that just
/// arrived.
pub(crate) fn reconcile_speculation(model: &mut Model, redraw: &[UiEvent]) {
    fold_redraw(model, redraw);
}

/// How long the loop may wait before a pending prediction would be older
/// than [`SPECULATION_MAX_AGE`], or `None` while nothing is pending.
///
/// The age bound is a promise about wall-clock time, and
/// [`expire_speculation`] can only keep it on a pass the loop actually
/// takes: every other wake source this loop arms is an order of magnitude
/// coarser than the bound, and a session whose heartbeat is paused arms
/// nothing at all, so a prediction made just before the engine went silent
/// would sit painted until something unrelated woke the loop. `None`
/// outside a typing burst is what keeps that from becoming a periodic
/// wakeup an idle session pays for: with nothing pending there is no
/// deadline, no clock reading, and the wait is exactly the wait it was.
pub(crate) fn next_expiry(model: &Model, clock: SpeculationClock) -> Option<Duration> {
    let pending = model.speculate.pending();
    if pending.is_empty() {
        return None;
    }
    let now = clock.now();
    pending
        .iter()
        .map(|cell| SPECULATION_MAX_AGE.saturating_sub(now.age_since(cell.predicted_at)))
        .min()
}

/// Whether this build predicts anything at all.
///
/// False only under `bench-no-speculate`, the bench matrix's arm for the
/// `echo` row. That row's boundary is the typed glyph appearing on screen,
/// and a predicted glyph is the same character in the same cell as the
/// authoritative one, so nothing the harness parses out of a pty can keep
/// the row measuring the round trip it is defined as. An arm that never
/// writes the predicted glyph can, and the speculated paint is measured by
/// `echo_speculated` under names of its own.
///
/// One flag at the one site that creates a prediction, rather than a
/// `cfg` at each of the loop's four call sites: with nothing ever pending,
/// reconciliation, expiry and the expiry deadline are already the no-ops
/// they are outside a typing burst, so the arm differs from the shipped
/// binary in exactly one branch that a release build folds away.
const PREDICTS: bool = !cfg!(feature = "bench-no-speculate");

// `task bench` and `task heartbeat-ab` set VIEW_BENCH_NO_SPECULATE for the
// arm binaries they build, and nothing else in the tree sets it, so every
// other optimized build with the prediction compiled out fails to compile
// instead of becoming an artifact that types like the shipped editor and
// answers slower. `debug_assertions` is what separates the two cases: a
// debug build cannot be mistaken for a shipped editor, so a lint or test
// leg may compile the arm with no ceremony, while the builds that could
// plausibly leave the machine must be deliberate. `option_env!` is tracked
// by cargo, so flipping the variable rebuilds rather than reusing a cached
// verdict. Mirrors the same guard on the no-heartbeat arm
// (view-engine/src/process.rs).
#[cfg(all(feature = "bench-no-speculate", not(debug_assertions)))]
const _: () = assert!(
    option_env!("VIEW_BENCH_NO_SPECULATE").is_some(),
    "bench-no-speculate compiles out speculative echo and must never ship: set \
     VIEW_BENCH_NO_SPECULATE=1 in the build environment (as `task bench` does) if this optimized \
     build really is the echo rows' arm"
);

/// Folds one call the loop is sending the engine into what speculation may
/// still claim.
pub(crate) fn note_engine_call(model: &mut Model, call: &RpcCall, clock: SpeculationClock) {
    if PREDICTS {
        fold_engine_call(model, call, clock.now());
    }
}

/// The loop's per-pass age check on what speculation is still holding.
///
/// Deliberately not gated on a redraw arriving: see this module's own doc.
pub(crate) fn expire_speculation(model: &mut Model, clock: SpeculationClock) {
    fold_expiry(model, clock.now());
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    #[cfg(not(feature = "bench-no-speculate"))]
    use std::time::Instant;
    use view_core::grid::GridOp;

    /// A model mid-typing-burst: an 80x24 grid, the cursor where the engine
    /// last reported it, and insert mode active.
    fn typing_model() -> Model {
        let mut model = Model::with_term_size(80, 24);
        model.engine.apply_grid(GridOp::Resize {
            width: 80,
            height: 24,
        });
        model
            .engine
            .apply_grid(GridOp::CursorGoto { row: 3, col: 5 });
        model.engine.mode.current = "insert".to_string();
        model.dirty = false;
        model
    }

    // Every case past the twin pair below asserts what happens to a
    // prediction once one exists -- it is reconciled, it expires, it bounds
    // the loop's wait -- so under the arm they would be assertions about a
    // prediction never made. Configured out there rather than given a
    // conditional expectation, so the arm's suite states only what the arm
    // itself owes and `task test-arms` runs a green suite rather than an
    // adapted one.

    /// One clock's reading `elapsed` later, modelled by moving its origin
    /// back rather than by sleeping: the difference between two stamps is
    /// the only thing anything reads, and a test that waits out
    /// `SPECULATION_MAX_AGE` in real time buys nothing for the second it
    /// spends.
    #[cfg(not(feature = "bench-no-speculate"))]
    fn clock_reading(origin: Instant, elapsed: Duration) -> SpeculationClock {
        SpeculationClock::started_at(origin.checked_sub(elapsed).unwrap_or(origin))
    }

    /// One engine-bound keystroke, as the loop hands it to the fold.
    fn typed(model: &mut Model, notation: &str, clock: SpeculationClock) {
        note_engine_call(
            model,
            &RpcCall::Input {
                notation: notation.to_string(),
            },
            clock,
        );
    }

    /// The shipped half of the one behavioural difference the `echo` row's
    /// arm has: a typed character leaves a prediction pending.
    ///
    /// Twinned with the arm's own case below rather than written once
    /// against [`PREDICTS`], because a test that reads the constant it
    /// guards moves its expectation with the code: forcing `PREDICTS` true
    /// under the feature would mutate both halves of such an assertion and
    /// it would still pass. Each twin states the outcome its configuration
    /// owes outright, so neither can be satisfied by the other's build.
    #[cfg(not(feature = "bench-no-speculate"))]
    #[test]
    fn a_shipped_build_leaves_a_keystroke_pending() {
        let mut model = typing_model();

        typed(&mut model, "x", SpeculationClock::default());

        assert_eq!(
            model.speculate.pending().len(),
            1,
            "the shipped build predicts the typed glyph, and the echo_speculated row measures \
             the paint it produces"
        );
    }

    /// The arm's half: under `bench-no-speculate` the same keystroke leaves
    /// nothing pending, which is the whole property the `echo` row's
    /// boundary depends on.
    #[cfg(feature = "bench-no-speculate")]
    #[test]
    fn the_arm_leaves_no_keystroke_pending() {
        let mut model = typing_model();

        typed(&mut model, "x", SpeculationClock::default());

        assert!(
            model.speculate.pending().is_empty(),
            "a prediction under this feature puts the typed glyph on screen before the engine \
             answers, and the echo row would time that paint under the round trip's own metric \
             names"
        );
    }

    /// The wiring case the age bound exists for: a session in which **no**
    /// redraw arrives at all -- not one that misses the predicted cell, but
    /// none -- still retires the prediction, because the pass that checks is
    /// the loop's own and not one a redraw has to reach.
    #[cfg(not(feature = "bench-no-speculate"))]
    #[test]
    fn a_pass_with_no_redraw_at_all_still_retires_a_prediction_past_the_bound() {
        let origin = Instant::now();
        let mut model = typing_model();
        typed(&mut model, "x", SpeculationClock::started_at(origin));
        assert_eq!(model.speculate.pending().len(), 1);
        model.dirty = false;

        expire_speculation(
            &mut model,
            clock_reading(origin, SPECULATION_MAX_AGE + Duration::from_millis(1)),
        );

        assert!(
            model.speculate.pending().is_empty(),
            "the bound is reached on a pass no redraw was needed for"
        );
        assert!(
            model.dirty,
            "a retirement nobody paints leaves the stale glyph on screen"
        );
    }

    /// The same pass, one instant earlier: a prediction still inside the
    /// bound is left alone, and nothing is repainted for it.
    #[cfg(not(feature = "bench-no-speculate"))]
    #[test]
    fn a_pass_inside_the_bound_retires_nothing_and_marks_nothing() {
        let origin = Instant::now();
        let mut model = typing_model();
        typed(&mut model, "x", SpeculationClock::started_at(origin));
        model.dirty = false;

        expire_speculation(&mut model, SpeculationClock::started_at(origin));

        assert_eq!(model.speculate.pending().len(), 1);
        assert!(!model.dirty);
    }

    /// The steady state: with nothing pending, the per-pass site touches
    /// neither the model nor the clock.
    #[test]
    fn a_pass_with_nothing_pending_leaves_the_frame_alone() {
        let mut model = typing_model();

        expire_speculation(&mut model, SpeculationClock::default());

        assert!(model.speculate.pending().is_empty());
        assert!(!model.dirty);
    }

    /// The wait the loop takes has to end before the oldest prediction is
    /// past the bound, and has to be absent entirely outside a burst.
    #[cfg(not(feature = "bench-no-speculate"))]
    #[test]
    fn the_next_expiry_is_what_is_left_of_the_bound_and_nothing_outside_a_burst() {
        let origin = Instant::now();
        let mut model = typing_model();
        assert_eq!(
            next_expiry(&model, SpeculationClock::started_at(origin)),
            None
        );

        typed(&mut model, "x", SpeculationClock::started_at(origin));

        let half = SPECULATION_MAX_AGE / 2;
        let left = next_expiry(&model, clock_reading(origin, half))
            .expect("a pending prediction comes due");
        assert!(left <= SPECULATION_MAX_AGE - half, "{left:?}");
    }

    /// A notation key never reaches `predict` as a character, so the caller
    /// is what has to invalidate for it -- and it must, since `<Left>` moves
    /// the cursor out from under everything pending. The frame is marked
    /// because the glyph it retires is already on the terminal.
    #[cfg(not(feature = "bench-no-speculate"))]
    #[test]
    fn a_notation_key_retires_a_painted_prediction_and_marks_the_frame() {
        let clock = SpeculationClock::default();
        let mut model = typing_model();
        typed(&mut model, "x", clock);
        let epoch = model.speculate.epoch();
        model.dirty = false;

        typed(&mut model, "<Left>", clock);

        assert!(model.speculate.pending().is_empty());
        assert!(model.speculate.epoch() > epoch);
        assert!(
            model.dirty,
            "a notation key retired a painted prediction and marked no frame"
        );
    }

    /// A redraw batch that answers a predicted cell retires it and marks the
    /// frame, so the layer painting over that cell goes with it.
    #[cfg(not(feature = "bench-no-speculate"))]
    #[test]
    fn a_redraw_that_answers_a_prediction_retires_it_and_marks_the_frame() {
        let mut model = typing_model();
        typed(&mut model, "x", SpeculationClock::default());
        model.dirty = false;

        reconcile_speculation(
            &mut model,
            &[UiEvent::GridLine {
                grid: 1,
                row: 3,
                col_start: 5,
                cells: vec![view_core::events::GridCell {
                    text: "x".to_string(),
                    hl_id: 0,
                    repeat: 1,
                }],
            }],
        );

        assert!(model.speculate.pending().is_empty());
        assert!(model.dirty);
    }

    /// A batch that answers nothing pending leaves the frame unmarked: a
    /// repaint nobody needs is a frame the loop pays for on every redraw of
    /// every session.
    #[cfg(not(feature = "bench-no-speculate"))]
    #[test]
    fn a_redraw_that_answers_nothing_marks_no_frame() {
        let mut model = typing_model();
        typed(&mut model, "x", SpeculationClock::default());
        model.dirty = false;

        reconcile_speculation(&mut model, &[UiEvent::Flush]);

        assert_eq!(model.speculate.pending().len(), 1);
        assert!(!model.dirty);
    }
}
