//! Event-boundary predicates over a parsed [`vt100::Screen`], the
//! spec-defined observation points every scenario shares: "cell change" is
//! always judged per CELL via the screen's cell API, never by byte counts
//! of a rendered string (multi-byte glyphs make byte counts lie about how
//! many cells hold a character).

use std::hash::{DefaultHasher, Hash, Hasher};

/// Content hash of every cell on screen, for quiescence detection: two
/// equal hashes across a quiet window mean no repaint changed any cell.
#[must_use]
pub fn screen_hash(screen: &vt100::Screen) -> u64 {
    let mut hasher = DefaultHasher::new();
    let (rows, cols) = screen.size();
    for row in 0..rows {
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                cell.contents().hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

/// Number of cells whose contents equal `target` exactly (a one-cell
/// string compare, not a substring/byte scan).
#[must_use]
pub fn count_char_cells(screen: &vt100::Screen, target: &str) -> usize {
    let (rows, cols) = screen.size();
    let mut count = 0;
    for row in 0..rows {
        for col in 0..cols {
            if screen
                .cell(row, col)
                .is_some_and(|cell| cell.contents() == target)
            {
                count += 1;
            }
        }
    }
    count
}

/// Positions of every cell holding `target` exactly, in row-major order.
/// Chrome (a statusline rendering a filename like `scratch.txt`) can
/// legitimately contain the probe character, so probing never assumes
/// uniqueness on the whole screen; callers diff position sets against a
/// pre-probe baseline instead.
#[must_use]
pub fn char_cell_positions(screen: &vt100::Screen, target: &str) -> Vec<(u16, u16)> {
    let (rows, cols) = screen.size();
    let mut positions = Vec::new();
    for row in 0..rows {
        for col in 0..cols {
            if screen
                .cell(row, col)
                .is_some_and(|cell| cell.contents() == target)
            {
                positions.push((row, col));
            }
        }
    }
    positions
}

/// The single position present in `now` but not in `before`, or `None`
/// when zero or more than one new position appeared. Both slices must be
/// row-major sorted, as [`char_cell_positions`] returns them.
#[must_use]
pub fn single_new_position(before: &[(u16, u16)], now: &[(u16, u16)]) -> Option<(u16, u16)> {
    let mut fresh = now.iter().filter(|p| !before.contains(p));
    let first = fresh.next().copied();
    if fresh.next().is_some() {
        return None;
    }
    first
}

/// Whether any cell on screen holds visible (non-empty, non-space)
/// contents.
///
/// Deliberately *not* the first-frame boundary for a paired spawn: view
/// runs nvim as a child and paints its own placeholder chrome well before
/// the engine attaches, while bare nvim's first visible cell is the buffer
/// window itself. Timing the two to this predicate times two different
/// events. Use [`row_holds`] against known fixture content for that.
#[must_use]
pub fn any_visible_cell(screen: &vt100::Screen) -> bool {
    let (rows, cols) = screen.size();
    for row in 0..rows {
        for col in 0..cols {
            if screen
                .cell(row, col)
                .is_some_and(|cell| !cell.contents().trim().is_empty())
            {
                return true;
            }
        }
    }
    false
}

/// Whether `needle` appears anywhere on screen.
///
/// The first-frame boundary for a paired spawn, where `needle` is content
/// only the opened buffer can supply: both sides then time the same event
/// -- the editor showing the file -- rather than whichever chrome each one
/// happens to paint first. A view that stopped attaching its engine
/// entirely still paints chrome, and would time identically under a
/// "something is on screen" predicate.
#[must_use]
pub fn screen_holds(screen: &vt100::Screen, needle: &str) -> bool {
    let (rows, cols) = screen.size();
    (0..rows).any(|row| row_text(screen, row, cols).contains(needle))
}

/// The whole screen as text, one line per row, rendered by [`row_text`].
///
/// The row index and the column count are both `u16` and both come from
/// `screen.size()`, so a caller assembling this itself has two adjacent
/// same-typed arguments to get wrong: transposed, every row reads past the
/// bottom of the screen and comes back empty, and a caller searching the
/// result finds nothing on a screen full of text. Assembled once here
/// instead.
///
/// Whole-screen rather than short-circuiting, so a caller polling for one
/// needle should use [`screen_holds`] instead: this one is for the readers
/// that need every row.
#[must_use]
pub fn screen_lines(screen: &vt100::Screen) -> String {
    let (rows, cols) = screen.size();
    let mut text = String::new();
    for row in 0..rows {
        text.push_str(&row_text(screen, row, cols));
        text.push('\n');
    }
    text
}

/// The first `len` cells of `row` joined into a string, one character per
/// column: cell by cell, so a wide glyph contributes its own contents once
/// rather than a byte-split pair, and a cell with no contents contributes
/// the space it displays.
///
/// The blank case is load-bearing, not tidiness. A terminal cell that was
/// never written holds no contents at all, and a diffing painter never
/// writes a space it can leave blank -- so concatenating raw cell contents
/// silently *deletes* every space from the row. `"view: waiting for
/// nvim..."` came back as `"view:waitingfornvim..."`, and every
/// [`screen_holds`] boundary whose needle contained a space therefore
/// never fired, on a screen plainly showing the text. The word-shaped
/// markers in use hid it: `VIEWBENCHCOLDSTARTMARKER` has no space to lose.
///
/// This matches vt100's own row rendering, which pads the gap before each
/// cell that has contents and skips wide continuations.
#[must_use]
pub fn row_text(screen: &vt100::Screen, row: u16, len: u16) -> String {
    row_text_from(screen, row, 0, len)
}

/// [`row_text`] over `len` columns starting at `col`, for a caller reading
/// a known field out of a row rather than the whole row.
#[must_use]
pub fn row_text_from(screen: &vt100::Screen, row: u16, col: u16, len: u16) -> String {
    let mut text = String::new();
    for offset in 0..len {
        let Some(cell) = screen.cell(row, col.saturating_add(offset)) else {
            break;
        };
        // the preceding wide cell already contributed this column's glyph
        if cell.is_wide_continuation() {
            continue;
        }
        if cell.has_contents() {
            text.push_str(cell.contents());
        } else {
            text.push(' ');
        }
    }
    text
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn screen_from(bytes: &[u8]) -> vt100::Parser {
        let mut parser = vt100::Parser::new(10, 20, 0);
        parser.process(bytes);
        parser
    }

    #[test]
    fn count_char_cells_counts_cells_not_bytes() {
        // one ASCII x plus one two-byte glyph cell; a byte counter looking
        // for single-byte content would miscount this screen
        let parser = screen_from("x\u{e9}x".as_bytes());
        assert_eq!(count_char_cells(parser.screen(), "x"), 2);
        assert_eq!(count_char_cells(parser.screen(), "\u{e9}"), 1);
    }

    #[test]
    fn char_cell_positions_reports_every_match_in_row_major_order() {
        let parser = screen_from(b"xa\r\nbx");
        assert_eq!(
            char_cell_positions(parser.screen(), "x"),
            vec![(0, 0), (1, 1)]
        );
        assert!(char_cell_positions(parser.screen(), "z").is_empty());
    }

    #[test]
    fn single_new_position_diffs_against_chrome_baseline() {
        // chrome already held an x before the probe; only the genuinely
        // new cell counts
        let before = vec![(9, 12)];
        assert_eq!(
            single_new_position(&before, &[(0, 0), (9, 12)]),
            Some((0, 0))
        );
        assert_eq!(single_new_position(&before, &[(9, 12)]), None);
        assert_eq!(
            single_new_position(&before, &[(0, 0), (0, 1), (9, 12)]),
            None
        );
    }

    #[test]
    fn any_visible_cell_ignores_blank_and_space_cells() {
        let blank = screen_from(b"");
        assert!(!any_visible_cell(blank.screen()));
        let spaces = screen_from(b"   ");
        assert!(!any_visible_cell(spaces.screen()));
        let content = screen_from(b"  ~");
        assert!(any_visible_cell(content.screen()));
    }

    #[test]
    fn screen_hash_changes_when_a_cell_changes() {
        let a = screen_from(b"hello");
        let b = screen_from(b"hellp");
        assert_ne!(screen_hash(a.screen()), screen_hash(b.screen()));
        let c = screen_from(b"hello");
        assert_eq!(screen_hash(a.screen()), screen_hash(c.screen()));
    }

    #[test]
    fn row_text_joins_cells_in_column_order() {
        let parser = screen_from(b"L000042 alpha");
        assert_eq!(row_text(parser.screen(), 0, 7), "L000042");
    }

    /// A diffing painter leaves a space it does not have to write, so the
    /// gap cells between two written words hold no contents at all. Read
    /// back naively they vanish and the row's words run together, which is
    /// how a `screen_holds` boundary can miss a phrase the terminal is
    /// plainly showing. Written here the way the real painter emits it:
    /// absolute cursor moves over the gap, never a space.
    ///
    /// Disconfirm: concatenating `cell.contents()` without the blank case
    /// yields `"twowords"`, and the `screen_holds` assertion below fails
    /// on a screen that visibly holds the phrase.
    #[test]
    fn a_column_the_painter_skipped_reads_back_as_the_space_it_shows() {
        let parser = screen_from(b"\x1b[1;1Htwo\x1b[1;7Hwords");

        assert_eq!(row_text(parser.screen(), 0, 12), "two   words ");
        assert!(screen_holds(parser.screen(), "two   words"));
    }

    /// A wide glyph occupies two columns but is one string in the first of
    /// them; its continuation column must contribute nothing, or every
    /// wide character pads the row with a space that is not on screen and
    /// shifts everything after it.
    #[test]
    fn a_wide_glyphs_continuation_column_adds_no_character() {
        let parser = screen_from("a\u{754c}b".as_bytes());

        // four columns, three characters: the glyph owns two of them
        assert_eq!(row_text(parser.screen(), 0, 4), "a\u{754c}b");
        assert_eq!(row_text(parser.screen(), 0, 5), "a\u{754c}b ");
    }
}
