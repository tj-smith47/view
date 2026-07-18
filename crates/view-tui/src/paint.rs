//! The `Surface` -> ratatui compositor: style conversion plus z-order
//! layer painting. `view-surface`'s `render()` decides *what* to paint and
//! *where*; this module is the only place that turns those decisions into
//! `ratatui::Buffer` writes.

use ratatui::style::{Color, Style};
use view_core::grid::Grid;
pub use view_core::hl::{HlAttr, HlTable};
use view_core::model::Model;
use view_surface::{LayerKind, Rect, Surface};

/// Paints every layer in `surface`, in order (z ascending), into `frame`.
/// Later layers would overwrite the cells of earlier ones within their own
/// rect, which is the z-order compositing contract: nothing here tracks
/// damage or diffs between frames, since `ratatui`'s own buffer diff already
/// limits what actually reaches the terminal.
///
/// `model` supplies the engine grid and highlight table backing the
/// [`LayerKind::EngineGrid`] layer: `Surface` describes where to paint and
/// what kind of content goes there, not the grid's (potentially large)
/// per-cell content itself, so painting reads that straight from `model`
/// rather than cloning it into every frame's `Surface`.
///
/// Only [`LayerKind::EngineGrid`] is painted this phase. `nvim_ui_attach`
/// requests `ext_cmdline`/`ext_tabline`/`ext_messages`/`ext_popupmenu`
/// (`view-engine`'s `ui_attach`), which tells nvim the frontend owns those
/// surfaces entirely and stops nvim reserving grid rows for them; the grid
/// nvim sends back therefore already spans the *full* attached height, with
/// no row left free for chrome. Compositing a cmdline/tabline/messages/
/// popupmenu layer's text into the buffer would silently overwrite live
/// grid content in whatever row its rect claims (verified: the exact
/// corruption a real-nvim pty capture showed for the cmdline row). Painting
/// only the grid keeps every other `LayerKind` as real, tested `Surface`
/// data without a place to draw it yet; reserving terminal rows for chrome
/// outside the grid is a layout change this phase does not make.
pub fn composite(model: &Model, surface: &Surface, frame: &mut ratatui::Frame<'_>) {
    let frame_area = frame.area();
    for layer in &surface.layers {
        let area = clip_to_frame(layer.rect, frame_area);
        if area.width == 0 || area.height == 0 {
            continue;
        }
        if let LayerKind::EngineGrid = &layer.kind {
            paint_grid(&model.engine.grid, &model.engine.hl, area, frame);
        }
    }
}

/// Intersects a [`view_surface::Rect`] with the frame's own area: a layer
/// rect is clamped to the grid at render time, but the terminal can still
/// be smaller than that grid between a resize and the next `Surface`
/// (`view-surface` has no live handle on the terminal size), so painting
/// clips a second time against the actual paintable area rather than
/// indexing past the buffer.
fn clip_to_frame(rect: Rect, frame_area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let x = frame_area.x.saturating_add(rect.col);
    let y = frame_area.y.saturating_add(rect.row);
    let max_x = frame_area.x.saturating_add(frame_area.width);
    let max_y = frame_area.y.saturating_add(frame_area.height);
    ratatui::layout::Rect {
        x: x.min(max_x),
        y: y.min(max_y),
        width: rect.width.min(max_x.saturating_sub(x)),
        height: rect.height.min(max_y.saturating_sub(y)),
    }
}

/// Paints every visible `grid` cell within `area`, styled per `hl`.
fn paint_grid(
    grid: &Grid,
    hl: &HlTable,
    area: ratatui::layout::Rect,
    frame: &mut ratatui::Frame<'_>,
) {
    let buf = frame.buffer_mut();
    let (w, h) = grid.size();
    for row in 0..h.min(area.height) {
        for col in 0..w.min(area.width) {
            if let Some(cell) = grid.cell(row, col) {
                let out = &mut buf[(area.x + col, area.y + row)];
                out.set_symbol(if cell.text.is_empty() {
                    " "
                } else {
                    &cell.text
                });
                out.set_style(style_for(cell.hl_id, hl));
            }
        }
    }
}

fn style_for(hl_id: u64, table: &HlTable) -> Style {
    let mut fg = table.default_fg;
    let mut bg = table.default_bg;
    let mut style = Style::default();
    if let Some(a) = table.attrs.get(&hl_id) {
        if a.fg.is_some() {
            fg = a.fg;
        }
        if a.bg.is_some() {
            bg = a.bg;
        }
        if a.reverse {
            std::mem::swap(&mut fg, &mut bg);
        }
        if a.bold {
            style = style.add_modifier(ratatui::style::Modifier::BOLD);
        }
        if a.italic {
            style = style.add_modifier(ratatui::style::Modifier::ITALIC);
        }
        if a.underline {
            style = style.add_modifier(ratatui::style::Modifier::UNDERLINED);
        }
    }
    if let Some(c) = fg {
        style = style.fg(rgb(c));
    }
    if let Some(c) = bg {
        style = style.bg(rgb(c));
    }
    style
}

fn rgb(c: u32) -> Color {
    Color::Rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use view_core::grid::GridOp;

    fn table_with(attr: HlAttr) -> HlTable {
        let mut attrs = std::collections::HashMap::new();
        attrs.insert(1, attr);
        HlTable {
            default_fg: None,
            default_bg: None,
            attrs,
        }
    }

    #[test]
    fn underline_attr_sets_underlined_modifier() {
        let table = table_with(HlAttr {
            fg: None,
            bg: None,
            bold: false,
            italic: false,
            underline: true,
            reverse: false,
        });
        let style = style_for(1, &table);
        assert!(style
            .add_modifier
            .contains(ratatui::style::Modifier::UNDERLINED));
    }

    #[test]
    fn no_underline_attr_leaves_modifier_unset() {
        let table = table_with(HlAttr {
            fg: None,
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            reverse: false,
        });
        let style = style_for(1, &table);
        assert!(!style
            .add_modifier
            .contains(ratatui::style::Modifier::UNDERLINED));
    }

    /// Golden test: the grid layer's own content paints correctly.
    #[test]
    fn composite_paints_grid_layer_content() {
        let mut model = Model::new();
        model.engine.grid.apply(GridOp::Resize {
            width: 10,
            height: 3,
        });
        model.engine.grid.apply(GridOp::PutLine {
            row: 0,
            col_start: 0,
            cells: vec![("h".into(), 0, 1), ("i".into(), 0, 1)],
        });
        let surface = view_surface::render(&model);

        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();

        let buf = terminal.backend().buffer().clone();
        assert_eq!(&buf[(0, 0)].symbol(), &"h");
        assert_eq!(&buf[(1, 0)].symbol(), &"i");
    }

    /// Regression test: `nvim_ui_attach` requests `ext_cmdline`, so nvim
    /// never reserves a grid row for it and the grid spans the full
    /// attached height. A cmdline layer sharing that same bottom row must
    /// not overwrite the grid's own content there (an earlier version of
    /// this compositor did exactly that, confirmed against a real-nvim pty
    /// capture: typed text vanished the instant the cmdline layer painted
    /// over the row it lived on).
    #[test]
    fn composite_does_not_let_a_cmdline_layer_corrupt_the_grids_bottom_row() {
        let mut model = Model::new();
        model.engine.grid.apply(GridOp::Resize {
            width: 10,
            height: 3,
        });
        model.engine.grid.apply(GridOp::PutLine {
            row: 2,
            col_start: 0,
            cells: vec![("x".into(), 0, 1), ("y".into(), 0, 1)],
        });
        let _ = view_core::update::update(
            &mut model,
            view_core::msg::Msg::Redraw(vec![view_core::events::UiEvent::CmdlineShow {
                content: vec![(0, "q".to_string())],
                pos: 0,
                firstc: ":".to_string(),
                prompt: String::new(),
                indent: 0,
                level: 1,
            }]),
        );
        let surface = view_surface::render(&model);
        assert_eq!(
            surface.layers.len(),
            2,
            "expected a cmdline layer above the grid"
        );

        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();

        let buf = terminal.backend().buffer().clone();
        assert_eq!(&buf[(0, 2)].symbol(), &"x");
        assert_eq!(&buf[(1, 2)].symbol(), &"y");
    }
}
