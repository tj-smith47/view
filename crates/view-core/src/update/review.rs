//! The diff review's own dispatch: the buffer-subscription messages its
//! hunks rebase against, and the keys that decide them.

use crate::model::Model;
use crate::msg::{BufferHandle, Effect};
use crate::native::ai_event::AiCommand;
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
    let Some(review) = model.ai_panel_mut().pending_diff.as_mut() else {
        return Vec::new();
    };
    let effects = review.bind(generation, buf, changedtick);
    model.dirty = true;
    effects
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
    let Some(review) = model.ai_panel_mut().pending_diff.as_mut() else {
        return Vec::new();
    };
    review.note_write_refused(buf, generation);
    model.dirty = true;
    Vec::new()
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
    // Set by the arm that owns no review state and surfaced after the
    // borrow above ends, the same way `refusal` is.
    let mut stray = false;
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
            let closing = review.close_effects();
            // Abandoned with decisions still owed, so the session forgets
            // the proposal was ever raised: the user dismissed it unread,
            // and an agent restating the same diff later must reach them
            // again rather than be deduplicated against this one.
            let abandoned = review
                .is_open()
                .then_some(Effect::Ai(AiCommand::DiscardProposal {
                    request_id: review.request_id,
                }));
            model.ai_panel_mut().pending_diff = None;
            let mut effects: Vec<Effect> = closing.into_iter().chain(abandoned).collect();
            effects.extend(promote_queued(model));
            return effects;
        }
        // Every other printable stays out of nvim, the same way an
        // unmatched key on a pending permission prompt does: a review is a
        // decision, and leaking its stray keys to the engine would edit the
        // very buffer under review. Swallowing it in silence is the part
        // that misleads -- a prompt typed at an unanswered review produces
        // no echo, no refusal and no agent turn, which reads as a dead
        // panel rather than as keys that belong to a decision. Delivering
        // it to the composer instead is not available: these keys are the
        // review's own vocabulary, and `a` cannot both accept a hunk and
        // type an `a`.
        _ => {
            stray = true;
            Vec::new()
        }
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
            effects.extend(finished.close_effects());
        }
        effects.extend(promote_queued(model));
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
    // Once per standing notice rather than once per key: a sentence typed
    // at an open review is one mistake, and a line per character would
    // bury the answer in the ring behind `:messages`.
    if stray {
        effects.extend(
            model
                .engine
                .record_native_notice_once(STRAY_KEY_FAMILY, STRAY_KEY_NOTICE.to_string()),
        );
    }
    effects
}

/// The opening [`STRAY_KEY_NOTICE`] is deduplicated on.
const STRAY_KEY_FAMILY: &str = "A review is open";

/// What an unmapped key inside a review answers with: the state the panel
/// is in, and both ways out of it. Names the keys rather than pointing at
/// the hint row, since the reader has just demonstrated they were not
/// reading it.
pub(super) const STRAY_KEY_NOTICE: &str =
    "A review is open and owns these keys -- decide it (a accept, x reject, R re-diff) \
     or close it (q) before typing";

/// Opens the proposal that was waiting behind the review that just ended,
/// if there is one. A queued proposal binds here rather than when it
/// arrived: nothing is subscribed to a buffer whose review is not the one
/// on screen.
fn promote_queued(model: &mut Model) -> Vec<Effect> {
    let panel = model.ai_panel_mut();
    if panel.pending_diff.is_some() {
        return Vec::new();
    }
    let Some(queued) = panel.pending_diff_next.take() else {
        return Vec::new();
    };
    let effects = vec![queued.bind_effect()];
    panel.pending_diff = Some(queued);
    effects
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
