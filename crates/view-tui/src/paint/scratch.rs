//! Per-frame scratch state a plain frame's paint reuses across frames: the
//! pane list, the highlight-style memo, and grid 1's own damage.

use ratatui::style::Style;
use view_core::grid::registry::Pane;
use view_core::model::Model;
use view_core::theme::Theme;

use super::{style_for, Damage, HlTable};

/// The buffers one composite reuses from the frame before it, so a plain
/// frame's paint allocates nothing once they have grown to the session's
/// shape.
#[derive(Debug, Default)]
pub(super) struct PaintScratch {
    /// The engine's panes in z order, built once per frame and read by both
    /// the damage pass and the compositor.
    pub(super) panes: Vec<Pane>,
    /// Whether [`Self::panes`] was built for the frame being composed.
    pub(super) panes_fresh: bool,
    pub(super) styles: StyleCache,
    /// The global grid's own damage, with the rows a window covers taken
    /// out.
    pub(super) global: Damage,
}

impl PaintScratch {
    pub(super) fn build_panes(&mut self, model: &Model) {
        model
            .engine
            .painted_grids()
            .panes_in_z_order_into(&mut self.panes);
        self.panes_fresh = true;
    }
}

/// Highest `hl_id` the per-frame dense style cache will hold. nvim
/// allocates highlight ids as small dense integers, so real frames sit
/// far below this; an id past the cap (or a pathological huge id) simply
/// resolves uncached rather than growing an unbounded table.
const STYLE_CACHE_CAP: usize = 4096;

/// A per-frame memo of `hl_id -> ratatui::Style`, indexed directly by id.
/// `Theme::style_for` costs a `HashMap` probe per call, and a full-grid
/// composite makes one call per cell (4800 on a 120x40 frame) out of only
/// a handful of distinct ids; resolving each id once per frame removes
/// the probe from the per-cell path entirely. Emptied at the start of
/// every pane, so there is no invalidation to get wrong when the highlight
/// table or theme changes between frames or between one pane's theme and
/// the next; the table's capacity is what carries over.
#[derive(Debug, Default)]
pub(super) struct StyleCache {
    pub(super) dense: Vec<Option<Style>>,
}

impl StyleCache {
    pub(super) fn reset(&mut self) {
        self.dense.clear();
    }

    pub(super) fn get(&mut self, theme: &Theme, hl: &HlTable, hl_id: u64) -> Style {
        let Ok(index) = usize::try_from(hl_id) else {
            return style_for(theme, hl_id, hl);
        };
        if index >= STYLE_CACHE_CAP {
            return style_for(theme, hl_id, hl);
        }
        if self.dense.len() <= index {
            self.dense.resize(index + 1, None);
        }
        if let Some(style) = self.dense[index] {
            return style;
        }
        let style = style_for(theme, hl_id, hl);
        self.dense[index] = Some(style);
        style
    }
}
