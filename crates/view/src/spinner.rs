//! The agent panel's spinner, on the loop's own clock: the one thing view
//! animates without being asked to by the engine, the terminal or the user.
//!
//! Split from `runtime` for the reason the animation is a module at all --
//! `view-core` has no clock, so the wall time a marker turns on cannot live
//! in the pure fold, and the loop that does own a clock should not grow a
//! second scheduler to run it. What lives here is one deadline the loop
//! folds in with the rest ([`crate::runtime`]'s `Wakeups`): the pass that
//! finds it due moves the frame, and a panel with nothing to animate
//! contributes no deadline at all.

use std::time::{Duration, Instant};

use view_core::model::Model;
use view_core::native::ai_panel::SPINNER_INTERVAL;

/// Moves the agent panel's spinner on when its frame has come due, and
/// holds the instant the next one does.
///
/// The loop's own deadline rather than a timer thread per frame: the wait
/// below already bounds itself on [`watch_deadline`], so an animation is a
/// deadline to fold into that, and a spinner that stops simply stops
/// contributing one. `due` is the loop's, for the same reason
/// speculation's expiry is: `view-core` has no clock, and the wall time an
/// animation runs on cannot live in a pure fold.
///
/// Costs nothing at all on a session with nothing animating -- no prompt
/// awaiting its first agent event and no tool call in flight -- and
/// nothing beyond one comparison on a session whose panel is closed: an
/// agent working behind a closed sidebar is not animating anything a user
/// can see, so it neither wakes this loop nor paints. The frame it resumes
/// on when the panel reopens is whichever one is current then, which is
/// the same frame a spinner that had been running the whole time would be
/// showing.
pub(crate) fn expire(model: &mut Model, due: &mut Option<Instant>, now: Instant) {
    if !(model.ai_panel_overlay_open() && model.ai_panel().transcript.is_spinning()) {
        *due = None;
        return;
    }
    match *due {
        Some(at) if now < at => return,
        Some(_) => {
            model.ai_panel_mut().transcript.advance_spinner();
            model.dirty = true;
        }
        None => {}
    }
    *due = Some(now + SPINNER_INTERVAL);
}

/// How long until the next frame is owed, or `None` when nothing is
/// animating -- what the loop bounds its wait by.
pub(crate) fn next_frame(due: Option<Instant>, now: Instant) -> Option<Duration> {
    due.map(|at| at.saturating_duration_since(now))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use view_core::msg::Msg;
    use view_core::native::ai_event::{AiEvent, ToolCallStatus};
    use view_core::update::update;

    /// A model with the agent panel open and one tool call in flight --
    /// one of the two states the spinner's deadline exists for, the other
    /// being a submitted prompt the agent has not answered
    /// (`a_submitted_prompt_bounds_the_wait_before_any_tool_call_exists`).
    fn spinning_model() -> Model {
        let mut model = Model::with_term_size(80, 24);
        model.ai_trusted = true;
        let _ = update(
            &mut model,
            Msg::FeatureInvoke {
                feature: "ai".to_string(),
                verb: "open".to_string(),
            },
        );
        let _ = update(
            &mut model,
            Msg::Ai(AiEvent::ToolCallUpdate {
                tool_call_id: "call_1".to_string(),
                title: "Run tests".to_string(),
                status: ToolCallStatus::InProgress,
                content: None,
            }),
        );
        model.dirty = false;
        model
    }

    /// The animation is the loop's own deadline: a frame comes due, the
    /// pass that folds it moves the marker and asks for the paint, and the
    /// wait that follows is bounded by the next frame rather than by a
    /// thread sleeping somewhere on the spinner's behalf.
    #[test]
    fn a_running_tool_call_bounds_the_wait_by_its_next_spinner_frame() {
        let mut model = spinning_model();
        let mut due = None;
        let start = Instant::now();

        expire(&mut model, &mut due, start);
        assert!(!model.dirty, "arming a deadline paints nothing by itself");
        let armed = due.expect("a call in flight arms the next frame");

        expire(&mut model, &mut due, start + SPINNER_INTERVAL / 2);
        assert!(!model.dirty, "a frame not yet due must not repaint");
        assert_eq!(due, Some(armed), "and must not move the deadline");

        let before = model
            .ai_panel()
            .transcript
            .rows_from(Default::default(), 8, 60);
        expire(&mut model, &mut due, armed);
        assert!(model.dirty, "the frame that came due owes a paint");
        assert_ne!(
            model
                .ai_panel()
                .transcript
                .rows_from(Default::default(), 8, 60),
            before,
            "and the marker it painted has moved"
        );
        assert!(due.is_some_and(|next| next > armed));
    }

    /// Submitting is what starts the wait, so it is what arms the frame:
    /// the gap this covers is the one before the agent's first event, which
    /// is exactly the stretch in which no tool call exists to animate. One
    /// clock for both -- the deadline the loop already folds in -- and the
    /// prompt is driven through `update()` so the submit path itself is
    /// load-bearing here.
    #[test]
    fn a_submitted_prompt_bounds_the_wait_before_any_tool_call_exists() {
        let mut model = Model::with_term_size(80, 24);
        model.ai_trusted = true;
        let _ = update(
            &mut model,
            Msg::FeatureInvoke {
                feature: "ai".to_string(),
                verb: "open".to_string(),
            },
        );
        model.ai_panel_mut().push_input("fix the retry policy");
        let _ = update(
            &mut model,
            Msg::Key(view_core::msg::Key {
                notation: "<CR>".to_string(),
            }),
        );
        assert_eq!(
            model.ai_panel().transcript.len(),
            1,
            "the prompt is the only thing on screen, and nothing else is animating"
        );
        model.dirty = false;
        let mut due = None;
        let start = Instant::now();

        expire(&mut model, &mut due, start);
        let armed = due.expect("a prompt awaiting its answer arms the next frame");
        assert!(!model.dirty, "arming a deadline paints nothing by itself");

        let before = model
            .ai_panel()
            .transcript
            .rows_from(Default::default(), 8, 60);
        expire(&mut model, &mut due, armed);
        assert!(model.dirty, "the frame that came due owes a paint");
        assert_ne!(
            model
                .ai_panel()
                .transcript
                .rows_from(Default::default(), 8, 60),
            before,
            "and the prompt's own marker has moved"
        );
    }

    /// A closed panel is not showing anything: an agent running a
    /// three-minute tool call behind one must cost no wakeup, no repaint,
    /// and no frame -- the whole reason the deadline is read off the model
    /// on the pass that would arm it.
    #[test]
    fn a_tool_call_running_behind_a_closed_panel_arms_nothing() {
        let mut model = spinning_model();
        let _ = update(
            &mut model,
            Msg::FeatureInvoke {
                feature: "ai".to_string(),
                verb: "close".to_string(),
            },
        );
        model.dirty = false;
        let mut due = Some(Instant::now());

        expire(&mut model, &mut due, Instant::now());

        assert_eq!(due, None, "a closed panel disarms the frame it had");
        assert!(!model.dirty, "and asks for no paint");
    }

    /// The resolved call is the end of the sequence: nothing left to
    /// animate is nothing left to wake up for.
    #[test]
    fn the_last_call_resolving_disarms_the_spinner() {
        let mut model = spinning_model();
        let mut due = None;
        expire(&mut model, &mut due, Instant::now());
        assert!(due.is_some());

        let _ = update(
            &mut model,
            Msg::Ai(AiEvent::ToolCallUpdate {
                tool_call_id: "call_1".to_string(),
                title: "Run tests".to_string(),
                status: ToolCallStatus::Completed,
                content: None,
            }),
        );
        expire(&mut model, &mut due, Instant::now());

        assert_eq!(due, None);
    }
}
