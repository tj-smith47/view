//! Where a native overlay sits on the terminal.
//!
//! Placement is stored as a share of the terminal rather than as absolute
//! cells, so an overlay keeps its proportions across a resize with nothing
//! recomputing and re-storing a rect that a missed resize could leave
//! stale. The single conversion from share to cells is [`OverlayBox::rect`];
//! routing a click and painting a frame both resolve through it, so the
//! rect a click is tested against is always the rect on screen.

/// A native overlay's placement: a share of the terminal, centered in it.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayBox {
    /// Share of the terminal's width, in percent.
    pub width_pct: u16,
    /// Share of the terminal's height, in percent.
    pub height_pct: u16,
}

impl OverlayBox {
    /// A box covering `width_pct` by `height_pct` of the terminal.
    ///
    /// Shares above 100 are clamped to the terminal instead of rejected: an
    /// overlay asking for more room than exists is a placement question with
    /// an obvious answer, not an error a user could act on.
    #[must_use]
    pub fn new(width_pct: u16, height_pct: u16) -> Self {
        Self {
            width_pct: width_pct.min(100),
            height_pct: height_pct.min(100),
        }
    }

    /// The absolute cell rect this box covers on a `term_w` by `term_h`
    /// terminal, centered and rounded down.
    ///
    /// A terminal too small to spare a whole cell yields a zero-sized rect,
    /// which contains no point and therefore claims no input.
    #[must_use]
    pub fn rect(&self, term_w: u16, term_h: u16) -> OverlayRect {
        let width = share(term_w, self.width_pct);
        let height = share(term_h, self.height_pct);
        OverlayRect {
            row: term_h.saturating_sub(height) / 2,
            col: term_w.saturating_sub(width) / 2,
            width,
            height,
        }
    }
}

/// The share of `extent` that `pct` percent covers, rounded down.
fn share(extent: u16, pct: u16) -> u16 {
    // u32 because the product of two u16 extents overflows u16 long before
    // the divide brings it back into range
    let cells = u32::from(extent) * u32::from(pct.min(100)) / 100;
    u16::try_from(cells).unwrap_or(extent)
}

/// An overlay's absolute position on the terminal, in cells.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayRect {
    /// Topmost terminal row the overlay covers.
    pub row: u16,
    /// Leftmost terminal column the overlay covers.
    pub col: u16,
    /// Width in cells.
    pub width: u16,
    /// Height in cells.
    pub height: u16,
}

impl OverlayRect {
    /// Whether the terminal cell at `(row, col)` falls inside this rect.
    #[must_use]
    pub fn contains(&self, row: u16, col: u16) -> bool {
        row >= self.row
            && col >= self.col
            && row < self.row.saturating_add(self.height)
            && col < self.col.saturating_add(self.width)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_full_share_covers_the_whole_terminal_from_the_origin() {
        let r = OverlayBox::new(100, 100).rect(80, 24);
        assert_eq!((r.row, r.col, r.width, r.height), (0, 0, 80, 24));
        assert!(r.contains(0, 0));
        assert!(r.contains(23, 79));
        assert!(!r.contains(24, 0));
        assert!(!r.contains(0, 80));
    }

    #[test]
    fn a_half_share_is_centered_with_the_remainder_split_evenly() {
        let r = OverlayBox::new(50, 50).rect(80, 24);
        assert_eq!((r.width, r.height), (40, 12));
        assert_eq!((r.row, r.col), (6, 20));
        assert!(r.contains(6, 20), "top-left corner is inside");
        assert!(r.contains(17, 59), "bottom-right corner is inside");
        assert!(!r.contains(5, 20), "the row above is outside");
        assert!(!r.contains(6, 19), "the column to the left is outside");
        assert!(!r.contains(18, 20), "the row below is outside");
        assert!(!r.contains(6, 60), "the column to the right is outside");
    }

    #[test]
    fn a_share_over_a_hundred_percent_is_clamped_to_the_terminal() {
        let clamped = OverlayBox::new(400, 900);
        assert_eq!((clamped.width_pct, clamped.height_pct), (100, 100));
        let r = clamped.rect(30, 10);
        assert_eq!((r.row, r.col, r.width, r.height), (0, 0, 30, 10));
    }

    #[test]
    fn a_terminal_too_small_for_a_whole_cell_claims_no_input() {
        let r = OverlayBox::new(10, 10).rect(5, 5);
        assert_eq!((r.width, r.height), (0, 0));
        assert!(!r.contains(0, 0), "a zero-sized rect contains nothing");
        assert!(!r.contains(r.row, r.col));
    }

    #[test]
    fn a_zero_sized_terminal_yields_a_rect_that_contains_nothing() {
        let r = OverlayBox::new(100, 100).rect(0, 0);
        assert_eq!((r.row, r.col, r.width, r.height), (0, 0, 0, 0));
        assert!(!r.contains(0, 0));
    }

    #[test]
    fn the_widest_terminal_does_not_overflow_the_share_arithmetic() {
        let r = OverlayBox::new(100, 100).rect(u16::MAX, u16::MAX);
        assert_eq!((r.width, r.height), (u16::MAX, u16::MAX));
        assert!(r.contains(u16::MAX - 1, u16::MAX - 1));
    }
}
