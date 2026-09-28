//! What the window layout looks like: tiles with their own frames, or the
//! picture nvim paints for itself.
//!
//! The arithmetic lives here, outside the painter, because three
//! places need the same answers and none of them can reach the others: the
//! spawn's geometry `--cmd`, the registry's inner-size requests, and the
//! compositor's frame.

use crate::native::geometry::OverlayRect;

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

/// The narrowest slot a gapped frame fits in, on each axis: the frame on
/// both sides and one cell of text between. The height axis is tested
/// against this plus the window's top margin, since the margin comes out
/// of the request and a request of 0 is no request.
pub const MIN_FRAMED_SLOT: (u16, u16) = (3, 3);

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

    /// Cells the outer ring takes off the width: 0 under nvim, 2 under
    /// either tiled look, one column on each side, so the rightmost tile
    /// has a column to close its frame on. The height loses only its top
    /// row, for the reason [`grid_target_for`](crate::model::grid_target_for)
    /// gives.
    #[must_use]
    pub const fn ring(self) -> u16 {
        match self.panes {
            Panes::Nvim => 0,
            Panes::Tiles => 2,
        }
    }

    /// Where the outer grid sits inside the terminal, on both axes.
    ///
    /// The ring takes one cell at the top and the left, and whatever else
    /// it costs is the margin on the right. The bottom margin is the bottom
    /// tiles' status rows, which the `laststatus = 2` hold under tiles
    /// keeps. Everything mapping a terminal cell to a grid cell, or the
    /// other way, spends this one number.
    #[must_use]
    pub const fn grid_offset(self) -> u16 {
        // `ring() > 0` in a const fn: `u16::from(bool)` is not const
        match self.ring() {
            0 => 0,
            _ => 1,
        }
    }

    /// Rows view's own bottom bar takes, given the `statusline` feature's
    /// switch.
    ///
    /// Under tiles each frame carries its own status segments in its
    /// bottom edge, so no row stands for a bar at all. The one answer both
    /// the spawn's geometry `--cmd` and [`Model::statusline_rows`] spend,
    /// since a spawn seeded a row taller than the attach relayouts every
    /// window on screen.
    ///
    /// [`Model::statusline_rows`]: crate::model::Model::statusline_rows
    #[must_use]
    pub const fn bar_rows(self, statusline_enabled: bool) -> u16 {
        match (self.panes, statusline_enabled) {
            (Panes::Nvim, true) => 1,
            _ => 0,
        }
    }

    /// Slot origin to grid origin, as `(rows, cols)`.
    ///
    /// Gapless tiles fill their slots, so only a gapped one moves its grid
    /// inward, by the one cell of frame on each side. The gap outside the
    /// frame is nvim's separator column and status row, which lie beyond
    /// the slot.
    #[must_use]
    pub const fn inset(self) -> (u16, u16) {
        match (self.panes, self.gaps) {
            (Panes::Tiles, true) => (1, 1),
            _ => (0, 0),
        }
    }

    /// The box a gapped tile's frame is drawn on, as `(row, col, width,
    /// height)`: the box handed in, itself. `None` for a gapless or
    /// `"nvim"` look, and for a box too small to hold the frame; a winbar's
    /// row is judged by [`Look::frames`].
    ///
    /// The frame painter and the edge text hand in a tile's
    /// [`Pane::filled`](crate::grid::registry::Pane::filled), the part of
    /// its slot the grid covers, and read this box back. The windowed
    /// palette band and every overlay
    /// [`Model::overlay_rect`](crate::model::Model::overlay_rect) resolves
    /// inset one ring where [`Look::inset`] is non-zero, which is exactly
    /// where this answers `Some`, so their borders land on the same rows
    /// and columns.
    #[must_use]
    pub const fn frame_box(self, slot: (u16, u16, u16, u16)) -> Option<(u16, u16, u16, u16)> {
        match (self.panes, self.gaps) {
            (Panes::Tiles, true) if slot.2 >= MIN_FRAMED_SLOT.0 && slot.3 >= MIN_FRAMED_SLOT.1 => {
                Some(slot)
            }
            _ => None,
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

    /// The grid rows a tile's frame edges stand on, for a window nvim
    /// placed in `slot`.
    ///
    /// A gapped tile has two: the slot's own top and bottom rows. A
    /// gapless tile has one, the lattice row under the slot, which this
    /// answers twice.
    ///
    /// Read by the painter's own damage, without the refusals
    /// [`Look::frames`] makes for a slot too small to carry a frame: a row
    /// repainted where nothing is drawn costs a frame of work, and one
    /// skipped leaves a stale reading on screen.
    #[must_use]
    pub fn edge_rows(self, slot: (u16, u16, u16, u16)) -> [u16; 2] {
        let (row, _, _, height) = slot;
        if self.gaps {
            [row, row.saturating_add(height).saturating_sub(1)]
        } else {
            [row.saturating_add(height); 2]
        }
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

impl super::Model {
    /// The cells `overlay`'s frame covers on the current terminal: its box,
    /// less the gutter [`Self::overlay_gutter`] names.
    #[must_use]
    pub fn overlay_rect(&self, overlay: &super::Overlay) -> OverlayRect {
        self.overlay_split(overlay).0
    }

    /// The blank column between an edge-anchored overlay's frame and the
    /// tiles it covers under gapped tiles, or `None` where the frame takes
    /// its whole box.
    #[must_use]
    pub fn overlay_gutter(&self, overlay: &super::Overlay) -> Option<OverlayRect> {
        self.overlay_split(overlay).1
    }

    fn overlay_split(&self, overlay: &super::Overlay) -> (OverlayRect, Option<OverlayRect>) {
        let full = self.overlay_box(overlay);
        // a gapless frame shares its edge with the tile beside it, which is
        // the lattice's own rule for two tiles
        if self.look.inset() == (0, 0) {
            return (full, None);
        }
        full.split_gutter(overlay.geometry.anchor)
    }
}

/// `overlay`'s box on a `term_w` by `band_h` band, grown to the rows a
/// modal's wrapped message needs. The message wraps at the width the share
/// gives, so those rows are only known once that width is.
///
/// A modal holds the keyboard, so one whose rows outgrow the band is
/// placed on the whole `term_h` instead, over the rows around the band, and
/// the second answer says so. A modal box too short for its frame and a
/// row inside it is one row tall, which is drawn unframed.
pub(super) fn grown_rect(
    overlay: &super::Overlay,
    term_w: u16,
    band_h: u16,
    term_h: u16,
) -> (OverlayRect, bool) {
    let view = match &overlay.kind {
        super::OverlayKind::Prompt(state) => state.view(),
        super::OverlayKind::EngineBusy(state) => state.view(),
        _ => return (overlay.geometry.rect(term_w, band_h), false),
    };
    let width = overlay.geometry.rect(term_w, band_h).width;
    let inner = crate::native::geometry::interior_text_width(width);
    let needed = view.rows_at(inner).saturating_add(2);
    let grown = overlay.geometry.with_min_height(needed);
    let in_band = grown.rect(term_w, band_h);
    let (rect, whole) = if in_band.height < needed && band_h < term_h {
        (grown.rect(term_w, term_h), true)
    } else {
        (in_band, false)
    };
    if rect.height >= 3 {
        return (rect, whole);
    }
    let short = OverlayRect {
        row: rect.row.saturating_add(rect.height.saturating_sub(1) / 2),
        height: rect.height.min(1),
        ..rect
    };
    (short, whole)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every slot size a gapped tile can be given, with and without a
    /// winbar: the frame is the slot's own outer ring, the grid sits one
    /// cell inside it, and a right-hand neighbour placed past nvim's
    /// separator column leaves exactly that one column between the two
    /// frames. The same holds downward, with the status row as the gap.
    #[test]
    fn every_slot_size_frames_one_cell_in_and_leaves_one_cell_between() {
        let look = Look::new(Panes::Tiles, true);
        assert_eq!(look.inset(), (1, 1));
        for w in 1..=40u16 {
            for h in 1..=40u16 {
                for m in 0..=1u16 {
                    let slot = (4, 6, w, h);
                    let fits = w >= 3 && h >= 3 + m;
                    let request = look.inner_request((w, h), m);
                    if fits {
                        assert_eq!(request, (w - 2, h - 2 - m), "slot {w}x{h} m={m}");
                        assert_eq!(look.frame_box(slot), Some(slot), "slot {w}x{h}");
                        assert_eq!(look.edge_rows(slot), [4, 4 + h - 1], "slot {w}x{h}");
                    } else {
                        assert_eq!(request, (0, 0), "slot {w}x{h} m={m} is too small");
                    }
                    assert_eq!(look.frames((w, h), m), fits, "slot {w}x{h} m={m}");
                    if w >= 3 && h >= 3 {
                        // nvim's separator column is `6 + w` and the status
                        // row `4 + h`: the frame's right column and bottom
                        // row are the cells just before them
                        let far = look.frame_box(slot).map(|b| (b.1 + b.2 - 1, b.0 + b.3 - 1));
                        assert_eq!(far, Some((6 + w - 1, 4 + h - 1)), "slot {w}x{h}");
                    }
                }
            }
        }
        let (min_w, min_h) = MIN_FRAMED_SLOT;
        assert_eq!((min_w, min_h), (3, 3));
        assert!(!look.frames((min_w - 1, min_h), 0));
        assert!(!look.frames((min_w, min_h - 1), 0));
        assert!(look.frames((min_w, min_h), 0));
    }

    /// A modal whose wrapped question needs more rows than its share gets
    /// them, so its choices stay on screen.
    #[test]
    fn a_prompt_box_grows_to_the_rows_its_wrapped_question_needs() {
        let mut model = crate::model::Model::with_term_size(60, 24);
        let state = crate::native::prompt::PromptState::ai_trust_prompt(
            std::path::PathBuf::from("/p"),
            "open".to_string(),
            "Trust /home/someone/work/a-project-with-a-long-name to launch an AI \
             agent? Agents can read and write files in this project."
                .to_string(),
        );
        let share = state.overlay_box().rect(60, 24).height;
        let view = state.view();
        model.push_overlay(
            state.overlay_box(),
            super::super::OverlayKind::Prompt(state),
        );
        assert_eq!(model.overlays.len(), 1, "the prompt is open");
        for overlay in &model.overlays {
            let rect = model.overlay_rect(overlay);
            let inner = crate::native::geometry::interior_text_width(rect.width);
            let needed = view.rows_at(inner) + 2;
            assert!(needed > share, "the question outgrows the share");
            assert_eq!(rect.height, needed, "the box holds every row");
        }
    }

    /// A Prompt and an EngineBusy modal on a 60-column terminal, each
    /// with the rows its every row needs framed.
    fn modals() -> Vec<(super::super::Overlay, u16)> {
        use crate::native::supervision::{EngineBusyState, SinceStamp, WedgeKind};
        let mut model = crate::model::Model::with_term_size(60, 24);
        let prompt = crate::native::prompt::PromptState::ai_trust_prompt(
            std::path::PathBuf::from("/p"),
            "open".to_string(),
            "Trust /home/someone/work/a-project to launch an AI agent?".to_string(),
        );
        model.push_overlay(
            prompt.overlay_box(),
            super::super::OverlayKind::Prompt(prompt),
        );
        model.push_overlay(
            crate::native::geometry::OverlayBox::new(60, 30),
            super::super::OverlayKind::EngineBusy(EngineBusyState::new(
                WedgeKind::ReadSide,
                SinceStamp::default(),
            )),
        );
        model
            .overlays
            .iter()
            .map(|overlay| {
                let view = match &overlay.kind {
                    super::super::OverlayKind::Prompt(state) => state.view(),
                    super::super::OverlayKind::EngineBusy(state) => state.view(),
                    _ => unreachable!("only modals are pushed"),
                };
                let width = overlay.geometry.rect(60, 24).width;
                let inner = crate::native::geometry::interior_text_width(width);
                (overlay.clone(), view.rows_at(inner) + 2)
            })
            .collect()
    }

    /// A band one row short of every row a modal needs, the rule under its
    /// input line included, still places the modal on the whole terminal:
    /// the rule sets the input line apart from a selected choice, so it is
    /// a row the modal needs.
    #[test]
    fn a_band_one_row_short_of_the_rule_covers_the_bars_since_the_rule_is_required() {
        for (overlay, needed) in modals() {
            let band_h = needed - 1;
            let (rect, whole) = grown_rect(&overlay, 60, band_h, band_h + 2);
            assert!(whole, "{:?} short of its rule", overlay.kind);
            assert_eq!((rect.row, rect.height), (0, needed), "{:?}", overlay.kind);
            let (_, whole) = grown_rect(&overlay, 60, needed, needed + 2);
            assert!(!whole, "{:?} with every row in the band", overlay.kind);
        }
    }

    /// Every band a modal can be given on every terminal up to three rows
    /// taller: it leaves the band exactly when the band is short of its
    /// rows and the terminal has more, stays on the terminal, and is one
    /// unframed row when fewer than three rows are left for it.
    #[test]
    fn a_modal_leaves_the_band_only_when_short_and_stays_on_the_terminal() {
        for (overlay, needed) in modals() {
            for band_h in 0..=needed + 2 {
                for term_h in band_h..=band_h + 3 {
                    let at = format!("{:?} band {band_h} term {term_h}", overlay.kind);
                    let (rect, whole) = grown_rect(&overlay, 60, band_h, term_h);
                    let expect_whole = needed > band_h && band_h < term_h;
                    assert_eq!(whole, expect_whole, "{at}");
                    let bound = if whole { term_h } else { band_h };
                    assert!(rect.row + rect.height <= bound, "{at}: {rect:?}");
                    let framed = overlay
                        .geometry
                        .with_min_height(needed)
                        .rect(60, bound)
                        .height;
                    let height = if framed >= 3 { framed } else { framed.min(1) };
                    assert_eq!(rect.height, height, "{at}");
                    if bound >= needed {
                        assert!(rect.height >= needed, "{at}: every row shown");
                    }
                }
            }
        }
    }

    /// Gapless tiles and nvim's own picture have no frame box and move no
    /// grid.
    #[test]
    fn only_gapped_tiles_have_a_frame_box() {
        for look in [Look::new(Panes::Tiles, false), Look::new(Panes::Nvim, true)] {
            assert_eq!(look.inset(), (0, 0));
            assert_eq!(look.frame_box((0, 0, 40, 20)), None);
        }
    }
}
