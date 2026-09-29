//! A registry laid out in the slots of a layout that is no longer nvim's.

use super::{GridId, GridRegistry, Slot, WindowSlot};
use crate::grid::{Grid, GridOp};

impl GridRegistry {
    /// Lays this registry out in `drawn` as drawn in `layout`: each window
    /// moved into the slot `layout` gives its handle, and a window standing
    /// in every slot no window here fills. `held` gives, by index into
    /// `layout`, the cells that slot showed before; a stand-in shows them,
    /// and so does a window here nvim has not drawn any text into yet.
    ///
    /// `drawn` keeps its own grids, and each is compared with the cells it
    /// now stands for and copied over, into its own buffers, only where the
    /// two differ. Answers how many grids were copied, or `None`, leaving
    /// `drawn` as it was, when a window here has no slot in `layout`, since
    /// the layout it would be drawn in is then no longer the one on screen.
    pub fn relay_into(
        &self,
        layout: &[WindowSlot],
        held: &[Option<Grid>],
        drawn: &mut Self,
    ) -> Option<usize> {
        let placed = self.window_layout();
        let mut windows = Vec::with_capacity(placed.len());
        for (win, _) in &placed {
            let index = layout.iter().position(|(slot_win, _)| slot_win == win)?;
            let (_, slot) = layout.get(index)?;
            let entry = self
                .slots
                .iter()
                .find(|s| s.window.as_ref().is_some_and(|w| w.win == *win))?;
            let cells = match (entry.grid.has_text(), held.get(index)) {
                (false, Some(Some(cells))) => Some(cells),
                _ => None,
            };
            windows.push((entry.id, *win, *slot, cells));
        }
        let mut copied = usize::from(drawn.global.follow(&self.global));
        drawn.cursor = self.cursor;
        drawn.claims.clone_from(&self.claims);
        drawn.look = self.look;
        drawn.placement_dirty = self.placement_dirty;
        drawn.docks.clone_from(&self.docks);
        drawn.announced.clone_from(&self.announced);
        let mut kept = std::mem::take(&mut drawn.slots);
        let mut reuse = |id: GridId| {
            kept.iter()
                .position(|slot| slot.id == id)
                .map_or_else(Grid::new, |index| kept.swap_remove(index).grid)
        };
        for slot in &self.slots {
            let mut grid = reuse(slot.id);
            let held_cells = windows
                .iter()
                .find(|(id, ..)| *id == slot.id)
                .and_then(|(.., cells)| *cells);
            copied += usize::from(grid.follow(held_cells.unwrap_or(&slot.grid)));
            drawn.slots.push(Slot {
                id: slot.id,
                grid,
                placed: slot.placed.clone(),
                window: slot.window.clone(),
            });
        }
        for (grid, win, slot, cells) in &windows {
            drawn.place_window(*grid, *win, *slot);
            // placing a native pane clears its grid, and the held cells
            // are what the slot shows until the replacement draws text
            if let Some(cells) = cells {
                if let Some(entry) = drawn.slots.iter_mut().find(|s| s.id == *grid) {
                    copied += usize::from(entry.grid.follow(cells));
                }
            }
        }
        // ids counted down from the top of the range, which nvim, counting
        // up from 2, never names in a session
        let mut spare = u64::MAX;
        for (index, (win, slot)) in layout.iter().enumerate() {
            if placed.iter().any(|(live, _)| live == win) {
                continue;
            }
            let id = GridId(spare);
            let mut grid = reuse(id);
            if let Some(Some(cells)) = held.get(index) {
                copied += usize::from(grid.follow(cells));
            } else if grid.size() != (slot.2, slot.3) || grid.has_text() {
                grid = Grid::new();
                grid.apply(GridOp::Resize {
                    width: slot.2,
                    height: slot.3,
                });
                copied += 1;
            }
            drawn.slots.push(Slot {
                id,
                grid,
                placed: None,
                window: None,
            });
            drawn.place_window(id, *win, *slot);
            spare = spare.saturating_sub(1);
        }
        Some(copied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::WinHandle;
    use crate::grid::registry::GridEvent;

    fn cells(grid: GridId, op: GridOp) -> GridEvent {
        GridEvent::Cells { grid, op }
    }

    fn text(row: &str) -> GridOp {
        GridOp::PutLine {
            row: 0,
            col_start: 0,
            cells: vec![(row.to_string(), 0, 1)],
        }
    }

    /// A relay copies each grid once, then only the grid whose cells moved.
    ///
    /// Disconfirm: copying every grid on each relay counts three copies on
    /// the second.
    #[test]
    fn a_relay_copies_only_the_grids_that_changed() {
        let mut live = GridRegistry::new();
        live.apply(cells(
            GridId(1),
            GridOp::Resize {
                width: 80,
                height: 24,
            },
        ));
        live.apply(cells(
            GridId(2),
            GridOp::Resize {
                width: 40,
                height: 23,
            },
        ));
        live.apply(cells(GridId(2), text("f")));
        live.apply(GridEvent::Window {
            grid: GridId(2),
            win: WinHandle(1003),
            startrow: 0,
            startcol: 0,
            width: 40,
            height: 23,
        });
        let layout = [
            (WinHandle(1002), (0, 41, 39, 23)),
            (WinHandle(1003), (0, 0, 40, 23)),
        ];
        let mut stand_in = Grid::new();
        stand_in.apply(GridOp::Resize {
            width: 39,
            height: 23,
        });
        stand_in.apply(text("d"));
        let held = [Some(stand_in), None];
        let mut drawn = GridRegistry::new();

        assert_eq!(live.relay_into(&layout, &held, &mut drawn), Some(3));
        assert_eq!(live.relay_into(&layout, &held, &mut drawn), Some(0));
        live.apply(cells(GridId(2), text("g")));
        assert_eq!(live.relay_into(&layout, &held, &mut drawn), Some(1));
        assert_eq!(
            drawn.window_grid(WinHandle(1003)).map(|g| g.row_text(0)),
            live.window_grid(WinHandle(1003)).map(|g| g.row_text(0)),
        );
        assert!(drawn
            .window_grid(WinHandle(1002))
            .is_some_and(|g| g.row_text(0).starts_with('d')));

        let elsewhere = [(WinHandle(1002), (0, 41, 39, 23))];
        assert_eq!(live.relay_into(&elsewhere, &held, &mut drawn), None);
        assert!(
            drawn.window_grid(WinHandle(1003)).is_some(),
            "a layout the live window has no slot in changed drawn"
        );
    }
}
