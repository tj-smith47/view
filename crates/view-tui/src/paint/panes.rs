//! The pane compositor: every visible grid painted where nvim placed it,
//! and the chrome view draws in the space left between them.
//!
//! Under `ext_multigrid` a window's text arrives in a grid of its own and
//! the picture on screen is something view builds rather than something
//! nvim sent. The column between two side-by-side windows is what that
//! leaves over, and it is view's to draw: the separator here resolves
//! through [`ChromeGroup::WinSeparator`] and takes its glyph from the
//! probed box-drawing charset, so a user's colorscheme still decides what
//! the space between windows looks like.
//!
//! Single-grid sessions come through here unchanged: nvim places no window
//! of its own, the registry answers one pane -- the global grid at the
//! origin -- and the loop below reduces to the one [`super::paint_grid`]
//! call it always made.

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
/// between windows rather than a window, so it is never dimmed, and a
/// session that has placed no cursor yet has no inactive pane to name.
fn pane_theme(theme: &Theme, pane: &Pane, cursor: Option<GridId>) -> Theme {
    if pane.id == GLOBAL_GRID || cursor.is_none_or(|id| id == pane.id) {
        return *theme;
    }
    let nc = theme.chrome(ChromeGroup::NormalNC);
    let mut inactive = *theme;
    inactive.fg = nc.fg;
    inactive.bg = nc.bg;
    inactive
}

/// Draws the column immediately right of each window box, on the rows where
/// a second window sits directly across it.
///
/// Derived from the boxes rather than read off a wire event: nvim announces
/// where each window sits and nothing at all about the space between them.
/// A neighbour across the column is what makes that space a separator --
/// the same predicate rules out a box against the layer's right edge, one
/// whose neighbour is hidden, and the rows of a split where the window
/// across is only as tall as its own half.
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
