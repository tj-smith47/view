//! Native text written into buffer cells one grapheme cluster at a time.
//!
//! The fit check and the run that follows it count the same clusters, in
//! one place, so a name measured here and a name painted here can never
//! disagree about how many cells it takes.

use ratatui::buffer::Buffer;
use ratatui::style::Style;
use unicode_segmentation::{Graphemes, UnicodeSegmentation};
use unicode_width::UnicodeWidthStr;
use view_core::native::views::Span;

/// The cells of `text`, one grapheme cluster each.
///
/// An all-ASCII run is split by byte: every ASCII character is one cluster
/// of one column, and the UAX#29 machine costs more per cell than the write
/// it feeds. Nearly every chrome row this crate paints is ASCII.
pub(super) fn clusters(text: &str) -> Clusters<'_> {
    if text.is_ascii() {
        Clusters::Ascii(text, 0)
    } else {
        Clusters::Segmented(text.graphemes(true))
    }
}

/// [`clusters`]'s two walks.
pub(super) enum Clusters<'a> {
    /// The run and how far into it the walk has reached.
    Ascii(&'a str, usize),
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
/// A cluster rather than a character because a name reaches view in
/// whatever form the filesystem holds it: macOS hands back `café.rs` as
/// `cafe` and a combining mark, and a mark given a cell of its own both
/// moves every character behind it and reads as a stray accent. `max(1)`
/// is what keeps a cluster that measures nothing from leaving the run
/// standing still.
pub(super) fn cluster_width(cluster: &str) -> u16 {
    // an ASCII cluster is one byte and one column, which is most of every
    // row a chrome surface paints; the table lookup is for the rest
    if cluster.len() == 1 {
        return 1;
    }
    u16::try_from(UnicodeWidthStr::width(cluster).max(1)).unwrap_or(1)
}

/// One group of spans' width in terminal cells, which is what a row has
/// room for rather than its count of characters.
pub(super) fn group_width(group: &[Span]) -> u16 {
    group
        .iter()
        .flat_map(|span| clusters(&span.text))
        .map(cluster_width)
        .fold(0, u16::saturating_add)
}

/// Writes one grapheme cluster into a cell as its whole symbol, which is
/// how a combining mark reaches the terminal in the cell its base
/// character stands in.
///
/// A cluster carrying a control character is written as a blank:
/// `ratatui::buffer::Cell::set_symbol` computes the width itself and
/// panics on one in a debug build. The cell is reset before its style is
/// set because `set_style` patches, so a chrome cell painted over the grid
/// would otherwise keep whatever background and modifiers the layer
/// beneath left in it.
pub(super) fn set_cluster(buf: &mut Buffer, x: u16, y: u16, cluster: &str, style: Style) {
    let cell = &mut buf[(x, y)];
    cell.reset();
    if cluster.chars().any(char::is_control) {
        cell.set_symbol(" ");
    } else {
        cell.set_symbol(cluster);
    }
    cell.set_style(style);
}
