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
use view_core::grid::registry::{GridId, Pane, GLOBAL_GRID};
use view_core::model::{Look, Model, Panes, WindowStatus};
use view_core::native::statusline::StatuslineState;
use view_core::native::surfaces::{view_draws, Surface};
use view_core::native::views::{Span, StyleRole};
use view_core::theme::{ChromeGroup, ResolvedStyle, Theme};
use view_surface::overlay::BorderSet;

use super::super::text::{cluster_width, clusters, group_width, set_cluster};
use super::super::{ratatui_style, rgb, set_border_cell, Damage};

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
    model: &Model,
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
    // the first row nvim keeps for itself at the grid's foot, which every
    // run and every clear below stops above. It is the grid's own height
    // where view owns the command line and the message area both, since
    // the takeover then holds `cmdheight` at 0 and the last row is a
    // window's status row like any other
    let foot = area.height.saturating_sub(model.cmdline_rows());
    if look.gaps {
        paint_gapped(
            panes, active, theme, borders, quiet, accent, area, foot, damage, buf,
        );
    } else {
        paint_gapless(
            panes, active, borders, quiet, accent, area, foot, damage, buf,
        );
    }
    paint_edges(model, panes, look, active, theme, area, foot, damage, buf);
}

/// Whether a pane is one of the tiles a frame is drawn around: the global
/// grid carries chrome rather than a window, and a float or a message grid
/// has no slot of its own.
fn is_tile(pane: &Pane) -> bool {
    pane.id != GLOBAL_GRID && pane.kind.is_window()
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

/// Writes each tile's buffer name and status segments into the frame the
/// two painters above have just drawn, over the line glyphs already there.
///
/// Runs after both of them because the run's own cells are frame cells: a
/// gapless tile's bottom edge is a lattice row the junction walk fills end
/// to end, and text written before it would be painted back over.
#[allow(clippy::too_many_arguments)]
fn paint_edges(
    model: &Model,
    panes: &[Pane],
    look: Look,
    active: Option<GridId>,
    theme: &Theme,
    area: Rect,
    foot: u16,
    damage: &Damage,
    buf: &mut Buffer,
) {
    // the surface the frame paints, handed back by `[native] statusline =
    // false`: the tile keeps its bottom edge and the edge carries nothing
    if !view_draws(Surface::Frame, model) {
        return;
    }
    let state = &model.engine.statusline;
    for pane in panes.iter().filter(|pane| is_tile(pane)) {
        let Some(status) = model
            .engine
            .grids()
            .window_handle(pane.id)
            .and_then(|win| model.window_status.get(&win))
        else {
            continue;
        };
        // a native surface's window holds an unnamed scratch buffer, so the
        // frame would carry nothing where every other tile carries a name
        let status = match pane.kind.native_surface() {
            Some(surface) => {
                let mut named = status.clone();
                named.name = surface.id().to_string();
                std::borrow::Cow::Owned(named)
            }
            None => std::borrow::Cow::Borrowed(status),
        };
        let status = &*status;
        let is_active = active == Some(pane.id);
        if look.gaps {
            if !framed(pane) {
                continue;
            }
            let (top, bottom) = gapped_edges(pane, area, buf.area);
            if let Some(edge) = top.filter(|edge| damage.covers(edge.y)) {
                paint_name(status, is_active, theme, edge, buf);
            }
            if let Some(edge) = bottom.filter(|edge| damage.covers(edge.y)) {
                paint_segments(status, is_active, state, theme, look, edge, buf);
            }
        } else if let Some(edge) =
            gapless_edge(pane, area, foot, buf.area).filter(|edge| damage.covers(edge.y))
        {
            paint_segments(status, is_active, state, theme, look, edge, buf);
        }
    }
}

/// A gapped tile's top and bottom frame edges as screen rects, in the same
/// coordinates [`box_edge`] draws the box in.
fn gapped_edges(pane: &Pane, area: Rect, screen: Rect) -> (Option<Rect>, Option<Rect>) {
    let (row, col, width, height) = pane.slot;
    let (box_w, box_h) = (width.saturating_sub(2), height.saturating_sub(2));
    if box_w < 2 || box_h < 2 {
        return (None, None);
    }
    let (top, left) = (row.saturating_add(1), col.saturating_add(1));
    let bottom = top.saturating_add(box_h).saturating_sub(1);
    let run = |r: u16| {
        (r < area.height && left < area.width).then(|| {
            clipped(
                Rect::new(
                    area.x.saturating_add(left),
                    area.y.saturating_add(r),
                    box_w.min(area.width.saturating_sub(left)),
                    1,
                ),
                screen,
            )
        })?
    };
    (run(top), run(bottom))
}

/// A gapless tile's one edge run as a screen rect: the lattice row under
/// the slot, from the cell left of the slot to the cell right of it, which
/// are the junctions that row runs between.
///
/// `None` where that row is one nvim keeps for its command line, the
/// bound [`lattice`] stops at, since no frame is drawn there to write
/// into.
fn gapless_edge(pane: &Pane, area: Rect, foot: u16, screen: Rect) -> Option<Rect> {
    let (row, col, width, height) = pane.slot;
    let edge_row = row.saturating_add(height);
    if edge_row >= foot {
        return None;
    }
    let left = area.x.saturating_add(col).saturating_sub(1);
    let right = area
        .x
        .saturating_add(col.saturating_add(width).min(area.width.saturating_sub(1)));
    clipped(
        Rect::new(
            left,
            area.y.saturating_add(edge_row),
            right.saturating_sub(left).saturating_add(1),
            1,
        ),
        screen,
    )
}

/// `rect` where it lies wholly inside `screen`, which is what the per-cell
/// writes below index without bounds of their own.
fn clipped(rect: Rect, screen: Rect) -> Option<Rect> {
    let inside = rect.y >= screen.y
        && rect.y < screen.y.saturating_add(screen.height)
        && rect.x >= screen.x
        && rect.x.saturating_add(rect.width) <= screen.x.saturating_add(screen.width);
    inside.then_some(rect)
}

/// Writes one tile's buffer name into a frame edge.
pub(crate) fn paint_name(
    status: &WindowStatus,
    active: bool,
    theme: &Theme,
    edge: Rect,
    buf: &mut Buffer,
) {
    write_edge(
        &[name_spans(status)],
        edge_style(active, theme),
        theme,
        edge,
        buf,
    );
}

/// Writes one tile's status segments into a frame edge.
///
/// A gapless tile has one edge row and no top run of its own, so its name
/// leads the segments there; a gapped tile's name is already in the top
/// edge and this is the segments alone.
pub(crate) fn paint_segments(
    status: &WindowStatus,
    active: bool,
    state: &StatuslineState,
    theme: &Theme,
    look: Look,
    edge: Rect,
    buf: &mut Buffer,
) {
    let mut groups = if look.gaps {
        Vec::new()
    } else {
        vec![name_spans(status)]
    };
    groups.extend(state.tile_segments(status, active));
    write_edge(&groups, edge_style(active, theme), theme, edge, buf);
}

/// The buffer name a tile's frame carries, with the unsaved marker behind
/// it.
fn name_spans(status: &WindowStatus) -> Vec<Span> {
    if status.name.is_empty() {
        return Vec::new();
    }
    let mut spans = vec![Span::new(status.name.clone(), StyleRole::File)];
    if status.modified {
        spans.push(Span::new(" [+]", StyleRole::Modified));
    }
    spans
}

/// The colour a tile's own edge text sits in, which is the colour its frame
/// is drawn in.
fn edge_style(active: bool, theme: &Theme) -> Style {
    if active {
        ratatui_style(theme.accent())
    } else {
        quiet_style(theme)
    }
}

/// One span's colour: the group its role names, or the frame's own where it
/// names none.
///
/// `StatusLine` is the bar's own group, and under tiles there is no bar, so
/// a role resolving to it reads as unnamed here rather than painting a
/// frame edge in the colours of a row that is not on screen.
fn span_style(role: StyleRole, base: Style, theme: &Theme) -> Style {
    match role.chrome_group() {
        None | Some(ChromeGroup::StatusLine) => base,
        Some(group) => theme.chrome(group).fg.map_or(base, |fg| base.fg(rgb(fg))),
    }
}

/// Writes `groups` along a one-row frame edge, one blank cell between
/// groups and one either side of the whole run, with the corners left to
/// their junction glyphs.
///
/// A group that does not fit is dropped whole, its separator with it, and
/// so is everything after it: the composer knows what a segment means and a
/// column count does not, so half a diagnostic count is worse than none.
///
/// Width is counted in cells rather than in characters, and the run places
/// one grapheme cluster per cell: a buffer named in a script that draws
/// two cells to the character would otherwise run past the closing blank
/// and over the corner, and one carrying a combining mark would show the
/// mark standing on its own.
fn write_edge(groups: &[Vec<Span>], base: Style, theme: &Theme, edge: Rect, buf: &mut Buffer) {
    // two corners, a blank either side and one character of text
    if edge.width < 5 {
        return;
    }
    let first = edge.x.saturating_add(2);
    let last = edge.x.saturating_add(edge.width).saturating_sub(3);
    // the closing blank's own cell, and the last cell inside the edge that
    // any write below may reach: `set_border_cell` indexes the buffer with
    // no bounds of its own, so a run that walked past this would panic
    let stop = last.saturating_add(1);
    let mut x = first;
    for group in groups.iter().filter(|group| group_width(group) > 0) {
        let separator = u16::from(x > first);
        if x.saturating_add(separator)
            .saturating_add(group_width(group))
            > last.saturating_add(1)
        {
            break;
        }
        if separator == 1 {
            set_border_cell(buf, x, edge.y, ' ', base);
            x = x.saturating_add(1);
        }
        for span in group {
            let style = span_style(span.role, base, theme);
            for cluster in clusters(&span.text) {
                if x > last {
                    break;
                }
                let width = cluster_width(cluster);
                set_cluster(buf, x, edge.y, cluster, style);
                // ratatui's own convention for the cell a two-cell glyph
                // covers: the diff skips it, and anything left in it would
                // be drawn one column to the right of where it was written
                if width == 2 && x < stop {
                    buf[(x.saturating_add(1), edge.y)].reset();
                }
                x = x.saturating_add(width);
            }
        }
    }
    if x == first {
        return;
    }
    set_border_cell(buf, first.saturating_sub(1), edge.y, ' ', base);
    if x <= stop {
        set_border_cell(buf, x, edge.y, ' ', base);
    }
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
    foot: u16,
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
        // a row nvim keeps for its command line is where the mode message
        // and the answer to every prompt are written, so every clear stops
        // above `foot`; any other row under a slot is the status row the
        // frame replaces
        let under = u16::from(row.saturating_add(height) < foot);
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
    foot: u16,
    damage: &Damage,
    buf: &mut Buffer,
) {
    let lattice = lattice(panes, area, foot, buf.area);
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
fn lattice(panes: &[Pane], area: Rect, foot: u16, screen: Rect) -> BTreeSet<Cell> {
    let mut cells = BTreeSet::new();
    // a row nvim keeps for its command line is where the mode message and
    // the answer to every prompt are written, so no run of the lattice
    // reaches `foot` -- the same bound the gapped ring already stops at
    for pane in panes.iter().filter(|pane| is_tile(pane)) {
        let (row, col, width, height) = pane.slot;
        let (edge_col, edge_row) = (col.saturating_add(width), row.saturating_add(height));
        if edge_col < area.width {
            for r in row..=edge_row.min(foot.saturating_sub(1)) {
                cells.insert((area.y.saturating_add(r), area.x.saturating_add(edge_col)));
            }
        }
        if edge_row < foot {
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
