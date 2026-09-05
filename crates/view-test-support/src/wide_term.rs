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
//! What widens is read from the painter's own question -- never from the
//! painter: a model that asked `view_tui::paint::emit::terminal_may_widen`
//! what to do would agree with it however that predicate changed, and a
//! class dropped from it would leave every assertion passing, quite apart
//! from a fixture crate that depended on `view-tui` being unable to be a
//! dev-dependency of `view-tui`. The East Asian half comes from
//! `unicode-width`'s own tables instead, because a range written out by
//! hand is a claim about Unicode that goes wrong silently: the block
//! elements written as `2580..=259f` widened U+2590 and U+2591, which
//! Unicode calls Neutral and no terminal draws two columns wide, and the
//! second half the model invented for one of them read as residue no
//! painter could have repainted. The classes that widen for a reason other
//! than East Asian ambiguity stay written out, because no table answers
//! them.

use unicode_properties::emoji::is_regional_indicator;
use unicode_properties::UnicodeEmoji;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
/// A glyph written over one the model drew two columns wide takes the
/// second half with it: a terminal owns the cell pair, so the half of a
/// glyph whose owner is gone cannot still be shown. The other direction --
/// a write into the second half while the glyph itself stands -- leaves the
/// glyph, which is charitable next to an xterm-family terminal that erases
/// both halves, and is the only assumption under which a residue-free frame
/// is reachable at all: an editor that believes a glyph one column wide
/// writes the next cell into the second half of every such glyph it draws,
/// so a model erasing the left half reports the pinned nvim's own recording
/// of a window split as 120 residue cells and holds a painter to a bar its
/// reference does not meet.
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
    /// East_Asian_Width = Ambiguous -- box drawing, block elements,
    /// geometric shapes, the private-use planes a nerd font fills, and
    /// every ambiguous letter and sign besides -- read off `unicode-width`'s
    /// two rulers rather than a hand-written range, plus the classes
    /// ambiguity does not answer for, read off the properties that do:
    /// Emoji, whose text-presentation members are Neutral to both rulers,
    /// and Regional_Indicator.
    #[must_use]
    pub fn widens(symbol: &str) -> bool {
        symbol
            .chars()
            .any(|c| c.width() != c.width_cjk() || widens_unambiguously(c))
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
        self.blank_orphaned_half(to);
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
        let past = self.x.saturating_add(columns);
        self.blank_orphaned_half(past);
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
        self.x = past;
    }

    /// Blanks the trailing halves a write ending at `past` orphaned: the
    /// glyph that owned them sat inside the write and is gone.
    ///
    /// Only the column at `past` can hold such a half -- one inside the
    /// write is overwritten by the write itself, and one before it still
    /// has its owner.
    fn blank_orphaned_half(&mut self, past: usize) {
        let Some(row) = self.grid.get_mut(self.y) else {
            return;
        };
        let mut at = past;
        while row.get(at).is_some_and(|cell| cell == WIDE_HALF) {
            if let Some(cell) = row.get_mut(at) {
                " ".clone_into(cell);
            }
            at = at.saturating_add(1);
        }
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

/// Whether a terminal may draw `c` two columns wide for a reason East Asian
/// ambiguity does not carry.
///
/// Read off the Emoji and Regional_Indicator properties. Dozens of
/// pictographs whose default presentation is text -- U+2328 KEYBOARD,
/// U+23F8 PAUSE, U+1F5A5 DESKTOP COMPUTER -- are `Neutral`, so neither
/// ruler in [`WideTerm::widens`] reaches them, and a terminal that draws
/// their emoji presentation gives them two columns; a regional-indicator
/// pair is one flag for the same reason and by the same means. A range
/// written out by hand instead named two of them and left the rest of the
/// property unmodelled.
///
/// Above ASCII, because `#`, `*` and the ten digits are `Emoji=Yes` -- they
/// are the bases of the keycap sequences -- and no terminal draws a digit
/// two columns wide for standing next to U+20E3.
fn widens_unambiguously(c: char) -> bool {
    c as u32 >= 0x80 && (c.is_emoji_char() || is_regional_indicator(c))
}

/// Cells where a widening terminal shows something other than what a narrow
/// one shows, excluding the second half of a glyph where nothing was meant
/// to show under it or where no painter could have written there.
///
/// Each entry is `(x, y, narrow, wide)`. An empty answer is the claim a
/// painter has to earn: every cell the widening terminal ended the frame
/// holding is the cell the painter meant to be there.
///
/// A second half over a cell the narrow model shows content in is the
/// user's own symptom -- a glyph that vanished under its neighbour -- and a
/// painter that addresses the column writes the glyph back over the half,
/// so it is not excused. The two shapes that are excused are the ones no
/// emission can reach: a half over a blank, which costs the user nothing,
/// and the second column of a regional-indicator pair, which is one cell to
/// the painter's own grid and so has no column of its own to address.
#[must_use]
pub fn widening_residue(narrow: &WideTerm, wide: &WideTerm) -> Vec<(u16, u16, String, String)> {
    let mut found = Vec::new();
    for y in 0..narrow.rows().min(wide.rows()) {
        for x in 0..narrow.cols().min(wide.cols()) {
            let (shown_narrow, shown_wide) = (narrow.cell(x, y), wide.cell(x, y));
            if shown_narrow == shown_wide {
                continue;
            }
            let left = wide.cell(x.saturating_sub(1), y);
            let covered = shown_wide == WIDE_HALF
                && x > 0
                && WideTerm::widens(left)
                && (shown_narrow == " " || left.chars().any(is_regional_indicator));
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
    fn a_glyph_left_under_its_widened_neighbour_is_residue() {
        // the shape a user photographs: a run of two glyphs a terminal draws
        // wide, then a repaint of the head that stops one column short of
        // the run, so the second glyph stays hidden under the first's own
        // second half
        let painted = "\u{1b}[1;1H\u{2502}\u{252c}";
        let short = format!("{painted}\u{1b}[1;1H\u{2502}");
        assert_eq!(
            widening_residue(
                &fed(Widening::Narrow, &short),
                &fed(Widening::Ambiguous, &short)
            ),
            vec![
                (1, 0, "\u{252c}".to_string(), WIDE_HALF.to_string()),
                (2, 0, " ".to_string(), "\u{252c}".to_string()),
            ],
            "a second half over a cell the narrow terminal shows a glyph in \
             is the glyph the user cannot see, not an unavoidable cover"
        );

        // the same repaint carried one column further, which is what a
        // painter that follows the run to its right puts on the wire
        let reached = format!("{painted}\u{1b}[1;1H\u{2502}\u{1b}[1;2H\u{252c}");
        assert!(
            widening_residue(
                &fed(Widening::Narrow, &reached),
                &fed(Widening::Ambiguous, &reached)
            )
            .is_empty(),
            "a second half over a blank is what a widening terminal costs \
             the user and no emission can avoid"
        );
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
                (1, 0, "\u{2502}".to_string(), " ".to_string()),
                (4, 0, " ".to_string(), "\u{2502}".to_string()),
            ],
            "the residue must name the column the overwritten glyph left \
             blank and the run's tail that never moved back"
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
    fn a_glyph_written_over_a_widened_one_takes_its_second_half_with_it() {
        let over_ambiguous = fed(Widening::Ambiguous, "\u{1b}[1;1H\u{2502}\u{1b}[1;1Hx");
        assert_eq!(over_ambiguous.cell(0, 0), "x");
        assert_eq!(
            over_ambiguous.cell(1, 0),
            " ",
            "the half of a glyph that is gone was left standing"
        );

        // the same for a glyph wide by its own unicode width, which every
        // model draws two columns wide
        let over_cjk = fed(Widening::Narrow, "\u{1b}[1;1H\u{6f22}\u{1b}[1;1Hx");
        assert_eq!(over_cjk.cell(0, 0), "x");
        assert_eq!(over_cjk.cell(1, 0), " ");

        // an erase is a write like any other, so it orphans a half the same
        // way: erasing up to and including the column a widened glyph sits
        // in leaves nothing of it behind
        let erased = fed(
            Widening::Ambiguous,
            "\u{1b}[1;3H\u{2502}\u{1b}[1;3H\u{1b}[1K",
        );
        assert_eq!(erased.cell(2, 0), " ");
        assert_eq!(erased.cell(3, 0), " ");

        // and the direction this model does not take: a write into the
        // second half leaves the glyph standing, because an editor that
        // sizes the glyph at one column writes there in every clean frame
        let into_half = fed(Widening::Ambiguous, "\u{1b}[1;1H\u{2502}\u{1b}[1;2Hx");
        assert_eq!(into_half.cell(0, 0), "\u{2502}");
        assert_eq!(into_half.cell(1, 0), "x");
    }

    /// A terminal draws a glyph two columns wide because Unicode says the
    /// width is ambiguous or because it draws the glyph as emoji; no other
    /// answer belongs in the model, and a range written out by hand instead
    /// of a property is how it came to widen a code point Unicode calls
    /// Neutral.
    #[test]
    fn the_model_widens_exactly_what_the_properties_say_it_may() {
        for cp in 0x80..=0x1_faff_u32 {
            let Some(c) = char::from_u32(cp) else {
                continue;
            };
            assert_eq!(
                WideTerm::widens(&c.to_string()),
                c.width() != c.width_cjk() || c.is_emoji_char() || is_regional_indicator(c),
                "U+{cp:04X}"
            );
        }

        // the keycap bases, which carry the Emoji property and none of the
        // width a terminal gives a glyph drawn as emoji
        for c in "#*0123456789".chars() {
            assert!(!WideTerm::widens(&c.to_string()), "{c:?}");
        }

        // the two the hand-written block-elements range got wrong: Neutral
        // code points inside a run of ambiguous ones, which no terminal
        // draws two columns wide
        for cp in [0x2590_u32, 0x2591] {
            let c = char::from_u32(cp).unwrap();
            assert!(
                !WideTerm::widens(&c.to_string()),
                "U+{cp:04X} is Neutral and the model widened it, which \
                 invents a second half no painter can repaint"
            );
        }
        // and the class a hand-written pair reached two of: every one of
        // these is Neutral to both rulers and drawn wide as emoji
        for cp in [0x2328_u32, 0x23f8, 0x261d, 0x270f, 0x1f321, 0x1f5a5] {
            let c = char::from_u32(cp).unwrap();
            assert_eq!(c.width(), c.width_cjk(), "U+{cp:04X}");
            assert!(WideTerm::widens(&c.to_string()), "U+{cp:04X}");
        }
        for cp in [0x2500_u32, 0x2502, 0x2588, 0x2592, 0x25a0, 0xe0b0, 0xf0219] {
            let c = char::from_u32(cp).unwrap();
            assert!(WideTerm::widens(&c.to_string()), "U+{cp:04X}");
        }
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
