//! The frame view draws around every window under `panes = "tiles"`.
//!
//! Gapped and gapless are the same picture drawn in two places. A gapped
//! tile's frame is the outer ring of the slot nvim gave the window, and
//! the separator column and status row nvim paints just past the slot are
//! cleared to the one cell of gap between two frames. A gapless tile has
//! no room of its own: its frame is the cell *between* two slots, which is
//! that same separator column and status row, restyled. One cell between
//! two tiles is what makes a junction glyph possible at all, since a
//! doubled line leaves nowhere for one to sit.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use std::collections::BTreeSet;
use view_core::grid::registry::{GridId, Pane, GLOBAL_GRID};
use view_core::model::{Look, Model, Panes, TileKind, WindowStatus};
use view_core::native::geometry::{Anchor, NativeSurface};
use view_core::native::statusline::StatuslineState;
use view_core::native::surfaces::{view_draws, Surface};
use view_core::native::views::{Span, StyleRole};
use view_core::theme::{ChromeGroup, ResolvedStyle, Theme};
use view_surface::overlay::BorderSet;

use super::super::text::{cluster_width, clusters, group_width, set_cluster};
use super::super::{border_color, ratatui_style, rgb, set_border_cell, Damage};

/// One cell of the lattice, as `(row, col)` on the screen.
type Cell = (u16, u16);

/// The gapless lattice's cells, kept apart by the direction of the run
/// through each. A line joins two neighbouring cells only along a run both
/// of them lie on, so two parallel runs one cell apart stay two lines; a
/// corner lies on both of its runs.
#[derive(Default)]
struct Lattice {
    across: BTreeSet<Cell>,
    down: BTreeSet<Cell>,
}

impl Lattice {
    fn across(&mut self, row: u16, cols: std::ops::RangeInclusive<u16>) {
        self.across.extend(cols.map(|col| (row, col)));
    }

    fn down(&mut self, col: u16, rows: std::ops::RangeInclusive<u16>) {
        self.down.extend(rows.map(|row| (row, col)));
    }

    fn cells(&self) -> BTreeSet<Cell> {
        self.across.union(&self.down).copied().collect()
    }
}

/// Paints the tiles' frames over the window panes already composited into
/// `buf`, and under every float the caller paints after it.
///
/// `panes` is the compositor's own z-ordered list, read here as it stands;
/// the window panes are the tiles and everything else is skipped. `area` is
/// the engine-grid layer's rect, the same one the panes were painted
/// inside, so a slot's coordinates are applied within it exactly once. A
/// look other than tiles paints nothing at all.
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
    // where view draws the message area, since nvim then keeps
    // `cmdheight` at 0 and the last row is a window's status row like any
    // other
    let foot = area.height.saturating_sub(model.cmdline_rows());
    let gap = ratatui_style(theme.normal());
    let joins = joins(model, buf.area);
    if look.gaps {
        let tiles = panes.iter().filter(|pane| is_tile(pane) && framed(pane));
        clear_bare(tiles, gap, area, foot, damage, buf);
        paint_gapped(
            model, panes, look, active, borders, quiet, accent, area, damage, buf,
        );
    } else {
        clear_bare(
            panes.iter().filter(|pane| is_tile(pane)),
            gap,
            area,
            foot,
            damage,
            buf,
        );
        paint_gapless(
            panes, &joins, active, borders, quiet, accent, area, foot, damage, buf,
        );
    }
    paint_edges(
        model, panes, look, &joins, active, theme, area, foot, damage, buf,
    );
}

/// Whether a pane is one of the tiles a frame is drawn around: the global
/// grid carries chrome and no window, and a float or a message grid
/// has no slot of its own.
fn is_tile(pane: &Pane) -> bool {
    pane.id != GLOBAL_GRID && pane.kind.is_window()
}

/// The frame colour of a tile the user is not working in: `WinSeparator`'s
/// foreground as the colorscheme states it, which is the colour it chose
/// for the lines between windows, or the dimmed text colour a float border
/// derives where the separator is only the text colour again (unset, or
/// linked to `Normal` as nvim's default scheme does). The accent alone is
/// what picks out the active tile.
///
/// A stated separator colour is never dimmed further: colorschemes already
/// pick it as their muted shade.
fn quiet_style(theme: &Theme) -> Style {
    let normal = theme.normal();
    let fg = theme
        .chrome(ChromeGroup::WinSeparator)
        .fg
        .filter(|fg| Some(*fg) != normal.fg)
        .unwrap_or_else(|| border_color(normal));
    ratatui_style(ResolvedStyle {
        fg: Some(fg),
        ..normal
    })
}

/// Whether the tile's grid sits inside its slot, which is what a frame
/// needs room for. A slot too small to spare the ring keeps its whole
/// grid, and the registry places it at the slot's own origin.
fn framed(pane: &Pane) -> bool {
    let (row, col, _, _) = pane.filled;
    pane.origin != (row, col)
}

/// A gapped tile's frame box: [`Model::tile_box`], closed short of every
/// docked float's gutter.
fn gapped_box(model: &Model, look: Look, pane: &Pane) -> Option<(u16, u16, u16, u16)> {
    look.frame_box(model.tile_box(pane)?)
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
    joins: &[Join],
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
            .painted_grids()
            .window_handle(pane.id)
            .and_then(|win| model.window_status.get(&win))
        else {
            continue;
        };
        // a native surface's window holds a scratch buffer nvim reports as
        // any other, so the surface is what names the kind, and the name is
        // what the surface holds: the agent panel's agent, and nothing for
        // a surface holding no one thing
        let status = match pane.kind.native_surface() {
            Some(surface) => {
                let mut native = status.clone();
                native.kind = TileKind::Native(surface);
                native.name = match surface {
                    NativeSurface::Agent => model.ai_panel().name().unwrap_or_default().to_owned(),
                    _ => String::new(),
                };
                std::borrow::Cow::Owned(native)
            }
            None => std::borrow::Cow::Borrowed(status),
        };
        let status = &*status;
        let is_active = active == Some(pane.id);
        let title = || status.kind.title_with(status, &model.tile_titles);
        if look.gaps {
            if !framed(pane) {
                continue;
            }
            let (top, bottom) = gapped_edges(model, look, pane, area, buf.area);
            if let Some(edge) = top.filter(|edge| damage.covers(edge.y)) {
                paint_name(title(), is_active, theme, edge, buf);
            }
            if let Some(edge) = bottom.filter(|edge| damage.covers(edge.y)) {
                paint_segments(status, None, is_active, state, theme, edge, buf);
            }
        } else if let Some(edge) =
            gapless_edge(pane, joins, area, foot, buf.area).filter(|edge| damage.covers(edge.y))
        {
            paint_segments(status, Some(title()), is_active, state, theme, edge, buf);
        }
    }
}

/// A gapped tile's top and bottom frame edges as screen rects, in the same
/// coordinates [`box_edge`] draws the box in.
fn gapped_edges(
    model: &Model,
    look: Look,
    pane: &Pane,
    area: Rect,
    screen: Rect,
) -> (Option<Rect>, Option<Rect>) {
    let Some((top, left, box_w, box_h)) = gapped_box(model, look, pane) else {
        return (None, None);
    };
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
fn gapless_edge(pane: &Pane, joins: &[Join], area: Rect, foot: u16, screen: Rect) -> Option<Rect> {
    let (row, col, width, height) = pane.filled;
    let edge_row = row.saturating_add(height);
    if edge_row >= foot {
        return None;
    }
    let y = area.y.saturating_add(edge_row);
    let (left, right) = joined_sides(
        joins,
        y..=y,
        area.x.saturating_add(col).saturating_sub(1),
        area.x.saturating_add(col.saturating_add(width)),
    );
    let right = right.min(screen.x.saturating_add(screen.width).saturating_sub(1));
    clipped(
        Rect::new(left, y, right.saturating_sub(left).saturating_add(1), 1),
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

/// Writes one tile's title into a frame edge.
pub(crate) fn paint_name(
    title: Vec<Span>,
    active: bool,
    theme: &Theme,
    edge: Rect,
    buf: &mut Buffer,
) {
    write_edge(&[title], edge_style(active, theme), theme, edge, buf);
}

/// Writes one tile's status segments into a frame edge, after `title`.
///
/// A gapless tile has one edge row and no top run of its own, so its title
/// leads the segments there; a gapped tile's title is already in the top
/// edge and passes none.
pub(crate) fn paint_segments(
    status: &WindowStatus,
    title: Option<Vec<Span>>,
    active: bool,
    state: &StatuslineState,
    theme: &Theme,
    edge: Rect,
    buf: &mut Buffer,
) {
    let mut groups: Vec<Vec<Span>> = title.into_iter().collect();
    groups.extend(state.tile_segments(status, active));
    write_edge(&groups, edge_style(active, theme), theme, edge, buf);
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
/// a role resolving to it reads as unnamed here, so no frame edge is
/// painted in the colours of a row that is not on screen.
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
/// Width is counted in cells, and the run places one grapheme cluster per
/// cell: a buffer named in a script that draws two cells to the character
/// would otherwise run past the closing blank and over the corner, and one
/// carrying a combining mark would show the mark standing on its own.
fn write_edge(groups: &[Vec<Span>], base: Style, theme: &Theme, edge: Rect, buf: &mut Buffer) {
    // two corners, a blank either side and one character of text
    if edge.width < 5 {
        return;
    }
    let (first, last) = view_surface::overlay::title_cells(edge.width);
    let (first, last) = (edge.x.saturating_add(first), edge.x.saturating_add(last));
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

/// Clears the separator column and status row nvim paints just past each
/// of `tiles`' slots, and the part of the slot its grid does not cover, to
/// the background the tiles sit on.
///
/// A stale separator left there survives every later redraw, because nvim
/// believes the window's own grid covers it. The bare part of a slot holds
/// whatever the global grid kept from the layout before a resize, an old
/// status or command-line row among it, until nvim resizes the grid. A
/// gapless lattice is drawn over these cells afterwards, which leaves only
/// that bare part blank under it.
fn clear_bare<'a>(
    tiles: impl Iterator<Item = &'a Pane>,
    gap: Style,
    area: Rect,
    foot: u16,
    damage: &Damage,
    buf: &mut Buffer,
) {
    for pane in tiles {
        // the slot is read here because its part the grid leaves bare is
        // what this clears
        let (row, col, width, height) = pane.slot;
        let (_, _, filled_width, filled_height) = pane.filled;
        // a row nvim keeps for its command line is where the mode message
        // and the answer to every prompt are written, so every clear stops
        // above `foot`; any other row under a slot is the status row
        let under = u16::from(row.saturating_add(height) < foot);
        clear(
            row,
            col.saturating_add(filled_width),
            width.saturating_sub(filled_width).saturating_add(1),
            height.saturating_add(under),
            gap,
            area,
            damage,
            buf,
        );
        clear(
            row.saturating_add(filled_height),
            col,
            width.saturating_add(1),
            height.saturating_sub(filled_height).saturating_add(under),
            gap,
            area,
            damage,
            buf,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_gapped(
    model: &Model,
    panes: &[Pane],
    look: Look,
    active: Option<GridId>,
    borders: BorderSet,
    quiet: Style,
    accent: Style,
    area: Rect,
    damage: &Damage,
    buf: &mut Buffer,
) {
    for pane in panes.iter().filter(|pane| is_tile(pane) && framed(pane)) {
        let Some((row, col, width, height)) = gapped_box(model, look, pane) else {
            continue;
        };
        let style = if active == Some(pane.id) {
            accent
        } else {
            quiet
        };
        box_edge(row, col, width, height, borders, style, area, damage, buf);
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

/// A float docked to the left or right of the screen under gapless tiles,
/// in screen cells. Its tile-side column is a lattice column, the way the
/// column between two tiles is, so the lines of the tiles beside it meet
/// it in a junction.
#[derive(Debug, Clone, Copy)]
struct Join {
    /// The float's column on the tiles' side.
    col: u16,
    top: u16,
    bottom: u16,
    /// The float's first and last columns.
    first: u16,
    last: u16,
    right: bool,
}

impl Join {
    /// Whether a run at `col` on `row` lies under the float, past the
    /// column it joins on.
    fn covers(self, row: u16, col: u16) -> bool {
        let beyond = if self.right {
            col > self.col
        } else {
            col < self.col
        };
        row > self.top && row < self.bottom && beyond
    }
}

/// Every float that joins the gapless lattice: the agent panel and the
/// tree, docked to a side and drawn as floats.
fn joins(model: &Model, screen: Rect) -> Vec<Join> {
    model
        .overlays()
        .iter()
        .filter_map(|open| {
            let right = model.joined_anchor(open)? == Anchor::Right;
            let rect = model.overlay_rect(open);
            let first = rect.col;
            let last = rect
                .col
                .saturating_add(rect.width)
                .min(screen.x.saturating_add(screen.width))
                .checked_sub(1)?;
            let bottom = rect
                .row
                .saturating_add(rect.height)
                .min(screen.y.saturating_add(screen.height))
                .checked_sub(1)?;
            Some(Join {
                col: if right { first } else { last },
                top: rect.row,
                bottom,
                first,
                last,
                right,
            })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn paint_gapless(
    panes: &[Pane],
    joins: &[Join],
    active: Option<GridId>,
    borders: BorderSet,
    quiet: Style,
    accent: Style,
    area: Rect,
    foot: u16,
    damage: &Damage,
    buf: &mut Buffer,
) {
    let (lattice, edges) = gapless_lattice(panes, joins, active, area, foot, buf.area);
    for (row, col) in lattice.cells() {
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

/// The gapless lattice and the cells of it the active tile's frame is
/// drawn on.
fn gapless_lattice(
    panes: &[Pane],
    joins: &[Join],
    active: Option<GridId>,
    area: Rect,
    foot: u16,
    screen: Rect,
) -> (Lattice, BTreeSet<Cell>) {
    let lattice = lattice(panes, joins, area, foot, screen);
    let cells = lattice.cells();
    let edges = active
        .and_then(|id| panes.iter().find(|pane| is_tile(pane) && pane.id == id))
        .map(|pane| perimeter(pane, joins, &cells, area))
        .unwrap_or_default();
    (lattice, edges)
}

/// Every screen cell the gapless lattice runs through: the column right of
/// each tile and the row under it, which are the cells nvim draws its
/// separator and status row into, the column left of it and the row above
/// it, plus the ring's top row and its left and right columns, which are
/// the edges the outermost tiles have no neighbour to share. Each run
/// reaches the corner cell past either end of the tile it closes.
///
/// A joined float's tile-side column and its top and bottom rows are runs
/// of the lattice too, and a tile's run under the float stops at that
/// column, so each line meeting it ends in a tee.
fn lattice(panes: &[Pane], joins: &[Join], area: Rect, foot: u16, screen: Rect) -> Lattice {
    let mut lattice = tile_lattice(panes, area, foot, screen);
    for join in joins {
        lattice.across.retain(|&(row, col)| !join.covers(row, col));
        lattice.down(join.col, join.top..=join.bottom);
        lattice.across(join.top, join.first..=join.last);
        lattice.across(join.bottom, join.first..=join.last);
    }
    lattice
}

/// The runs the tiles and the ring put in the lattice.
fn tile_lattice(panes: &[Pane], area: Rect, foot: u16, screen: Rect) -> Lattice {
    let mut lattice = Lattice::default();
    let ring = area.y.checked_sub(1).zip(area.x.checked_sub(1));
    let (first_row, first_col) = ring.unwrap_or((area.y, area.x));
    let ring_right = area.x.saturating_add(area.width);
    let last_col = if ring.is_some() && ring_right < screen.width {
        ring_right
    } else {
        ring_right.saturating_sub(1)
    };
    // a row nvim keeps for its command line is where the mode message and
    // the answer to every prompt are written, so no run of the lattice
    // reaches `foot` -- the same bound the gapped ring already stops at
    let last_row = area.y.saturating_add(foot.saturating_sub(1));
    for pane in panes.iter().filter(|pane| is_tile(pane)) {
        let (row, col, width, height) = pane.filled;
        let (top, left) = (area.y.saturating_add(row), area.x.saturating_add(col));
        let (bottom, right) = (top.saturating_add(height), left.saturating_add(width));
        let rows = top.saturating_sub(1).max(first_row)..=bottom.min(last_row);
        let cols = left.saturating_sub(1).max(first_col)..=right.min(last_col);
        // a tile's left and top edges are its neighbours' right and bottom
        // ones in a settled layout; while a resize is half applied a
        // neighbour's grid stops short of them, and the tile would stand
        // open on that side
        if let Some(before) = left.checked_sub(1).filter(|&c| c >= first_col) {
            lattice.down(before, rows.clone());
        }
        if let Some(above) = top.checked_sub(1).filter(|&r| r >= first_row) {
            lattice.across(above, cols.clone());
        }
        if right <= last_col {
            lattice.down(right, rows);
        }
        if row.saturating_add(height) < foot {
            lattice.across(bottom, cols);
        }
    }
    let Some((top, left)) = ring else {
        return lattice;
    };
    // each ring column ends at the lowest corner of the tiles beside it: a
    // tile whose grid is still a row short while its neighbour's is resized
    // closes its side one row higher, and nothing runs on under it
    let lowest = |beside: &dyn Fn(&Pane) -> bool| {
        panes
            .iter()
            .filter(|pane| is_tile(pane) && beside(pane))
            .map(|pane| {
                let (row, _, _, height) = pane.filled;
                area.y
                    .saturating_add(row)
                    .saturating_add(height)
                    .min(last_row)
            })
            .fold(top, u16::max)
    };
    lattice.across(top, left..=last_col);
    lattice.down(left, top..=lowest(&|pane| pane.slot.1 == 0));
    if ring_right < screen.width {
        let bottom = lowest(&|pane| pane.slot.1.saturating_add(pane.slot.2) >= area.width);
        lattice.down(ring_right, top..=bottom);
    }
    lattice
}

/// The lattice cells that are this tile's own edges: the ring one cell out
/// from its slot on all four sides. A side the lattice does not reach is
/// the terminal's own edge, which nothing draws. A joined float's column
/// is the side of a tile it covers part of.
fn perimeter(pane: &Pane, joins: &[Join], lattice: &BTreeSet<Cell>, area: Rect) -> BTreeSet<Cell> {
    let (row, col, width, height) = pane.filled;
    let (row, col) = (area.y.saturating_add(row), area.x.saturating_add(col));
    let (top, bottom) = (row.saturating_sub(1), row.saturating_add(height));
    let (left, right) = joined_sides(
        joins,
        top..=bottom,
        col.saturating_sub(1),
        col.saturating_add(width),
    );
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

/// A tile's left and right lattice columns, `left` and `right`, moved to
/// the column of each joined float that covers part of the tile on any of
/// `rows`.
fn joined_sides(
    joins: &[Join],
    rows: std::ops::RangeInclusive<u16>,
    mut left: u16,
    mut right: u16,
) -> (u16, u16) {
    for join in joins {
        if *rows.start() > join.bottom || *rows.end() < join.top {
            continue;
        }
        if left < join.col && right > join.col {
            if join.right {
                right = join.col;
            } else {
                left = join.col;
            }
        }
    }
    (left, right)
}

/// The glyph one lattice cell takes, from the four directions a line leaves
/// it in.
fn junction(lattice: &Lattice, row: u16, col: u16, borders: BorderSet) -> char {
    let across =
        |c: u16| lattice.across.contains(&(row, col)) && lattice.across.contains(&(row, c));
    let along = |r: u16| lattice.down.contains(&(row, col)) && lattice.down.contains(&(r, col));
    let up = row.checked_sub(1).is_some_and(along);
    let down = along(row.saturating_add(1));
    let left = col.checked_sub(1).is_some_and(across);
    let right = across(col.saturating_add(1));
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

/// Paints the windowed command palette's band through this same primitive,
/// apart from the float style every other overlay uses, so the band joins
/// the one family of tiles: a plain `box_edge` border (no gapless lattice,
/// since the band has no neighbouring slot to share a junction with), the
/// surface's own title on the top edge in the surface's own style, and an
/// empty bottom edge, matching a real windowed tile's own footer with no window
/// status to state.
///
/// `area` is already the band's own rect (`Model::palette_rect`'s
/// `windowed` answer): the caller's gap ring, if any, is baked into that
/// rect by `palette_rect` itself, so the border is drawn flush with
/// `area`'s own edges here, the way a real tile's `box_edge` call draws on
/// its slot's own edges.
///
/// The border is always `theme.accent()`, never `quiet_style`: a real tile
/// only earns the active colour while the user is in it, but the band holds
/// the caret every frame it is open at all, so it is never the unfocused
/// tile a quiet frame would mean.
pub(crate) fn paint_windowed_palette(
    layer: &view_surface::Layer,
    theme: &Theme,
    borders: BorderSet,
    area: Rect,
    damage: &Damage,
    buf: &mut Buffer,
) {
    let view_surface::LayerKind::Palette(view) = &layer.kind else {
        return;
    };
    if area.width < 2 || area.height < 2 {
        return;
    }
    let style = ratatui_style(theme.accent());
    box_edge(
        0,
        0,
        area.width,
        area.height,
        borders,
        style,
        area,
        damage,
        buf,
    );
    if damage.covers_row_of(area, 0) {
        let top_edge = Rect::new(area.x, area.y, area.width, 1);
        write_edge(
            &[vec![Span::new(view.title.clone(), StyleRole::Title)]],
            style,
            theme,
            top_edge,
            buf,
        );
    }
    let (row_off, col_off) =
        view_surface::overlay::windowed_interior_origin(area.width, area.height);
    let interior = Rect::new(
        area.x.saturating_add(col_off),
        area.y.saturating_add(row_off),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    if interior.width == 0 || interior.height == 0 {
        return;
    }
    let laid =
        view_surface::overlay::unframed_rows(interior.width, interior.height, &layer.kind, borders);
    super::super::paint_native_overlay(layer, Some(&laid), theme, interior, damage, buf);
}
