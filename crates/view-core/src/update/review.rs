//! The diff review's own dispatch: the buffer-subscription messages its
//! hunks rebase against, and the verbs that decide them.
//!
//! The verbs arrive as `Msg::FeatureInvoke { feature: "review", .. }` --
//! from the buffer-local mappings `RpcCall::ReviewShow` installs, or from
//! `:View review <verb>` typed by hand. No key reaches here as a key: the
//! review is read and decided in the file itself, so its vocabulary lives
//! in nvim's key stream on that buffer and never in the panel's.

use crate::model::Model;
use crate::msg::{BufferHandle, Effect};
use crate::native::ai_event::AiCommand;
use crate::native::ai_panel::{Refusal, ReviewSync};
use crate::native::diff::BufTextChangedEvent;

/// Folds one reported buffer change into the open review's hunks, and
/// nothing else. Work is O(open hunks) and allocation-free for an edit
/// outside every anchor (see [`crate::native::diff::rebase`]), so this
/// holds the O(edit size) contract `RpcCall::BufAttach` states rather than
/// adding a term in buffer size on top of it.
pub(super) fn on_buf_text_changed(model: &mut Model, change: BufTextChangedEvent) -> Vec<Effect> {
    let target = model.ai_review_open_target;
    model.dirty = true;
    let Some(review) = model.ai_panel_mut().pending_diff.as_mut() else {
        return Vec::new();
    };
    // The repaint gate. Re-issuing the decoration on every reported edit
    // would put one notify on the wire per keystroke in the reviewed
    // buffer, for a screen that in the common case did not change: an
    // extmark shifts with the edit natively, so only a hunk whose *status*
    // moved (or a review that just lost its buffer) has anything new to
    // say. See `DiffReviewState::presentation_stamp`.
    let before = review.presentation_stamp();
    review.apply_change(&change);
    if review.presentation_stamp() == before {
        return Vec::new();
    }
    review.show_effect(false, target).into_iter().collect()
}

/// nvim ended the subscription on its own initiative, so no further edit
/// will ever be reported: the review says so rather than going on offering
/// accepts it can no longer honour.
pub(super) fn on_buf_detached(
    model: &mut Model,
    buf: BufferHandle,
    generation: u64,
) -> Vec<Effect> {
    let target = model.ai_review_open_target;
    model.dirty = true;
    let Some(review) = model.ai_panel_mut().pending_diff.as_mut() else {
        return Vec::new();
    };
    let before = review.presentation_stamp();
    review.note_detached(buf, generation);
    if review.presentation_stamp() == before {
        return Vec::new();
    }
    // Repainted rather than left as it was: every header on screen still
    // offers keys that now answer with a refusal, and the one thing this
    // review can still say is why.
    review.show_effect(false, target).into_iter().collect()
}

/// nvim answered which buffer the proposal's path names (creating a hidden
/// one when none existed); a successful bind answers the attach that
/// follows it. `created` is diagnostic only (see [`crate::msg::RpcCall::LoadHidden`]'s
/// own doc) and plays no part in this fold.
pub(super) fn on_hidden_buffer_loaded(
    model: &mut Model,
    generation: u64,
    buf: Option<BufferHandle>,
    changedtick: u64,
) -> Vec<Effect> {
    let target = model.ai_review_open_target;
    model.dirty = true;
    let Some(review) = model.ai_panel_mut().pending_diff.as_mut() else {
        return Vec::new();
    };
    let mut effects = review.bind(generation, buf, changedtick);
    if review.sync == ReviewSync::Unbindable {
        // The one case inline review cannot serve: nvim gave the path no
        // buffer, so there is nowhere to draw the diff and nothing to
        // write it into. Said once in the transcript and discarded --
        // holding a review that can never be acted on open would leave the
        // user a decision with no way to make it.
        return unbindable(model);
    }
    // The review comes to the user: the buffer is bound, so this is the
    // moment the file, the decision and the keys that answer it are all in
    // one place.
    effects.extend(review.show_effect(true, target));
    take_panel_focus_off(model);
    effects
}

/// Announces a review nvim could give no buffer for, discards it, and
/// opens whatever was queued behind it.
fn unbindable(model: &mut Model) -> Vec<Effect> {
    let panel = model.ai_panel_mut();
    let Some(review) = panel.pending_diff.take() else {
        return Vec::new();
    };
    let notice = format!(
        "{} could not be opened -- {}",
        review.path.display(),
        ReviewSync::Unbindable.notice().unwrap_or_default()
    );
    panel.transcript.record_review(notice);
    let mut effects: Vec<Effect> = review
        .close_effects()
        .into_iter()
        .chain(Some(Effect::Ai(AiCommand::DiscardProposal {
            request_id: review.request_id,
        })))
        .collect();
    effects.extend(promote_queued(model));
    effects
}

/// Hands the keyboard back to the buffer the review is now drawn in. A
/// panel still holding focus would swallow the very keys the review just
/// installed there.
///
/// Refused while something else in the panel is still holding those keys:
/// an unanswered permission request has an agent's turn blocked behind it,
/// and a review the user can come back to at any time does not outrank a
/// question that is stopping work. The answer path calls this again the
/// moment the question is settled.
pub(super) fn take_panel_focus_off(model: &mut Model) {
    if model.ai_panel().focused && !model.ai_panel().an_owner_holds_the_keys() {
        model.ai_panel_mut().focused = false;
        model.dirty = true;
    }
}

/// nvim applied a write. The tick it produced is what the next write
/// names, so the accept after this one is not a race with the edit event
/// still on its way here.
pub(super) fn on_buf_write_applied(
    model: &mut Model,
    buf: BufferHandle,
    generation: u64,
    changedtick: u64,
) -> Vec<Effect> {
    if let Some(review) = model.ai_panel_mut().pending_diff.as_mut() {
        review.note_write_applied(buf, generation, changedtick);
    }
    Vec::new()
}

/// nvim refused a write because the buffer had moved past the tick the
/// review named. Nothing was written, so the hunks that write claimed go
/// back to being decisions the user owes.
pub(super) fn on_buf_write_refused(
    model: &mut Model,
    buf: BufferHandle,
    generation: u64,
) -> Vec<Effect> {
    let target = model.ai_review_open_target;
    model.dirty = true;
    let Some(review) = model.ai_panel_mut().pending_diff.as_mut() else {
        return Vec::new();
    };
    let before = review.presentation_stamp();
    review.note_write_refused(buf, generation);
    if review.presentation_stamp() == before {
        return Vec::new();
    }
    // Hunks that were painted as decided are decisions the user owes
    // again, so the buffer has to show them again.
    review.show_effect(false, target).into_iter().collect()
}

/// One `Msg::FeatureInvoke { feature: "review", verb }`.
///
/// A refusal is answered here, through the same dispatch every verb goes
/// through, rather than by the verb simply not being offered: the state is
/// what refuses, and a future affordance that wanted an "accept anyway"
/// would have to change that refusal rather than route around this arm.
/// Every verb that can refuse says so out loud -- a verb the user reached
/// for through a key the header was offering, answered with an identical
/// screen and nothing else, reads as view having dropped the keystroke.
///
/// A verb naming no review is inert: every key that produces one is
/// installed on the reviewed buffer and removed with it, so the only way to
/// reach that case is `:View review accept` typed with nothing under
/// review, where nothing happening is the honest answer.
///
/// A verb the review does not know is different, and is answered: `:View
/// review <Tab>` offers these words, so a typo in one of them is a user
/// asking for something view advertised. It changes nothing else -- no
/// status, no cursor, no repaint, not even the dirty flag -- because there
/// is nothing about the review it could have changed.
pub(super) fn review_verb(model: &mut Model, verb: &str) -> Vec<Effect> {
    if !VERBS.contains(&verb) {
        return model
            .engine
            .record_native_notice(refusal_notice(Refusal::UnknownVerb), false);
    }
    let target = model.ai_review_open_target;
    let Some(review) = model.ai_panel_mut().pending_diff.as_mut() else {
        return Vec::new();
    };
    let index = review.cursor;
    // True for the verbs that move the user rather than change a status:
    // the cursor goes where the review says, so the show that follows has
    // to take the cursor with it.
    let mut navigated = false;
    let decided = match verb {
        ACCEPT => review.accept(index),
        ACCEPT_ALL => review.accept_all(),
        REJECT => review.reject(index).map(|()| Vec::new()),
        REJECT_ALL => review.reject_all().map(|()| Vec::new()),
        REDIFF => review.re_diff(index).map(|()| Vec::new()),
        NEXT => {
            review.next_hunk();
            navigated = true;
            Ok(Vec::new())
        }
        PREV => {
            review.prev_hunk();
            navigated = true;
            Ok(Vec::new())
        }
        LEAVE => {
            let closing = review.close_effects();
            let outcome = review.outcome();
            // Abandoned with decisions still owed, so the session forgets
            // the proposal was ever raised: the user dismissed it unread,
            // and an agent restating the same diff later must reach them
            // again rather than be deduplicated against this one.
            let abandoned = review
                .is_open()
                .then_some(Effect::Ai(AiCommand::DiscardProposal {
                    request_id: review.request_id,
                }));
            let panel = model.ai_panel_mut();
            panel.pending_diff = None;
            panel.transcript.record_review(outcome);
            model.dirty = true;
            let mut effects: Vec<Effect> = closing.into_iter().chain(abandoned).collect();
            effects.extend(promote_queued(model));
            return effects;
        }
        // unreachable: `VERBS` is checked above, and this match answers
        // every word in it
        _ => return Vec::new(),
    };
    // Every verb answers, one way or the other: a refusal is carried out
    // of the match rather than dropped, because the key that produced it
    // was on screen a moment ago and an identical repaint is not an answer
    // to it.
    let (mut effects, refusal) = match decided {
        Ok(effects) => (effects, None),
        Err(why) => (Vec::new(), Some(why)),
    };
    model.dirty = true;
    // The cursor follows the work: once the hunk it names is decided,
    // there is nothing left to act on there and the next decision should
    // already be on screen.
    if let Some(review) = model.ai_panel_mut().pending_diff.as_mut() {
        if review
            .hunks
            .get(review.cursor)
            .is_some_and(|hunk| !hunk.status.is_open())
        {
            review.next_hunk();
        }
    }
    // A review with nothing left to decide ends itself, subscription and
    // all: every further edit in that buffer would otherwise be reported
    // to a review that has no hunk left to rebase, one message per
    // keystroke for no decision.
    if model
        .ai_panel()
        .pending_diff
        .as_ref()
        .is_some_and(|review| !review.is_open())
    {
        let panel = model.ai_panel_mut();
        if let Some(finished) = panel.pending_diff.take() {
            effects.extend(finished.close_effects());
            panel.transcript.record_review(finished.outcome());
        }
        effects.extend(promote_queued(model));
    } else if let Some(review) = model.ai_panel().pending_diff.as_ref() {
        // Still open, so what is drawn in the buffer no longer matches
        // what the review holds. `focus` follows the gesture: a jump is
        // the user asking to be taken to the next decision, a decision is
        // not, and repainting under a reader who has scrolled elsewhere
        // must not move them.
        effects.extend(review.show_effect(navigated, target));
        if navigated {
            take_panel_focus_off(model);
        }
    }
    // The notice joins the effects rather than replacing them: the block
    // above can have produced the detach that ends a review with nothing
    // left to decide, and dropping it here would leave nvim reporting
    // every keystroke in that buffer to a review that is gone.
    if let Some(why) = refusal {
        effects.extend(
            model
                .engine
                .record_native_notice(refusal_notice(why), false),
        );
    }
    effects
}

/// Accept the hunk the review's cursor is on.
pub(super) const ACCEPT: &str = "accept";
/// Accept every hunk still fresh, as one write.
pub(super) const ACCEPT_ALL: &str = "accept_all";
/// Decline the hunk the review's cursor is on.
pub(super) const REJECT: &str = "reject";
/// Decline every hunk still open.
pub(super) const REJECT_ALL: &str = "reject_all";
/// Re-anchor the cursor's hunk against the text the buffer now holds.
pub(super) const REDIFF: &str = "rediff";
/// Move to the next hunk still awaiting a decision.
pub(super) const NEXT: &str = "next";
/// Move to the previous one.
pub(super) const PREV: &str = "prev";
/// Close the review, deciding nothing further.
pub(super) const LEAVE: &str = "leave";

/// Every verb this dispatch answers, in the order `:View review <Tab>`
/// offers them. The list a word is checked against before any state is
/// touched, so an unknown one is told so rather than silently doing
/// nothing.
pub(super) const VERBS: [&str; 8] = [
    ACCEPT, ACCEPT_ALL, REJECT, REJECT_ALL, REDIFF, NEXT, PREV, LEAVE,
];

/// Opens the proposal that was waiting behind the review that just ended,
/// if there is one, and says in the transcript that it has the floor now.
///
/// A queued proposal binds here rather than when it arrived: nothing is
/// subscribed to a buffer whose review is not the one on screen. It
/// announces itself because the review it was waiting behind has just
/// disappeared out of the buffer the user was looking at, and the next one
/// decorates a different file -- without a line saying so, the jump reads
/// as view having moved the cursor for no reason.
fn promote_queued(model: &mut Model) -> Vec<Effect> {
    let panel = model.ai_panel_mut();
    if panel.pending_diff.is_some() {
        return Vec::new();
    }
    let Some(queued) = panel.pending_diff_next.take() else {
        return Vec::new();
    };
    let effects = vec![queued.bind_effect()];
    panel
        .transcript
        .record_review(format!("now reviewing {}", queued.path.display()));
    panel.pending_diff = Some(queued);
    model.dirty = true;
    effects
}

/// What the user is told when a verb is refused. Names the state that
/// refused it and the way forward, never just "cannot".
///
/// One formatter for every verb's refusal, on the same reasoning
/// [`crate::native::ai_panel::Refusal`] is one enum: two refusals worded as
/// if they were the same fact is how a user comes to press a key twice.
fn refusal_notice(why: Refusal) -> String {
    match why {
        Refusal::NotLive => "This review's buffer can no longer be written -- re-open the review",
        Refusal::NotFresh => {
            "That hunk is not fresh -- re-diff it (<leader>hR) or reject it (<leader>hx) first"
        }
        Refusal::NothingFresh => {
            "No hunk here is fresh -- re-diff (<leader>hR) or reject (<leader>hx) what the \
             buffer moved under"
        }
        Refusal::NotOpen => "That hunk is decided already -- ]c moves to the next one still open",
        Refusal::NotStale => "That hunk is not stale, so there is nothing to re-diff",
        Refusal::AnchorLost => {
            "Your own edits took this hunk's anchor with them -- reject it (<leader>hx) or \
             leave the review (<leader>hq)"
        }
        Refusal::UnknownVerb => {
            "The review has no such verb -- :View review <Tab> offers the ones it answers"
        }
    }
    .to_string()
}
