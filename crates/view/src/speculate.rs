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
    /// moving an origin back rather than by sleeping through it.
    #[cfg(test)]
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

/// Folds one call the loop is sending the engine into what speculation may
/// still claim.
pub(crate) fn note_engine_call(model: &mut Model, call: &RpcCall, clock: SpeculationClock) {
    fold_engine_call(model, call, clock.now());
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

    /// One clock's reading `elapsed` later, modelled by moving its origin
    /// back rather than by sleeping: the difference between two stamps is
    /// the only thing anything reads, and a test that waits out
    /// `SPECULATION_MAX_AGE` in real time buys nothing for the second it
    /// spends.
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

    /// The wiring case the age bound exists for: a session in which **no**
    /// redraw arrives at all -- not one that misses the predicted cell, but
    /// none -- still retires the prediction, because the pass that checks is
    /// the loop's own and not one a redraw has to reach.
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
