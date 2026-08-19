//! The diff review's own dispatch: the buffer-subscription messages its
//! hunks rebase against, and the keys that decide them.

use crate::model::Model;
use crate::msg::{BufferHandle, Effect};
use crate::native::ai_panel::AcceptRefusal;
use crate::native::diff::BufTextChangedEvent;

/// Folds one reported buffer change into the open review's hunks, and
/// nothing else. Work is O(open hunks) and allocation-free for an edit
/// outside every anchor (see [`crate::native::diff::rebase`]), so this
/// holds the O(edit size) contract `RpcCall::BufAttach` states rather than
/// adding a term in buffer size on top of it.
pub(super) fn on_buf_text_changed(model: &mut Model, change: BufTextChangedEvent) -> Vec<Effect> {
    let Some(review) = model.ai_panel_mut().pending_diff.as_mut() else {
        return Vec::new();
    };
    review.apply_change(&change);
    model.dirty = true;
    Vec::new()
}

/// nvim ended the subscription on its own initiative, so no further edit
/// will ever be reported: the review says so rather than going on offering
/// accepts it can no longer honour.
pub(super) fn on_buf_detached(
    model: &mut Model,
    buf: BufferHandle,
    generation: u64,
) -> Vec<Effect> {
    let Some(review) = model.ai_panel_mut().pending_diff.as_mut() else {
        return Vec::new();
    };
    review.note_detached(buf, generation);
    model.dirty = true;
    Vec::new()
}

/// nvim answered which buffer the proposal's path names; a successful bind
/// answers the attach that follows it.
pub(super) fn on_buf_resolved(
    model: &mut Model,
    generation: u64,
    buf: Option<BufferHandle>,
) -> Vec<Effect> {
    let Some(review) = model.ai_panel_mut().pending_diff.as_mut() else {
        return Vec::new();
    };
    let effects = review.bind(generation, buf);
    model.dirty = true;
    effects
}

/// One printable key inside an open diff review.
///
/// The accept refusal is answered here, through the same dispatch every
/// accept goes through, rather than by the key simply not being offered:
/// `DiffReviewState::accept` is what refuses, and a future affordance that
/// wanted an "accept anyway" would have to change that refusal rather than
/// route around this arm.
pub(super) fn review_key(model: &mut Model, notation: &str) -> Vec<Effect> {
    model.dirty = true;
    let Some(review) = model.ai_panel_mut().pending_diff.as_mut() else {
        return Vec::new();
    };
    let index = review.cursor;
    let mut refusal = None;
    let effects = match notation {
        "a" => match review.accept(index) {
            Ok(effects) => effects,
            Err(why) => {
                refusal = Some(why);
                Vec::new()
            }
        },
        "A" => review.accept_all(),
        "x" => {
            review.reject(index);
            Vec::new()
        }
        "R" => {
            review.re_diff(index);
            Vec::new()
        }
        "]" => {
            review.next_hunk();
            Vec::new()
        }
        "[" => {
            review.prev_hunk();
            Vec::new()
        }
        "q" => {
            let closing = review.close_effect();
            model.ai_panel_mut().pending_diff = None;
            return closing.into_iter().collect();
        }
        // Every other printable is swallowed, the same way an unmatched key
        // on a pending permission prompt is: a review is a decision, and
        // leaking its stray keys to nvim would edit the very buffer under
        // review.
        _ => return Vec::new(),
    };
    let mut effects = effects;
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
            effects.extend(finished.close_effect());
        }
    }
    match refusal {
        Some(why) => model
            .engine
            .record_native_notice(refusal_notice(why), false),
        None => effects,
    }
}

/// What the user is told when an accept is refused. Names the state that
/// refused it and the way forward, never just "cannot".
fn refusal_notice(why: AcceptRefusal) -> String {
    match why {
        AcceptRefusal::NotLive => {
            "This review's buffer can no longer be written -- re-open the review".to_string()
        }
        AcceptRefusal::NotFresh => {
            "That hunk is not fresh -- re-diff it (R) or reject it (x) first".to_string()
        }
    }
}
