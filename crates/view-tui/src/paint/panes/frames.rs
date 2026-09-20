//! The frame view draws around every window under `panes = "tiles"`.
//!
//! Gapped and gapless are the same picture drawn in two places. A gapped
//! tile's frame sits one cell inside the slot nvim gave the window, with a
//! cell of gap outside it, so two neighbouring frames never touch. A
//! gapless tile has no room of its own: its frame is the cell *between* two
//! slots, which is the separator column and the status row nvim already
//! paints there, restyled. One cell between two tiles is what makes a
//! junction glyph possible at all, since a doubled line leaves nowhere for
//! one to sit.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use std::collections::BTreeSet;
use view_core::grid::registry::{GridId, Pane, PaneKind, GLOBAL_GRID};
use view_core::model::{Look, Panes};
use view_core::theme::{ChromeGroup, ResolvedStyle, Theme};
use view_surface::overlay::BorderSet;

use super::super::{ratatui_style, set_border_cell, Damage};

/// One cell of the lattice, as `(row, col)` in the engine layer's own
/// coordinates.
type Cell = (u16, u16);

/// Paints the tiles' frames over the window panes already composited into
/// `buf`, and under every float the caller paints after it.
///
/// `panes` is the compositor's own z-ordered list, read here rather than
/// collected again; the window panes are the tiles and everything else is
/// skipped. `area` is the engine-grid layer's rect, the same one the panes
/// were painted inside, so a slot's coordinates are applied within it
/// exactly once. A look other than tiles paints nothing at all.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_frames(
    panes: &[Pane],
    look: Look,
    active: Option<GridId>,
    theme: &Theme,
    borders: BorderSet,
    area: Rect,
    damage: &Damage,
    buf: &mut Buffer,
) {
    if look.panes != Panes::Tiles || !panes.iter().any(is_tile) {
        return;
    }
    let quiet = quiet_style(theme);
    let accent = ratatui_style(theme.accent());
    if look.gaps {
        paint_gapped(
            panes, active, theme, borders, quiet, accent, area, damage, buf,
        );
    } else {
        paint_gapless(panes, active, borders, quiet, accent, area, damage, buf);
    }
}

/// Whether a pane is one of the tiles a frame is drawn around: the global
/// grid carries chrome rather than a window, and a float or a message grid
/// has no slot of its own.
fn is_tile(pane: &Pane) -> bool {
    pane.id != GLOBAL_GRID && matches!(pane.kind, PaneKind::Window)
}

/// The frame colour of a tile the user is not working in: `WinSeparator`'s
/// foreground, or the default one where the colorscheme states none,
/// halfway to the background so the active tile's own frame is the one the
/// eye lands on.
fn quiet_style(theme: &Theme) -> Style {
    let separator = theme.chrome(ChromeGroup::WinSeparator);
    let fg = separator.fg.or(theme.fg);
    ratatui_style(ResolvedStyle {
        fg: fg.map(|fg| halfway(fg, theme.bg.unwrap_or(0))),
        ..theme.normal()
    })
}

/// `colour` halfway to `toward`, channel by channel.
fn halfway(colour: u32, toward: u32) -> u32 {
    let channel = |shift: u32| -> u32 {
        let (from, to) = ((colour >> shift) & 0xFF, (toward >> shift) & 0xFF);
        (from + to) / 2
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

/// Whether the tile's grid sits inside its slot, which is what a frame
/// needs room for. A slot too small to spare the ring keeps its whole
/// grid, and the registry places it at the slot's own origin.
fn framed(pane: &Pane) -> bool {
    let (row, col, _, _) = pane.slot;
    pane.origin != (row, col)
}

#[allow(clippy::too_many_arguments)]
fn paint_gapped(
    panes: &[Pane],
    active: Option<GridId>,
    theme: &Theme,
    borders: BorderSet,
    quiet: Style,
    accent: Style,
    area: Rect,
    damage: &Damage,
    buf: &mut Buffer,
) {
    // two rings are cleared per tile. The slot's own outermost row and
    // column are the gap the frame sits inside, and the cell just beyond
    // the slot is where nvim paints the separator column and the status
    // row; a stale separator left in the gap row survives every later
    // redraw, because nvim believes the window's own grid covers it
    let gap = ratatui_style(theme.normal());
    for pane in panes.iter().filter(|pane| is_tile(pane) && framed(pane)) {
        let (row, col, width, height) = pane.slot;
        // the grid's last row is nvim's command line, where the mode
        // message and the answer to every prompt are written, so every
        // clear stops above it; any other row under a slot is the status
        // row the frame replaces
        let under = u16::from(row.saturating_add(height).saturating_add(1) < area.height);
        let (span_w, span_h) = (width.saturating_add(1), height.saturating_add(under));
        clear(row, col, span_w, 1, gap, area, damage, buf);
        clear(
            row.saturating_add(height).saturating_sub(1),
            col,
            span_w,
            1 + under,
            gap,
            area,
            damage,
            buf,
        );
        clear(row, col, 1, span_h, gap, area, damage, buf);
        clear(
            row,
            col.saturating_add(width).saturating_sub(1),
            2,
            span_h,
            gap,
            area,
            damage,
            buf,
        );
    }
    for pane in panes.iter().filter(|pane| is_tile(pane) && framed(pane)) {
        let (row, col, width, height) = pane.slot;
        let style = if active == Some(pane.id) {
            accent
        } else {
            quiet
        };
        box_edge(
            row.saturating_add(1),
            col.saturating_add(1),
            width.saturating_sub(2),
            height.saturating_sub(2),
            borders,
            style,
            area,
            damage,
            buf,
        );
    }
}

/// Clears `width`x`height` cells at `(row, col)` to the background the
/// tiles sit on.
#[allow(clippy::too_many_arguments)]
fn clear(
    row: u16,
    col: u16,
    width: u16,
    height: u16,
    style: Style,
    area: Rect,
    damage: &Damage,
    buf: &mut Buffer,
) {
    for r in row..row.saturating_add(height) {
        for c in col..col.saturating_add(width) {
            if let Some((x, y)) = screen(r, c, area, damage) {
                set_border_cell(buf, x, y, ' ', style);
            }
        }
    }
}

/// Draws one box's four edges, corners included.
#[allow(clippy::too_many_arguments)]
fn box_edge(
    row: u16,
    col: u16,
    width: u16,
    height: u16,
    borders: BorderSet,
    style: Style,
    area: Rect,
    damage: &Damage,
    buf: &mut Buffer,
) {
    if width < 2 || height < 2 {
        return;
    }
    let (last_row, last_col) = (
        row.saturating_add(height).saturating_sub(1),
        col.saturating_add(width).saturating_sub(1),
    );
    for c in col..=last_col {
        put(row, c, borders.horizontal, style, area, damage, buf);
        put(last_row, c, borders.horizontal, style, area, damage, buf);
    }
    for r in row..=last_row {
        put(r, col, borders.vertical, style, area, damage, buf);
        put(r, last_col, borders.vertical, style, area, damage, buf);
    }
    put(row, col, borders.top_left, style, area, damage, buf);
    put(row, last_col, borders.top_right, style, area, damage, buf);
    put(last_row, col, borders.bottom_left, style, area, damage, buf);
    put(
        last_row,
        last_col,
        borders.bottom_right,
        style,
        area,
        damage,
        buf,
    );
}

#[allow(clippy::too_many_arguments)]
fn paint_gapless(
    panes: &[Pane],
    active: Option<GridId>,
    borders: BorderSet,
    quiet: Style,
    accent: Style,
    area: Rect,
    damage: &Damage,
    buf: &mut Buffer,
) {
    let lattice = lattice(panes, area, buf.area);
    let edges: BTreeSet<Cell> = active
        .and_then(|id| panes.iter().find(|pane| is_tile(pane) && pane.id == id))
        .map(|pane| perimeter(pane, &lattice, area))
        .unwrap_or_default();
    for &(row, col) in &lattice {
        if !damage.covers(row) {
            continue;
        }
        let style = if edges.contains(&(row, col)) {
            accent
        } else {
            quiet
        };
        set_border_cell(buf, col, row, junction(&lattice, row, col, borders), style);
    }
}

/// Every screen cell the gapless lattice runs through: the column right of
/// each tile and the row under it, which are the cells nvim draws its
/// separator and status row into, plus the ring's top row and left column,
/// which are the edges the topmost and leftmost tiles have no neighbour to
/// share.
fn lattice(panes: &[Pane], area: Rect, screen: Rect) -> BTreeSet<Cell> {
    let mut cells = BTreeSet::new();
    // the grid's last row is nvim's command line, where the mode message
    // and the answer to every prompt are written, so no run of the lattice
    // reaches it -- the same bound the gapped ring already stops at
    let cmdline_row = area.height.saturating_sub(1);
    for pane in panes.iter().filter(|pane| is_tile(pane)) {
        let (row, col, width, height) = pane.slot;
        let (edge_col, edge_row) = (col.saturating_add(width), row.saturating_add(height));
        if edge_col < area.width {
            for r in row..=edge_row.min(cmdline_row.saturating_sub(1)) {
                cells.insert((area.y.saturating_add(r), area.x.saturating_add(edge_col)));
            }
        }
        if edge_row < cmdline_row {
            for c in col..=edge_col.min(area.width.saturating_sub(1)) {
                cells.insert((area.y.saturating_add(edge_row), area.x.saturating_add(c)));
            }
        }
    }
    let (Some(top), Some(left)) = (area.y.checked_sub(1), area.x.checked_sub(1)) else {
        return cells;
    };
    let bottom = cells.iter().map(|&(row, _)| row).max().unwrap_or(top);
    for col in left..area.x.saturating_add(area.width).min(screen.width) {
        cells.insert((top, col));
    }
    for row in top..=bottom {
        cells.insert((row, left));
    }
    cells
}

/// The lattice cells that are this tile's own edges: the ring one cell out
/// from its slot on all four sides. A side the lattice does not reach is
/// the terminal's own edge, which nothing draws.
fn perimeter(pane: &Pane, lattice: &BTreeSet<Cell>, area: Rect) -> BTreeSet<Cell> {
    let (row, col, width, height) = pane.slot;
    let (row, col) = (area.y.saturating_add(row), area.x.saturating_add(col));
    let (top, bottom) = (row.saturating_sub(1), row.saturating_add(height));
    let (left, right) = (col.saturating_sub(1), col.saturating_add(width));
    let mut cells = BTreeSet::new();
    for c in left..=right {
        cells.insert((top, c));
        cells.insert((bottom, c));
    }
    for r in top..=bottom {
        cells.insert((r, left));
        cells.insert((r, right));
    }
    cells.retain(|cell| lattice.contains(cell));
    cells
}

/// The glyph one lattice cell takes, from the four directions a line leaves
/// it in.
fn junction(lattice: &BTreeSet<Cell>, row: u16, col: u16, borders: BorderSet) -> char {
    let linked = |r: u16, c: u16| lattice.contains(&(r, c));
    let up = row.checked_sub(1).is_some_and(|r| linked(r, col));
    let down = linked(row.saturating_add(1), col);
    let left = col.checked_sub(1).is_some_and(|c| linked(row, c));
    let right = linked(row, col.saturating_add(1));
    let unicode = borders.horizontal != '-';
    let crossing = |glyph: char| if unicode { glyph } else { '+' };
    match (up, down, left, right) {
        (true, true, true, true) => crossing('\u{253C}'),
        (true, true, false, true) => crossing('\u{251C}'),
        (true, true, true, false) => crossing('\u{2524}'),
        (false, true, true, true) => crossing('\u{252C}'),
        (true, false, true, true) => crossing('\u{2534}'),
        (false, true, false, true) => borders.top_left,
        (false, true, true, false) => borders.top_right,
        (true, false, false, true) => borders.bottom_left,
        (true, false, true, false) => borders.bottom_right,
        (true, _, false, false) | (false, true, false, false) => borders.vertical,
        _ => borders.horizontal,
    }
}

/// Writes one glyph at a slot coordinate, if the row is being repainted
/// and the cell is inside the layer.
#[allow(clippy::too_many_arguments)]
fn put(
    row: u16,
    col: u16,
    glyph: char,
    style: Style,
    area: Rect,
    damage: &Damage,
    buf: &mut Buffer,
) {
    if let Some((x, y)) = screen(row, col, area, damage) {
        set_border_cell(buf, x, y, glyph, style);
    }
}

/// A slot coordinate as a screen one, or `None` where the cell is outside
/// the layer or its row is not being repainted.
fn screen(row: u16, col: u16, area: Rect, damage: &Damage) -> Option<(u16, u16)> {
    if row >= area.height || col >= area.width {
        return None;
    }
    let y = area.y.checked_add(row)?;
    let x = area.x.checked_add(col)?;
    if !damage.covers(y) {
        return None;
    }
    Some((x, y))
}
