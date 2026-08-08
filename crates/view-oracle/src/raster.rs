//! Pure `Surface` + `Grid` -> text conversion, with no `view-tui` (and so no
//! crossterm/ratatui) dependency: the oracle's Msg-level and engine-attached
//! drivers script against plain strings, not terminal cells, and
//! `scripts/audit-deps.sh`'s crossterm/ratatui reach checks would fail the
//! moment this crate took a normal dependency on `view-tui` to reuse its
//! real painter instead.
//!
//! Not a pixel-accurate reproduction of `view-tui`'s own rendering:
//! styling is compared at the model layer (`attr_rows` resolves each
//! cell's `hl_id` through the `HlTable` -- see the `attr` module's
//! coverage-boundary note -- never through `view-tui`'s painted output),
//! and no clipping subtleties exist beyond what `view_surface::render`
//! already clamped. The oracle only needs enough fidelity for test
//! scripts to assert on buffer content, cursor position, highlight
//! identity, and which overlay is showing, layered in the same z-order
//! `view_surface::render` builds.

use std::borrow::Cow;

use view_core::grid::Grid;
use view_core::hl::HlTable;
use view_surface::{Layer, LayerKind, Surface, SHELL_PLACEHOLDER};

use crate::attr::{row_fingerprint, ResolvedAttr};

/// Renders `surface` to a plain-text screen dump: one newline-joined row per
/// canvas line, each row the concatenation of the canvas's cells (short
/// overlay text is left-aligned and space-padded, never truncated mid-run).
/// Built on [`screen_rows`]; kept as a separate entry point since most
/// callers (test assertions, printed diagnostics) want one string, not a
/// `Vec`.
#[must_use]
pub fn screen_text(surface: &Surface, grid: &Grid) -> String {
    screen_rows(surface, grid).join("\n")
}

/// Renders `surface` to one `String` per canvas row, in row order --
/// [`screen_text`]'s per-row split, exposed directly so a row index (e.g.
/// from [`crate::masked_rows`]) lines up with an element index without a
/// caller having to re-split a joined string back apart. The canvas size is
/// the union of every layer's rect (`row + height`, `col + width`) rather
/// than a caller-supplied width/height, so this stays a pure function of
/// `surface` and `grid` alone. Later layers (`surface.layers`'
/// z-ascending order, the same order [`view_surface::render`] builds) paint
/// over earlier ones, matching the real terminal painter's stacking.
///
/// A canvas column holds a whole grid cell's text, not one `char`, so a row
/// is as many *cells* wide as the canvas and only as many chars wide as its
/// content needs. That is what nvim's wire actually carries: a double-width
/// grapheme arrives as one cell holding the glyph plus a cell whose text is
/// empty for the column it covers, and a grapheme cluster arrives as several
/// chars in one cell. Concatenating a char per column instead would shift
/// every glyph after a wide one and give the row a different length from the
/// reference side's [`crate::ReferenceSession::screen_rows`], which
/// concatenates the same cells -- turning any wide glyph into a divergence,
/// and padding the two back to equal width would instead hide a real one.
#[must_use]
pub fn screen_rows(surface: &Surface, grid: &Grid) -> Vec<String> {
    let (width, height) = canvas_size(surface);
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let mut canvas = vec![vec![Cow::Borrowed(" "); usize::from(width)]; usize::from(height)];
    for layer in &surface.layers {
        paint_layer(&mut canvas, layer, grid);
    }
    canvas.into_iter().map(|row| row.concat()).collect()
}

/// One canvas row: a cell's worth of text per column. Borrowed from the grid
/// (or from the blank prefill) wherever nothing has to be built, so a screen
/// dump allocates only for the overlay text it splits into chars.
type Canvas<'a> = [Vec<Cow<'a, str>>];

/// Renders each canvas row's per-cell highlight identity to one string,
/// row-indexed to line up cell-for-cell with [`screen_rows`]: the attr-parity
/// counterpart of the text dump, so [`crate::compare`] can diff a
/// [`crate::attr::ResolvedAttr`] fingerprint per cell alongside the glyph.
/// Only the `EngineGrid` layer contributes -- overlay layers
/// (cmdline/messages/tabline/popupmenu) carry no grid `hl_id` of their own and
/// their rows are excluded by [`crate::masked_rows`] anyway -- so every other
/// canvas row renders empty, holding its index slot the same way
/// [`crate::ReferenceSession::screen_rows`]'s chrome placeholders do. Each
/// cell's `hl_id` is resolved through `hl` into the semantic attributes it
/// stands for (never the raw per-session id: see [`crate::attr`]'s docs).
#[must_use]
pub fn attr_rows(surface: &Surface, grid: &Grid, hl: &HlTable) -> Vec<String> {
    let (width, height) = canvas_size(surface);
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let mut rows = vec![String::new(); usize::from(height)];
    for layer in &surface.layers {
        if matches!(layer.kind, LayerKind::EngineGrid) {
            attr_grid(&mut rows, layer, grid, hl);
        }
    }
    rows
}

/// Writes the `EngineGrid` layer's per-cell attr fingerprints into `rows` at
/// the same canvas offset [`paint_grid`] paints the grid's text into, so the
/// attr row for a grid line and its glyph row share a canvas index.
fn attr_grid(rows: &mut [String], layer: &Layer, grid: &Grid, hl: &HlTable) {
    let (grid_w, grid_h) = grid.size();
    for r in 0..grid_h.min(layer.rect.height) {
        let canvas_row = layer.rect.row.saturating_add(r);
        let Some(slot) = rows.get_mut(usize::from(canvas_row)) else {
            continue;
        };
        *slot = row_fingerprint((0..grid_w).map(|c| {
            grid.cell(r, c)
                .map_or(ResolvedAttr::DEFAULT, |cell| resolve_attr(hl, cell.hl_id))
        }));
    }
}

/// Resolves one grid cell's `hl_id` through `hl` into its
/// [`ResolvedAttr`], falling back to [`ResolvedAttr::DEFAULT`] for `hl_id` 0
/// and for any id not yet defined -- the same fallback the reference side
/// applies, so an undefined id can never itself be a divergence.
fn resolve_attr(hl: &HlTable, hl_id: u64) -> ResolvedAttr {
    hl.attr(hl_id).map_or(ResolvedAttr::DEFAULT, Into::into)
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
///
/// A char per cell rather than a cell's worth of text per cell, because an
/// overlay's text arrives as a string with no cell structure of its own,
/// unlike the grid (see [`paint_grid`]). Every overlay row is masked out of
/// the parity comparison, so the two sides never have to agree on how one is
/// split.
fn paint_text(canvas: &mut Canvas<'_>, row: u16, col: u16, text: &str) {
    let Some(row_cells) = canvas.get_mut(usize::from(row)) else {
        return;
    };
    for (c, ch) in (usize::from(col)..).zip(text.chars()) {
        let Some(slot) = row_cells.get_mut(c) else {
            break;
        };
        *slot = Cow::Owned(ch.to_string());
    }
}

fn joined_content(content: &[(u64, String)]) -> String {
    content.iter().map(|(_, text)| text.as_str()).collect()
}

fn paint_layer<'a>(canvas: &mut Canvas<'a>, layer: &Layer, grid: &'a Grid) {
    match &layer.kind {
        LayerKind::EngineGrid => paint_grid(canvas, layer, grid),
        // vertically centred, matching view-tui's paint_shell; the layer's
        // own row would put it at the top of the frame, where the real
        // painter never writes it. The painter's other output, a statusline
        // bar of spaces on the bottom row, is styling with no text and so
        // has nothing to represent in a plain-text raster.
        LayerKind::Shell => paint_text(
            canvas,
            layer.rect.row + layer.rect.height / 2,
            layer.rect.col,
            SHELL_PLACEHOLDER,
        ),
        LayerKind::Tabline(state) => paint_tabline(canvas, layer, state),
        LayerKind::Cmdline(state) => paint_cmdline(canvas, layer, state),
        LayerKind::Messages(entries) => paint_messages(canvas, layer, entries),
        LayerKind::Popupmenu(state) => paint_popupmenu(canvas, layer, state),
        LayerKind::Picker(_)
        | LayerKind::Tree(_)
        | LayerKind::Statusline(_)
        | LayerKind::Prompt(_)
        | LayerKind::Palette(_) => paint_native_overlay(canvas, layer),
        // LayerKind is #[non_exhaustive]: a future variant paints nothing
        // here until this raster gains explicit support for it, rather
        // than failing to compile against it
        _ => {}
    }
}

/// Writes a framed native overlay's rows into `canvas`.
///
/// The rows themselves come from `view_surface::overlay::rows`, the same
/// layout pass the terminal painter blits, so this raster reproduces a
/// native overlay exactly rather than approximating it the way the
/// tabline/cmdline arms above approximate nvim's own chrome. That is what
/// makes a screen dump of one usable as a golden: a border glyph, a title,
/// a scroll window, or a selection marker that changed in the layout shows
/// up here without this module being taught about it.
///
/// A layer with no border charset is not a framed overlay at all and paints
/// nothing, matching what the layout pass itself returns for one.
fn paint_native_overlay(canvas: &mut Canvas<'_>, layer: &Layer) {
    let Some(borders) = layer.borders else {
        return;
    };
    let rows =
        view_surface::overlay::rows(layer.rect.width, layer.rect.height, &layer.kind, borders);
    for (r, line) in rows.lines.iter().enumerate() {
        let Ok(r) = u16::try_from(r) else { break };
        paint_text(
            canvas,
            layer.rect.row.saturating_add(r),
            layer.rect.col,
            &view_surface::overlay::line_text(line),
        );
    }
}

/// Writes the grid's cells into `canvas` one cell per column, so a canvas
/// column and a grid column are the same thing and the row concatenates to
/// exactly what the reference side's `row_text` concatenates.
fn paint_grid<'a>(canvas: &mut Canvas<'a>, layer: &Layer, grid: &'a Grid) {
    let (grid_w, grid_h) = grid.size();
    for r in 0..grid_h.min(layer.rect.height) {
        let Some(row_cells) = canvas.get_mut(usize::from(layer.rect.row.saturating_add(r))) else {
            continue;
        };
        for c in 0..grid_w {
            let Some(slot) = row_cells.get_mut(usize::from(layer.rect.col.saturating_add(c)))
            else {
                break;
            };
            if let Some(cell) = grid.cell(r, c) {
                *slot = Cow::Borrowed(cell.text.as_str());
            }
        }
    }
}

fn paint_tabline(canvas: &mut Canvas<'_>, layer: &Layer, state: &view_core::model::TablineState) {
    let text = state
        .tabs
        .iter()
        .map(|t| t.name.as_str())
        .collect::<Vec<_>>()
        .join(" | ");
    paint_text(canvas, layer.rect.row, layer.rect.col, &text);
}

fn paint_cmdline(canvas: &mut Canvas<'_>, layer: &Layer, state: &view_core::model::CmdlineState) {
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
fn blank_row(canvas: &mut Canvas<'_>, row: u16, col: u16, width: u16) {
    let Some(row_cells) = canvas.get_mut(usize::from(row)) else {
        return;
    };
    let len = row_cells.len();
    let start = usize::from(col).min(len);
    let end = usize::from(col.saturating_add(width)).min(len);
    for slot in &mut row_cells[start..end] {
        *slot = Cow::Borrowed(" ");
    }
}

/// `lines` is already the exact visible set `Messages::visible_lines`
/// selected -- one physical line per row, in display order -- so this only
/// has to blank each row (mirroring the real painter's own toast-box clear;
/// without it a row's cells past a shorter line's text would keep showing
/// whatever an earlier layer painted there) and write each line.
fn paint_messages(
    canvas: &mut Canvas<'_>,
    layer: &Layer,
    lines: &[Vec<view_core::native::views::Span>],
) {
    for r in 0..layer.rect.height {
        blank_row(canvas, layer.rect.row + r, layer.rect.col, layer.rect.width);
    }
    for (i, spans) in lines.iter().enumerate() {
        let Ok(r) = u16::try_from(i) else { break };
        paint_text(
            canvas,
            layer.rect.row.saturating_add(r),
            layer.rect.col,
            &view_surface::overlay::line_text(spans),
        );
    }
}

fn paint_popupmenu(
    canvas: &mut Canvas<'_>,
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
        model.engine.apply_grid(GridOp::Resize { width, height });
        model
    }

    fn apply(model: &mut Model, ev: UiEvent) {
        let _ = update(model, Msg::Redraw(vec![ev]));
    }

    #[test]
    fn plain_grid_renders_row_text_joined_by_newlines() {
        let mut model = model_with_grid(5, 2);
        model.engine.apply_grid(GridOp::PutLine {
            row: 0,
            col_start: 0,
            cells: vec![("h".into(), 0, 1), ("i".into(), 0, 1)],
        });
        let surface = view_surface::render(&model);

        let text = screen_text(&surface, model.engine.grid());

        assert_eq!(text, "hi   \n     ");
    }

    /// A double-width glyph arrives from nvim as one cell holding it plus a
    /// cell whose text is empty for the column it covers, so a canvas column
    /// must hold a cell's text rather than a `char`. Getting this wrong is
    /// invisible on ASCII and shifts every glyph after the first wide one,
    /// which the reference side (concatenating the same cells) would then
    /// report as a divergence on content that actually matches.
    ///
    /// Disconfirm: writing the row's concatenated text one char per canvas
    /// column instead makes this render `"a界b  "`, five chars with `b` at
    /// column 2 rather than the four the cells hold.
    #[test]
    fn a_wide_glyphs_covered_column_contributes_no_char_to_its_row() {
        let mut model = model_with_grid(5, 1);
        model.engine.apply_grid(GridOp::PutLine {
            row: 0,
            col_start: 0,
            // exactly what the wire carries: glyph, then the covered
            // column's empty cell
            cells: vec![
                ("a".into(), 0, 1),
                ("界".into(), 0, 1),
                (String::new(), 0, 1),
                ("b".into(), 0, 1),
            ],
        });
        let surface = view_surface::render(&model);

        assert_eq!(screen_text(&surface, model.engine.grid()), "a界b ");
    }

    #[test]
    fn empty_grid_renders_empty_string() {
        let model = Model::new();
        let surface = view_surface::render(&model);
        assert_eq!(screen_text(&surface, model.engine.grid()), "");
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

        let text = screen_text(&surface, model.engine.grid());

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
        model.engine.apply_grid(GridOp::PutLine {
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

        let text = screen_text(&surface, model.engine.grid());

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

        let text = screen_text(&surface, model.engine.grid());

        assert!(
            text.lines().any(|l| l.trim_end().ends_with("hi")),
            "expected a row ending in the message text; screen:\n{text}"
        );
    }

    /// The reference raster's shell arm must place the same text on the same
    /// row as `view-tui`'s `paint_shell`, or the two sides disagree about a
    /// frame neither can currently be compared on. Both halves had already
    /// drifted, silently: the raster carried its own `"waiting for nvim"`
    /// literal against the painter's `"view: waiting for nvim..."`, and put
    /// it on the layer's own row (0) against the painter's vertical centre.
    ///
    /// Disconfirm: painting at `layer.rect.row` instead puts the placeholder
    /// on row 0 and leaves row 2 blank, failing both assertions.
    #[test]
    fn the_startup_shell_rasters_where_the_real_painter_puts_it() {
        let mut model = Model::with_term_size(40, 5);
        model.content_painted = false;
        let surface = view_surface::render(&model);

        let rows = screen_rows(&surface, model.engine.grid());

        assert_eq!(
            rows.len(),
            5,
            "the shell layer sizes the canvas to the terminal even with no grid; rows:\n{rows:#?}"
        );
        assert!(
            rows[2].starts_with(SHELL_PLACEHOLDER),
            "expected the placeholder at column 0 of the vertically centred row; rows:\n{rows:#?}"
        );
        assert!(
            rows[0].trim().is_empty(),
            "the painter writes nothing to the top row; rows:\n{rows:#?}"
        );
    }
}
