//! The pane compositor: every visible grid painted where nvim placed it,
//! and view's own chrome drawn over the space between them.
//!
//! Under `ext_multigrid` a window's text arrives in a grid of its own and
//! the picture on screen is something view builds rather than something
//! nvim sent. nvim does not leave the column between two side-by-side
//! windows empty -- it paints a separator there into the global grid, and
//! `docs/multigrid-wire-capture.md` records the line
//! (`grid_line [1, 0, 40, [["│", 12]], false]`, "the separator column 40
//! belonging to grid 1"). What multigrid changes is that grid 1 now holds
//! *only* chrome, so view can restyle that column instead of having to
//! pick it out of buffer text: the global-grid pane paints first,
//! [`paint_separators`] overpaints its separator cells with a glyph from
//! the probed box-drawing charset styled through
//! [`ChromeGroup::WinSeparator`], and a user's colorscheme decides what the
//! result looks like.
//!
//! Single-grid sessions come through here unchanged: nvim places no window
//! of its own, so [`GridRegistry::has_panes`] is false and this reduces to
//! the one [`super::paint_grid`] call the compositor always made.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as TermRect;
use view_core::grid::registry::{GridId, GridRegistry, Pane, PaneKind, GLOBAL_GRID};
use view_core::hl::HlTable;
use view_core::theme::{ChromeGroup, Theme};
use view_surface::overlay::BorderSet;
use view_surface::Rect;

use super::{clip_to_frame, paint_grid, ratatui_style, set_border_cell, Damage};

/// Paints every visible pane and the chrome between them.
///
/// Separator cells are view's own, derived from the engine's highlight
/// table like all other chrome, so a user's colorscheme still decides what
/// the editor looks like.
///
/// `area` is the engine-grid layer's own rect, so a pane's origin -- which
/// nvim states in global-grid coordinates -- is applied inside it and the
/// reserved chrome rows are accounted for exactly once.
pub(super) fn paint_panes(
    registry: &GridRegistry,
    theme: &Theme,
    hl: &HlTable,
    borders: BorderSet,
    area: TermRect,
    damage: &Damage,
    buf: &mut Buffer,
) {
    // the shipped single-grid frame, and every multigrid one before its
    // first window lands: one grid covering the layer, no chrome between
    // windows, and no pane list allocated on the paint path to say so
    if !registry.has_panes() {
        paint_grid(registry.global(), theme, hl, area, damage, buf);
        return;
    }
    let cursor = registry.cursor_grid();
    let mut windows: Vec<TermRect> = Vec::new();
    for pane in registry.panes_in_z_order() {
        let Some(grid) = registry.grid(pane.id) else {
            continue;
        };
        let (width, height) = grid.size();
        let (top, left) = pane.origin;
        let pane_area = clip_to_frame(Rect::new(top, left, width, height), area);
        if pane_area.width == 0 || pane_area.height == 0 {
            continue;
        }
        paint_grid(
            grid,
            &pane_theme(theme, &pane, cursor),
            hl,
            pane_area,
            damage,
            buf,
        );
        if pane.id != GLOBAL_GRID && matches!(pane.kind, PaneKind::Window) {
            windows.push(pane_area);
        }
    }
    paint_separators(&windows, theme, borders, damage, buf);
}

/// The theme one pane's cells resolve through: the caller's, or one whose
/// `Normal` colors are `NormalNC`'s for a window the cursor is not in.
///
/// Substituting the base rather than restyling each cell is what nvim
/// itself does with the group, so a cell that carries a highlight of its
/// own keeps it and only unhighlighted text dims. The global grid is chrome
/// between windows rather than a window, so it is never dimmed; nvim's own
/// message area is exempted the same way, since nvim never treats its own
/// message text as an unfocused window either; and a session that has
/// placed no cursor yet has no inactive pane to name.
fn pane_theme(theme: &Theme, pane: &Pane, cursor: Option<GridId>) -> Theme {
    if pane.id == GLOBAL_GRID
        || matches!(pane.kind, PaneKind::Message { .. })
        || cursor.is_none_or(|id| id == pane.id)
    {
        return *theme;
    }
    let nc = theme.chrome(ChromeGroup::NormalNC);
    let mut inactive = *theme;
    inactive.fg = nc.fg;
    inactive.bg = nc.bg;
    inactive
}

/// Overpaints the column immediately right of each window box, on the rows
/// where a second window sits directly across it.
///
/// Which cells those are is derived from the boxes, not read off the wire:
/// nvim paints its own glyph into that column of the global grid but names
/// no event for where the column *is*, and the pane origins already say.
/// A neighbour across the column is what makes the space a separator -- the
/// same predicate rules out a box against the layer's right edge, one whose
/// neighbour is hidden, and the rows of a split where the window across is
/// only as tall as its own half.
///
/// Runs after every pane so the global grid's own cells are underneath it;
/// what lands is view's glyph and view's style, over nvim's.
fn paint_separators(
    windows: &[TermRect],
    theme: &Theme,
    borders: BorderSet,
    damage: &Damage,
    buf: &mut Buffer,
) {
    let style = ratatui_style(theme.chrome(ChromeGroup::WinSeparator));
    for window in windows {
        let col = window.x.saturating_add(window.width);
        for row in window.y..window.y.saturating_add(window.height) {
            if !damage.covers(row) || !faces_a_neighbour(windows, col, row) {
                continue;
            }
            set_border_cell(buf, col, row, borders.vertical, style);
        }
    }
}

/// Whether a window box opens at the column just past `col` on `row`.
fn faces_a_neighbour(windows: &[TermRect], col: u16, row: u16) -> bool {
    windows
        .iter()
        .any(|w| w.x == col.saturating_add(1) && (w.y..w.y.saturating_add(w.height)).contains(&row))
}

#[cfg(test)]
mod tests;
