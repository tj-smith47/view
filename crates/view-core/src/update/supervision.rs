//! The transitions an engine-liveness reading drives: the sticky banner it
//! raises, the interrupt/restart modal that reading escalates to, and what a
//! replacement engine says about the work it recovered on the way up.

use crate::model::{Model, OverlayKind};
use crate::msg::{Effect, RpcCall};
use crate::native::geometry::OverlayBox;
use crate::native::supervision::{
    swap_recovery_failure_notice, swap_recovery_notice, EngineBusyState, ReconnectProgress,
    SinceStamp, SupervisionChoice, WedgeKind,
};

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
    // a closed connection the runtime is already reconnecting reports the
    // attempt it is on instead of the bare closure: the wording is the
    // reconnect's, the condition is the same one, and there is still only
    // the single condition notice
    let reconnect = model
        .supervision
        .reconnect()
        .filter(|_| kind == WedgeKind::Dead);
    let counted = reconnect.and_then(ReconnectProgress::notice);
    // re-asserted on every reading rather than raised once on the way in:
    // `msg_clear` empties the log wholesale, and a notice raised only on the
    // transition would be gone for good while its condition is still true
    if model
        .engine
        .messages
        .set_native_condition(Some(counted.as_deref().unwrap_or(kind.notice())))
    {
        model.dirty = true;
    }
    // an attempt is already owed and already waiting out its backoff:
    // asking for a second one here would spend the session's whole
    // unattended-recovery budget on one outage, and would do it at the
    // cadence the readout repaints on
    if counted.is_some() {
        return Vec::new();
    }
    // ahead of every escalation rule below, and answering none of them: a
    // connection that has closed has exactly one recovery, the user has
    // already said whether they want to be asked for it, and asking anyway
    // would leave a modal on screen while the engine it offers to replace
    // is already being replaced. A sequence that has run out of attempts is
    // past that: what it has evidence of is a recovery that is not working,
    // so the choice goes back to the user through the modal below
    if kind == WedgeKind::Dead && reconnect.is_none() && model.supervision.recovers_unattended() {
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
    // the one-offer rule belongs to the wedges a user can put away: it keeps
    // a dismissed annunciator dismissed. A dead connection has no dismissal,
    // so a modal that is off screen while the connection is still closed was
    // answered by a restart that did not bring an engine back, and the same
    // question is owed again rather than swallowed
    let answered_already = kind.dismissible() && model.supervision.already_offered(kind);
    if model.engine_busy().is_none()
        && !answered_already
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

/// Folds one connection's swap-recovery reading into what the user is told
/// and what the screen is asked to do about it.
///
/// Three outcomes, and the difference between them is the user's file.
///
/// - **Nothing recovered, nothing reported.** The ordinary session. Silent,
///   and no redraw: a full repaint issued for a startup that went perfectly
///   discards every reuse the grid's own damage tracking earns.
/// - **Recovered.** nvim's multi-line report is view's own overlay by now
///   (`ext_messages` puts no message in the grid), and only nvim can say it
///   is over -- the `msg_clear` [`RpcCall::Redraw`] answers with is what
///   takes it off the buffer, leaving the notice, which `Messages::clear`
///   keeps, as the account of it.
/// - **Asked for and failed.** The buffer is empty where the file's contents
///   should be, and the engine's error is the only thing on screen saying
///   why. view did not put that error there and does not redraw it away:
///   clearing an error view cannot attribute would leave the user an empty
///   buffer with no account of itself, one `:w` away from truncating the
///   file. The notice names the state and carries the error with it.
///
/// The failure branch is the one reachable before its connection has
/// finished starting -- a startup error parks nvim ahead of `VimEnter` -- so
/// it is deduplicated by the failure itself rather than by the generation:
/// the second reading of the same connection must not say it twice.
pub(super) fn note_swap_recovery(
    model: &mut Model,
    generation: u64,
    count: u64,
    reported: bool,
    failure: Option<String>,
) -> Vec<Effect> {
    // a restart hands the replacement engine's pump the sink the dead one
    // wrote into, so a reading that crossed before the cutover can arrive
    // after it, speaking for an engine that is gone -- and one connection is
    // asked twice, so its earlier answer is superseded by the later question
    if generation != model.supervision.swap_probe_generation() {
        return Vec::new();
    }
    if let Some(error) = failure {
        if !model.supervision.note_swap_failure(&error) {
            return Vec::new();
        }
        model.dirty = true;
        return model
            .engine
            .record_native_notice(swap_recovery_failure_notice(&error), false);
    }
    if !reported {
        return Vec::new();
    }
    let mut effects = match swap_recovery_notice(count) {
        Some(notice) => {
            model.dirty = true;
            model.engine.record_native_notice(notice, false)
        }
        // a recovery that replayed a swap holding nothing the file on disk
        // did not already have took nothing back, so there is nothing to
        // tell the user -- but nvim reported it all the same, and that
        // report is still over their buffer
        None => Vec::new(),
    };
    effects.push(Effect::Rpc(RpcCall::Redraw));
    effects
}
