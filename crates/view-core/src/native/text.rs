//! How many terminal columns a piece of text takes, counted one grapheme
//! cluster at a time.
//!
//! The painter that writes a name into cells and the model that answers
//! which name a column names both spend this one count. A width measured
//! beside the painter and a width measured beside the router are two
//! answers to the same question, and the one nobody looks at is the one
//! that goes wrong.

use unicode_segmentation::{Graphemes, UnicodeSegmentation};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::views::Span;

/// The cells of `text`, one grapheme cluster each.
///
/// An all-ASCII run is split by byte: every ASCII character is one cluster
/// of one column, and the UAX#29 machine costs more per cell than the
/// write it feeds. Nearly every chrome row view paints is ASCII.
#[must_use]
pub fn clusters(text: &str) -> Clusters<'_> {
    if text.is_ascii() {
        Clusters::Ascii(text, 0)
    } else {
        Clusters::Segmented(text.graphemes(true))
    }
}

/// [`clusters`]'s two walks.
#[non_exhaustive]
pub enum Clusters<'a> {
    /// The run and how far into it the walk has reached.
    Ascii(&'a str, usize),
    /// The UAX#29 walk, for text carrying anything else.
    Segmented(Graphemes<'a>),
}

impl<'a> Iterator for Clusters<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        match self {
            Self::Ascii(text, at) => {
                let run: &'a str = text;
                let next = run.get(*at..at.saturating_add(1))?;
                *at = at.saturating_add(1);
                Some(next)
            }
            Self::Segmented(graphemes) => graphemes.next(),
        }
    }
}

impl DoubleEndedIterator for Clusters<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        match self {
            Self::Ascii(text, at) => {
                let run: &str = text;
                let end = run.len().checked_sub(1).filter(|end| end >= at)?;
                let (rest, last) = (run.get(..end)?, run.get(end..)?);
                *text = rest;
                Some(last)
            }
            Self::Segmented(graphemes) => graphemes.next_back(),
        }
    }
}

/// The cells one grapheme cluster takes, which is both what a fit check
/// counts and what a run advances by.
///
/// A cluster, whatever its count of characters, because a name reaches
/// view in whatever form the filesystem holds it: macOS hands back `café.rs` as
/// `cafe` and a combining mark, and a mark given a cell of its own both
/// moves every character behind it and reads as a stray accent. `max(1)`
/// is what keeps a cluster that measures nothing from leaving the run
/// standing still.
#[must_use]
pub fn cluster_width(cluster: &str) -> u16 {
    // an ASCII cluster is one byte and one column, which is most of every
    // row a chrome surface paints; the table lookup is for the rest
    if cluster.len() == 1 {
        return 1;
    }
    u16::try_from(UnicodeWidthStr::width(cluster).max(1)).unwrap_or(1)
}

/// One string's width in terminal cells.
#[must_use]
pub fn text_width(text: &str) -> u16 {
    text_width_by(text, cluster_width)
}

/// One string's width in terminal cells, each grapheme cluster measured by
/// `cluster`, saturating at `u16::MAX`.
#[must_use]
pub fn text_width_by(text: &str, cluster: impl Fn(&str) -> u16) -> u16 {
    clusters(text).map(cluster).fold(0, u16::saturating_add)
}

/// One group of spans' width in terminal cells, which is what a row has
/// room for, whatever its count of characters.
#[must_use]
pub fn group_width(group: &[Span]) -> u16 {
    group
        .iter()
        .map(|span| text_width(&span.text))
        .fold(0, u16::saturating_add)
}

/// The glyph a cut text ends with, so a shortened one reads as cut.
pub const TRUNCATION_MARK: char = '…';

/// The longest prefix of `text` that leaves room for [`TRUNCATION_MARK`]
/// within `budget` cells and ends in no whitespace, and the cells that
/// prefix takes.
///
/// `cells` is `text`'s [`text_width`]: the cut subtracts each cluster it
/// drops by [`cluster_width`], so a count taken any other way describes a
/// prefix other than the one returned.
#[must_use]
pub fn cut_before_mark(text: &str, cells: u16, budget: u16) -> (&str, u16) {
    let mark_cells = u16::try_from(TRUNCATION_MARK.width().unwrap_or(1)).unwrap_or(1);
    let mut end = text.len();
    let mut cells = cells;
    let mut back = clusters(text);
    while let Some(last) = back.next_back() {
        // a space before the mark reads as a gap in the text
        if cells.saturating_add(mark_cells) <= budget && !last.trim().is_empty() {
            break;
        }
        end = end.saturating_sub(last.len());
        cells = cells.saturating_sub(cluster_width(last));
    }
    (text.get(..end).unwrap_or_default(), cells)
}

/// One line broken into rows of at most `width` cells: at the last space
/// that fits, and at the cell where a word is wider than the row. A line
/// keeps at least one row, so an empty line stays a row.
#[must_use]
pub fn wrap_line(line: &str, width: usize) -> Vec<String> {
    let mut rows = vec![String::new()];
    let mut used = 0_usize;
    for cluster in clusters(line) {
        let cells = usize::from(cluster_width(cluster));
        if used > 0 && used.saturating_add(cells) > width {
            if cluster == " " {
                rows.push(String::new());
                used = 0;
                continue;
            }
            // the unfinished word moves down with the break, and the space
            // before it goes, since a row ending in one reads as nothing
            let carried = rows.last_mut().and_then(|row| {
                let space = row.rfind(' ').filter(|&at| at > 0)?;
                let word = row.split_off(space.saturating_add(1));
                row.truncate(space);
                Some(word)
            });
            let word = carried.unwrap_or_default();
            used = usize::from(text_width(&word));
            rows.push(word);
        }
        if let Some(row) = rows.last_mut() {
            row.push_str(cluster);
        }
        used = used.saturating_add(cells);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The case the measure exists for: a decomposed accent is one cluster
    /// of one cell, and takes no cell of its own.
    #[test]
    fn a_decomposed_accent_takes_no_cell_of_its_own() {
        assert_eq!(text_width("cafe\u{301}.rs"), 7);
        assert_eq!(text_width("café.rs"), 7);
        assert_eq!(clusters("cafe\u{301}.rs").count(), 7);
    }

    #[test]
    fn a_wide_glyph_takes_two_cells_and_a_zero_width_cluster_takes_one() {
        assert_eq!(text_width("日本"), 4);
        assert_eq!(cluster_width("\u{301}"), 1);
    }

    #[test]
    fn an_ascii_run_walks_one_byte_per_cell() {
        assert_eq!(clusters("main.rs").collect::<Vec<_>>().len(), 7);
        assert_eq!(text_width("main.rs"), 7);
    }

    #[test]
    fn a_walk_from_both_ends_meets_once_in_the_middle() {
        for text in ["abc", "a❤\u{fe0f}c"] {
            let mut walk = clusters(text);
            assert_eq!(walk.next(), Some("a"), "{text:?}");
            assert_eq!(walk.next_back(), Some("c"), "{text:?}");
            assert!(walk.next_back().is_some(), "{text:?}");
            assert_eq!((walk.next(), walk.next_back()), (None, None), "{text:?}");
        }
    }

    /// The cut counts a dropped cluster the way [`text_width`] counted it,
    /// so an emoji carrying a variation selector or a ZWJ sequence leaves
    /// the returned width equal to the returned prefix's.
    #[test]
    fn a_cut_returns_the_width_of_the_prefix_it_keeps() {
        for (text, budget, kept) in [
            ("x❤\u{fe0f}yz", 4, "x❤\u{fe0f}"),
            ("x❤\u{fe0f}yz", 3, "x"),
            ("a👨\u{200d}👩\u{200d}👧bc", 4, "a👨\u{200d}👩\u{200d}👧"),
            ("a👨\u{200d}👩\u{200d}👧bc", 3, "a"),
            ("ab cd", 4, "ab"),
        ] {
            let (prefix, cells) = cut_before_mark(text, text_width(text), budget);
            assert_eq!(
                (prefix, cells),
                (kept, text_width(kept)),
                "{text:?} at {budget}"
            );
        }
    }
}
