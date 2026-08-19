//! Keeping open hunks anchored while the user goes on editing the buffer
//! underneath them.
//!
//! Every keystroke in an attached buffer produces one
//! [`crate::msg::Msg::BufTextChanged`], and every one of those reaches
//! [`rebase`] while a review is open -- so this runs on the key-dispatch
//! path and its cost is a direct addend to that message's own O(edit size)
//! budget. It stays O(open hunks) per call, each hunk's own work bounded by
//! that hunk's line count and never by the buffer's, and allocation-free
//! for the common case of an edit outside every hunk's anchor: an edit
//! elsewhere in the file only shifts `u32` row offsets in place.

use super::hunk::{Hunk, HunkStatus};
use crate::msg::BufferHandle;

/// One `nvim_buf_lines_event`, as plain data this crate can fold without
/// naming an engine type: exactly the fields
/// [`crate::msg::Msg::BufTextChanged`] carries, and with the same meanings
/// (see that variant's own doc, which is the contract this reads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufTextChangedEvent {
    pub buf: BufferHandle,
    pub generation: u64,
    pub firstline: u64,
    pub lastline: u64,
    pub linedata: Vec<String>,
    pub changedtick: u64,
    pub desynced: bool,
}

/// Folds one buffer change into every hunk of one review.
///
/// A change strictly outside a hunk's anchor shifts its row offsets and
/// nothing else; a change inside the anchor re-checks the anchor against
/// the text the event carries and, when it no longer matches verbatim,
/// carries the anchor forward over the change and marks the hunk
/// [`HunkStatus::Stale`] -- never leaving it `Fresh` against context that
/// no longer exists, which is what would let an accept write its bytes over
/// rows the user has since changed.
///
/// `desynced` short-circuits all of that. The flag means a prior event for
/// this buffer was dropped or failed to decode, so the incremental state
/// here describes a buffer version that no longer exists and no fold can
/// repair it: every open hunk goes `Stale` with its anchor marked
/// unusable, which leaves reject (or closing the review) as the only
/// actions the panel offers. Folding this event in as if it followed the
/// last one is the single failure that would silently write bytes at the
/// wrong rows.
pub fn rebase(hunks: &mut [Hunk], change: &BufTextChangedEvent) {
    if change.desynced {
        for hunk in hunks.iter_mut() {
            if hunk.status.is_open() {
                hunk.status = HunkStatus::Stale;
                hunk.anchor_intact = false;
            }
        }
        return;
    }
    let removed = change.lastline.saturating_sub(change.firstline);
    let added = u64::try_from(change.linedata.len()).unwrap_or(u64::MAX);
    let delta = i64::try_from(added).unwrap_or(i64::MAX) - i64::try_from(removed).unwrap_or(0);
    for hunk in hunks.iter_mut() {
        fold(hunk, change, added, delta);
    }
}

/// Marks every open hunk [`HunkStatus::Stale`] with an unusable anchor, the
/// terminal state for a review whose buffer can no longer report its edits
/// at all -- an nvim-initiated detach (`Msg::BufDetached`), where no
/// further event will ever arrive to rebase against.
pub fn stale_all(hunks: &mut [Hunk]) {
    for hunk in hunks.iter_mut() {
        if hunk.status.is_open() {
            hunk.status = HunkStatus::Stale;
            hunk.anchor_intact = false;
        }
    }
}

fn fold(hunk: &mut Hunk, change: &BufTextChangedEvent, added: u64, delta: i64) {
    let anchor_start = u64::from(hunk.anchor_start);
    let anchor_end = u64::from(hunk.anchor_end());
    // A resolved hunk is no longer a decision the user owes, but its rows
    // are still drawn beside the open ones, so its offsets are kept honest
    // for an edit above it and left alone for anything else.
    if !hunk.status.is_open() {
        if change.lastline <= anchor_start {
            shift(hunk, delta);
        }
        return;
    }
    // An empty anchor is the new-file hunk's own premise: there was no
    // buffer text to anchor on because the file did not exist. Any line
    // arriving in it retires that premise, and no offset arithmetic can
    // stand in for text this crate never saw.
    if hunk.anchor_context.is_empty() {
        if added > 0 {
            hunk.status = HunkStatus::Stale;
            hunk.anchor_intact = false;
        }
        return;
    }
    if change.lastline <= anchor_start {
        shift(hunk, delta);
        return;
    }
    if change.firstline >= anchor_end {
        return;
    }
    if change.firstline < anchor_start || change.lastline > anchor_end {
        // The change reaches past the anchor's own window, so its result
        // includes rows whose new text this event never named.
        hunk.status = HunkStatus::Stale;
        hunk.anchor_intact = false;
        return;
    }
    let Ok(pre) = usize::try_from(change.firstline - anchor_start) else {
        hunk.anchor_intact = false;
        hunk.status = HunkStatus::Stale;
        return;
    };
    let Ok(suf) = usize::try_from(change.lastline - anchor_start) else {
        hunk.anchor_intact = false;
        hunk.status = HunkStatus::Stale;
        return;
    };
    let pre = pre.min(hunk.anchor_context.len());
    let suf = suf.clamp(pre, hunk.anchor_context.len());
    if hunk.anchor_context[pre..suf] == change.linedata[..] {
        // The rows the edit touched came back identical -- an undo, or a
        // replacement with the same text. The anchor still matches
        // verbatim, so there is nothing stale about the hunk.
        return;
    }
    let had_leading = hunk.has_leading_context();
    let had_trailing = hunk.has_trailing_context();
    hunk.anchor_context
        .splice(pre..suf, change.linedata.iter().cloned());
    let (start, end) = hunk.old_range;
    let start = if change.lastline <= u64::from(start) {
        shift_row(start, delta)
    } else {
        start.min(row_of(change.firstline))
    };
    let end = if change.lastline <= u64::from(end) {
        shift_row(end, delta)
    } else if change.firstline >= u64::from(end) {
        end
    } else {
        shift_row(hunk.old_range.1, delta).max(row_of(change.firstline.saturating_add(added)))
    };
    hunk.old_range = (start, end.max(start));
    hunk.status = HunkStatus::Stale;
    if hunk.has_leading_context() != had_leading || hunk.has_trailing_context() != had_trailing {
        hunk.anchor_intact = false;
    }
}

fn shift(hunk: &mut Hunk, delta: i64) {
    hunk.old_range = (
        shift_row(hunk.old_range.0, delta),
        shift_row(hunk.old_range.1, delta),
    );
    hunk.anchor_start = shift_row(hunk.anchor_start, delta);
}

fn shift_row(row: u32, delta: i64) -> u32 {
    u32::try_from(i64::from(row).saturating_add(delta).max(0)).unwrap_or(u32::MAX)
}

fn row_of(line: u64) -> u32 {
    u32::try_from(line).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::super::hunk::diff;
    use super::*;

    fn owned(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|l| (*l).to_string()).collect()
    }

    fn change(firstline: u64, lastline: u64, linedata: &[&str]) -> BufTextChangedEvent {
        BufTextChangedEvent {
            buf: BufferHandle(1),
            generation: 7,
            firstline,
            lastline,
            linedata: owned(linedata),
            changedtick: 2,
            desynced: false,
        }
    }

    /// Two hunks, five rows apart, and an edit that lands inside the
    /// second's anchor: the first stays `Fresh` at its own rows, the second
    /// goes `Stale`. The falsifiable check the brief states, at the rebase
    /// layer.
    #[test]
    fn an_edit_inside_one_hunks_anchor_stales_only_that_hunk() {
        let old = "a\nb\nc\nd\ne\nf\ng\n";
        let new = "a\nB\nc\nd\ne\nF\ng\n";
        let mut hunks = diff(Some(old), new);
        assert_eq!(hunks.len(), 2);
        rebase(&mut hunks, &change(5, 6, &["f edited"]));
        assert_eq!(hunks[0].status, HunkStatus::Fresh);
        assert_eq!(hunks[0].old_range, (1, 2));
        assert_eq!(hunks[1].status, HunkStatus::Stale);
        assert_eq!(
            hunks[1].anchor_context,
            owned(&["e", "f edited", "g"]),
            "the anchor carries the new text forward so a re-diff has \
             something real to narrow against"
        );
    }

    #[test]
    fn an_edit_above_every_hunk_only_shifts_their_rows() {
        let mut hunks = diff(Some("a\nb\nc\nd\ne\n"), "a\nb\nc\nD\ne\n");
        assert_eq!(hunks[0].old_range, (3, 4));
        rebase(&mut hunks, &change(0, 0, &["x", "y"]));
        assert_eq!(hunks[0].status, HunkStatus::Fresh);
        assert_eq!(hunks[0].old_range, (5, 6));
        assert_eq!(hunks[0].anchor_start, 4);
    }

    #[test]
    fn an_edit_below_every_hunk_changes_nothing() {
        let mut hunks = diff(Some("a\nb\nc\nd\ne\n"), "a\nB\nc\nd\ne\n");
        let before = hunks.clone();
        rebase(&mut hunks, &change(4, 5, &["E"]));
        assert_eq!(hunks, before);
    }

    #[test]
    fn a_deletion_above_a_hunk_shifts_it_up() {
        let mut hunks = diff(Some("a\nb\nc\nd\ne\n"), "a\nb\nc\nD\ne\n");
        rebase(&mut hunks, &change(0, 2, &[]));
        assert_eq!(hunks[0].old_range, (1, 2));
        assert_eq!(hunks[0].status, HunkStatus::Fresh);
    }

    /// The disconfirm for the stale check itself: an offsets-only rebase
    /// would leave this `Fresh`, and an accept would then write the
    /// proposal over rows the user had already replaced.
    #[test]
    fn an_edit_replacing_a_hunks_own_rows_stales_it_rather_than_shifting_it() {
        let mut hunks = diff(Some("a\nb\nc\n"), "a\nB\nc\n");
        rebase(&mut hunks, &change(1, 2, &["something else entirely"]));
        assert_eq!(hunks[0].status, HunkStatus::Stale);
        assert!(hunks[0].anchor_intact);
    }

    #[test]
    fn an_edit_that_restores_the_same_text_leaves_the_hunk_fresh() {
        let mut hunks = diff(Some("a\nb\nc\n"), "a\nB\nc\n");
        rebase(&mut hunks, &change(1, 2, &["b"]));
        assert_eq!(
            hunks[0].status,
            HunkStatus::Fresh,
            "the anchor still matches verbatim, so nothing about the hunk \
             is stale"
        );
    }

    /// The desync contract: a dropped event means the state here describes
    /// a buffer version that no longer exists, so nothing may be folded in
    /// on top of it.
    #[test]
    fn a_desynced_event_stales_every_open_hunk_and_folds_nothing() {
        let mut hunks = diff(Some("a\nb\nc\nd\ne\n"), "a\nB\nc\nD\ne\n");
        let before = hunks.clone();
        let mut desynced = change(0, 0, &["x"]);
        desynced.desynced = true;
        rebase(&mut hunks, &desynced);
        for (hunk, was) in hunks.iter().zip(&before) {
            assert_eq!(hunk.status, HunkStatus::Stale);
            assert!(!hunk.anchor_intact);
            assert_eq!(
                hunk.old_range, was.old_range,
                "a desynced event carries no trustworthy geometry, so its \
                 own firstline must not shift anything"
            );
        }
    }

    #[test]
    fn a_desynced_event_leaves_resolved_hunks_alone() {
        let mut hunks = diff(Some("a\nb\nc\n"), "a\nB\nc\n");
        hunks[0].status = HunkStatus::Accepted;
        let mut desynced = change(0, 0, &["x"]);
        desynced.desynced = true;
        rebase(&mut hunks, &desynced);
        assert_eq!(hunks[0].status, HunkStatus::Accepted);
    }

    #[test]
    fn stale_all_retires_every_open_hunks_anchor() {
        let mut hunks = diff(Some("a\nb\nc\nd\ne\n"), "a\nB\nc\nD\ne\n");
        hunks[1].status = HunkStatus::Rejected;
        stale_all(&mut hunks);
        assert_eq!(hunks[0].status, HunkStatus::Stale);
        assert!(!hunks[0].anchor_intact);
        assert_eq!(hunks[1].status, HunkStatus::Rejected);
    }

    /// An edit reaching past the anchor's window leaves rows this crate
    /// never saw the text of, so the hunk may not be re-diffed against a
    /// partly-guessed anchor.
    #[test]
    fn an_edit_spanning_past_the_anchor_retires_the_anchor() {
        let mut hunks = diff(Some("a\nb\nc\nd\ne\n"), "a\nb\nc\nD\ne\n");
        rebase(&mut hunks, &change(0, 5, &["one line now"]));
        assert_eq!(hunks[0].status, HunkStatus::Stale);
        assert!(!hunks[0].anchor_intact);
    }

    /// A new-file hunk has no text to anchor on, so its premise -- that
    /// the file does not exist -- is retired by any line appearing.
    #[test]
    fn an_edit_into_a_new_files_buffer_stales_its_add_hunk() {
        let mut hunks = diff(None, "a\nb\n");
        rebase(&mut hunks, &change(0, 0, &["typed by hand"]));
        assert_eq!(hunks[0].status, HunkStatus::Stale);
        assert!(!hunks[0].anchor_intact);
    }

    #[test]
    fn a_resolved_hunk_still_tracks_rows_inserted_above_it() {
        let mut hunks = diff(Some("a\nb\nc\n"), "a\nB\nc\n");
        hunks[0].status = HunkStatus::Accepted;
        rebase(&mut hunks, &change(0, 0, &["x"]));
        assert_eq!(hunks[0].old_range, (2, 3));
        assert_eq!(hunks[0].status, HunkStatus::Accepted);
    }

    /// An edit that deletes the row the hunk's trailing context sits on
    /// changes which of `edits`'s three shapes is correct, and no offset
    /// arithmetic recovers that -- so the anchor is retired rather than
    /// silently reshaped.
    #[test]
    fn an_edit_swallowing_the_trailing_context_row_retires_the_anchor() {
        let mut hunks = diff(Some("a\nb\nc\n"), "a\nB\nc\n");
        assert!(hunks[0].has_trailing_context());
        rebase(&mut hunks, &change(2, 3, &[]));
        assert_eq!(hunks[0].status, HunkStatus::Stale);
        assert!(!hunks[0].anchor_intact);
    }

    /// The rebase-then-re-diff round trip the review's own re-diff action
    /// depends on: a concurrent edit inside the anchor stales the hunk,
    /// and re-diffing narrows it onto the rows that still differ, with an
    /// anchor good enough to address the edit by byte column.
    #[test]
    fn a_staled_hunk_re_diffs_against_the_text_the_rebase_carried_forward() {
        let mut hunks = diff(Some("a\nb\nc\n"), "a\nB\nc\n");
        rebase(&mut hunks, &change(0, 1, &["a edited"]));
        assert_eq!(hunks[0].status, HunkStatus::Stale);
        hunks[0].re_diff();
        assert_eq!(hunks[0].status, HunkStatus::Fresh);
        assert_eq!(hunks[0].old_range, (1, 2));
        assert_eq!(hunks[0].anchor_context, owned(&["a edited", "b", "c"]));
    }

    #[test]
    fn a_hunk_with_a_retired_anchor_refuses_to_re_diff() {
        let mut hunks = diff(Some("a\nb\nc\n"), "a\nB\nc\n");
        stale_all(&mut hunks);
        let before = hunks[0].clone();
        hunks[0].re_diff();
        assert_eq!(hunks[0], before);
    }
}
