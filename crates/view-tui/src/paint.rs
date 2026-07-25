//! The `Surface` -> ratatui compositor: style conversion plus z-order
//! layer painting. `view-surface`'s `render()` decides *what* to paint and
//! *where*; this module is the only place that turns those decisions into
//! `ratatui::Buffer` writes.

use ratatui::backend::Backend;
use ratatui::buffer::{Buffer, Cell, CellWidth};
use ratatui::style::{Color, Modifier, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use view_core::grid::{Grid, GridDamage};
pub use view_core::hl::{HlAttr, HlTable};
use view_core::model::{CmdlineState, Model, PopupmenuState, TablineState};
use view_core::theme::{ResolvedStyle, Theme};
use view_surface::{LayerKind, Rect, Surface};

/// The terminal-space rows a frame's composite must repaint, so a redraw
/// touches only the changed region instead of all ~4800 cells.
///
/// Rows are in terminal (post-chrome-offset) space, the same space
/// [`ratatui::buffer::Buffer`] indexes, so [`composite_into`] can test a
/// grid row's painted position directly. `full` supersedes `rows`: a
/// first paint, resize, or chrome-offset change repaints everything. The
/// grid layer is the only one clipped; transient overlays (cmdline,
/// messages, popupmenu, tabline, shell) are small and always painted whole
/// when present, and their rows are always included in a non-full
/// `Damage` (see [`Damage::from_frame`]) so the grid underneath a vacated
/// overlay repaints.
#[derive(Debug, Clone)]
pub struct Damage {
    full: bool,
    rows: Vec<u16>,
}

impl Default for Damage {
    /// Repaints every row, not none of them. A damage nobody chose is a
    /// damage nobody computed, and the two candidate meanings are not
    /// symmetric: repainting a clean row wastes composite CPU for one
    /// frame, while skipping a dirty one leaves the terminal showing
    /// something the model no longer says.
    fn default() -> Self {
        Self::full()
    }
}

impl Damage {
    /// Damage that repaints every row.
    #[must_use]
    pub fn full() -> Self {
        Self {
            full: true,
            rows: Vec::new(),
        }
    }

    /// Builds a frame's damage from the grid's own changed rows plus the
    /// overlay rows of this frame and the last, offsetting grid-space rows
    /// by the reserved chrome rows to reach terminal space.
    ///
    /// The union with both frames' overlay rows is what makes an overlay
    /// transition correct by construction: a toast that appears, moves,
    /// shrinks, or vanishes has every cell it now covers *and* every cell it
    /// covered last frame marked dirty, so the grid (or the new overlay
    /// position) repaints underneath the vacated cells. `force_full` (a
    /// chrome-offset change that shifts the whole grid) and a full
    /// [`GridDamage`] (a resize or clear) both collapse to a whole-frame
    /// repaint.
    #[must_use]
    pub fn from_frame(
        grid: &GridDamage,
        offset: u16,
        prev_overlay_rows: &[u16],
        cur_overlay_rows: &[u16],
        force_full: bool,
    ) -> Self {
        if force_full || grid.full {
            return Self::full();
        }
        let mut rows =
            Vec::with_capacity(grid.rows.len() + prev_overlay_rows.len() + cur_overlay_rows.len());
        rows.extend(grid.rows.iter().map(|&r| r.saturating_add(offset)));
        rows.extend_from_slice(prev_overlay_rows);
        rows.extend_from_slice(cur_overlay_rows);
        Self { full: false, rows }
    }

    /// Whether terminal-space `row` must repaint (always true when `full`).
    #[must_use]
    pub fn covers(&self, row: u16) -> bool {
        self.full || self.rows.contains(&row)
    }

    /// The damage covering every row either input covers.
    #[must_use]
    pub fn union(&self, other: &Self) -> Self {
        if self.full || other.full {
            return Self::full();
        }
        let mut rows = Vec::with_capacity(self.rows.len() + other.rows.len());
        rows.extend_from_slice(&self.rows);
        rows.extend(other.rows.iter().filter(|r| !self.rows.contains(r)));
        Self { full: false, rows }
    }

    /// Replaces `out` with the contiguous runs of rows this damage covers
    /// inside `area`, each a `(first row, row count)` pair in terminal space
    /// and in ascending row order.
    ///
    /// Runs rather than individual rows because a run is what
    /// [`ratatui::buffer::Buffer::diff_iter`] can be handed as one
    /// sub-buffer: the rows of a run are adjacent in the buffer's flat cell
    /// array, so one diff covers all of them. `out` is caller-owned so the
    /// per-frame call reuses a single allocation.
    fn row_runs(&self, area: ratatui::layout::Rect, out: &mut Vec<(u16, u16)>) {
        out.clear();
        let mut open: Option<(u16, u16)> = None;
        for offset in 0..area.height {
            let row = area.y.saturating_add(offset);
            if self.covers(row) {
                match &mut open {
                    Some((_, height)) => *height = height.saturating_add(1),
                    None => open = Some((row, 1)),
                }
            } else if let Some(run) = open.take() {
                out.push(run);
            }
        }
        if let Some(run) = open {
            out.push(run);
        }
    }
}

/// Whether diffing only `runs` costs less than diffing all `height` rows.
///
/// Staging a run swaps its cells out of the shadow's buffers and back
/// again, measured at 0.37 us per repainted row of a 120-column frame
/// against the 0.71 us the unclipped scan spends on that same row, so
/// clipping stops paying once roughly two thirds of the frame repaints.
/// Half the frame sits on the conservative side of that crossover: a large
/// repaint (a scroll, a resize) takes the unclipped path rather than paying
/// the swap on top of a scan it cannot avoid. The two paths emit the same
/// updates, so this only ever chooses which one is cheaper.
fn clipping_pays(runs: &[(u16, u16)], height: u16) -> bool {
    let rows: u32 = runs.iter().map(|&(_, h)| u32::from(h)).sum();
    rows.saturating_mul(2) < u32::from(height)
}

/// The double-buffered shadow of the terminal's cells: one buffer holding
/// what the terminal currently shows, one to composite the next frame into.
///
/// Exists to make a damage-clipped frame emit exactly the cells a full
/// recomposite would, without either of the two costs a naive clip pays.
///
/// The buffers swap rather than copy. A frame composites into `back`, the
/// diff against `front` is emitted, and the two then trade places, so no
/// per-frame buffer copy runs at all -- the same trick `ratatui::Terminal`
/// plays, which is why the pre-clipping paint path never paid for one.
/// Swapping means `back` holds the frame *before* last on entry, not last,
/// so a frame must repaint its own damaged rows plus the previous frame's:
/// exactly the rows that can differ between the frame before last and this
/// one. [`Shadow::compose`] carries that set forward internally so callers
/// pass only the frame's own damage.
///
/// The emitted diff comes from [`ratatui::buffer::Buffer::diff_iter`], not
/// a hand-rolled cell scan. A double-width symbol occupies its following
/// cell, which must therefore *not* be emitted separately -- the crossterm
/// backend skips the cursor move for a cell one column right of the last,
/// so emitting it would print it one column past the wide glyph, over the
/// cell after it. Delegating keeps that (and the VS16 trailing-cell and
/// blank-visible-style rules beside it) correct by construction rather than
/// by a re-derivation that would silently drift on a `ratatui` upgrade.
///
/// That delegation is also what bounds the diff's cost. A whole-frame diff
/// compares every cell, and a `Cell` comparison is a symbol-string compare
/// before it is anything else, measured at 6 ns per cell -- 29 us for a
/// 120x40 frame in which one cell changed. So the diff is handed *fewer
/// cells* rather than a cheaper comparison: the rows the frame actually
/// repainted are lifted into sub-buffers carrying their own terminal-space
/// rect, and `diff_iter` runs over those. Same iterator, same wide-glyph
/// rules, same absolute coordinates out; only the input is clipped.
///
/// Diffing a subset of rows is exact because no cell ever overflows its
/// row (see [`fitted_symbol`]), which leaves the diff carrying
/// no state across a row boundary, and because a row the frame did not
/// repaint cannot differ between the two buffers: `back` holds the frame
/// before last there, which the previous frame's damage having missed it
/// says is still what the model shows.
#[derive(Debug, Default)]
pub struct Shadow {
    front: Buffer,
    back: Buffer,
    carried: Damage,
    /// The rows the last [`Shadow::compose`] repainted, which are the only
    /// rows in which `front` and `back` can differ.
    painted: Damage,
    /// Scratch for [`Shadow::emit_updates`]'s row runs, kept across frames
    /// so a frame's clip costs no allocation.
    runs: Vec<(u16, u16)>,
    /// Scratch sub-buffers, one per run, likewise kept across frames.
    staged: Vec<StagedRun>,
}

/// One repainted row run lifted out of the shadow's buffers, so the diff
/// scans that run's cells instead of the whole frame's.
///
/// The run's cells are *swapped* in rather than cloned. A `Cell` clone
/// copies a `CompactString` per cell and measured 8.5 ns against 0.8 ns for
/// the swap, which would have spent more on the copy than the 6 ns per-cell
/// scan the clip exists to avoid. Both buffers carry the run's real
/// terminal-space rect, so `ratatui::buffer::Buffer::diff_iter` reports
/// absolute coordinates and nothing translates them back.
#[derive(Debug, Default)]
struct StagedRun {
    front: Buffer,
    back: Buffer,
}

/// A [`Shadow`] with its repainted row runs lifted into scratch sub-buffers,
/// which puts them back when it drops.
///
/// Staging is only reachable through this guard, so the lift can never be
/// left half-done: the rows return on the error path, on an early return, and
/// on an unwinding panic out of the backend alike. The shadow is the only
/// record of what the terminal shows, so a lift that failed to reverse would
/// make every later frame diff against scratch cells and emit the wrong
/// updates for the rest of the session.
struct StagedRuns<'a> {
    shadow: &'a mut Shadow,
    runs: &'a [(u16, u16)],
}

impl<'a> StagedRuns<'a> {
    /// Sizes one scratch sub-buffer per run and lifts the runs' rows into
    /// them.
    fn stage(shadow: &'a mut Shadow, runs: &'a [(u16, u16)]) -> Self {
        if shadow.staged.len() < runs.len() {
            shadow.staged.resize_with(runs.len(), StagedRun::default);
        }
        let area = shadow.front.area;
        for (slot, &(start, height)) in shadow.staged.iter_mut().zip(runs) {
            let rect = ratatui::layout::Rect {
                x: area.x,
                y: start,
                width: area.width,
                height,
            };
            let len = usize::from(height) * usize::from(area.width);
            slot.front.area = rect;
            slot.back.area = rect;
            slot.front.content.resize(len, Cell::EMPTY);
            slot.back.content.resize(len, Cell::EMPTY);
        }
        shadow.swap_runs(runs);
        Self { shadow, runs }
    }

    /// The staged runs' diffs, chained in ascending row order, which is the
    /// order the whole-frame diff would have yielded them in.
    fn diffs(&self) -> impl Iterator<Item = (u16, u16, &Cell)> + '_ {
        self.shadow
            .staged
            .iter()
            .take(self.runs.len())
            .flat_map(|run| run.front.diff_iter(&run.back))
    }
}

impl Drop for StagedRuns<'_> {
    fn drop(&mut self) {
        self.shadow.swap_runs(self.runs);
    }
}

impl Shadow {
    /// An empty shadow, zero-sized until the first [`Shadow::resize`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// What the terminal currently shows.
    #[must_use]
    pub fn front(&self) -> &Buffer {
        &self.front
    }

    /// Points the shadow at `area`, rebuilding both buffers blank when it
    /// differs from the current one. Returns whether that rebuild happened,
    /// which is the caller's cue to clear the real terminal and force a
    /// whole-frame repaint: after a size change neither the shadow nor the
    /// terminal's contents mean anything.
    pub fn resize(&mut self, area: ratatui::layout::Rect) -> bool {
        if self.front.area == area {
            return false;
        }
        self.front = Buffer::empty(area);
        self.back = Buffer::empty(area);
        self.carried = Damage::full();
        self.painted = Damage::full();
        self.staged.clear();
        true
    }

    /// Composites one frame into the back buffer, repainting `damage`'s rows
    /// plus the rows the previous frame repainted (see the type docs for why
    /// the previous frame's rows are needed).
    pub fn compose(&mut self, model: &Model, surface: &Surface, damage: &Damage) {
        let repaint = damage.union(&self.carried);
        composite_into(&mut self.back, model, surface, &repaint);
        self.carried = damage.clone();
        self.painted = repaint;
    }

    /// Writes the cells that differ between what the terminal shows and the
    /// frame just composed to `backend`, in the order they should appear on
    /// the wire.
    ///
    /// One `draw` call per frame either way, so the byte stream a run-clipped
    /// frame produces is the stream the unclipped diff would have produced.
    ///
    /// # Errors
    ///
    /// Returns the backend's own write error.
    pub fn emit_updates<B: Backend>(&mut self, backend: &mut B) -> Result<(), B::Error> {
        let mut runs = std::mem::take(&mut self.runs);
        self.painted.row_runs(self.front.area, &mut runs);
        let result = if clipping_pays(&runs, self.front.area.height) {
            self.emit_clipped(backend, &runs)
        } else {
            backend.draw(self.updates())
        };
        self.runs = runs;
        result
    }

    /// The whole-frame diff: every cell of what the terminal shows against
    /// every cell of the frame just composed.
    fn updates(&self) -> ratatui::buffer::BufferDiff<'_, '_> {
        self.front.diff_iter(&self.back)
    }

    /// The same emission clipped to `runs`, one chained `draw` over the
    /// staged sub-buffers.
    ///
    /// [`StagedRuns`] brackets exactly the backend call: while it lives the
    /// shadow's own buffers hold the scratch's stale cells in those rows, and
    /// nothing outside this function can observe that, because `backend`
    /// cannot reach the shadow. Its `Drop` puts the rows back before the
    /// backend's result reaches the caller, so neither a failed write nor an
    /// unwinding panic can leave the shadow holding scratch cells.
    fn emit_clipped<B: Backend>(
        &mut self,
        backend: &mut B,
        runs: &[(u16, u16)],
    ) -> Result<(), B::Error> {
        let staged = StagedRuns::stage(self, runs);
        backend.draw(staged.diffs())
    }

    /// Exchanges each run's rows between the shadow's buffers and its staged
    /// sub-buffer. Its own inverse, which is why staging and unstaging are
    /// the same call: a swap moves the cells without copying a `Cell`, so
    /// neither direction allocates or clones a symbol.
    fn swap_runs(&mut self, runs: &[(u16, u16)]) {
        let area = self.front.area;
        for (slot, &(start, _)) in self.staged.iter_mut().zip(runs) {
            let offset = usize::from(start.saturating_sub(area.y)) * usize::from(area.width);
            let len = slot.front.content.len();
            let end = offset.saturating_add(len);
            let (Some(front), Some(back)) = (
                self.front.content.get_mut(offset..end),
                self.back.content.get_mut(offset..end),
            ) else {
                // in range by construction (the runs come from `row_runs`
                // over this same area). Skipping a run would silently drop
                // that run's updates from the frame, so the invariant is
                // loud under test; a release build degrades to the missed
                // repaint rather than panicking in the paint loop
                debug_assert!(
                    false,
                    "staged run at row {start} spans {offset}..{end}, past the \
                     shadow's {} cells",
                    self.front.content.len()
                );
                continue;
            };
            slot.front.content.swap_with_slice(front);
            slot.back.content.swap_with_slice(back);
        }
    }

    /// Promotes the composed frame to be what the terminal shows, once its
    /// [`Shadow::emit_updates`] have been written.
    pub fn commit(&mut self) {
        std::mem::swap(&mut self.front, &mut self.back);
    }
}

/// The terminal-space rows every non-[`LayerKind::EngineGrid`] layer covers.
/// [`Term`](crate::terminal::Term) feeds this frame's set and remembers it as
/// the next frame's "previous overlay rows" so [`Damage::from_frame`] can
/// dirty the cells a vanished or moved overlay leaves behind.
#[must_use]
pub fn overlay_rows(surface: &Surface) -> Vec<u16> {
    let mut rows = Vec::new();
    for layer in &surface.layers {
        if matches!(layer.kind, LayerKind::EngineGrid) {
            continue;
        }
        let first = layer.rect.row;
        let last = first.saturating_add(layer.rect.height);
        rows.extend(first..last);
    }
    rows
}

/// Paints every layer in `surface`, in order (z ascending), into `buf`,
/// clipping the engine grid to `damage`'s rows. Later layers overwrite the
/// cells of earlier ones within their own rect, which is the z-order
/// compositing contract.
///
/// `buf` is persistent across frames (it holds the last frame's content),
/// so a non-full `damage` repaints only its rows and leaves every other
/// cell as the previous frame painted it -- the compositor's half of the
/// terminal's own cell diff. A full `damage` repaints every cell, which is
/// what a fresh buffer needs.
///
/// `model` supplies the engine grid and highlight table backing the
/// [`LayerKind::EngineGrid`] layer: `Surface` describes where to paint and
/// what kind of content goes there, not the grid's (potentially large)
/// per-cell content itself, so painting reads that straight from `model`
/// rather than cloning it into every frame's `Surface`.
///
/// `view-surface`'s `render()` chose each layer's placement per the two
/// layout mechanisms nvim's full ext attach demands: the tabline reserves a
/// real row (`view_core::model::Model::chrome_rows`) so it is never in the
/// same rect as the `EngineGrid` layer, while cmdline/messages/popupmenu
/// are transient overlays that intentionally paint over grid content only
/// while their state is active, then vanish the frame it clears (their
/// `LayerKind` variant is simply absent from `surface.layers` that frame,
/// so the unconditional `EngineGrid` paint below is what restores the
/// resting text underneath).
pub fn composite_into(buf: &mut Buffer, model: &Model, surface: &Surface, damage: &Damage) {
    let frame_area = buf.area;
    // Clear the rows this frame repaints before any layer paints, so each
    // layer writes over blank cells exactly as a full recomposite writes
    // over a fresh buffer. Without this, `buf` is persistent: a cell whose
    // new style leaves a field unset (grid text under a vanished overlay,
    // whose resolved style has no explicit fg) keeps the previous frame's
    // value, because `ratatui::buffer::Cell::set_style` *merges* rather than
    // replaces. Resetting the damaged rows makes the clipped path
    // byte-identical to the full path -- the property the equality test pins.
    reset_damaged_rows(buf, damage);
    // derived once per frame from the engine's live highlight state: a
    // lookup over already-decoded fields, not an RPC round trip, so
    // re-deriving on every paint costs nothing beyond this struct copy
    let theme = Theme::from_hl(model.engine.hl());
    for layer in &surface.layers {
        let area = clip_to_frame(layer.rect, frame_area);
        if area.width == 0 || area.height == 0 {
            continue;
        }
        match &layer.kind {
            LayerKind::EngineGrid => {
                paint_grid(
                    model.engine.grid(),
                    &theme,
                    model.engine.hl(),
                    area,
                    damage,
                    buf,
                );
            }
            LayerKind::Cmdline(state) => paint_cmdline(state, &theme, area, buf),
            LayerKind::Messages(entries) => paint_messages(entries, &theme, area, buf),
            LayerKind::Tabline(state) => paint_tabline(state, &theme, area, buf),
            LayerKind::Popupmenu(state) => paint_popupmenu(state, &theme, area, buf),
            LayerKind::Shell => paint_shell(&theme, area, buf),
            // LayerKind is #[non_exhaustive]: a future variant degrades to
            // painting nothing rather than failing to compile here
            _ => {}
        }
    }
}

/// The display columns a cell at buffer column `col` has before `buf`'s row
/// ends, which is the width a symbol written there must not exceed.
fn columns_left(buf: &Buffer, col: u16) -> u16 {
    buf.area
        .x
        .saturating_add(buf.area.width)
        .saturating_sub(col)
}

/// `symbol` if it fits in `columns_left` display columns, a blank if it does
/// not.
///
/// A terminal cannot show a grapheme in fewer columns than it occupies, and
/// `ratatui::buffer::Buffer::set_stringn` drops one that does not fit rather
/// than write it; the per-cell writers in this module place cells
/// individually (they must, to honour a grid's own column assignment) and so
/// have no equivalent stop of their own. nvim leaves a blank there for the
/// same reason.
///
/// Fitting is also what makes a row-clipped diff exact.
/// `ratatui::buffer::Buffer::diff_iter` walks a flat cell array and advances
/// past a wide cell's covered columns by index, so a symbol overflowing the
/// end of a row would swallow cells of the row below it -- leaving them stale
/// on screen, and making a diff that starts at that lower row emit cells the
/// whole-buffer diff does not. With no symbol overflowing its row, every row
/// boundary is a point at which the diff carries no state, which is what lets
/// [`Shadow`] diff a subset of rows and still emit exactly what a
/// whole-buffer diff emits. The width that matters is `ratatui`'s own
/// `cell_width`, not `unicode-width`'s raw one, because `diff_iter` skips by
/// the former.
///
/// A symbol's byte length is an upper bound on its display width, so a symbol
/// no longer than the columns left fits without computing anything: every
/// char two columns wide is at least three UTF-8 bytes, and the halfwidth
/// dakuten `ratatui` adds a column back for spends three bytes on that one
/// column. That bound is what keeps this a length compare on nearly every
/// cell of a text frame -- including the wide-glyph cells a width computation
/// would otherwise charge for -- rather than a width computation per cell.
fn fitted_symbol(symbol: &str, columns_left: u16) -> &str {
    if symbol.len() > usize::from(columns_left) && symbol.cell_width() > columns_left {
        " "
    } else {
        symbol
    }
}

/// Resets every cell of `buf`'s damaged rows to its default, so the layers
/// then paint over blank cells. See [`composite_into`] for why a persistent
/// buffer needs this and a fresh one does not.
fn reset_damaged_rows(buf: &mut Buffer, damage: &Damage) {
    let area = buf.area;
    let width = area.width as usize;
    for row in 0..area.height {
        if !damage.covers(area.y + row) {
            continue;
        }
        // walks the row's cells as a slice rather than indexing each one:
        // per-cell index arithmetic and its bounds check cost more than the
        // reset itself, which is what made a whole-frame repaint here more
        // expensive than the one `ratatui::buffer::Buffer::reset` runs
        let start = buf.index_of(area.x, area.y + row);
        for cell in &mut buf.content[start..start + width] {
            cell.reset();
        }
    }
}

/// Writes `text` into row `row_offset` of `area` (styled `style`),
/// truncating at the area's width or height rather than writing past it.
/// The shared primitive every chrome renderer below uses, so column/row
/// bounds-checking lives in exactly one place.
///
/// Each character advances the column by its own display width (1 for
/// ordinary text, 2 for wide characters like CJK ideographs) rather than
/// unconditionally by one cell: a fixed one-column advance would place a
/// wide character's glyph in a single cell it does not fit, misaligning
/// every character painted after it on the row. A wide character's second
/// (shadow) cell is reset so no later character in this same call can draw
/// into it, matching the convention `ratatui::buffer::Buffer::set_stringn`
/// itself uses for multi-width graphemes. A character wider than the columns
/// left before the buffer's own row ends is written as a blank instead (see
/// [`fitted_symbol`]).
fn paint_text_row(
    text: &str,
    style: Style,
    area: ratatui::layout::Rect,
    row_offset: u16,
    buf: &mut Buffer,
) {
    if row_offset >= area.height {
        return;
    }
    let mut col = 0_u16;
    for ch in text.chars() {
        if col >= area.width {
            break;
        }
        let ch = sanitized_char(ch);
        // sanitized_char already replaced every control character with a
        // plain space, so `width` is `None` here only for the handful of
        // zero-width combining marks that survive sanitization; `.max(1)`
        // still advances the column for those rather than looping forever
        // painting into the same cell
        let width = ch.width().unwrap_or(1).max(1) as u16;
        let room = columns_left(buf, area.x.saturating_add(col));
        let mut encode_buf = [0_u8; 4];
        let symbol = fitted_symbol(ch.encode_utf8(&mut encode_buf), room);
        let width = if symbol.len() == 1 { 1 } else { width };
        let cell = &mut buf[(area.x + col, area.y + row_offset)];
        cell.set_symbol(symbol);
        cell.set_style(style);
        if width == 2 && col + 1 < area.width {
            buf[(area.x + col + 1, area.y + row_offset)].reset();
        }
        col = col.saturating_add(width);
    }
}

/// Replaces a control character with a plain space, otherwise passes `ch`
/// through unchanged.
///
/// `ratatui::buffer::Cell::set_symbol` is a low-level API that, unlike
/// `Buffer::set_stringn`/`Span::styled_graphemes`, does not filter control
/// characters before computing cell width, and panics (in a debug build) on
/// one that slips through. Every renderer in this module calls
/// `set_symbol` directly (needed for per-cell placement, not just
/// left-to-right string layout), so every one of them routes through this
/// first. Reachable from live content, not just hostile input: an `emsg`
/// from a real nvim plugin's autocommand error carried a raw control byte
/// during manual verification of this module's overlay renderers.
fn sanitized_char(ch: char) -> char {
    if ch.is_control() {
        ' '
    } else {
        ch
    }
}

/// Renders the command line: first a full-row clear to the cmdline's own
/// style (so no glyph the grid layer painted underneath -- a statusline,
/// most commonly -- survives past the end of the typed text), then
/// `firstc`, then `prompt`, then its content chunks. In prompt mode (e.g.
/// `:call input("name: ")`) nvim sends an empty `firstc` and puts the label
/// in `prompt` instead, so concatenating both in this order reproduces
/// nvim's own `:` prefix in the ordinary case and the prompt label in the
/// input() case without a branch (live-verified against a real nvim's
/// `cmdline_show` traffic for `:call input("name: ")`: `firstc=""`,
/// `prompt="name: "`). The cursor itself is a separate concern
/// (`view_surface::render`'s `CursorSpec`, applied by
/// [`crate::terminal::Term::draw_surface`]), not painted here.
fn paint_cmdline(
    state: &CmdlineState,
    theme: &Theme,
    area: ratatui::layout::Rect,
    buf: &mut Buffer,
) {
    // a live `--clean` capture of nvim's `hl_group_set` batch (see
    // view-engine's ui_events tests) carries no builtin group naming the
    // cmdline row: nvim styles it from "Normal" itself, so `theme.normal()`
    // is not a fallback standing in for a missing mapping here, it is the
    // correct source
    let style = ratatui_style(theme.normal());
    let blank = " ".repeat(usize::from(area.width));
    paint_text_row(&blank, style, area, 0, buf);

    let mut text = state.firstc.clone();
    text.push_str(&state.prompt);
    for (_, chunk) in &state.content {
        text.push_str(chunk);
    }
    paint_text_row(&text, style, area, 0, buf);
}

/// Renders the message log as a bordered toast box: `render()` already
/// picked exactly the visible physical lines (`Messages::visible_lines` --
/// persistent error/warn lines always kept, the most recent transient
/// lines filling the rest) and grew/right-anchored `area` to them plus a
/// one-cell frame on every edge, so painting only has to draw the border
/// around `area` and write one line per interior row, in the order given
/// (oldest of the visible set on top). A truly empty `lines` paints nothing
/// at all -- no clear, no border -- matching `render()`'s own contract of
/// never emitting a `Messages` layer for an empty log; a caller that hands
/// this an empty slice with a stale nonzero `area` (only possible by
/// bypassing `render()`, e.g. directly in tests) must still see no bleed
/// from a frame that has no content to frame.
///
/// The whole rect -- border cells included -- is cleared to the toast's own
/// `msg_area` style first, before any text or border glyph: without this, a
/// row, a border cell, or the columns past a line's own text on a row keeps
/// showing whatever the `EngineGrid` layer painted underneath (real nvim
/// content, e.g. a floating window's cells composited into the base grid
/// when the frontend has no `ext_multigrid` support), which is what a live
/// repro showed as foreign glyphs bleeding through at a toast row's right
/// edge.
fn paint_messages(lines: &[String], theme: &Theme, area: ratatui::layout::Rect, buf: &mut Buffer) {
    if lines.is_empty() {
        return;
    }

    let style = ratatui_style(theme.msg_area);
    let blank = " ".repeat(usize::from(area.width));
    for row in 0..area.height {
        paint_text_row(&blank, style, area, row, buf);
    }

    let border_style = ratatui_style(ResolvedStyle {
        fg: Some(message_border_color(theme)),
        bg: theme.msg_area.bg,
        ..ResolvedStyle::default()
    });
    paint_message_border(area, border_style, buf);

    let inner = inset_by_one(area);
    for (i, line) in lines.iter().enumerate() {
        let Ok(row) = u16::try_from(i) else {
            break;
        };
        paint_text_row(line, style, inner, row, buf);
    }
}

/// `area` shrunk by one cell on every edge: the interior the border frame
/// leaves for `Messages::visible_lines`' own content, matching exactly the
/// unframed rect `view-surface` grew by two cols/two rows to make room for
/// the border this module draws around it.
fn inset_by_one(area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    ratatui::layout::Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

/// Draws box-drawing glyphs on all four edges of `area`, styled `style`.
/// A degenerate area narrower or shorter than 2 cells has no distinct edge
/// cells to draw (the frame `view-surface` builds is never this small in
/// practice, since it always adds a full 2-cell frame around at least a
/// 1x1 content rect, but a direct unit-test caller could still construct
/// one) and paints nothing rather than writing corner glyphs on top of
/// each other.
fn paint_message_border(area: ratatui::layout::Rect, style: Style, buf: &mut Buffer) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    let last_col = area.width - 1;
    let last_row = area.height - 1;
    for col in 0..area.width {
        let top = match col {
            0 => '┌',
            c if c == last_col => '┐',
            _ => '─',
        };
        let bottom = match col {
            0 => '└',
            c if c == last_col => '┘',
            _ => '─',
        };
        set_border_cell(buf, area.x + col, area.y, top, style);
        set_border_cell(buf, area.x + col, area.y + last_row, bottom, style);
    }
    for row in 1..last_row {
        set_border_cell(buf, area.x, area.y + row, '│', style);
        set_border_cell(buf, area.x + last_col, area.y + row, '│', style);
    }
}

/// Writes one border glyph directly into `buf`, bypassing [`paint_text_row`]:
/// its column-advance-by-display-width logic exists for laying out a whole
/// string of arbitrary (possibly wide/control) characters across a row,
/// which a single fixed-width box-drawing character never needs.
fn set_border_cell(buf: &mut ratatui::buffer::Buffer, x: u16, y: u16, ch: char, style: Style) {
    let mut encode_buf = [0_u8; 4];
    let cell = &mut buf[(x, y)];
    cell.set_symbol(ch.encode_utf8(&mut encode_buf));
    cell.set_style(style);
}

/// The message toast border's foreground. `Theme` resolves no builtin
/// group carrying a genuinely muted/comment tone -- `from_hl` maps only
/// `StatusLine`, the tabline and popup-menu families, and `MsgArea`, none
/// of which plays the role real nvim's own `Comment`/`NonText`/
/// `FloatBorder` groups would (an unobtrusive chrome color distinct from
/// both emphasis and interior-text colors), and this module never probes
/// nvim for a group it does not already resolve -- so the border derives a
/// dimmed variant of the interior's own `msg_area` foreground when one is
/// set, visibly distinct from the full-brightness interior text with no
/// further highlight lookup or RPC round trip. Never falls back to a
/// dimmed `msg_area.bg`: the border sits ON that background, so dimming it
/// paints a frame that is merely a darker shade of the surface it is
/// supposed to stand out from -- on a black-bg/no-fg theme this dims pure
/// black to itself, an invisible border around a box the user cannot tell
/// apart from empty screen. The floor is the plain (undimmed) neutral grey
/// constant instead, which stays visible against any background.
fn message_border_color(theme: &Theme) -> u32 {
    theme.msg_area.fg.map_or(0x0080_8080, dim)
}

/// Scales each RGB channel of `c` to 60% of its original value, the muted
/// transform [`message_border_color`] applies when no themed group already
/// carries one.
fn dim(c: u32) -> u32 {
    let channel = |shift: u32| -> u32 { ((c >> shift) & 0xFF) * 3 / 5 };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

/// Renders the tabline into its reserved row: each tab as ` name `, the
/// current tab reverse-styled so it reads as selected without needing
/// bracket characters that would shift every other tab's column.
fn paint_tabline(
    state: &TablineState,
    theme: &Theme,
    area: ratatui::layout::Rect,
    buf: &mut Buffer,
) {
    // painted before the tab labels themselves so `TabLineFill` shows
    // through any column the labels below do not reach (a short tab list
    // in a wide terminal), matching what that builtin group names: the
    // row's background beyond the tabs
    let fill = " ".repeat(usize::from(area.width));
    paint_text_row(&fill, ratatui_style(theme.tab_line_fill), area, 0, buf);

    let mut text = String::new();
    let mut current_range: Option<(u16, u16)> = None;
    for tab in &state.tabs {
        // display-cell width, not char count: a tab name containing a wide
        // (CJK) character occupies more columns than it has chars, and the
        // selection-highlight range below must land on the same columns
        // paint_text_row actually painted the label into
        let start = u16::try_from(text.width()).unwrap_or(u16::MAX);
        text.push_str(&format!(" {} ", tab.name));
        let end = u16::try_from(text.width()).unwrap_or(u16::MAX);
        if tab.tab == state.current {
            current_range = Some((start, end));
        }
    }
    paint_text_row(&text, ratatui_style(theme.tab_line), area, 0, buf);
    if let Some((start, end)) = current_range {
        for col in start..end.min(area.width) {
            buf[(area.x + col, area.y)].set_style(ratatui_style(theme.tab_line_sel));
        }
    }
}

/// Renders the popup menu: one item per row via [`PmItem::display_text`],
/// the `selected` index reverse-styled. `render()` already anchored and
/// sized `area` to the event's `(row, col)` and the widest item.
fn paint_popupmenu(
    state: &PopupmenuState,
    theme: &Theme,
    area: ratatui::layout::Rect,
    buf: &mut Buffer,
) {
    for (i, item) in state.items.iter().enumerate() {
        let Ok(row) = u16::try_from(i) else {
            break;
        };
        if row >= area.height {
            break;
        }
        let is_selected = i64::try_from(i).is_ok_and(|idx| idx == state.selected);
        let style = if is_selected {
            ratatui_style(theme.pmenu_sel)
        } else {
            ratatui_style(theme.pmenu)
        };
        paint_text_row(&item.display_text(), style, area, row, buf);
    }
}

/// Renders the pre-content startup shell: a themed statusline placeholder
/// bar on the terminal's bottom row, plus a static "waiting for nvim"
/// indicator centered in the remaining rows. Present only while
/// `view_core::model::Model::content_painted` is `false` (see
/// `view_surface::render`); `render()` stops including the
/// [`LayerKind::Shell`] layer at all once real grid content has arrived, so
/// this function has nothing left to overwrite it with.
///
/// No animation: the runtime loop is timer-free (no clock anywhere in its
/// steady-state body), so this glyph is fixed rather than advancing frames
/// on its own -- a real spinner would need a tick this architecture
/// deliberately does not have.
fn paint_shell(theme: &Theme, area: ratatui::layout::Rect, buf: &mut Buffer) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let fill = " ".repeat(usize::from(area.width));
    let bottom_row = area.height - 1;
    paint_text_row(
        &fill,
        ratatui_style(theme.status_line),
        area,
        bottom_row,
        buf,
    );

    let label = "view: waiting for nvim...";
    let text: String = label.chars().take(usize::from(area.width)).collect();
    let mid_row = area.height / 2;
    paint_text_row(&text, ratatui_style(theme.normal()), area, mid_row, buf);
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

/// Highest `hl_id` the per-frame dense style cache will hold. nvim
/// allocates highlight ids as small dense integers, so real frames sit
/// far below this; an id past the cap (or a pathological huge id) simply
/// resolves uncached rather than growing an unbounded table.
const STYLE_CACHE_CAP: usize = 4096;

/// A per-frame memo of `hl_id -> ratatui::Style`, indexed directly by id.
/// `Theme::style_for` costs a `HashMap` probe per call, and a full-grid
/// composite makes one call per cell (4800 on a 120x40 frame) out of only
/// a handful of distinct ids; resolving each id once per frame removes
/// the probe from the per-cell path entirely. Frame-scoped rather than
/// persistent so there is no invalidation to get wrong when the
/// highlight table or theme changes between frames.
struct StyleCache {
    dense: Vec<Option<Style>>,
}

impl StyleCache {
    fn new() -> Self {
        Self { dense: Vec::new() }
    }

    fn get(&mut self, theme: &Theme, hl: &HlTable, hl_id: u64) -> Style {
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

/// Paints the `grid` cells within `area` that `damage` covers, styled per
/// `hl` through `theme`. Rows `damage` does not cover keep whatever the
/// persistent `buf` already holds for them (last frame's content), which is
/// what turns a full recomposite into a damage-clipped one; a full `damage`
/// repaints every visible cell exactly as before.
fn paint_grid(
    grid: &Grid,
    theme: &Theme,
    hl: &HlTable,
    area: ratatui::layout::Rect,
    damage: &Damage,
    buf: &mut Buffer,
) {
    let (w, h) = grid.size();
    let mut styles = StyleCache::new();
    let cols = w.min(area.width) as usize;
    for row in 0..h.min(area.height) {
        // `area.y + row` is the terminal-space row `damage` is expressed in;
        // skipping an unchanged row leaves its cells as the previous frame
        // painted them
        if !damage.covers(area.y + row) {
            continue;
        }
        // the row's destination cells as one slice: indexing each cell
        // separately recomputes the same offset and re-checks the same
        // bounds ~4800 times on a whole-frame repaint
        let start = buf.index_of(area.x, area.y + row);
        // the columns from this layer's left edge to the end of the buffer's
        // own row, which is what a symbol's width is measured against rather
        // than the layer's width: a glyph overflowing the layer still lands
        // inside the row, while one overflowing the row lands in the next
        let span = columns_left(buf, area.x);
        let out_row = &mut buf.content[start..start + cols];
        for (col, out) in out_row.iter_mut().enumerate() {
            let col = col as u16;
            if let Some(cell) = grid.cell(row, col) {
                let symbol = sanitized_symbol(&cell.text);
                out.set_symbol(fitted_symbol(&symbol, span.saturating_sub(col)));
                out.set_style(styles.get(theme, hl, cell.hl_id));
            }
        }
    }
}

/// Like [`sanitized_char`], but over a whole grid cell's (possibly
/// multi-char grapheme) text, and substituting a single space for an empty
/// cell the same way the pre-sanitization code always did. Only allocates
/// when a control character is actually present; the common case (an
/// ordinary printable grapheme) borrows straight through.
fn sanitized_symbol(text: &str) -> std::borrow::Cow<'_, str> {
    if text.is_empty() {
        return std::borrow::Cow::Borrowed(" ");
    }
    if text.chars().any(char::is_control) {
        std::borrow::Cow::Owned(text.chars().map(sanitized_char).collect())
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

/// One grid cell's resolved style, converted to `ratatui`: derivation
/// itself lives in [`Theme::style_for`] (backend-free, in `view-core`), so
/// this is purely the `ResolvedStyle` -> `ratatui::style::Style` mapping.
fn style_for(theme: &Theme, hl_id: u64, table: &HlTable) -> Style {
    ratatui_style(theme.style_for(hl_id, table))
}

/// Converts a backend-free [`ResolvedStyle`] into a `ratatui::style::Style`.
fn ratatui_style(resolved: ResolvedStyle) -> Style {
    let mut style = Style::default();
    if let Some(c) = resolved.fg {
        style = style.fg(rgb(c));
    }
    if let Some(c) = resolved.bg {
        style = style.bg(rgb(c));
    }
    if resolved.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if resolved.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if resolved.underline {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if resolved.reverse {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

fn rgb(c: u32) -> Color {
    Color::Rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use view_core::grid::GridOp;

    /// A whole-frame composite into a fresh `ratatui::Frame`. Lives here
    /// rather than beside [`composite_into`] because nothing in production
    /// paints this way: the runtime drives [`Shadow`], which owns the
    /// persistent buffers a damage clip needs, and a second entry point
    /// bypassing it would be an entry point bypassing the damage discipline.
    /// A `Frame` (through `Terminal::draw`) is still the least indirect way
    /// for a test to observe a whole painted frame's cells.
    fn composite(model: &Model, surface: &Surface, frame: &mut ratatui::Frame<'_>) {
        composite_into(frame.buffer_mut(), model, surface, &Damage::full());
    }

    fn table_with(attr: HlAttr) -> HlTable {
        let mut table = HlTable::new();
        table.define_attr(1, attr);
        table
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
        let theme = Theme::from_hl(&table);
        let style = style_for(&theme, 1, &table);
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
        let theme = Theme::from_hl(&table);
        let style = style_for(&theme, 1, &table);
        assert!(!style
            .add_modifier
            .contains(ratatui::style::Modifier::UNDERLINED));
    }

    /// Golden test: the grid layer's own content paints correctly.
    #[test]
    fn composite_paints_grid_layer_content() {
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize {
            width: 10,
            height: 3,
        });
        model.engine.apply_grid(GridOp::PutLine {
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

    /// End-to-end regression at the rendering layer: once a probe reply
    /// confirms `Normal` has no background at all (the transparent-config
    /// fixture -- see `view-engine`'s `decode_hl_probe_reply` doc comment
    /// for the wire-verified shape this mirrors), a default grid cell must
    /// carry `Color::Reset` (ratatui's "no color set" default), never an
    /// explicit RGB, so the real terminal's own background shows through.
    /// Disconfirm: collapsing `Theme::from_hl`'s `bg` derivation to the raw
    /// wire default (see `theme.rs`) makes this assert `Color::Rgb(0,0,0)`
    /// instead -- an all-black paint where transparency was expected. Both
    /// of that derivation's confirmed-reading branches produce the same
    /// value for the state here, so it takes dropping the pair to move this
    /// assertion; the branches are told apart from each other in `theme.rs`,
    /// against state built for that purpose.
    #[test]
    fn transparent_confirmed_default_paints_grid_cells_with_no_bg_color() {
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize {
            width: 4,
            height: 1,
        });
        model.engine.apply_grid(GridOp::PutLine {
            row: 0,
            col_start: 0,
            cells: vec![("x".into(), 0, 1)],
        });
        apply(
            &mut model,
            view_core::events::UiEvent::DefaultColorsSet {
                fg: Some(0xF8F8F2),
                bg: Some(0), // wire-ambiguous: nvim sends 0 for "unset"
                sp: None,
            },
        );
        let generation = model.engine.hl().probe_generation();
        let _ = view_core::update::update(
            &mut model,
            view_core::msg::Msg::HlProbeReply {
                generation,
                fg: Some(0xF8F8F2),
                bg: None, // the probe reply's map had no "bg" key
            },
        );

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(4, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        assert_eq!(
            buf[(0, 0)].bg,
            ratatui::style::Color::Reset,
            "confirmed-unset bg must paint with no color, letting the terminal default show through"
        );
    }

    /// The warm-start counterpart to the test above: a confirmed
    /// transparent bg, seeded from persisted state *before* attach
    /// (mirrored here by setting `hl.confirmed` directly, the same state
    /// that seeding produces), and attach's own `default_colors_set` then
    /// resends the
    /// same wire-ambiguous zero the real bug report always reproduces with
    /// -- before its own probe reply has landed. The frame must still carry
    /// `Color::Reset`, never an explicit black, for that entire in-flight
    /// window: painting the raw wire zero here is exactly the black flash
    /// the user reported on every startup of a transparent config, even
    /// with a warm cache that already knew the answer.
    /// Disconfirm: reverting `Theme::from_hl`'s ambiguous-bg branch to the
    /// raw-wire fallback (its pre-fix shape) makes this assert
    /// `Color::Rgb(0,0,0)` instead -- the warm-start frame paints black.
    #[test]
    fn warm_start_confirmed_transparent_bg_survives_attachs_ambiguous_default_colors_set() {
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize {
            width: 4,
            height: 1,
        });
        model.engine.apply_grid(GridOp::PutLine {
            row: 0,
            col_start: 0,
            cells: vec![("x".into(), 0, 1)],
        });
        // mirrors the pre-attach state seeded from persisted state: a
        // confirmed value at the table's starting generation, from a prior
        // session's cached, already-disambiguated theme
        let generation = model.engine.hl().probe_generation();
        model
            .engine
            .confirm_hl_defaults(view_core::hl::ProbedDefaults {
                generation,
                fg: Some(0xF8F8F2),
                bg: None,
            });
        apply(
            &mut model,
            view_core::events::UiEvent::DefaultColorsSet {
                fg: Some(0xF8F8F2),
                bg: Some(0), // wire-ambiguous: nvim sends 0 for "unset"
                sp: None,
            },
        );
        // deliberately no HlProbeReply yet: the probe this DefaultColorsSet
        // just triggered is still in flight

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(4, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        assert_eq!(
            buf[(0, 0)].bg,
            ratatui::style::Color::Reset,
            "a warm-cached transparent bg must not flash black while attach's probe is in flight"
        );
    }

    /// The counterpart: a probe reply that confirms `bg = 0` (a genuinely
    /// black theme) keeps painting an explicit black cell rather than being
    /// conflated with the unset case above.
    #[test]
    fn genuinely_black_confirmed_default_still_paints_black() {
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize {
            width: 4,
            height: 1,
        });
        model.engine.apply_grid(GridOp::PutLine {
            row: 0,
            col_start: 0,
            cells: vec![("x".into(), 0, 1)],
        });
        apply(
            &mut model,
            view_core::events::UiEvent::DefaultColorsSet {
                fg: Some(0xFFFFFF),
                bg: Some(0),
                sp: None,
            },
        );
        let generation = model.engine.hl().probe_generation();
        let _ = view_core::update::update(
            &mut model,
            view_core::msg::Msg::HlProbeReply {
                generation,
                fg: Some(0xFFFFFF),
                bg: Some(0), // the probe reply's map DID carry "bg": 0
            },
        );

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(4, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        assert_eq!(buf[(0, 0)].bg, ratatui::style::Color::Rgb(0, 0, 0));
    }

    /// A helper for driving `view-core` events through `update()` the same
    /// way production code does, since the state structs behind each
    /// `LayerKind` are `#[non_exhaustive]` and cannot be built with
    /// struct-literal syntax from outside `view-core`.
    fn apply(model: &mut Model, ev: view_core::events::UiEvent) {
        let _ = view_core::update::update(model, view_core::msg::Msg::Redraw(vec![ev]));
    }

    /// Supersedes an earlier EngineGrid-only regression test: the cmdline is
    /// a transient overlay, so it is correct UX for it to paint over the
    /// grid's bottom row while it is open (matching the cmdheight=0
    /// floating UX external UIs give). The invariant pinned here instead is
    /// that the overlay vanishes with its state, restoring the resting
    /// buffer text on the very next frame that has no cmdline layer.
    #[test]
    fn cmdline_overlay_paints_while_shown_and_vanishes_with_cmdlinehide() {
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize {
            width: 10,
            height: 3,
        });
        model.engine.apply_grid(GridOp::PutLine {
            row: 2,
            col_start: 0,
            cells: vec![("x".into(), 0, 1), ("y".into(), 0, 1)],
        });

        apply(
            &mut model,
            view_core::events::UiEvent::CmdlineShow {
                content: vec![(0, "q".to_string())],
                pos: 0,
                firstc: ":".to_string(),
                prompt: String::new(),
                indent: 0,
                level: 1,
            },
        );
        let surface = view_surface::render(&model);
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert_eq!(
            &buf[(0, 2)].symbol(),
            &":",
            "cmdline overlay must paint over the grid's bottom row while shown"
        );

        apply(&mut model, view_core::events::UiEvent::CmdlineHide);
        let surface = view_surface::render(&model);
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert_eq!(
            &buf[(0, 2)].symbol(),
            &"x",
            "resting grid text must return once the overlay's state is gone"
        );
        assert_eq!(&buf[(1, 2)].symbol(), &"y");
    }

    /// Regression: while the cmdline is active, it must claim the *whole*
    /// bottom row, not just the cells its own prompt+content text occupies.
    /// Reproduces the reported bug shape (a statusline-bearing grid row
    /// bleeding through past the typed command). Disconfirm: without the
    /// row-claim fill in `paint_cmdline`, cell `(5, 2)` still reads a
    /// statusline glyph instead of a blank space.
    #[test]
    fn cmdline_overlay_claims_the_full_bottom_row_no_grid_glyph_bleeds_through() {
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize {
            width: 10,
            height: 3,
        });
        model.engine.apply_grid(GridOp::PutLine {
            row: 2,
            col_start: 0,
            cells: "STATUSLIN".chars().map(|c| (c.to_string(), 0, 1)).collect(),
        });

        apply(
            &mut model,
            view_core::events::UiEvent::CmdlineShow {
                content: vec![(0, "wq".to_string())],
                pos: 2,
                firstc: ":".to_string(),
                prompt: String::new(),
                indent: 0,
                level: 1,
            },
        );
        let surface = view_surface::render(&model);
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        let row: String = (0..10).map(|c| buf[(c, 2)].symbol().to_string()).collect();
        assert_eq!(
            row, ":wq       ",
            "cmdline overlay must claim the whole bottom row"
        );
    }

    /// Regression for a real bug: nvim's prompt mode (`:call input("name:
    /// ")`) sends an empty `firstc` and puts the label in `prompt` instead,
    /// which `paint_cmdline` previously dropped entirely, rendering a blank
    /// bottom row while keystrokes were silently accepted with no visible
    /// label. Values match a live nvim capture of that exact command (see
    /// `paint_cmdline`'s doc comment).
    ///
    /// Also pins that `paint_cmdline`'s full-row claim (see
    /// `cmdline_overlay_claims_the_full_bottom_row_no_grid_glyph_bleeds_through`)
    /// covers prompt mode too, not just the `firstc` mode that regression
    /// test drives: row 2 is seeded with statusline-shaped glyphs first, so
    /// the full-row assertion below disconfirms unless the blank-row fill
    /// actually runs ahead of the prompt label and content.
    #[test]
    fn cmdline_prompt_mode_renders_prompt_label_and_places_cursor_after_it() {
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize {
            width: 20,
            height: 3,
        });
        model.engine.apply_grid(GridOp::PutLine {
            row: 2,
            col_start: 0,
            cells: "STATUSLINESTATUSLINE"
                .chars()
                .map(|c| (c.to_string(), 0, 1))
                .collect(),
        });

        apply(
            &mut model,
            view_core::events::UiEvent::CmdlineShow {
                content: vec![(0, "X".to_string())],
                pos: 1,
                firstc: String::new(),
                prompt: "name: ".to_string(),
                indent: 0,
                level: 1,
            },
        );
        let surface = view_surface::render(&model);
        let backend = TestBackend::new(20, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        for (col, ch) in "name: X".chars().enumerate() {
            assert_eq!(
                &buf[(col as u16, 2)].symbol(),
                &ch.to_string(),
                "prompt row column {col} must hold {ch:?}"
            );
        }

        let cursor = surface.cursor.expect("prompt mode must place a cursor");
        assert_eq!(
            cursor.col, 7,
            "cursor must land after \"name: \" (6 cols) plus pos=1 into the typed content"
        );
        assert_eq!(cursor.row, 2);

        let full_row: String = (0..20).map(|c| buf[(c, 2)].symbol().to_string()).collect();
        assert_eq!(
            full_row,
            format!("{:<20}", "name: X"),
            "prompt-mode cmdline must claim the whole bottom row; the seeded \
             statusline glyphs past column 6 must not bleed through"
        );
    }

    /// Invariant pinned here: persistent chrome (the tabline) may never sit
    /// over resting buffer text. With more than one tab open the grid is
    /// offset below the reserved top row, so the tabline and the
    /// grid's own content occupy disjoint rows in the same frame.
    #[test]
    fn tabline_reserves_the_top_row_and_never_covers_resting_grid_text() {
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize {
            width: 10,
            height: 3,
        });
        model.engine.apply_grid(GridOp::PutLine {
            row: 0,
            col_start: 0,
            cells: vec![("a".into(), 0, 1), ("b".into(), 0, 1)],
        });
        apply(
            &mut model,
            view_core::events::UiEvent::TablineUpdate {
                current: view_core::events::TabHandle(1),
                tabs: vec![
                    view_core::events::TabEntry {
                        tab: view_core::events::TabHandle(1),
                        name: "one".into(),
                    },
                    view_core::events::TabEntry {
                        tab: view_core::events::TabHandle(2),
                        name: "two".into(),
                    },
                ],
            },
        );
        // TablineUpdate crossing the 1-tab boundary shrinks the grid target;
        // the grid itself only reflects that once nvim's GridResize round
        // trips, which this test drives directly to exercise the settled
        // (post-round-trip) frame.
        model.engine.apply_grid(GridOp::Resize {
            width: 10,
            height: 2,
        });
        model.engine.apply_grid(GridOp::PutLine {
            row: 0,
            col_start: 0,
            cells: vec![("a".into(), 0, 1), ("b".into(), 0, 1)],
        });

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        assert_eq!(
            &buf[(0, 0)].symbol(),
            &" ",
            "tabline label starts with a space"
        );
        assert_eq!(&buf[(1, 0)].symbol(), &"o");
        assert_eq!(&buf[(2, 0)].symbol(), &"n");
        assert_eq!(
            &buf[(0, 1)].symbol(),
            &"a",
            "grid content must land one row below the reserved tabline row"
        );
        assert_eq!(&buf[(1, 1)].symbol(), &"b");
    }

    /// Pins the one-frame transient: `TablineUpdate` crossing the 1-tab
    /// boundary reserves a chrome row immediately (the same frame), but
    /// the grid itself keeps its pre-shrink height until
    /// nvim's corresponding `GridOp::Resize` round-trips on a later frame.
    /// For exactly that one frame, `render()`'s `EngineGrid` layer (now
    /// offset by the new chrome row, but still the old height) extends one
    /// row past the terminal's actual frame -- `clip_to_frame` must clip
    /// it rather than let the draw call index past the buffer.
    #[test]
    fn tabline_update_without_matching_grid_resize_clips_the_transient_overflow_row() {
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize {
            width: 10,
            height: 3,
        });
        model.engine.apply_grid(GridOp::PutLine {
            row: 0,
            col_start: 0,
            cells: vec![("a".into(), 0, 1)],
        });
        model.engine.apply_grid(GridOp::PutLine {
            row: 2,
            col_start: 0,
            cells: vec![("z".into(), 0, 1)],
        });

        apply(
            &mut model,
            view_core::events::UiEvent::TablineUpdate {
                current: view_core::events::TabHandle(1),
                tabs: vec![
                    view_core::events::TabEntry {
                        tab: view_core::events::TabHandle(1),
                        name: "one".into(),
                    },
                    view_core::events::TabEntry {
                        tab: view_core::events::TabHandle(2),
                        name: "two".into(),
                    },
                ],
            },
        );
        // deliberately no GridOp::Resize here: the grid is still 3 rows
        // tall while the tabline already reserves row 0, the exact
        // transient window under test

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        // must not panic despite the EngineGrid layer (row=1, height=3)
        // extending one row past the frame's own 3 rows
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        assert_eq!(
            &buf[(0, 0)].symbol(),
            &" ",
            "tabline label starts with a space on the reserved row"
        );
        assert_eq!(
            &buf[(0, 1)].symbol(),
            &"a",
            "grid row 0 shifted down by the reserved chrome row"
        );
        for row in 0..3 {
            for col in 0..10 {
                assert_ne!(
                    buf[(col, row)].symbol(),
                    "z",
                    "grid row 2 (\"z\") is outside the clipped area and must not be painted"
                );
            }
        }
    }

    #[test]
    fn single_tab_reserves_no_row_and_grid_fills_the_full_frame() {
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize {
            width: 10,
            height: 3,
        });
        model.engine.apply_grid(GridOp::PutLine {
            row: 0,
            col_start: 0,
            cells: vec![("z".into(), 0, 1)],
        });

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert_eq!(&buf[(0, 0)].symbol(), &"z");
    }

    #[test]
    fn messages_overlay_renders_stacked_toasts_top_right() {
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize {
            width: 10,
            height: 3,
        });
        apply(
            &mut model,
            view_core::events::UiEvent::MsgShow {
                kind: "echomsg".into(),
                content: vec![(0, "hi".into())],
                replace_last: false,
            },
        );

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        // framed box: "hi" (2) + the 2-cell border = 4 wide, 3 tall,
        // right-anchored at col 6 (10 - 4), so the interior text row lands
        // at (7..9, 1)
        assert_eq!(&buf[(7, 1)].symbol(), &"h");
        assert_eq!(&buf[(8, 1)].symbol(), &"i");

        apply(&mut model, view_core::events::UiEvent::MsgClear);
        let surface = view_surface::render(&model);
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert_eq!(
            &buf[(7, 1)].symbol(),
            &" ",
            "message toast must vanish once MsgClear empties the log"
        );
    }

    /// A CJK (wide, 2-cell) character in a message toast must not misalign
    /// the character painted after it: `messages_width` sizes the layer by
    /// display cells (3 for "中b": 2 for the wide glyph, 1 for "b"), not
    /// char count (which would undercount to 2 and anchor the layer one
    /// column too far right), and `paint_text_row` must advance past the
    /// wide glyph's own shadow cell before placing "b" rather than writing
    /// "b" directly over it.
    #[test]
    fn messages_overlay_wide_char_advances_two_columns_not_one() {
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize {
            width: 10,
            height: 3,
        });
        apply(
            &mut model,
            view_core::events::UiEvent::MsgShow {
                kind: "echomsg".into(),
                content: vec![(0, "中b".into())],
                replace_last: false,
            },
        );

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        // interior content is 3 display cells wide ("中" = 2, "b" = 1); the
        // framed box adds the 2-cell border (5 wide total), right-anchored
        // at col 5 (10 - 5), so the interior text row lands at (6..9, 1) --
        // one column further left than an unframed box, not the same
        // columns the border now occupies
        assert_eq!(&buf[(6, 1)].symbol(), &"中");
        assert_eq!(
            &buf[(7, 1)].symbol(),
            &" ",
            "the wide glyph's shadow cell must be empty, not overwritten by the next char"
        );
        assert_eq!(&buf[(8, 1)].symbol(), &"b");
    }

    /// Reproduces a real repro: a startup `emsg` autocommand error carries
    /// an embedded `\n` (nvim's own multi-line message convention), and the
    /// toast must lay it out as two rows sized to the longer physical
    /// line, not one row wide enough to hold both lines concatenated with
    /// the columns past each line's own text left showing the grid content
    /// underneath.
    #[test]
    fn messages_overlay_multiline_message_gets_one_row_per_physical_line_and_clears_its_box() {
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize {
            width: 26,
            height: 5,
        });
        // stands in for real grid content underneath the toast's rect (a
        // composited floating window in the live repro): every cell the
        // toast's own clear must overwrite, or it bleeds through exactly
        // like the reported foreign glyphs at a message row's right edge
        for row in 0..2u16 {
            model.engine.apply_grid(GridOp::PutLine {
                row,
                col_start: 0,
                cells: vec![("X".into(), 0, 26)],
            });
        }
        apply(
            &mut model,
            view_core::events::UiEvent::MsgShow {
                kind: "echoerr".into(),
                content: vec![(0, "short\nmuch longer second line".into())],
                replace_last: false,
            },
        );

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(26, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let row_text =
            |r: u16| -> String { (0..26).map(|c| buf[(c, r)].symbol().to_string()).collect() };

        // interior width = the longer physical line (23 cells), not the sum
        // of both lines (28); the framed box adds the 2-cell border (25
        // wide, 4 tall total), right-anchored at col 1 (26 - 25), so the
        // interior's two text rows land at y=1 and y=2, columns 2..25, with
        // the border's own left/right edge glyphs at columns 1 and 25 --
        // column 0 alone stays real grid content, and the second line
        // exactly fills its row with no leftover to clear, while the first
        // line's row has 18 cells of clear past "short" that must not
        // still show the grid's "X" stand-in
        let expected_first_line = format!("X│{:<23}│", "short");
        let expected_second_line = format!(" │{}│", "much longer second line");
        assert_eq!(
            row_text(1),
            expected_first_line,
            "first physical line's row must be cleared past its own text, not left showing the grid underneath"
        );
        assert_eq!(
            row_text(2),
            expected_second_line,
            "second physical line must land on its own row, right-anchored to the wider line's width"
        );
    }

    /// Clamp-boundary regression: five physical lines exactly fill a
    /// 5-row grid (no eviction `Messages::visible_lines` itself would ever
    /// perform against a full `grid_h` budget), but the framed interior
    /// only has 3 rows once the border's own top/bottom edge is
    /// subtracted. Before `render()` shrank the selection budget by the
    /// frame's 2 rows, `visible_lines` kept all 5 lines (nothing to evict
    /// at max_rows == total lines) and the interior clamp then silently
    /// dropped the tail of that `Vec` -- the two newest lines, including
    /// the persistent `echoerr` -- without `visible_lines`'s own
    /// persistent-line-priority eviction ever getting a say. Disconfirm:
    /// reverting `render()`'s three `.saturating_sub(2)` budget clamps back
    /// to the raw `grid_h`/`grid_w` reproduces exactly this -- `cargo test
    /// -p view-tui messages_toast_paints_the_newest_persistent_line`
    /// fails with the interior's last row reading `"info2"`, a transient
    /// line three older than the dropped `echoerr`, instead of
    /// "critical error".
    #[test]
    fn messages_toast_paints_the_newest_persistent_line_at_the_clamp_boundary_instead_of_silently_dropping_it(
    ) {
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize {
            width: 20,
            height: 5,
        });
        for kind_and_text in [
            ("echomsg", "info0"),
            ("echomsg", "info1"),
            ("echomsg", "info2"),
            ("echomsg", "info3"),
            ("echoerr", "critical error"),
        ] {
            apply(
                &mut model,
                view_core::events::UiEvent::MsgShow {
                    kind: kind_and_text.0.into(),
                    content: vec![(0, kind_and_text.1.into())],
                    replace_last: false,
                },
            );
        }

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(20, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        let messages = surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::Messages(_)))
            .expect("messages layer present");
        // the frame still closes: a full 5-row box (3-row interior + 2
        // border rows), not clamped shorter than the grid it fits inside
        assert_eq!(
            messages.rect.height, 5,
            "frame must fit the grid exactly, not overflow and get clamped"
        );
        let (x, y, w) = (messages.rect.col, messages.rect.row, messages.rect.width);
        assert_eq!(
            &buf[(x, y)].symbol(),
            &"┌",
            "top-left corner still closes the frame"
        );
        assert_eq!(
            &buf[(x + w - 1, y + messages.rect.height - 1)].symbol(),
            &"┘",
            "bottom-right corner still closes the frame"
        );

        // the interior's last row (y + 1 + 2, the third and final interior
        // row) must carry the persistent, newest line -- not blank border
        // fill left over from a silently dropped selection
        let last_row: String = (x + 1..x + w - 1)
            .map(|c| buf[(c, y + 3)].symbol().to_string())
            .collect();
        assert_eq!(
            last_row, "critical error",
            "the persistent error, as the newest selected line, must be painted on the interior's last row"
        );
    }

    /// Width analogue of the clamp-boundary test above: a single 9-cell
    /// line in a 10-wide grid leaves only 8 interior cells once the
    /// border's own left/right edge is subtracted, so the interior must
    /// show exactly the widest line's first 8 cells -- never fewer, which
    /// is what happens if `render()`'s width budget is measured against
    /// the raw `grid_w` instead of `grid_w` shrunk by the frame's own 2
    /// columns first. (This specific right-anchored geometry -- `col =
    /// grid_w.saturating_sub(width)` saturates to 0 the moment `width`
    /// exceeds `grid_w`, and the subsequent `clamp_to` then caps `width`
    /// back to exactly `grid_w` regardless of how large the unclamped
    /// request was -- means the final interior width converges to the
    /// same `grid_w - 2` whether or not the budget was pre-shrunk, so
    /// reverting the width leg of the fix alone does not fail this
    /// particular assertion; the row leg above is what the disconfirm run
    /// actually falsifies. The width leg is still correct to keep: it
    /// makes the selected budget equal the interior by construction
    /// rather than by this clamp's saturating-arithmetic coincidence, so
    /// it stays correct if that anchor formula ever changes.)
    #[test]
    fn messages_toast_shows_the_widest_lines_final_interior_cell_at_the_width_clamp_boundary() {
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize {
            width: 10,
            height: 5,
        });
        apply(
            &mut model,
            view_core::events::UiEvent::MsgShow {
                kind: "echomsg".into(),
                content: vec![(0, "123456789".into())],
                replace_last: false,
            },
        );

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(10, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        let messages = surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::Messages(_)))
            .expect("messages layer present");
        let (x, y, w) = (messages.rect.col, messages.rect.row, messages.rect.width);
        assert_eq!(
            w, 10,
            "box must span the full grid width, not overflow past it"
        );

        let interior_row: String = (x + 1..x + w - 1)
            .map(|c| buf[(c, y + 1)].symbol().to_string())
            .collect();
        assert_eq!(
            interior_row, "12345678",
            "the interior's 8 cells must show the line's own first 8 characters, ending at '8', not fewer"
        );
    }

    #[test]
    fn framed_toast_renders_border_glyphs_on_all_four_edges() {
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize {
            width: 10,
            height: 5,
        });
        apply(
            &mut model,
            view_core::events::UiEvent::MsgShow {
                kind: "echomsg".into(),
                content: vec![(0, "hi".into())],
                replace_last: false,
            },
        );

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(10, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        // interior "hi" (2 wide, 1 tall) framed with a 2-cell border: box
        // is 4 wide x 3 tall, right-anchored at col 6 (10 - 4), top-
        // anchored at row 0 -- corners, horizontal edges, and vertical
        // edges must all be distinct border glyphs, not blank/text cells
        assert_eq!(&buf[(6, 0)].symbol(), &"┌", "top-left corner");
        assert_eq!(&buf[(9, 0)].symbol(), &"┐", "top-right corner");
        assert_eq!(&buf[(6, 2)].symbol(), &"└", "bottom-left corner");
        assert_eq!(&buf[(9, 2)].symbol(), &"┘", "bottom-right corner");
        assert_eq!(&buf[(7, 0)].symbol(), &"─", "top edge");
        assert_eq!(&buf[(8, 0)].symbol(), &"─", "top edge");
        assert_eq!(&buf[(7, 2)].symbol(), &"─", "bottom edge");
        assert_eq!(&buf[(8, 2)].symbol(), &"─", "bottom edge");
        assert_eq!(&buf[(6, 1)].symbol(), &"│", "left edge");
        assert_eq!(&buf[(9, 1)].symbol(), &"│", "right edge");
    }

    /// A theme with no `MsgArea` foreground -- e.g. a colorscheme that
    /// only sets `guibg` on `MsgArea`, or a pre-attach/no-colorscheme
    /// `Theme::default()` -- must still get a visible border. Disconfirm:
    /// reverting `message_border_color` to its pre-fix `.or(msg_area.bg)`
    /// chain and setting `msg_area.bg` to black here makes this assert
    /// `0` instead of the grey constant -- an invisible black-on-black
    /// frame around a black background.
    #[test]
    fn message_border_color_falls_back_to_neutral_grey_never_a_dimmed_background() {
        let theme = Theme::default();
        assert_eq!(
            message_border_color(&theme),
            0x0080_8080,
            "no msg_area foreground at all must fall back to the plain (undimmed) grey constant"
        );

        let bg_only = Theme {
            msg_area: ResolvedStyle {
                bg: Some(0x0000_0000),
                ..ResolvedStyle::default()
            },
            ..Theme::default()
        };
        assert_eq!(
            message_border_color(&bg_only),
            0x0080_8080,
            "a background-only theme must not derive the border from a dimmed bg -- \
             dimming black yields black, an invisible frame on its own background"
        );
    }

    #[test]
    fn message_border_color_dims_a_set_msg_area_foreground() {
        let theme = Theme {
            msg_area: ResolvedStyle {
                fg: Some(0x00FF_0000),
                ..ResolvedStyle::default()
            },
            ..Theme::default()
        };
        assert_eq!(
            message_border_color(&theme),
            0x0099_0000,
            "a set msg_area foreground must still dim to 60%, distinct from the full-brightness interior text"
        );
    }

    #[test]
    fn empty_message_log_paints_nothing() {
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize {
            width: 8,
            height: 4,
        });
        for row in 0..4u16 {
            model.engine.apply_grid(GridOp::PutLine {
                row,
                col_start: 0,
                cells: vec![("Z".into(), 0, 8)],
            });
        }
        let theme = Theme::from_hl(model.engine.hl());
        // an area shaped like a real toast rect would occupy, handed
        // straight to paint_messages with an empty slice: render()'s own
        // contract never emits a Messages layer for an empty log, but this
        // exercises paint_messages' own guard directly rather than relying
        // solely on that upstream omission
        let area = ratatui::layout::Rect {
            x: 2,
            y: 0,
            width: 6,
            height: 3,
        };
        let backend = TestBackend::new(8, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let fa = f.area();
                let buf = f.buffer_mut();
                paint_grid(
                    model.engine.grid(),
                    &theme,
                    model.engine.hl(),
                    fa,
                    &Damage::full(),
                    buf,
                );
                paint_messages(&[], &theme, area, buf);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        for row in 0..4u16 {
            for col in 0..8u16 {
                assert_eq!(
                    &buf[(col, row)].symbol(),
                    &"Z",
                    "empty message log must clear, border, or write nothing at ({col}, {row})"
                );
            }
        }
    }

    #[test]
    fn clear_under_frame_overwrites_grid_content_across_the_whole_framed_rect_including_border_cells(
    ) {
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize {
            width: 10,
            height: 5,
        });
        for row in 0..5u16 {
            model.engine.apply_grid(GridOp::PutLine {
                row,
                col_start: 0,
                cells: vec![("Z".into(), 0, 10)],
            });
        }
        apply(
            &mut model,
            view_core::events::UiEvent::MsgShow {
                kind: "echomsg".into(),
                content: vec![(0, "hi".into())],
                replace_last: false,
            },
        );

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(10, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        // framed box: cols 6..10, rows 0..3 (4 wide x 3 tall) -- every cell
        // in that rect, border cells included, must have been overwritten;
        // none may still show the grid's "Z" stand-in
        for row in 0..3u16 {
            for col in 6..10u16 {
                assert_ne!(
                    &buf[(col, row)].symbol(),
                    &"Z",
                    "grid stand-in must not bleed through the framed rect at ({col}, {row}), border cells included"
                );
            }
        }
    }

    /// Reproduces a real crash: a live nvim with user plugins fed an `emsg`
    /// containing a raw control byte (observed via a `BufNewFile`
    /// autocommand error), which `ratatui`'s low-level `Cell::set_symbol`
    /// does not filter (unlike its safe `Buffer::set_stringn`/`Span`
    /// counterparts) and asserts against in a debug build. Every renderer
    /// that calls `set_symbol` directly must sanitize first.
    #[test]
    fn control_characters_in_message_text_do_not_panic_and_are_sanitized() {
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize {
            width: 10,
            height: 3,
        });
        apply(
            &mut model,
            view_core::events::UiEvent::MsgShow {
                kind: "emsg".into(),
                content: vec![(0, "a\u{7}b".into())], // BEL between two printable chars
                replace_last: false,
            },
        );

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        // interior content "a\u{7}b" is 3 cells wide; the framed box adds
        // the 2-cell border (5 wide total), right-anchored at col 5
        // (10 - 5), so the interior text row lands at (6..9, 1)
        assert_eq!(&buf[(6, 1)].symbol(), &"a");
        assert_eq!(
            &buf[(7, 1)].symbol(),
            &" ",
            "control byte sanitized to a space"
        );
        assert_eq!(&buf[(8, 1)].symbol(), &"b");
    }

    #[test]
    fn control_characters_in_grid_cell_text_do_not_panic_and_are_sanitized() {
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize {
            width: 10,
            height: 1,
        });
        model.engine.apply_grid(GridOp::PutLine {
            row: 0,
            col_start: 0,
            cells: vec![("\u{7}".into(), 0, 1)],
        });
        let surface = view_surface::render(&model);
        let backend = TestBackend::new(10, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert_eq!(&buf[(0, 0)].symbol(), &" ");
    }

    #[test]
    fn popupmenu_overlay_anchors_at_its_grid_coords_and_highlights_selected() {
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize {
            width: 20,
            height: 6,
        });
        apply(
            &mut model,
            view_core::events::UiEvent::PopupmenuShow {
                items: vec![
                    view_core::events::PmItem {
                        word: "foo".into(),
                        ..Default::default()
                    },
                    view_core::events::PmItem {
                        word: "bar".into(),
                        ..Default::default()
                    },
                ],
                selected: 1,
                row: 2,
                col: 3,
                grid: 0,
            },
        );

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(20, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert_eq!(&buf[(3, 2)].symbol(), &"f");
        assert_eq!(&buf[(3, 3)].symbol(), &"b");
        assert!(
            buf[(3, 3)]
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED),
            "selected item must be highlighted"
        );
        assert!(
            !buf[(3, 2)]
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED),
            "unselected item must not be highlighted"
        );

        apply(&mut model, view_core::events::UiEvent::PopupmenuHide);
        let surface = view_surface::render(&model);
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert_eq!(
            &buf[(3, 2)].symbol(),
            &" ",
            "popup menu must vanish once PopupmenuHide clears its state"
        );
    }

    #[test]
    fn shell_paints_a_themed_statusline_row_and_a_waiting_indicator() {
        let mut model = Model::with_term_size(20, 4);
        model.content_painted = false;

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(20, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        // bottom row (3): the statusline placeholder fill, present even
        // though nothing has painted real content there yet
        assert_eq!(&buf[(0, 3)].symbol(), &" ");
        // middle row (height/2 == 2): the waiting indicator's text
        assert_eq!(&buf[(0, 2)].symbol(), &"v");
        assert_eq!(&buf[(1, 2)].symbol(), &"i");
    }

    #[test]
    fn shell_never_paints_once_content_painted_is_true() {
        let model = Model::with_term_size(20, 4);
        assert!(model.content_painted, "default must be the steady state");

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(20, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        assert_eq!(
            &buf[(0, 2)].symbol(),
            &" ",
            "no waiting indicator once content_painted is true"
        );
    }

    // --- Damage-clip equality moat -------------------------------------
    //
    // The disconfirm-capable test for the whole change: a damage-clipped
    // composite (paint only the damaged rows over the previous frame's
    // persistent buffer) must be byte-identical to a full recomposite of
    // the same state. The differential oracle compares model-level
    // text/attr state and cannot see a wrong clip rect; the compat harness
    // asserts text presence, not per-cell repaint. Only this test fails on
    // an under-clip that strands a stale cell.

    /// A full recomposite of `model` into a fresh `area`-sized buffer -- the
    /// reference every clipped frame is measured against.
    fn full_paint(model: &Model, area: ratatui::layout::Rect) -> ratatui::buffer::Buffer {
        let surface = view_surface::render(model);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        composite_into(&mut buf, model, &surface, &Damage::full());
        buf
    }

    /// Drives one state transition through the damage-clipped path exactly
    /// as [`crate::terminal::Term`] does -- full-paint state A into a
    /// persistent shadow, mutate to state B, then clip-composite only B's
    /// damage over the shadow -- and asserts the shadow equals a full
    /// recomposite of B. `mutate` returns nothing; B's grid damage is read
    /// from the grid's own tracker, and its overlay rows from the rendered
    /// surfaces, the same inputs the runtime feeds the real term.
    fn assert_clip_matches_full(
        w_a: u16,
        h_a: u16,
        setup_a: impl FnOnce(&mut Model),
        w_b: u16,
        h_b: u16,
        mutate_b: impl FnOnce(&mut Model),
    ) {
        let area_a = ratatui::layout::Rect::new(0, 0, w_a, h_a);
        let area_b = ratatui::layout::Rect::new(0, 0, w_b, h_b);

        let mut model = Model::new();
        setup_a(&mut model);
        let surf_a = view_surface::render(&model);
        let prev_overlay = overlay_rows(&surf_a);
        let offset_a = model.chrome_rows();
        // clear the damage state A's construction accumulated: the shadow is
        // about to hold A in full, so only B's later damage matters
        let _ = model.take_paint_damage();
        let mut shadow = ratatui::buffer::Buffer::empty(area_a);
        composite_into(&mut shadow, &model, &surf_a, &Damage::full());

        mutate_b(&mut model);
        let grid_damage = model.take_paint_damage();
        let surf_b = view_surface::render(&model);
        let cur_overlay = overlay_rows(&surf_b);
        let offset_b = model.chrome_rows();
        // a chrome-offset change or a paint-area change forces a full repaint,
        // matching Term's own force_full; otherwise clip to B's damage
        let force_full = offset_a != offset_b || area_a != area_b;
        if shadow.area != area_b {
            shadow = ratatui::buffer::Buffer::empty(area_b);
        }
        let damage = if force_full {
            Damage::full()
        } else {
            Damage::from_frame(&grid_damage, offset_b, &prev_overlay, &cur_overlay, false)
        };
        composite_into(&mut shadow, &model, &surf_b, &damage);

        let full = full_paint(&model, area_b);
        assert_eq!(
            shadow, full,
            "damage-clipped composite diverged from a full recomposite"
        );
    }

    /// The multi-frame counterpart to [`assert_clip_matches_full`], which
    /// only ever drives one transition. [`Shadow`]'s buffers swap instead of
    /// copying, so a frame composites into the buffer holding the frame
    /// *before* last, and must therefore repaint the previous frame's rows
    /// as well as its own. This drives a sequence whose damaged rows differ
    /// from frame to frame and checks the promoted front buffer against a
    /// full recomposite after every one: a missing carry-forward strands the
    /// row an earlier frame changed, which no single-transition check sees.
    #[test]
    fn shadow_front_matches_full_recomposite_every_frame() {
        let area = ratatui::layout::Rect::new(0, 0, 40, 12);
        let mut model = Model::new();
        seed_grid(&mut model, 40, 12);
        let mut shadow = Shadow::new();
        assert!(shadow.resize(area), "a fresh shadow must size itself");

        // each step damages different rows from the one before it, so the
        // buffer being composited into is always stale somewhere the current
        // frame's own damage does not cover
        type Step = (&'static str, Box<dyn Fn(&mut Model)>);
        let steps: Vec<Step> = vec![
            ("first paint", Box::new(|_: &mut Model| {})),
            (
                "edit row 3",
                Box::new(|m: &mut Model| {
                    m.engine.apply_grid(GridOp::PutLine {
                        row: 3,
                        col_start: 2,
                        cells: vec![("Z".into(), 1, 1)],
                    });
                }),
            ),
            (
                "edit row 7",
                Box::new(|m: &mut Model| {
                    m.engine.apply_grid(GridOp::PutLine {
                        row: 7,
                        col_start: 5,
                        cells: vec![("Q".into(), 2, 1)],
                    });
                }),
            ),
            (
                "overlay appears",
                Box::new(|m: &mut Model| {
                    apply(
                        m,
                        view_core::events::UiEvent::MsgShow {
                            kind: "echomsg".into(),
                            content: vec![(0, "a toast".into())],
                            replace_last: false,
                        },
                    );
                }),
            ),
            (
                "overlay vanishes",
                Box::new(|m: &mut Model| {
                    apply(m, view_core::events::UiEvent::MsgClear);
                }),
            ),
            (
                "scroll",
                Box::new(|m: &mut Model| {
                    m.engine.apply_grid(GridOp::Scroll {
                        top: 2,
                        bot: 10,
                        left: 0,
                        right: 40,
                        rows: 3,
                    });
                }),
            ),
            (
                "edit row 0",
                Box::new(|m: &mut Model| {
                    m.engine.apply_grid(GridOp::PutLine {
                        row: 0,
                        col_start: 0,
                        cells: vec![("W".into(), 3, 1)],
                    });
                }),
            ),
        ];

        let mut prev_overlay: Vec<u16> = Vec::new();
        let mut first = true;
        for (label, mutate) in steps {
            mutate(&mut model);
            let grid_damage = model.take_paint_damage();
            let surface = view_surface::render(&model);
            let cur_overlay = overlay_rows(&surface);
            let damage = Damage::from_frame(
                &grid_damage,
                model.chrome_rows(),
                &prev_overlay,
                &cur_overlay,
                first,
            );
            first = false;
            prev_overlay = cur_overlay;
            shadow.compose(&model, &surface, &damage);
            shadow.commit();
            assert_eq!(
                shadow.front(),
                &full_paint(&model, area),
                "shadow diverged from a full recomposite after: {label}"
            );
        }
    }

    /// A double-width symbol occupies the cell to its right, so that cell
    /// must never be emitted as its own update: the crossterm backend omits
    /// the cursor move for a cell one column right of the last one written,
    /// which would print it one column past the wide glyph and over the cell
    /// after it. Pins the delegation to `ratatui`'s own buffer diff -- a
    /// hand-rolled per-cell scan passes every ASCII test and corrupts this.
    #[test]
    fn wide_symbol_trailing_cell_is_never_emitted_separately() {
        let mut shadow = Shadow::new();
        shadow.resize(ratatui::layout::Rect::new(0, 0, 4, 1));
        // what the terminal shows: four narrow cells, so the wide symbol's
        // trailing cell differs from what is on screen. A blank there would
        // let a per-cell scan skip it for the wrong reason and pass.
        for (col, symbol) in ["a", "b", "c", "d"].iter().enumerate() {
            shadow.front[(col as u16, 0)].set_symbol(symbol);
        }
        // the frame just composed: a wide symbol covering columns 0 and 1
        shadow.back[(0, 0)].set_symbol("界");
        shadow.back[(1, 0)].reset();
        shadow.back[(2, 0)].set_symbol("x");
        shadow.back[(3, 0)].set_symbol("d");

        let updates: Vec<(u16, u16)> = shadow.updates().map(|(x, y, _)| (x, y)).collect();

        assert!(
            updates.contains(&(0, 0)),
            "the wide symbol itself must be emitted, got {updates:?}"
        );
        assert!(
            !updates.contains(&(1, 0)),
            "the wide symbol's trailing cell must not be emitted, got {updates:?}"
        );
        assert!(
            updates.contains(&(2, 0)),
            "the cell after the wide symbol must still be emitted, got {updates:?}"
        );
    }

    // --- Clipped-diff equality moat ------------------------------------
    //
    // The clip that keeps the diff off the ~4800 cells a frame did not
    // repaint is only sound if it emits what the whole-buffer diff emits.
    // Nothing else in the tree can see that: `assert_clip_matches_full`
    // compares composited *buffers*, not the update stream; the differential
    // oracle compares model-level text and attributes; the compat harness
    // asserts text presence in a vt100 capture. A clip that skipped or
    // duplicated an update, or emitted a wide glyph's trailing column, would
    // pass all three and corrupt a real terminal.

    /// A `CrosstermBackend` writer whose bytes stay readable after the
    /// backend has moved it, since `CrosstermBackend`'s own writer accessor
    /// is behind an unstable feature gate.
    #[derive(Clone, Default)]
    struct ByteSink(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

    impl std::io::Write for ByteSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The bytes `draw` puts on the wire through a real `CrosstermBackend`,
    /// which is what a terminal ultimately receives: cursor moves included,
    /// so an update stream that visits the same cells in a different order is
    /// a mismatch here, not a pass.
    fn drawn_bytes(
        draw: impl FnOnce(&mut ratatui::backend::CrosstermBackend<ByteSink>) -> std::io::Result<()>,
    ) -> Vec<u8> {
        let sink = ByteSink::default();
        let mut backend = ratatui::backend::CrosstermBackend::new(sink.clone());
        draw(&mut backend).unwrap();
        let bytes = sink.0.borrow().clone();
        bytes
    }

    /// Asserts the row-clipped emission is byte-identical to the whole-buffer
    /// diff of the same two buffers, on both the forced-clip path and the
    /// path [`Shadow::emit_updates`] picks for itself. Leaves `shadow`
    /// untouched, so a caller can go on to `commit` and drive another frame.
    fn assert_clipped_emission_matches_unclipped(shadow: &mut Shadow, label: &str) {
        let expected = drawn_bytes(|backend| backend.draw(shadow.updates()));

        let mut runs = Vec::new();
        shadow.painted.row_runs(shadow.front.area, &mut runs);
        let clipped = drawn_bytes(|backend| shadow.emit_clipped(backend, &runs));
        assert_eq!(
            clipped, expected,
            "row-clipped emission diverged from the whole-buffer diff after: {label}"
        );

        let chosen = drawn_bytes(|backend| shadow.emit_updates(backend));
        assert_eq!(
            chosen, expected,
            "emit_updates diverged from the whole-buffer diff after: {label}"
        );
    }

    /// Writes `cells` into `row` starting at `col`, one grid cell each.
    fn put(model: &mut Model, row: u16, col: u16, cells: &[&str]) {
        model.engine.apply_grid(GridOp::PutLine {
            row,
            col_start: col,
            cells: cells
                .iter()
                .map(|text| ((*text).to_string(), 4, 1))
                .collect(),
        });
    }

    /// Seeds a grid whose every row carries the shapes a per-cell diff gets
    /// wrong: a CJK ideograph, an emoji with a VS16 presentation selector, a
    /// ZWJ sequence, a three-column halfwidth-dakuten cluster, and a wide
    /// glyph in the row's final column. Each wide glyph is followed by the
    /// blank cells nvim itself sends for the columns the glyph covers.
    fn seed_wide_grid(model: &mut Model, width: u16, height: u16) {
        model.engine.apply_grid(GridOp::Resize { width, height });
        let row_cells = [
            "a",
            "界",
            " ",
            "\u{2764}\u{FE0F}",
            " ",
            "\u{3042}\u{FF9E}",
            " ",
            " ",
            "\u{1F468}\u{200D}\u{1F4BB}",
            " ",
            "b",
            "界",
        ];
        for row in 0..height {
            model.engine.apply_grid(GridOp::PutLine {
                row,
                col_start: 0,
                cells: row_cells
                    .iter()
                    .map(|text| ((*text).to_string(), u64::from(row % 5), 1))
                    .collect(),
            });
        }
    }

    #[test]
    fn clipped_emission_matches_the_unclipped_diff_over_wide_glyph_content() {
        let area = ratatui::layout::Rect::new(0, 0, 12, 8);
        let mut model = Model::new();
        seed_wide_grid(&mut model, 12, 8);
        let mut shadow = Shadow::new();
        assert!(shadow.resize(area), "a fresh shadow must size itself");

        type Step = (&'static str, Box<dyn Fn(&mut Model)>);
        let steps: Vec<Step> = vec![
            ("first paint", Box::new(|_: &mut Model| {})),
            (
                "narrow edit on the row under one ending in a wide glyph",
                Box::new(|m: &mut Model| put(m, 3, 0, &["Z"])),
            ),
            (
                "a wide glyph replaced by narrow text",
                Box::new(|m: &mut Model| put(m, 4, 2, &["q", "r"])),
            ),
            (
                "narrow text replaced by a wide glyph",
                Box::new(|m: &mut Model| put(m, 5, 6, &["界", " "])),
            ),
            (
                "a VS16 emoji landing mid-row",
                Box::new(|m: &mut Model| put(m, 6, 4, &["\u{2764}\u{FE0F}", " "])),
            ),
            (
                "a wide glyph written into the final column",
                Box::new(|m: &mut Model| put(m, 2, 11, &["界"])),
            ),
            (
                "the first cell of the row under a wide row end",
                Box::new(|m: &mut Model| put(m, 7, 0, &["W"])),
            ),
            (
                "two non-adjacent rows in one frame",
                Box::new(|m: &mut Model| {
                    put(m, 1, 0, &["1"]);
                    put(m, 6, 0, &["6"]);
                }),
            ),
            (
                "a three-column cluster overflowing a row's last two columns",
                Box::new(|m: &mut Model| put(m, 4, 10, &["\u{3042}\u{FF9E}", " "])),
            ),
            // an unrelated frame in between, so the overflowing row is not
            // carried into the next frame's repaint set and the run below it
            // starts at a row boundary the overflow reaches across
            (
                "an unrelated row",
                Box::new(|m: &mut Model| put(m, 0, 0, &["F"])),
            ),
            (
                "the first cell of the row under that overflow",
                Box::new(|m: &mut Model| put(m, 5, 0, &["V"])),
            ),
        ];

        let mut prev_overlay: Vec<u16> = Vec::new();
        let mut first = true;
        for (label, mutate) in steps {
            mutate(&mut model);
            let grid_damage = model.take_paint_damage();
            let surface = view_surface::render(&model);
            let cur_overlay = overlay_rows(&surface);
            let damage = Damage::from_frame(
                &grid_damage,
                model.chrome_rows(),
                &prev_overlay,
                &cur_overlay,
                first,
            );
            first = false;
            prev_overlay = cur_overlay;
            shadow.compose(&model, &surface, &damage);
            assert_clipped_emission_matches_unclipped(&mut shadow, label);
            shadow.commit();
            // the byte comparison above reads both paths out of the same two
            // buffers, so a clip that lifted rows out of them and failed to
            // put them back would agree with itself. Only this catches that:
            // it measures the shadow against the model instead.
            assert_eq!(
                shadow.front(),
                &full_paint(&model, area),
                "emitting updates left the shadow holding something other than \
                 the frame it composed, after: {label}"
            );
        }
    }

    #[test]
    fn a_symbol_wider_than_the_columns_left_in_its_row_is_painted_blank() {
        let area = ratatui::layout::Rect::new(0, 0, 6, 2);
        // three display columns by `ratatui`'s own cell_width: `unicode-width`
        // calls the halfwidth voiced sound mark zero-width, and `ratatui` adds
        // a column back for it because terminals render it as its own cell.
        // A check that only looked at the final column would miss this one.
        let three_wide = "\u{3042}\u{FF9E}";
        for (col, symbol, why) in [
            (5, "界", "two columns wide with one column left"),
            (5, three_wide, "three columns wide with one column left"),
            (4, three_wide, "three columns wide with two columns left"),
        ] {
            let mut model = Model::new();
            model.engine.apply_grid(GridOp::Resize {
                width: 6,
                height: 2,
            });
            put(&mut model, 0, col, &[symbol]);
            let buf = full_paint(&model, area);
            assert_eq!(
                buf[(col, 0)].symbol(),
                " ",
                "a glyph that overflows its row has nowhere to go ({why}), and \
                 leaving it there makes the diff skip cells of the row below"
            );
        }

        // the same symbol one column earlier fills the row exactly, and must
        // survive: this is a fit check, not a ban on wide glyphs near the edge
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize {
            width: 6,
            height: 2,
        });
        put(&mut model, 0, 3, &[three_wide]);
        let buf = full_paint(&model, area);
        assert_eq!(
            buf[(3, 0)].symbol(),
            three_wide,
            "a glyph filling exactly the columns left must still be painted"
        );
    }

    /// Every grapheme class the wire carries, at every column of a row,
    /// against the rule the clip's exactness rests on: blank exactly when the
    /// symbol's `cell_width` exceeds the columns left, and never otherwise.
    ///
    /// The reference is the rule itself rather than the implementation, so
    /// this pins both directions -- a check that blanks too eagerly loses a
    /// glyph the terminal could have shown, and one that blanks too late
    /// lets the symbol overflow its row and strand the cells below it. Both
    /// the helper and the painted frame are checked, because the painter is
    /// what computes the columns left from the buffer's own rect.
    ///
    /// Disconfirm: dropping [`fitted_symbol`]'s width test and blanking on
    /// the byte-length bound alone fails the over-blanking direction (a VS16
    /// emoji spends six bytes on two columns); loosening that bound by two
    /// bytes fails the under-blanking direction (a CJK ideograph spends three
    /// bytes on two columns, so the width test stops running while the glyph
    /// still overflows).
    #[test]
    fn the_fit_check_blanks_exactly_the_symbols_too_wide_for_the_columns_left() {
        let symbols = [
            ("ASCII", "a"),
            ("CJK", "界"),
            ("VS16 emoji", "\u{2764}\u{FE0F}"),
            ("ZWJ family", "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"),
            ("halfwidth dakuten cluster", "\u{3042}\u{FF9E}"),
            ("regional indicator flag", "\u{1F1EF}\u{1F1F5}"),
            ("combining mark", "e\u{0301}"),
            ("blank", " "),
        ];
        const WIDTH: u16 = 6;
        let area = ratatui::layout::Rect::new(0, 0, WIDTH, 2);

        for (name, symbol) in symbols {
            let width = symbol.cell_width();
            for columns_left in 0..=WIDTH + 1 {
                let fits = width <= columns_left;
                let expected = if fits { symbol } else { " " };
                assert_eq!(
                    fitted_symbol(symbol, columns_left),
                    expected,
                    "{name} is {width} columns wide with {columns_left} left"
                );
            }

            for col in 0..WIDTH {
                let mut model = Model::new();
                model.engine.apply_grid(GridOp::Resize {
                    width: WIDTH,
                    height: 2,
                });
                put(&mut model, 0, col, &[symbol]);
                let buf = full_paint(&model, area);
                let expected = if width <= WIDTH - col { symbol } else { " " };
                assert_eq!(
                    buf[(col, 0)].symbol(),
                    expected,
                    "{name} is {width} columns wide, painted at column {col} of {WIDTH}"
                );
            }
        }
    }

    /// A shadow whose rows are staged is mid-surgery: its buffers hold the
    /// scratch's cells, so a frame that abandoned the stage would leave every
    /// later diff comparing against those. The rows must come back however
    /// the staged scope ends, including by unwinding out of the backend.
    ///
    /// Disconfirm: emptying `StagedRuns`'s `Drop` body leaves the shadow
    /// holding blank scratch rows and fails the comparison below.
    #[test]
    fn a_panic_while_the_shadow_is_staged_still_puts_its_rows_back() {
        let area = ratatui::layout::Rect::new(0, 0, 12, 8);
        let mut model = Model::new();
        seed_wide_grid(&mut model, 12, 8);
        let mut shadow = Shadow::new();
        assert!(shadow.resize(area), "a fresh shadow must size itself");
        let surface = view_surface::render(&model);
        shadow.compose(&model, &surface, &Damage::full());
        shadow.commit();
        let intact = shadow.front().clone();

        let runs = vec![(2_u16, 2_u16)];
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _staged = StagedRuns::stage(&mut shadow, &runs);
            // stands in for an unwinding panic anywhere inside the backend's
            // write, which is the whole window in which the rows are lifted
            panic!("backend write");
        }));
        std::panic::set_hook(hook);

        assert!(unwound.is_err(), "the panic must have crossed the guard");
        assert_eq!(
            shadow.front(),
            &intact,
            "an unwind out of the staged scope must still restore the shadow, \
             or every later frame diffs against scratch cells"
        );
    }

    /// A run reaching past the shadow's cells cannot be swapped, and skipping
    /// it would drop that run's updates from the frame with nothing said. The
    /// guard is unreachable through [`Damage::row_runs`], so it is reached
    /// here directly.
    ///
    /// Disconfirm: removing the `debug_assert!` beside the `continue` makes
    /// the skip silent again, and this test fails for want of the panic it
    /// expects.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "past the")]
    fn a_staged_run_reaching_past_the_shadow_fails_loudly_rather_than_silently() {
        let area = ratatui::layout::Rect::new(0, 0, 12, 8);
        let mut shadow = Shadow::new();
        assert!(shadow.resize(area), "a fresh shadow must size itself");
        // sizes one scratch slot, so the out-of-range run below has a slot to
        // zip against
        drop(StagedRuns::stage(&mut shadow, &[(2, 2)]));
        shadow.swap_runs(&[(area.height, 2)]);
    }

    #[test]
    fn row_runs_groups_adjacent_rows_and_splits_at_gaps() {
        let area = ratatui::layout::Rect::new(0, 0, 4, 8);
        let damage = Damage {
            full: false,
            rows: vec![6, 1, 2, 4, 99],
        };
        let mut runs = Vec::new();
        damage.row_runs(area, &mut runs);
        assert_eq!(
            runs,
            vec![(1, 2), (4, 1), (6, 1)],
            "runs must be ascending, merged where adjacent, and clipped to the area"
        );

        Damage::full().row_runs(area, &mut runs);
        assert_eq!(runs, vec![(0, 8)], "full damage is one run over every row");
    }

    /// Seeds every row of `model`'s grid with distinct full-width text, so a
    /// stranded stale cell from an under-clip is a visible mismatch rather
    /// than two default spaces comparing equal by accident.
    fn seed_grid(model: &mut Model, width: u16, height: u16) {
        model.engine.apply_grid(GridOp::Resize { width, height });
        for row in 0..height {
            let cells = (0..width)
                .map(|col| {
                    let ch = char::from(b'a' + ((row + col) % 26) as u8);
                    (ch.to_string(), u64::from((row + col) % 5), 1)
                })
                .collect();
            model.engine.apply_grid(GridOp::PutLine {
                row,
                col_start: 0,
                cells,
            });
        }
    }

    #[test]
    fn clip_matches_full_single_cell_change() {
        assert_clip_matches_full(
            40,
            12,
            |m| seed_grid(m, 40, 12),
            40,
            12,
            |m| {
                m.engine.apply_grid(GridOp::PutLine {
                    row: 5,
                    col_start: 7,
                    cells: vec![("Z".into(), 2, 1)],
                });
            },
        );
    }

    #[test]
    fn clip_matches_full_multi_row_span_change() {
        assert_clip_matches_full(
            40,
            12,
            |m| seed_grid(m, 40, 12),
            40,
            12,
            |m| {
                for row in 3..=6 {
                    m.engine.apply_grid(GridOp::PutLine {
                        row,
                        col_start: 0,
                        cells: vec![("Q".into(), 1, 40)],
                    });
                }
            },
        );
    }

    #[test]
    fn clip_matches_full_scroll_region() {
        assert_clip_matches_full(
            40,
            12,
            |m| seed_grid(m, 40, 12),
            40,
            12,
            |m| {
                m.engine.apply_grid(GridOp::Scroll {
                    top: 2,
                    bot: 10,
                    left: 0,
                    right: 40,
                    rows: 3,
                });
            },
        );
    }

    #[test]
    fn clip_matches_full_overlay_appears() {
        assert_clip_matches_full(
            40,
            12,
            |m| seed_grid(m, 40, 12),
            40,
            12,
            |m| {
                apply(
                    m,
                    view_core::events::UiEvent::MsgShow {
                        kind: "echomsg".into(),
                        content: vec![(0, "a toast".into())],
                        replace_last: false,
                    },
                );
            },
        );
    }

    #[test]
    fn clip_matches_full_overlay_disappears() {
        assert_clip_matches_full(
            40,
            12,
            |m| {
                seed_grid(m, 40, 12);
                apply(
                    m,
                    view_core::events::UiEvent::MsgShow {
                        kind: "echomsg".into(),
                        content: vec![(0, "a toast".into())],
                        replace_last: false,
                    },
                );
            },
            40,
            12,
            |m| apply(m, view_core::events::UiEvent::MsgClear),
        );
    }

    #[test]
    fn clip_matches_full_overlay_moves() {
        assert_clip_matches_full(
            40,
            12,
            |m| {
                seed_grid(m, 40, 12);
                apply(
                    m,
                    view_core::events::UiEvent::PopupmenuShow {
                        items: vec![
                            view_core::events::PmItem {
                                word: "alpha".into(),
                                ..Default::default()
                            },
                            view_core::events::PmItem {
                                word: "beta".into(),
                                ..Default::default()
                            },
                        ],
                        selected: 0,
                        row: 2,
                        col: 4,
                        grid: 0,
                    },
                );
            },
            40,
            12,
            |m| {
                // hide-then-show at a new anchor: the popupmenu jumps from
                // row 2 to row 7, vacating its old rows
                apply(m, view_core::events::UiEvent::PopupmenuHide);
                apply(
                    m,
                    view_core::events::UiEvent::PopupmenuShow {
                        items: vec![
                            view_core::events::PmItem {
                                word: "alpha".into(),
                                ..Default::default()
                            },
                            view_core::events::PmItem {
                                word: "beta".into(),
                                ..Default::default()
                            },
                        ],
                        selected: 1,
                        row: 7,
                        col: 4,
                        grid: 0,
                    },
                );
            },
        );
    }

    /// Sends one probe reply for the highlight table's current generation,
    /// the shape `view-engine` produces when `nvim_get_hl` answers the probe
    /// a `default_colors_set` triggered. A `Msg`, not a `UiEvent`, so it
    /// cannot go through `apply`.
    fn probe_reply(model: &mut Model, fg: Option<u32>, bg: Option<u32>) {
        let generation = model.engine.hl().probe_generation();
        let _ = view_core::update::update(
            model,
            view_core::msg::Msg::HlProbeReply { generation, fg, bg },
        );
    }

    #[test]
    fn clip_matches_full_default_colors_change() {
        assert_clip_matches_full(
            40,
            12,
            |m| {
                seed_grid(m, 40, 12);
                apply(
                    m,
                    view_core::events::UiEvent::DefaultColorsSet {
                        fg: Some(0xF8F8F2),
                        bg: Some(0x101010),
                        sp: None,
                    },
                );
            },
            40,
            12,
            |m| {
                // every cell resolves its colors through the defaults, so a
                // colorscheme swap restyles the whole screen without any grid
                // cell's text changing
                apply(
                    m,
                    view_core::events::UiEvent::DefaultColorsSet {
                        fg: Some(0x202020),
                        bg: Some(0x445566),
                        sp: None,
                    },
                );
            },
        );
    }

    #[test]
    fn clip_matches_full_hl_attr_redefinition() {
        assert_clip_matches_full(
            40,
            12,
            |m| {
                seed_grid(m, 40, 12);
                apply(
                    m,
                    view_core::events::UiEvent::HlAttrDefine {
                        id: 2,
                        fg: Some(0xFF0000),
                        bg: None,
                        bold: false,
                        italic: false,
                        underline: false,
                        reverse: false,
                    },
                );
            },
            40,
            12,
            // redefining an id already on screen restyles every cell holding
            // it, again with no grid cell changing its text
            |m| {
                apply(
                    m,
                    view_core::events::UiEvent::HlAttrDefine {
                        id: 2,
                        fg: Some(0x00FF00),
                        bg: Some(0x000080),
                        bold: true,
                        italic: false,
                        underline: false,
                        reverse: false,
                    },
                );
            },
        );
    }

    #[test]
    fn clip_matches_full_hl_probe_reply_confirms_a_black_default() {
        assert_clip_matches_full(
            40,
            12,
            |m| {
                seed_grid(m, 40, 12);
                // the wire-ambiguous zero: painted transparent until a probe
                // reply says whether the colorscheme genuinely sets black
                apply(
                    m,
                    view_core::events::UiEvent::DefaultColorsSet {
                        fg: Some(0xF8F8F2),
                        bg: Some(0),
                        sp: None,
                    },
                );
            },
            40,
            12,
            // the reply lands after its own frame has painted, which the
            // paint loop's never-await-RPC contract makes the common case
            |m| probe_reply(m, Some(0xF8F8F2), Some(0)),
        );
    }

    /// A highlight-only frame interposed after a partially damaged one: the
    /// hole [`Shadow`]'s carry-forward can mask for exactly one frame.
    ///
    /// Frame two damages a single row, so frame three composites into a
    /// buffer whose every other row is two frames old. If a highlight change
    /// produced no damage, that third frame would repaint the carried row
    /// alone and leave the rest of the screen in the previous theme's colors
    /// -- one restyled stripe on an otherwise stale screen, which no
    /// single-transition check and no first-frame check can see.
    #[test]
    fn shadow_front_matches_full_recomposite_across_a_highlight_only_frame() {
        let area = ratatui::layout::Rect::new(0, 0, 40, 12);
        let mut model = Model::new();
        seed_grid(&mut model, 40, 12);
        apply(
            &mut model,
            view_core::events::UiEvent::DefaultColorsSet {
                fg: Some(0xF8F8F2),
                bg: Some(0),
                sp: None,
            },
        );
        let mut shadow = Shadow::new();
        assert!(shadow.resize(area), "a fresh shadow must size itself");

        type Step = (&'static str, Box<dyn Fn(&mut Model)>);
        let steps: Vec<Step> = vec![
            ("first paint", Box::new(|_: &mut Model| {})),
            (
                "edit row 4 only",
                Box::new(|m: &mut Model| {
                    m.engine.apply_grid(GridOp::PutLine {
                        row: 4,
                        col_start: 3,
                        cells: vec![("K".into(), 1, 1)],
                    });
                }),
            ),
            (
                "probe reply confirms black, no grid change",
                Box::new(|m: &mut Model| probe_reply(m, Some(0xF8F8F2), Some(0))),
            ),
            (
                "edit row 9 only",
                Box::new(|m: &mut Model| {
                    m.engine.apply_grid(GridOp::PutLine {
                        row: 9,
                        col_start: 1,
                        cells: vec![("J".into(), 3, 1)],
                    });
                }),
            ),
            (
                "colorscheme swap, no grid change",
                Box::new(|m: &mut Model| {
                    apply(
                        m,
                        view_core::events::UiEvent::DefaultColorsSet {
                            fg: Some(0x1A1A1A),
                            bg: Some(0xEEEEEE),
                            sp: None,
                        },
                    );
                }),
            ),
            (
                "redefine an on-screen highlight id",
                Box::new(|m: &mut Model| {
                    apply(
                        m,
                        view_core::events::UiEvent::HlAttrDefine {
                            id: 3,
                            fg: Some(0x00FF00),
                            bg: Some(0x000080),
                            bold: true,
                            italic: false,
                            underline: false,
                            reverse: false,
                        },
                    );
                }),
            ),
        ];

        let mut prev_overlay: Vec<u16> = Vec::new();
        let mut first = true;
        for (label, mutate) in steps {
            mutate(&mut model);
            let grid_damage = model.take_paint_damage();
            let surface = view_surface::render(&model);
            let cur_overlay = overlay_rows(&surface);
            let damage = Damage::from_frame(
                &grid_damage,
                model.chrome_rows(),
                &prev_overlay,
                &cur_overlay,
                first,
            );
            first = false;
            prev_overlay = cur_overlay;
            shadow.compose(&model, &surface, &damage);
            shadow.commit();
            assert_eq!(
                shadow.front(),
                &full_paint(&model, area),
                "shadow diverged from a full recomposite after: {label}"
            );
        }
    }

    #[test]
    fn clip_matches_full_grid_clear_is_full_damage() {
        assert_clip_matches_full(
            40,
            12,
            |m| seed_grid(m, 40, 12),
            40,
            12,
            |m| m.engine.apply_grid(GridOp::Clear),
        );
    }

    #[test]
    fn clip_matches_full_empty_damage_is_a_noop_frame() {
        // no mutation at all: the shadow already holds the correct state, and
        // an empty damage must leave it byte-identical to a full recomposite
        assert_clip_matches_full(40, 12, |m| seed_grid(m, 40, 12), 40, 12, |_| {});
    }

    #[test]
    fn clip_matches_full_resize_forces_full_repaint() {
        assert_clip_matches_full(
            40,
            12,
            |m| seed_grid(m, 40, 12),
            48,
            16,
            |m| seed_grid(m, 48, 16),
        );
    }

    #[test]
    fn clip_matches_full_tabline_offset_shift_forces_full_repaint() {
        assert_clip_matches_full(
            40,
            12,
            |m| seed_grid(m, 40, 12),
            40,
            12,
            |m| {
                // a second tab reserves the chrome row, shifting every grid
                // row down by one -- a chrome-offset change Term repaints in
                // full rather than clipping
                apply(
                    m,
                    view_core::events::UiEvent::TablineUpdate {
                        current: view_core::events::TabHandle(1),
                        tabs: vec![
                            view_core::events::TabEntry {
                                tab: view_core::events::TabHandle(1),
                                name: "one".into(),
                            },
                            view_core::events::TabEntry {
                                tab: view_core::events::TabHandle(2),
                                name: "two".into(),
                            },
                        ],
                    },
                );
            },
        );
    }

    /// Disconfirm control: a deliberately under-clipped `Damage` (the changed
    /// row omitted) must make the equality assertion fail. Proves the moat
    /// can actually catch a wrong clip rect rather than passing vacuously;
    /// run `#[ignore]`d so it does not fail the suite, and flipped to a real
    /// failing run by hand to capture the mismatch evidence.
    #[test]
    #[ignore = "disconfirm control: passes only when the clip is broken"]
    fn under_clip_that_omits_the_changed_row_is_caught() {
        let area = ratatui::layout::Rect::new(0, 0, 40, 12);
        let mut model = Model::new();
        seed_grid(&mut model, 40, 12);
        let _ = model.take_paint_damage();
        let surf_a = view_surface::render(&model);
        let mut shadow = ratatui::buffer::Buffer::empty(area);
        composite_into(&mut shadow, &model, &surf_a, &Damage::full());

        model.engine.apply_grid(GridOp::PutLine {
            row: 5,
            col_start: 7,
            cells: vec![("Z".into(), 2, 1)],
        });
        let _grid_damage = model.take_paint_damage();
        let surf_b = view_surface::render(&model);
        // sabotage: an EMPTY damage, dropping the changed row 5 -- the clip
        // paints nothing, so the shadow keeps row 5's stale cell. Built
        // field-wise because no constructor produces it: `Damage::default`
        // is whole-frame precisely so this value cannot be reached by
        // accident.
        let sabotaged = Damage {
            full: false,
            rows: Vec::new(),
        };
        composite_into(&mut shadow, &model, &surf_b, &sabotaged);

        let full = full_paint(&model, area);
        assert_eq!(
            shadow, full,
            "under-clip must be caught by the equality moat"
        );
    }
}
