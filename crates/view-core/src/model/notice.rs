//! Where view stacks its own notices: toasts, sticky notices and the
//! engine's wedge banner, in one column the toast painter and the float
//! detector both read.
//!
//! The column keeps clear of every frame line and every windowed surface,
//! and the stack moves off the cursor's row when the other end of the
//! column has room, so a notice leaves the text a person is working on in
//! view.

use super::{Model, Panes, TileKind};
use crate::grid::registry::{PaneKind, GLOBAL_GRID};
use crate::native::geometry::{Anchor, NativeSurface};

/// The widest the notice column gets, in cells. A notice longer than a
/// line of this width wraps inside it.
pub const NOTICE_COLUMN_MAX: u16 = 60;

/// A rect in grid cells, as `(row, col, width, height)`.
type Cells = (u16, u16, u16, u16);

/// Where view stacks its own notices this frame, in grid coordinates.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoticeColumn {
    /// The column, as `(row, col, width, height)`.
    pub rect: (u16, u16, u16, u16),
    /// Whether the stack grows down from the column's top row, once the
    /// cursor row has had its say.
    pub from_top: bool,
    /// Whether the boxes sit against the column's left edge, which is also
    /// the edge a dismissed box slides out through.
    pub left_edge: bool,
}

impl Model {
    /// The corner `[ui.surfaces.notifications] anchor` names for the stack.
    ///
    /// The same key places the windowed notification stream, where an edge
    /// (`bottom`, `left`) is a valid answer; a stack needs a corner to grow
    /// from, so an edge answers the top-right corner.
    #[must_use]
    pub fn notice_anchor(&self) -> Anchor {
        let anchor = self.surfaces.layout(NativeSurface::Notifications).anchor;
        if anchor.is_corner() {
            anchor
        } else {
            Anchor::TopRight
        }
    }

    /// Where view stacks its own notices, in grid coordinates.
    ///
    /// The column sits in the anchor's corner of the grid left over once
    /// every windowed surface's pane, every sidebar window and every side
    /// panel drawn over the grid are taken out. Where they leave no room
    /// for a box, it sits in the grid's own corner. Under tiles it stays
    /// inside the inner rect of the tile in that corner, so no box lands on
    /// a frame line, and a float of a plugin's that overlaps it pushes the
    /// stack past its rows. When the boxes stacked from the anchor's end
    /// would cover the cursor's row in the focused window, and stacking
    /// them from the other end would not, they stack from the other end.
    #[must_use]
    pub fn notice_column(&self) -> NoticeColumn {
        let anchor = self.notice_anchor();
        let rect = self.notice_rect();
        let anchored = anchor.is_top_corner();
        let stack = self.stack_height(rect);
        let from_top = if self.covers_cursor_row(rect, stack, anchored)
            && !self.covers_cursor_row(rect, stack, !anchored)
        {
            !anchored
        } else {
            anchored
        };
        NoticeColumn {
            rect,
            from_top,
            left_edge: anchor.is_left_corner(),
        }
    }

    /// The rows the toast stack is laid out in: the notice column's height,
    /// floored at one framed box so a terminal too small to hold one still
    /// shows the newest notice. Read by the renderer that lays the boxes
    /// out and by `update`'s own read of which notice the budget is
    /// showing, so the model animates only the boxes the frame drew.
    #[must_use]
    pub fn toast_rows(&self) -> usize {
        usize::from(self.notice_rect().3).max(3)
    }

    /// Where the armed toast sits among the boxes the column is showing.
    /// Skips the geometry outright on an empty stack, which is every
    /// message on a session with nothing up.
    pub(crate) fn armed_toast_slot(&self) -> Option<usize> {
        if self.engine.messages.entries.is_empty() {
            return None;
        }
        let rect = self.notice_rect();
        self.engine
            .messages
            .armed_visible_slot(usize::from(rect.3).max(3), rect.2)
    }

    /// The column before any plugin float is taken out of it.
    ///
    /// The float detector reads this one: a float that overlaps the column
    /// is the thing it reports, and the column with that float already
    /// taken out overlaps nothing.
    #[must_use]
    pub fn notice_bounds(&self) -> (u16, u16, u16, u16) {
        let anchor = self.notice_anchor();
        let (top, left) = (anchor.is_top_corner(), anchor.is_left_corner());
        let (grid_w, grid_h) = self.engine.grid().size();
        let panes = self.engine.grids().panes_in_z_order();
        let sidebar = |id| {
            self.engine
                .grids()
                .window_handle(id)
                .and_then(|win| self.window_status.get(&win))
                .is_some_and(|status| matches!(status.kind, TileKind::Sidebar { .. }))
        };
        let grid = (0, 0, grid_w, grid_h);
        let area = panes
            .iter()
            .filter(|pane| matches!(pane.kind, PaneKind::Native { .. }) || sidebar(pane.id))
            .map(|pane| pane.slot)
            .chain(self.side_panels())
            .fold(grid, cut);
        // a side panel or windowed surface that fills the grid leaves no
        // room for a box, and the grid's own corner is the one left to use
        let area = if area.2 < 3 || area.3 < 3 { grid } else { area };
        let corner = (
            if top { area.0 } else { far(area.0, area.3) },
            if left { area.1 } else { far(area.1, area.2) },
        );
        let (inset_rows, inset_cols) = self.look.inset();
        let tile = (self.look.panes == Panes::Tiles)
            .then(|| {
                panes
                    .iter()
                    .filter(|pane| {
                        pane.id != GLOBAL_GRID && pane.kind == PaneKind::Window && !sidebar(pane.id)
                    })
                    // a tile runs on under a side panel drawn over it, so
                    // only the part of it the area keeps is the tile's
                    .map(|pane| within(shrink(pane.slot, inset_rows, inset_cols), area))
                    .filter(|kept| kept.2 > 0 && kept.3 > 0)
                    .min_by_key(|&(row, col, width, height)| {
                        let at = (
                            if top { row } else { far(row, height) },
                            if left { col } else { far(col, width) },
                        );
                        corner.0.abs_diff(at.0) + corner.1.abs_diff(at.1)
                    })
            })
            .flatten();
        let base = tile.unwrap_or_else(|| shrink(area, inset_rows, inset_cols));
        let width = NOTICE_COLUMN_MAX.min((area.2 / 2).max(1)).min(base.2);
        let col = if left {
            base.1
        } else {
            base.1.saturating_add(base.2).saturating_sub(width)
        };
        (base.0, col, width, base.3)
    }

    /// Every open overlay pinned to an edge, the tree and the agent panel
    /// among them, in grid coordinates. Each one covers the tiles under it,
    /// so a box stacked there would sit over the panel.
    fn side_panels(&self) -> impl Iterator<Item = Cells> + '_ {
        let offset = self.look.grid_offset();
        let top = self.chrome_rows().saturating_add(offset);
        self.overlays()
            .iter()
            .filter(|overlay| overlay.geometry.anchor != Anchor::Center)
            .map(move |overlay| {
                let rect = self.overlay_rect(overlay);
                (
                    rect.row.saturating_sub(top),
                    rect.col.saturating_sub(offset),
                    rect.width,
                    rect.height,
                )
            })
    }

    /// [`Self::notice_bounds`] with every visible float that overlaps it
    /// taken off the end it lies nearer.
    fn notice_rect(&self) -> Cells {
        let (row, col, width, height) = self.notice_bounds();
        let right = col.saturating_add(width);
        let mut top = row;
        let mut bottom = row.saturating_add(height);
        for pane in self.engine.grids().panes_in_z_order() {
            if !matches!(pane.kind, PaneKind::Float { .. }) {
                continue;
            }
            let (f_row, f_col, f_width, f_height) = pane.slot;
            let f_bottom = f_row.saturating_add(f_height);
            if f_width == 0
                || f_height == 0
                || f_row >= bottom
                || f_bottom <= top
                || f_col >= right
                || f_col.saturating_add(f_width) <= col
            {
                continue;
            }
            if u32::from(f_row) + u32::from(f_bottom) < u32::from(top) + u32::from(bottom) {
                top = top.max(f_bottom);
            } else {
                bottom = bottom.min(f_row);
            }
        }
        (top, col, width, bottom.saturating_sub(top))
    }

    /// The rows the boxes showing in `rect` take together, frames included.
    fn stack_height(&self, rect: Cells) -> u16 {
        let rows: usize = self
            .engine
            .messages
            .visible_toasts_in(usize::from(rect.3).max(3), rect.2)
            .iter()
            .map(|lines| lines.len().saturating_add(2))
            .sum();
        u16::try_from(rows).unwrap_or(u16::MAX).min(rect.3)
    }

    /// Whether `stack` rows stacked from the top (or the bottom) of `rect`
    /// cover the cursor's row inside the focused window.
    fn covers_cursor_row(&self, rect: Cells, stack: u16, from_top: bool) -> bool {
        if stack == 0 {
            return false;
        }
        let grids = self.engine.grids();
        let (cursor_row, _) = grids.cursor_pos();
        let (focused, ..) = grids.cursor_local();
        let window = grids
            .panes_in_z_order()
            .into_iter()
            .find(|pane| pane.id == focused)
            .map(|pane| pane.slot);
        let Some(window) = window else {
            return false;
        };
        let band = if from_top {
            (rect.0, rect.1, rect.2, stack)
        } else {
            (
                rect.0.saturating_add(rect.3).saturating_sub(stack),
                rect.1,
                rect.2,
                stack,
            )
        };
        overlaps(band, (cursor_row, window.1, window.2, 1))
    }
}

/// The last cell of a run `len` long starting at `start`.
const fn far(start: u16, len: u16) -> u16 {
    start.saturating_add(len).saturating_sub(1)
}

/// Whether two rects share a cell.
fn overlaps(a: Cells, b: Cells) -> bool {
    a.2 > 0
        && a.3 > 0
        && b.2 > 0
        && b.3 > 0
        && a.0 < b.0.saturating_add(b.3)
        && b.0 < a.0.saturating_add(a.3)
        && a.1 < b.1.saturating_add(b.2)
        && b.1 < a.1.saturating_add(a.2)
}

/// The cells `rect` and `area` share, empty where they share none.
fn within(rect: Cells, area: Cells) -> Cells {
    let row = rect.0.max(area.0);
    let col = rect.1.max(area.1);
    let bottom = far(rect.0, rect.3)
        .min(far(area.0, area.3))
        .saturating_add(1);
    let right = far(rect.1, rect.2)
        .min(far(area.1, area.2))
        .saturating_add(1);
    (
        row,
        col,
        right.saturating_sub(col),
        bottom.saturating_sub(row),
    )
}

/// `rect` less `rows` on the top and bottom and `cols` on either side.
fn shrink(rect: Cells, rows: u16, cols: u16) -> Cells {
    (
        rect.0.saturating_add(rows),
        rect.1.saturating_add(cols),
        rect.2.saturating_sub(cols.saturating_mul(2)),
        rect.3.saturating_sub(rows.saturating_mul(2)),
    )
}

/// The largest of the four bands of `area` lying wholly to one side of
/// `pane`: what is left once a windowed surface's pane is taken out,
/// whichever edge it was opened against. nvim keeps a status row under a
/// pane, which leaves it one row short of the edge it stands on, so the
/// bands are judged by their size.
fn cut(area: Cells, pane: Cells) -> Cells {
    if !overlaps(area, pane) {
        return area;
    }
    let (row, col, width, height) = area;
    let (bottom, right) = (row.saturating_add(height), col.saturating_add(width));
    let (p_row, p_col, p_width, p_height) = pane;
    let (p_bottom, p_right) = (
        p_row.saturating_add(p_height).min(bottom),
        p_col.saturating_add(p_width).min(right),
    );
    [
        (row, col, p_col.saturating_sub(col), height),
        (row, p_right, right.saturating_sub(p_right), height),
        (row, col, width, p_row.saturating_sub(row)),
        (p_bottom, col, width, bottom.saturating_sub(p_bottom)),
    ]
    .into_iter()
    .max_by_key(|cells| u32::from(cells.2) * u32::from(cells.3))
    .unwrap_or(area)
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::events::{UiEvent, WinHandle};
    use crate::grid::registry::GridId;
    use crate::model::{Look, OverlayKind, WindowStatus, MIN_FRAMED_SLOT};
    use crate::msg::Msg;
    use crate::native::geometry::OverlayBox;
    use crate::native::geometry::SurfaceLayout;
    use crate::native::tree::TreeState;
    use crate::update::update;

    const CORNERS: [Anchor; 4] = [
        Anchor::TopLeft,
        Anchor::TopRight,
        Anchor::BottomLeft,
        Anchor::BottomRight,
    ];

    /// A laid-out session and what the walk needs to know about it.
    pub(crate) struct Scene {
        pub(crate) model: Model,
        /// Each tile's grid, left to right.
        pub(crate) tiles: Vec<u64>,
        /// Every placed window's slot, tiles and surfaces alike.
        pub(crate) slots: Vec<(u16, u16, u16, u16)>,
        /// The slots of the windowed surfaces.
        pub(crate) natives: Vec<(u16, u16, u16, u16)>,
    }

    /// One session on a `size` terminal under `look`: `tiles` vsplits side
    /// by side, a windowed tree on the left, agent on the right and
    /// notification stream under the tiles where `natives` has bits 0, 1
    /// and 2 set. Every window keeps its status row under it, as the
    /// `laststatus = 2` hold leaves it. `None` where a window would be
    /// narrower or shorter than a framed slot.
    pub(crate) fn scene(size: (u16, u16), look: Look, natives: u8, tiles: u16) -> Option<Scene> {
        let mut model = Model::with_term_size(size.0, size.1).with_look(look);
        let (grid_w, grid_h) = model.grid_target();
        let full = grid_h.checked_sub(1)?;
        let panel = |total: u16| (total * 3 / 10).max(3);
        let mut placed: Vec<(u64, Cells, Option<NativeSurface>)> = Vec::new();
        let mut left = 0;
        let mut right = grid_w;
        if natives & 1 != 0 {
            let width = panel(grid_w);
            placed.push((20, (0, 0, width, full), Some(NativeSurface::Tree)));
            left = width + 1;
        }
        if natives & 2 != 0 {
            let width = panel(grid_w);
            placed.push((
                21,
                (0, grid_w.checked_sub(width)?, width, full),
                Some(NativeSurface::Agent),
            ));
            right = grid_w.checked_sub(width + 1)?;
        }
        let middle = right.checked_sub(left)?;
        let mut tile_h = full;
        if natives & 4 != 0 {
            let height = (grid_h / 4).max(3);
            let row = full.checked_sub(height)?;
            placed.push((
                22,
                (row, left, middle, height),
                Some(NativeSurface::Notifications),
            ));
            tile_h = row.checked_sub(1)?;
        }
        let tile_w = middle.checked_sub(tiles - 1)? / tiles;
        if tile_w < MIN_FRAMED_SLOT.0 || tile_h < MIN_FRAMED_SLOT.1 {
            return None;
        }
        let mut grids = Vec::new();
        for index in 0..tiles {
            let col = left + index * (tile_w + 1);
            let width = if index + 1 == tiles {
                right - col
            } else {
                tile_w
            };
            let grid = 2 + u64::from(index);
            grids.push(grid);
            placed.push((grid, (0, col, width, tile_h), None));
        }
        let mut events = vec![UiEvent::GridResize {
            grid: 1,
            width: u64::from(grid_w),
            height: u64::from(grid_h),
        }];
        for (grid, (row, col, width, height), surface) in &placed {
            if let Some(surface) = surface {
                model
                    .engine
                    .grids_mut()
                    .claim_native_window(WinHandle(1000 + grid), *surface);
            }
            let inner = match look.inner_request((*width, *height), 0) {
                (0, 0) => (*width, *height),
                inner => inner,
            };
            events.push(UiEvent::GridResize {
                grid: *grid,
                width: u64::from(inner.0),
                height: u64::from(inner.1),
            });
            events.push(UiEvent::WinPos {
                grid: *grid,
                win: WinHandle(1000 + grid),
                startrow: u64::from(*row),
                startcol: u64::from(*col),
                width: u64::from(*width),
                height: u64::from(*height),
            });
        }
        let _ = update(&mut model, Msg::Redraw(events));
        // past the launch: messages reach the stack, and a float over the
        // stack paints at once
        let _ = model
            .engine
            .messages
            .resolve_startup_hold(crate::native::toast::HoldOutcome::Release);
        model.surface_conflicts.note_user_acted();
        Some(Scene {
            model,
            tiles: grids,
            slots: placed.iter().map(|(_, slot, _)| *slot).collect(),
            natives: placed
                .iter()
                .filter(|(.., surface)| surface.is_some())
                .map(|(_, slot, _)| *slot)
                .collect(),
        })
    }

    /// Puts the cursor in `grid` at `row` of its own grid.
    pub(crate) fn focus(model: &mut Model, grid: u64, row: u16) {
        let _ = update(
            model,
            Msg::Redraw(vec![
                UiEvent::GridCursorGoto {
                    grid,
                    row: u64::from(row),
                    col: 0,
                },
                UiEvent::Flush,
            ]),
        );
    }

    /// Points the stack at `anchor`.
    pub(crate) fn anchor_at(model: &mut Model, anchor: Anchor) {
        let layout = model.surfaces.layout(NativeSurface::Notifications);
        model.surfaces.set_layout(
            NativeSurface::Notifications,
            SurfaceLayout::new(layout.placement, anchor, layout.size),
        );
    }

    /// Every layout the column is judged on: terminal widths 40 to 240 in
    /// steps of 7, heights 10 to 60 in steps of 5, both looks with and
    /// without gaps, every corner, each windowed surface on and off, one to
    /// three vsplits, and the cursor in every tile. `each` gets the scene
    /// and a label naming it.
    pub(crate) fn walk(mut each: impl FnMut(&mut Scene, &str)) {
        for width in (40..=240).step_by(7) {
            for height in (10..=60).step_by(5) {
                for panes in [Panes::Tiles, Panes::Nvim] {
                    for gaps in [true, false] {
                        for natives in 0..8 {
                            for tiles in 1..=3 {
                                let look = Look::new(panes, gaps);
                                let Some(mut scene) = scene((width, height), look, natives, tiles)
                                else {
                                    continue;
                                };
                                for anchor in CORNERS {
                                    anchor_at(&mut scene.model, anchor);
                                    for index in 0..scene.tiles.len() {
                                        let grid = scene.tiles[index];
                                        focus(&mut scene.model, grid, 0);
                                        let label = format!(
                                            "{width}x{height} {panes:?} gaps={gaps} \
                                             natives={natives:03b} tiles={tiles} \
                                             {anchor:?} focus={index}"
                                        );
                                        each(&mut scene, &label);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// The four one-cell-thick edges of `frame`.
    fn edges(frame: Cells) -> [Cells; 4] {
        let (row, col, width, height) = frame;
        [
            (row, col, width, 1),
            (far(row, height), col, width, 1),
            (row, col, 1, height),
            (row, far(col, width), 1, height),
        ]
    }

    #[test]
    fn the_notice_column_never_covers_a_frame_a_windowed_surface_or_the_focused_frame() {
        let mut scenes = 0;
        walk(|scene, label| {
            scenes += 1;
            let model = &scene.model;
            let column = model.notice_column().rect;
            assert!(
                column.2 > 0 && column.3 > 0,
                "{label}: the column is empty: {column:?}"
            );
            assert!(column.2 <= NOTICE_COLUMN_MAX, "{label}: {column:?}");
            for slot in &scene.natives {
                assert!(
                    !overlaps(column, *slot),
                    "{label}: the column {column:?} covers the windowed surface at {slot:?}"
                );
            }
            for slot in &scene.slots {
                if let Some(frame) = model.look.frame_box(*slot) {
                    for edge in edges(frame) {
                        assert!(
                            !overlaps(column, edge),
                            "{label}: the column {column:?} covers the frame of {slot:?}"
                        );
                    }
                }
            }
            let (focused, ..) = model.engine.grids().cursor_local();
            let focused = model
                .engine
                .grids()
                .panes_in_z_order()
                .into_iter()
                .find(|pane| pane.id == focused)
                .expect("the cursor is in a placed tile");
            if model.look.panes == Panes::Tiles {
                // a gapless frame is the lattice around the slot, and the
                // lattice at the grid's own edge is the ring outside it
                let (row, col, width, height) = focused.slot;
                let frame = match model.look.frame_box(focused.slot) {
                    Some(frame) => edges(frame).to_vec(),
                    None => [
                        row.checked_sub(1).map(|up| (up, col, width, 1)),
                        Some((row + height, col, width, 1)),
                        col.checked_sub(1).map(|left| (row, left, 1, height)),
                        Some((row, col + width, 1, height)),
                    ]
                    .into_iter()
                    .flatten()
                    .collect(),
                };
                for edge in frame {
                    assert!(
                        !overlaps(column, edge),
                        "{label}: the column {column:?} covers the focused frame at {edge:?}"
                    );
                }
            }
        });
        assert!(scenes > 10_000, "the walk reached only {scenes} scenes");
    }

    /// Where a panel of `kind` sits in terminal cells once it is opened on
    /// `scene`, for each way a surface can be drawn beside the tiles.
    fn open_panel(scene: &mut Scene, kind: usize) -> Option<Cells> {
        let model = &mut scene.model;
        let overlay = |surface: NativeSurface, anchor: Anchor| {
            let geometry = OverlayBox::new(30, 100).with_anchor(anchor);
            let kind = match surface {
                NativeSurface::Tree => OverlayKind::Tree(TreeState::open(".".into())),
                _ => OverlayKind::Ai,
            };
            (geometry, kind)
        };
        let (geometry, kind) = match kind {
            0 => overlay(NativeSurface::Tree, Anchor::Left),
            1 => overlay(NativeSurface::Tree, Anchor::Right),
            2 => overlay(NativeSurface::Agent, Anchor::Right),
            3 => overlay(NativeSurface::Agent, Anchor::Left),
            _ => {
                // a plugin's tree in the leftmost tile, which is the only
                // window a single tile leaves
                if scene.tiles.len() < 2 {
                    return None;
                }
                let grid = scene.tiles.remove(0);
                model.window_status.insert(
                    WinHandle(1000 + grid),
                    WindowStatus {
                        kind: TileKind::Sidebar {
                            filetype: "NvimTree".into(),
                        },
                        ..WindowStatus::default()
                    },
                );
                let origin = model.chrome_rows() + model.look.grid_offset();
                let (row, col, width, height) = scene.slots[0];
                return Some((row + origin, col + model.look.grid_offset(), width, height));
            }
        };
        model.push_overlay(geometry, kind);
        let panel = model.overlays().last()?;
        let rect = model.overlay_rect(panel);
        Some((rect.row, rect.col, rect.width, rect.height))
    }

    /// The tree and the agent panel drawn over the tiles at either edge,
    /// and a plugin's sidebar tile, under tiles with and without gaps,
    /// every corner and the cursor in every other tile.
    #[test]
    fn the_notice_column_never_intersects_a_side_panel_or_a_sidebar_tile() {
        let mut scenes = 0;
        for width in (40..=240).step_by(13) {
            for height in (10..=60).step_by(5) {
                for gaps in [true, false] {
                    for tiles in 1..=3 {
                        for kind in 0..5 {
                            let look = Look::new(Panes::Tiles, gaps);
                            let Some(mut scene) = scene((width, height), look, 0, tiles) else {
                                continue;
                            };
                            let Some(panel) = open_panel(&mut scene, kind) else {
                                continue;
                            };
                            for anchor in CORNERS {
                                anchor_at(&mut scene.model, anchor);
                                for index in 0..scene.tiles.len() {
                                    focus(&mut scene.model, scene.tiles[index], 0);
                                    scenes += 1;
                                    let model = &scene.model;
                                    let (row, col, w, h) = model.notice_column().rect;
                                    let origin = model.chrome_rows() + model.look.grid_offset();
                                    let column =
                                        (row + origin, col + model.look.grid_offset(), w, h);
                                    assert!(
                                        w > 0 && h > 0,
                                        "{width}x{height} gaps={gaps} tiles={tiles} \
                                         panel={kind} {anchor:?}: the column is empty"
                                    );
                                    assert!(
                                        !overlaps(column, panel),
                                        "{width}x{height} gaps={gaps} tiles={tiles} \
                                         panel={kind} {anchor:?} focus={index}: the column \
                                         {column:?} covers the panel at {panel:?}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        assert!(scenes > 5_000, "the walk reached only {scenes} scenes");
    }

    /// The agent panel windowed into the only window there is: nothing is
    /// left beside it, and the stack takes the grid's own corner.
    #[test]
    fn a_panel_filling_the_grid_leaves_the_column_in_the_grids_corner() {
        for panes in [Panes::Tiles, Panes::Nvim] {
            let look = Look::new(panes, true);
            let mut model = Model::with_term_size(80, 24).with_look(look);
            let (grid_w, grid_h) = model.grid_target();
            model
                .engine
                .grids_mut()
                .claim_native_window(WinHandle(1021), NativeSurface::Agent);
            let _ = update(
                &mut model,
                Msg::Redraw(vec![
                    UiEvent::GridResize {
                        grid: 1,
                        width: u64::from(grid_w),
                        height: u64::from(grid_h),
                    },
                    UiEvent::GridResize {
                        grid: 21,
                        width: u64::from(grid_w),
                        height: u64::from(grid_h - 1),
                    },
                    UiEvent::WinPos {
                        grid: 21,
                        win: WinHandle(1021),
                        startrow: 0,
                        startcol: 0,
                        width: u64::from(grid_w),
                        height: u64::from(grid_h - 1),
                    },
                    UiEvent::Flush,
                ]),
            );
            let (inset_rows, inset_cols) = model.look.inset();
            let (row, col, width, height) = model.notice_column().rect;
            assert!(width > 0 && height > 0, "{panes:?}: the column is empty");
            assert_eq!(row, inset_rows, "{panes:?}: the column starts at the top");
            assert_eq!(
                col + width,
                grid_w - inset_cols,
                "{panes:?}: the column ends at the grid's right edge"
            );
        }
    }

    /// Three notices in the column of the focused tile, and the cursor on
    /// every row of it in turn, at both vertical anchors.
    #[test]
    fn the_stack_flips_off_the_cursor_row() {
        let look = Look::new(Panes::Tiles, true);
        for anchor in [Anchor::TopRight, Anchor::BottomRight] {
            let mut scene = scene((80, 24), look, 0, 2).expect("an 80x24 vsplit");
            anchor_at(&mut scene.model, anchor);
            for text in ["saved", "2 matches", "linted"] {
                scene.model.engine.messages.push(
                    "echomsg".to_string(),
                    vec![(0, text.into())],
                    false,
                );
            }
            let right = scene.tiles[1];
            let (inner_w, inner_h) = scene
                .model
                .engine
                .grids()
                .grid(GridId(right))
                .unwrap()
                .size();
            assert!(inner_w > 0);
            let mut flipped = 0;
            for row in 0..inner_h {
                focus(&mut scene.model, right, row);
                let (cursor_row, _) = scene.model.engine.grids().cursor_pos();
                let column = scene.model.notice_column();
                let (top, _, _, height) = column.rect;
                let stack = scene.model.stack_height(column.rect);
                let band = |from_top: bool| {
                    if from_top {
                        top..top + stack
                    } else {
                        top + height - stack..top + height
                    }
                };
                if band(true).contains(&cursor_row) && band(false).contains(&cursor_row) {
                    continue;
                }
                assert!(
                    !band(column.from_top).contains(&cursor_row),
                    "{anchor:?}: a box covers the cursor on row {cursor_row} with room at the other end"
                );
                flipped += usize::from(column.from_top != anchor.is_top_corner());
            }
            assert!(
                flipped > 0,
                "{anchor:?}: the stack never moved off the cursor"
            );

            // the cursor in the other tile is under no box, so nothing flips
            for row in 0..inner_h {
                focus(&mut scene.model, scene.tiles[0], row);
                assert_eq!(
                    scene.model.notice_column().from_top,
                    anchor.is_top_corner(),
                    "{anchor:?}: row {row} of the unfocused-column tile flipped the stack"
                );
            }
        }
    }

    /// A plugin float over the top six rows of the column: the stack
    /// starts on the row under it, and the float itself stays where the
    /// plugin put it.
    #[test]
    fn a_foreign_float_in_the_column_pushes_the_stack_past_it() {
        let look = Look::new(Panes::Tiles, true);
        let mut scene = scene((80, 24), look, 0, 2).expect("an 80x24 vsplit");
        scene
            .model
            .engine
            .messages
            .push("echomsg".to_string(), vec![(0, "saved".into())], false);
        let before = scene.model.notice_column();
        let (top, col, ..) = before.rect;
        assert_eq!(top, 1, "the column starts inside the ring");
        let _ = update(
            &mut scene.model,
            Msg::Redraw(vec![
                UiEvent::GridResize {
                    grid: 30,
                    width: 10,
                    height: 6,
                },
                UiEvent::WinFloatPos {
                    grid: 30,
                    win: WinHandle(1030),
                    anchor_grid: 1,
                    zindex: 50,
                    compindex: 1,
                    screen_row: u64::from(top),
                    screen_col: u64::from(col),
                },
                UiEvent::Flush,
            ]),
        );
        let float = scene
            .model
            .engine
            .grids()
            .panes_in_z_order()
            .into_iter()
            .find(|pane| pane.id == GridId(30))
            .expect("the float is placed")
            .slot;
        assert_eq!((float.0, float.3), (1, 6), "the float covers rows 1 to 6");
        let after = scene.model.notice_column();
        assert_eq!(after.rect.0, 7, "the first box starts under the float");
        assert!(after.from_top);
        assert_eq!(
            after.rect.0 + after.rect.3,
            before.rect.0 + before.rect.3,
            "the far end of the column stays"
        );
        assert_eq!(scene.model.notice_bounds(), before.rect);
    }
}
