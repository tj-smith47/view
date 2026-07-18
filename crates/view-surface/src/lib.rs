//! The render model: what to draw, independent of any frontend.
//!
//! [`render`] turns a [`Model`] into a [`Surface`]: an ordered list of
//! [`Layer`]s plus the real terminal cursor's position and shape. Pure
//! data, no drawing here; `view-tui` is the only crate that turns a
//! `Surface` into pixels.

use view_core::events::saturate_u16;
use view_core::model::{CmdlineState, MessageEntry, Model, PopupmenuState, TablineState};

/// A rectangular region in terminal cells, addressed the same way as
/// [`view_core::grid::Grid`]: `(row, col)` is the top-left corner.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub row: u16,
    pub col: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    /// Clamps `self` to fit within a `bounds_width` x `bounds_height` grid:
    /// `row`/`col` are capped to the bounds first, then `width`/`height` are
    /// capped to whatever remains. A hostile rect (a huge row/col/width/
    /// height, as from an untrusted wire-derived value) always yields a
    /// rect fully inside the bounds rather than one that overflows past
    /// them or wraps through saturating arithmetic.
    #[must_use]
    pub fn clamp_to(self, bounds_width: u16, bounds_height: u16) -> Self {
        let row = self.row.min(bounds_height);
        let col = self.col.min(bounds_width);
        Self {
            row,
            col,
            width: self.width.min(bounds_width.saturating_sub(col)),
            height: self.height.min(bounds_height.saturating_sub(row)),
        }
    }
}

/// One region of the screen and what to paint into it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct Layer {
    pub rect: Rect,
    pub kind: LayerKind,
}

/// What a [`Layer`] paints.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum LayerKind {
    /// The embedded engine's grid, full-frame at z0.
    EngineGrid,
    /// The command line, present while nvim's command line is open.
    Cmdline(CmdlineState),
    /// The message log, present while it holds unshown-cleared entries.
    Messages(Vec<MessageEntry>),
    /// The open tabs, present once nvim has sent a `tabline_update`.
    Tabline(TablineState),
    /// The completion popup menu, present while it is open.
    Popupmenu(PopupmenuState),
}

/// The terminal cursor's shape, decoded from the active mode's
/// `mode_info_set` cursor style string. An empty or unrecognized style
/// string decodes to [`CursorShape::Block`], matching nvim's own fallback.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    /// Underline cursor; the `u8` is the cell height percentage.
    Horizontal(u8),
    /// Bar cursor; the `u8` is the cell width percentage.
    Vertical(u8),
}

/// The real terminal cursor: position plus shape. IME candidate windows and
/// screen readers key off the terminal's own cursor rather than anything
/// `view` paints into the grid, so this must always name a real cell when
/// the grid has one.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorSpec {
    pub row: u16,
    pub col: u16,
    pub shape: CursorShape,
}

/// What to paint: an ordered (z ascending) list of layers plus the real
/// terminal cursor spec.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct Surface {
    pub layers: Vec<Layer>,
    pub cursor: Option<CursorSpec>,
}

/// Fixed popup menu width in cells, pending real content-based sizing.
const POPUP_WIDTH: u16 = 30;

/// Builds the [`Surface`] for one frame from `model`.
///
/// Total: any `Model`, including a hostile or partially-initialized one,
/// yields a valid `Surface` whose layers never exceed the grid's current
/// bounds. Never panics.
#[must_use]
pub fn render(model: &Model) -> Surface {
    let engine = &model.engine;
    let (grid_w, grid_h) = engine.grid.size();

    let mut layers = vec![Layer {
        rect: Rect {
            row: 0,
            col: 0,
            width: grid_w,
            height: grid_h,
        },
        kind: LayerKind::EngineGrid,
    }];

    if let Some(tabline) = &engine.tabline {
        layers.push(overlay_layer(
            0,
            0,
            grid_w,
            1,
            (grid_w, grid_h),
            LayerKind::Tabline(tabline.clone()),
        ));
    }
    if let Some(cmdline) = &engine.cmdline {
        layers.push(overlay_layer(
            grid_h.saturating_sub(1),
            0,
            grid_w,
            1,
            (grid_w, grid_h),
            LayerKind::Cmdline(cmdline.clone()),
        ));
    }
    if !engine.messages.entries.is_empty() {
        layers.push(overlay_layer(
            grid_h.saturating_sub(1),
            0,
            grid_w,
            1,
            (grid_w, grid_h),
            LayerKind::Messages(engine.messages.entries.clone()),
        ));
    }
    if let Some(pm) = &engine.popupmenu {
        let row = saturate_u16(pm.row);
        let col = saturate_u16(pm.col);
        let height = u16::try_from(pm.items.len()).unwrap_or(u16::MAX).max(1);
        layers.push(overlay_layer(
            row,
            col,
            POPUP_WIDTH,
            height,
            (grid_w, grid_h),
            LayerKind::Popupmenu(pm.clone()),
        ));
    }

    Surface {
        layers,
        cursor: cursor_spec(model),
    }
}

/// Builds one overlay [`Layer`], clamping its rect to `bounds` so a hostile
/// or stale position/size from wire-derived state can never place a layer
/// outside the current grid.
fn overlay_layer(
    row: u16,
    col: u16,
    width: u16,
    height: u16,
    bounds: (u16, u16),
    kind: LayerKind,
) -> Layer {
    Layer {
        rect: Rect {
            row,
            col,
            width,
            height,
        }
        .clamp_to(bounds.0, bounds.1),
        kind,
    }
}

/// The real terminal cursor: the grid cursor position, shaped by the active
/// mode's cursor style. `None` when the grid has no cells to place it in
/// (a freshly started `Model` before the first resize).
fn cursor_spec(model: &Model) -> Option<CursorSpec> {
    let (width, height) = model.engine.grid.size();
    if width == 0 || height == 0 {
        return None;
    }
    let (row, col) = model.engine.grid.cursor();
    let shape = model
        .engine
        .mode
        .active_cursor()
        .map_or(CursorShape::Block, |info| {
            let pct = u8::try_from(info.cell_percentage).unwrap_or(u8::MAX);
            match info.cursor_shape.as_str() {
                "horizontal" => CursorShape::Horizontal(pct),
                "vertical" => CursorShape::Vertical(pct),
                _ => CursorShape::Block,
            }
        });
    Some(CursorSpec { row, col, shape })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use view_core::events::{ModeInfo, UiEvent};
    use view_core::grid::GridOp;
    use view_core::msg::Msg;
    use view_core::update::update;

    fn model_with_grid(width: u16, height: u16) -> Model {
        let mut model = Model::new();
        model.engine.grid.apply(GridOp::Resize { width, height });
        model
    }

    /// Non-exhaustive `view-core` state structs (`CmdlineState`,
    /// `TablineState`, `PopupmenuState`) cannot be built with struct-literal
    /// syntax from outside their defining crate, so tests drive them through
    /// the same `update()` path production code uses instead of
    /// constructing them directly.
    fn apply(model: &mut Model, ev: UiEvent) {
        let _ = update(model, Msg::Redraw(vec![ev]));
    }

    #[test]
    fn engine_only_model_renders_one_grid_layer_with_block_cursor_at_grid_cursor() {
        let mut model = model_with_grid(10, 5);
        model
            .engine
            .grid
            .apply(GridOp::CursorGoto { row: 2, col: 4 });

        let surface = render(&model);

        assert_eq!(surface.layers.len(), 1);
        assert_eq!(surface.layers[0].kind, LayerKind::EngineGrid);
        assert_eq!(
            surface.layers[0].rect,
            Rect {
                row: 0,
                col: 0,
                width: 10,
                height: 5
            }
        );
        assert_eq!(
            surface.cursor,
            Some(CursorSpec {
                row: 2,
                col: 4,
                shape: CursorShape::Block,
            })
        );
    }

    #[test]
    fn cmdline_state_renders_a_layer_above_the_grid() {
        let mut model = model_with_grid(20, 8);
        apply(
            &mut model,
            UiEvent::CmdlineShow {
                content: vec![(0, "hello".to_string())],
                pos: 5,
                firstc: ":".to_string(),
                prompt: String::new(),
                indent: 0,
                level: 1,
            },
        );

        let surface = render(&model);

        assert_eq!(surface.layers.len(), 2);
        assert_eq!(surface.layers[0].kind, LayerKind::EngineGrid);
        let LayerKind::Cmdline(state) = &surface.layers[1].kind else {
            unreachable!("expected Cmdline layer, got {:?}", surface.layers[1].kind);
        };
        assert_eq!(state.firstc, ":");
    }

    #[test]
    fn insert_mode_mode_info_yields_vertical_cursor_shape() {
        let mut model = model_with_grid(10, 5);
        model.engine.mode.modes = vec![ModeInfo {
            name: "insert".to_string(),
            short_name: "i".to_string(),
            cursor_shape: "vertical".to_string(),
            cell_percentage: 25,
            ..Default::default()
        }];
        model.engine.mode.current_idx = 0;

        let surface = render(&model);

        assert_eq!(
            surface.cursor.map(|c| c.shape),
            Some(CursorShape::Vertical(25))
        );
    }

    #[test]
    fn horizontal_mode_info_yields_horizontal_cursor_shape() {
        let mut model = model_with_grid(10, 5);
        model.engine.mode.modes = vec![ModeInfo {
            name: "replace".to_string(),
            short_name: "r".to_string(),
            cursor_shape: "horizontal".to_string(),
            cell_percentage: 20,
            ..Default::default()
        }];
        model.engine.mode.current_idx = 0;

        let surface = render(&model);

        assert_eq!(
            surface.cursor.map(|c| c.shape),
            Some(CursorShape::Horizontal(20))
        );
    }

    #[test]
    fn unrecognized_cursor_shape_string_falls_back_to_block() {
        let mut model = model_with_grid(10, 5);
        model.engine.mode.modes = vec![ModeInfo {
            cursor_shape: "unknown-shape".to_string(),
            ..Default::default()
        }];
        model.engine.mode.current_idx = 0;

        let surface = render(&model);

        assert_eq!(surface.cursor.map(|c| c.shape), Some(CursorShape::Block));
    }

    #[test]
    fn zero_sized_grid_yields_no_cursor() {
        let model = Model::new();
        let surface = render(&model);
        assert_eq!(surface.cursor, None);
        // still exactly one EngineGrid layer: render is total, never skips it
        assert_eq!(surface.layers.len(), 1);
        assert_eq!(surface.layers[0].kind, LayerKind::EngineGrid);
    }

    #[test]
    fn hostile_popupmenu_position_clamps_within_grid_bounds() {
        let mut model = model_with_grid(5, 3);
        apply(
            &mut model,
            UiEvent::PopupmenuShow {
                items: vec![
                    view_core::events::PmItem::default(),
                    view_core::events::PmItem::default(),
                ],
                selected: -1,
                row: u64::MAX,
                col: u64::MAX,
                grid: 0,
            },
        );

        // must not panic, and the resulting rect must fit inside the grid
        let surface = render(&model);

        let popup = surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::Popupmenu(_)))
            .expect("popupmenu layer present");
        assert!(popup.rect.row <= 3);
        assert!(popup.rect.col <= 5);
        assert!(popup.rect.row + popup.rect.height <= 3);
        assert!(popup.rect.col + popup.rect.width <= 5);
    }

    #[test]
    fn oversized_tabline_rect_clamps_to_grid_width() {
        // a grid narrower than a typical tabline row still yields a clamped,
        // in-bounds layer rather than one wider than the grid
        let mut model = model_with_grid(3, 4);
        apply(
            &mut model,
            UiEvent::TablineUpdate {
                current: view_core::events::TabHandle(0),
                tabs: vec![],
            },
        );

        let surface = render(&model);

        let tabline = surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::Tabline(_)))
            .expect("tabline layer present");
        assert!(tabline.rect.col + tabline.rect.width <= 3);
        assert!(tabline.rect.row + tabline.rect.height <= 4);
    }
}
