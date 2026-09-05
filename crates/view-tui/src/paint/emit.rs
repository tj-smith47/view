//! What a composed frame puts on the wire: view's own copy of ratatui's
//! crossterm emission loop, with the cursor re-sync a terminal that draws
//! some glyphs two columns wide needs.
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
use ratatui::backend::IntoCrossterm;
use ratatui::buffer::{Buffer, BufferDiff, Cell, CellWidth};
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
fn widening_excess(symbol: &str) -> u16 {
    if symbol.is_ascii() {
        return 0;
    }
    let widening = symbol.chars().filter(|&c| char_may_widen(c)).count();
    u16::try_from(widening).unwrap_or(u16::MAX)
}

/// The cell diff, plus the columns to the right of every changed cell whose
/// old or new symbol a terminal may draw wider than `ratatui` sized it: on
/// such a terminal those columns hold the glyph's own excess, so they are
/// stale whenever the glyph changes even though the model never touched
/// them.
///
/// The reach past a changed cell starts at the first column past the wider
/// of its old and new [`CellWidth`] -- the columns inside that width belong
/// to the glyph, not to stale content -- and runs for the glyph's
/// [`widening_excess`], at least one column. It extends again from every
/// cell it yields that itself widens, since repainting a widened glyph
/// pushes the same staleness further right, so a run of box drawing whose
/// leftmost cell changes is repainted to its end, and it stops at the right
/// edge of the area.
///
/// Row-major left-to-right order is preserved, so [`draw_resynced`]'s
/// adjacency logic is unchanged, and a column the diff already carries is
/// left to the diff rather than yielded twice.
pub(crate) fn with_widened_neighbours<'p, 'n>(
    front: &'p Buffer,
    back: &'n Buffer,
    diff: BufferDiff<'p, 'n>,
) -> impl Iterator<Item = (u16, u16, &'n Cell)> + use<'p, 'n> {
    let right = back.area.right();
    let mut diff = diff.peekable();
    let mut reach: Option<(u16, u16, u16)> = None;
    std::iter::from_fn(move || {
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
            (next, row, back.cell((next, row))?)
        } else {
            let (x, y, cell) = diff.next()?;
            reach = live
                .filter(|&(_, end, row)| row == y && x < end)
                .map(|(next, end, row)| (next.max(x.saturating_add(1)), end, row));
            (x, y, cell)
        };
        let old = front.cell((x, y));
        let widened = terminal_may_widen(cell.symbol())
            || old.is_some_and(|old| terminal_may_widen(old.symbol()));
        if widened {
            let span = cell
                .cell_width()
                .max(old.map_or(1, |old| old.cell_width()))
                .max(1);
            let excess = widening_excess(cell.symbol())
                .max(old.map_or(0, |old| widening_excess(old.symbol())))
                .max(1);
            let start = x.saturating_add(span);
            let end = start.saturating_add(excess).min(right);
            reach = match reach {
                Some((next, reached, row)) if row == y => {
                    Some((next.max(start), reached.max(end), row))
                }
                _ => Some((start, end, y)),
            };
        }
        Some((x, y, cell))
    })
}

/// Writes `content` the way `CrosstermBackend::draw` does -- same style
/// diffing, same trailing reset -- and, after any cell whose symbol a
/// terminal may draw wider than the shadow assumes, addresses the next cell
/// absolutely instead of trusting the terminal's advance.
///
/// Two consequences of addressing absolutely, both of which the crossterm
/// loop is free of because it never moves mid-run. A cell inside the span
/// the previous symbol already covers is dropped rather than printed over
/// that symbol's own second half; and a symbol `ratatui` sizes at two
/// columns that a terminal may draw at one has both of its columns blanked
/// first, so the column the terminal declines to cover is left blank rather
/// than stale.
///
/// # Errors
///
/// Returns the writer's own error.
pub(crate) fn draw_resynced<'a, W: Write>(
    writer: &mut W,
    content: impl Iterator<Item = (u16, u16, &'a Cell)>,
) -> std::io::Result<()> {
    let mut fg = Color::Reset;
    let mut bg = Color::Reset;
    let mut underline_color = Color::Reset;
    let mut modifier = Modifier::empty();
    let mut last_pos: Option<(u16, u16)> = None;
    let mut covered_until: Option<(u16, u16)> = None;
    for (x, y, cell) in content {
        // `ratatui` yields the trailing column of a VS16 emoji as a clear,
        // on the assumption a backend prints it straight after the glyph and
        // lets the terminal's own advance place it; addressed absolutely it
        // lands on the glyph's second half instead and the terminal drops
        // the glyph
        if matches!(covered_until, Some((cx, cy)) if cy == y && x < cx) {
            continue;
        }
        if !matches!(last_pos, Some((px, py)) if px.checked_add(1) == Some(x) && y == py) {
            queue!(writer, MoveTo(x, y))?;
        }
        last_pos = Some((x, y));
        if cell.modifier != modifier {
            queue_modifier_diff(writer, modifier, cell.modifier)?;
            modifier = cell.modifier;
        }
        if cell.fg != fg || cell.bg != bg {
            queue!(
                writer,
                SetColors(CtColors::new(
                    cell.fg.into_crossterm(),
                    cell.bg.into_crossterm(),
                ))
            )?;
            fg = cell.fg;
            bg = cell.bg;
        }
        if cell.underline_color != underline_color {
            queue!(
                writer,
                SetUnderlineColor(cell.underline_color.into_crossterm())
            )?;
            underline_color = cell.underline_color;
        }
        if cell.cell_width() >= 2 && terminal_may_widen(cell.symbol()) {
            // nvim's TUI writes the same two spaces and two backspaces ahead
            // of this class: on a terminal that draws the glyph one column
            // wide the second column is then blank rather than stale
            queue!(writer, Print("  \u{8}\u{8}"))?;
        }
        queue!(writer, Print(cell.symbol()))?;
        covered_until = x.checked_add(cell.cell_width()).map(|next| (next, y));
        if terminal_may_widen(cell.symbol()) {
            // the terminal's own cursor is now somewhere this loop cannot
            // predict, so the next cell is addressed rather than assumed
            last_pos = None;
        }
    }
    queue!(
        writer,
        SetForegroundColor(CtColor::Reset),
        SetBackgroundColor(CtColor::Reset),
        SetUnderlineColor(CtColor::Reset),
        SetAttribute(CtAttribute::Reset),
    )
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
