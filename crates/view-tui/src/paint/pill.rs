//! The top pill: the row above everything else, naming what is open.
//!
//! Placement is [`PillView::slots`]'s, the same answer the mouse router
//! spends, so the name a click selects is the name that was drawn under
//! the pointer.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use view_core::native::pill::{edge_cells, PillView};
use view_core::theme::{ChromeGroup, Theme};

use super::text::{cluster_width, clusters, set_cluster};
use super::{ratatui_style, rgb};

/// Draws the pill across `area`, which is the terminal's own top row.
///
/// The row is filled in `TabLineFill` first, so every column the names do
/// not reach carries the group nvim names for exactly that: the row behind
/// the tabs.
pub(super) fn paint_pill(pill: &PillView, theme: &Theme, area: Rect, buf: &mut Buffer) {
    let fill = ratatui_style(theme.chrome(ChromeGroup::TabLineFill));
    fill_run(buf, area, 0, area.width, fill);
    // the accent over the row's own background, not `TabLineFill`'s
    // foreground: the host is the one thing here that says which machine
    // the session is on, and a colorscheme that dims the tab row would
    // take it down with the rest
    let edge = theme.accent().fg.map_or(fill, |fg| fill.fg(rgb(fg)));
    write_at(buf, area, 1, &pill.host, edge);
    let agent = area.width.saturating_sub(edge_cells(pill.agent));
    write_at(buf, area, agent.saturating_add(1), pill.agent, edge);

    let tab = ratatui_style(theme.chrome(ChromeGroup::TabLine));
    let selected = ratatui_style(theme.chrome(ChromeGroup::TabLineSel));
    for (slot, entry) in pill.slots(area.width).into_iter().zip(&pill.entries) {
        let style = if slot.current { selected } else { tab };
        fill_run(buf, area, slot.col, slot.cells, style);
        write_at(buf, area, slot.col.saturating_add(1), &entry.label, style);
    }
}

/// Paints `cells` blank columns from `col`, which is what gives a selected
/// name its own background either side of the text.
///
/// `ratatui::buffer::Cell::reset` leaves a blank behind, so this writes no
/// symbol of its own and the names are the only text the row carries.
fn fill_run(buf: &mut Buffer, area: Rect, col: u16, cells: u16, style: Style) {
    for at in col..col.saturating_add(cells).min(area.width) {
        let cell = &mut buf[(area.x.saturating_add(at), area.y)];
        cell.reset();
        cell.set_style(style);
    }
}

/// Writes `text` from column `col`, one grapheme cluster per cell,
/// stopping at the row's own end.
fn write_at(buf: &mut Buffer, area: Rect, col: u16, text: &str, style: Style) {
    let mut at = col;
    for cluster in clusters(text) {
        let width = cluster_width(cluster);
        if at.saturating_add(width) > area.width {
            return;
        }
        set_cluster(buf, area.x.saturating_add(at), area.y, cluster, style);
        // ratatui's own convention for the cell a two-cell glyph covers:
        // the diff skips it, and anything left in it would be drawn one
        // column right of where it was written
        if width == 2 {
            buf[(area.x.saturating_add(at).saturating_add(1), area.y)].reset();
        }
        at = at.saturating_add(width);
    }
}
