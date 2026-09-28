//! What a composed frame puts on the wire: view's own emission loop, grown
//! from ratatui's crossterm one, with the cursor re-sync a terminal that
//! draws some glyphs two columns wide needs.
//!
//! `ratatui::backend::CrosstermBackend::draw` moves the cursor only when the
//! next cell is not the immediate successor of the last one, letting adjacent
//! cells ride on the terminal's own advance. That advance is one column per
//! cell only while view and the terminal agree on how wide a glyph is, and
//! they do not agree on every class [`terminal_may_widen`] answers for --
//! East_Asian_Width = Ambiguous, text-presentation pictographs, regional
//! indicators, VS16 emoji: `unicode-width` says one column, several
//! terminals draw two. After such a glyph every later cell of the run lands
//! one column right of where the shadow believes it is, and when the
//! glyph changes its second half goes stale without the model touching the
//! cell that holds it -- and so does every following cell that widens too,
//! each one's half pushed a column further along the run. The next frame,
//! which repaints only model-changed cells, covers none of that.

use std::io::Write;

use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::{
    Attribute as CtAttribute, Color as CtColor, Colors as CtColors, Print, SetAttribute,
    SetBackgroundColor, SetColors, SetForegroundColor, SetUnderlineColor,
};
use crossterm::terminal::{Clear, ClearType};
use ratatui::backend::IntoCrossterm;
use ratatui::buffer::{Buffer, Cell, CellWidth};
use ratatui::style::{Color, Modifier};
use unicode_properties::emoji::is_regional_indicator;
use unicode_properties::UnicodeEmoji;
use unicode_width::UnicodeWidthChar;

/// Whether a terminal may draw `symbol` wider than `unicode_width` says.
///
/// Answers nvim's `utf_ambiguous_width` in full: East_Asian_Width =
/// Ambiguous (nerd-font private-use icons, box drawing), pictographic
/// characters whose default presentation is text (U+270F PENCIL), regional
/// indicators, and emoji presentation selected by a following VS16. nvim's
/// TUI re-syncs its cursor after exactly these, and there is no class of
/// glyph it re-syncs after that this leaves out.
///
/// The pictographic half reads `unicode-properties`' Emoji property, the
/// table that crate carries. Every Extended_Pictographic code point it does
/// not carry is one Unicode has not assigned -- the reserved ranges the
/// property holds open for future emoji -- so no character a terminal can
/// draw falls between the two.
pub(crate) fn terminal_may_widen(symbol: &str) -> bool {
    if symbol.is_ascii() {
        return false;
    }
    symbol.chars().any(|c| c == '\u{fe0f}' || char_may_widen(c))
}

/// Whether this terminal may draw `symbol` wider than `unicode_width` says,
/// given whether its startup probe measured a box-drawing glyph one cell
/// wide.
///
/// A terminal gives every box-drawing character the advance it gave `╭`,
/// so the probe's answer covers the U+2500 to U+257F block and a border
/// drawn from it rides on the terminal's own advance. Every other class
/// keeps [`terminal_may_widen`]'s answer: a nerd-font icon's width is the
/// font's, which no probe asks.
pub(crate) fn may_widen(symbol: &str, boxes_one_cell: bool) -> bool {
    terminal_may_widen(symbol)
        && !(boxes_one_cell
            && symbol
                .chars()
                .all(|c| ('\u{2500}'..='\u{257f}').contains(&c)))
}

/// Whether a terminal may draw `c` alone wider than `unicode_width` says.
///
/// The per-code-point half of [`terminal_may_widen`], minus the VS16 itself:
/// a variation selector adds no column of its own, it selects the emoji
/// presentation of the character before it, which this already answers for.
fn char_may_widen(c: char) -> bool {
    c as u32 >= 0x80
        && (c.width() != c.width_cjk() || c.is_emoji_char() || is_regional_indicator(c))
}

/// How many columns beyond the width `ratatui` sizes its cell by a widening
/// terminal may give `symbol`.
///
/// One per code point that widens on its own, because a terminal sizes each
/// of them separately: `unicode-width` calls a regional-indicator pair two
/// columns and a terminal drawing each indicator two wide covers four, so
/// the pair's excess is two, while a box-drawing character sized one column
/// and drawn two has an excess of one.
///
/// A code point both rulers already call two columns wide is not counted:
/// no terminal draws it wider than the two it is sized at, so a ZWJ
/// sequence of them would otherwise claim a column of excess apiece.
fn widening_excess(symbol: &str) -> u16 {
    if symbol.is_ascii() {
        return 0;
    }
    let widening = symbol
        .chars()
        .filter(|&c| char_may_widen(c) && c.width().unwrap_or(0) < 2)
        .count();
    u16::try_from(widening).unwrap_or(u16::MAX)
}

/// The cell diff of `front` against `back`, plus the columns to the right
/// of every changed cell whose old or new symbol a terminal may draw wider
/// than `ratatui` sized it: on such a terminal those columns hold the
/// glyph's own excess, so they are stale whenever the glyph changes even
/// though the model never touched them.
///
/// The reach past a changed cell starts at the first column past the new
/// symbol's own [`CellWidth`] -- the columns inside that width are the ones
/// this print covers -- and runs out past the wider of the old and new
/// widths by the glyph's [`widening_excess`], at least one column. Starting
/// at the new width rather than the wider of the two is what reaches a
/// symbol `ratatui` sized at two columns whose code points a terminal draws
/// as separate glyphs: a narrower symbol printed over the first column
/// clears that column alone, and the second still holds the glyph the old
/// symbol's second code point put there. The reach extends again from every
/// cell it yields that itself widens, since repainting a widened glyph
/// pushes the same staleness further right, so a run of box drawing whose
/// leftmost cell changes is repainted to its end, and it stops at the right
/// edge of the area.
///
/// Row-major left-to-right order is preserved, so [`draw_resynced`]'s
/// adjacency logic is unchanged, and a column the diff already carries is
/// left to the diff rather than yielded twice.
///
/// `boxes_one_cell` is the probe's measurement that box drawing takes one
/// cell on this terminal (see [`may_widen`]); with it set a box-drawing
/// glyph reaches no column past its own.
pub(crate) fn with_widened_neighbours<'p, 'n>(
    front: &'p Buffer,
    back: &'n Buffer,
    boxes_one_cell: bool,
) -> impl Iterator<Item = (u16, u16, &'n Cell)> + use<'p, 'n> {
    let right = back.area.right();
    let mut diff = front.diff_iter(back).peekable();
    let mut reach: Option<(u16, u16, u16)> = None;
    std::iter::from_fn(move || loop {
        let live = reach.filter(|&(next, end, _)| next < end);
        // whichever of the two comes first in row-major order, and the diff
        // when they name the same column, so no column is yielded twice
        let from_reach = match (live, diff.peek()) {
            (Some((next, _, row)), Some(&(dx, dy, _))) => (row, next) < (dy, dx),
            (Some(_), None) => true,
            (None, _) => false,
        };
        let (x, y, cell) = if let Some((next, end, row)) = live.filter(|_| from_reach) {
            reach = Some((next.saturating_add(1), end, row));
            let Some(cell) = back.cell((next, row)) else {
                // a reached column the buffer does not hold is skipped, not
                // returned: ending the walk here would leave every remaining
                // diff cell of the frame unpainted with nothing to say so
                continue;
            };
            (next, row, cell)
        } else {
            let (x, y, cell) = diff.next()?;
            reach = live
                .filter(|&(_, end, row)| row == y && x < end)
                .map(|(next, end, row)| (next.max(x.saturating_add(1)), end, row));
            (x, y, cell)
        };
        let old = front.cell((x, y));
        let widened = may_widen(cell.symbol(), boxes_one_cell)
            || old.is_some_and(|old| may_widen(old.symbol(), boxes_one_cell));
        if widened {
            let span = cell
                .cell_width()
                .max(old.map_or(1, |old| old.cell_width()))
                .max(1);
            let excess = widening_excess(cell.symbol())
                .max(old.map_or(0, |old| widening_excess(old.symbol())))
                .max(1);
            let start = x.saturating_add(cell.cell_width().max(1));
            let end = x.saturating_add(span).saturating_add(excess).min(right);
            reach = match reach {
                Some((next, reached, row)) if row == y => {
                    Some((next.max(start), reached.max(end), row))
                }
                _ => Some((start, end, y)),
            };
        }
        return Some((x, y, cell));
    })
}

/// Writes `content` as `CrosstermBackend::draw` does, with its trailing
/// reset, and, after any cell whose symbol a terminal may draw wider than
/// the shadow assumes, addresses the next cell absolutely instead of
/// trusting the terminal's advance.
///
/// Two consequences of addressing absolutely, both of which the crossterm
/// loop is free of because it never moves mid-run. A cell inside the span
/// the previous symbol already covers is dropped rather than printed over
/// that symbol's own second half; and a symbol `ratatui` sizes at two
/// columns that a terminal may draw at one has both of its columns blanked
/// first, so the column the terminal declines to cover is left blank rather
/// than stale.
///
/// Returns whether any cell reached the writer, which is also what decides
/// the trailing reset: the reset undoes styles this call itself set, so a
/// pass that printed nothing owes the terminal nothing -- and a frame whose
/// every cell already matched what the terminal shows would otherwise cost
/// a write of pure trailer.
///
/// With `boxes_one_cell` set, the probe having measured box drawing one
/// cell wide (see [`may_widen`]), a run of box-drawing cells is addressed
/// once at its start and rides on the terminal's advance after that. A
/// cursor move per border cell is several times the glyph's own three
/// bytes, and a frame that reaches a terminal behind tmux in several reads
/// is shown by a slow client one read at a time.
///
/// A colour switch writes only the colour that changes. A run of blank
/// cells in one style on the terminal's default background that reaches
/// `right`, the terminal's last column plus one, is one erase to the end
/// of the line where that is shorter than the spaces. The screen a frame
/// leaves is the one `CrosstermBackend::draw` leaves; the bytes differ.
///
/// # Errors
///
/// Returns the writer's own error.
pub(crate) fn draw_resynced<'a, W: Write>(
    writer: &mut W,
    content: impl Iterator<Item = (u16, u16, &'a Cell)>,
    boxes_one_cell: bool,
    right: u16,
) -> std::io::Result<bool> {
    let mut pen = Pen::default();
    let mut blanks: Option<BlankRun> = None;
    for (x, y, cell) in content {
        // `ratatui` yields the trailing column of a VS16 emoji as a clear,
        // on the assumption a backend prints it straight after the glyph and
        // lets the terminal's own advance place it; addressed absolutely it
        // lands on the glyph's second half instead and the terminal drops
        // the glyph
        if matches!(pen.covered_until, Some((cx, cy)) if cy == y && x < cx) {
            continue;
        }
        let erasable = cell.bg == Color::Reset && is_blank(cell);
        let starts = BlankRun { x, y, len: 1, cell };
        blanks = match blanks {
            Some(run)
                if erasable
                    && run.y == y
                    && run.end() == Some(x)
                    && run.cell.style() == cell.style() =>
            {
                Some(BlankRun {
                    len: run.len.saturating_add(1),
                    ..run
                })
            }
            Some(run) => {
                pen.spaces(writer, run)?;
                erasable.then_some(starts)
            }
            None => erasable.then_some(starts),
        };
        if erasable {
            if let Some(run) = blanks.filter(|_| x.saturating_add(1) >= right) {
                if run.len > ERASE_LEN {
                    pen.erase_to_end(writer, run)?;
                } else {
                    pen.spaces(writer, run)?;
                }
                blanks = None;
            }
            continue;
        }
        pen.put(writer, x, y, cell, boxes_one_cell)?;
    }
    if let Some(run) = blanks {
        pen.spaces(writer, run)?;
    }
    if pen.emitted {
        queue!(
            writer,
            SetForegroundColor(CtColor::Reset),
            SetBackgroundColor(CtColor::Reset),
            SetUnderlineColor(CtColor::Reset),
            SetAttribute(CtAttribute::Reset),
        )?;
    }
    Ok(pen.emitted)
}

/// The attributes under which a space shows the foreground colour.
const SHOWS_FG: Modifier = Modifier::REVERSED
    .union(Modifier::UNDERLINED)
    .union(Modifier::CROSSED_OUT);

/// The bytes of `CSI K`, which a run of spaces has to outgrow before the
/// erase is the shorter of the two.
const ERASE_LEN: u16 = 3;

/// Whether `cell` shows nothing but its background.
fn is_blank(cell: &Cell) -> bool {
    cell.symbol() == " " && !cell.modifier.intersects(SHOWS_FG)
}

/// Adjacent blank cells in one style on the default background, held until
/// the loop knows whether they reach the end of the line.
#[derive(Clone, Copy)]
struct BlankRun<'a> {
    x: u16,
    y: u16,
    len: u16,
    cell: &'a Cell,
}

impl BlankRun<'_> {
    /// The column past the run's last cell.
    fn end(self) -> Option<u16> {
        self.x.checked_add(self.len)
    }
}

/// The terminal's cursor and SGR state as the escapes queued so far leave
/// them.
struct Pen {
    fg: Color,
    bg: Color,
    underline_color: Color,
    modifier: Modifier,
    last_pos: Option<(u16, u16)>,
    covered_until: Option<(u16, u16)>,
    emitted: bool,
}

impl Default for Pen {
    fn default() -> Self {
        Self {
            fg: Color::Reset,
            bg: Color::Reset,
            underline_color: Color::Reset,
            modifier: Modifier::empty(),
            last_pos: None,
            covered_until: None,
            emitted: false,
        }
    }
}

impl Pen {
    /// Moves the cursor to `(x, y)` unless the terminal's advance already
    /// put it there.
    fn address<W: Write>(&mut self, writer: &mut W, x: u16, y: u16) -> std::io::Result<()> {
        if !matches!(self.last_pos, Some((px, py)) if px.checked_add(1) == Some(x) && y == py) {
            queue!(writer, MoveTo(x, y))?;
        }
        self.last_pos = Some((x, y));
        Ok(())
    }

    /// Sets every attribute `cell` is drawn with, writing only the colours
    /// that change.
    fn style<W: Write>(&mut self, writer: &mut W, cell: &Cell) -> std::io::Result<()> {
        if cell.modifier != self.modifier {
            queue_modifier_diff(writer, self.modifier, cell.modifier)?;
            self.modifier = cell.modifier;
        }
        match (cell.fg != self.fg, cell.bg != self.bg) {
            (true, true) => queue!(
                writer,
                SetColors(CtColors::new(
                    cell.fg.into_crossterm(),
                    cell.bg.into_crossterm(),
                ))
            )?,
            (true, false) => queue!(writer, SetForegroundColor(cell.fg.into_crossterm()))?,
            (false, true) => queue!(writer, SetBackgroundColor(cell.bg.into_crossterm()))?,
            (false, false) => {}
        }
        self.fg = cell.fg;
        self.bg = cell.bg;
        if cell.underline_color != self.underline_color {
            queue!(
                writer,
                SetUnderlineColor(cell.underline_color.into_crossterm())
            )?;
            self.underline_color = cell.underline_color;
        }
        Ok(())
    }

    /// Writes one cell.
    fn put<W: Write>(
        &mut self,
        writer: &mut W,
        x: u16,
        y: u16,
        cell: &Cell,
        boxes_one_cell: bool,
    ) -> std::io::Result<()> {
        self.address(writer, x, y)?;
        self.style(writer, cell)?;
        let widens = may_widen(cell.symbol(), boxes_one_cell);
        if cell.cell_width() >= 2 && widens {
            // nvim's TUI writes the same two spaces and two backspaces ahead
            // of this class: on a terminal that draws the glyph one column
            // wide the second column is then blank rather than stale
            queue!(writer, Print("  \u{8}\u{8}"))?;
        }
        queue!(writer, Print(cell.symbol()))?;
        self.emitted = true;
        self.covered_until = x.checked_add(cell.cell_width()).map(|next| (next, y));
        if widens {
            // the terminal's own cursor is now somewhere this loop cannot
            // predict, so the next cell is addressed rather than assumed
            self.last_pos = None;
        }
        Ok(())
    }

    /// Writes `run` as spaces.
    fn spaces<W: Write>(&mut self, writer: &mut W, run: BlankRun) -> std::io::Result<()> {
        self.address(writer, run.x, run.y)?;
        self.style(writer, run.cell)?;
        for _ in 0..run.len {
            queue!(writer, Print(' '))?;
        }
        let last = run.end().map(|end| end.saturating_sub(1));
        self.last_pos = last.map(|x| (x, run.y));
        self.covered_until = run.end().map(|end| (end, run.y));
        self.emitted = true;
        Ok(())
    }

    /// Erases from `run`'s first cell to the end of the line.
    fn erase_to_end<W: Write>(&mut self, writer: &mut W, run: BlankRun) -> std::io::Result<()> {
        self.address(writer, run.x, run.y)?;
        // a terminal fills the erased cells with the pen's colours, and a
        // block cursor resting on one later draws its foreground
        self.style(writer, run.cell)?;
        queue!(writer, Clear(ClearType::UntilNewLine))?;
        // the erase leaves the cursor on the run's first cell, where no
        // later cell of the frame is written without an address
        self.last_pos = None;
        self.covered_until = None;
        self.emitted = true;
        Ok(())
    }
}

/// Queues the attribute escapes that turn `from` into `to`, in the order
/// `ratatui-crossterm`'s own `ModifierDiff` emits them.
fn queue_modifier_diff<W: Write>(
    writer: &mut W,
    from: Modifier,
    to: Modifier,
) -> std::io::Result<()> {
    let removed = from - to;
    if removed.contains(Modifier::REVERSED) {
        queue!(writer, SetAttribute(CtAttribute::NoReverse))?;
    }
    let reset_intensity = removed.contains(Modifier::BOLD) || removed.contains(Modifier::DIM);
    if reset_intensity {
        // Normal intensity resets bold and dim together, so whichever of the
        // two survives into `to` has to be written again after it
        queue!(writer, SetAttribute(CtAttribute::NormalIntensity))?;
        if to.contains(Modifier::DIM) {
            queue!(writer, SetAttribute(CtAttribute::Dim))?;
        }
        if to.contains(Modifier::BOLD) {
            queue!(writer, SetAttribute(CtAttribute::Bold))?;
        }
    }
    if removed.contains(Modifier::ITALIC) {
        queue!(writer, SetAttribute(CtAttribute::NoItalic))?;
    }
    if removed.contains(Modifier::UNDERLINED) {
        queue!(writer, SetAttribute(CtAttribute::NoUnderline))?;
    }
    if removed.contains(Modifier::CROSSED_OUT) {
        queue!(writer, SetAttribute(CtAttribute::NotCrossedOut))?;
    }
    if removed.contains(Modifier::HIDDEN) {
        queue!(writer, SetAttribute(CtAttribute::NoHidden))?;
    }
    if removed.contains(Modifier::SLOW_BLINK) || removed.contains(Modifier::RAPID_BLINK) {
        queue!(writer, SetAttribute(CtAttribute::NoBlink))?;
    }

    let added = to - from;
    if added.contains(Modifier::REVERSED) {
        queue!(writer, SetAttribute(CtAttribute::Reverse))?;
    }
    if added.contains(Modifier::BOLD) && !reset_intensity {
        queue!(writer, SetAttribute(CtAttribute::Bold))?;
    }
    if added.contains(Modifier::ITALIC) {
        queue!(writer, SetAttribute(CtAttribute::Italic))?;
    }
    if added.contains(Modifier::UNDERLINED) {
        queue!(writer, SetAttribute(CtAttribute::Underlined))?;
    }
    if added.contains(Modifier::DIM) && !reset_intensity {
        queue!(writer, SetAttribute(CtAttribute::Dim))?;
    }
    if added.contains(Modifier::CROSSED_OUT) {
        queue!(writer, SetAttribute(CtAttribute::CrossedOut))?;
    }
    if added.contains(Modifier::HIDDEN) {
        queue!(writer, SetAttribute(CtAttribute::Hidden))?;
    }
    if added.contains(Modifier::SLOW_BLINK) {
        queue!(writer, SetAttribute(CtAttribute::SlowBlink))?;
    }
    if added.contains(Modifier::RAPID_BLINK) {
        queue!(writer, SetAttribute(CtAttribute::RapidBlink))?;
    }
    Ok(())
}
