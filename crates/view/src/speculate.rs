//! The runtime's half of display-only speculative echo: the session clock
//! every prediction is stamped from, and the three folds the loop drives
//! [`SpeculateState`](view_core::native::speculate::SpeculateState) with.
//!
//! Split across two call sites on purpose, and the split is the design:
//! [`reconcile_speculation`] runs only where a redraw batch is in hand, since
//! that is the only thing a prediction can be judged against, while
//! [`expire_speculation`] runs on every loop pass whether or not the engine
//! said anything -- a call reachable only when a redraw arrives is provably
//! unreachable during a total redraw stall, which is the exact condition the
//! age bound exists to survive. [`next_expiry`] is the third piece of that
//! same bound: a per-pass check keeps its promise only if the loop takes a
//! pass, so the wait itself has to be told when the next prediction comes
//! due.
//!
//! Nothing here sends anything. A prediction is a glyph painted over the
//! authoritative grid until that grid says otherwise; the keystroke that
//! produced it reaches nvim by the path it always did.

use std::time::Duration;

use view_core::events::UiEvent;
use view_core::model::Model;
use view_core::msg::RpcCall;
use view_core::native::speculate::{SpecStamp, SPECULATION_MAX_AGE};

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
/// arrived, and marks the frame when that retired one.
///
/// The mark is what takes a retired glyph off the screen: a batch carrying no
/// `flush` leaves `update()` with no reason to set `dirty`, and speculation's
/// own layer would then keep painting a cell the engine has already answered.
pub(crate) fn reconcile_speculation(model: &mut Model, redraw: &[UiEvent]) {
    // nothing pending is every batch outside a typing burst, and reconciling
    // an empty list against one can produce nothing to paint. A mode change
    // such a batch carries therefore turns no epoch over, which reads
    // against `reconcile`'s stated contract without costing anything: an
    // epoch means something only relative to the predictions tagged with it,
    // and with none pending there is nothing under the one a mode change
    // would supersede
    if model.speculate.pending().is_empty() {
        return;
    }
    let before = model.speculate.pending().len();
    model.speculate.reconcile(redraw);
    mark_retirement(model, before);
}

/// Marks the frame when a fold has changed what speculation is holding
/// since `before`.
///
/// Every fold in this module ends here, and that is the point: a frame is
/// painted only once something has marked it, so a fold that retires an
/// already-painted prediction without marking one leaves the glyph on the
/// terminal with nothing left to take it off -- the age bound cannot rescue
/// it either, since it has nothing pending left to find stale. Marking on
/// any change rather than on a shrink covers the appearing prediction with
/// the same rule; marking on no change at all is what keeps every `<Esc>`,
/// click and normal-mode key of a session outside a typing burst from
/// buying a repaint.
fn mark_retirement(model: &mut Model, before: usize) {
    model.dirty |= model.speculate.pending().len() != before;
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

/// Folds one call view is sending the engine into what speculation may still
/// claim.
///
/// Three of them can touch a prediction, and they are the three that reach
/// the buffer or the cursor: the keystroke a prediction is made from, and the
/// paste and mouse events that move one or the other without any character
/// ever reaching `predict` -- the caller obligation that method states, for
/// the two paths that are not keys. Everything else view sends nvim (an
/// option, a resize, a reply, a registration) leaves both the buffer and the
/// cursor exactly where the prediction found them; a resize additionally
/// comes back as a `grid_resize` that reconciliation reads for itself.
pub(crate) fn note_engine_call(model: &mut Model, call: &RpcCall, clock: SpeculationClock) {
    match call {
        RpcCall::Input { notation } => predict_echo(model, notation, clock),
        RpcCall::Paste { .. } | RpcCall::InputMouse { .. } => invalidate_speculation(model),
        // `RpcCall` is `#[non_exhaustive]`: a call added later is assumed to
        // touch neither the buffer nor the cursor until someone decides
        // otherwise, which is the reading that costs an unaccelerated
        // character rather than a wrong one
        _ => {}
    }
}

/// Folds one engine-bound keystroke into the display-only prediction it is
/// expected to produce, and marks the frame when what is pending changed.
///
/// The mark is the whole point in both directions: a keystroke changes no
/// authoritative state, so without it a new predicted glyph would wait for
/// the engine's own redraw -- exactly the wait the prediction exists to skip
/// -- and a keystroke that instead retires what was already painted would
/// leave that glyph standing.
fn predict_echo(model: &mut Model, notation: &str, clock: SpeculationClock) {
    let Some(key) = lone_char(notation) else {
        // the caller obligation `predict` states: a notation key moves the
        // cursor or changes what typing means without ever reaching it as a
        // character, so nothing pending can survive one
        invalidate_speculation(model);
        return;
    };
    let cursor = model.engine.grid().cursor();
    let mode = model.engine.mode.current.as_str();
    let before = model.speculate.pending().len();
    // the refusal path is why the answer is read off the pending list rather
    // than off this `Option`: a character `predict` declines discards
    // everything pending inside `predict` itself, and the caller sees only
    // the `None` it shares with a mode that was never predicting at all
    let _ = model.speculate.predict(mode, key, cursor, clock.now());
    mark_retirement(model, before);
}

/// Discards everything pending because something that is not a predictable
/// keystroke has reached the buffer or the cursor, marking the frame when
/// that took a painted glyph off it.
fn invalidate_speculation(model: &mut Model) {
    let before = model.speculate.pending().len();
    model.speculate.reset_epoch();
    mark_retirement(model, before);
}

/// The one character `notation` is, or `None` when it is a notation key.
///
/// nvim notation writes every key that is not a single printable character as
/// a bracketed name -- `<Esc>`, `<C-w>`, `<S-Tab>`, and `<lt>` for a literal
/// `<` -- so "exactly one char" separates the two with no table of key names
/// to keep in step with the encoder that produced them.
fn lone_char(notation: &str) -> Option<char> {
    let mut chars = notation.chars();
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
}

/// The loop's per-pass age check on what speculation is still holding.
///
/// Deliberately not gated on a redraw arriving: see this module's own doc.
pub(crate) fn expire_speculation(model: &mut Model, clock: SpeculationClock) {
    // the clock is read only when something is pending, which no steady-state
    // pass is: expiring an empty list is a no-op, so a pass that skips the
    // reading and a pass that takes it fold identically, and the reading
    // saved is one every pass of every session would otherwise pay for a
    // feature that is idle outside a typing burst
    if model.speculate.pending().is_empty() {
        return;
    }
    retire_speculation(model, clock.now());
}

/// Retires every prediction the age bound has caught as of `now`, marking the
/// frame when it retired one.
///
/// A pass that reaches the bound is by definition a pass no redraw arrived
/// on, so nothing else about it has any reason to mark the frame, and a
/// retirement nobody paints leaves exactly the stale glyph the bound exists
/// to take off the screen.
fn retire_speculation(model: &mut Model, now: SpecStamp) {
    let before = model.speculate.pending().len();
    model.speculate.expire_stale(now);
    mark_retirement(model, before);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::time::Instant;
    use view_core::grid::GridOp;
    use view_core::native::speculate::SPECULATION_MAX_AGE;

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
    fn clock_reading(origin: Instant, elapsed: std::time::Duration) -> SpeculationClock {
        SpeculationClock::started_at(origin.checked_sub(elapsed).unwrap_or(origin))
    }

    /// The wiring case the age bound exists for: a session in which **no**
    /// redraw arrives at all -- not one that misses the predicted cell, but
    /// none -- still retires the prediction, because the pass that checks is
    /// the loop's own and not one a redraw has to reach.
    #[test]
    fn a_pass_with_no_redraw_at_all_still_retires_a_prediction_past_the_bound() {
        let origin = Instant::now();
        let mut model = typing_model();
        predict_echo(&mut model, "x", SpeculationClock::started_at(origin));
        assert_eq!(model.speculate.pending().len(), 1);
        model.dirty = false;

        expire_speculation(
            &mut model,
            clock_reading(
                origin,
                SPECULATION_MAX_AGE + std::time::Duration::from_millis(1),
            ),
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
        predict_echo(&mut model, "x", SpeculationClock::started_at(origin));
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

    /// A notation key never reaches `predict` as a character, so the caller
    /// is what has to invalidate for it -- and it must, since `<Left>` moves
    /// the cursor out from under everything pending.
    #[test]
    fn a_notation_key_retires_what_is_pending_and_supersedes_the_epoch() {
        let clock = SpeculationClock::default();
        let mut model = typing_model();
        predict_echo(&mut model, "x", clock);
        let epoch = model.speculate.epoch();

        predict_echo(&mut model, "<Left>", clock);

        assert!(model.speculate.pending().is_empty());
        assert!(model.speculate.epoch() > epoch);
    }

    /// A paste and a mouse click both move the cursor or the text under a
    /// prediction without any character reaching `predict`, so both owe the
    /// invalidation a notation key owes.
    #[test]
    fn a_call_that_moves_the_cursor_without_a_keystroke_retires_what_is_pending() {
        for call in [
            RpcCall::Paste {
                text: "pasted".to_string(),
            },
            RpcCall::InputMouse {
                button: "left".to_string(),
                action: "press".to_string(),
                modifier: String::new(),
                row: 9,
                col: 9,
            },
        ] {
            let clock = SpeculationClock::default();
            let mut model = typing_model();
            note_engine_call(
                &mut model,
                &RpcCall::Input {
                    notation: "x".into(),
                },
                clock,
            );
            assert_eq!(model.speculate.pending().len(), 1, "{call:?}");

            note_engine_call(&mut model, &call, clock);

            assert!(model.speculate.pending().is_empty(), "{call:?}");
        }
    }

    /// A notation key retires a prediction that is already painted, so the
    /// frame it is painted on has to be marked: the loop paints only a dirty
    /// frame, and nothing else on this pass has any reason to mark one.
    #[test]
    fn a_notation_key_that_retires_a_painted_prediction_marks_the_frame() {
        let clock = SpeculationClock::default();
        let mut model = typing_model();
        predict_echo(&mut model, "x", clock);
        assert_eq!(model.speculate.pending().len(), 1);
        model.dirty = false;

        predict_echo(&mut model, "<Left>", clock);

        assert!(model.speculate.pending().is_empty());
        assert!(
            model.dirty,
            "a notation key retired a painted prediction and marked no frame"
        );
    }

    /// The same obligation for the character `predict` itself refuses: the
    /// refusal clears what is pending inside `predict`, so the caller sees
    /// only a `None` and has to read the pending list to notice.
    #[test]
    fn a_refused_character_that_retires_a_painted_prediction_marks_the_frame() {
        let clock = SpeculationClock::default();
        let mut model = typing_model();
        predict_echo(&mut model, "x", clock);
        model.engine.mode.current = "normal".to_string();
        model.dirty = false;

        predict_echo(&mut model, "j", clock);

        assert!(model.speculate.pending().is_empty());
        assert!(
            model.dirty,
            "a refused character retired a painted prediction and marked no frame"
        );
    }

    /// And for the two calls that reach the buffer or the cursor without any
    /// character reaching `predict` at all.
    #[test]
    fn a_paste_or_a_click_that_retires_a_painted_prediction_marks_the_frame() {
        for call in [
            RpcCall::Paste {
                text: "pasted".to_string(),
            },
            RpcCall::InputMouse {
                button: "left".to_string(),
                action: "press".to_string(),
                modifier: String::new(),
                row: 9,
                col: 9,
            },
        ] {
            let clock = SpeculationClock::default();
            let mut model = typing_model();
            predict_echo(&mut model, "x", clock);
            assert_eq!(model.speculate.pending().len(), 1, "{call:?}");
            model.dirty = false;

            note_engine_call(&mut model, &call, clock);

            assert!(model.speculate.pending().is_empty(), "{call:?}");
            assert!(
                model.dirty,
                "{call:?} retired a painted prediction and marked no frame"
            );
        }
    }

    /// Nothing pending is the steady state for all three invalidation
    /// classes, and none of them may mark a frame there: a repaint bought by
    /// every `<Esc>`, every click and every normal-mode key of every session
    /// is a cost the feature never earns back.
    #[test]
    fn an_invalidation_with_nothing_pending_marks_no_frame() {
        let clock = SpeculationClock::default();
        let mut model = typing_model();

        predict_echo(&mut model, "<Left>", clock);
        note_engine_call(
            &mut model,
            &RpcCall::Paste {
                text: "pasted".to_string(),
            },
            clock,
        );
        model.engine.mode.current = "normal".to_string();
        predict_echo(&mut model, "j", clock);

        assert!(model.speculate.pending().is_empty());
        assert!(
            !model.dirty,
            "an invalidation with nothing to retire repainted"
        );
    }

    /// A call that reaches neither the buffer nor the cursor leaves a
    /// prediction standing: invalidating on one would cost an accelerated
    /// character for nothing.
    #[test]
    fn a_call_that_reaches_neither_the_buffer_nor_the_cursor_retires_nothing() {
        let clock = SpeculationClock::default();
        let mut model = typing_model();
        note_engine_call(
            &mut model,
            &RpcCall::Input {
                notation: "x".into(),
            },
            clock,
        );

        note_engine_call(&mut model, &RpcCall::GetDefaultHl { generation: 1 }, clock);

        assert_eq!(model.speculate.pending().len(), 1);
    }

    /// `<lt>` is nvim's notation for a typed `<`, and it is three characters
    /// on the wire: predicting one glyph per character would put a `<`, a
    /// `l` and a `t` on the grid.
    #[test]
    fn a_bracketed_notation_is_never_mistaken_for_the_characters_that_spell_it() {
        for notation in ["<lt>", "<Esc>", "<C-w>", "<S-Tab>"] {
            assert_eq!(lone_char(notation), None, "{notation}");
        }
        assert_eq!(lone_char("x"), Some('x'));
        assert_eq!(lone_char(" "), Some(' '));
    }

    /// Speculation is mode-gated at the model, so a key typed outside insert
    /// mode reaches nvim exactly as before and leaves nothing behind.
    #[test]
    fn a_key_typed_outside_insert_mode_predicts_nothing() {
        let mut model = typing_model();
        model.engine.mode.current = "normal".to_string();

        predict_echo(&mut model, "j", SpeculationClock::default());

        assert!(model.speculate.pending().is_empty());
        assert!(!model.dirty, "an unpredicted key repaints nothing");
    }

    /// A redraw batch that answers a predicted cell retires it and marks the
    /// frame, so the layer painting over that cell goes with it.
    #[test]
    fn a_redraw_that_answers_a_prediction_retires_it_and_marks_the_frame() {
        let mut model = typing_model();
        predict_echo(&mut model, "x", SpeculationClock::default());
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
        predict_echo(&mut model, "x", SpeculationClock::default());
        model.dirty = false;

        reconcile_speculation(&mut model, &[UiEvent::Flush]);

        assert_eq!(model.speculate.pending().len(), 1);
        assert!(!model.dirty);
    }
}
