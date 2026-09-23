//! How many terminal columns a piece of text takes, counted one grapheme
//! cluster at a time.
//!
//! The painter that writes a name into cells and the model that answers
//! which name a column names both spend this one count. A width measured
//! beside the painter and a width measured beside the router are two
//! answers to the same question, and the one nobody looks at is the one
//! that goes wrong.

use unicode_segmentation::{Graphemes, UnicodeSegmentation};
use unicode_width::UnicodeWidthStr;

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
    clusters(text)
        .map(cluster_width)
        .fold(0, u16::saturating_add)
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
}
