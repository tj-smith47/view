//! Line hunks computed from an agent's whole-file before/after pair, and
//! the buffer edit each one turns into when the user accepts it.
//!
//! ACP v1 hands a client a `{path, oldText, newText}` triple
//! (`docs/acp-v1-wire-capture.md`'s `Diff` pin), never a hunk list, so the
//! hunk boundaries a reviewer accepts or rejects one at a time are this
//! crate's own to compute. They are computed with `similar` rather than by
//! hand for the reason `view-core`'s manifest states: a wrong boundary here
//! does not render badly, it writes the wrong bytes into a user's buffer.

use crate::msg::TextEdit;
use similar::{capture_diff_slices, Algorithm, DiffOp};

/// Where a hunk stands in the review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HunkStatus {
    /// Still anchored to the buffer text it was computed against, so its
    /// [`Hunk::edits`] describe the rows they were meant to.
    Fresh,
    /// The buffer changed underneath the hunk's [`Hunk::anchor_context`].
    /// Never acceptable in this state -- see [`Hunk::edits`]'s doc for what
    /// force-applying against a vanished anchor would write.
    Stale,
    /// The proposal is in the buffer: either written by an accept, or
    /// already satisfied by what the user typed (see [`Hunk::re_diff`]).
    Accepted,
    /// The user declined it. Terminal: a rejected hunk is never re-offered
    /// by a later re-diff of the same review.
    Rejected,
}

impl HunkStatus {
    /// Whether the review still owes the user a decision on a hunk in this
    /// state. The count the panel's own summary row shows, and the set
    /// [`crate::native::diff::rebase`] keeps anchored.
    #[must_use]
    pub fn is_open(self) -> bool {
        matches!(self, Self::Fresh | Self::Stale)
    }
}

/// One reviewable change: the old buffer rows it replaces, the lines it
/// replaces them with, and the verbatim old text it is anchored on.
///
/// `old_range` is a half-open `[start, end)` range of 0-indexed buffer
/// rows, in the buffer's *current* row space -- [`super::rebase::rebase`]
/// shifts it as the user edits elsewhere, so it never goes stale merely by
/// the buffer moving. `start == end` is a pure insertion at that row.
///
/// `anchor_context` is the verbatim old text of `[anchor_start,
/// anchor_start + anchor_context.len())`: the rows `old_range` covers plus
/// one row of context on each side where the file has one. The surrounding
/// rows are part of the anchor rather than decoration for two reasons: a
/// pure insertion covers no rows of its own and would otherwise have
/// nothing to verify against, and the byte column of the row before and
/// the row after is exactly what [`Self::edits`] needs to address an edit
/// at the end of a buffer, where the row past the last one is not a
/// position `nvim_buf_set_text` accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_range: (u32, u32),
    pub new_lines: Vec<String>,
    pub anchor_start: u32,
    pub anchor_context: Vec<String>,
    pub status: HunkStatus,
    /// Whether the anchor still spans everything a [`Self::re_diff`] would
    /// need: the disputed rows plus the same edges it was computed with.
    ///
    /// A rebase can carry the anchor across an edit only while it can name
    /// every row of the result -- an edit reaching past the anchor's own
    /// window leaves rows whose new text this crate never saw, and an edit
    /// that swallows the context row on one side changes which of
    /// [`Self::edits`]'s three shapes is the correct one. Neither is
    /// recoverable without re-reading the buffer, so the hunk is refused a
    /// re-diff rather than re-diffed against text that is partly a guess.
    pub anchor_intact: bool,
}

impl Hunk {
    /// A [`HunkStatus::Fresh`] hunk. `anchor_context` must be the verbatim
    /// buffer text of the rows starting at `anchor_start`, covering at
    /// least `old_range`; [`diff`] is the only production caller and builds
    /// it that way by construction.
    #[must_use]
    pub fn new(
        old_range: (u32, u32),
        new_lines: Vec<String>,
        anchor_start: u32,
        anchor_context: Vec<String>,
    ) -> Self {
        Self {
            old_range,
            new_lines,
            anchor_start,
            anchor_context,
            status: HunkStatus::Fresh,
            anchor_intact: true,
        }
    }

    /// The row one past the last row the anchor covers.
    #[must_use]
    pub fn anchor_end(&self) -> u32 {
        self.anchor_start
            .saturating_add(u32::try_from(self.anchor_context.len()).unwrap_or(u32::MAX))
    }

    /// Whether the anchor reaches a row above `old_range` -- false only for
    /// a hunk at row 0, which has no row above it to anchor on.
    #[must_use]
    pub fn has_leading_context(&self) -> bool {
        self.anchor_start < self.old_range.0
    }

    /// Whether the anchor reaches a row below `old_range` -- false only for
    /// a hunk that runs to the buffer's last row, which is the case
    /// [`Self::edits`] has to address by byte column rather than by the
    /// row past the end.
    #[must_use]
    pub fn has_trailing_context(&self) -> bool {
        self.anchor_end() > self.old_range.1
    }

    /// The anchor's copy of buffer row `row`, or `None` when the anchor
    /// does not span it.
    #[must_use]
    pub fn anchor_row(&self, row: u32) -> Option<&String> {
        let offset = usize::try_from(row.checked_sub(self.anchor_start)?).ok()?;
        self.anchor_context.get(offset)
    }

    /// The `nvim_buf_set_text` edits that apply this hunk, as one
    /// non-overlapping batch for one [`crate::msg::RpcCall::BufSetText`]
    /// call.
    ///
    /// Correct only while the hunk is anchored: every column here is
    /// derived from [`Self::anchor_context`], so applying this against a
    /// buffer whose anchor rows have changed would address byte columns
    /// that no longer exist on those rows and splice the replacement into
    /// the middle of whatever is there now. That is why an accept is
    /// refused for any status but [`HunkStatus::Fresh`] rather than merely
    /// discouraged.
    ///
    /// Three shapes, picked by where the hunk sits in the buffer:
    ///
    /// - With a row below it, the whole-row span `(start, 0)..(end, 0)` is
    ///   addressable, and the replacement carries a trailing empty line so
    ///   the row below starts on a line of its own rather than being joined
    ///   onto the last replaced row.
    /// - Running to the buffer's last row, `(end, 0)` is one row past the
    ///   end and not a position `nvim_buf_set_text` accepts, so the span
    ///   runs from the end of the row above to the end of the last replaced
    ///   row, and the replacement carries a *leading* empty line instead --
    ///   an empty replacement then deletes the rows outright rather than
    ///   leaving a blank one behind.
    /// - Covering the whole buffer (no row above and none below), the span
    ///   runs from `(0, 0)` to the end of the last row with no added empty
    ///   line at either end.
    #[must_use]
    pub fn edits(&self) -> Vec<TextEdit> {
        let (start, end) = self.old_range;
        if self.has_trailing_context() {
            let mut lines = self.new_lines.clone();
            lines.push(String::new());
            return vec![TextEdit {
                start_row: start,
                start_col: 0,
                end_row: end,
                end_col: 0,
                lines,
            }];
        }
        // Addressed by row position rather than by the anchor's first and
        // last elements: a rebase can leave the anchor holding more than
        // one row of context on a side, and the row that matters is always
        // the one immediately beside `old_range`, never the outermost one
        // the anchor happens to still carry.
        let last_len = self
            .anchor_row(end.saturating_sub(1))
            .map(String::as_str)
            .map_or(0, byte_len);
        if self.has_leading_context() {
            let lead_len = self
                .anchor_row(start.saturating_sub(1))
                .map(String::as_str)
                .map_or(0, byte_len);
            let lines = if self.new_lines.is_empty() {
                Vec::new()
            } else {
                let mut lines = Vec::with_capacity(self.new_lines.len() + 1);
                lines.push(String::new());
                lines.extend(self.new_lines.iter().cloned());
                lines
            };
            return vec![TextEdit {
                start_row: start.saturating_sub(1),
                start_col: lead_len,
                end_row: end.saturating_sub(1),
                end_col: last_len,
                lines,
            }];
        }
        vec![TextEdit {
            start_row: 0,
            start_col: 0,
            end_row: end.saturating_sub(u32::from(end > 0)),
            end_col: last_len,
            lines: self.new_lines.clone(),
        }]
    }

    /// Re-anchors a [`HunkStatus::Stale`] hunk against the buffer text the
    /// rebase kept for it, narrowing `old_range` to the rows that still
    /// actually differ from the proposal.
    ///
    /// The alternative -- applying the original range against text that no
    /// longer matches it -- is the force-apply this whole status machine
    /// exists to make unrepresentable. Re-diffing can also find the
    /// proposal already satisfied (the user typed what the agent proposed,
    /// or undid their own edit), which resolves as [`HunkStatus::Accepted`]
    /// without a write: the buffer holds the proposed text either way, and
    /// leaving it open would ask the user to decide about a change that no
    /// longer exists.
    pub fn re_diff(&mut self) {
        if !self.anchor_intact {
            return;
        }
        let had_trailing = self.has_trailing_context();
        let (start, end) = self.old_range;
        let lo = usize::try_from(start.saturating_sub(self.anchor_start)).unwrap_or(0);
        let hi = usize::try_from(end.saturating_sub(self.anchor_start))
            .unwrap_or(0)
            .clamp(lo, self.anchor_context.len());
        let inner: Vec<&str> = self.anchor_context[lo..hi]
            .iter()
            .map(String::as_str)
            .collect();
        let target: Vec<&str> = self.new_lines.iter().map(String::as_str).collect();
        let ops = capture_diff_slices(Algorithm::Myers, &inner, &target);
        let Some(span) = changed_span(&ops) else {
            self.status = HunkStatus::Accepted;
            return;
        };
        let inner_start = self
            .anchor_start
            .saturating_add(u32::try_from(lo).unwrap_or(0));
        let new_range = (
            inner_start.saturating_add(u32::try_from(span.old.0).unwrap_or(0)),
            inner_start.saturating_add(u32::try_from(span.old.1).unwrap_or(0)),
        );
        let new_lines: Vec<String> = target[span.new.0..span.new.1]
            .iter()
            .map(|line| (*line).to_string())
            .collect();
        // The re-anchored window is a sub-range of the old one, so its own
        // one-row context is always still inside the anchor already held --
        // no read of the buffer is needed to rebuild it. The trailing edge
        // is carried over rather than recomputed: a hunk that ran to the
        // buffer's last row still does, and letting the narrowed range grow
        // a trailing context row it has no buffer row for would flip
        // `edits` onto the wrong one of its three shapes.
        let anchor_lo = new_range.0.saturating_sub(1).max(self.anchor_start);
        let anchor_hi = if had_trailing {
            new_range.1.saturating_add(1).min(self.anchor_end())
        } else {
            new_range.1
        };
        let lo = usize::try_from(anchor_lo.saturating_sub(self.anchor_start)).unwrap_or(0);
        let hi = usize::try_from(anchor_hi.saturating_sub(self.anchor_start))
            .unwrap_or(0)
            .clamp(lo, self.anchor_context.len());
        self.anchor_context = self.anchor_context[lo..hi].to_vec();
        self.anchor_start = anchor_lo;
        self.old_range = new_range;
        self.new_lines = new_lines;
        self.status = HunkStatus::Fresh;
    }
}

/// The `[start, end)` row spans, on each side, that a re-diff found still
/// differing.
struct ChangedSpan {
    old: (usize, usize),
    new: (usize, usize),
}

/// The tightest pair of spans covering every non-`Equal` op, or `None` when
/// the two sequences are identical.
fn changed_span(ops: &[DiffOp]) -> Option<ChangedSpan> {
    let mut span: Option<ChangedSpan> = None;
    for op in ops {
        if matches!(op, DiffOp::Equal { .. }) {
            continue;
        }
        let old = op.old_range();
        let new = op.new_range();
        span = Some(match span {
            None => ChangedSpan {
                old: (old.start, old.end),
                new: (new.start, new.end),
            },
            Some(prev) => ChangedSpan {
                old: (prev.old.0.min(old.start), prev.old.1.max(old.end)),
                new: (prev.new.0.min(new.start), prev.new.1.max(new.end)),
            },
        });
    }
    span
}

/// A line's length in BYTES, which is what `nvim_buf_set_text`'s columns
/// are (see [`TextEdit`]'s own doc): a character count would corrupt every
/// line holding a multi-byte character.
fn byte_len(line: &str) -> u32 {
    u32::try_from(line.len()).unwrap_or(u32::MAX)
}

/// Splits buffer text into rows the way nvim's own buffer does: a trailing
/// newline terminates the last line rather than starting an empty one, and
/// nothing else is stripped.
///
/// Deliberately not `str::lines`, which also strips a `\r` before every
/// `\n`: the text on both sides of this diff is compared verbatim and
/// written back verbatim, so silently dropping a carriage return would
/// rewrite every line of a CRLF file the moment one hunk of it was
/// accepted.
#[must_use]
pub fn split_lines(text: &str) -> Vec<&str> {
    let mut rows: Vec<&str> = text.split('\n').collect();
    if rows.last() == Some(&"") {
        rows.pop();
    }
    rows
}

/// Computes the reviewable hunks between a file's before and after text.
///
/// `old_text: None` is a file that does not exist yet, which has no rows to
/// anchor on at all: the whole proposal is one insertion hunk at row 0, and
/// its empty anchor is what [`super::rebase::rebase`] reads as "this
/// buffer was empty" (any text arriving in it invalidates the premise).
#[must_use]
pub fn diff(old_text: Option<&str>, new_text: &str) -> Vec<Hunk> {
    let old_rows = old_text.map(split_lines).unwrap_or_default();
    let new_rows = split_lines(new_text);
    let ops = capture_diff_slices(Algorithm::Myers, &old_rows, &new_rows);
    let mut hunks = Vec::new();
    for op in &ops {
        if matches!(op, DiffOp::Equal { .. }) {
            continue;
        }
        let old = op.old_range();
        let new = op.new_range();
        let anchor_lo = old.start.saturating_sub(1);
        let anchor_hi = (old.end + 1).min(old_rows.len());
        hunks.push(Hunk::new(
            (
                u32::try_from(old.start).unwrap_or(u32::MAX),
                u32::try_from(old.end).unwrap_or(u32::MAX),
            ),
            new_rows[new.start..new.end]
                .iter()
                .map(|line| (*line).to_string())
                .collect(),
            u32::try_from(anchor_lo).unwrap_or(u32::MAX),
            old_rows[anchor_lo..anchor_hi.max(anchor_lo)]
                .iter()
                .map(|line| (*line).to_string())
                .collect(),
        ));
    }
    hunks
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn owned(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|l| (*l).to_string()).collect()
    }

    #[test]
    fn a_single_changed_line_is_one_replace_hunk_anchored_on_its_neighbours() {
        let hunks = diff(Some("a\nb\nc\n"), "a\nB\nc\n");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_range, (1, 2));
        assert_eq!(hunks[0].new_lines, owned(&["B"]));
        assert_eq!(hunks[0].anchor_start, 0);
        assert_eq!(hunks[0].anchor_context, owned(&["a", "b", "c"]));
        assert_eq!(hunks[0].status, HunkStatus::Fresh);
    }

    #[test]
    fn two_separated_changes_are_two_hunks() {
        let hunks = diff(Some("a\nb\nc\nd\ne\n"), "a\nB\nc\nD\ne\n");
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].old_range, (1, 2));
        assert_eq!(hunks[1].old_range, (3, 4));
        assert_eq!(hunks[1].anchor_context, owned(&["c", "d", "e"]));
    }

    #[test]
    fn a_pure_insertion_has_an_empty_old_range_and_still_anchors_on_context() {
        let hunks = diff(Some("a\nc\n"), "a\nb\nc\n");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_range, (1, 1));
        assert_eq!(hunks[0].new_lines, owned(&["b"]));
        assert_eq!(hunks[0].anchor_start, 0);
        assert_eq!(
            hunks[0].anchor_context,
            owned(&["a", "c"]),
            "an insertion covers no rows of its own, so its anchor is the \
             rows either side of the insertion point"
        );
    }

    #[test]
    fn a_pure_deletion_has_no_new_lines() {
        let hunks = diff(Some("a\nb\nc\n"), "a\nc\n");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_range, (1, 2));
        assert!(hunks[0].new_lines.is_empty());
    }

    #[test]
    fn a_new_file_is_one_add_hunk_with_an_empty_anchor() {
        let hunks = diff(None, "a\nb\n");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_range, (0, 0));
        assert_eq!(hunks[0].new_lines, owned(&["a", "b"]));
        assert!(hunks[0].anchor_context.is_empty());
    }

    #[test]
    fn identical_texts_produce_no_hunks() {
        assert!(diff(Some("a\nb\n"), "a\nb\n").is_empty());
    }

    /// `str::lines` would strip these carriage returns, and an accept would
    /// then rewrite every line of a CRLF file without them.
    #[test]
    fn carriage_returns_survive_the_line_split_verbatim() {
        assert_eq!(split_lines("a\r\nb\r\n"), vec!["a\r", "b\r"]);
        let hunks = diff(Some("a\r\nb\r\n"), "a\r\nB\r\n");
        assert_eq!(hunks[0].new_lines, owned(&["B\r"]));
    }

    #[test]
    fn a_mid_buffer_hunk_edits_the_whole_row_span_with_a_trailing_blank() {
        let hunks = diff(Some("a\nb\nc\n"), "a\nB\nc\n");
        assert_eq!(
            hunks[0].edits(),
            vec![TextEdit {
                start_row: 1,
                start_col: 0,
                end_row: 2,
                end_col: 0,
                lines: owned(&["B", ""]),
            }]
        );
    }

    #[test]
    fn a_mid_buffer_deletion_edits_the_row_span_to_a_single_empty_line() {
        let hunks = diff(Some("a\nb\nc\n"), "a\nc\n");
        assert_eq!(
            hunks[0].edits(),
            vec![TextEdit {
                start_row: 1,
                start_col: 0,
                end_row: 2,
                end_col: 0,
                lines: owned(&[""]),
            }]
        );
    }

    /// The row past the buffer's last one is not a position
    /// `nvim_buf_set_text` accepts, so a hunk that reaches the end is
    /// addressed from the end of the row above instead. The columns are
    /// byte columns, which is what the multi-byte row here pins.
    #[test]
    fn a_hunk_reaching_the_last_row_is_addressed_by_byte_column_from_the_row_above() {
        let hunks = diff(Some("é\nb\n"), "é\nB\n");
        assert_eq!(hunks.len(), 1);
        assert!(!hunks[0].has_trailing_context());
        assert_eq!(
            hunks[0].edits(),
            vec![TextEdit {
                start_row: 0,
                start_col: 2,
                end_row: 1,
                end_col: 1,
                lines: owned(&["", "B"]),
            }]
        );
    }

    #[test]
    fn an_append_past_the_last_row_inserts_after_the_row_above() {
        let hunks = diff(Some("a\n"), "a\nb\n");
        assert_eq!(hunks[0].old_range, (1, 1));
        assert_eq!(
            hunks[0].edits(),
            vec![TextEdit {
                start_row: 0,
                start_col: 1,
                end_row: 0,
                end_col: 1,
                lines: owned(&["", "b"]),
            }]
        );
    }

    /// Deleting through the buffer's end must remove the rows, not leave a
    /// blank one where they were -- so the replacement carries no leading
    /// empty line in this one case.
    #[test]
    fn a_deletion_reaching_the_last_row_removes_the_rows_outright() {
        let hunks = diff(Some("a\nb\nc\n"), "a\n");
        assert_eq!(hunks[0].old_range, (1, 3));
        assert_eq!(
            hunks[0].edits(),
            vec![TextEdit {
                start_row: 0,
                start_col: 1,
                end_row: 2,
                end_col: 1,
                lines: Vec::new(),
            }]
        );
    }

    #[test]
    fn a_whole_buffer_replacement_spans_from_the_origin_to_the_last_rows_end() {
        let hunks = diff(Some("a\nb\n"), "x\ny\n");
        assert_eq!(hunks.len(), 1);
        assert!(!hunks[0].has_leading_context());
        assert!(!hunks[0].has_trailing_context());
        assert_eq!(
            hunks[0].edits(),
            vec![TextEdit {
                start_row: 0,
                start_col: 0,
                end_row: 1,
                end_col: 1,
                lines: owned(&["x", "y"]),
            }]
        );
    }

    #[test]
    fn a_new_files_hunk_inserts_at_the_origin_with_no_added_blank_line() {
        let hunks = diff(None, "a\nb\n");
        assert_eq!(
            hunks[0].edits(),
            vec![TextEdit {
                start_row: 0,
                start_col: 0,
                end_row: 0,
                end_col: 0,
                lines: owned(&["a", "b"]),
            }]
        );
    }

    #[test]
    fn re_diff_narrows_a_stale_hunk_to_the_rows_that_still_differ() {
        // The proposal replaces rows 1..3 with ["B", "C"]; the user has
        // since typed row 1's proposed text themselves, so only row 2 is
        // still owed.
        let mut hunk = Hunk::new((1, 3), owned(&["B", "C"]), 0, owned(&["a", "B", "c", "d"]));
        hunk.status = HunkStatus::Stale;
        hunk.re_diff();
        assert_eq!(hunk.status, HunkStatus::Fresh);
        assert_eq!(hunk.old_range, (2, 3));
        assert_eq!(hunk.new_lines, owned(&["C"]));
        assert_eq!(hunk.anchor_start, 1);
        assert_eq!(hunk.anchor_context, owned(&["B", "c", "d"]));
    }

    #[test]
    fn re_diff_resolves_a_hunk_the_user_already_satisfied_without_a_write() {
        let mut hunk = Hunk::new((1, 2), owned(&["B"]), 0, owned(&["a", "B", "c"]));
        hunk.status = HunkStatus::Stale;
        hunk.re_diff();
        assert_eq!(
            hunk.status,
            HunkStatus::Accepted,
            "the buffer already holds the proposed text, so there is nothing \
             left to ask about and nothing to write"
        );
    }

    #[test]
    fn open_counts_exactly_the_undecided_statuses() {
        assert!(HunkStatus::Fresh.is_open());
        assert!(HunkStatus::Stale.is_open());
        assert!(!HunkStatus::Accepted.is_open());
        assert!(!HunkStatus::Rejected.is_open());
    }
}
