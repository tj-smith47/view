//! What the window layout looks like: tiles with their own frames, or the
//! picture nvim paints for itself.
//!
//! The arithmetic lives here rather than in the painter because three
//! places need the same answers and none of them can reach the others: the
//! spawn's geometry `--cmd`, the registry's inner-size requests, and the
//! compositor's frame.

/// How window layout is drawn.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panes {
    /// Every window is a tile with a frame of view's own, and the active
    /// one is marked in the accent colour.
    Tiles,
    /// nvim's own picture: its separator columns and status rows, with
    /// view's bottom bar under them.
    Nvim,
}

/// The whole look of the window layout.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Look {
    /// How window layout is drawn.
    pub panes: Panes,
    /// Whether a gap separates neighbouring frames. Gapless tiles share
    /// their edges, and `panes = Nvim` ignores this entirely.
    pub gaps: bool,
}

/// The narrowest slot a gapped frame fits in, on each axis. The height
/// axis is tested against this plus the window's top margin, since the
/// margin comes out of the request and a request of 0 is no request.
pub const MIN_FRAMED_SLOT: (u16, u16) = (5, 5);

/// What `panes = "auto"` answers on this session's environment, and the
/// environment variable that decided it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Detected {
    /// The mode auto resolves to, once a session has seeded it.
    pub panes: Option<Panes>,
    /// The marker that decided it, where one did.
    pub marker: Option<&'static str>,
}

impl Default for Look {
    fn default() -> Self {
        Self {
            panes: Panes::Nvim,
            gaps: true,
        }
    }
}

impl Look {
    /// A look from the two answers the `[ui]` table resolves.
    #[must_use]
    pub const fn new(panes: Panes, gaps: bool) -> Self {
        Self { panes, gaps }
    }

    /// Cells the outer ring takes off each axis: 0, 1 gapless, 2 gapped.
    #[must_use]
    pub const fn ring(self) -> u16 {
        match (self.panes, self.gaps) {
            (Panes::Nvim, _) => 0,
            (Panes::Tiles, false) => 1,
            (Panes::Tiles, true) => 2,
        }
    }

    /// Where the outer grid sits inside the terminal, on both axes.
    ///
    /// The ring takes one cell at the top and the left, and whatever else
    /// it costs is the margin on the right and the bottom. Everything
    /// mapping a terminal cell to a grid cell, or the other way, spends
    /// this one number.
    #[must_use]
    pub const fn grid_offset(self) -> u16 {
        // `ring() > 0` in a const fn: `u16::from(bool)` is not const
        match self.ring() {
            0 => 0,
            _ => 1,
        }
    }

    /// Slot origin to grid origin, as `(rows, cols)`.
    ///
    /// Gapless tiles fill their slots, so only a gapped one moves its grid
    /// inward -- one cell of gap and one of frame on each side.
    #[must_use]
    pub const fn inset(self) -> (u16, u16) {
        match (self.panes, self.gaps) {
            (Panes::Tiles, true) => (2, 2),
            _ => (0, 0),
        }
    }

    /// The `nvim_ui_try_resize_grid` size for a slot; `(0, 0)` for none.
    ///
    /// `margin_top` is the window's `win_viewport_margins` top, which nvim
    /// adds to whatever height is asked for, so it comes out of the request
    /// first. nvim reads a non-positive axis as no request at all and gives
    /// the window its whole slot back, which would put window text over the
    /// frame; a slot that cannot spare the ring on both axes is drawn bare
    /// instead.
    #[must_use]
    pub fn inner_request(self, slot: (u16, u16), margin_top: u16) -> (u16, u16) {
        let (inset_rows, inset_cols) = self.inset();
        if inset_rows == 0 && inset_cols == 0 {
            return (0, 0);
        }
        let (min_w, min_h) = MIN_FRAMED_SLOT;
        if slot.0 < min_w || slot.1 < min_h.saturating_add(margin_top) {
            return (0, 0);
        }
        (
            slot.0.saturating_sub(inset_cols * 2),
            slot.1
                .saturating_sub(inset_rows * 2)
                .saturating_sub(margin_top),
        )
    }

    /// Whether a slot of this size, with this top margin, carries a frame
    /// of view's own.
    #[must_use]
    pub fn frames(self, slot: (u16, u16), margin_top: u16) -> bool {
        match self.panes {
            Panes::Nvim => false,
            Panes::Tiles if self.gaps => self.inner_request(slot, margin_top) != (0, 0),
            Panes::Tiles => true,
        }
    }
}
