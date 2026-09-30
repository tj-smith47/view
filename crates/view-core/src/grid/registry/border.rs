//! The cells between two windows, and which two windows they separate.

use super::{GridId, GridRegistry, PaneKind};

/// Which way a border between two windows runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderAxis {
    /// A column between a window on the left and one on the right. Dragging
    /// it moves it across columns.
    Columns,
    /// A row between a window above and one below. Dragging it moves it
    /// across rows.
    Rows,
}

/// Two windows and the border between them, as [`GridRegistry::border_between`]
/// answers for a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Border {
    /// Which way the border runs.
    pub axis: BorderAxis,
    /// The window on the left, or above.
    pub before: GridId,
    /// The window on the right, or below.
    pub after: GridId,
    /// The size of the window before the border along the axis, in the
    /// cells nvim laid it out in.
    pub span: u16,
}

/// A window's text rect and nvim's slot for it, both as
/// `(row, col, width, height)`.
type Sides = ((u16, u16, u16, u16), (u16, u16, u16, u16));

impl GridRegistry {
    /// The border screen `(col, row)` stands on, column first as
    /// [`Self::hit_test`] takes it, or `None` for a cell that separates no
    /// two windows.
    ///
    /// A border cell lies between the text of two windows nvim placed side
    /// by side, one separator apart: the separator itself and, under a
    /// framed look, the frame edge on each side of it. The cell has to lie
    /// inside both windows' text on the other axis, so a corner where
    /// frames or separators meet answers nothing. Floats never border
    /// anything, and a cell a pane's text covers is that pane's.
    #[must_use]
    pub fn border_between(&self, col: u16, row: u16) -> Option<Border> {
        if self.hit_test(col, row).is_some() {
            return None;
        }
        let windows: Vec<(GridId, Sides)> = self
            .panes_in_z_order()
            .into_iter()
            .filter(|pane| matches!(pane.kind, PaneKind::Window | PaneKind::Native { .. }))
            .filter(|pane| self.window_handle(pane.id).is_some())
            .filter_map(|pane| {
                let text = pane.text(self.grid(pane.id)?.size());
                Some((pane.id, (text, pane.slot)))
            })
            .collect();
        for &(before, (text, slot)) in &windows {
            for &(after, (next_text, next_slot)) in &windows {
                if let Some(axis) = separates((text, slot), (next_text, next_slot), col, row) {
                    let span = match axis {
                        BorderAxis::Columns => slot.2,
                        BorderAxis::Rows => slot.3,
                    };
                    return Some(Border {
                        axis,
                        before,
                        after,
                        span,
                    });
                }
            }
        }
        None
    }
}

/// The axis on which `before` and `after` are neighbours whose border
/// `(col, row)` stands on, or `None`.
///
/// Neighbours are what nvim's own layout makes them: the second slot starts
/// one separator past the end of the first.
fn separates(before: Sides, after: Sides, col: u16, row: u16) -> Option<BorderAxis> {
    let ((top, left, width, height), slot) = before;
    let ((next_top, next_left, next_width, next_height), next_slot) = after;
    let within = |at: u16, start: u16, len: u16| at >= start && at < start.saturating_add(len);
    let beside = next_slot.1 == slot.1.saturating_add(slot.2).saturating_add(1);
    if beside
        && within(row, top, height)
        && within(row, next_top, next_height)
        && col >= left.saturating_add(width)
        && col < next_left
    {
        return Some(BorderAxis::Columns);
    }
    let below = next_slot.0 == slot.0.saturating_add(slot.3).saturating_add(1);
    if below
        && within(col, left, width)
        && within(col, next_left, next_width)
        && row >= top.saturating_add(height)
        && row < next_top
    {
        return Some(BorderAxis::Rows);
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::events::WinHandle;
    use crate::grid::registry::GridEvent;
    use crate::grid::GridOp;
    use crate::model::{Look, Panes};
    use crate::native::geometry::NativeSurface;

    /// Places `grid`'s window in `slot`, `(row, col, width, height)`, with a
    /// grid of the size the look asks nvim for.
    fn tile(registry: &mut GridRegistry, grid: u64, slot: (u16, u16, u16, u16)) {
        let (startrow, startcol, width, height) = slot;
        let ring = if registry.look().inset() == (0, 0) {
            0
        } else {
            2
        };
        registry.apply(GridEvent::Cells {
            grid: GridId(grid),
            op: GridOp::Resize {
                width: width - ring,
                height: height - ring,
            },
        });
        registry.apply(GridEvent::Window {
            grid: GridId(grid),
            win: WinHandle(grid),
            startrow,
            startcol,
            width,
            height,
        });
    }

    fn looks() -> [Look; 3] {
        [
            Look::new(Panes::Nvim, true),
            Look::new(Panes::Tiles, false),
            Look::new(Panes::Tiles, true),
        ]
    }

    /// Two windows side by side, one separator column apart, and a third
    /// under the right one.
    fn layout(look: Look) -> GridRegistry {
        let mut registry = GridRegistry::new();
        registry.set_look(look);
        tile(&mut registry, 2, (0, 0, 40, 23));
        tile(&mut registry, 3, (0, 41, 39, 11));
        tile(&mut registry, 4, (12, 41, 39, 11));
        registry
    }

    #[test]
    fn the_separator_between_two_tiles_names_both_and_the_axis() {
        for look in looks() {
            let registry = layout(look);
            let columns = Some(Border {
                axis: BorderAxis::Columns,
                before: GridId(2),
                after: GridId(3),
                span: 40,
            });
            assert_eq!(registry.border_between(40, 5), columns, "{look:?}");
            let rows = Some(Border {
                axis: BorderAxis::Rows,
                before: GridId(3),
                after: GridId(4),
                span: 11,
            });
            assert_eq!(registry.border_between(60, 11), rows, "{look:?}");
        }
    }

    #[test]
    fn a_framed_tiles_edges_border_their_neighbour_like_the_gap() {
        let registry = layout(Look::new(Panes::Tiles, true));
        for col in [39, 40, 41] {
            assert_eq!(
                registry.border_between(col, 5).map(|b| (b.before, b.after)),
                Some((GridId(2), GridId(3))),
                "column {col}"
            );
        }
        assert_eq!(
            registry.border_between(38, 5),
            None,
            "text of the left tile"
        );
    }

    #[test]
    fn a_corner_where_borders_meet_answers_nothing() {
        for look in looks() {
            let registry = layout(look);
            assert_eq!(registry.border_between(40, 11), None, "{look:?}");
        }
        let gapped = layout(Look::new(Panes::Tiles, true));
        assert_eq!(gapped.border_between(40, 0), None, "a top frame corner");
    }

    #[test]
    fn a_docked_trees_right_edge_borders_the_window_beside_it() {
        for look in looks() {
            let mut registry = GridRegistry::new();
            registry.set_look(look);
            registry.claim_native_window(WinHandle(2), NativeSurface::Tree);
            tile(&mut registry, 2, (0, 0, 24, 23));
            tile(&mut registry, 3, (0, 25, 55, 23));
            let border = registry.border_between(24, 5).expect("the tree's edge");
            assert_eq!(
                (border.axis, border.before, border.after),
                (BorderAxis::Columns, GridId(2), GridId(3)),
                "{look:?}"
            );
            assert_eq!(
                registry.native_surface(border.before),
                Some(NativeSurface::Tree)
            );
        }
    }

    #[test]
    fn a_cell_a_window_covers_is_no_border() {
        let registry = layout(Look::new(Panes::Nvim, true));
        assert_eq!(registry.border_between(10, 5), None);
        assert_eq!(registry.border_between(41, 5), None);
    }
}
