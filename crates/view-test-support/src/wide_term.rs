//! A terminal model that draws a chosen class of glyphs two columns wide,
//! for the tests that hold a painter to what a user's terminal actually
//! shows.
//!
//! Two models rather than one: [`Widening::Narrow`] draws every glyph at
//! its unicode width -- the assumption a painter makes -- and
//! [`Widening::Ambiguous`] draws box drawing, block elements, geometric
//! shapes, text-presentation pictographs, regional indicators and
//! private-use icons two columns wide, which is what Termius does with a
//! nerd font. Replaying one recording through both and comparing the two
//! screens ([`widening_residue`]) is the residue a user photographs.
//!
//! The code-point table here is written out rather than read from
//! `view_tui::paint::emit::terminal_may_widen`: a model that asked the
//! predicate under test what to do would agree with it however the
//! predicate changed, and a class dropped from it would leave every
//! assertion passing. The independence is also what keeps this crate a
//! leaf -- a fixture crate that depended on `view-tui` could not be a
//! dev-dependency of `view-tui`.

use unicode_width::UnicodeWidthStr;

/// The second half of a glyph the model drew two columns wide.
///
/// A code point no editor paints, so a cell holding it can only be a
/// terminal having covered it.
pub const WIDE_HALF: &str = "\u{0}";

/// Which ruler a [`WideTerm`] sizes glyphs by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Widening {
    /// Every glyph at its unicode width.
    Narrow,
    /// The glyphs [`WideTerm::widens`] answers for at two columns, every
    /// other at its unicode width.
    Ambiguous,
}

/// A terminal grid fed the bytes a painter wrote.
///
/// Interprets what an editor's emission loop and its terminal setup put on
/// the wire: absolute cursor addressing, the relative moves, erase-display
/// and erase-line, the alternate screen, and printable text. Everything
/// else -- SGR, private modes, OSC and DCS strings, character-set
/// designators -- is consumed without touching a cell, which is what a
/// screen comparison needs it to do.
///
/// Assumes a terminal that leaves the left glyph standing when something
/// narrow is written into its second half; an xterm-family terminal erases
/// both halves instead. Nothing in this tree proves which Termius does --
/// the grounds for the assumption are that nvim writes the same sequence
/// and is clean on that device, so a device capture that disagreed would
/// show a vanished icon rather than a shifted row.
pub struct WideTerm {
    grid: Vec<Vec<String>>,
    cols: u16,
    rows: u16,
    x: usize,
    y: usize,
    widening: Widening,
}

impl WideTerm {
    /// A `cols`x`rows` grid of never-painted cells, sizing glyphs by
    /// `widening`.
    #[must_use]
    pub fn new(cols: u16, rows: u16, widening: Widening) -> Self {
        Self {
            grid: vec![vec![" ".to_string(); usize::from(cols)]; usize::from(rows)],
            cols,
            rows,
            x: 0,
            y: 0,
            widening,
        }
    }

    /// The grid's width in columns.
    #[must_use]
    pub fn cols(&self) -> u16 {
        self.cols
    }

    /// The grid's height in rows.
    #[must_use]
    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// What the cell at `(x, y)` shows: a space where nothing was ever
    /// painted, [`WIDE_HALF`] where the glyph to its left covers it, and
    /// the empty string past the grid.
    #[must_use]
    pub fn cell(&self, x: u16, y: u16) -> &str {
        self.grid
            .get(usize::from(y))
            .and_then(|row| row.get(usize::from(x)))
            .map_or("", String::as_str)
    }

    /// Whether a widening terminal draws `symbol` two columns wide.
    ///
    /// Read off the code point, so the answer is independent of whatever a
    /// painter believes about the same glyph.
    #[must_use]
    pub fn widens(symbol: &str) -> bool {
        symbol.chars().any(|c| {
            matches!(c as u32,
                0x2500..=0x257f      // box drawing
                | 0x2580..=0x259f    // block elements
                | 0x25a0..=0x25ff    // geometric shapes
                | 0x270f | 0x2712    // pictographs with text presentation
                | 0x1f1e6..=0x1f1ff  // regional indicators
                | 0xe000..=0xf8ff    // private use
                | 0xf0000..=0xffffd) // supplementary private use
        })
    }

    /// Feeds `bytes` -- a recording of what a child wrote to its terminal
    /// -- through the model.
    ///
    /// Decoded lossily rather than rejected: a recording truncated at a
    /// byte limit ends mid-sequence by construction, and a model that
    /// refused the tail would answer nothing about the frames before it.
    pub fn feed(&mut self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes).into_owned();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\u{1b}' => self.escape(&mut chars),
                '\r' => self.x = 0,
                '\n' => self.y = self.y.saturating_add(1).min(self.last_row()),
                '\u{8}' => self.x = self.x.saturating_sub(1),
                c if c < ' ' => {}
                _ => self.print(c, &mut chars),
            }
        }
    }

    fn last_row(&self) -> usize {
        usize::from(self.rows).saturating_sub(1)
    }

    /// Consumes one escape sequence, acting only on the ones that move the
    /// cursor or erase cells.
    fn escape(&mut self, chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
        match chars.next() {
            Some('[') => {
                let mut params = String::new();
                let mut final_byte = '\0';
                for p in chars.by_ref() {
                    if ('@'..='~').contains(&p) {
                        final_byte = p;
                        break;
                    }
                    params.push(p);
                }
                self.csi(&params, final_byte);
            }
            // a string sequence (OSC, DCS, APC, PM) runs to its own
            // terminator and holds no cell content
            Some(']' | 'P' | '_' | '^') => {
                while let Some(c) = chars.next() {
                    match c {
                        '\u{7}' => break,
                        '\u{1b}' if chars.peek() == Some(&'\\') => {
                            chars.next();
                            break;
                        }
                        _ => {}
                    }
                }
            }
            // two-byte character-set designators: `ESC ( B` is in every
            // nvim startup, and dropping only the ESC would print its `(B`
            // into two cells
            Some('(' | ')' | '*' | '+' | '#' | '%') => {
                chars.next();
            }
            _ => {}
        }
    }

    fn csi(&mut self, params: &str, final_byte: char) {
        // a private sequence (`ESC[?2026h`, `ESC[?1049h`) carries no
        // numeric argument this model reads
        let args: Vec<usize> = if params.starts_with('?') {
            Vec::new()
        } else {
            params.split(';').map(|v| v.parse().unwrap_or(0)).collect()
        };
        let first = args.first().copied().unwrap_or(0);
        let count = first.max(1);
        match final_byte {
            'H' | 'f' => {
                self.y = first.saturating_sub(1).min(self.last_row());
                self.x = args.get(1).copied().unwrap_or(0).saturating_sub(1);
            }
            'J' if first == 2 => self.clear(),
            'K' => self.erase_line(first),
            'A' => self.y = self.y.saturating_sub(count),
            'B' => self.y = self.y.saturating_add(count).min(self.last_row()),
            'C' => self.x = self.x.saturating_add(count),
            'D' => self.x = self.x.saturating_sub(count),
            'G' => self.x = first.saturating_sub(1),
            'd' => self.y = first.saturating_sub(1).min(self.last_row()),
            // entering the alternate screen hands the child a blank grid;
            // a model that carried the old one over would report the rows
            // under a partial first frame as residue
            'h' if params == "?1049" => {
                self.clear();
                self.x = 0;
                self.y = 0;
            }
            _ => {}
        }
    }

    fn clear(&mut self) {
        for row in &mut self.grid {
            for cell in row.iter_mut() {
                " ".clone_into(cell);
            }
        }
    }

    fn erase_line(&mut self, mode: usize) {
        let (from, to) = match mode {
            1 => (0, self.x.saturating_add(1)),
            2 => (0, usize::from(self.cols)),
            _ => (self.x, usize::from(self.cols)),
        };
        if let Some(row) = self.grid.get_mut(self.y) {
            for cell in row.iter_mut().take(to).skip(from) {
                " ".clone_into(cell);
            }
        }
    }

    /// Paints one grapheme cluster and advances the cursor by the columns
    /// the model says it covers.
    fn print(&mut self, c: char, chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
        let mut symbol = String::from(c);
        loop {
            match chars.peek() {
                Some(&('\u{fe0f}' | '\u{ff9e}')) => {
                    if let Some(mark) = chars.next() {
                        symbol.push(mark);
                    }
                }
                Some(&'\u{200d}') => {
                    if let Some(zwj) = chars.next() {
                        symbol.push(zwj);
                    }
                    if let Some(joined) = chars.next() {
                        symbol.push(joined);
                    }
                }
                _ => break,
            }
        }
        let columns = if self.widening == Widening::Ambiguous && Self::widens(&symbol) {
            2
        } else {
            cell_width(&symbol).max(1)
        };
        if let Some(row) = self.grid.get_mut(self.y) {
            if let Some(cell) = row.get_mut(self.x) {
                *cell = symbol;
            }
            for covered in 1..columns {
                if let Some(cell) = row.get_mut(self.x.saturating_add(covered)) {
                    WIDE_HALF.clone_into(cell);
                }
            }
        }
        self.x = self.x.saturating_add(columns);
    }
}

/// The display columns a terminal gives `symbol`.
///
/// `unicode-width` reports the halfwidth voiced sound marks as zero-width
/// because they carry `Grapheme_Extend`, and terminals draw them as a cell
/// of their own; the painter under test sizes its own cells the same way,
/// so a model measuring narrower would feed too few [`WIDE_HALF`] cells to
/// cover what the painter placed.
fn cell_width(symbol: &str) -> usize {
    let marks = symbol
        .chars()
        .filter(|c| matches!(c, '\u{ff9e}' | '\u{ff9f}'))
        .count();
    symbol.width().saturating_add(marks)
}

/// Cells where a widening terminal shows something other than what a narrow
/// one shows, excluding the second half directly under a glyph the widening
/// model holds wide.
///
/// Each entry is `(x, y, narrow, wide)`. An empty answer is the claim a
/// painter has to earn: every cell the widening terminal ended the frame
/// holding is the cell the painter meant to be there.
#[must_use]
pub fn widening_residue(narrow: &WideTerm, wide: &WideTerm) -> Vec<(u16, u16, String, String)> {
    let mut found = Vec::new();
    for y in 0..narrow.rows().min(wide.rows()) {
        for x in 0..narrow.cols().min(wide.cols()) {
            let (shown_narrow, shown_wide) = (narrow.cell(x, y), wide.cell(x, y));
            if shown_narrow == shown_wide {
                continue;
            }
            let covered = shown_wide == WIDE_HALF
                && x > 0
                && WideTerm::widens(wide.cell(x.saturating_sub(1), y));
            if covered {
                continue;
            }
            found.push((x, y, shown_narrow.to_string(), shown_wide.to_string()));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn fed(widening: Widening, bytes: &str) -> WideTerm {
        let mut term = WideTerm::new(10, 3, widening);
        term.feed(bytes.as_bytes());
        term
    }

    #[test]
    fn a_box_drawing_glyph_is_two_columns_only_where_the_terminal_widens_it() {
        let wide = fed(Widening::Ambiguous, "\u{2502}x");
        assert_eq!(wide.cell(0, 0), "\u{2502}");
        assert_eq!(
            wide.cell(1, 0),
            WIDE_HALF,
            "a widening terminal covers the column after a box-drawing glyph"
        );
        assert_eq!(wide.cell(2, 0), "x");

        let narrow = fed(Widening::Narrow, "\u{2502}x");
        assert_eq!(narrow.cell(0, 0), "\u{2502}");
        assert_eq!(
            narrow.cell(1, 0),
            "x",
            "a narrow terminal draws the same glyph in one column"
        );
    }

    #[test]
    fn an_east_asian_wide_glyph_is_two_columns_under_either_model() {
        for widening in [Widening::Narrow, Widening::Ambiguous] {
            let term = fed(widening, "\u{6f22}x");
            assert_eq!(term.cell(0, 0), "\u{6f22}", "{widening:?}");
            assert_eq!(term.cell(1, 0), WIDE_HALF, "{widening:?}");
            assert_eq!(
                term.cell(2, 0),
                "x",
                "{widening:?}: a glyph wide by its own unicode width is not \
                 the terminal's ambiguity"
            );
        }
    }

    #[test]
    fn absolute_addressing_places_a_glyph_at_the_column_it_names() {
        for widening in [Widening::Narrow, Widening::Ambiguous] {
            let term = fed(widening, "\u{1b}[1;1Ha\u{1b}[1;3Hb");
            assert_eq!(term.cell(0, 0), "a", "{widening:?}");
            assert_eq!(term.cell(1, 0), " ", "{widening:?}");
            assert_eq!(term.cell(2, 0), "b", "{widening:?}");
        }
    }

    #[test]
    fn the_tail_a_widening_terminal_never_repainted_is_residue() {
        // a run of glyphs the widening model holds wide, then a repaint of
        // its leftmost cell alone: the narrow terminal overwrites one cell
        // and the widening one leaves the shifted halves of the whole run
        let paint = "\u{1b}[1;1H\u{2502}\u{2502}\u{2502}\u{1b}[1;1Hx";
        let narrow = fed(Widening::Narrow, paint);
        let wide = fed(Widening::Ambiguous, paint);
        let residue = widening_residue(&narrow, &wide);
        assert_eq!(
            residue,
            vec![
                (1, 0, "\u{2502}".to_string(), WIDE_HALF.to_string()),
                (4, 0, " ".to_string(), "\u{2502}".to_string()),
            ],
            "the residue must name the half the repaint left standing and \
             the run's tail that never moved back"
        );

        // the same run addressed cell by cell, which is what a painter that
        // re-syncs after every widening glyph puts on the wire
        let clean = "\u{1b}[1;1H\u{2502}\u{1b}[1;2H\u{2502}\u{1b}[1;3H\u{2502}";
        assert!(
            widening_residue(
                &fed(Widening::Narrow, clean),
                &fed(Widening::Ambiguous, clean)
            )
            .is_empty(),
            "the second half under each widened glyph is not residue"
        );
    }

    #[test]
    fn string_sequences_and_charset_designators_reach_no_cell() {
        let term = fed(
            Widening::Ambiguous,
            "\u{1b}]0;a title\u{7}\u{1b}(B\u{1b}Pq payload \u{1b}\\A",
        );
        assert_eq!(
            term.cell(0, 0),
            "A",
            "the first cell must be the only printed character"
        );
        for x in 1..term.cols() {
            assert_eq!(
                term.cell(x, 0),
                " ",
                "column {x} holds part of a sequence that paints nothing"
            );
        }
    }
}
