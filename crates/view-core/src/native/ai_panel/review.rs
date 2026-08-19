//! The agent panel's diff review: the hunks one proposal opened, which one
//! the user is looking at, and what the buffer underneath them is still
//! able to tell this session.
//!
//! A state inside the panel rather than a second top-level overlay: a
//! proposed diff is always the direct consequence of an in-flight agent
//! turn, so it opens as part of the panel's own flow and closes with it.
//!
//! Nothing here writes: an accept produces the
//! [`crate::msg::RpcCall::BufSetText`] call that does, and nvim remains the
//! sole owner of every byte in the buffer.

use std::path::PathBuf;

use super::super::diff::{rebase, BufTextChangedEvent, Hunk, HunkStatus};
use super::super::views::{Span, StyleRole};
use crate::msg::{BufferHandle, Effect, RpcCall};

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
    /// Stamped on this review's own `BufResolve`/`BufAttach` and matched
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
    /// [`RpcCall::BufSetText`]'s own per-hunk undo contract.
    pub written: bool,
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
        }
    }

    /// The effect that starts the binding: nvim owns buffer identity, so
    /// the path the agent named has to be resolved there before anything
    /// can attach to it or write into it.
    #[must_use]
    pub fn bind_effect(&self) -> Effect {
        Effect::Rpc(RpcCall::BufResolve {
            path: self.path.to_string_lossy().into_owned(),
            generation: self.generation,
        })
    }

    /// Folds one `Msg::BufResolved` in, returning the attach that follows a
    /// successful bind. A reply whose generation is not this review's is
    /// dropped and answers no effects.
    #[must_use]
    pub fn bind(&mut self, generation: u64, buf: Option<BufferHandle>) -> Vec<Effect> {
        if generation != self.generation || self.sync != ReviewSync::Binding {
            return Vec::new();
        }
        let Some(buf) = buf else {
            self.sync = ReviewSync::Unbindable;
            return Vec::new();
        };
        self.buffer = Some(buf);
        self.sync = ReviewSync::Live;
        vec![Effect::Rpc(RpcCall::BufAttach {
            buf,
            generation: self.generation,
        })]
    }

    /// The detach that ends this review's subscription, or `None` when
    /// there is nothing attached to end.
    #[must_use]
    pub fn close_effect(&self) -> Option<Effect> {
        let buf = self.buffer?;
        matches!(self.sync, ReviewSync::Live | ReviewSync::Desynced)
            .then_some(Effect::Rpc(RpcCall::BufDetach { buf }))
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
        let edits = hunk.edits();
        hunk.status = HunkStatus::Accepted;
        let undojoin = self.written;
        self.written = true;
        Ok(vec![Effect::Rpc(RpcCall::BufSetText {
            buf,
            edits,
            undojoin,
        })])
    }

    /// Accepts every [`HunkStatus::Fresh`] hunk, bottom of the buffer
    /// first.
    ///
    /// The order is load-bearing and is this function's whole reason to
    /// exist separately from a loop over [`Self::accept`]: all of these
    /// calls are emitted before nvim reports a single one of them back, so
    /// no rebase runs in between and every later hunk's rows are still the
    /// ones computed against the pre-accept buffer. Applying top-down would
    /// have each accepted hunk shift the rows of every hunk below it that
    /// has already been addressed.
    pub fn accept_all(&mut self) -> Vec<Effect> {
        if !self.sync.can_apply() {
            return Vec::new();
        }
        let mut order: Vec<usize> = (0..self.hunks.len())
            .filter(|i| self.hunks[*i].status == HunkStatus::Fresh)
            .collect();
        order.sort_by_key(|i| std::cmp::Reverse(self.hunks[*i].old_range));
        let mut effects = Vec::with_capacity(order.len());
        for index in order {
            match self.accept(index) {
                Ok(one) => effects.extend(one),
                Err(_) => continue,
            }
        }
        effects
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

    /// The review's always-visible summary rows: which file, which hunk of
    /// how many, and the keys that act on it.
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

    /// The scrolling rows: every hunk's own header, the buffer row above
    /// it, its removed and added lines, and the buffer row below it.
    /// Deliberately nothing further -- the review's scroll region is the
    /// hunks and their own context, never the whole file, so hunk-jump
    /// stays the way to reach the next decision rather than scrolling
    /// across buffer regions the proposal does not touch.
    #[must_use]
    pub fn hunk_rows(&self) -> Vec<Vec<Span>> {
        let mut rows = Vec::new();
        for hunk in &self.hunks {
            rows.push(vec![Span::plain(format!(
                "@@ {}..{} @@ {}",
                hunk.old_range.0,
                hunk.old_range.1,
                status_label(hunk.status)
            ))]);
            if hunk.has_leading_context() {
                rows.push(context_row(hunk, hunk.old_range.0.saturating_sub(1)));
            }
            for row in hunk.old_range.0..hunk.old_range.1 {
                let text = hunk.anchor_row(row).map_or("", String::as_str);
                rows.push(vec![Span::new(format!("-{text}"), StyleRole::DiffRemoved)]);
            }
            for line in &hunk.new_lines {
                rows.push(vec![Span::new(format!("+{line}"), StyleRole::DiffAdded)]);
            }
            if hunk.has_trailing_context() {
                rows.push(context_row(hunk, hunk.old_range.1));
            }
        }
        rows
    }

    /// The index into [`Self::hunk_rows`] of the cursor's own hunk header,
    /// which is what the panel's scroll window anchors on.
    #[must_use]
    pub fn cursor_row(&self) -> Option<usize> {
        if self.hunks.is_empty() {
            return None;
        }
        let mut row = 0;
        for (index, hunk) in self.hunks.iter().enumerate() {
            if index == self.cursor {
                return Some(row);
            }
            row += rendered_rows(hunk);
        }
        None
    }
}

/// One unchanged buffer row shown beside a hunk, rendered in the diff's
/// own gutter shape (a leading space) so the removed and added rows still
/// line up with it.
fn context_row(hunk: &Hunk, row: u32) -> Vec<Span> {
    vec![Span::plain(format!(
        " {}",
        hunk.anchor_row(row).map_or("", String::as_str)
    ))]
}

/// How many rows one hunk contributes to [`DiffReviewState::hunk_rows`].
/// Shared with `cursor_row` rather than counted twice: a cursor row that
/// disagreed with the rendered rows would scroll the window to the wrong
/// hunk.
fn rendered_rows(hunk: &Hunk) -> usize {
    1 + usize::from(hunk.has_leading_context())
        + usize::try_from(hunk.old_range.1.saturating_sub(hunk.old_range.0)).unwrap_or(0)
        + hunk.new_lines.len()
        + usize::from(hunk.has_trailing_context())
}

fn status_label(status: HunkStatus) -> &'static str {
    match status {
        HunkStatus::Fresh => "fresh",
        HunkStatus::Stale => "stale -- re-diff or reject",
        HunkStatus::Accepted => "accepted",
        HunkStatus::Rejected => "rejected",
    }
}

/// The review's own keys, shown in place of the sync notice while nothing
/// is wrong. Named here rather than in the key handler because this is the
/// text a reader sees; `update::mod`'s entered-panel arm is what answers
/// them.
const KEY_HINT: &str = "a accept  A accept all  x reject  R re-diff  ] next  [ prev  q close";

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
        assert_eq!(state.bind(3, Some(BufferHandle(9))).len(), 1);
        state
    }

    #[test]
    fn binding_resolves_then_attaches() {
        let mut state = DiffReviewState::new(1, PathBuf::from("/tmp/a.rs"), 3, Vec::new());
        assert_eq!(
            rpc(&state.bind_effect()),
            &RpcCall::BufResolve {
                path: "/tmp/a.rs".to_string(),
                generation: 3,
            }
        );
        let effects = state.bind(3, Some(BufferHandle(9)));
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
        assert!(state.bind(2, Some(BufferHandle(9))).is_empty());
        assert_eq!(state.buffer, None);
        assert_eq!(state.sync, ReviewSync::Binding);
    }

    #[test]
    fn a_path_with_no_buffer_leaves_the_review_unbindable() {
        let mut state = DiffReviewState::new(1, PathBuf::from("/tmp/a.rs"), 3, Vec::new());
        assert!(state.bind(3, None).is_empty());
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

    /// The ordering contract: every call is emitted before nvim reports any
    /// of them back, so the batch has to be bottom-up or the later hunks'
    /// rows are wrong by the time they are applied.
    #[test]
    fn accept_all_issues_one_call_per_hunk_bottom_of_the_buffer_first() {
        let mut state = review();
        let effects = state.accept_all();
        assert_eq!(effects.len(), 2);
        let rows: Vec<u32> = effects
            .iter()
            .map(|effect| {
                let RpcCall::BufSetText { edits, .. } = rpc(effect) else {
                    panic!("accept_all emits only BufSetText")
                };
                edits[0].start_row
            })
            .collect();
        assert!(
            rows[0] > rows[1],
            "accept-all must apply bottom-up; got start rows {rows:?}"
        );
        let joins: Vec<bool> = effects
            .iter()
            .map(|effect| {
                let RpcCall::BufSetText { undojoin, .. } = rpc(effect) else {
                    panic!("accept_all emits only BufSetText")
                };
                *undojoin
            })
            .collect();
        assert_eq!(joins, vec![false, true]);
    }

    #[test]
    fn accept_all_skips_a_stale_hunk_rather_than_forcing_it() {
        let mut state = review();
        state.hunks[1].status = HunkStatus::Stale;
        let effects = state.accept_all();
        assert_eq!(effects.len(), 1);
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
        assert!(
            state.close_effect().is_none(),
            "nvim already ended the subscription; asking it to detach again \
             names a subscription that no longer exists"
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

    #[test]
    fn hunk_rows_carry_the_diff_roles_and_never_the_untouched_buffer() {
        let state = review();
        let rows = state.hunk_rows();
        assert_eq!(rows[0], vec![Span::plain("@@ 1..2 @@ fresh")]);
        assert_eq!(rows[1], vec![Span::plain(" a")], "the row above the hunk");
        assert_eq!(rows[2], vec![Span::new("-b", StyleRole::DiffRemoved)]);
        assert_eq!(rows[3], vec![Span::new("+B", StyleRole::DiffAdded)]);
        assert_eq!(rows[4], vec![Span::plain(" c")], "the row below it");
        assert_eq!(
            rows.len(),
            10,
            "two hunks of one removed and one added line each, plus their \
             headers and their own one row of context on each side -- the \
             rows between the hunks are not part of the review's scroll \
             region: {rows:?}"
        );
    }

    #[test]
    fn the_cursor_row_names_the_focused_hunks_own_header() {
        let mut state = review();
        assert_eq!(state.cursor_row(), Some(0));
        state.next_hunk();
        assert_eq!(state.cursor_row(), Some(5));
    }
}
