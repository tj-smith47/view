//! Where view stacks its own notices: toasts, sticky notices and the
//! engine's wedge banner, in one column the toast painter and the float
//! detector both read.
//!
//! The column keeps clear of every frame line and every windowed surface,
//! and the stack moves off the cursor's row when the other end of the
//! column has room, so a notice leaves the text a person is working on in
//! view.

use super::{Model, Panes, TileKind};
use crate::grid::registry::{GridId, PaneKind, GLOBAL_GRID};
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
    /// stack past its rows.
    ///
    /// The stack keeps clear of the cursor's row in the focused window and,
    /// while a review is open in it, of the hunk under review from its
    /// header to its last added line. It stays at the end it was drawn from
    /// until that region enters it, then moves to the other end. When both
    /// ends would cover the region, the column shrinks to the larger side
    /// of it and the notices that no longer fit wait in the history.
    ///
    /// The end the stack holds is kept by `update`, and the rest is read
    /// off the layout of the moment.
    #[must_use]
    pub fn notice_column(&self) -> NoticeColumn {
        let anchor = self.notice_anchor();
        let held = self
            .notice_held
            .filter(|(corner, _)| *corner == anchor)
            .map(|(_, from_top)| from_top);
        self.place_column(held)
    }

    /// Records the end the stack is drawn from this update, and wraps every
    /// notice to the column's width. A session with nothing on the stack
    /// records nothing, so the next notice starts from the anchor.
    pub(crate) fn place_notices(&mut self) {
        if self.engine.messages.entries.is_empty() && self.toast_motion.is_none() {
            self.notice_held = None;
            return;
        }
        let column = self.notice_column();
        self.notice_held = Some((self.notice_anchor(), column.from_top));
        self.engine.messages.rewrap(column.rect.2);
    }

    /// Where the armed toast sits among the boxes the column is showing.
    /// Skips the geometry outright on an empty stack, which is every
    /// message on a session with nothing up.
    pub(crate) fn armed_toast_slot(&self) -> Option<usize> {
        if self.engine.messages.entries.is_empty() {
            return None;
        }
        let rect = self.notice_column().rect;
        self.engine
            .messages
            .armed_visible_slot(usize::from(rect.3).max(3), rect.2)
    }

    /// The column for the current layout, drawn from the top where `held`
    /// is `Some(true)` and the bottom where it is `Some(false)`, until the
    /// region it keeps clear enters that end.
    fn place_column(&self, held: Option<bool>) -> NoticeColumn {
        let anchor = self.notice_anchor();
        let rect = self.notice_rect();
        let column = |rect, from_top| NoticeColumn {
            rect,
            from_top,
            left_edge: anchor.is_left_corner(),
        };
        let preferred = held.unwrap_or(anchor.is_top_corner());
        let Some((first, last)) = self.keep_clear(rect) else {
            return column(rect, preferred);
        };
        let stack = self.stack_height(rect);
        let covers = |from_top: bool| {
            let (top, bottom) = if from_top {
                (rect.0, rect.0.saturating_add(stack))
            } else {
                (
                    rect.0.saturating_add(rect.3).saturating_sub(stack),
                    rect.0.saturating_add(rect.3),
                )
            };
            stack > 0 && first < bottom && last >= top
        };
        if !covers(preferred) {
            return column(rect, preferred);
        }
        if !covers(!preferred) {
            return column(rect, !preferred);
        }
        let above = first.saturating_sub(rect.0);
        let below = rect
            .0
            .saturating_add(rect.3)
            .saturating_sub(last.saturating_add(1));
        if above.max(below) < 3 {
            // no side holds a framed box, and a box over the region is
            // still a notice read
            return column(rect, preferred);
        }
        if above >= below {
            column((rect.0, rect.1, rect.2, above), true)
        } else {
            column((last.saturating_add(1), rect.1, rect.2, below), false)
        }
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
        let (grid_w, grid_h) = self.engine.painted_grid().size();
        let panes = self.engine.painted_grids().panes_in_z_order();
        let sidebar = |id| {
            self.engine
                .painted_grids()
                .window_handle(id)
                .and_then(|win| self.window_status.get(&win))
                .is_some_and(|status| matches!(status.kind, TileKind::Sidebar { .. }))
        };
        let grid = (0, 0, grid_w, grid_h);
        let area = panes
            .iter()
            .filter(|pane| matches!(pane.kind, PaneKind::Native { .. }) || sidebar(pane.id))
            .map(|pane| pane.filled)
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
                    .filter_map(|pane| self.tile_box(pane))
                    .map(|frame| within(shrink(frame, inset_rows, inset_cols), area))
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
                // the box, gutter included: a notice beside the panel keeps
                // the same gap from its frame that the tiles do
                let rect = self.overlay_box(overlay);
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
        for pane in self.engine.painted_grids().panes_in_z_order() {
            if !matches!(pane.kind, PaneKind::Float { .. }) {
                continue;
            }
            let (f_row, f_col, f_width, f_height) = pane.filled;
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
        let rows = self
            .engine
            .messages
            .shown_rows(usize::from(rect.3).max(3), rect.2);
        u16::try_from(rows).unwrap_or(u16::MAX).min(rect.3)
    }

    /// The first and last grid rows the stack keeps clear of, where the
    /// focused window shares columns with `rect`: the cursor's row, and
    /// while a review is open in that window, the hunk under review from
    /// the first row it replaces or its header to its last added line.
    fn keep_clear(&self, rect: Cells) -> Option<(u16, u16)> {
        let grids = self.engine.painted_grids();
        let (cursor_row, _) = grids.cursor_pos();
        let (focused, ..) = grids.cursor_local();
        let window = grids
            .panes_in_z_order()
            .into_iter()
            .find(|pane| pane.id == focused)?
            .filled;
        if !overlaps(
            (cursor_row, rect.1, rect.2, 1),
            (cursor_row, window.1, window.2, 1),
        ) {
            return None;
        }
        let hunk = self.hunk_rows(focused, cursor_row, window);
        Some(hunk.map_or((cursor_row, cursor_row), |(first, last)| {
            (first.min(cursor_row), last.max(cursor_row))
        }))
    }

    /// The grid rows the hunk under review takes in the window on `grid`,
    /// clipped to the window, or `None` when no review is open there.
    ///
    /// Counted out from the cursor's buffer line: every buffer line between
    /// the two is one row, and so is every virtual line an open hunk hangs
    /// between them. A hunk's virtual lines sit above its first row for an
    /// insertion and under its last row for a replacement, and only the
    /// hunk under review carries the header. A fold or a wrapped line
    /// between the cursor and the hunk moves the span by the rows it hides
    /// or adds.
    fn hunk_rows(&self, grid: GridId, cursor_row: u16, window: Cells) -> Option<(u16, u16)> {
        let review = self.ai_panel().pending_diff.as_ref()?;
        let buffer = review.buffer?;
        let status = self
            .engine
            .painted_grids()
            .window_handle(grid)
            .and_then(|win| self.window_status.get(&win))
            .filter(|status| status.buf == buffer.0)?;
        // each open hunk as the buffer line its virtual lines are drawn
        // under, the count of them, and its first and end rows
        let hunks: Vec<(i64, i64, u32, u32, bool)> = review
            .open_hunk_rows()
            .into_iter()
            .map(|(row, end_row, virt, current)| {
                let after = if end_row > row {
                    i64::from(end_row) - 1
                } else {
                    i64::from(row) - 1
                };
                (after, i64::from(virt), row, end_row, current)
            })
            .collect();
        let &(after, virt, row, end_row, _) = hunks.iter().find(|hunk| hunk.4)?;
        let cursor_line = i64::from(status.row.saturating_sub(1));
        let screen = |line: i64| {
            let between: i64 = hunks
                .iter()
                .map(|&(after, virt, ..)| {
                    if cursor_line <= after && after < line {
                        virt
                    } else if line <= after && after < cursor_line {
                        -virt
                    } else {
                        0
                    }
                })
                .sum();
            i64::from(cursor_row) + (line - cursor_line) + between
        };
        let first = if end_row > row {
            screen(i64::from(row))
        } else {
            screen(after) + 1
        };
        let last = screen(after) + virt;
        let top = i64::from(window.0);
        let bottom = i64::from(far(window.0, window.3));
        let clip = |at: i64| u16::try_from(at.clamp(top, bottom)).unwrap_or(window.0);
        (last >= top && first <= bottom).then(|| (clip(first), clip(last)))
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
    use crate::native::geometry::{SurfaceLayout, SurfacePlacement};
    use crate::native::tree::TreeState;
    use crate::update::update;

    /// Every corner the stack can be anchored at.
    const CORNERS: &[Anchor] =
        SurfaceLayout::accepted_anchors(NativeSurface::Notifications, SurfacePlacement::Overlay);

    /// Every set of windowed surfaces the walk opens: none, each surface
    /// alone at every edge it accepts windowed, and every combination of
    /// them at their default windowed edges.
    fn layouts() -> Vec<Vec<(NativeSurface, Anchor)>> {
        let mut layouts = Vec::new();
        for surface in NativeSurface::ALL {
            for &anchor in SurfaceLayout::accepted_anchors(surface, SurfacePlacement::Windowed) {
                layouts.push(vec![(surface, anchor)]);
            }
        }
        for mask in 0..1_u32 << NativeSurface::ALL.len() {
            let set: Vec<_> = (0..NativeSurface::ALL.len())
                .filter(|bit| mask & 1 << bit != 0)
                .map(|bit| {
                    let surface = NativeSurface::ALL[bit];
                    (surface, SurfaceLayout::default_windowed_anchor(surface))
                })
                .collect();
            if set.len() != 1 {
                layouts.push(set);
            }
        }
        layouts
    }

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

    /// One session on a `size` terminal under `look`: each windowed
    /// surface in `natives` opened against its edge of what the ones before
    /// it left, then `tiles` vsplits side by side in the rest. Every window
    /// keeps its status row under it, as the `laststatus = 2` hold leaves
    /// it. `None` where a window would be narrower or shorter than a framed
    /// slot, or an anchor is no edge.
    pub(crate) fn scene(
        size: (u16, u16),
        look: Look,
        natives: &[(NativeSurface, Anchor)],
        tiles: u16,
    ) -> Option<Scene> {
        let mut model = Model::with_term_size(size.0, size.1).with_look(look);
        let (grid_w, grid_h) = model.grid_target();
        let panel = (grid_w * 3 / 10).max(3);
        let band = (grid_h / 4).max(3);
        let mut placed: Vec<(u64, Cells, Option<NativeSurface>)> = Vec::new();
        let (mut top, mut left) = (0, 0);
        let (mut bottom, mut right) = (grid_h.checked_sub(1)?, grid_w);
        for (surface, anchor) in natives {
            let (width, height) = (right.checked_sub(left)?, bottom.checked_sub(top)?);
            let slot = match anchor {
                Anchor::Left => {
                    left += panel + 1;
                    (top, left - panel - 1, panel, height)
                }
                Anchor::Right => {
                    right = right.checked_sub(panel + 1)?;
                    (top, right + 1, panel, height)
                }
                Anchor::Top => {
                    top += band + 1;
                    (top - band - 1, left, width, band)
                }
                Anchor::Bottom => {
                    bottom = bottom.checked_sub(band)?;
                    let slot = (bottom, left, width, band);
                    bottom = bottom.checked_sub(1)?;
                    slot
                }
                _ => return None,
            };
            let grid = 20 + u64::try_from(surface.index()).unwrap();
            placed.push((grid, slot, Some(*surface)));
        }
        let middle = right.checked_sub(left)?;
        let tile_h = bottom.checked_sub(top)?;
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
            placed.push((grid, (top, col, width, tile_h), None));
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
    /// steps of 11, heights 10 to 60 in steps of 5, both looks with and
    /// without gaps, every corner, every set of [`layouts`], one to three
    /// vsplits, and the cursor in every tile. `each` gets the scene and a
    /// label naming it.
    pub(crate) fn walk(mut each: impl FnMut(&mut Scene, &str)) {
        let layouts = layouts();
        for width in (40..=240).step_by(11) {
            for height in (10..=60).step_by(5) {
                for panes in [Panes::Tiles, Panes::Nvim] {
                    for gaps in [true, false] {
                        for natives in &layouts {
                            for tiles in 1..=3 {
                                let look = Look::new(panes, gaps);
                                let Some(mut scene) = scene((width, height), look, natives, tiles)
                                else {
                                    continue;
                                };
                                for &anchor in CORNERS {
                                    anchor_at(&mut scene.model, anchor);
                                    for index in 0..scene.tiles.len() {
                                        let grid = scene.tiles[index];
                                        focus(&mut scene.model, grid, 0);
                                        let label = format!(
                                            "{width}x{height} {panes:?} gaps={gaps} \
                                             natives={natives:?} tiles={tiles} \
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
                let (row, col, width, height) = focused.filled;
                let frame = match model.look.frame_box(focused.filled) {
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

    /// Every anchor there is, in a chain the compiler checks: a new anchor
    /// fails to compile here until it has a place in it.
    fn anchors() -> impl Iterator<Item = Anchor> {
        std::iter::successors(Some(Anchor::Center), |anchor| match anchor {
            Anchor::Center => Some(Anchor::Left),
            Anchor::Left => Some(Anchor::Right),
            Anchor::Right => Some(Anchor::Top),
            Anchor::Top => Some(Anchor::Bottom),
            Anchor::Bottom => Some(Anchor::TopLeft),
            Anchor::TopLeft => Some(Anchor::TopRight),
            Anchor::TopRight => Some(Anchor::BottomLeft),
            Anchor::BottomLeft => Some(Anchor::BottomRight),
            Anchor::BottomRight => None,
        })
    }

    /// The share of the terminal a panel at `anchor` takes, as the width
    /// and height percentages of its box.
    fn panel_share(anchor: Anchor) -> (u16, u16) {
        match anchor {
            Anchor::Center => (60, 60),
            Anchor::Left | Anchor::Right => (30, 100),
            Anchor::Top | Anchor::Bottom => (100, 30),
            Anchor::TopLeft | Anchor::TopRight | Anchor::BottomLeft | Anchor::BottomRight => {
                (30, 30)
            }
        }
    }

    /// Where a panel sits in terminal cells once it is opened on `scene`:
    /// an overlay pinned at `anchor`, or a plugin's sidebar tile where
    /// `anchor` is `None`.
    fn open_panel(scene: &mut Scene, anchor: Option<Anchor>) -> Option<Cells> {
        let model = &mut scene.model;
        let (geometry, kind) = match anchor {
            Some(anchor) => {
                let (width, height) = panel_share(anchor);
                let kind = match anchor {
                    Anchor::Left => OverlayKind::Tree(TreeState::open(".".into())),
                    _ => OverlayKind::Ai,
                };
                (OverlayBox::new(width, height).with_anchor(anchor), kind)
            }
            None => {
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
        let rect = model.overlay_box(panel);
        Some((rect.row, rect.col, rect.width, rect.height))
    }

    /// A panel drawn over the tiles at every anchor but the centre, and a
    /// plugin's sidebar tile, under both looks with and without gaps,
    /// every corner and the cursor in every other tile.
    #[test]
    fn the_notice_column_never_intersects_a_side_panel_or_a_sidebar_tile() {
        let panels: Vec<Option<Anchor>> = anchors()
            .filter(|&anchor| anchor != Anchor::Center)
            .map(Some)
            .chain([None])
            .collect();
        let mut scenes = 0;
        for width in (40..=240).step_by(13) {
            for height in (10..=60).step_by(5) {
                for (panes, gaps) in [Panes::Tiles, Panes::Nvim]
                    .into_iter()
                    .flat_map(|panes| [(panes, true), (panes, false)])
                {
                    for tiles in 1..=3 {
                        for &kind in &panels {
                            let look = Look::new(panes, gaps);
                            let Some(mut scene) = scene((width, height), look, &[], tiles) else {
                                continue;
                            };
                            let Some(panel) = open_panel(&mut scene, kind) else {
                                continue;
                            };
                            for &anchor in CORNERS {
                                anchor_at(&mut scene.model, anchor);
                                for index in 0..scene.tiles.len() {
                                    focus(&mut scene.model, scene.tiles[index], 0);
                                    scenes += 1;
                                    let model = &scene.model;
                                    let (row, col, w, h) = model.notice_column().rect;
                                    let origin = model.chrome_rows() + model.look.grid_offset();
                                    let column =
                                        (row + origin, col + model.look.grid_offset(), w, h);
                                    let label = format!(
                                        "{width}x{height} {panes:?} gaps={gaps} \
                                         tiles={tiles} panel={kind:?} {anchor:?} \
                                         focus={index}"
                                    );
                                    assert!(w > 0 && h > 0, "{label}: the column is empty");
                                    assert!(
                                        !overlaps(column, panel),
                                        "{label}: the column {column:?} covers the panel \
                                         at {panel:?}"
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

    /// Under gapless tiles, the agent panel docked right over all but a
    /// sliver of the right tile, too narrow for a frame, leaves the stack
    /// in the left tile's text, anchored top-right. The tree docked left
    /// mirrors it, anchored top-left.
    #[test]
    fn a_tile_a_gapless_docked_float_leaves_too_narrow_takes_no_notice() {
        for (dock, corner) in [
            (Anchor::Right, Anchor::TopRight),
            (Anchor::Left, Anchor::TopLeft),
        ] {
            let right = dock == Anchor::Right;
            let (covered, kept) = if right { (1, 0) } else { (0, 1) };
            let mut found = 0;
            for share in 1..100 {
                let mut scene = scene((100, 30), Look::new(Panes::Tiles, false), &[], 2)
                    .expect("two tiles fit");
                let model = &mut scene.model;
                anchor_at(model, corner);
                let kind = if right {
                    OverlayKind::Ai
                } else {
                    OverlayKind::Tree(TreeState::open(".".into()))
                };
                model.push_overlay(OverlayBox::new(share, 100).with_anchor(dock), kind);
                let open = model.overlays().last().expect("the float is open");
                let Some(joined) = model.joined(open) else {
                    continue;
                };
                let join = joined.col;
                let offset = model.look.grid_offset();
                let (_, col, width, _) = scene.slots[covered];
                let sliver = if right {
                    (join - offset).saturating_sub(col)
                } else {
                    (col + width).saturating_sub(join - offset + 1)
                };
                if !(1..MIN_FRAMED_SLOT.0).contains(&sliver) {
                    continue;
                }
                found += 1;
                let label = format!("{dock:?} at {share}%");
                let text = model
                    .engine
                    .grids()
                    .pane_text(GridId(scene.tiles[kept]))
                    .expect("the kept tile shows text");
                let column = model.notice_column().rect;
                assert!(column.2 > 0 && column.3 > 0, "{label}: the column is empty");
                assert_eq!(
                    within(column, text),
                    column,
                    "{label}: the column {column:?} sits in the kept tile's text {text:?}"
                );
                assert!(
                    !(column.1..column.1 + column.2).contains(&(join - offset)),
                    "{label}: the column {column:?} is off the join column {join}"
                );
            }
            assert!(found > 0, "{dock:?}: some share leaves a sliver");
        }
    }

    /// The message history floating in the top-right corner over one gapped
    /// tile closes no frame, so the stack beside it stands flush against
    /// its box, with no column left blank for a border that is not drawn.
    #[test]
    fn a_corner_float_closes_no_frame_the_stack_keeps_clear_of() {
        for size in [(220, 50), (80, 24)] {
            let mut scene =
                scene(size, Look::new(Panes::Tiles, true), &[], 1).expect("one tile fits");
            let model = &mut scene.model;
            anchor_at(model, Anchor::TopRight);
            let state =
                crate::native::palette::MessageHistoryState::snapshot(&model.engine.toast_history);
            model.push_overlay(
                OverlayBox::new(30, 70).with_anchor(Anchor::TopRight),
                OverlayKind::MessageHistory(state),
            );
            let open = model.overlays().last().expect("the history is open");
            let float = model.overlay_box(open);
            let offset = model.look.grid_offset();
            let (_, col, width, _) = model.notice_column().rect;
            assert_eq!(
                col + width + offset,
                float.col,
                "{size:?}: the stack ends where the history's box begins"
            );
        }
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
            let mut scene = scene((80, 24), look, &[], 2).expect("an 80x24 vsplit");
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

            // the cursor in the other tile is under no box, so nothing moves
            let held = scene.model.notice_column().from_top;
            for row in 0..inner_h {
                focus(&mut scene.model, scene.tiles[0], row);
                assert_eq!(
                    scene.model.notice_column().from_top,
                    held,
                    "{anchor:?}: row {row} of the unfocused-column tile moved the stack"
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
        let mut scene = scene((80, 24), look, &[], 2).expect("an 80x24 vsplit");
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

    /// While a grow is half applied, the new slot placed and the grid not
    /// yet resized to it, a notice pinned to a tile's far corner stands
    /// inside the frame drawn on the tile's text.
    #[test]
    fn a_tile_notice_stands_inside_the_frame_until_nvim_resizes_the_grid() {
        for gaps in [true, false] {
            let look = Look::new(Panes::Tiles, gaps);
            let mut scene = scene((80, 24), look, &[], 1).expect("an 80x24 tile");
            anchor_at(&mut scene.model, Anchor::BottomRight);
            let grid = scene.tiles[0];
            let (row, col, width, height) = scene.slots[0];
            let (grid_w, grid_h) = scene.model.engine.grids().global().size();
            let _ = update(
                &mut scene.model,
                Msg::Redraw(vec![
                    UiEvent::GridResize {
                        grid: 1,
                        width: u64::from(grid_w + 20),
                        height: u64::from(grid_h + 6),
                    },
                    UiEvent::WinPos {
                        grid,
                        win: WinHandle(1000 + grid),
                        startrow: u64::from(row),
                        startcol: u64::from(col),
                        width: u64::from(width + 20),
                        height: u64::from(height + 6),
                    },
                    UiEvent::Flush,
                ]),
            );
            let tile = scene
                .model
                .engine
                .grids()
                .panes_in_z_order()
                .into_iter()
                .find(|pane| pane.id == GridId(grid))
                .expect("the tile is placed");
            assert_eq!(tile.slot.2, width + 20, "gaps={gaps}: the grow is placed");
            let (f_row, f_col, f_width, f_height) = tile.filled;
            let (n_row, n_col, n_width, n_height) = scene.model.notice_bounds();
            assert!(
                n_col + n_width <= f_col + f_width && n_row + n_height <= f_row + f_height,
                "gaps={gaps}: the notice column {:?} reaches past the tile's text {:?}",
                (n_row, n_col, n_width, n_height),
                tile.filled
            );
        }
    }

    /// One tile at 80x24 under `anchor` with three notices up, and where
    /// the stack stands before any cursor has had its say: the column, the
    /// rows the stack takes, and the grid row the tile's first line is on.
    fn stacked(anchor: Anchor) -> (Scene, Cells, u16, u16) {
        let look = Look::new(Panes::Tiles, true);
        let mut scene = scene((80, 24), look, &[], 1).expect("an 80x24 tile");
        anchor_at(&mut scene.model, anchor);
        for text in ["saved", "2 matches", "linted"] {
            scene
                .model
                .engine
                .messages
                .push("echomsg".to_string(), vec![(0, text.into())], false);
        }
        let grid = scene.tiles[0];
        focus(&mut scene.model, grid, 0);
        let origin = scene.model.engine.grids().cursor_pos().0;
        let rect = scene.model.notice_bounds();
        let stack = scene.model.stack_height(rect);
        assert!(
            rect.3 > stack * 2 + 1,
            "the column holds a stack at each end with a row between"
        );
        // the first row moved the stack; start over from the anchor's end
        // with the cursor between the two ends
        scene.model.notice_held = None;
        focus(&mut scene.model, grid, rect.0 + stack - origin);
        assert_eq!(scene.model.notice_column().from_top, anchor.is_top_corner());
        (scene, rect, stack, origin)
    }

    /// Opens a review of `old` against `new` in the tile, and puts the
    /// cursor on buffer `line` at the tile's `row`.
    fn review_at(scene: &mut Scene, old: &str, new: &str, line: u32, row: u16) {
        let grid = scene.tiles[0];
        let mut review = crate::native::ai_panel::DiffReviewState::new(
            1,
            std::path::PathBuf::from("a.rs"),
            1,
            crate::native::diff::hunk::diff(Some(old), new),
        );
        review.buffer = Some(crate::msg::BufferHandle(7));
        scene.model.ai_panel_mut().pending_diff = Some(review);
        scene.model.window_status.insert(
            WinHandle(1000 + grid),
            WindowStatus {
                buf: 7,
                row: line + 1,
                ..WindowStatus::default()
            },
        );
        focus(&mut scene.model, grid, row);
    }

    /// The rows the stack takes in `column`.
    fn band(model: &Model, column: NoticeColumn) -> std::ops::Range<u16> {
        let (top, _, _, height) = column.rect;
        let stack = model.stack_height(column.rect);
        if column.from_top {
            top..top + stack
        } else {
            top + height - stack..top + height
        }
    }

    fn lines(count: usize) -> String {
        (0..count).map(|line| format!("line {line}\n")).collect()
    }

    /// A line inserted at the top of the file, its header and the line
    /// itself drawn above the first row, and the cursor below the rows a
    /// top stack takes: the stack leaves the hunk for the bottom.
    #[test]
    fn the_stack_leaves_an_insertion_hunk_at_the_window_top() {
        let (mut scene, rect, stack, origin) = stacked(Anchor::TopRight);
        let old = lines(40);
        let new = format!("inserted\n{old}");
        let cursor = rect.0 + stack;
        // the header and the inserted line take the tile's first 3 rows
        let line = u32::from(cursor - origin - 3);
        review_at(&mut scene, &old, &new, line, cursor - origin);
        assert_eq!(scene.model.engine.grids().cursor_pos().0, cursor);
        let column = scene.model.notice_column();
        let rows = band(&scene.model, column);
        assert!(!column.from_top, "the stack stayed over the hunk: {rows:?}");
        for row in origin..=cursor {
            assert!(
                !rows.contains(&row),
                "the stack {rows:?} covers row {row} of the hunk"
            );
        }
    }

    /// The last two lines of the file replaced by eight, the cursor on the
    /// first of them above the rows a bottom stack takes: the header and the
    /// added lines run into those rows, and the stack moves to the top.
    #[test]
    fn the_stack_leaves_a_long_replacement_at_the_file_end() {
        let (mut scene, rect, stack, origin) = stacked(Anchor::BottomRight);
        let old = lines(40);
        let new = format!(
            "{}{}",
            lines(38),
            (0..8).map(|n| format!("new {n}\n")).collect::<String>()
        );
        let cursor = rect.0 + stack;
        review_at(&mut scene, &old, &new, 38, cursor - origin);
        let column = scene.model.notice_column();
        let rows = band(&scene.model, column);
        assert!(column.from_top, "the stack stayed over the hunk: {rows:?}");
        let last = (cursor + 2 + 10).min(rect.0 + rect.3 - 1);
        for row in cursor..=last {
            assert!(
                !rows.contains(&row),
                "the stack {rows:?} covers row {row} of the hunk"
            );
        }
    }

    /// A hunk that reaches into both ends of the column: the column
    /// shrinks to the larger side of it, and no box covers the hunk.
    #[test]
    fn a_hunk_under_both_ends_shrinks_the_column_to_the_larger_side() {
        let (mut scene, rect, _, origin) = stacked(Anchor::TopRight);
        let old = lines(40);
        let new = format!(
            "{}{}{}",
            lines(10),
            (0..6).map(|n| format!("new {n}\n")).collect::<String>(),
            &lines(40)[lines(11).len()..]
        );
        let cursor = rect.0 + 4;
        review_at(&mut scene, &old, &new, 10, cursor - origin);
        // the replaced row, the header and the six added lines
        let last = cursor + 8;
        let column = scene.model.notice_column();
        let (top, _, _, height) = column.rect;
        assert!(height >= 3, "the column keeps room for a box: {column:?}");
        assert!(
            top > last || top + height <= cursor,
            "the column {column:?} covers the hunk on rows {cursor} to {last}"
        );
        assert_eq!(
            (top, top + height),
            (last + 1, rect.0 + rect.3),
            "the column takes the larger side, under the hunk"
        );
    }

    /// Two replacements, the first under review, and the cursor on the line
    /// under the second, whose six added lines stand between the cursor and
    /// the first: the column shrinks clear of the first hunk's rows where
    /// they are drawn.
    #[test]
    fn the_reviewed_hunk_counts_the_added_lines_of_a_hunk_below_it() {
        let (mut scene, rect, stack, origin) = stacked(Anchor::TopRight);
        let old = lines(30);
        let new = format!(
            "{}first\n{}{}{}",
            lines(3),
            &lines(5)[lines(4).len()..],
            (0..6).map(|n| format!("second {n}\n")).collect::<String>(),
            &lines(30)[lines(6).len()..]
        );
        // lines 0 to 3, the header and the added line of the first hunk,
        // lines 4 and 5, the six added lines of the second, then line 6
        let cursor = origin + 15;
        review_at(&mut scene, &old, &new, 6, cursor - origin);
        let review = scene.model.ai_panel().pending_diff.as_ref().unwrap();
        assert_eq!(review.hunks.len(), 2, "two hunks: {:?}", review.hunks);
        let bottom = rect.0 + rect.3;
        assert!(
            cursor >= bottom - stack && rect.0 + 3 < rect.0 + stack,
            "the cursor is under a bottom stack and the hunk under a top one"
        );
        let column = scene.model.notice_column();
        let (top, _, _, height) = column.rect;
        assert!(height >= 3, "the column keeps room for a box: {column:?}");
        for row in origin + 3..=origin + 6 {
            assert!(
                !(top..top + height).contains(&row),
                "the column {column:?} covers row {row} of the hunk under review"
            );
        }
    }

    /// The cursor moving down every row and back up: the stack moves only
    /// on a step that brings the cursor into the rows it takes.
    #[test]
    fn the_stack_holds_its_end_until_the_cursor_enters_it() {
        for anchor in [Anchor::TopRight, Anchor::BottomRight] {
            let (mut scene, _, _, _) = stacked(anchor);
            let grid = scene.tiles[0];
            let (_, inner_h) = scene
                .model
                .engine
                .grids()
                .grid(GridId(grid))
                .unwrap()
                .size();
            let mut before = scene.model.notice_column();
            let mut moves = 0;
            for row in (0..inner_h).chain((0..inner_h).rev()) {
                focus(&mut scene.model, grid, row);
                let cursor = scene.model.engine.grids().cursor_pos().0;
                let after = scene.model.notice_column();
                if after.from_top != before.from_top {
                    moves += 1;
                    assert!(
                        band(&scene.model, before).contains(&cursor),
                        "{anchor:?}: the stack moved with the cursor on row {cursor}, \
                         outside the rows it took"
                    );
                }
                before = after;
            }
            assert!(moves >= 2, "{anchor:?}: the stack moved {moves} times");
        }
    }
}
