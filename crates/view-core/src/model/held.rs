//! The frame a restart holds on screen, and the reads the compositor paints
//! through so that it shows the dead engine's last frame until the
//! replacement puts a window up.

use super::EngineModel;
use crate::grid::registry::GridRegistry;
use crate::grid::Grid;
use crate::hl::HlTable;

impl EngineModel {
    /// The engine grid the screen shows, for the compositor: the dead
    /// engine's last one while a restart holds its frame.
    #[must_use]
    pub fn painted_grid(&self) -> &Grid {
        self.painted_grids().global()
    }

    /// Every grid and pane the screen shows, for the compositor: the dead
    /// engine's last registry while a restart holds its frame, and
    /// [`Self::grids`] otherwise.
    #[must_use]
    #[inline]
    pub fn painted_grids(&self) -> &GridRegistry {
        self.held_frame
            .as_ref()
            .map_or(&self.grids, |(grids, _)| grids)
    }

    /// The highlight table the screen's cells are drawn with, for the
    /// compositor: the dead engine's while a restart holds its frame, and
    /// [`Self::hl`] otherwise.
    #[must_use]
    #[inline]
    pub fn painted_hl(&self) -> &HlTable {
        self.held_frame.as_ref().map_or(&self.hl, |(_, hl)| hl)
    }

    /// Hands the screen back to the replacement once it has put a window
    /// on screen.
    pub(crate) fn release_held_frame(&mut self) {
        // the scan runs only on a held frame: a single-grid answer reads
        // every cell
        self.held_frame = self
            .held_frame
            .take()
            .filter(|_| !self.grids.shows_a_window());
    }
}
