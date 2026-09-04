//! What a composed frame puts on the wire: view's own copy of ratatui's
//! crossterm emission loop, with the cursor re-sync a terminal that draws
//! ambiguous-width glyphs two columns wide needs.
//!
//! `ratatui::backend::CrosstermBackend::draw` moves the cursor only when the
//! next cell is not the immediate successor of the last one, letting adjacent
//! cells ride on the terminal's own advance. That advance is one column per
//! cell only while view and the terminal agree on how wide a glyph is, and
//! they do not agree on East_Asian_Width = Ambiguous: `unicode-width` says
//! one column, several terminals draw two. After such a glyph every later
//! cell of the run lands one column right of where the shadow believes it
//! is, and the next frame -- which repaints only model-changed cells --
//! covers neither the shifted cells nor the glyph's second half.

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
use unicode_width::UnicodeWidthChar;

/// Whether a terminal may draw `symbol` wider than `unicode_width` says.
///
/// Terminals disagree on East_Asian_Width = Ambiguous glyphs (nerd-font
/// private-use icons, box drawing) and on emoji presentation selected by
/// VS16; nvim's TUI re-syncs its cursor after exactly these.
///
/// nvim's third class -- Extended_Pictographic with text presentation and
/// Regional_Indicator -- is not covered: no crate in this tree's dependency
/// set carries those tables, so a pictographic character that is neither
/// Ambiguous nor VS16-marked still rides on the terminal's advance here.
pub(crate) fn terminal_may_widen(symbol: &str) -> bool {
    if symbol.is_ascii() {
        return false;
    }
    symbol
        .chars()
        .any(|c| c == '\u{fe0f}' || (c as u32 >= 0x80 && c.width() != c.width_cjk()))
}

/// The cell diff, plus the cell to the right of every changed cell whose old
/// or new symbol a terminal may draw two wide: on such a terminal that
/// neighbour is the glyph's second half, so it is stale whenever the glyph
/// changes even though the model never touched it.
///
/// Row-major left-to-right order is preserved, so [`draw_resynced`]'s
/// adjacency logic is unchanged. A neighbour is skipped where the diff
/// already carries it, where the new cell is itself multi-column (its own
/// continuation column belongs to the glyph, not to stale content), and at
/// the right edge of the area.
pub(crate) fn with_widened_neighbours<'p, 'n>(
    front: &'p Buffer,
    back: &'n Buffer,
    diff: BufferDiff<'p, 'n>,
) -> impl Iterator<Item = (u16, u16, &'n Cell)> + use<'p, 'n> {
    let right = back.area.right();
    let mut diff = diff.peekable();
    let mut pending: Option<(u16, u16, &'n Cell)> = None;
    std::iter::from_fn(move || {
        if let Some(item) = pending.take() {
            return Some(item);
        }
        let (x, y, cell) = diff.next()?;
        let widened = terminal_may_widen(cell.symbol())
            || front
                .cell((x, y))
                .is_some_and(|old| terminal_may_widen(old.symbol()));
        if widened && cell.cell_width() <= 1 {
            if let Some(nx) = x.checked_add(1).filter(|nx| *nx < right) {
                let already = matches!(diff.peek(), Some(&(px, py, _)) if px == nx && py == y);
                if !already {
                    pending = back.cell((nx, y)).map(|neighbour| (nx, y, neighbour));
                }
            }
        }
        Some((x, y, cell))
    })
}

/// Writes `content` the way `CrosstermBackend::draw` does -- same style
/// diffing, same trailing reset -- and, after any cell whose symbol a
/// terminal may draw wider than the shadow assumes, addresses the next cell
/// absolutely instead of trusting the terminal's advance.
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
    for (x, y, cell) in content {
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
        queue!(writer, Print(cell.symbol()))?;
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
