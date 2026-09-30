//! A border being dragged, the resize mode's own state, and the one setter
//! each sidebar's share goes through.

use crate::events::WinHandle;
use crate::grid::registry::{Border, BorderAxis};
use crate::msg::WinSplit;
use crate::native::geometry::{self, Anchor, NativeSurface};

/// What a border drag or a resize-mode step resizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resized {
    /// An nvim window, whose size nvim lays out by its own rules.
    Window(WinHandle),
    /// A sidebar's share of the screen, in percent.
    Sidebar(NativeSurface),
}

/// A border drag in flight: what it resizes, along which axis, and where
/// the press started it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorderGrip {
    /// What the drag resizes.
    pub resized: Resized,
    /// Which way the border runs.
    pub axis: BorderAxis,
    /// The terminal column the press landed on, or its row for a border
    /// between rows.
    pub pressed_at: u16,
    /// The size at the press: cells for a window, percent for a sidebar.
    pub start: u16,
    /// The cells one percent of a sidebar's share is taken of, `0` for a
    /// window, which is sized in cells.
    pub basis: u16,
    /// Whether moving the border right, or down, grows what it resizes.
    pub grows_forward: bool,
    /// The whole-cell distance from the press the last resize answered.
    pub moved: i32,
}

impl BorderGrip {
    /// The size a border moved `delta` cells from the press asks for, in the
    /// unit [`Self::start`] is in, or `None` while `delta` is the distance
    /// already answered.
    #[must_use]
    pub fn size_at(&self, delta: i32) -> Option<u16> {
        if delta == self.moved {
            return None;
        }
        let growth = if self.grows_forward { delta } else { -delta };
        let step = match self.resized {
            Resized::Window(_) => growth,
            Resized::Sidebar(_) => {
                let basis = i32::from(self.basis.max(1));
                let scaled = growth.saturating_mul(100);
                (scaled + scaled.signum() * basis / 2) / basis
            }
        };
        let size = i32::from(self.start).saturating_add(step).max(1);
        Some(u16::try_from(size).unwrap_or(u16::MAX))
    }
}

/// The resize mode while it holds the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeMode {
    /// The sidebar the mode resizes, or `None` for nvim's current window.
    pub sidebar: Option<NativeSurface>,
    /// The count typed ahead of the next step, `0` while none is.
    pub count: u32,
}

impl super::Model {
    /// The resize mode, while it holds the keyboard.
    #[must_use]
    pub fn resize_mode(&self) -> Option<ResizeMode> {
        self.resize_mode
    }

    /// Sets the resize mode, and the status word that shows it.
    pub(crate) fn set_resize_mode(&mut self, mode: Option<ResizeMode>) {
        self.resize_mode = mode;
        self.engine.statusline.set_resizing(mode.is_some());
        self.dirty = true;
    }

    /// `surface`'s share of the screen, in percent.
    #[must_use]
    pub fn sidebar_share(&self, surface: NativeSurface) -> u16 {
        match surface {
            NativeSurface::Tree => self.tree_width_pct,
            NativeSurface::Agent => self.ai_panel_width_pct,
            NativeSurface::Notifications | NativeSurface::Palette => {
                self.surfaces.layout(surface).size
            }
        }
    }

    /// The axis `surface`'s share is measured along: columns for a sidebar
    /// on the left or right, rows for the stream pinned to the top or
    /// bottom.
    #[must_use]
    pub fn sidebar_axis(&self, surface: NativeSurface) -> BorderAxis {
        match surface {
            NativeSurface::Tree | NativeSurface::Agent => BorderAxis::Columns,
            NativeSurface::Notifications | NativeSurface::Palette => {
                if WinSplit::for_anchor(self.surfaces.layout(surface).anchor).is_vertical() {
                    BorderAxis::Columns
                } else {
                    BorderAxis::Rows
                }
            }
        }
    }

    /// The grip a press on `border` starts, `pressed_at` being the press's
    /// terminal column, or its row for a border between rows.
    ///
    /// A windowed sidebar on either side moves its own share. Two windows
    /// move the one before the border, in cells.
    #[must_use]
    pub fn border_grip(&self, border: Border, pressed_at: u16) -> Option<BorderGrip> {
        let grids = self.engine.painted_grids();
        let (columns, rows) = grids.global().size();
        let basis = match border.axis {
            BorderAxis::Columns => columns,
            BorderAxis::Rows => rows,
        };
        let sidebar = |grid| {
            grids
                .native_surface(grid)
                .filter(|&surface| self.sidebar_axis(surface) == border.axis)
        };
        let (resized, start, basis, grows_forward) = if let Some(surface) = sidebar(border.before) {
            (
                Resized::Sidebar(surface),
                self.sidebar_share(surface),
                basis,
                true,
            )
        } else if let Some(surface) = sidebar(border.after) {
            (
                Resized::Sidebar(surface),
                self.sidebar_share(surface),
                basis,
                false,
            )
        } else {
            let win = grids.window_handle(border.before)?;
            (Resized::Window(win), border.span, 0, true)
        };
        Some(BorderGrip {
            resized,
            axis: border.axis,
            pressed_at,
            start,
            basis,
            grows_forward,
            moved: 0,
        })
    }

    /// The grip a press on a sidebar float's inner edge starts: the frame
    /// column facing the tiles, or the gutter beside it. `None` for any
    /// other terminal cell.
    #[must_use]
    pub fn sidebar_edge(&self, row: u16, col: u16) -> Option<BorderGrip> {
        let id = self.overlay_at(row, col)?;
        let overlay = self.overlays.iter().find(|overlay| overlay.id == id)?;
        let surface = match overlay.kind {
            super::OverlayKind::Tree(_) => NativeSurface::Tree,
            super::OverlayKind::Ai => NativeSurface::Agent,
            _ => return None,
        };
        let grows_forward = match overlay.geometry.anchor {
            Anchor::Left => true,
            Anchor::Right => false,
            _ => return None,
        };
        let frame = self.overlay_rect(overlay);
        let edge = if grows_forward {
            frame.col.saturating_add(frame.width).saturating_sub(1)
        } else {
            frame.col
        };
        let in_gutter = self
            .overlay_gutter(overlay)
            .is_some_and(|gutter| gutter.contains(row, col));
        if col != edge && !in_gutter {
            return None;
        }
        Some(BorderGrip {
            resized: Resized::Sidebar(surface),
            axis: BorderAxis::Columns,
            pressed_at: col,
            start: self.sidebar_share(surface),
            basis: self
                .term_width
                .saturating_sub(self.look.inset().1.saturating_mul(2)),
            grows_forward,
            moved: 0,
        })
    }

    /// Sets the tree's width, clamped to the sidebar range, reporting
    /// whether it moved.
    pub(crate) fn set_tree_width(&mut self, pct: u16) -> bool {
        let next = geometry::clamp_panel_width(i64::from(pct));
        let moved = next != self.tree_width_pct;
        self.tree_width_pct = next;
        self.rewidth(|kind| matches!(kind, super::OverlayKind::Tree(_)), next);
        moved
    }

    /// Sets the agent panel's width, on the same terms as
    /// [`Self::set_tree_width`].
    pub(crate) fn set_ai_panel_width(&mut self, pct: u16) -> bool {
        let next = geometry::clamp_panel_width(i64::from(pct));
        let moved = next != self.ai_panel_width_pct;
        self.ai_panel_width_pct = next;
        self.rewidth(|kind| matches!(kind, super::OverlayKind::Ai), next);
        moved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grip(resized: Resized, grows_forward: bool, basis: u16) -> BorderGrip {
        BorderGrip {
            resized,
            axis: BorderAxis::Columns,
            pressed_at: 40,
            start: 30,
            basis,
            grows_forward,
            moved: 0,
        }
    }

    #[test]
    fn a_window_grows_by_the_cells_the_border_moved() {
        let window = grip(Resized::Window(WinHandle(1000)), true, 0);
        assert_eq!(window.size_at(0), None, "no move, no resize");
        assert_eq!(window.size_at(3), Some(33));
        assert_eq!(window.size_at(-40), Some(1), "a window keeps one cell");
    }

    #[test]
    fn a_sidebar_moves_by_the_share_the_cells_make() {
        let left = grip(Resized::Sidebar(NativeSurface::Tree), true, 100);
        assert_eq!(left.size_at(10), Some(40));
        let right = grip(Resized::Sidebar(NativeSurface::Agent), false, 200);
        assert_eq!(
            right.size_at(10),
            Some(25),
            "right of the tiles, moving right narrows"
        );
        assert_eq!(
            right.size_at(-3),
            Some(32),
            "1.5 percent rounds away from zero"
        );
    }
}
