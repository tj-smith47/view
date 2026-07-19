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
/// contents; the first-frame boundary for a freshly spawned process.
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

/// The first `len` cells of `row` joined into a string (cell by cell, so a
/// wide glyph contributes its own contents once, not a byte-split pair).
#[must_use]
pub fn row_text(screen: &vt100::Screen, row: u16, len: u16) -> String {
    let mut text = String::new();
    for col in 0..len {
        if let Some(cell) = screen.cell(row, col) {
            text.push_str(cell.contents());
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
}
