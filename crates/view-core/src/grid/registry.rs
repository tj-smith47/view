//! Grids addressed by the id nvim gives them, and the windows placed over
//! them.
//!
//! [`Grid`] models one grid's cells. Under `ext_multigrid` nvim addresses
//! many of them by id and announces where each one sits with its own
//! window-placement events, so the addressing and the geometry live here
//! rather than in the cell buffer. Pure data, like everything in this
//! module: no I/O, no RPC, and every wire-sourced value already saturated by
//! the decoder that produced it.

use crate::grid::{Grid, GridOp};

/// A grid's identity as nvim assigns it. The global grid keeps the id the
/// engine gives it rather than a sentinel, so single-grid and multigrid
/// sessions share one code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GridId(pub u64);

/// The global grid, which nvim numbers 1 in both attach modes and never
/// destroys (`docs/multigrid-wire-capture.md`, "The grid id space"). Under
/// `ext_multigrid` it carries the chrome between windows -- statuslines,
/// separators, the ruler area -- and under single-grid it carries
/// everything, which is why it is a paintable pane in both.
pub const GLOBAL_GRID: GridId = GridId(1);

/// Where a grid sits on screen, and what kind of surface it is.
///
/// The content of a pane is an enum with one variant today because every
/// pane holds an engine window. Compositing non-engine content adds a
/// variant here rather than a parallel structure.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneKind {
    /// An ordinary window laid out by nvim's own window tree.
    Window,
    /// A floating window, drawn above the window layer at its own zindex.
    Float {
        /// nvim's own stacking order, larger is nearer the viewer.
        zindex: u32,
        /// The grid the float was anchored to.
        anchor_grid: GridId,
    },
}

/// A grid that has been sized, placed, or both.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    /// The grid whose cells this pane paints.
    pub id: GridId,
    /// Screen position as `(row, col)`, matching the order nvim announces it
    /// in (`win_pos`'s `startrow`/`startcol`) and reads it back in
    /// (`nvim_win_get_position`).
    pub origin: (u16, u16),
    /// Which layer the pane belongs to and, for a float, how it sorts.
    pub kind: PaneKind,
    /// Whether nvim has taken the pane off screen without destroying it, per
    /// `win_hide`/`win_external_pos`.
    pub hidden: bool,
}

/// One decoded operation as nvim addresses it: cells into a named grid, or a
/// window placed over one.
///
/// Every variant names its grid, because every event on the wire does. The
/// cell operations are the `ext_linegrid` vocabulary [`Grid`] already
/// applies; the rest are the placement vocabulary `ext_multigrid` adds, with
/// field names taken from the declared parameter names in
/// `docs/multigrid-wire-capture.md`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GridEvent {
    /// A cell operation addressed to one grid.
    Cells {
        /// The grid to apply it to.
        grid: GridId,
        /// What to do to its cells.
        op: GridOp,
    },
    /// `grid_destroy`: the grid is gone, along with any placement over it.
    Destroy {
        /// The grid nvim destroyed.
        grid: GridId,
    },
    /// `win_pos`: an ordinary window sits at `(startrow, startcol)`.
    Window {
        /// The window's grid.
        grid: GridId,
        /// Screen row of the window's first text row.
        startrow: u16,
        /// Screen column of the window's first text column.
        startcol: u16,
    },
    /// `win_float_pos`: a floating window sits at the position nvim already
    /// resolved from the anchor.
    Float {
        /// The float's grid, which holds its border too.
        grid: GridId,
        /// The grid the float is anchored to.
        anchor_grid: GridId,
        /// Resolved screen row, after the anchor was applied.
        screen_row: u16,
        /// Resolved screen column, after the anchor was applied.
        screen_col: u16,
        /// nvim's stacking layer for the float.
        zindex: u32,
        /// nvim's own order within one `zindex`.
        compindex: u32,
    },
    /// `win_external_pos`: the window left the layout for a UI window of its
    /// own, so it has no box on this screen.
    External {
        /// The window's grid.
        grid: GridId,
    },
    /// `win_hide`: the window is off screen but alive, as every window of a
    /// tab page that stops being current is.
    Hide {
        /// The hidden window's grid.
        grid: GridId,
    },
    /// `win_close`: the window is gone; its grid follows in the same cycle.
    Close {
        /// The closed window's grid.
        grid: GridId,
    },
}

/// A grid nvim has named, with the placement it has been given if any.
#[derive(Debug, Clone)]
struct Slot {
    id: GridId,
    grid: Grid,
    /// `None` for a grid nvim has sized but not placed, which is every grid
    /// between its first `win_viewport_margins` and its `win_pos`.
    placed: Option<Placement>,
}

#[derive(Debug, Clone)]
struct Placement {
    origin: (u16, u16),
    kind: PaneKind,
    hidden: bool,
    /// nvim's own tiebreak within one zindex; 0 for a window, which has no
    /// stacking order of its own because windows never overlap.
    compindex: u32,
}

impl Placement {
    /// Which layer the placement paints in: every float is above every
    /// window, whatever zindex nvim gave it.
    fn layer(&self) -> u8 {
        match self.kind {
            PaneKind::Window => 0,
            PaneKind::Float { .. } => 1,
        }
    }

    fn zindex(&self) -> u32 {
        match self.kind {
            PaneKind::Window => 0,
            PaneKind::Float { zindex, .. } => zindex,
        }
    }
}

/// Beyond any real session's window count by orders of magnitude, and small
/// enough that a desynced stream naming fresh grid ids forever cannot grow
/// this unboundedly -- the same reason the decoder clamps grid dimensions.
const MAX_GRIDS: usize = 256;

/// Every grid nvim has named, the windows placed over them, and the order
/// they paint in.
///
/// The global grid is a field rather than an entry because it is the one
/// grid nvim never destroys and never places: it is always the full screen
/// at the origin, and holding it apart keeps the single-grid session's every
/// `grid_line` a branch rather than a search.
#[derive(Debug, Clone)]
pub struct GridRegistry {
    global: Grid,
    slots: Vec<Slot>,
    cursor: Option<GridId>,
}

impl GridRegistry {
    /// A registry holding nothing but an empty global grid.
    #[must_use]
    pub fn new() -> Self {
        Self {
            global: Grid::new(),
            slots: Vec::new(),
            cursor: None,
        }
    }

    /// The global grid, for reading.
    #[must_use]
    #[inline]
    pub fn global(&self) -> &Grid {
        &self.global
    }

    /// The global grid, for the crate-private damage drain.
    #[inline]
    pub(crate) fn global_mut(&mut self) -> &mut Grid {
        &mut self.global
    }

    /// The cells of one grid, or `None` for an id nvim has never named.
    #[must_use]
    pub fn grid(&self, id: GridId) -> Option<&Grid> {
        if id == GLOBAL_GRID {
            return Some(&self.global);
        }
        self.slots
            .iter()
            .find(|slot| slot.id == id)
            .map(|s| &s.grid)
    }

    /// Applies one decoded grid or window operation.
    ///
    /// An operation naming a grid that has no size yet records the
    /// placement and waits: nvim may place a window before it sizes the
    /// grid behind it, and dropping the placement would lose a pane that
    /// never reappears.
    pub fn apply(&mut self, op: GridEvent) {
        match op {
            GridEvent::Cells { grid, op } => self.apply_cells(grid, op),
            // the global grid is never destroyed and never placed, so an
            // event naming it can only be a desynced stream; dropping the
            // screen's own surface on one is not a recoverable state
            GridEvent::Destroy { grid } if grid != GLOBAL_GRID => {
                self.slots.retain(|slot| slot.id != grid);
            }
            GridEvent::Window {
                grid,
                startrow,
                startcol,
            } if grid != GLOBAL_GRID => {
                self.place(grid, (startrow, startcol), PaneKind::Window, 0);
            }
            GridEvent::Float {
                grid,
                anchor_grid,
                screen_row,
                screen_col,
                zindex,
                compindex,
            } if grid != GLOBAL_GRID => {
                self.place(
                    grid,
                    (screen_row, screen_col),
                    PaneKind::Float {
                        zindex,
                        anchor_grid,
                    },
                    compindex,
                );
            }
            // an external window has a UI window of its own and no box on
            // this screen, which is the same thing a hidden one has; its
            // grid stays alive either way
            GridEvent::Hide { grid } | GridEvent::External { grid } => {
                if let Some(slot) = self.slot_mut(grid) {
                    if let Some(placed) = slot.placed.as_mut() {
                        placed.hidden = true;
                    }
                }
            }
            // the grid outlives the window by one event (`win_close` is
            // always followed by `grid_destroy` in the same cycle), so the
            // pane goes now and the cells go with the destroy
            GridEvent::Close { grid } => {
                if let Some(slot) = self.slot_mut(grid) {
                    slot.placed = None;
                }
            }
            GridEvent::Destroy { .. } | GridEvent::Window { .. } | GridEvent::Float { .. } => {}
        }
    }

    /// Applies one cell operation to the grid it names.
    ///
    /// The hot path's own entry: `grid_line` is the highest-frequency event
    /// on the wire, and routing it through [`GridEvent`] would build an
    /// enum only to take it apart again on the way to the same
    /// [`Grid::apply`] the single-grid session always made.
    #[inline]
    pub(crate) fn apply_cells(&mut self, grid: GridId, op: GridOp) {
        if let GridOp::CursorGoto { .. } = op {
            self.cursor = Some(grid);
        }
        if grid == GLOBAL_GRID {
            self.global.apply(op);
        } else if let Some(slot) = self.slot_mut(grid) {
            slot.grid.apply(op);
        }
    }

    /// Every visible pane, back to front, ready to paint.
    ///
    /// The global grid is always the first: under `ext_multigrid` it holds
    /// the chrome between windows, and under single-grid it holds the whole
    /// picture, so it is the layer every window pane paints over.
    #[must_use]
    pub fn panes_in_z_order(&self) -> Vec<Pane> {
        let mut panes = vec![Pane {
            id: GLOBAL_GRID,
            origin: (0, 0),
            kind: PaneKind::Window,
            hidden: false,
        }];
        let mut placed: Vec<(&Slot, &Placement)> = self
            .slots
            .iter()
            .filter_map(|slot| slot.placed.as_ref().map(|p| (slot, p)))
            .filter(|(_, p)| !p.hidden)
            .collect();
        // the id is the last key so the order is total: nvim reuses no id
        // after a destroy, so two panes never tie on all four
        placed.sort_by_key(|(slot, p)| (p.layer(), p.zindex(), p.compindex, slot.id));
        panes.extend(placed.into_iter().map(|(slot, p)| Pane {
            id: slot.id,
            origin: p.origin,
            kind: p.kind.clone(),
            hidden: p.hidden,
        }));
        panes
    }

    /// The grid the global screen coordinates fall inside, topmost pane
    /// first.
    ///
    /// Takes screen `(col, row)` and answers `(grid, col, row)` inside that
    /// grid -- column first on both sides, which is the order a mouse event
    /// carries and the opposite of [`Pane::origin`]'s `(row, col)`, the
    /// order nvim announces a *placement* in.
    ///
    /// The global grid answers only for a session that has no window panes
    /// at all: under `ext_multigrid` its cells are the chrome *between*
    /// windows, and a click on a separator is view's own to interpret rather
    /// than the engine's to receive.
    #[must_use]
    pub fn hit_test(&self, col: u16, row: u16) -> Option<(GridId, u16, u16)> {
        for pane in self.panes_in_z_order().into_iter().rev() {
            if pane.id == GLOBAL_GRID {
                continue;
            }
            if let Some(hit) = self.hit_pane(&pane, col, row) {
                return Some(hit);
            }
        }
        if self.slots.iter().any(|slot| slot.placed.is_some()) {
            return None;
        }
        let (width, height) = self.global.size();
        (col < width && row < height).then_some((GLOBAL_GRID, col, row))
    }

    /// The grid nvim last placed the cursor in.
    #[must_use]
    pub fn cursor_grid(&self) -> Option<GridId> {
        self.cursor
    }

    /// Drops every grid but the global one, and every placement with them,
    /// for a connection being replaced.
    ///
    /// Grid ids are per-connection allocations, so a replacement's are
    /// somebody else's numbers -- both halves: a pane left behind names a
    /// window that no longer exists and nothing will ever destroy it, and
    /// the cells behind it are a dead session's. The global grid's own
    /// cells stay, so the last frame survives the restart the way it always
    /// has.
    pub(crate) fn forget_grids(&mut self) {
        self.slots.clear();
        self.cursor = None;
    }

    /// Where `(col, row)` lands inside `pane`, if it lands inside it at all.
    fn hit_pane(&self, pane: &Pane, col: u16, row: u16) -> Option<(GridId, u16, u16)> {
        let (width, height) = self.grid(pane.id)?.size();
        let (top, left) = pane.origin;
        let (in_row, in_col) = (row.checked_sub(top)?, col.checked_sub(left)?);
        (in_col < width && in_row < height).then_some((pane.id, in_col, in_row))
    }

    /// Records a placement over `grid`, creating the grid if nvim has named
    /// it here first, and un-hiding it: a hidden window comes back through a
    /// bare `win_pos` with no paired "show" event of its own.
    fn place(&mut self, grid: GridId, origin: (u16, u16), kind: PaneKind, compindex: u32) {
        if let Some(slot) = self.slot_mut(grid) {
            slot.placed = Some(Placement {
                origin,
                kind,
                hidden: false,
                compindex,
            });
        }
    }

    /// The slot for `id`, created empty if nvim has not named it before.
    fn slot_mut(&mut self, id: GridId) -> Option<&mut Slot> {
        if let Some(index) = self.slots.iter().position(|slot| slot.id == id) {
            return self.slots.get_mut(index);
        }
        if self.slots.len() >= MAX_GRIDS {
            return None;
        }
        self.slots.push(Slot {
            id,
            grid: Grid::new(),
            placed: None,
        });
        self.slots.last_mut()
    }
}

impl Default for GridRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn a_placement_before_its_resize_is_retained() {
        let mut registry = GridRegistry::new();
        registry.apply(GridEvent::Window {
            grid: GridId(2),
            startrow: 0,
            startcol: 41,
        });
        registry.apply(GridEvent::Cells {
            grid: GridId(2),
            op: GridOp::Resize {
                width: 39,
                height: 23,
            },
        });
        let panes = registry.panes_in_z_order();
        assert!(
            panes
                .iter()
                .any(|p| p.id == GridId(2) && p.origin == (0, 41)),
            "the placement that arrived before the resize was dropped: {panes:?}"
        );
        assert_eq!(
            registry.grid(GridId(2)).map(Grid::size),
            Some((39, 23)),
            "the resize that followed the placement did not reach the grid"
        );
    }

    /// Both shapes the capture records: a closed window sends `win_close`
    /// then `grid_destroy`, while a closed tab page sends `grid_destroy`
    /// alone, with the placement still standing when it arrives.
    #[test]
    fn grid_destroy_removes_the_pane_and_its_placement() {
        for closed in [false, true] {
            let mut registry = GridRegistry::new();
            resize(&mut registry, GridId(4), 40, 23);
            registry.apply(GridEvent::Window {
                grid: GridId(4),
                startrow: 0,
                startcol: 0,
            });
            if closed {
                registry.apply(GridEvent::Close { grid: GridId(4) });
            }
            registry.apply(GridEvent::Destroy { grid: GridId(4) });
            assert_eq!(
                ids(&registry),
                vec![GLOBAL_GRID],
                "a destroyed grid still has a pane (win_close first: {closed})"
            );
            assert!(registry.grid(GridId(4)).is_none());
        }
    }

    #[test]
    fn hidden_grids_are_not_painted_but_are_not_forgotten() {
        let mut registry = GridRegistry::new();
        resize(&mut registry, GridId(5), 29, 19);
        registry.apply(GridEvent::Window {
            grid: GridId(5),
            startrow: 0,
            startcol: 41,
        });
        registry.apply(GridEvent::Hide { grid: GridId(5) });
        assert_eq!(ids(&registry), vec![GLOBAL_GRID]);
        assert_eq!(
            registry.grid(GridId(5)).map(Grid::size),
            Some((29, 19)),
            "a hidden grid's cells were dropped, and nvim resends none of them"
        );
        // a hidden window comes back through a bare `win_pos`; the wire
        // carries no paired "show"
        registry.apply(GridEvent::Window {
            grid: GridId(5),
            startrow: 0,
            startcol: 41,
        });
        assert_eq!(ids(&registry), vec![GLOBAL_GRID, GridId(5)]);
    }

    #[test]
    fn floats_sort_above_windows_by_zindex() {
        let mut registry = GridRegistry::new();
        for id in [GridId(2), GridId(6)] {
            resize(&mut registry, id, 10, 5);
            registry.apply(GridEvent::Window {
                grid: id,
                startrow: 0,
                startcol: 0,
            });
        }
        for (id, zindex) in [(GridId(9), 200), (GridId(7), 50)] {
            resize(&mut registry, id, 4, 2);
            registry.apply(GridEvent::Float {
                grid: id,
                anchor_grid: GLOBAL_GRID,
                screen_row: 1,
                screen_col: 1,
                zindex,
                compindex: 1,
            });
        }
        assert_eq!(
            ids(&registry),
            vec![GLOBAL_GRID, GridId(2), GridId(6), GridId(7), GridId(9)],
            "floats must paint after every window, lowest zindex first"
        );
    }

    #[test]
    fn an_op_naming_an_unknown_grid_is_recorded_not_dropped() {
        let mut registry = GridRegistry::new();
        registry.apply(GridEvent::Cells {
            grid: GridId(8),
            op: GridOp::PutLine {
                row: 0,
                col_start: 0,
                cells: vec![("x".into(), 0, 1)],
            },
        });
        assert!(
            registry.grid(GridId(8)).is_some(),
            "a grid nvim named only by writing to it was dropped"
        );
        registry.apply(GridEvent::Hide { grid: GridId(11) });
        assert!(registry.grid(GridId(11)).is_some());
    }

    #[test]
    fn single_grid_sessions_produce_exactly_one_pane() {
        let mut registry = GridRegistry::new();
        resize(&mut registry, GLOBAL_GRID, 80, 24);
        registry.apply(GridEvent::Cells {
            grid: GLOBAL_GRID,
            op: GridOp::PutLine {
                row: 0,
                col_start: 0,
                cells: vec![("h".into(), 0, 1), ("i".into(), 0, 1)],
            },
        });
        registry.apply(GridEvent::Cells {
            grid: GLOBAL_GRID,
            op: GridOp::CursorGoto { row: 0, col: 2 },
        });
        assert_eq!(ids(&registry), vec![GLOBAL_GRID]);
        assert_eq!(registry.cursor_grid(), Some(GLOBAL_GRID));
        assert_eq!(registry.global().row_text(0).trim_end(), "hi");
        // the identity translation the mouse path relies on while nvim has
        // placed no window of its own
        assert_eq!(registry.hit_test(3, 1), Some((GLOBAL_GRID, 3, 1)));
        assert_eq!(registry.hit_test(80, 0), None);
    }

    #[test]
    fn hit_test_names_the_topmost_pane_in_its_own_coordinates() {
        let mut registry = GridRegistry::new();
        resize(&mut registry, GLOBAL_GRID, 80, 24);
        resize(&mut registry, GridId(4), 40, 23);
        registry.apply(GridEvent::Window {
            grid: GridId(4),
            startrow: 0,
            startcol: 0,
        });
        resize(&mut registry, GridId(2), 39, 23);
        registry.apply(GridEvent::Window {
            grid: GridId(2),
            startrow: 0,
            startcol: 41,
        });
        resize(&mut registry, GridId(7), 22, 5);
        registry.apply(GridEvent::Float {
            grid: GridId(7),
            anchor_grid: GLOBAL_GRID,
            screen_row: 2,
            screen_col: 4,
            zindex: 50,
            compindex: 1,
        });
        assert_eq!(registry.hit_test(41, 3), Some((GridId(2), 0, 3)));
        assert_eq!(registry.hit_test(5, 3), Some((GridId(7), 1, 1)));
        // column 40 is the separator: it is the global grid's cell, and no
        // window's
        assert_eq!(registry.hit_test(40, 10), None);
    }

    #[test]
    fn the_grid_ceiling_refuses_the_next_id_and_keeps_every_earlier_one() {
        let mut registry = GridRegistry::new();
        let last = u64::try_from(MAX_GRIDS).unwrap_or(u64::MAX) + 1;
        // ids are sparse and never reused, so a desynced stream naming a
        // fresh one forever is the shape the ceiling exists for
        for id in 2..=last {
            resize(&mut registry, GridId(id), 4, 2);
        }
        assert!(registry.grid(GridId(2)).is_some());
        assert_eq!(
            registry.grid(GridId(last)).map(Grid::size),
            Some((4, 2)),
            "the last grid inside the ceiling was refused"
        );
        resize(&mut registry, GridId(9999), 4, 2);
        assert!(
            registry.grid(GridId(9999)).is_none(),
            "the ceiling admitted a grid past MAX_GRIDS"
        );
    }

    fn resize(registry: &mut GridRegistry, grid: GridId, width: u16, height: u16) {
        registry.apply(GridEvent::Cells {
            grid,
            op: GridOp::Resize { width, height },
        });
    }

    fn ids(registry: &GridRegistry) -> Vec<GridId> {
        registry
            .panes_in_z_order()
            .into_iter()
            .map(|pane| pane.id)
            .collect()
    }
}
