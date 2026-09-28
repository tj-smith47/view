//! The frame a restart holds on screen, and the reads the compositor paints
//! through so that it shows the dead engine's last frame until the
//! replacement puts a window up, then the replacement's windows in the dead
//! engine's slots until the replacement's own layout has settled.

use super::EngineModel;
use crate::grid::registry::{GridRegistry, WindowSlot};
use crate::grid::Grid;
use crate::hl::HlTable;

/// How long the replacement's windows are drawn in the dead engine's slots
/// at most, from the first window it puts on screen. A config opens its
/// sidebars from startup autocmds a plugin manager runs after the attach,
/// which takes a few hundred milliseconds on a loaded config; a replacement
/// that never reopens one hands the screen back to its own layout here.
pub(crate) const RESTART_LAYOUT_HOLD: std::time::Duration = std::time::Duration::from_secs(2);

/// What a restart keeps on screen in place of the replacement's own frame.
#[derive(Debug, Clone, Default)]
pub(crate) enum Held {
    /// The replacement's own frame is on screen.
    #[default]
    Nothing,
    /// The registry the dead engine last painted and the highlight table
    /// its cells were drawn with, until the replacement puts a window on
    /// screen.
    ///
    /// The replacement's attach reaches that frame over several flushes, and
    /// the ones before it carry a cleared grid and no window, so painting
    /// the live registry through them paints an empty screen. Its first
    /// batch also redefines the highlight ids the held cells carry.
    Frame(GridRegistry, HlTable),
    /// The windows the dead engine last had on screen, and the
    /// replacement's registry drawn in them
    /// ([`GridRegistry::laid_out_as`]).
    ///
    /// The attach draws the file alone, and a config reopens its sidebars
    /// from its own startup autocmds after that, so painting the live
    /// registry puts the file across the whole width and then back. The
    /// cells are the replacement's from its first window: only the slots
    /// are the dead engine's.
    Layout {
        /// The dead engine's windows, as handle and slot.
        slots: Vec<WindowSlot>,
        /// The live registry drawn in `slots`.
        drawn: GridRegistry,
    },
}

impl EngineModel {
    /// The engine grid the screen shows, for the compositor: the dead
    /// engine's last one while a restart holds its frame.
    #[must_use]
    pub fn painted_grid(&self) -> &Grid {
        self.painted_grids().global()
    }

    /// Every grid and pane the screen shows, for the compositor: the dead
    /// engine's last registry while a restart holds its frame, the live one
    /// drawn in the dead engine's slots while it holds the layout, and
    /// [`Self::grids`] otherwise.
    #[must_use]
    #[inline]
    pub fn painted_grids(&self) -> &GridRegistry {
        match &self.held {
            Held::Nothing => &self.grids,
            Held::Frame(grids, _) => grids,
            Held::Layout { drawn, .. } => drawn,
        }
    }

    /// The highlight table the screen's cells are drawn with, for the
    /// compositor: the dead engine's while a restart holds its frame, and
    /// [`Self::hl`] otherwise.
    #[must_use]
    #[inline]
    pub fn painted_hl(&self) -> &HlTable {
        match &self.held {
            Held::Frame(_, hl) => hl,
            Held::Nothing | Held::Layout { .. } => &self.hl,
        }
    }

    /// Whether a restart keeps something other than the live registry on
    /// screen.
    #[must_use]
    pub fn holds_the_screen(&self) -> bool {
        !matches!(self.held, Held::Nothing)
    }

    /// Takes the frame on screen as the one a restart holds.
    pub(crate) fn hold_frame(&mut self) {
        // a failed attempt comes back here holding the frame already, and
        // the registry it would copy now is the empty one
        if !matches!(self.held, Held::Frame(..)) {
            self.held = Held::Frame(self.painted_grids().clone(), self.hl.clone());
        }
    }

    /// Moves the hold on at a flush: from the dead frame to the dead
    /// layout once the replacement puts a window up, and from the layout to
    /// the live registry once the replacement's own windows fill the same
    /// slots or one of them stands outside every slot. Answers whether the
    /// layout hold began here, which is when [`RESTART_LAYOUT_HOLD`] starts.
    pub(crate) fn settle_held(&mut self) -> bool {
        let mut began = false;
        if let Held::Frame(grids, _) = &self.held {
            if !self.grids.shows_a_window() {
                return false;
            }
            // a single-grid frame has no slots to hold
            let slots = grids.window_layout();
            began = !slots.is_empty();
            self.held = Held::Layout {
                slots,
                drawn: GridRegistry::new(),
            };
        }
        if let Held::Layout { slots, .. } = &self.held {
            let mut live: Vec<_> = self
                .grids
                .window_layout()
                .into_iter()
                .map(|w| w.1)
                .collect();
            let mut held: Vec<_> = slots.iter().map(|w| w.1).collect();
            live.sort_unstable();
            held.sort_unstable();
            self.held = match self.grids.laid_out_as(slots) {
                Some(drawn) if live != held => Held::Layout {
                    slots: slots.clone(),
                    drawn,
                },
                _ => Held::Nothing,
            };
        }
        began && matches!(self.held, Held::Layout { .. })
    }

    /// Hands the screen to the replacement's own layout.
    pub(crate) fn release_held_layout(&mut self) {
        if matches!(self.held, Held::Layout { .. }) {
            self.held = Held::Nothing;
        }
    }
}
