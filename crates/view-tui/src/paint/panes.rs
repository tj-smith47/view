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
use view_core::grid::registry::{GridId, Pane, PaneKind, GLOBAL_GRID};
use view_core::model::{Focus, Model, OverlayKind, Panes};
use view_core::native::geometry::NativeSurface;
use view_core::theme::{ChromeGroup, Theme};
use view_surface::overlay::BorderSet;
use view_surface::{Layer, LayerKind, Rect};

use super::{clip_to_frame, paint_grid, ratatui_style, set_border_cell, Damage, PaintScratch};

/// Paints every visible pane and the chrome between them.
///
/// Separator cells are view's own, derived from the engine's highlight
/// table like all other chrome, so a user's colorscheme still decides what
/// the editor looks like.
///
/// `area` is the engine-grid layer's own rect, so a pane's origin -- which
/// nvim states in global-grid coordinates -- is applied inside it and the
/// reserved chrome rows are accounted for exactly once.
///
/// Under tiles the frames take the separators' place at the same boundary,
/// so a float paints over a finished frame and no frame cuts it.
pub(super) fn paint_panes(
    model: &Model,
    theme: &Theme,
    borders: BorderSet,
    area: TermRect,
    damage: &Damage,
    scratch: &mut PaintScratch,
    buf: &mut Buffer,
) {
    let registry = model.engine.painted_grids();
    let hl = model.engine.painted_hl();
    // the shipped single-grid frame, and every multigrid one before its
    // first window lands: one grid covering the layer and no chrome
    // between windows
    if !registry.has_panes() {
        paint_grid(
            registry.global(),
            theme,
            hl,
            area,
            damage,
            &mut scratch.styles,
            buf,
        );
        return;
    }
    // tiles restyle the separator column themselves: gapped paints it as
    // gap, gapless makes it the frame, and neither wants nvim's own glyph
    // put back under it
    let look = registry.look();
    let tiled = look.panes == Panes::Tiles;
    let cursor = registry.cursor_grid();
    let PaintScratch {
        panes,
        styles,
        global,
        ..
    } = scratch;
    let panes = &*panes;
    uncovered_global_damage(registry, panes, area, damage, global);
    let mut windows: Vec<TermRect> = Vec::new();
    let mut separated = false;
    for pane in panes {
        // a separator is a cell of the global grid, so it belongs to the
        // window layer and nothing above it: the pane list puts every
        // window ahead of every float and message grid, and the boundary
        // between the two is where the separators go in. Painted after the
        // windows because their boxes are what say where the column is, and
        // before anything floating because a float's rect owns every cell
        // under it.
        if !separated && !pane.kind.is_window() {
            if tiled {
                frames::paint_frames(
                    model, panes, look, cursor, theme, borders, area, damage, buf,
                );
            } else {
                paint_separators(&windows, theme, borders, damage, buf);
            }
            separated = true;
        }
        let Some(grid) = registry.grid(pane.id) else {
            continue;
        };
        let (top, left, width, height) = pane.text(grid.size());
        let pane_area = clip_to_frame(Rect::new(top, left, width, height), area);
        if pane_area.width == 0 || pane_area.height == 0 {
            continue;
        }
        match pane.kind.native_surface() {
            Some(surface) => {
                paint_native_pane(model, surface, theme, borders, pane_area, damage, buf)
            }
            None => paint_grid(
                grid,
                &pane_theme(theme, pane, cursor),
                hl,
                pane_area,
                if pane.id == GLOBAL_GRID {
                    global
                } else {
                    damage
                },
                styles,
                buf,
            ),
        }
        if pane.id != GLOBAL_GRID && pane.kind.is_window() {
            windows.push(pane_area);
        }
    }
    if !separated {
        if tiled {
            frames::paint_frames(
                model, panes, look, cursor, theme, borders, area, damage, buf,
            );
        } else {
            paint_separators(&windows, theme, borders, damage, buf);
        }
    }
}

/// Replaces `out` with the rows of `damage` the global grid still shows
/// through: every one but those a single window's text spans from the
/// layer's left edge to the global grid's right one.
///
/// Under multigrid a window's text is its own grid's, and whatever grid 1
/// holds beneath it is painted over cell for cell by the window pane that
/// follows. The status rows, the separator column, the command line and a
/// row two side-by-side windows share all stay grid 1's.
fn uncovered_global_damage(
    registry: &view_core::grid::registry::GridRegistry,
    panes: &[Pane],
    area: TermRect,
    damage: &Damage,
    out: &mut Damage,
) {
    let right = area
        .x
        .saturating_add(registry.global().size().0.min(area.width));
    let covered = |row: u16| {
        panes.iter().any(|pane| {
            pane.id != GLOBAL_GRID
                && pane.kind.is_window()
                && pane.kind.native_surface().is_none()
                && registry.grid(pane.id).is_some_and(|grid| {
                    let (top, left, width, height) = pane.text(grid.size());
                    let rect = clip_to_frame(Rect::new(top, left, width, height), area);
                    rect.x <= area.x
                        && rect.x.saturating_add(rect.width) >= right
                        && (rect.y..rect.y.saturating_add(rect.height)).contains(&row)
                })
        })
    };
    out.full = false;
    out.rows.clear();
    let rows = area.y..area.y.saturating_add(area.height);
    if damage.full {
        out.rows.extend(rows.filter(|&row| !covered(row)));
    } else {
        out.rows.extend(
            damage
                .rows
                .iter()
                .copied()
                .filter(|&row| rows.contains(&row) && !covered(row)),
        );
    }
}

/// Every windowed surface's pane rect and what it draws there, in the order
/// [`paint_panes`] paints them, for the engine-grid layer at `area`.
///
/// A surface's rows come from view's own state, so nvim sends no cell
/// change when they move: comparing this list across frames is the only
/// record of which rows a tree scan or a selection step repainted.
pub(super) fn native_panes(
    model: &Model,
    panes: &[Pane],
    area: TermRect,
) -> Vec<(TermRect, LayerKind)> {
    let registry = model.engine.painted_grids();
    if !registry.has_panes() {
        return Vec::new();
    }
    panes
        .iter()
        .filter_map(|pane| {
            let surface = pane.kind.native_surface()?;
            let (top, left, width, height) = pane.text(registry.grid(pane.id)?.size());
            let rect = clip_to_frame(Rect::new(top, left, width, height), area);
            if rect.width == 0 || rect.height == 0 {
                return None;
            }
            Some((
                rect,
                native_pane_content(model, surface, rect.height, rect.width)?,
            ))
        })
        .collect()
}

/// Paints one of view's own surfaces into the pane nvim laid out for it.
///
/// Unframed: the tile's frame is already drawn around this rect, and a
/// border of the surface's own inside it would be two boxes where a person
/// sees one.
fn paint_native_pane(
    model: &Model,
    surface: NativeSurface,
    theme: &Theme,
    borders: BorderSet,
    pane_area: TermRect,
    damage: &Damage,
    buf: &mut Buffer,
) {
    let Some(kind) = native_pane_content(model, surface, pane_area.height, pane_area.width) else {
        return;
    };
    let layer = Layer::new(
        Rect::new(pane_area.y, pane_area.x, pane_area.width, pane_area.height),
        kind,
        model.caps,
    );
    let laid = view_surface::overlay::unframed_rows(
        pane_area.width,
        pane_area.height,
        &layer.kind,
        borders,
    );
    super::paint_native_overlay(
        &layer,
        Some(&laid),
        theme,
        model.engine.painted_hl(),
        pane_area,
        damage,
        buf,
    );
}

/// What a windowed surface draws, or `None` while its state is not open.
///
/// `height`/`width` are the pane's own resolved cell size, spent only by the
/// agent panel: its transcript window and composer wrap derive from the
/// room the tile actually has, the same numbers the floating placement's
/// `layer_kind` (`view-surface/src/lib.rs`) draws it at.
fn native_pane_content(
    model: &Model,
    surface: NativeSurface,
    height: u16,
    width: u16,
) -> Option<LayerKind> {
    match surface {
        NativeSurface::Tree => model
            .overlays()
            .iter()
            .find_map(|overlay| match &overlay.kind {
                OverlayKind::Tree(state) => Some(LayerKind::Tree(
                    state.view().with_icons(model.tree_icons_shown()),
                )),
                _ => None,
            }),
        NativeSurface::Agent => model.overlays().iter().find_map(|overlay| {
            matches!(overlay.kind, OverlayKind::Ai).then(|| {
                // the floating placement's `focused` never sets for this
                // placement (nvim's own cursor move is what "entered"
                // means here, see `AiPanelState::focused`'s doc), so the
                // hint rows read the placement's own keyboard-holder; the
                // field stays false for it
                let has_keyboard = model.focus() == Focus::Pane(NativeSurface::Agent);
                LayerKind::Ai(model.ai_panel().view(
                    usize::from(height),
                    usize::from(width),
                    has_keyboard,
                ))
            })
        }),
        // The palette carries no window of nvim's own, so no
        // pane this compositor iterates is ever `NativeSurface::Palette`
        // (`view-surface::render` paints the windowed tile itself, through
        // the same `LayerKind::Palette` push the centred placement uses).
        // left/right (a tall, narrow stream) and top/bottom (a short, wide
        // ticker) are the same entry list at two aspect ratios -- the split
        // direction nvim opened the window with already decided which one
        // `pane_area` is, and `view_for_width` reads the room this call was
        // actually handed to shorten the stamp in a narrow tile of either
        // shape
        NativeSurface::Notifications => model.overlays().iter().find_map(|overlay| match &overlay
            .kind
        {
            OverlayKind::MessageHistory(state) => Some(LayerKind::Stream(
                state.view_for_width(width, model.utc_offset_secs()),
            )),
            _ => None,
        }),
        // `NativeSurface` is `#[non_exhaustive]`: every variant this crate
        // knows about is matched above, so a future one paints nothing
        // until its own arm lands here.
        _ => None,
    }
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
/// Runs after the window panes so the global grid's own cells are
/// underneath it -- what lands is view's glyph and view's style, over
/// nvim's -- and before the floating ones, which own every cell their rect
/// covers.
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

pub(crate) mod frames;

#[cfg(test)]
mod tests;
