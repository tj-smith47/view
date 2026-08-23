//! The agent panel's diff review: the hunks one proposal opened, which one
//! the user is looking at, and what the buffer underneath them is still
//! able to tell this session.
//!
//! A state inside the panel rather than a second top-level overlay: a
//! proposed diff is always the direct consequence of an in-flight agent
//! turn, so it opens as part of the panel's own flow and closes with it.
//! The review is *read* in the file itself -- [`DiffReviewState::marks`]
//! is the whole of its presentation, drawn by nvim as extmarks over the
//! real rows through [`crate::msg::RpcCall::ReviewShow`] -- and the panel
//! keeps only the one summary row that says which file and how far in.
//!
//! Nothing here writes: an accept produces the
//! [`crate::msg::RpcCall::BufSetText`] call that does, and nvim remains the
//! sole owner of every byte in the buffer. The marks are presentation and
//! nothing else -- no view-side shadow of buffer text ever exists, and an
//! extmark shifts with the user's own edits without this state hearing
//! about it.

use std::path::PathBuf;

use super::super::diff::{rebase, BufTextChangedEvent, Hunk, HunkStatus};
use super::super::views::Span;
use crate::msg::{BufferHandle, Effect, HunkMark, ReviewOpenTarget, RpcCall};

/// What the review's buffer is still able to tell this session.
///
/// Separate from each hunk's own [`HunkStatus`] because it answers a
/// different question: a hunk's status says whether *that change* is still
/// anchored, this says whether the *review* can trust anything it holds. A
/// review whose event stream broke has stale hunks, but it also has no way
/// to un-stale them, which is a different offer to make the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewSync {
    /// The resolve for the proposal's path is in flight; no buffer is bound
    /// yet, so nothing may be written.
    Binding,
    /// Bound and attached: edits arrive and hunks rebase against them.
    Live,
    /// The engine could not give the path a buffer handle at all. Terminal:
    /// there is nowhere to write and nothing to attach to.
    Unbindable,
    /// A `BufTextChanged` arrived marked desynced, so an edit for this
    /// buffer was dropped or failed to decode and the hunk geometry here
    /// describes a buffer version that no longer exists. Terminal for this
    /// review: re-diffing would narrow against text that is partly a guess.
    Desynced,
    /// nvim ended the subscription itself (a `:edit!` reload, a wipeout).
    /// Terminal for the same reason as [`Self::Desynced`], plus the harder
    /// one that no further event will ever arrive.
    Detached,
}

impl ReviewSync {
    /// Whether the review may still write to its buffer. False for every
    /// state but [`Self::Live`] -- an accept needs both a bound buffer and
    /// a trustworthy account of where its rows are.
    #[must_use]
    pub fn can_apply(self) -> bool {
        matches!(self, Self::Live)
    }

    /// The reason line shown beside the review's own summary, or `None`
    /// while nothing is wrong.
    #[must_use]
    pub fn notice(self) -> Option<&'static str> {
        match self {
            Self::Binding => Some("resolving the file's buffer"),
            Self::Live => None,
            Self::Unbindable => Some("no buffer for this path -- nothing can be applied"),
            Self::Desynced => Some("lost track of this buffer's edits -- re-open the review"),
            Self::Detached => Some("this buffer's edit stream ended -- re-open the review"),
        }
    }
}

/// Why an accept was refused. Returned rather than silently ignored so the
/// panel can say which of the two refusals happened, and so a test can
/// assert the refusal came from the dispatch path rather than from the UI
/// merely not offering the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptRefusal {
    /// The review's buffer is not (or no longer) writable: see
    /// [`ReviewSync::can_apply`].
    NotLive,
    /// The hunk is not [`HunkStatus::Fresh`]. A stale hunk's byte columns
    /// name rows that have changed underneath it, and a resolved one has
    /// already been decided.
    NotFresh,
}

/// One agent proposal under review.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffReviewState {
    /// The `request_id` the proposal arrived under, so a second proposal
    /// for the same path is a different review rather than a mutation of
    /// this one.
    pub request_id: u64,
    /// The file the agent proposed changes to.
    pub path: PathBuf,
    /// Stamped on this review's own `LoadHidden`/`BufAttach` and matched
    /// against every reply, on the `PickerState::generation` precedent: a
    /// reply for a review a later proposal has superseded must never bind
    /// its buffer onto the newer one.
    pub generation: u64,
    /// The buffer nvim holds [`Self::path`] under, once resolved.
    pub buffer: Option<BufferHandle>,
    /// The proposal's hunks, top of the buffer first.
    pub hunks: Vec<Hunk>,
    /// Which hunk the user is on. The panel's scroll window follows
    /// this and nothing else -- hunk-jump is the navigation, so the review
    /// never scrolls over buffer regions no hunk touches.
    pub cursor: usize,
    pub sync: ReviewSync,
    /// Whether this review has already written a hunk. What decides the
    /// next accept's `undojoin`: the first write of a review opens its own
    /// undo entry and every later one joins onto it, per
    /// [`RpcCall::BufSetText`]'s own per-hunk undo contract, so one `u`
    /// retracts the review the user decided rather than one hunk of it
    /// (proven live in `crates/view-engine/tests/diff_review_undo_live.rs`).
    ///
    /// One case does not compose, and it is nvim's, not this state's: an
    /// accept issued when the user's immediately preceding action was an
    /// undo cannot join, because `:undojoin` throws `E790` there and the
    /// engine's documented fallback is to apply the batch un-joined rather
    /// than drop an accepted hunk (see [`RpcCall::BufSetText`] and
    /// `undojoin_true_after_an_undo_falls_back_to_applying_unjoined`). The
    /// hunks accepted before that point stay in their own entry, so the
    /// review then undoes in two steps instead of one. That is the honest
    /// outcome of the user having undone mid-review -- their `u` is what
    /// broke the chain -- and it is why the accept path never treats an
    /// un-joined apply as a failure: the alternative is refusing an accept
    /// the user just made.
    pub written: bool,
    /// The buffer's `b:changedtick` as this review last saw it -- from the
    /// resolve that bound it, then from every edit event folded since.
    ///
    /// Stamped on every write so nvim refuses one computed against text the
    /// buffer no longer holds. It is deliberately the last *seen* tick and
    /// not a re-read: the point is to name the version whose rows these
    /// hunks were computed against, so an edit this review has not folded
    /// yet is exactly what must make the write fail.
    pub changedtick: u64,
    /// The hunks of the write currently on the wire, and what
    /// [`Self::written`] was before it -- everything needed to put the
    /// review back the way it was if nvim refuses that write.
    pub in_flight: Option<InFlight>,
}

/// One issued [`RpcCall::BufSetText`]'s undo state, kept until nvim either
/// applies it (the edit event lands) or refuses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFlight {
    /// The hunks this write marked [`HunkStatus::Accepted`].
    pub hunks: Vec<usize>,
    /// [`DiffReviewState::written`] before the write. Restored on a
    /// refusal: a write that never applied must not leave the next one
    /// joining its undo entry onto whatever the user's own last edit was.
    pub written_before: bool,
}

impl DiffReviewState {
    /// A review of `hunks` against `path`, not yet bound to a buffer.
    #[must_use]
    pub fn new(request_id: u64, path: PathBuf, generation: u64, hunks: Vec<Hunk>) -> Self {
        Self {
            request_id,
            path,
            generation,
            buffer: None,
            hunks,
            cursor: 0,
            sync: ReviewSync::Binding,
            written: false,
            changedtick: 0,
            in_flight: None,
        }
    }

    /// The effect that starts the binding: nvim owns buffer identity, so
    /// the path the agent named has to be resolved there before anything
    /// can attach to it or write into it. Also acquires this review's one
    /// hidden-buffer hold on `path` -- released exactly once, by
    /// [`Self::close_effects`], whichever way this review ends. A path with
    /// a window already open resolves onto that same buffer (see
    /// `docs/hidden-buffer-wire-capture.md`); nothing here or in
    /// `close_effects` needs to know which case it is, since
    /// `RpcCall::ReleaseHidden`'s decrement-to-zero delete is guarded on the
    /// engine side, not by nvim itself: a buffer any window still shows, or
    /// one this connection never created, is never deleted regardless of
    /// what this review's own count reaches.
    #[must_use]
    pub fn bind_effect(&self) -> Effect {
        Effect::Rpc(RpcCall::LoadHidden {
            path: self.path.to_string_lossy().into_owned(),
            generation: self.generation,
        })
    }

    /// Folds one `Msg::HiddenBufferLoaded` in, returning the attach that
    /// follows a successful bind. A reply whose generation is not this
    /// review's is dropped and answers no effects.
    #[must_use]
    pub fn bind(
        &mut self,
        generation: u64,
        buf: Option<BufferHandle>,
        changedtick: u64,
    ) -> Vec<Effect> {
        if generation != self.generation || self.sync != ReviewSync::Binding {
            return Vec::new();
        }
        let Some(buf) = buf else {
            self.sync = ReviewSync::Unbindable;
            return Vec::new();
        };
        self.buffer = Some(buf);
        self.changedtick = changedtick;
        self.sync = ReviewSync::Live;
        vec![Effect::Rpc(RpcCall::BufAttach {
            buf,
            generation: self.generation,
        })]
    }

    /// The effects that end this review: the detach that stops its
    /// subscription (when one is still live) and the release of the one
    /// hidden-buffer hold [`Self::bind_effect`] acquired. Called exactly
    /// once per review, by whichever of the two paths ends it -- an
    /// explicit abandon (`q`) or every hunk reaching a final status -- so
    /// the hold's acquire/release pair stays 1:1 regardless of which path
    /// ends it or how many hunks were ever accepted.
    ///
    /// The release fires even for a review that never bound (`self.buffer`
    /// is `None`): `bind_effect` and this are the one acquire/release pair
    /// per review, so a review abandoned before its resolve ever answered
    /// still owes the release its bind already sent.
    #[must_use]
    pub fn close_effects(&self) -> Vec<Effect> {
        let detach = self
            .buffer
            .filter(|_| matches!(self.sync, ReviewSync::Live | ReviewSync::Desynced));
        let mut effects = Vec::with_capacity(3);
        // Ahead of the detach, and for a detached review too: the
        // decoration outlives the subscription that maintained it (nvim
        // ends one on a `:edit!` reload, which leaves the buffer and this
        // namespace on it very much alive), and marks left behind would
        // offer keys no review answers.
        if let Some(buf) = self.buffer {
            effects.push(Effect::Rpc(RpcCall::ReviewClear { buf }));
        }
        if let Some(buf) = detach {
            effects.push(Effect::Rpc(RpcCall::BufDetach { buf }));
        }
        effects.push(Effect::Rpc(RpcCall::ReleaseHidden {
            path: self.path.to_string_lossy().into_owned(),
        }));
        effects
    }

    /// Folds one buffer change into every hunk. A desynced event also
    /// retires the review itself, not only its hunks: nothing left in this
    /// session can re-anchor them, so the state has to say so rather than
    /// leave a re-diff key that would narrow against a guess.
    pub fn apply_change(&mut self, change: &BufTextChangedEvent) {
        if self.buffer != Some(change.buf) || change.generation != self.generation {
            return;
        }
        rebase(&mut self.hunks, change);
        self.changedtick = change.changedtick;
        // The edit that lands after a write is that write's own
        // confirmation: nothing is left to roll back once the buffer has
        // moved to a tick this review has seen.
        self.in_flight = None;
        if change.desynced {
            self.sync = ReviewSync::Desynced;
        }
    }

    /// Folds one `Msg::BufDetached` in: the subscription died on nvim's own
    /// initiative, so no further edit will ever be reported and every open
    /// hunk is un-anchorable from here on.
    pub fn note_detached(&mut self, buf: BufferHandle, generation: u64) {
        if self.buffer != Some(buf) || generation != self.generation {
            return;
        }
        crate::native::diff::rebase::stale_all(&mut self.hunks);
        self.sync = ReviewSync::Detached;
    }

    /// Whether any hunk still awaits a decision.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.hunks.iter().any(|hunk| hunk.status.is_open())
    }

    /// Moves the cursor to the next still-open hunk, wrapping. The review's
    /// primary navigation: the scroll window follows the cursor, so this is
    /// what puts the next decision on screen without ever scrolling across
    /// buffer regions the proposal does not touch.
    pub fn next_hunk(&mut self) {
        self.jump(true);
    }

    /// [`Self::next_hunk`]'s counterpart.
    pub fn prev_hunk(&mut self) {
        self.jump(false);
    }

    fn jump(&mut self, forward: bool) {
        let len = self.hunks.len();
        if len == 0 {
            return;
        }
        for step in 1..=len {
            let offset = if forward { step } else { len - step };
            let candidate = (self.cursor + offset) % len;
            if self.hunks[candidate].status.is_open() {
                self.cursor = candidate;
                return;
            }
        }
    }

    /// Accepts the hunk the cursor is on, producing the one
    /// [`RpcCall::BufSetText`] that writes it.
    ///
    /// Refused for anything but a [`HunkStatus::Fresh`] hunk on a
    /// [`ReviewSync::Live`] review -- the refusal is here, in the state
    /// every dispatch path goes through, rather than in whichever keys the
    /// UI happens to offer, so an "accept anyway" affordance added later
    /// would have to change this and its test rather than route around it.
    pub fn accept(&mut self, index: usize) -> Result<Vec<Effect>, AcceptRefusal> {
        if !self.sync.can_apply() {
            return Err(AcceptRefusal::NotLive);
        }
        let Some(buf) = self.buffer else {
            return Err(AcceptRefusal::NotLive);
        };
        let Some(hunk) = self.hunks.get_mut(index) else {
            return Err(AcceptRefusal::NotFresh);
        };
        if hunk.status != HunkStatus::Fresh {
            return Err(AcceptRefusal::NotFresh);
        }
        Ok(self.write(buf, &[index]))
    }

    /// Marks `indices` accepted and answers the one write that applies all
    /// of them.
    ///
    /// One call, never one per hunk: the batch's edits are applied bottom
    /// of the buffer first inside nvim's own chunk, so no hunk's rows can
    /// shift before a later hunk in the same batch is written, and the
    /// whole batch stands or falls on a single `expected_changedtick`
    /// check. Emitting a call per hunk would leave every call after the
    /// first unguarded -- the first call's own apply moves the tick, so
    /// only the first could carry an expectation at all.
    fn write(&mut self, buf: BufferHandle, indices: &[usize]) -> Vec<Effect> {
        let mut edits = Vec::new();
        for index in indices {
            let Some(hunk) = self.hunks.get_mut(*index) else {
                continue;
            };
            edits.extend(hunk.edits());
            hunk.status = HunkStatus::Accepted;
        }
        if edits.is_empty() {
            return Vec::new();
        }
        let undojoin = self.written;
        self.in_flight = Some(InFlight {
            hunks: indices.to_vec(),
            written_before: self.written,
        });
        self.written = true;
        vec![Effect::Rpc(RpcCall::BufSetText {
            buf,
            edits,
            undojoin,
            expected_changedtick: Some(self.changedtick),
            generation: self.generation,
        })]
    }

    /// Folds one `Msg::BufWriteApplied` in: the write landed, so there is
    /// nothing left to roll back and the next write names the tick this
    /// one produced -- never the tick from before it, which the buffer has
    /// already moved past.
    pub fn note_write_applied(&mut self, buf: BufferHandle, generation: u64, changedtick: u64) {
        if self.buffer != Some(buf) || generation != self.generation {
            return;
        }
        self.in_flight = None;
        self.changedtick = changedtick;
    }

    /// Folds one `Msg::BufWriteRefused` in: the write never applied, so the
    /// hunks it claimed go back to being decisions the user owes, and the
    /// undo state goes back to what it was. They come back
    /// [`HunkStatus::Stale`] rather than `Fresh` because the buffer
    /// provably moved under them -- the edit that moved it is on its way
    /// here and re-anchors them, and re-diff (`R`) is what makes them
    /// writable again.
    pub fn note_write_refused(&mut self, buf: BufferHandle, generation: u64) {
        if self.buffer != Some(buf) || generation != self.generation {
            return;
        }
        let Some(in_flight) = self.in_flight.take() else {
            return;
        };
        self.written = in_flight.written_before;
        for index in in_flight.hunks {
            if let Some(hunk) = self.hunks.get_mut(index) {
                if hunk.status == HunkStatus::Accepted {
                    hunk.status = HunkStatus::Stale;
                }
            }
        }
    }

    /// Accepts every [`HunkStatus::Fresh`] hunk as one write, bottom of the
    /// buffer first.
    ///
    /// The order is load-bearing: every edit in the batch was computed
    /// against the same pre-accept buffer, so applying top-down would have
    /// each accepted hunk shift the rows of every hunk below it. The
    /// executor sorts a batch descending for exactly this reason; the order
    /// is stated here as well so the batch reads the way it applies.
    pub fn accept_all(&mut self) -> Vec<Effect> {
        if !self.sync.can_apply() {
            return Vec::new();
        }
        let Some(buf) = self.buffer else {
            return Vec::new();
        };
        let mut order: Vec<usize> = (0..self.hunks.len())
            .filter(|i| self.hunks[*i].status == HunkStatus::Fresh)
            .collect();
        order.sort_by_key(|i| std::cmp::Reverse(self.hunks[*i].old_range));
        self.write(buf, &order)
    }

    /// Declines the hunk at `index`. Terminal: a later re-diff of this
    /// review never re-offers it.
    pub fn reject(&mut self, index: usize) -> bool {
        let Some(hunk) = self.hunks.get_mut(index) else {
            return false;
        };
        if !hunk.status.is_open() {
            return false;
        }
        hunk.status = HunkStatus::Rejected;
        true
    }

    /// Declines every hunk still open, answering whether any was.
    ///
    /// [`Self::accept_all`]'s counterpart, and the reason it exists is the
    /// same: a proposal a reader has decided against as a whole is one
    /// decision, not one per hunk. Unlike accept-all it takes stale hunks
    /// too -- rejecting needs nothing anchored, since nothing is written.
    pub fn reject_all(&mut self) -> bool {
        let mut any = false;
        for hunk in &mut self.hunks {
            if hunk.status.is_open() {
                hunk.status = HunkStatus::Rejected;
                any = true;
            }
        }
        any
    }

    /// Re-anchors the stale hunk at `index` against the text the rebase
    /// carried forward for it. Refused on a review whose buffer state is no
    /// longer trustworthy at all, for the reason [`ReviewSync`]'s own doc
    /// gives.
    pub fn re_diff(&mut self, index: usize) -> bool {
        if !self.sync.can_apply() {
            return false;
        }
        let Some(hunk) = self.hunks.get_mut(index) else {
            return false;
        };
        if hunk.status != HunkStatus::Stale || !hunk.anchor_intact {
            return false;
        }
        hunk.re_diff();
        true
    }

    /// The review's summary rows in the panel: which file, which hunk of
    /// how many, and the keys that act on it.
    ///
    /// The second copy of the key hint on purpose -- the first is the
    /// header [`Self::marks`] puts at the hunk itself, where the decision
    /// is made. This one costs two rows and survives the user scrolling
    /// the buffer away from every hunk.
    #[must_use]
    pub fn summary_rows(&self) -> Vec<Vec<Span>> {
        let open = self.hunks.iter().filter(|h| h.status.is_open()).count();
        let mut rows = vec![vec![Span::plain(format!(
            "Review {} -- hunk {}/{}, {open} open",
            self.path.display(),
            (self.cursor + 1).min(self.hunks.len().max(1)),
            self.hunks.len()
        ))]];
        if let Some(notice) = self.sync.notice() {
            rows.push(vec![Span::plain(notice.to_string())]);
        } else {
            rows.push(vec![Span::plain(KEY_HINT)]);
        }
        rows
    }

    /// Every open hunk's presentation in the buffer itself, top of the
    /// buffer first.
    ///
    /// Only the open ones: an accepted hunk's lines are already the
    /// buffer's own text and a rejected one is not on offer, so either
    /// still drawn would be view asserting a change nobody can act on.
    ///
    /// Nothing here is styled by view -- see [`HunkMark`] for why the
    /// highlight groups are nvim's and are resolved on the engine side.
    #[must_use]
    pub fn marks(&self) -> Vec<HunkMark> {
        self.hunks
            .iter()
            .enumerate()
            .filter(|(_, hunk)| hunk.status.is_open())
            .map(|(index, hunk)| {
                let (row, end_row) = hunk.old_range;
                HunkMark {
                    row,
                    end_row,
                    // A pure insertion replaces no row, so it hangs off the
                    // row it is inserted before and is drawn above it; every
                    // other hunk hangs off the last row it replaces, where
                    // the proposed lines read as following the removed ones.
                    anchor: if end_row > row { end_row - 1 } else { row },
                    added: hunk.new_lines.clone(),
                    stale: hunk.status == HunkStatus::Stale,
                    current: index == self.cursor,
                    header: (index == self.cursor).then(|| self.header(index)),
                }
            })
            .collect()
    }

    /// The current hunk's header: where the user is in the review, and
    /// either the keys that decide this hunk or the reason nothing here
    /// can be decided any more.
    fn header(&self, index: usize) -> String {
        let position = format!("hunk {}/{}", index + 1, self.hunks.len());
        if let Some(notice) = self.sync.notice() {
            return format!("{position} -- {notice} -- {LEAVE_HINT}");
        }
        let stale = self
            .hunks
            .get(index)
            .is_some_and(|hunk| hunk.status == HunkStatus::Stale);
        // A stale hunk has no accept to offer -- `accept` refuses it -- so
        // its header names the two keys that do work on it instead of
        // advertising the one that answers with a refusal notice.
        format!(
            "{position} -- {}",
            if stale { STALE_KEY_HINT } else { KEY_HINT }
        )
    }

    /// The call that draws this review in its buffer, or `None` for a
    /// review with no buffer to draw in (still binding, or unbindable).
    ///
    /// `focus` decides whether the review comes to the user or merely
    /// repaints under them; see [`RpcCall::ReviewShow`]'s own doc.
    #[must_use]
    pub fn show_effect(&self, focus: bool, open_target: ReviewOpenTarget) -> Option<Effect> {
        let buf = self.buffer?;
        Some(Effect::Rpc(RpcCall::ReviewShow {
            buf,
            marks: self.marks(),
            cursor_row: self
                .hunks
                .get(self.cursor)
                .map_or(0, |hunk| hunk.old_range.0),
            focus,
            open_target,
        }))
    }

    /// What a repaint depends on, cheaply: how many hunks have gone stale
    /// and whether the review as a whole still trusts its buffer.
    ///
    /// The gate on re-issuing [`Self::show_effect`] after folding an edit
    /// (`update::review::on_buf_text_changed`), so ordinary typing
    /// somewhere else in the reviewed buffer costs no RPC at all. A count
    /// rather than the status vector because a fold can only ever stale a
    /// hunk, never un-stale one, so the count moving is exactly "some
    /// hunk's presentation changed" -- and it is O(hunks) with no
    /// allocation, on a path that runs once per keystroke.
    #[must_use]
    pub fn presentation_stamp(&self) -> (usize, ReviewSync) {
        (
            self.hunks
                .iter()
                .filter(|hunk| hunk.status == HunkStatus::Stale)
                .count(),
            self.sync,
        )
    }

    /// What the transcript records when this review ends: what was decided
    /// and in which file, or that the proposal was dismissed with
    /// decisions still owed.
    ///
    /// The review's own sentence rather than the caller's, so the two ways
    /// it can end cannot describe the same outcome differently.
    #[must_use]
    pub fn outcome(&self) -> String {
        let open = self.hunks.iter().filter(|h| h.status.is_open()).count();
        let path = self.path.display();
        if open > 0 {
            return format!("discarded the proposal for {path} -- {open} hunks left undecided");
        }
        let accepted = count(&self.hunks, HunkStatus::Accepted);
        let rejected = count(&self.hunks, HunkStatus::Rejected);
        format!("accepted {accepted} and rejected {rejected} hunks in {path}")
    }
}

/// How many hunks are in `status`.
fn count(hunks: &[Hunk], status: HunkStatus) -> usize {
    hunks.iter().filter(|hunk| hunk.status == status).count()
}

/// The review's own keys, on the current hunk's header in the buffer and
/// on the panel's summary row. Buffer-local nvim mappings rather than
/// panel keys (see `docs/keymaps.md`): a reviewed buffer stays editable,
/// so claiming bare `a`/`x`/`q` in it for the length of a review would
/// break the one contract -- ordinary nvim keys do ordinary nvim things --
/// that everything else here is built on.
///
/// One line rather than a continuation on purpose: `ai-conformance.sh`
/// reads this constant out of the source to know what to look for on
/// screen, and its reader takes the value from a single `const` line.
const KEY_HINT: &str = "<leader>ha accept  <leader>hA accept all  <leader>hx reject  ]c next  [c prev  <leader>hq leave";

/// [`KEY_HINT`] for a hunk the buffer has moved under: re-diff in place of
/// the accept that would be refused.
const STALE_KEY_HINT: &str =
    "stale -- <leader>hR re-diff  <leader>hx reject  ]c next  [c prev  <leader>hq leave";

/// The one key a review whose buffer can no longer be trusted still has,
/// appended to the sync notice that says why nothing else is on offer.
const LEAVE_HINT: &str = "<leader>hq leave";

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::native::diff::hunk;

    /// `Effect` carries no `PartialEq` (see its own doc), so an effect
    /// assertion goes through the `RpcCall` inside it, which does.
    fn rpc(effect: &Effect) -> &RpcCall {
        match effect {
            Effect::Rpc(call) => call,
            other => panic!("expected an Rpc effect, got {other:?}"),
        }
    }

    fn review() -> DiffReviewState {
        let mut state = DiffReviewState::new(
            1,
            PathBuf::from("/tmp/a.rs"),
            3,
            hunk::diff(Some("a\nb\nc\nd\ne\nf\ng\n"), "a\nB\nc\nd\ne\nF\ng\n"),
        );
        assert_eq!(state.bind(3, Some(BufferHandle(9)), 11).len(), 1);
        state
    }

    #[test]
    fn binding_resolves_then_attaches() {
        let mut state = DiffReviewState::new(1, PathBuf::from("/tmp/a.rs"), 3, Vec::new());
        assert_eq!(
            rpc(&state.bind_effect()),
            &RpcCall::LoadHidden {
                path: "/tmp/a.rs".to_string(),
                generation: 3,
            }
        );
        let effects = state.bind(3, Some(BufferHandle(9)), 11);
        assert_eq!(effects.len(), 1);
        assert_eq!(
            rpc(&effects[0]),
            &RpcCall::BufAttach {
                buf: BufferHandle(9),
                generation: 3,
            }
        );
        assert_eq!(state.sync, ReviewSync::Live);
    }

    #[test]
    fn a_bind_reply_for_a_superseded_generation_is_dropped() {
        let mut state = DiffReviewState::new(1, PathBuf::from("/tmp/a.rs"), 3, Vec::new());
        assert!(state.bind(2, Some(BufferHandle(9)), 11).is_empty());
        assert_eq!(state.buffer, None);
        assert_eq!(state.sync, ReviewSync::Binding);
    }

    #[test]
    fn a_path_with_no_buffer_leaves_the_review_unbindable() {
        let mut state = DiffReviewState::new(1, PathBuf::from("/tmp/a.rs"), 3, Vec::new());
        assert!(state.bind(3, None, 0).is_empty());
        assert_eq!(state.sync, ReviewSync::Unbindable);
        assert!(!state.sync.can_apply());
    }

    #[test]
    fn accepting_a_fresh_hunk_writes_it_unjoined_and_the_next_one_joined() {
        let mut state = review();
        let expected = state.hunks[0].edits();
        let first = state.accept(0).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(
            rpc(&first[0]),
            &RpcCall::BufSetText {
                buf: BufferHandle(9),
                edits: expected,
                undojoin: false,
                expected_changedtick: Some(11),
                generation: 3,
            }
        );
        assert_eq!(state.hunks[0].status, HunkStatus::Accepted);
        let second = state.accept(1).unwrap();
        let RpcCall::BufSetText { undojoin, .. } = rpc(&second[0]) else {
            panic!("accept emits exactly one BufSetText")
        };
        assert!(
            *undojoin,
            "every accept after a review's first joins onto it"
        );
    }

    #[test]
    fn accepting_a_stale_hunk_is_refused() {
        let mut state = review();
        state.hunks[0].status = HunkStatus::Stale;
        assert_eq!(state.accept(0).err(), Some(AcceptRefusal::NotFresh));
        assert_eq!(state.hunks[0].status, HunkStatus::Stale);
        assert!(!state.written);
    }

    #[test]
    fn accepting_on_a_desynced_review_is_refused_before_any_hunk_is_looked_at() {
        let mut state = review();
        state.sync = ReviewSync::Desynced;
        assert_eq!(state.accept(0).err(), Some(AcceptRefusal::NotLive));
        assert_eq!(state.hunks[0].status, HunkStatus::Fresh);
    }

    /// The ordering contract: every edit in the batch was computed against
    /// the same pre-accept buffer, so it has to apply bottom-up or the
    /// later hunks' rows are wrong by the time they are written. One call,
    /// not one per hunk: a per-hunk call after the first could carry no
    /// `expected_changedtick` at all, since the first call's own apply
    /// moves the tick.
    #[test]
    fn accept_all_issues_one_batched_call_bottom_of_the_buffer_first() {
        let mut state = review();
        let effects = state.accept_all();
        assert_eq!(effects.len(), 1, "one write, not one per hunk");
        let RpcCall::BufSetText {
            edits,
            undojoin,
            expected_changedtick,
            ..
        } = rpc(&effects[0])
        else {
            panic!("accept_all emits only BufSetText")
        };
        let rows: Vec<u32> = edits.iter().map(|edit| edit.start_row).collect();
        assert!(
            rows[0] > rows[1],
            "accept-all must apply bottom-up; got start rows {rows:?}"
        );
        assert!(!undojoin, "the first write of a review joins nothing");
        assert_eq!(
            *expected_changedtick,
            Some(11),
            "the batch stands or falls on the tick it was computed against"
        );
    }

    /// A write nvim refuses never happened: the hunks it claimed go back to
    /// being decisions the user owes, and the undo state goes back too --
    /// otherwise the next accept would join its undo entry onto whatever
    /// the user's own last edit was.
    #[test]
    fn a_refused_write_puts_its_hunks_and_the_undo_state_back() {
        let mut state = review();
        let _ = state.accept(0).unwrap();
        assert_eq!(state.hunks[0].status, HunkStatus::Accepted);
        assert!(state.written);

        state.note_write_refused(BufferHandle(9), 3);

        assert_eq!(
            state.hunks[0].status,
            HunkStatus::Stale,
            "a hunk whose write was refused is not applied, and the buffer \
             provably moved under it"
        );
        assert!(
            !state.written,
            "a write that never applied must not make the next one join"
        );
        assert_eq!(state.in_flight, None);
    }

    /// A refusal naming another buffer, or a superseded review, is not this
    /// review's to answer.
    #[test]
    fn a_refusal_for_another_buffer_or_generation_is_ignored() {
        let mut state = review();
        let _ = state.accept(0).unwrap();
        state.note_write_refused(BufferHandle(11), 3);
        state.note_write_refused(BufferHandle(9), 4);
        assert_eq!(state.hunks[0].status, HunkStatus::Accepted);
    }

    /// The edit event that lands after a write is that write's own
    /// confirmation: there is nothing left to roll back, and the tick the
    /// next write names is the one that event carried.
    #[test]
    fn a_folded_edit_confirms_the_write_and_advances_the_stamp() {
        let mut state = review();
        let _ = state.accept(0).unwrap();
        state.apply_change(&BufTextChangedEvent {
            buf: BufferHandle(9),
            generation: 3,
            firstline: 1,
            lastline: 2,
            linedata: vec!["B".to_string()],
            changedtick: 12,
            desynced: false,
        });
        assert_eq!(state.in_flight, None);
        assert_eq!(state.changedtick, 12);
        state.note_write_refused(BufferHandle(9), 3);
        assert_eq!(
            state.hunks[0].status,
            HunkStatus::Accepted,
            "a confirmed write is not rolled back by a later stray refusal"
        );
    }

    #[test]
    fn accept_all_skips_a_stale_hunk_rather_than_forcing_it() {
        let mut state = review();
        state.hunks[1].status = HunkStatus::Stale;
        let effects = state.accept_all();
        let RpcCall::BufSetText { edits, .. } = rpc(&effects[0]) else {
            panic!("accept_all emits only BufSetText")
        };
        assert_eq!(edits.len(), 1, "only the fresh hunk is in the batch");
        assert_eq!(state.hunks[1].status, HunkStatus::Stale);
    }

    #[test]
    fn a_change_for_another_buffer_is_not_folded_in() {
        let mut state = review();
        let change = BufTextChangedEvent {
            buf: BufferHandle(11),
            generation: 3,
            firstline: 1,
            lastline: 2,
            linedata: vec!["different".to_string()],
            changedtick: 1,
            desynced: false,
        };
        state.apply_change(&change);
        assert_eq!(state.hunks[0].status, HunkStatus::Fresh);
    }

    #[test]
    fn a_desynced_change_retires_the_whole_review_not_only_its_hunks() {
        let mut state = review();
        let change = BufTextChangedEvent {
            buf: BufferHandle(9),
            generation: 3,
            firstline: 0,
            lastline: 0,
            linedata: vec!["x".to_string()],
            changedtick: 1,
            desynced: true,
        };
        state.apply_change(&change);
        assert_eq!(state.sync, ReviewSync::Desynced);
        assert!(state.hunks.iter().all(|h| h.status == HunkStatus::Stale));
        assert!(!state.re_diff(0), "a desynced review may not re-diff");
        assert_eq!(state.accept(0).err(), Some(AcceptRefusal::NotLive));
    }

    #[test]
    fn a_detach_retires_the_review_and_stales_every_open_hunk() {
        let mut state = review();
        state.note_detached(BufferHandle(9), 3);
        assert_eq!(state.sync, ReviewSync::Detached);
        assert!(state.hunks.iter().all(|h| h.status == HunkStatus::Stale));
        assert_eq!(state.accept(0).err(), Some(AcceptRefusal::NotLive));
        let effects = state.close_effects();
        assert!(
            !effects
                .iter()
                .any(|e| matches!(rpc(e), RpcCall::BufDetach { .. })),
            "nvim already ended the subscription; asking it to detach again \
             names a subscription that no longer exists"
        );
        assert_eq!(effects.len(), 2);
        assert_eq!(
            rpc(&effects[0]),
            &RpcCall::ReviewClear {
                buf: BufferHandle(9),
            },
            "the decoration outlives the subscription and is still this \
             review's to take back off the buffer"
        );
        assert_eq!(
            rpc(&effects[1]),
            &RpcCall::ReleaseHidden {
                path: "/tmp/a.rs".to_string(),
            },
            "the hidden-buffer hold this review's bind acquired is still owed \
             a release even though the edit subscription already ended"
        );
    }

    #[test]
    fn closing_a_live_review_undecorates_detaches_and_releases_the_hidden_hold() {
        let state = review();
        let effects = state.close_effects();
        assert_eq!(effects.len(), 3);
        assert_eq!(
            rpc(&effects[0]),
            &RpcCall::ReviewClear {
                buf: BufferHandle(9),
            }
        );
        assert_eq!(
            rpc(&effects[1]),
            &RpcCall::BufDetach {
                buf: BufferHandle(9),
            }
        );
        assert_eq!(
            rpc(&effects[2]),
            &RpcCall::ReleaseHidden {
                path: "/tmp/a.rs".to_string(),
            }
        );
    }

    #[test]
    fn a_detach_for_another_buffer_leaves_the_review_alone() {
        let mut state = review();
        state.note_detached(BufferHandle(11), 3);
        assert_eq!(state.sync, ReviewSync::Live);
    }

    #[test]
    fn hunk_jump_skips_resolved_hunks_and_wraps() {
        let mut state = review();
        assert_eq!(state.cursor, 0);
        state.next_hunk();
        assert_eq!(state.cursor, 1);
        state.next_hunk();
        assert_eq!(state.cursor, 0, "the jump wraps rather than stopping");
        state.hunks[1].status = HunkStatus::Rejected;
        state.next_hunk();
        assert_eq!(
            state.cursor, 0,
            "a resolved hunk is not a decision the user still owes"
        );
    }

    #[test]
    fn prev_hunk_walks_backwards() {
        let mut state = review();
        state.prev_hunk();
        assert_eq!(state.cursor, 1);
    }

    #[test]
    fn rejecting_a_hunk_is_terminal() {
        let mut state = review();
        assert!(state.reject(0));
        assert_eq!(state.hunks[0].status, HunkStatus::Rejected);
        assert!(!state.reject(0));
    }

    #[test]
    fn a_review_with_every_hunk_decided_is_no_longer_open() {
        let mut state = review();
        state.accept_all();
        assert!(!state.is_open());
    }

    #[test]
    fn the_summary_names_the_file_the_hunk_count_and_the_keys() {
        let state = review();
        let rows = state.summary_rows();
        assert_eq!(
            rows[0],
            vec![Span::plain("Review /tmp/a.rs -- hunk 1/2, 2 open")]
        );
        assert_eq!(rows[1], vec![Span::plain(KEY_HINT)]);
    }

    #[test]
    fn the_summary_says_why_a_broken_review_cannot_be_acted_on() {
        let mut state = review();
        state.sync = ReviewSync::Detached;
        let rows = state.summary_rows();
        assert!(
            rows[1][0].text.contains("edit stream ended"),
            "a review that cannot apply must say so: {rows:?}"
        );
    }

    /// A replacement marks the rows it replaces and hangs its proposed
    /// lines off the last of them; the header belongs to the cursor's hunk
    /// and to no other, so the keys are on screen exactly once.
    #[test]
    fn marks_name_the_replaced_rows_and_header_only_the_current_hunk() {
        let state = review();
        let marks = state.marks();
        assert_eq!(marks.len(), 2);
        assert_eq!((marks[0].row, marks[0].end_row), (1, 2));
        assert_eq!(marks[0].anchor, 1, "the last row the hunk replaces");
        assert_eq!(marks[0].added, vec!["B".to_string()]);
        assert!(!marks[0].stale);
        assert!(marks[0].current);
        let header = marks[0].header.clone().expect("the current hunk's header");
        assert!(header.starts_with("hunk 1/2 --"), "{header}");
        assert!(header.contains("<leader>ha accept"), "{header}");
        assert!(header.contains("]c next"), "{header}");
        assert!(header.contains("<leader>hq leave"), "{header}");
        assert!(
            !marks[1].current && marks[1].header.is_none(),
            "one copy of the keys, at the decision being made: {marks:?}"
        );
    }

    /// A pure insertion replaces nothing, so it highlights nothing: its
    /// mark is an empty row span the engine draws above, and a pure
    /// deletion is the mirror image with no lines to add.
    #[test]
    fn a_pure_insertion_spans_no_row_and_a_pure_deletion_adds_no_line() {
        let insert = DiffReviewState::new(
            1,
            PathBuf::from("/tmp/a.rs"),
            3,
            hunk::diff(Some("a\nb\n"), "a\nNEW\nb\n"),
        );
        let marks = insert.marks();
        assert_eq!(marks.len(), 1);
        assert_eq!(
            marks[0].row, marks[0].end_row,
            "an insertion replaces no row, so nothing is highlighted as \
             removed: {marks:?}"
        );
        assert_eq!(marks[0].anchor, marks[0].row);
        assert_eq!(marks[0].added, vec!["NEW".to_string()]);

        let delete = DiffReviewState::new(
            1,
            PathBuf::from("/tmp/a.rs"),
            3,
            hunk::diff(Some("a\nb\nc\n"), "a\nc\n"),
        );
        let marks = delete.marks();
        assert_eq!(marks.len(), 1);
        assert!(marks[0].end_row > marks[0].row, "{marks:?}");
        assert!(
            marks[0].added.is_empty(),
            "a deletion proposes no line: {marks:?}"
        );
    }

    /// A hunk the buffer moved under says so where the decision is made,
    /// and offers the key that makes it decidable again rather than the
    /// accept that would be refused.
    #[test]
    fn a_stale_current_hunk_carries_the_stale_flag_and_the_re_diff_key() {
        let mut state = review();
        state.hunks[0].status = HunkStatus::Stale;
        let marks = state.marks();
        assert!(marks[0].stale);
        let header = marks[0].header.clone().expect("the current hunk's header");
        assert!(header.contains("<leader>hR re-diff"), "{header}");
        assert!(!header.contains("<leader>ha accept"), "{header}");
    }

    /// A review that can no longer be acted on has to say so at the code,
    /// not only in the panel, and still name the way out.
    #[test]
    fn a_broken_review_replaces_the_keys_with_the_reason_and_the_way_out() {
        let mut state = review();
        state.sync = ReviewSync::Detached;
        let header = state.marks()[0]
            .header
            .clone()
            .expect("the current hunk's header");
        assert!(header.contains("edit stream ended"), "{header}");
        assert!(header.contains(LEAVE_HINT), "{header}");
    }

    /// Only decisions the user still owes are drawn: an accepted hunk's
    /// lines are the buffer's own text by then, and a rejected one is not
    /// on offer.
    #[test]
    fn a_decided_hunk_leaves_no_mark_behind() {
        let mut state = review();
        assert!(state.reject(0));
        let marks = state.marks();
        assert_eq!(marks.len(), 1);
        assert_eq!((marks[0].row, marks[0].end_row), (5, 6));
    }

    /// The gate on repainting: a fold that stales nothing and breaks
    /// nothing leaves the stamp where it was, so typing elsewhere in a
    /// reviewed buffer costs no RPC.
    #[test]
    fn the_presentation_stamp_moves_only_when_a_hunk_or_the_sync_changes() {
        let mut state = DiffReviewState::new(
            1,
            PathBuf::from("/tmp/a.rs"),
            3,
            hunk::diff(Some("a\nb\nc\nd\ne\nf\ng\nh\n"), "a\nB\nc\nd\ne\nf\ng\nh\n"),
        );
        assert_eq!(state.bind(3, Some(BufferHandle(9)), 11).len(), 1);
        let before = state.presentation_stamp();
        state.apply_change(&BufTextChangedEvent {
            buf: BufferHandle(9),
            generation: 3,
            firstline: 6,
            lastline: 7,
            linedata: vec!["G".to_string()],
            changedtick: 12,
            desynced: false,
        });
        assert_eq!(
            state.presentation_stamp(),
            before,
            "an edit outside every anchor changes nothing on screen"
        );
        state.hunks[0].status = HunkStatus::Stale;
        assert_ne!(state.presentation_stamp(), before);
    }

    #[test]
    fn show_effect_carries_the_marks_the_cursor_row_and_the_focus() {
        let mut state = review();
        state.next_hunk();
        let effect = state
            .show_effect(true, ReviewOpenTarget::Split)
            .expect("a bound review draws");
        let RpcCall::ReviewShow {
            buf,
            marks,
            cursor_row,
            focus,
            open_target,
        } = rpc(&effect)
        else {
            panic!("show_effect emits only ReviewShow")
        };
        assert_eq!(*buf, BufferHandle(9));
        assert_eq!(marks.len(), 2);
        assert_eq!(*cursor_row, 5, "the row the second hunk starts at");
        assert!(*focus);
        assert_eq!(*open_target, ReviewOpenTarget::Split);
    }

    #[test]
    fn an_unbound_review_has_nothing_to_draw_in() {
        let state = DiffReviewState::new(1, PathBuf::from("/tmp/a.rs"), 3, Vec::new());
        assert!(state.show_effect(true, ReviewOpenTarget::Current).is_none());
    }

    #[test]
    fn reject_all_declines_every_open_hunk_including_the_stale_ones() {
        let mut state = review();
        state.hunks[0].status = HunkStatus::Stale;
        assert!(state.reject_all());
        assert!(state.hunks.iter().all(|h| h.status == HunkStatus::Rejected));
        assert!(!state.is_open());
        assert!(!state.reject_all(), "nothing left to decline");
    }

    #[test]
    fn the_outcome_counts_the_decisions_or_says_the_proposal_was_dismissed() {
        let mut state = review();
        assert_eq!(
            state.outcome(),
            "discarded the proposal for /tmp/a.rs -- 2 hunks left undecided"
        );
        let _ = state.accept(0).unwrap();
        assert!(state.reject(1));
        assert_eq!(
            state.outcome(),
            "accepted 1 and rejected 1 hunks in /tmp/a.rs"
        );
    }
}
