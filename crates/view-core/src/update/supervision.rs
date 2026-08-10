//! The transitions an engine-liveness reading drives: the sticky banner
//! it raises, and the interrupt/restart modal that reading escalates to.

use crate::model::{Model, OverlayKind};
use crate::msg::Effect;
use crate::native::geometry::OverlayBox;
use crate::native::supervision::{EngineBusyState, SinceStamp, SupervisionChoice, WedgeKind};

/// Folds one supervision reading into the banner and, past the escalation
/// threshold, the interrupt/restart modal.
pub(super) fn note_engine_liveness(
    model: &mut Model,
    wedge: Option<WedgeKind>,
    observed_for: std::time::Duration,
) -> Vec<Effect> {
    let Some(kind) = wedge else {
        let retracted = model.engine.messages.set_native_condition(None);
        let closed = model.close_engine_busy();
        model.supervision.forget_episode();
        if retracted || closed {
            model.dirty = true;
        }
        return Vec::new();
    };
    // re-asserted on every reading rather than raised once on the way in:
    // `msg_clear` empties the log wholesale, and a notice raised only on the
    // transition would be gone for good while its condition is still true
    if model
        .engine
        .messages
        .set_native_condition(Some(kind.notice()))
    {
        model.dirty = true;
    }
    // ahead of every escalation rule below, and answering none of them: a
    // connection that has closed has exactly one recovery, the user has
    // already said whether they want to be asked for it, and asking anyway
    // would leave a modal on screen while the engine it offers to replace
    // is already being replaced
    if kind == WedgeKind::Dead && model.supervision.recovers_unattended() {
        model.supervision.note_unattended_recovery();
        if model.close_engine_busy() {
            model.dirty = true;
        }
        return vec![Effect::RestartEngine];
    }
    let since = SinceStamp::new(observed_for);
    match model.engine_busy_mut() {
        Some(open) if open.kind == kind => {
            if open.since.readout() != since.readout() {
                open.since = since;
                model.dirty = true;
            }
        }
        // a different failure with a different recovery: the open modal is
        // offering choices for a wedge that is no longer the one in front of
        // the user, so it is replaced rather than relabelled
        Some(_) => {
            model.close_engine_busy();
            model.dirty = true;
        }
        None => {}
    }
    if model.engine_busy().is_none()
        && !model.supervision.already_offered(kind)
        && (kind.escalates_immediately() || since.past_modal_threshold())
    {
        model.supervision.note_offered(kind);
        model.push_overlay(
            OverlayBox::new(60, 30),
            OverlayKind::EngineBusy(EngineBusyState::new(kind, since)),
        );
        model.dirty = true;
    }
    Vec::new()
}

/// Folds a keypress into the open interrupt/restart modal's own bookkeeping
/// and returns only the effects that bookkeeping owes.
///
/// The caller goes on to route the very same key, and what that routing
/// delivers is decided by the flow this function's effects produce, not by
/// this function. Two shapes, and the difference is the whole contract:
///
/// - **The keys that only mean something to nvim keep meaning it.** The
///   interrupt is picked by the key it sends
///   ([`INTERRUPT_NOTATION`](crate::native::supervision::INTERRUPT_NOTATION)),
///   so the wire input is byte-identical to the no-modal path and this
///   function's whole contribution is leaving the modal up -- an interrupt
///   reaches an engine whose break check still runs and no other, so a user
///   who sees nothing change still has the modal in front of them rather
///   than having spent their one look at it. The dismissal closes the modal
///   and the `<Esc>` still lands wherever `<Esc>` was going, so nothing has
///   to be retyped. The banner stays: the condition it describes is exactly
///   as true after the dismissal as before it.
/// - **The keys that ask view for something are spent on it.**
///   [`Restart`](crate::native::supervision::SupervisionChoice::Restart) and
///   [`Quit`](crate::native::supervision::SupervisionChoice::Quit) return
///   effects the loop resolves as a non-`Continue` flow, which stops the
///   batch, so the routed key never reaches the wire. That is the correct
///   reading and not a leak: both keys are answered by replacing or ending
///   the very connection the routed copy would have been written to, and
///   both are bound to notations no user presses by reflex
///   (`no_offered_choice_binds_a_key_a_user_could_type_by_reflex`), so there
///   is no keystroke a user meant for the buffer to lose.
///
/// The modal is raised by view noticing a condition, never by the user
/// asking for it, and the condition it announces is very often a long
/// operation that is going to finish -- keystrokes typed at one have always
/// queued and been applied on catch-up, and an annunciator that ate them
/// would turn a slow save into lost work. Which is why the first shape above
/// covers every key a user could plausibly be typing at the editor, and the
/// second covers only the two that are a request to view itself.
pub(super) fn note_supervision_choice(model: &mut Model, notation: &str) -> Vec<Effect> {
    let Some(choice) = model.engine_busy().and_then(|state| state.choose(notation)) else {
        return Vec::new();
    };
    match choice {
        // the interrupt is already on its way as ordinary input; what is
        // recorded here is that it was, so a wedge outliving it can be
        // reported as one the interrupt did not reach
        SupervisionChoice::Interrupt => {
            if let Some(open) = model.engine_busy_mut() {
                let sent = open.since;
                open.note_interrupt(sent);
            }
            Vec::new()
        }
        SupervisionChoice::Restart => {
            model.close_engine_busy();
            model.dirty = true;
            vec![Effect::RestartEngine]
        }
        SupervisionChoice::Dismiss => {
            model.close_engine_busy();
            model.dirty = true;
            Vec::new()
        }
        SupervisionChoice::Quit => {
            model.running = false;
            vec![Effect::Quit {
                exit_code: model.supervision.exit_code(),
            }]
        }
    }
}
