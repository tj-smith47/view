//! Pure `Surface` + `Grid` -> text conversion, with no `view-tui` (and so no
//! crossterm/ratatui) dependency: the oracle's Msg-level and engine-attached
//! drivers script against plain strings, not terminal cells, and
//! `scripts/audit-deps.sh`'s crossterm/ratatui reach checks would fail the
//! moment this crate took a normal dependency on `view-tui` to reuse its
//! real painter instead.
//!
//! Not a pixel-accurate reproduction of `view-tui`'s own rendering (no
//! styling, no clipping subtleties beyond what `view_surface::render`
//! already clamped): the oracle only needs enough fidelity for test scripts
//! to assert on buffer content, cursor position, and which overlay is
//! showing, layered in the same z-order `view_surface::render` builds.

use view_core::grid::Grid;
use view_surface::{Layer, LayerKind, Surface};

const SHELL_PLACEHOLDER: &str = "waiting for nvim";

/// Renders `surface` to a plain-text screen dump: one newline-joined row per
/// canvas line, each row exactly as wide as the canvas (short overlay text
/// is left-aligned and space-padded, never truncated mid-run). The canvas
/// size is the union of every layer's rect (`row + height`, `col + width`)
/// rather than a caller-supplied width/height, so this stays a pure
/// function of `surface` and `grid` alone. Later layers (`surface.layers`'
/// z-ascending order, the same order [`view_surface::render`] builds) paint
/// over earlier ones, matching the real terminal painter's stacking.
#[must_use]
pub fn screen_text(surface: &Surface, grid: &Grid) -> String {
    let (width, height) = canvas_size(surface);
    if width == 0 || height == 0 {
        return String::new();
    }
    let mut canvas = vec![vec![' '; usize::from(width)]; usize::from(height)];
    for layer in &surface.layers {
        paint_layer(&mut canvas, layer, grid);
    }
    canvas
        .into_iter()
        .map(|row| row.into_iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn canvas_size(surface: &Surface) -> (u16, u16) {
    let mut width = 0u16;
    let mut height = 0u16;
    for layer in &surface.layers {
        width = width.max(layer.rect.col.saturating_add(layer.rect.width));
        height = height.max(layer.rect.row.saturating_add(layer.rect.height));
    }
    (width, height)
}

/// Writes `text` into `canvas` starting at `(row, col)`, one `char` per
/// cell, silently stopping at the canvas edge rather than wrapping or
/// panicking: every caller here already derived `canvas`'s size from the
/// same layers it is about to paint, but a defensive bound keeps this total
/// even if a future overlay's content legitimately overflows its own rect.
fn paint_text(canvas: &mut [Vec<char>], row: u16, col: u16, text: &str) {
    let Some(row_cells) = canvas.get_mut(usize::from(row)) else {
        return;
    };
    for (c, ch) in (usize::from(col)..).zip(text.chars()) {
        let Some(slot) = row_cells.get_mut(c) else {
            break;
        };
        *slot = ch;
    }
}

fn joined_content(content: &[(u64, String)]) -> String {
    content.iter().map(|(_, text)| text.as_str()).collect()
}

fn paint_layer(canvas: &mut [Vec<char>], layer: &Layer, grid: &Grid) {
    match &layer.kind {
        LayerKind::EngineGrid => paint_grid(canvas, layer, grid),
        LayerKind::Shell => paint_text(canvas, layer.rect.row, layer.rect.col, SHELL_PLACEHOLDER),
        LayerKind::Tabline(state) => paint_tabline(canvas, layer, state),
        LayerKind::Cmdline(state) => paint_cmdline(canvas, layer, state),
        LayerKind::Messages(entries) => paint_messages(canvas, layer, entries),
        LayerKind::Popupmenu(state) => paint_popupmenu(canvas, layer, state),
        // LayerKind is #[non_exhaustive]: a future variant paints nothing
        // here until this raster gains explicit support for it, rather
        // than failing to compile against it
        _ => {}
    }
}

fn paint_grid(canvas: &mut [Vec<char>], layer: &Layer, grid: &Grid) {
    let (_, grid_h) = grid.size();
    for r in 0..grid_h.min(layer.rect.height) {
        paint_text(
            canvas,
            layer.rect.row.saturating_add(r),
            layer.rect.col,
            &grid.row_text(r),
        );
    }
}

fn paint_tabline(canvas: &mut [Vec<char>], layer: &Layer, state: &view_core::model::TablineState) {
    let text = state
        .tabs
        .iter()
        .map(|t| t.name.as_str())
        .collect::<Vec<_>>()
        .join(" | ");
    paint_text(canvas, layer.rect.row, layer.rect.col, &text);
}

fn paint_cmdline(canvas: &mut [Vec<char>], layer: &Layer, state: &view_core::model::CmdlineState) {
    // claims the full row before writing content: without this, cells
    // beyond the prompt+content's length still show whatever the grid layer
    // beneath painted at this row (e.g. a statusline), since `paint_text`
    // only overwrites as many cells as `text` occupies
    blank_row(canvas, layer.rect.row, layer.rect.col, layer.rect.width);
    let text = format!(
        "{}{}{}",
        state.firstc,
        state.prompt,
        joined_content(&state.content)
    );
    paint_text(canvas, layer.rect.row, layer.rect.col, &text);
}

/// Overwrites `width` cells of `row` starting at `col` with spaces, the
/// row-claim primitive [`paint_cmdline`] uses to blank the row before
/// writing its own content.
fn blank_row(canvas: &mut [Vec<char>], row: u16, col: u16, width: u16) {
    let Some(row_cells) = canvas.get_mut(usize::from(row)) else {
        return;
    };
    let len = row_cells.len();
    let start = usize::from(col).min(len);
    let end = usize::from(col.saturating_add(width)).min(len);
    for slot in &mut row_cells[start..end] {
        *slot = ' ';
    }
}

/// `lines` is already the exact visible set `Messages::visible_lines`
/// selected -- one physical line per row, in display order -- so this only
/// has to blank each row (mirroring the real painter's own toast-box clear;
/// without it a row's cells past a shorter line's text would keep showing
/// whatever an earlier layer painted there) and write each line.
fn paint_messages(canvas: &mut [Vec<char>], layer: &Layer, lines: &[String]) {
    for r in 0..layer.rect.height {
        blank_row(canvas, layer.rect.row + r, layer.rect.col, layer.rect.width);
    }
    for (i, line) in lines.iter().enumerate() {
        let Ok(r) = u16::try_from(i) else { break };
        paint_text(
            canvas,
            layer.rect.row.saturating_add(r),
            layer.rect.col,
            line,
        );
    }
}

fn paint_popupmenu(
    canvas: &mut [Vec<char>],
    layer: &Layer,
    state: &view_core::model::PopupmenuState,
) {
    for (i, item) in state.items.iter().enumerate() {
        let Ok(r) = u16::try_from(i) else { break };
        paint_text(
            canvas,
            layer.rect.row.saturating_add(r),
            layer.rect.col,
            &item.display_text(),
        );
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use view_core::events::UiEvent;
    use view_core::grid::GridOp;
    use view_core::model::Model;
    use view_core::msg::Msg;
    use view_core::update::update;

    fn model_with_grid(width: u16, height: u16) -> Model {
        let mut model = Model::new();
        model.engine.grid.apply(GridOp::Resize { width, height });
        model
    }

    fn apply(model: &mut Model, ev: UiEvent) {
        let _ = update(model, Msg::Redraw(vec![ev]));
    }

    #[test]
    fn plain_grid_renders_row_text_joined_by_newlines() {
        let mut model = model_with_grid(5, 2);
        model.engine.grid.apply(GridOp::PutLine {
            row: 0,
            col_start: 0,
            cells: vec![("h".into(), 0, 1), ("i".into(), 0, 1)],
        });
        let surface = view_surface::render(&model);

        let text = screen_text(&surface, &model.engine.grid);

        assert_eq!(text, "hi   \n     ");
    }

    #[test]
    fn empty_grid_renders_empty_string() {
        let model = Model::new();
        let surface = view_surface::render(&model);
        assert_eq!(screen_text(&surface, &model.engine.grid), "");
    }

    #[test]
    fn cmdline_overlay_paints_over_the_bottom_row() {
        let mut model = model_with_grid(20, 3);
        apply(
            &mut model,
            UiEvent::CmdlineShow {
                content: vec![(0, "wq".to_string())],
                pos: 2,
                firstc: ":".to_string(),
                prompt: String::new(),
                indent: 0,
                level: 1,
            },
        );
        let surface = view_surface::render(&model);

        let text = screen_text(&surface, &model.engine.grid);

        let last_row = text.lines().next_back().unwrap();
        assert!(
            last_row.starts_with(":wq"),
            "expected cmdline text at start of bottom row, got {last_row:?}"
        );
    }

    /// While the cmdline is active, it must own the full bottom row: no
    /// glyph from whatever the grid painted at that row (a statusline, in
    /// this reproduction of the reported bug) may survive past the
    /// cmdline's own prompt+content text. Disconfirm: without the blank-row
    /// claim in `paint_cmdline`, the tail of the row still reads the grid's
    /// `NvimTree_1 [-] ... COMMAND ...`-shaped text instead of spaces.
    #[test]
    fn cmdline_overlay_claims_the_full_bottom_row() {
        let mut model = model_with_grid(20, 3);
        model.engine.grid.apply(GridOp::PutLine {
            row: 2,
            col_start: 0,
            cells: "NvimTree_1 [-] COMMAND"
                .chars()
                .map(|c| (c.to_string(), 0, 1))
                .collect(),
        });
        apply(
            &mut model,
            UiEvent::CmdlineShow {
                content: vec![(0, "wq".to_string())],
                pos: 2,
                firstc: ":".to_string(),
                prompt: String::new(),
                indent: 0,
                level: 1,
            },
        );
        let surface = view_surface::render(&model);

        let text = screen_text(&surface, &model.engine.grid);

        let last_row = text.lines().next_back().unwrap();
        assert_eq!(
            last_row, ":wq                 ",
            "cmdline overlay must blank the whole row, not just where it wrote text"
        );
    }

    #[test]
    fn messages_overlay_paints_entry_text() {
        let mut model = model_with_grid(20, 5);
        apply(
            &mut model,
            UiEvent::MsgShow {
                kind: "echomsg".to_string(),
                content: vec![(0, "hi".to_string())],
                replace_last: false,
            },
        );
        let surface = view_surface::render(&model);

        let text = screen_text(&surface, &model.engine.grid);

        assert!(
            text.lines().any(|l| l.trim_end().ends_with("hi")),
            "expected a row ending in the message text; screen:\n{text}"
        );
    }
}
