//! A registry laid out in the slots of a layout that is no longer nvim's.

use super::{GridId, GridRegistry, Slot, WindowSlot};
use crate::grid::Grid;

impl GridRegistry {
    /// Lays this registry out in `drawn` as drawn in `layout`: each window
    /// moved into the slot `layout` gives its handle, and a window standing
    /// in every slot no window here fills. `placed` is this registry's
    /// [`Self::window_layout`]. `held` gives, by index into `layout`, the
    /// cells that slot showed before. A window or stand-in claimed for a
    /// native pane not placed yet shows a blank grid, whatever its slot
    /// held. An unclaimed stand-in shows its held cells, and so does an
    /// unclaimed window here nvim has not drawn any text into yet.
    ///
    /// `drawn` keeps its own grids, and each is compared with the cells it
    /// now stands for and copied over, into its own buffers, only where the
    /// two differ. A copy allocates only for a cell whose text outgrows the
    /// buffer it had, and for a grid whose size moved or whose id `drawn`
    /// has not held before. Answers how many grids were copied, or `None`, leaving
    /// `drawn` as it was, when a window here has no slot in `layout`, since
    /// the layout it would be drawn in is then no longer the one on screen,
    /// or when `reported` says every slot is a window here that reported
    /// its status and each of them has drawn text, since the replacement
    /// then draws every slot itself.
    #[must_use]
    pub(crate) fn relay_into(
        &self,
        placed: &[WindowSlot],
        layout: &[WindowSlot],
        held: &[Option<Grid>],
        reported: bool,
        drawn: &mut Self,
    ) -> Option<usize> {
        let mut windows = Vec::with_capacity(placed.len());
        let mut texted = true;
        for (win, _) in placed {
            let index = layout.iter().position(|(slot_win, _)| slot_win == win)?;
            let (_, slot) = layout.get(index)?;
            let entry = self
                .slots
                .iter()
                .find(|s| s.window.as_ref().is_some_and(|w| w.win == *win))?;
            let has_text = entry.grid.has_text();
            texted &= has_text;
            let blank = self.clears_when_placed(entry.id, *win);
            let cells = match (has_text || blank, held.get(index)) {
                (false, Some(Some(cells))) => Some(cells),
                _ => None,
            };
            windows.push((entry.id, *win, *slot, cells, blank));
        }
        if reported && texted {
            return None;
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
            let window = windows.iter().find(|(id, ..)| *id == slot.id);
            copied += usize::from(match window {
                Some((.., Some(cells), _)) => grid.follow(cells),
                Some((.., None, true)) => grid.blank_to(slot.grid.size(), slot.grid.cursor()),
                _ => grid.follow(&slot.grid),
            });
            drawn.slots.push(Slot {
                id: slot.id,
                grid,
                placed: slot.placed.clone(),
                window: slot.window.clone(),
            });
        }
        // each grid, stand-ins included, already holds what placing its
        // window leaves on screen, the blank a native pane is cleared to
        // among them, so the clear is skipped and last flush's copy compares
        // equal
        for (grid, win, slot, ..) in &windows {
            drawn.seat_window(*grid, *win, *slot);
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
            let blank = drawn.clears_when_placed(id, *win);
            copied += usize::from(match held.get(index) {
                Some(Some(cells)) if blank => grid.blank_to(cells.size(), cells.cursor()),
                Some(Some(cells)) => grid.follow(cells),
                _ => grid.blank_to((slot.2, slot.3), (0, 0)),
            });
            drawn.slots.push(Slot {
                id,
                grid,
                placed: None,
                window: None,
            });
            drawn.seat_window(id, *win, *slot);
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
    use crate::grid::GridOp;
    use crate::native::geometry::NativeSurface;

    fn cells(grid: GridId, op: GridOp) -> GridEvent {
        GridEvent::Cells { grid, op }
    }

    fn relay(
        live: &GridRegistry,
        layout: &[WindowSlot],
        held: &[Option<Grid>],
        drawn: &mut GridRegistry,
    ) -> Option<usize> {
        live.relay_into(&live.window_layout(), layout, held, false, drawn)
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

        assert_eq!(relay(&live, &layout, &held, &mut drawn), Some(3));
        assert_eq!(relay(&live, &layout, &held, &mut drawn), Some(0));
        live.apply(cells(GridId(2), text("g")));
        assert_eq!(relay(&live, &layout, &held, &mut drawn), Some(1));
        assert_eq!(
            drawn.window_grid(WinHandle(1003)).map(|g| g.row_text(0)),
            live.window_grid(WinHandle(1003)).map(|g| g.row_text(0)),
        );
        assert!(drawn
            .window_grid(WinHandle(1002))
            .is_some_and(|g| g.row_text(0).starts_with('d')));

        let elsewhere = [(WinHandle(1002), (0, 41, 39, 23))];
        assert_eq!(relay(&live, &elsewhere, &held, &mut drawn), None);
        assert!(
            drawn.window_grid(WinHandle(1003)).is_some(),
            "a layout the live window has no slot in changed drawn"
        );
    }

    /// A layout whose every slot is a window that reported its status and
    /// drew text is left to the replacement, and one still missing text is
    /// relaid.
    ///
    /// Disconfirm: ignoring `reported` relays the settled layout.
    #[test]
    fn a_reported_layout_with_text_in_every_slot_is_not_relaid() {
        let mut live = GridRegistry::new();
        live.apply(cells(
            GridId(2),
            GridOp::Resize {
                width: 40,
                height: 23,
            },
        ));
        live.apply(GridEvent::Window {
            grid: GridId(2),
            win: WinHandle(1003),
            startrow: 0,
            startcol: 0,
            width: 40,
            height: 23,
        });
        let layout = [(WinHandle(1003), (0, 0, 40, 23))];
        let placed = live.window_layout();
        let mut drawn = GridRegistry::new();

        assert!(live
            .relay_into(&placed, &layout, &[None], true, &mut drawn)
            .is_some());
        live.apply(cells(GridId(2), text("f")));
        assert_eq!(
            live.relay_into(&placed, &layout, &[None], false, &mut drawn),
            Some(1)
        );
        assert_eq!(
            live.relay_into(&placed, &layout, &[None], true, &mut drawn),
            None
        );
    }

    /// A window or stand-in claimed for a surface but not yet placed as its
    /// pane is drawn blank, and a relay that finds it blank already copies
    /// nothing.
    ///
    /// Disconfirm: copying the live or held cells before the clear placing
    /// them runs counts copies on the second relay.
    #[test]
    fn a_claimed_window_awaiting_its_pane_is_blanked_without_a_copy() {
        let mut live = GridRegistry::new();
        live.apply(cells(
            GridId(3),
            GridOp::Resize {
                width: 30,
                height: 10,
            },
        ));
        live.apply(cells(GridId(3), text("engine")));
        live.apply(GridEvent::Window {
            grid: GridId(3),
            win: WinHandle(1004),
            startrow: 0,
            startcol: 0,
            width: 30,
            height: 10,
        });
        live.claims.push((WinHandle(1004), NativeSurface::Tree));
        live.claims.push((WinHandle(1005), NativeSurface::Agent));
        let layout = [
            (WinHandle(1004), (0, 0, 30, 10)),
            (WinHandle(1005), (0, 31, 20, 10)),
        ];
        let mut before = Grid::new();
        before.apply(GridOp::Resize {
            width: 20,
            height: 10,
        });
        before.apply(text("held"));
        let held = [None, Some(before)];
        let mut drawn = GridRegistry::new();

        assert_eq!(relay(&live, &layout, &held, &mut drawn), Some(2));
        assert_eq!(relay(&live, &layout, &held, &mut drawn), Some(0));
        for win in [WinHandle(1004), WinHandle(1005)] {
            assert!(
                drawn.window_grid(win).is_some_and(|g| !g.has_text()),
                "{win:?} shows cells under a pane"
            );
        }
        assert_eq!(
            drawn.window_grid(WinHandle(1004)).map(Grid::size),
            Some((30, 10))
        );
    }

    /// A claimed window nvim has drawn no text into is blank whatever its
    /// slot held.
    ///
    /// Disconfirm: taking the held cells for a window without text before
    /// asking whether it is claimed shows "held" in the pane.
    #[test]
    fn a_claimed_window_without_text_is_blank_over_held_cells() {
        let mut live = GridRegistry::new();
        live.apply(cells(
            GridId(3),
            GridOp::Resize {
                width: 20,
                height: 10,
            },
        ));
        live.apply(GridEvent::Window {
            grid: GridId(3),
            win: WinHandle(1004),
            startrow: 0,
            startcol: 0,
            width: 20,
            height: 10,
        });
        live.claims.push((WinHandle(1004), NativeSurface::Tree));
        let layout = [(WinHandle(1004), (0, 0, 20, 10))];
        let mut before = Grid::new();
        before.apply(GridOp::Resize {
            width: 20,
            height: 10,
        });
        before.apply(text("held"));
        let mut drawn = GridRegistry::new();

        assert!(relay(&live, &layout, &[Some(before.clone())], &mut drawn).is_some());
        assert_eq!(relay(&live, &layout, &[Some(before)], &mut drawn), Some(0));
        assert!(drawn
            .window_grid(WinHandle(1004))
            .is_some_and(|g| !g.has_text()));
    }

    /// A stand-in that showed coloured blanks is redrawn blank once its slot
    /// holds nothing.
    ///
    /// Disconfirm: judging the reused stand-in by its text alone keeps
    /// highlight 7 in the cell.
    #[test]
    fn a_reused_stand_in_drops_the_highlight_it_showed() {
        let mut live = GridRegistry::new();
        live.apply(cells(
            GridId(1),
            GridOp::Resize {
                width: 80,
                height: 24,
            },
        ));
        let layout = [(WinHandle(1002), (0, 0, 20, 5))];
        let mut coloured = Grid::new();
        coloured.apply(GridOp::Resize {
            width: 20,
            height: 5,
        });
        coloured.apply(GridOp::PutLine {
            row: 0,
            col_start: 0,
            cells: vec![(" ".to_string(), 7, 20)],
        });
        let mut drawn = GridRegistry::new();

        assert!(relay(&live, &layout, &[Some(coloured)], &mut drawn).is_some());
        assert_eq!(relay(&live, &layout, &[None], &mut drawn), Some(1));
        assert_eq!(
            drawn
                .window_grid(WinHandle(1002))
                .and_then(|g| g.cell(0, 0))
                .map(|c| c.hl_id),
            Some(0)
        );
    }
}
