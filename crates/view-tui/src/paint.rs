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
use view_core::native::speculate::PredictedCell;
use view_core::native::views::{Span, StyleRole};
use view_core::theme::{ChromeGroup, ResolvedStyle, Theme};
use view_surface::{Layer, LayerKind, Rect, Surface};

/// The terminal-space rows a frame's composite must repaint, so a redraw
/// touches only the changed region instead of all ~4800 cells.
///
/// Rows are in terminal (post-chrome-offset) space, the same space
/// [`ratatui::buffer::Buffer`] indexes, so [`composite_into`] can test a
/// grid row's painted position directly. `full` supersedes `rows`: a
/// first paint, resize, or chrome-offset change repaints everything.
/// Every layer is clipped to these rows: a layer covering none of them
/// paints nothing at all, and a framed overlay paints only the rows of its
/// own frame that fall inside them. Which rows an overlay contributes is
/// [`OverlayShadow::advance`]'s answer -- the rows it draws differently
/// from the frame on screen -- so a full-height panel answering a
/// composer keystroke costs the row that changed rather than the screen it
/// covers.
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
    /// rows the overlay stack draws differently than the frame on screen,
    /// offsetting grid-space rows by the reserved chrome rows to reach
    /// terminal space.
    ///
    /// `overlay` is [`OverlayShadow::advance`]'s answer, which already
    /// carries every row an overlay transition uncovers: one that appears,
    /// moves, shrinks or vanishes contributes the rows it now covers *and*
    /// the rows it covered last frame, so the grid (or the new overlay
    /// position) repaints underneath the vacated cells. `force_full` (a
    /// chrome-offset change that shifts the whole grid) and a full
    /// [`GridDamage`] (a resize or clear) both collapse to a whole-frame
    /// repaint.
    #[must_use]
    pub fn from_frame(grid: &GridDamage, offset: u16, overlay: &[u16], force_full: bool) -> Self {
        if force_full || grid.full {
            return Self::full();
        }
        let mut rows = Vec::with_capacity(grid.rows.len() + overlay.len());
        rows.extend(grid.rows.iter().map(|&r| r.saturating_add(offset)));
        rows.extend_from_slice(overlay);
        Self { full: false, rows }
    }

    /// Whether terminal-space `row` must repaint (always true when `full`).
    #[must_use]
    pub fn covers(&self, row: u16) -> bool {
        self.full || self.rows.contains(&row)
    }

    /// Whether any row of `area` must repaint.
    #[must_use]
    pub fn covers_any(&self, area: ratatui::layout::Rect) -> bool {
        self.full
            || (area.y..area.y.saturating_add(area.height)).any(|row| self.rows.contains(&row))
    }

    /// Whether this frame repaints the buffer row `row` rows below `area`'s
    /// own top -- the row-level clip every painter that writes more than one
    /// row applies, so a layer only ever writes inside the rows
    /// [`composite_layers`] cleared for it.
    #[must_use]
    pub fn covers_row_of(&self, area: ratatui::layout::Rect, row: u16) -> bool {
        self.covers(area.y.saturating_add(row))
    }

    /// Whether this damage repaints something, and nothing outside `rows`.
    ///
    /// Containment rather than set equality, because the bench taps use it
    /// to attribute a repaint to what it covers: a frame that also repaints
    /// a row outside the region asked about was driven by something else as
    /// well, and attributing it whole to that region would explain away a
    /// paint nothing accounts for. The other direction is not the same
    /// question and must not be asserted -- a panel repaints the rows its
    /// content changed on, which is one row for a composer keystroke and
    /// the whole transcript window for a streamed chunk, and both are
    /// repaints the panel explains.
    #[must_use]
    pub fn covers_only(&self, rows: &[u16]) -> bool {
        !self.full
            && !self.rows.is_empty()
            && !rows.is_empty()
            && self.rows.iter().all(|row| rows.contains(row))
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
    /// The overlay stack as the terminal shows it, which answers both which
    /// overlay rows a frame damages and how the damaged ones lay out.
    overlays: OverlayShadow,
    /// Frames composed, so the debug-build equivalence guard can name the
    /// frame a divergence appeared on.
    #[cfg(debug_assertions)]
    frames: u64,
    /// Whether [`Shadow::overlay_damage`] has run since the last
    /// [`Shadow::compose`], which is the once-per-frame contract it pins.
    #[cfg(debug_assertions)]
    overlay_advanced: bool,
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
        // both buffers are blank now, so nothing the overlay shadow holds is
        // on screen any more: keeping it would let an unchanged layer report
        // no rows for a terminal that is showing none of it
        self.overlays = OverlayShadow::default();
        true
    }

    /// Folds `surface`'s overlay stack in, returning the terminal-space rows
    /// its overlays draw differently than the frame on screen -- the overlay
    /// half of the next [`Damage::from_frame`].
    ///
    /// Call it once per frame, before [`Shadow::compose`]: it is what leaves
    /// the layouts behind for that compose to paint from.
    pub fn overlay_damage(&mut self, surface: &Surface) -> Vec<u16> {
        // `advance` folds the surface in as what the terminal shows, so a
        // second call for the same frame reports no rows at all -- a caller
        // that recomputes damage (to feed a second consumer, say) would
        // silently under-damage the whole frame
        #[cfg(debug_assertions)]
        {
            debug_assert!(
                !self.overlay_advanced,
                "overlay_damage called twice without an intervening compose"
            );
            self.overlay_advanced = true;
        }
        self.overlays.advance(surface)
    }

    /// Composites one frame into the back buffer, repainting `damage`'s rows
    /// plus the rows the previous frame repainted (see the type docs for why
    /// the previous frame's rows are needed).
    pub fn compose(&mut self, model: &Model, surface: &Surface, damage: &Damage) {
        #[cfg(debug_assertions)]
        {
            self.overlay_advanced = false;
        }
        let repaint = damage.union(&self.carried);
        composite_layers(
            &mut self.back,
            model,
            surface,
            &repaint,
            Some(&self.overlays),
        );
        self.carried = damage.clone();
        self.painted = repaint;
        #[cfg(debug_assertions)]
        self.assert_matches_full_recomposite(model, surface);
    }

    /// Asserts the frame just composed is cell-for-cell what a full
    /// recomposite of the same `surface` into a fresh buffer produces,
    /// naming the frame and the first divergent cell.
    ///
    /// Debug builds only, on every frame: a damage set that misses a
    /// changed row leaves the terminal showing something the model no
    /// longer says, and nothing else fails loudly when that happens -- the
    /// stale row looks exactly like an untouched one. Together with the
    /// surface-level guard in `view_surface::cache` (cached surface ==
    /// from-scratch render), this composes into "the painted frame equals a
    /// from-scratch rebuild of the same `Model`": that guard proves the
    /// surface, this one proves the cells painted from it. The release
    /// path is the same clipped composite with only the check compiled
    /// out, never a separate code path.
    #[cfg(debug_assertions)]
    fn assert_matches_full_recomposite(&mut self, model: &Model, surface: &Surface) {
        self.frames = self.frames.wrapping_add(1);
        let mut fresh = Buffer::empty(self.back.area);
        composite_into(&mut fresh, model, surface, &Damage::full());
        let width = usize::from(self.back.area.width.max(1));
        for (i, (got, want)) in self.back.content.iter().zip(&fresh.content).enumerate() {
            if got != want {
                let row = i / width;
                let col = i % width;
                debug_assert!(
                    false,
                    "damage-clipped composite diverged from a full recomposite at \
                     frame {}, cell ({row},{col}): {got:?} != {want:?}",
                    self.frames
                );
                return;
            }
        }
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

/// One overlay layer as the frame on screen painted it: the layer itself,
/// and the rows it was laid out into.
#[derive(Debug, Clone)]
struct PaintedOverlay {
    layer: Layer,
    laid: view_surface::overlay::Rows,
}

/// The overlay stack as the terminal currently shows it, kept so the next
/// frame can ask which of its rows actually change.
///
/// Exists because "this layer is present" and "this layer draws something
/// new" are different questions, and only the second one is damage. The
/// agent panel is the case that makes the difference structural: it is
/// full height, so answering a composer keystroke by dirtying every row it
/// covers re-resolved the whole screen -- every grid cell beside the panel
/// included -- for one changed cell, and that cost grows with the
/// terminal rather than with what the user typed.
///
/// The comparison is two-stage, cheapest first. A layer equal to the one
/// on screen draws identical cells by construction (the layout is a pure
/// function of rect, kind and border set), so an unchanged panel costs one
/// `==` and no layout at all -- which is what a buffer keystroke typed
/// beside an open panel now pays. Only a layer that differs is laid out,
/// and then row by row against the layout on screen, so the damage is the
/// rows whose spans (or selection) moved.
#[derive(Debug, Default)]
pub struct OverlayShadow {
    painted: Vec<PaintedOverlay>,
}

impl OverlayShadow {
    /// Folds `surface` in as what the terminal will show, returning the
    /// terminal-space rows its overlay layers draw differently than the
    /// frame they replace.
    ///
    /// Layers are paired by stack position: a stack that grew, shrank or
    /// reordered pairs a layer against a different one and falls back to
    /// both rects whole, which is conservative in the direction that
    /// repaints too much rather than too little.
    pub fn advance(&mut self, surface: &Surface) -> Vec<u16> {
        let mut rows = Vec::new();
        for gone in self.painted.iter().skip(surface.layers.len()) {
            push_rect_rows(&gone.layer, &mut rows);
        }
        self.painted.truncate(surface.layers.len());
        for (i, layer) in surface.layers.iter().enumerate() {
            // the whole point of the pairing: an unchanged layer draws what
            // is already on screen, so it is neither laid out nor damaged
            if self.painted.get(i).is_some_and(|was| was.layer == *layer) {
                continue;
            }
            // the grid layer stays in the stack so positions keep pairing,
            // but contributes no rows: its own damage already names the
            // rows its cells changed on, and a rect change under it is a
            // resize or a chrome-offset shift, both of which repaint whole
            let contributes = !matches!(layer.kind, LayerKind::EngineGrid);
            let laid = lay_out(layer);
            match self.painted.get_mut(i) {
                Some(was) => {
                    if contributes {
                        push_changed_rows(was, layer, &laid, &mut rows);
                    }
                    *was = PaintedOverlay {
                        layer: layer.clone(),
                        laid,
                    };
                }
                None => {
                    if contributes {
                        push_rect_rows(layer, &mut rows);
                    }
                    self.painted.push(PaintedOverlay {
                        layer: layer.clone(),
                        laid,
                    });
                }
            }
        }
        rows
    }

    /// The layout this shadow holds for the layer at stack position `index`,
    /// when it is the layout of exactly `layer`.
    ///
    /// Lets the painter spend [`OverlayShadow::advance`]'s layout instead of
    /// repeating it: laying a full-height panel out measured 67 us against
    /// 0.7 us for the row compare that decided it was needed, so a frame
    /// that laid the panel out twice spent most of itself there.
    fn laid_for(&self, index: usize, layer: &Layer) -> Option<&view_surface::overlay::Rows> {
        self.painted
            .get(index)
            .filter(|painted| painted.layer == *layer)
            .map(|painted| &painted.laid)
    }
}

/// This layer's rows as the painter will lay them out, empty for a kind
/// that carries no framed rows of its own (the transient overlays, the
/// speculated cells, the engine grid).
fn lay_out(layer: &Layer) -> view_surface::overlay::Rows {
    layer.borders.map_or_else(Default::default, |borders| {
        view_surface::overlay::rows(layer.rect.width, layer.rect.height, &layer.kind, borders)
    })
}

/// Appends every terminal row `layer` covers.
fn push_rect_rows(layer: &Layer, out: &mut Vec<u16>) {
    let first = layer.rect.row;
    let last = first.saturating_add(layer.rect.height);
    for row in first..last {
        if !out.contains(&row) {
            out.push(row);
        }
    }
}

/// Appends the rows on which `now` (already laid out as `laid`) draws
/// something other than what `was` put on screen.
///
/// Falls back to both rects whole where a row-by-row answer would not be
/// sound: a layer that moved or resized vacates cells outside its own new
/// rows, and a kind with no framed layout (a toast, the cmdline, the
/// speculated cells) has no rows to compare -- its content lives in the
/// layer, which already compared unequal to get here.
///
/// The [`LayerKind`] discriminant is deliberately not compared, which is
/// sound only because `view_surface::render` never pushes two different
/// framed kinds at the same stack position: a pair that matched on rect,
/// `framed` and every laid line but differed in kind would keep the old
/// kind's chrome group (a statusline's, say, instead of a float's) on every
/// row judged unchanged. A reorder in `render`'s push order is what would
/// make that reachable.
fn push_changed_rows(
    was: &PaintedOverlay,
    now: &Layer,
    laid: &view_surface::overlay::Rows,
    out: &mut Vec<u16>,
) {
    if was.layer.rect != now.rect
        || was.laid.framed != laid.framed
        || laid.lines.is_empty()
        || was.laid.lines.is_empty()
    {
        push_rect_rows(&was.layer, out);
        push_rect_rows(now, out);
        return;
    }
    let base = now.rect.row;
    // the selection is a whole-row style the spans themselves do not
    // carry, so a selection that moved between two identical rows still
    // repaints both of them
    let moved = if was.laid.selected == laid.selected {
        [None, None]
    } else {
        [was.laid.selected, laid.selected]
    };
    for row in moved.into_iter().flatten() {
        let row = base.saturating_add(row);
        if !out.contains(&row) {
            out.push(row);
        }
    }
    for i in 0..was.laid.lines.len().max(laid.lines.len()) {
        if was.laid.lines.get(i) == laid.lines.get(i) {
            continue;
        }
        let Ok(row) = u16::try_from(i) else {
            break;
        };
        let row = base.saturating_add(row);
        if !out.contains(&row) {
            out.push(row);
        }
    }
}

/// The terminal-space rows the agent panel covers, empty when this frame
/// paints no panel.
///
/// Only the bench taps ask: a frame whose whole damage is these rows is a
/// repaint the streamed turn explains, and one that reaches past them is
/// not (see [`Damage::covers_only`]). The answer itself is unconditional so
/// the shipped test run keeps proving it, rather than only the builds that
/// carry the taps.
#[must_use]
pub fn agent_panel_rows(surface: &Surface) -> Vec<u16> {
    let mut rows = Vec::new();
    for layer in &surface.layers {
        if !matches!(layer.kind, LayerKind::Ai(_)) {
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
    composite_layers(buf, model, surface, damage, None);
}

/// [`composite_into`], optionally spending overlay layouts a caller already
/// computed rather than laying the same layers out a second time.
///
/// `layouts` is only ever an optimisation: a layer it has no matching entry
/// for is laid out here, which is what keeps the equality guard's
/// from-scratch recomposite an independent answer.
fn composite_layers(
    buf: &mut Buffer,
    model: &Model,
    surface: &Surface,
    damage: &Damage,
    layouts: Option<&OverlayShadow>,
) {
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
    for (index, layer) in surface.layers.iter().enumerate() {
        let area = clip_to_frame(layer.rect, frame_area);
        if area.width == 0 || area.height == 0 {
            continue;
        }
        // a layer with no damaged row under it draws what the buffer
        // already holds there, so it is skipped before it is laid out --
        // the grid painter clips itself, row by row, and is the one layer
        // whose rows are named by something other than this test
        if !matches!(layer.kind, LayerKind::EngineGrid) && !damage.covers_any(area) {
            continue;
        }
        // Past this gate a layer covers at least one damaged row, never
        // necessarily all of them, so every painter that can write more
        // than one row clips row by row on its own -- the cmdline and the
        // tabline are one row tall, which the gate above already names
        // exactly. A painter repainting its whole rect writes rows this
        // frame is not repainting, and a clipped layer above it never
        // paints back over them: a toast border left sitting in the
        // sidebar's top edge until something else damages that row.
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
            LayerKind::Messages(entries) => paint_messages(entries, &theme, area, damage, buf),
            LayerKind::Tabline(state) => paint_tabline(state, &theme, area, buf),
            LayerKind::Popupmenu(state) => paint_popupmenu(state, &theme, area, damage, buf),
            LayerKind::Shell => paint_shell(&theme, area, damage, buf),
            LayerKind::Speculated(cells) => {
                let offset = model.chrome_rows();
                paint_speculated(cells, &theme, model.engine.hl(), offset, damage, buf);
            }
            LayerKind::Picker(_)
            | LayerKind::Tree(_)
            | LayerKind::Statusline(_)
            | LayerKind::Prompt(_)
            | LayerKind::Palette(_)
            | LayerKind::Ai(_) => {
                let laid = layouts.and_then(|shadow| shadow.laid_for(index, layer));
                paint_native_overlay(layer, laid, &theme, area, damage, buf);
            }
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
        col = col.saturating_add(paint_char_cell(buf, area, row_offset, col, ch, style));
    }
}

/// Places one character at column `col` of row `row_offset` within `area`,
/// styled `style`, and returns the number of columns it consumed.
///
/// The single per-cell placement primitive [`paint_text_row`] (one style for
/// a whole row) and [`paint_span_row`] (one style per span, continuing the
/// same column cursor across span boundaries) both build on, so wide-glyph
/// handling and edge-of-buffer clipping live in exactly one place rather
/// than as two copies that could drift.
fn paint_char_cell(
    buf: &mut Buffer,
    area: ratatui::layout::Rect,
    row_offset: u16,
    col: u16,
    ch: char,
    style: Style,
) -> u16 {
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
    // reset before styling, never merge: `ratatui::buffer::Cell::set_style`
    // patches (a `None` field leaves the cell's current value), so a chrome
    // cell painted over the grid keeps whatever background and modifiers the
    // layer beneath left in it wherever this style carries none -- which is
    // how a single cursorline cell survived mid-row inside an overlay in the
    // live repro. A chrome layer owns every cell it covers, opaquely.
    cell.reset();
    cell.set_symbol(symbol);
    cell.set_style(style);
    if width == 2 && col + 1 < area.width {
        buf[(area.x + col + 1, area.y + row_offset)].reset();
    }
    width
}

/// Writes `spans` into row `row_offset` of `area`, each span styled by
/// resolving its [`view_core::native::views::StyleRole`] through `resolve`,
/// continuing the same column cursor across span boundaries so spans
/// compose into one unbroken row exactly like [`paint_text_row`]'s single
/// string does.
///
/// `resolve` decides the whole story, including what a
/// [`view_core::native::views::StyleRole::Plain`] span gets (typically the
/// row's own base style, matching `Plain`'s documented meaning: "whatever
/// base style the row it sits on already carries") -- this function only
/// walks spans and places cells.
fn paint_span_row(
    spans: &[Span],
    resolve: impl Fn(StyleRole) -> Style,
    area: ratatui::layout::Rect,
    row_offset: u16,
    buf: &mut Buffer,
) {
    if row_offset >= area.height {
        return;
    }
    let mut col = 0_u16;
    for span in spans {
        if col >= area.width {
            break;
        }
        let style = resolve(span.role);
        for ch in span.text.chars() {
            if col >= area.width {
                break;
            }
            col = col.saturating_add(paint_char_cell(buf, area, row_offset, col, ch, style));
        }
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
///
/// Every write here is clipped to the rows `damage` names, border cells
/// included: see [`composite_layers`] for why a row of this rect the frame
/// is not repainting is not this painter's to touch.
fn paint_messages(
    lines: &[Vec<Span>],
    theme: &Theme,
    area: ratatui::layout::Rect,
    damage: &Damage,
    buf: &mut Buffer,
) {
    if lines.is_empty() {
        return;
    }

    let msg_area = theme.chrome(ChromeGroup::MsgArea);
    let style = ratatui_style(msg_area);
    let blank = " ".repeat(usize::from(area.width));
    for row in (0..area.height).filter(|&row| damage.covers_row_of(area, row)) {
        paint_text_row(&blank, style, area, row, buf);
    }

    let border_style = ratatui_style(ResolvedStyle {
        fg: Some(message_border_color(theme)),
        bg: msg_area.bg,
        ..ResolvedStyle::default()
    });
    paint_message_border(area, border_style, damage, buf);

    let inner = inset_by_one(area);
    // every toast line is a single `StyleRole::Plain` span (see
    // `LayerKind::Messages`'s doc comment), so this row's own `style` is
    // the whole story -- `paint_text_row` over the flattened text is the
    // honest rendering, not a placeholder for per-span resolution nobody
    // asked for here
    for (i, spans) in lines.iter().enumerate() {
        let Ok(row) = u16::try_from(i) else {
            break;
        };
        if !damage.covers_row_of(inner, row) {
            continue;
        }
        paint_text_row(
            &view_surface::overlay::line_text(spans),
            style,
            inner,
            row,
            buf,
        );
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
fn paint_message_border(
    area: ratatui::layout::Rect,
    style: Style,
    damage: &Damage,
    buf: &mut Buffer,
) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    let last_col = area.width - 1;
    let last_row = area.height - 1;
    let top_row = damage.covers_row_of(area, 0);
    let bottom_row = damage.covers_row_of(area, last_row);
    for col in 0..area.width {
        let (top, bottom) = match col {
            0 => ('┌', '└'),
            c if c == last_col => ('┐', '┘'),
            _ => ('─', '─'),
        };
        if top_row {
            set_border_cell(buf, area.x + col, area.y, top, style);
        }
        if bottom_row {
            set_border_cell(buf, area.x + col, area.y + last_row, bottom, style);
        }
    }
    for row in 1..last_row {
        if !damage.covers_row_of(area, row) {
            continue;
        }
        set_border_cell(buf, area.x, area.y + row, '│', style);
        set_border_cell(buf, area.x + last_col, area.y + row, '│', style);
    }
}

/// Writes one border glyph directly into `buf`, bypassing [`paint_text_row`]:
/// its column-advance-by-display-width logic exists for laying out a whole
/// string of arbitrary (possibly wide/control) characters across a row,
/// which a single fixed-width box-drawing character never needs.
///
/// Resets the cell first for the reason [`paint_char_cell`] does, plus one
/// of its own: a frame glyph restyled over an already-painted interior cell
/// must carry the frame's style alone, not the interior text's bold or
/// italic as well, and `set_style` merges modifiers.
fn set_border_cell(buf: &mut ratatui::buffer::Buffer, x: u16, y: u16, ch: char, style: Style) {
    let mut encode_buf = [0_u8; 4];
    let cell = &mut buf[(x, y)];
    cell.reset();
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
    border_color(theme.chrome(ChromeGroup::MsgArea))
}

/// A frame's foreground given the style of the surface it encloses: a
/// dimmed variant of that surface's own foreground, or the neutral grey
/// floor when it has none. See [`message_border_color`] for why the floor
/// is a fixed color rather than a dimmed background.
fn border_color(interior: ResolvedStyle) -> u32 {
    interior.fg.map_or(0x0080_8080, dim)
}

/// The style a selected row takes over an interior styled `base`:
/// `PmenuSel` -- the group a colorscheme already uses for "this row is the
/// one you are on" -- with its background made concrete.
///
/// Reverse video is resolved into colors here rather than sent as an SGR
/// attribute. A colorscheme that never defines `PmenuSel` leaves it on
/// `Theme::emphasis`, which is the reverse flag over the theme's own
/// colors; emitting that as `ESC[7m` gave the user a full-width inverted
/// bar whose color no colorscheme chose, and inverting an *unset*
/// foreground/background inverts whatever the terminal's ambient default
/// happens to be. Swapping the two resolved colors instead paints the same
/// intent in the theme's palette. With neither color known there is nothing
/// to swap, and the flag stays as the one selection signal any terminal can
/// still carry.
fn selection_style(theme: &Theme, base: ResolvedStyle) -> ResolvedStyle {
    let sel = theme.chrome(ChromeGroup::PmenuSel);
    let fg = sel.fg.or(base.fg);
    let bg = sel.bg.or(base.bg);
    if !sel.reverse {
        return ResolvedStyle { fg, bg, ..sel };
    }
    if fg.is_none() && bg.is_none() {
        return sel;
    }
    ResolvedStyle {
        fg: bg,
        bg: fg,
        reverse: false,
        ..sel
    }
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
    paint_text_row(
        &fill,
        ratatui_style(theme.chrome(ChromeGroup::TabLineFill)),
        area,
        0,
        buf,
    );

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
    paint_text_row(
        &text,
        ratatui_style(theme.chrome(ChromeGroup::TabLine)),
        area,
        0,
        buf,
    );
    if let Some((start, end)) = current_range {
        for col in start..end.min(area.width) {
            buf[(area.x + col, area.y)]
                .set_style(ratatui_style(theme.chrome(ChromeGroup::TabLineSel)));
        }
    }
}

/// Renders the popup menu: one item per row via [`PmItem::display_text`],
/// the `selected` index in the selection style. `render()` already anchored
/// and sized `area` to the event's `(row, col)` and the widest item.
///
/// Each row is blanked to the menu's full width before its item text: a
/// completion candidate shorter than the widest one leaves columns the menu
/// still covers, and an unpainted column shows the buffer straight through
/// the middle of the popup.
fn paint_popupmenu(
    state: &PopupmenuState,
    theme: &Theme,
    area: ratatui::layout::Rect,
    damage: &Damage,
    buf: &mut Buffer,
) {
    let base = theme.chrome(ChromeGroup::Pmenu);
    let blank = " ".repeat(usize::from(area.width));
    for (i, item) in state.items.iter().enumerate() {
        let Ok(row) = u16::try_from(i) else {
            break;
        };
        if row >= area.height {
            break;
        }
        if !damage.covers_row_of(area, row) {
            continue;
        }
        let is_selected = i64::try_from(i).is_ok_and(|idx| idx == state.selected);
        let style = if is_selected {
            ratatui_style(selection_style(theme, base))
        } else {
            ratatui_style(base)
        };
        paint_text_row(&blank, style, area, row, buf);
        paint_text_row(&item.display_text(), style, area, row, buf);
    }
}

/// Renders one native overlay: a picker, a file tree, a statusline, a
/// prompt, or a command palette.
///
/// Layout is not repeated here. `view_surface::overlay::rows` already cut
/// this layer's rect into exactly the strings that cover it -- frame,
/// padding, title, scroll window, selection marker -- and those same
/// strings are what the oracle's rasterizer blits into a golden screen
/// dump. One layout pass serves both, so a golden depicts what a terminal
/// actually receives instead of a parallel reimplementation of it. This
/// function adds only the part that needs a terminal to decide: style.
///
/// Color is not gated on the probed `truecolor` bit, and never was on the
/// capability tier. The tier's whole contribution was choosing the border
/// charset, back at render time. The gate that used to stand here sent an
/// overlay no color at all when `COLORTERM` was unset -- routine over SSH,
/// which forwards `TERM` and not `COLORTERM` -- while [`paint_grid`] went on
/// resolving every buffer cell to a 24-bit color regardless. The result was
/// a default-background box sitting on a themed buffer, on the very
/// terminals that demonstrably render the buffer's colors. One rule for
/// both layers is the coherent one, and it is the grid's.
///
/// The overlay's colors come from the floating-window groups the user's
/// colorscheme already defines, so a native overlay reads as part of their
/// theme rather than as a second, unrelated palette. The statusline is the
/// one exception: it derives its interior and frame from
/// `ChromeGroup::StatusLine` instead, because a status line is a distinct
/// piece of chrome a colorscheme styles on its own, not a float.
///
/// The title set into the top edge is the one piece of frame chrome with a
/// style of its own (`ChromeGroup::FloatTitle`, bold): it is the label
/// naming what the overlay is, and the frame's color is deliberately dimmed
/// away from readable.
fn paint_native_overlay(
    layer: &Layer,
    laid: Option<&view_surface::overlay::Rows>,
    theme: &Theme,
    area: ratatui::layout::Rect,
    damage: &Damage,
    buf: &mut Buffer,
) {
    if layer.borders.is_none() {
        return;
    }
    let computed;
    let laid = match laid {
        Some(laid) => laid,
        None => {
            computed = lay_out(layer);
            &computed
        }
    };
    // every overlay reads its colors from the floating-window group except
    // the statusline, which is its own chrome group (a status line is not a
    // float and a colorscheme that restyles one must not restyle the other)
    let group = if matches!(layer.kind, LayerKind::Statusline(_)) {
        ChromeGroup::StatusLine
    } else {
        ChromeGroup::NormalFloat
    };
    let base = theme.chrome(group);
    let interior = ratatui_style(base);
    // the one background an overlay must never paint (see the per-span
    // resolve below), read once for the whole layer rather than per row
    let buffer_bg = theme.normal().bg;
    let selected = ratatui_style(selection_style(theme, base));
    let frame = ratatui_style(ResolvedStyle {
        fg: Some(border_color(base)),
        bg: base.bg,
        ..ResolvedStyle::default()
    });
    // the title takes its group's whole resolved style -- italic, underline
    // and reverse included, the same way a content row's roles do at the
    // per-span resolve below -- with two deliberate overrides. The bg stays
    // the overlay's, so the top edge reads as one continuous run rather
    // than a differently-lit patch mid-border. Bold is OR-ed in rather than
    // read, because it is what still separates the title from the frame
    // under a colorscheme that gives the two the same foreground.
    // The reverse flag is dropped for the same reason the bg is pinned: a
    // colorscheme that leaves `FloatTitle` on the emphasis fallback carries
    // it, and an inverted patch of border mid-title is the differently-lit
    // run this pins the background to avoid. Bold already carries the
    // distinction.
    let title = {
        let group = theme.chrome(ChromeGroup::FloatTitle);
        ratatui_style(ResolvedStyle {
            fg: group.fg.or(base.fg),
            bg: base.bg,
            bold: true,
            reverse: false,
            ..group
        })
    };

    let last = laid.lines.len().saturating_sub(1);
    for (i, line) in laid.lines.iter().enumerate() {
        let Ok(row) = u16::try_from(i) else {
            break;
        };
        if row >= area.height {
            break;
        }
        // the row-level half of the clip the loop above applies to the
        // whole layer: a framed overlay is one layer but many independent
        // rows, and a keystroke in a panel's composer changes exactly one
        // of them
        if !damage.covers_row_of(area, row) {
            continue;
        }
        let edge_row = laid.framed && (i == 0 || i == last);
        if edge_row {
            // per span, not per row: the top edge carries the overlay's
            // title in its own role, and blitting the row in one style is
            // what made the title inherit the border's dimmed color
            paint_span_row(
                line,
                |role| match role {
                    StyleRole::Title => title,
                    _ => frame,
                },
                area,
                row,
                buf,
            );
        } else if laid.selected == Some(row) {
            // a selected row's whole-row reverse/highlight is a fact about
            // the row, not about any one span in it, so it stays a single
            // uniform style even though the row's spans may carry roles of
            // their own
            paint_text_row(
                &view_surface::overlay::line_text(line),
                selected,
                area,
                row,
                buf,
            );
        } else {
            // ordinary content rows resolve style per span -- this is what
            // lets the statusline's diagnostic glyphs, mode text, git
            // branch, etc. read in distinct colors instead of collapsing to
            // one flat style; every other overlay's rows carry only
            // `StyleRole::Plain` spans, so `resolve` falling back to
            // `interior` for those keeps their appearance unchanged. A
            // role's own background is honoured, but a role that names none
            // (most of them: a colorscheme colors a diagnostic glyph, not
            // the box behind it) keeps the overlay's, so a styled span never
            // punches a hole in the surface it sits on.
            //
            // "Names none" is read by comparison rather than off an
            // `Option`, because by here there is no `Option` left to read:
            // `Theme::style_for` resolves a chrome group the way nvim
            // resolves a grid cell, filling an unset background from the
            // buffer's own -- correct on the grid, and a hole through every
            // float. The buffer's background is exactly the value an
            // overlay must never paint, so a colorscheme that named it
            // deliberately is asking for the same hole and is answered the
            // same way.
            let resolve = |role: StyleRole| -> Style {
                role.chrome_group().map_or(interior, |group| {
                    let style = theme.chrome(group);
                    ratatui_style(ResolvedStyle {
                        bg: style.bg.filter(|bg| Some(*bg) != buffer_bg).or(base.bg),
                        ..style
                    })
                })
            };
            paint_span_row(line, resolve, area, row, buf);
        }
        if laid.framed && !edge_row {
            paint_frame_cells(
                &view_surface::overlay::line_text(line),
                layer.rect.width,
                area,
                row,
                frame,
                buf,
            );
        }
    }
}

/// Restyles the two vertical frame glyphs of one already-painted interior
/// row, which [`paint_native_overlay`] blitted in the interior's own style
/// along with the content between them.
///
/// The glyphs are read back out of `line` rather than out of the border
/// charset: the row that was painted is the only authority on what sits in
/// its first and last cell, and re-deriving them here would put a second
/// opinion about the frame in a module that deliberately holds none.
/// `width` is the layer's own rect width, not `area`'s: when the terminal
/// clipped the rect, the right-hand glyph was never painted and there is
/// nothing at that column to restyle.
fn paint_frame_cells(
    line: &str,
    width: u16,
    area: ratatui::layout::Rect,
    row: u16,
    style: Style,
    buf: &mut Buffer,
) {
    let mut chars = line.chars();
    if let Some(left) = chars.next() {
        set_border_cell(buf, area.x, area.y + row, left, style);
    }
    let right = width.saturating_sub(1);
    if right < area.width {
        if let Some(glyph) = line.chars().next_back() {
            set_border_cell(buf, area.x + right, area.y + row, glyph, style);
        }
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
fn paint_shell(theme: &Theme, area: ratatui::layout::Rect, damage: &Damage, buf: &mut Buffer) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let bottom_row = area.height - 1;
    if damage.covers_row_of(area, bottom_row) {
        let fill = " ".repeat(usize::from(area.width));
        let style = ratatui_style(theme.chrome(ChromeGroup::StatusLine));
        paint_text_row(&fill, style, area, bottom_row, buf);
    }

    let mid_row = area.height / 2;
    if damage.covers_row_of(area, mid_row) {
        let text: String = view_surface::SHELL_PLACEHOLDER
            .chars()
            .take(usize::from(area.width))
            .collect();
        paint_text_row(&text, ratatui_style(theme.normal()), area, mid_row, buf);
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
        // skipping an unchanged row leaves its cells as the previous frame
        // painted them
        if !damage.covers_row_of(area, row) {
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

/// nvim's own id for the default highlight -- the style a grid cell carries
/// when no `hl_attr_define` applies to it.
const DEFAULT_HL_ID: u64 = 0;

/// Paints the display-only predicted glyphs over the cells they were
/// predicted for.
///
/// Their coordinates are the engine grid's own, not the layer rect's (see
/// `LayerKind::Speculated`), so they are offset past the reserved chrome rows
/// exactly as the grid layer's placement already is -- one coordinate space
/// for both grid-content layers, and one fewer place the two could disagree
/// about where a cell is.
///
/// Styled as a default grid cell, deliberately, with no marker of any kind: a
/// prediction that announced itself would advertise the latency this exists
/// to hide, and restyling each cell again the moment the authoritative redraw
/// confirms it would flicker at typing cadence.
///
/// A cell outside the buffer is skipped rather than clamped, per
/// `PredictedCell`'s own contract: a clamped prediction paints a glyph the
/// user did not type at the last real column.
fn paint_speculated(
    cells: &[PredictedCell],
    theme: &Theme,
    hl: &HlTable,
    offset: u16,
    damage: &Damage,
    buf: &mut Buffer,
) {
    let style = style_for(theme, DEFAULT_HL_ID, hl);
    for cell in cells {
        let row = cell.row.saturating_add(offset);
        if !damage.covers(row) {
            continue;
        }
        let Some(out) = buf.cell_mut((cell.col, row)) else {
            continue;
        };
        // every predicted glyph is one ASCII column wide (see `predict`), so
        // it needs neither the grid's width fitting nor its control-character
        // sanitization
        let mut encoded = [0u8; 4];
        out.set_symbol(cell.glyph.encode_utf8(&mut encoded));
        out.set_style(style);
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
    use view_core::native::ai_event::{AiEvent, ToolCallStatus};

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

    /// A tabline with two entries, which is what reserves the one chrome row
    /// a speculated cell's grid coordinates have to be offset past.
    fn two_tabs() -> view_core::events::UiEvent {
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
        }
    }

    /// The whole point of the feature at the painter: a prediction the model
    /// is holding reaches the terminal cell it names, offset past the chrome
    /// the grid itself is offset past. `PredictedCell` coordinates are the
    /// engine grid's own, not the layer rect's, so an arm that treated them
    /// as rect-relative would paint this glyph a row and five columns from
    /// where it belongs.
    #[test]
    fn a_pending_prediction_paints_its_glyph_at_the_engine_cell_it_names() {
        let mut model = Model::new();
        apply(&mut model, two_tabs());
        model.engine.apply_grid(GridOp::Resize {
            width: 10,
            height: 3,
        });
        assert_eq!(model.chrome_rows(), 1, "the fixture must reserve a row");
        let stamp = view_core::native::speculate::SpecStamp::new(std::time::Duration::ZERO);
        assert!(model
            .speculate
            .predict("insert", 'z', (2, 5), stamp)
            .is_some());
        let surface = view_surface::render(&model);

        let backend = TestBackend::new(10, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();

        let buf = terminal.backend().buffer().clone();
        assert_eq!(
            &buf[(5, 3)].symbol(),
            &"z",
            "grid row 2 sits on terminal row 3 behind one chrome row"
        );
    }

    /// The other half: once the authoritative redraw has answered the
    /// prediction, the model stops holding it and the painter has nothing
    /// left to put over the engine's own cell.
    #[test]
    fn a_reconciled_prediction_leaves_the_authoritative_cell_showing() {
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize {
            width: 10,
            height: 3,
        });
        let stamp = view_core::native::speculate::SpecStamp::new(std::time::Duration::ZERO);
        assert!(model
            .speculate
            .predict("insert", 'z', (1, 4), stamp)
            .is_some());
        let answer = view_core::events::UiEvent::GridLine {
            grid: 1,
            row: 1,
            col_start: 4,
            cells: vec![view_core::events::GridCell {
                text: "q".to_string(),
                hl_id: 0,
                repeat: 1,
            }],
        };
        model.speculate.reconcile(std::slice::from_ref(&answer));
        apply(&mut model, answer);
        assert!(model.speculate.pending().is_empty());
        let surface = view_surface::render(&model);

        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();

        let buf = terminal.backend().buffer().clone();
        assert_eq!(
            &buf[(4, 1)].symbol(),
            &"q",
            "the engine's own glyph is what remains once the guess is retired"
        );
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

    /// Drives the agent panel's `:View ai <verb>` entry point.
    fn ai_verb(model: &mut Model, verb: &str) {
        let _ = view_core::update::update(
            model,
            view_core::msg::Msg::FeatureInvoke {
                feature: "ai".to_string(),
                verb: verb.to_string(),
            },
        );
    }

    /// Opens the tree sidebar already holding a scan, so its selection is a
    /// real row that a later keystroke can move.
    fn open_tree_with_entries(model: &mut Model) {
        let _ = view_core::update::update(
            model,
            view_core::msg::Msg::FeatureInvoke {
                feature: "tree".to_string(),
                verb: "toggle".to_string(),
            },
        );
        let Some(tree) = model.tree_mut() else {
            panic!("the tree sidebar is open");
        };
        let generation = tree.generation();
        tree.apply_scan(
            generation,
            ["src", "src/main.rs", "Cargo.toml"]
                .into_iter()
                .enumerate()
                .map(|(i, path)| {
                    view_core::native::tree::TreeEntry::new(
                        std::path::PathBuf::from(path),
                        i == 0,
                        u16::from(i == 1),
                    )
                })
                .collect(),
        );
    }

    /// The tree sidebar's selected index, or `None` when no tree is open.
    fn selected_row(model: &Model) -> Option<usize> {
        view_surface::render(model)
            .layers
            .iter()
            .find_map(|layer| match &layer.kind {
                LayerKind::Tree(view) => Some(view.selected),
                _ => None,
            })
            .flatten()
    }

    /// How many rows the agent panel's composer currently paints, from the
    /// rendered surface rather than from the panel's width arithmetic, so a
    /// test drives the boundary the painter actually sees.
    fn composer_row_count(model: &Model) -> usize {
        view_surface::render(model)
            .layers
            .iter()
            .find_map(|layer| match &layer.kind {
                LayerKind::Ai(view) => Some(view.input.len()),
                _ => None,
            })
            .unwrap_or(0)
    }

    /// The agent panel layer's own width in cells, or 0 when no panel is
    /// open.
    fn agent_panel_width(model: &Model) -> u16 {
        view_surface::render(model)
            .layers
            .iter()
            .find(|layer| matches!(layer.kind, LayerKind::Ai(_)))
            .map_or(0, |layer| layer.rect.width)
    }

    /// Types into the composer until its first row is exactly full: one
    /// character short of wrapping, so the keystroke after this one moves
    /// the transcript boundary and nothing else does.
    fn fill_composer_row(model: &mut Model) {
        for _ in 0..256 {
            if composer_row_count(model) > 1 {
                type_key(model, "<BS>");
                return;
            }
            type_key(model, "e");
        }
        panic!("the composer never wrapped");
    }

    /// Folds one agent event through `update()`, the way the runtime's own
    /// `Msg::Ai` dispatch does.
    fn ai_event(model: &mut Model, event: AiEvent) {
        let _ = view_core::update::update(model, view_core::msg::Msg::Ai(event));
    }

    /// One spinner frame, both halves of what `runtime::expire_ai_spinner`
    /// does when the panel's own deadline comes due: `view-tui` has no loop
    /// to run and no clock to run it on, so the frame is moved here and the
    /// repaint it owes is asked for here.
    fn spinner_tick(model: &mut Model) {
        model.ai_panel_mut().transcript.advance_spinner();
        model.dirty = true;
    }

    /// Sends one key through `update()`, which routes it to whatever holds
    /// focus -- the agent panel's composer, when the panel is open.
    fn type_key(model: &mut Model, notation: &str) {
        let _ = view_core::update::update(
            model,
            view_core::msg::Msg::Key(view_core::msg::Key {
                notation: notation.to_string(),
            }),
        );
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

    /// The agent panel is full height, so answering a composer keystroke by
    /// dirtying every row it covers costs a whole-screen recomposite -- the
    /// grid cells beside the panel included -- for one changed cell, and
    /// that cost grows with the terminal rather than with what was typed.
    /// Over a link with any latency it is what makes typing into the panel
    /// feel slower than typing into the buffer, which is backwards: the
    /// composer is native state and the buffer round-trips nvim.
    #[test]
    fn a_composer_keystroke_damages_only_the_composer_row() {
        let mut model = Model::with_term_size(120, 40);
        model.engine.apply_grid(GridOp::Resize {
            width: 120,
            height: 40,
        });
        model.ai_trusted = true;
        ai_verb(&mut model, "open");
        assert!(model.ai_panel().focused, "the composer holds focus");

        let area = ratatui::layout::Rect::new(0, 0, 120, 40);
        let mut shadow = Shadow::new();
        assert!(shadow.resize(area));
        let surface = view_surface::render(&model);
        let opened = shadow.overlay_damage(&surface);
        let panel = agent_panel_rows(&surface);
        assert_eq!(
            panel.len(),
            usize::from(area.height),
            "the panel spans the terminal's height: {panel:?}"
        );
        assert_eq!(
            opened.len(),
            panel.len(),
            "the panel's own first frame draws every row it covers"
        );
        let _ = model.take_paint_damage();
        shadow.compose(&model, &surface, &Damage::full());
        shadow.commit();

        type_key(&mut model, "x");
        let surface = view_surface::render(&model);
        let typed = shadow.overlay_damage(&surface);
        assert_eq!(
            typed.len(),
            1,
            "one composer row changed, but these rows were damaged: {typed:?}"
        );
        assert!(
            panel.contains(&typed[0]),
            "the damaged row is inside the panel"
        );
        let grid_damage = model.take_paint_damage();
        assert!(
            grid_damage.rows.is_empty(),
            "a composer keystroke never reaches the engine grid"
        );
        // the damage shape above is only worth pinning if the frame it
        // produces is the whole truth: composing it runs the debug guard
        // against a from-scratch recomposite, and the front buffer is
        // checked against one here so release builds prove it too
        let damage = Damage::from_frame(&grid_damage, model.chrome_rows(), &typed, false);
        shadow.compose(&model, &surface, &damage);
        shadow.commit();
        assert_eq!(
            shadow.front(),
            &full_paint(&model, area),
            "the one damaged row carried every cell the keystroke changed"
        );
    }

    /// The terminal-space rows the toast box covers in `surface`.
    fn message_box_rows(surface: &Surface) -> Vec<u16> {
        let mut rows = Vec::new();
        for layer in &surface.layers {
            if !matches!(layer.kind, LayerKind::Messages(_)) {
                continue;
            }
            let first = layer.rect.row;
            rows.extend(first..first.saturating_add(layer.rect.height));
        }
        rows
    }

    /// Dismissing a toast produces no engine redraw at all -- nvim never
    /// hears about it -- so the rows the box was occupying are the only
    /// thing that can restore the buffer text underneath. A frame that
    /// damaged nothing would leave the error painted on a screen whose model
    /// no longer holds it, which reads as a dismissal key that did nothing.
    #[test]
    fn dismissing_a_sticky_toast_damages_the_rows_it_was_covering() {
        let mut model = Model::with_term_size(80, 24);
        model.engine.apply_grid(GridOp::Resize {
            width: 80,
            height: 24,
        });
        apply(
            &mut model,
            view_core::events::UiEvent::ModeChange {
                mode: "normal".to_string(),
                mode_idx: 0,
            },
        );
        apply(
            &mut model,
            view_core::events::UiEvent::MsgShow {
                kind: "emsg".to_string(),
                content: vec![(0, "E492: Not an editor command: bogus".to_string())],
                replace_last: false,
            },
        );
        apply(&mut model, view_core::events::UiEvent::Flush);

        let area = ratatui::layout::Rect::new(0, 0, 80, 24);
        let mut shadow = Shadow::new();
        assert!(shadow.resize(area));
        let surface = view_surface::render(&model);
        let mut toast = message_box_rows(&surface);
        assert!(!toast.is_empty(), "the error toast is on screen");
        toast.sort_unstable();
        let _ = shadow.overlay_damage(&surface);
        let _ = model.take_paint_damage();
        shadow.compose(&model, &surface, &Damage::full());
        shadow.commit();

        type_key(&mut model, "<Esc>");
        let surface = view_surface::render(&model);
        assert!(
            message_box_rows(&surface).is_empty(),
            "the dismissed toast leaves the surface"
        );
        let mut dismissed = shadow.overlay_damage(&surface);
        dismissed.sort_unstable();

        // exact, not containment: over-damage repaints correctly and so
        // survives the full-recomposite check below, while costing rows
        // the row-granular damage model exists to save
        assert_eq!(
            dismissed, toast,
            "the toast's own rows are the whole cost of taking it down"
        );
        let grid_damage = model.take_paint_damage();
        let damage = Damage::from_frame(&grid_damage, model.chrome_rows(), &dismissed, false);
        shadow.compose(&model, &surface, &damage);
        shadow.commit();
        assert_eq!(
            shadow.front(),
            &full_paint(&model, area),
            "the damaged rows carried every cell the dismissal changed"
        );
    }

    /// The spinner is the one thing on this panel that repaints without
    /// anybody touching a key, eight times a second for as long as a tool
    /// call runs. A frame of it costs the marker's row and nothing else --
    /// damage is row-granular here (see [`Damage`]), so the row is the
    /// floor, and a spinner that dirtied the panel would spend the whole
    /// sidebar on one animated glyph on a timer.
    #[test]
    fn a_spinner_frame_damages_only_the_row_its_marker_sits_on() {
        let mut model = Model::with_term_size(120, 40);
        model.engine.apply_grid(GridOp::Resize {
            width: 120,
            height: 40,
        });
        model.ai_trusted = true;
        ai_verb(&mut model, "open");
        for i in 0..5 {
            ai_event(
                &mut model,
                AiEvent::MessageChunk {
                    message_id: Some(format!("m{i}")),
                    text: "the agent talking".to_string(),
                    from_agent: true,
                },
            );
        }
        ai_event(
            &mut model,
            AiEvent::ToolCallUpdate {
                tool_call_id: "call_1".to_string(),
                title: "Read file".to_string(),
                status: ToolCallStatus::InProgress,
                content: None,
            },
        );

        let area = ratatui::layout::Rect::new(0, 0, 120, 40);
        let mut shadow = Shadow::new();
        assert!(shadow.resize(area));
        let surface = view_surface::render(&model);
        let _ = shadow.overlay_damage(&surface);
        let _ = model.take_paint_damage();
        shadow.compose(&model, &surface, &Damage::full());
        shadow.commit();

        spinner_tick(&mut model);
        let surface = view_surface::render(&model);
        let ticked = shadow.overlay_damage(&surface);
        assert_eq!(
            ticked.len(),
            1,
            "one marker moved, but these rows were damaged: {ticked:?}"
        );
        assert!(
            agent_panel_rows(&surface).contains(&ticked[0]),
            "the damaged row is inside the panel"
        );
        let grid_damage = model.take_paint_damage();
        assert!(
            grid_damage.rows.is_empty(),
            "a spinner frame never reaches the engine grid"
        );
        let damage = Damage::from_frame(&grid_damage, model.chrome_rows(), &ticked, false);
        shadow.compose(&model, &surface, &damage);
        shadow.commit();
        assert_eq!(
            shadow.front(),
            &full_paint(&model, area),
            "the one damaged row carried every cell the frame changed"
        );
    }

    /// What the agent-paint tap is allowed to explain. The panel repainting
    /// itself under a streamed turn is the frame a bench row attributes to
    /// the agent; the same panel on screen while a toast expires is a frame
    /// something else drove, and reading it as the agent's would explain
    /// away a paint the row exists to count.
    #[test]
    fn only_a_frame_whose_whole_damage_is_the_panel_reads_as_the_agents() {
        let mut model = Model::with_term_size(40, 12);
        model.engine.apply_grid(GridOp::Resize {
            width: 40,
            height: 12,
        });
        model.push_overlay(
            view_core::native::geometry::OverlayBox::new(40, 60),
            view_core::model::OverlayKind::Ai,
        );
        apply(
            &mut model,
            view_core::events::UiEvent::MsgShow {
                kind: "echomsg".into(),
                content: vec![(0, "a toast".into())],
                replace_last: false,
            },
        );
        let surface = view_surface::render(&model);
        let panel = agent_panel_rows(&surface);
        assert!(!panel.is_empty(), "the panel is on screen");
        // The rows are the panel's own and nobody else's: the toast is a
        // layer too, and counting its rows as the panel's would let a
        // frame the toast drove read as a frame the agent drove.
        let ai = surface
            .layers
            .iter()
            .find(|layer| matches!(layer.kind, LayerKind::Ai(_)))
            .expect("the panel is one of the layers");
        assert_eq!(
            panel.len(),
            usize::from(ai.rect.height),
            "one row per row of the panel layer, and no layer beside it"
        );

        let quiet = GridDamage::default();
        assert!(
            Damage::from_frame(&quiet, 0, &panel, false).covers_only(&panel),
            "a frame repainting the panel and nothing else"
        );
        assert!(
            Damage::from_frame(&quiet, 0, &panel[..1], false).covers_only(&panel),
            "one changed panel row is still the panel and nothing else"
        );

        let mut beside = panel.clone();
        beside.push(panel.iter().max().copied().unwrap_or(0).saturating_add(1));
        assert!(
            !Damage::from_frame(&quiet, 0, &beside, false).covers_only(&panel),
            "the toast's rows repaint beside the panel's"
        );
        assert!(
            !Damage::full().covers_only(&panel),
            "a whole-frame repaint is not the panel's doing"
        );
        assert!(
            !Damage::from_frame(&quiet, 0, &[], false).covers_only(&panel),
            "a frame with no damage at all explains nothing"
        );
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

        let mut bg_only = Theme::default();
        bg_only.set_chrome(
            ChromeGroup::MsgArea,
            ResolvedStyle {
                bg: Some(0x0000_0000),
                ..ResolvedStyle::default()
            },
        );
        assert_eq!(
            message_border_color(&bg_only),
            0x0080_8080,
            "a background-only theme must not derive the border from a dimmed bg -- \
             dimming black yields black, an invisible frame on its own background"
        );
    }

    #[test]
    fn message_border_color_dims_a_set_msg_area_foreground() {
        let mut theme = Theme::default();
        theme.set_chrome(
            ChromeGroup::MsgArea,
            ResolvedStyle {
                fg: Some(0x00FF_0000),
                ..ResolvedStyle::default()
            },
        );
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
                paint_messages(&[], &theme, area, &Damage::full(), buf);
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
        let mut overlay = OverlayShadow::default();
        let _ = overlay.advance(&surf_a);
        let offset_a = model.chrome_rows();
        // clear the damage state A's construction accumulated: the shadow is
        // about to hold A in full, so only B's later damage matters
        let _ = model.take_paint_damage();
        let mut shadow = ratatui::buffer::Buffer::empty(area_a);
        composite_into(&mut shadow, &model, &surf_a, &Damage::full());

        mutate_b(&mut model);
        let grid_damage = model.take_paint_damage();
        let surf_b = view_surface::render(&model);
        let overlay_damage = overlay.advance(&surf_b);
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
            Damage::from_frame(&grid_damage, offset_b, &overlay_damage, false)
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
            // the agent panel is the layer whose damage is row-wise rather
            // than whole-rect, so every one of its transitions has to land
            // in the sequence this checks: appearing, one row of it
            // changing, the grid changing under it while it holds still,
            // and vanishing
            (
                "agent panel opens",
                Box::new(|m: &mut Model| {
                    m.ai_trusted = true;
                    ai_verb(m, "open");
                }),
            ),
            (
                "composer keystroke",
                Box::new(|m: &mut Model| type_key(m, "q")),
            ),
            (
                "second composer keystroke",
                Box::new(|m: &mut Model| type_key(m, "w")),
            ),
            // a composer that wraps moves the boundary between itself and
            // the transcript, so the rows that change are not the row that
            // was typed into -- the one shape a per-row damage set gets
            // wrong by under-damaging, and a stale row there looks exactly
            // like an untouched one on screen
            ("composer fills its first row", Box::new(fill_composer_row)),
            (
                "one more character wraps the composer",
                Box::new(|m: &mut Model| type_key(m, "e")),
            ),
            // a resize re-wraps the composer and vacates the columns the
            // panel gave up in the same frame; `push_changed_rows` takes
            // the whole-rect fallback on any rect change, and this is what
            // proves that fallback covers both
            (
                "panel narrows with a wrapped composer",
                Box::new(|m: &mut Model| {
                    let before = agent_panel_width(m);
                    type_key(m, "<S-Left>");
                    assert!(
                        agent_panel_width(m) < before,
                        "the resize key moved the panel's own rect"
                    );
                }),
            ),
            (
                "panel widens again",
                Box::new(|m: &mut Model| type_key(m, "<S-Right>")),
            ),
            (
                "one backspace unwraps it",
                Box::new(|m: &mut Model| type_key(m, "<BS>")),
            ),
            (
                "composer clears",
                Box::new(|m: &mut Model| {
                    while !m.ai_panel().input.is_empty() {
                        type_key(m, "<BS>");
                    }
                }),
            ),
            // submitting empties the composer and appends to the transcript
            // in one frame, so the rows that change are at both ends of the
            // panel at once -- and the transcript's own newest row is one
            // the panel has never painted before
            (
                "a submitted prompt echoes into the transcript",
                Box::new(|m: &mut Model| {
                    for ch in ["a", "s", "k"] {
                        type_key(m, ch);
                    }
                    let before = m.ai_panel().transcript.len();
                    type_key(m, "<CR>");
                    assert!(
                        m.ai_panel().input.is_empty(),
                        "the submit emptied the composer"
                    );
                    assert_eq!(
                        m.ai_panel().transcript.len(),
                        before + 1,
                        "and put the prompt on screen"
                    );
                }),
            ),
            (
                "a tool call starts running",
                Box::new(|m: &mut Model| {
                    ai_event(
                        m,
                        AiEvent::ToolCallUpdate {
                            tool_call_id: "call_1".to_string(),
                            title: "Read file".to_string(),
                            status: ToolCallStatus::InProgress,
                            content: None,
                        },
                    );
                }),
            ),
            // the one frame in this sequence nobody asked for: a timer, not
            // a keystroke, and it must still compose to the same bytes a
            // full recomposite would
            (
                "spinner tick",
                Box::new(|m: &mut Model| {
                    spinner_tick(m);
                }),
            ),
            (
                "the tool call resolves",
                Box::new(|m: &mut Model| {
                    ai_event(
                        m,
                        AiEvent::ToolCallUpdate {
                            tool_call_id: "call_1".to_string(),
                            title: "Read file".to_string(),
                            status: ToolCallStatus::Completed,
                            content: None,
                        },
                    );
                }),
            ),
            (
                "grid edit beside the open panel",
                Box::new(|m: &mut Model| {
                    m.engine.apply_grid(GridOp::PutLine {
                        row: 4,
                        col_start: 1,
                        cells: vec![("Y".into(), 2, 1)],
                    });
                }),
            ),
            (
                "agent panel closes",
                Box::new(|m: &mut Model| {
                    ai_verb(m, "close");
                }),
            ),
            // the selection is a whole-row style the spans do not carry, so
            // it is the one row-wise input the line comparison cannot see
            ("tree sidebar opens", Box::new(open_tree_with_entries)),
            (
                "tree selection moves",
                Box::new(|m: &mut Model| {
                    let before = selected_row(m);
                    type_key(m, "<Down>");
                    assert_ne!(before, selected_row(m), "the selection moved");
                }),
            ),
        ];

        let mut first = true;
        let mut row_wise_frames = 0_u32;
        let mut boundary_frames = 0_u32;
        for (label, mutate) in steps {
            mutate(&mut model);
            let grid_damage = model.take_paint_damage();
            let surface = view_surface::render(&model);
            // a framed overlay resolved to a 0x0 rect lays out no rows, takes
            // the whole-rect fallback on every comparison and paints nothing,
            // which leaves this moat passing while proving nothing about the
            // layer it names
            for layer in &surface.layers {
                if layer.borders.is_some() {
                    assert!(
                        !lay_out(layer).lines.is_empty(),
                        "a framed layer with no laid rows after {label}: {:?}",
                        layer.rect
                    );
                }
            }
            let overlay_damage = shadow.overlay_damage(&surface);
            if matches!(label, "composer keystroke" | "tree selection moves") {
                assert!(
                    !overlay_damage.is_empty() && overlay_damage.len() < 4,
                    "{label} took the row-wise branch, not the whole-rect \
                     fallback: {overlay_damage:?}"
                );
                row_wise_frames += 1;
            }
            if matches!(
                label,
                "one more character wraps the composer" | "one backspace unwraps it"
            ) {
                let panel = agent_panel_rows(&surface).len();
                assert!(
                    overlay_damage.len() > 1,
                    "{label} moved the transcript boundary, so more than the \
                     typed row changed: {overlay_damage:?}"
                );
                assert!(
                    overlay_damage.len() < panel,
                    "{label} still damaged rows rather than the panel's whole \
                     rect: {overlay_damage:?} of {panel}"
                );
                boundary_frames += 1;
            }
            let damage =
                Damage::from_frame(&grid_damage, model.chrome_rows(), &overlay_damage, first);
            first = false;
            shadow.compose(&model, &surface, &damage);
            shadow.commit();
            assert_eq!(
                shadow.front(),
                &full_paint(&model, area),
                "shadow diverged from a full recomposite after: {label}"
            );
        }
        assert_eq!(row_wise_frames, 2, "both row-wise frames were composited");
        assert_eq!(
            boundary_frames, 2,
            "the composer grew and shrank by a row, and both frames were checked"
        );
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

    /// The compose-time equivalence guard must be seen to catch: a damage
    /// set that misses a changed row is exactly the silent-drift failure
    /// the guard exists for, and a guard that has never fired proves
    /// nothing. Drives the shadow through a correct frame, then a frame
    /// whose damage deliberately omits the row an edit changed, and
    /// expects the debug assert naming the frame and cell. The third
    /// compose is the one that can under-repaint: the shadow re-repaints
    /// the previous frame's rows on top of the given damage, so the empty
    /// set only becomes a miss once the carried set is empty too.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "diverged from a full recomposite")]
    fn an_under_reported_damage_trips_the_composite_equivalence_guard() {
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize {
            width: 20,
            height: 6,
        });
        let mut shadow = Shadow::new();
        let _ = shadow.resize(ratatui::layout::Rect::new(0, 0, 20, 6));
        put(&mut model, 2, 0, &["A"]);
        let surface = view_surface::render(&model);
        shadow.compose(&model, &surface, &Damage::full());
        shadow.commit();
        shadow.compose(
            &model,
            &surface,
            &Damage {
                full: false,
                rows: Vec::new(),
            },
        );
        shadow.commit();
        put(&mut model, 3, 0, &["B"]);
        let surface = view_surface::render(&model);
        shadow.compose(
            &model,
            &surface,
            &Damage {
                full: false,
                rows: Vec::new(),
            },
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
        set_term_size(model, width, height);
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

        let mut first = true;
        for (label, mutate) in steps {
            mutate(&mut model);
            let grid_damage = model.take_paint_damage();
            let surface = view_surface::render(&model);
            let overlay_damage = shadow.overlay_damage(&surface);
            let damage =
                Damage::from_frame(&grid_damage, model.chrome_rows(), &overlay_damage, first);
            first = false;
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

    /// Points the model's terminal at the same size as the grid being
    /// seeded.
    ///
    /// A model left at the default 0x0 resolves every native overlay's
    /// geometry to a 0x0 rect, which lays out no rows and paints no cells:
    /// a moat built on one still passes, but it compares two blank overlays
    /// and proves nothing about the layer it named.
    fn set_term_size(model: &mut Model, width: u16, height: u16) {
        model.term_width = width;
        model.term_height = height;
    }

    /// Seeds every row of `model`'s grid with distinct full-width text, so a
    /// stranded stale cell from an under-clip is a visible mismatch rather
    /// than two default spaces comparing equal by accident.
    fn seed_grid(model: &mut Model, width: u16, height: u16) {
        set_term_size(model, width, height);
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

    /// A toast wide enough to reach across the tree sidebar, and a frame
    /// whose whole damage is one sidebar row.
    ///
    /// The toast itself is unchanged, so it names no damaged row of its
    /// own, yet it still covers one -- which is what puts its painter on
    /// this frame at all. Laying its whole rect out there writes the top
    /// border back over the sidebar's own top edge, on a row the sidebar
    /// (clipped row by row) never paints again, and the stale border sits
    /// in the sidebar's frame until something else happens to damage that
    /// row. The compat harness caught this as a native tree that opened
    /// and then never showed the directory it had just scanned.
    #[test]
    fn clip_matches_full_toast_over_the_sidebar_leaves_its_undamaged_rows() {
        assert_clip_matches_full(
            60,
            12,
            |m| {
                seed_grid(m, 60, 12);
                // longer than the grid is wide, so the box spans the
                // sidebar's own columns instead of sitting clear of them,
                // and an error kind so an incidental keypress cannot
                // dismiss it between the two frames
                apply(
                    m,
                    view_core::events::UiEvent::MsgShow {
                        kind: "echoerr".into(),
                        content: vec![(0, "e".repeat(80))],
                        replace_last: false,
                    },
                );
                open_tree_with_entries(m);
            },
            60,
            12,
            |m| type_key(m, "<Down>"),
        );
    }

    /// The same shape for the completion popup, which shares the toast's
    /// defect exactly: an unframed multi-row layer under a row-clipped
    /// sidebar, unchanged this frame and therefore damaging none of its own
    /// rows, admitted by the rect-level gate because it happens to cover one
    /// the sidebar changed.
    #[test]
    fn clip_matches_full_popupmenu_over_the_sidebar_leaves_its_undamaged_rows() {
        assert_clip_matches_full(
            60,
            12,
            |m| {
                seed_grid(m, 60, 12);
                // anchored at the sidebar's own top-left, so the menu's
                // first rows are rows the tree's frame owns and a selection
                // move never repaints
                apply(
                    m,
                    view_core::events::UiEvent::PopupmenuShow {
                        items: ["alpha", "beta", "gamma", "delta"]
                            .into_iter()
                            .map(|word| view_core::events::PmItem {
                                word: word.into(),
                                ..Default::default()
                            })
                            .collect(),
                        selected: 0,
                        row: 0,
                        col: 0,
                        grid: 0,
                    },
                );
                open_tree_with_entries(m);
            },
            60,
            12,
            |m| type_key(m, "<Down>"),
        );
    }

    /// The startup shell under the same sidebar. Its two writes are the
    /// screen's middle and bottom rows, neither of which a selection move
    /// damages, and both of which the tree's frame covers.
    #[test]
    fn clip_matches_full_shell_under_the_sidebar_leaves_its_undamaged_rows() {
        assert_clip_matches_full(
            60,
            12,
            |m| {
                set_term_size(m, 60, 12);
                // the shell layer exists only before the first real content
                // flush, which is also when a sidebar opened from the
                // command line is already on screen
                m.content_painted = false;
                open_tree_with_entries(m);
            },
            60,
            12,
            |m| type_key(m, "<Down>"),
        );
    }

    /// Predicted glyphs under the same sidebar. Their coordinates are the
    /// grid's own, so the clip reads the absolute row rather than a
    /// rect-relative one, and a prediction still pending from an earlier
    /// keystroke damages nothing on the frame the sidebar moves.
    #[test]
    fn clip_matches_full_speculation_under_the_sidebar_leaves_its_undamaged_rows() {
        assert_clip_matches_full(
            60,
            12,
            |m| {
                seed_grid(m, 60, 12);
                let stamp = view_core::native::speculate::SpecStamp::new(std::time::Duration::ZERO);
                // two rows far enough apart that the layer rect spans the
                // sidebar rows a selection move damages as well as ones it
                // does not
                for row in [0, 8] {
                    assert!(
                        m.speculate
                            .predict("insert", 'z', (row, 2), stamp)
                            .is_some(),
                        "the fixture must leave a prediction pending"
                    );
                }
                open_tree_with_entries(m);
            },
            60,
            12,
            |m| type_key(m, "<Down>"),
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

        let mut first = true;
        for (label, mutate) in steps {
            mutate(&mut model);
            let grid_damage = model.take_paint_damage();
            let surface = view_surface::render(&model);
            let overlay_damage = shadow.overlay_damage(&surface);
            let damage =
                Damage::from_frame(&grid_damage, model.chrome_rows(), &overlay_damage, first);
            first = false;
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

    /// A model whose only chrome is the terminal capabilities under test.
    fn caps_model(sync: bool, truecolor: bool, kitty: bool) -> Model {
        let mut model = Model::new();
        model.caps = view_core::model::TermCaps::from_probe(sync, truecolor, kitty);
        model
    }

    fn native_picker() -> LayerKind {
        LayerKind::Picker(
            view_core::native::views::PickerView::new("Files")
                .with_query("ma")
                .with_rows(vec!["src/main.rs".to_string(), "src/lib.rs".to_string()])
                .with_selected(1),
        )
    }

    /// Paints `layer` alone into a `width` x `height` terminal and returns
    /// the painted buffer.
    fn paint_layer_alone(
        model: &Model,
        layer: Layer,
        width: u16,
        height: u16,
    ) -> ratatui::buffer::Buffer {
        let surface = Surface::from_layers(vec![layer]);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(model, &surface, f)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn row_text(buf: &ratatui::buffer::Buffer, row: u16, from: u16, to: u16) -> String {
        (from..to)
            .map(|c| buf[(c, row)].symbol().to_string())
            .collect()
    }

    /// The painter must blit the layout pass verbatim. If it ever grows a
    /// second opinion about framing, the goldens in `view-oracle` (which
    /// go through the same layout pass, not through this painter) stop
    /// describing what a terminal actually receives.
    #[test]
    fn a_native_overlay_paints_exactly_the_rows_the_layout_pass_produced() {
        for (sync, truecolor, kitty) in [
            (true, true, true),
            (false, true, false),
            (false, false, false),
        ] {
            let model = caps_model(sync, truecolor, kitty);
            let borders = view_surface::overlay::BorderSet::for_tier(model.caps.tier);
            let rect = Rect::new(1, 2, 24, 7);
            let layer = Layer::new(rect, native_picker(), model.caps.tier);
            let buf = paint_layer_alone(&model, layer, 30, 10);
            let laid = view_surface::overlay::rows(24, 7, &native_picker(), borders);
            for (i, line) in laid.lines.iter().enumerate() {
                let row = u16::try_from(i).unwrap();
                assert_eq!(
                    row_text(&buf, 1 + row, 2, 26),
                    view_surface::overlay::line_text(line),
                    "tier {:?} row {row}",
                    model.caps.tier
                );
            }
        }
    }

    /// An overlay takes the colorscheme whatever the color probe found,
    /// because the grid layer beneath it already does: `COLORTERM` is not
    /// forwarded over ssh, and the gate that used to stand here painted a
    /// default-background box on top of a fully themed buffer on every
    /// remote session. Every cell of the rect -- frame, padding, text --
    /// carries the theme's background, so nothing underneath shows through
    /// and no cell resets to the terminal default.
    #[test]
    fn an_overlay_takes_the_theme_whatever_the_color_probe_found() {
        for (sync, truecolor, kitty) in [(true, true, true), (false, false, false)] {
            let mut model = caps_model(sync, truecolor, kitty);
            model.engine.apply_grid(GridOp::Resize {
                width: 30,
                height: 10,
            });
            apply(
                &mut model,
                view_core::events::UiEvent::DefaultColorsSet {
                    fg: Some(0x00FF_FFFF),
                    bg: Some(0x0011_2233),
                    sp: None,
                },
            );
            let borders = view_surface::overlay::BorderSet::for_tier(model.caps.tier);
            let laid = view_surface::overlay::rows(24, 7, &native_picker(), borders);
            let selected = laid.selected.expect("the picker has a selection");
            let layer = Layer::new(Rect::new(1, 2, 24, 7), native_picker(), model.caps.tier);
            let buf = paint_layer_alone(&model, layer, 30, 10);
            for row in 1..8_u16 {
                for col in 2..26_u16 {
                    // the selected row's interior carries the selection's own
                    // background, themed just as explicitly (see
                    // `an_unthemed_selection_swaps_the_themes_colors_instead_of_inverting`);
                    // its two frame cells stay the frame's, since a
                    // highlighted row does not highlight the box around it
                    let on_frame = col == 2 || col == 25;
                    let expected = if row - 1 == selected && !on_frame {
                        rgb(0x00FF_FFFF)
                    } else {
                        rgb(0x0011_2233)
                    };
                    let cell = &buf[(col, row)];
                    assert_eq!(
                        cell.bg, expected,
                        "({col},{row}) bg at truecolor={truecolor}"
                    );
                    assert_ne!(
                        cell.fg,
                        Color::Reset,
                        "({col},{row}) fg at truecolor={truecolor}"
                    );
                }
            }
        }
    }

    /// A selected row takes the colorscheme's own selection background, not
    /// the reverse-video attribute: `ESC[7m` inverts whatever the terminal's
    /// ambient colors happen to be, which is how a full-width bar in a color
    /// no colorscheme chose ended up across the Confirm prompt.
    #[test]
    fn the_selected_row_takes_a_themed_background_rather_than_reverse_video() {
        let mut model = caps_model(true, true, true);
        apply(
            &mut model,
            view_core::events::UiEvent::DefaultColorsSet {
                fg: Some(0x00C8_C8C8),
                bg: Some(0x0028_2A36),
                sp: None,
            },
        );
        // a colorscheme that defines PmenuSel the way dracula does: its own
        // background, no reverse flag
        apply(
            &mut model,
            view_core::events::UiEvent::HlAttrDefine {
                id: 7,
                fg: Some(0x00F8_F8F2),
                bg: Some(0x0044_475A),
                bold: false,
                italic: false,
                underline: false,
                reverse: false,
            },
        );
        apply(
            &mut model,
            view_core::events::UiEvent::HlGroupSet {
                name: ChromeGroup::PmenuSel.hl_name().to_string(),
                hl_id: 7,
            },
        );
        let borders = view_surface::overlay::BorderSet::for_tier(model.caps.tier);
        let layer = Layer::new(Rect::new(1, 2, 24, 7), native_picker(), model.caps.tier);
        let laid = view_surface::overlay::rows(24, 7, &native_picker(), borders);
        let selected = laid.selected.expect("the picker has a selection");
        let buf = paint_layer_alone(&model, layer, 30, 10);
        for row in 0..u16::try_from(laid.lines.len()).unwrap() {
            let cell = &buf[(4, 1 + row)];
            assert!(
                !cell.modifier.contains(Modifier::REVERSED),
                "row {row} was sent reverse video"
            );
            let expected = if row == selected {
                rgb(0x0044_475A)
            } else {
                rgb(0x0028_2A36)
            };
            assert_eq!(cell.bg, expected, "row {row} background");
        }
    }

    /// A styled span inside a float keeps the float's background, whatever
    /// the colorscheme said about the group behind its role.
    ///
    /// The live defect: most colorschemes color a group's foreground and
    /// leave its background alone, and an unset background resolves to the
    /// buffer's own. Painted into an overlay that is the one background it
    /// must not be, so every user prompt row in the agent panel came out on
    /// the buffer's color -- a hole in an opaque box, in the shape of the
    /// text.
    #[test]
    fn a_styled_span_in_a_float_keeps_the_floats_background_not_the_buffers() {
        let buffer_bg = 0x0028_2A36;
        let float_bg = 0x0021_222C;
        let mut model = caps_model(true, true, true);
        apply(
            &mut model,
            view_core::events::UiEvent::DefaultColorsSet {
                fg: Some(0x00F8_F8F2),
                bg: Some(buffer_bg),
                sp: None,
            },
        );
        for (id, group, fg, bg) in [
            (
                11_u64,
                ChromeGroup::NormalFloat,
                0x00F8_F8F2,
                Some(float_bg),
            ),
            // the shape a colorscheme actually ships: a color for the text,
            // nothing for the box behind it
            (12, ChromeGroup::Question, 0x0050_FA7B, None),
        ] {
            apply(
                &mut model,
                view_core::events::UiEvent::HlAttrDefine {
                    id,
                    fg: Some(fg),
                    bg,
                    bold: false,
                    italic: false,
                    underline: false,
                    reverse: false,
                },
            );
            apply(
                &mut model,
                view_core::events::UiEvent::HlGroupSet {
                    name: group.hl_name().to_string(),
                    hl_id: id,
                },
            );
        }
        let panel = LayerKind::Ai(
            view_core::native::views::AiPanelView::new("AI Agent")
                .with_rows(vec![vec![Span::new("propose", StyleRole::AiUser)]]),
        );
        assert_eq!(
            StyleRole::AiUser.chrome_group(),
            Some(ChromeGroup::Question),
            "this test colors the group the role reads; a re-pointed role would leave it painting nothing"
        );
        let layer = Layer::new(Rect::new(0, 0, 24, 7), panel, model.caps.tier);
        let buf = paint_layer_alone(&model, layer, 24, 7);
        let mut found = false;
        for row in 0..7_u16 {
            for col in 0..24_u16 {
                if buf[(col, row)].symbol() != "p" || buf[(col, row)].fg != rgb(0x0050_FA7B) {
                    continue;
                }
                found = true;
                assert_eq!(
                    buf[(col, row)].bg,
                    rgb(float_bg),
                    "({col},{row}) paints the styled span on the buffer's own background"
                );
            }
        }
        assert!(found, "the styled span never reached the panel's interior");
    }

    /// A colorscheme that never defines `PmenuSel` leaves it on the theme's
    /// reverse-flagged emphasis fallback. That flag is resolved into the
    /// theme's own two colors, swapped, rather than sent as an attribute --
    /// so the selection still stands out and the row still carries a
    /// concrete background no layer beneath can show through.
    #[test]
    fn an_unthemed_selection_swaps_the_themes_colors_instead_of_inverting() {
        let mut model = caps_model(true, true, true);
        apply(
            &mut model,
            view_core::events::UiEvent::DefaultColorsSet {
                fg: Some(0x00C8_C8C8),
                bg: Some(0x0011_2233),
                sp: None,
            },
        );
        let borders = view_surface::overlay::BorderSet::for_tier(model.caps.tier);
        let layer = Layer::new(Rect::new(1, 2, 24, 7), native_picker(), model.caps.tier);
        let laid = view_surface::overlay::rows(24, 7, &native_picker(), borders);
        let selected = laid.selected.expect("the picker has a selection");
        let buf = paint_layer_alone(&model, layer, 30, 10);
        let cell = &buf[(4, 1 + selected)];
        assert!(!cell.modifier.contains(Modifier::REVERSED));
        assert_eq!(cell.bg, rgb(0x00C8_C8C8), "the theme's foreground, swapped");
        assert_eq!(cell.fg, rgb(0x0011_2233), "the theme's background, swapped");
    }

    /// Reverse video is the one selection signal every terminal honours,
    /// so it must survive when there is no color at all to swap -- a
    /// pre-attach frame, where the theme carries neither foreground nor
    /// background yet.
    #[test]
    fn the_selected_row_reverses_even_with_no_color_available() {
        let model = caps_model(false, false, false);
        let borders = view_surface::overlay::BorderSet::for_tier(model.caps.tier);
        let layer = Layer::new(Rect::new(1, 2, 24, 7), native_picker(), model.caps.tier);
        let laid = view_surface::overlay::rows(24, 7, &native_picker(), borders);
        let selected = laid.selected.expect("the picker has a selection");
        let buf = paint_layer_alone(&model, layer, 30, 10);
        for row in 0..u16::try_from(laid.lines.len()).unwrap() {
            let reversed = buf[(4, 1 + row)].modifier.contains(Modifier::REVERSED);
            assert_eq!(reversed, row == selected, "row {row} reversed={reversed}");
        }
    }

    /// A themed model whose buffer is filled edge to edge with cells
    /// carrying a background *and* a modifier of their own -- the
    /// cursorline highlight that ran the full window width underneath the
    /// toast in the live repro. Everything an overlay paints over this must
    /// own its cells outright.
    ///
    /// The italic is what makes the two tests below load-bearing.
    /// `ratatui::buffer::Cell::set_style` overwrites a background the
    /// caller does set, so a themed chrome style hides a missing reset from
    /// any background assertion; modifiers are only ever unioned in, and no
    /// chrome style clears them. A cell that is not reset therefore keeps
    /// this italic, and nothing else in the frame can put it there --
    /// italic rather than bold because an overlay's title is legitimately
    /// bold, so bold would not distinguish a bleed from the frame's own
    /// chrome.
    fn model_over_a_highlighted_buffer(width: u16, height: u16) -> Model {
        // the ssh case: no COLORTERM, so nothing about this frame may depend
        // on the color probe having found one
        let mut model = caps_model(false, false, false);
        model.engine.apply_grid(GridOp::Resize { width, height });
        apply(
            &mut model,
            view_core::events::UiEvent::DefaultColorsSet {
                fg: Some(0x00F8_F8F2),
                bg: Some(0x0028_2A36),
                sp: None,
            },
        );
        apply(
            &mut model,
            view_core::events::UiEvent::HlAttrDefine {
                id: 5,
                fg: None,
                bg: Some(UNDERLYING_BG),
                bold: false,
                italic: true,
                underline: false,
                reverse: false,
            },
        );
        for row in 0..height {
            model.engine.apply_grid(GridOp::PutLine {
                row,
                col_start: 0,
                cells: vec![("x".into(), 5, u64::from(width))],
            });
        }
        model
    }

    /// The background every cell of [`model_over_a_highlighted_buffer`]'s
    /// buffer carries, and therefore the one no overlay cell may show.
    const UNDERLYING_BG: u32 = 0x0044_475A;

    /// The bleed the first dogfood session caught: one character mid-row
    /// inside an overlay carried the background of the layer beneath it
    /// while its neighbours ran default. `ratatui::buffer::Cell::set_style`
    /// merges rather than replaces, so any style field a chrome layer left
    /// unset kept whatever the grid painted into that cell in the same
    /// frame -- visible only on the cells whose underlying background was
    /// not the default one.
    #[test]
    fn no_background_from_the_layer_beneath_survives_into_an_overlay_row() {
        let model = model_over_a_highlighted_buffer(40, 12);
        let tier = model.caps.tier;
        let rect = Rect::new(2, 3, 24, 7);
        let surface = Surface::from_layers(vec![
            Layer::new(Rect::new(0, 0, 40, 12), LayerKind::EngineGrid, tier),
            Layer::new(rect, native_picker(), tier),
        ]);
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        assert_eq!(
            buf[(0, 0)].bg,
            rgb(UNDERLYING_BG),
            "the fixture's own premise: the layer beneath is painted"
        );
        assert!(
            buf[(0, 0)].modifier.contains(Modifier::ITALIC),
            "the fixture's own premise: the layer beneath carries a modifier"
        );
        let borders = view_surface::overlay::BorderSet::for_tier(tier);
        let laid = view_surface::overlay::rows(rect.width, rect.height, &native_picker(), borders);
        let selected = laid.selected.expect("the picker has a selection");
        let last_col = rect.col + rect.width - 1;
        for row in rect.row..rect.row + rect.height {
            for col in rect.col..rect.col + rect.width {
                // the selected row's interior takes the selection background;
                // its frame cells, like every other frame cell, take the
                // overlay's own
                let on_frame = col == rect.col || col == last_col;
                let expected = if row - rect.row == selected && !on_frame {
                    rgb(0x00F8_F8F2)
                } else {
                    rgb(0x0028_2A36)
                };
                assert_ne!(
                    buf[(col, row)].bg,
                    rgb(UNDERLYING_BG),
                    "({col},{row}) shows the layer beneath through the overlay"
                );
                assert_eq!(
                    buf[(col, row)].bg,
                    expected,
                    "({col},{row}) is not the overlay's own background"
                );
                assert!(
                    !buf[(col, row)].modifier.contains(Modifier::ITALIC),
                    "({col},{row}) kept the modifier of the layer beneath"
                );
            }
        }
    }

    /// The same opacity contract for the message toast, which is where the
    /// user actually met it: a default-background box sitting inside a
    /// selection-colored bar that ran on past both its edges.
    #[test]
    fn a_toast_owns_every_cell_of_its_box_including_the_border() {
        let model = model_over_a_highlighted_buffer(40, 12);
        let tier = model.caps.tier;
        let rect = Rect::new(0, 27, 13, 3);
        let lines = vec![vec![Span::plain("saved".to_string())]];
        let surface = Surface::from_layers(vec![
            Layer::new(Rect::new(0, 0, 40, 12), LayerKind::EngineGrid, tier),
            Layer::new(rect, LayerKind::Messages(lines), tier),
        ]);
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        let theme = Theme::from_hl(model.engine.hl());
        let expected = rgb(theme.chrome(ChromeGroup::MsgArea).bg.unwrap());
        for row in rect.row..rect.row + rect.height {
            for col in rect.col..rect.col + rect.width {
                assert_eq!(
                    buf[(col, row)].bg,
                    expected,
                    "({col},{row}) is not the toast's own background"
                );
                assert!(
                    !buf[(col, row)].modifier.contains(Modifier::ITALIC),
                    "({col},{row}) kept the modifier of the layer beneath"
                );
            }
        }
    }

    /// An overlay gets the floating-window group the colorscheme already
    /// defines, with the frame dimmed off the interior's own foreground
    /// rather than sharing it.
    #[test]
    fn a_native_overlay_is_framed_in_a_dimmed_shade_of_its_interior_color() {
        let mut model = caps_model(true, true, true);
        apply(
            &mut model,
            view_core::events::UiEvent::DefaultColorsSet {
                fg: Some(0x00C8_C8C8),
                bg: Some(0x0011_2233),
                sp: None,
            },
        );
        let layer = Layer::new(Rect::new(1, 2, 24, 7), native_picker(), model.caps.tier);
        let buf = paint_layer_alone(&model, layer, 30, 10);
        let theme = Theme::from_hl(model.engine.hl());
        assert_eq!(
            buf[(2, 1)].fg,
            rgb(border_color(theme.chrome(ChromeGroup::NormalFloat))),
            "corner glyph"
        );
        assert_eq!(
            buf[(4, 2)].fg,
            rgb(theme.chrome(ChromeGroup::NormalFloat).fg.unwrap()),
            "interior text"
        );
        assert_ne!(
            buf[(2, 1)].fg,
            buf[(4, 2)].fg,
            "the frame must be distinguishable from the text it encloses"
        );
    }

    /// A rect wider and taller than the terminal must paint what fits and
    /// stop, never index past the buffer.
    #[test]
    fn a_native_overlay_larger_than_the_terminal_is_clipped_not_panicked() {
        // `caps_model` alone establishes no theme, so every cell's derived
        // background is `Color::Reset` by construction regardless
        // of whether the paint walk actually reached it -- that would make
        // this test's clip-boundary proof below vacuous. A real
        // `DefaultColorsSet`, the same setup
        // `a_native_overlay_is_framed_in_a_dimmed_shade_of_its_interior_color`
        // uses, gives the interior a non-`Reset` color the walk can
        // actually be caught failing to reach.
        let mut model = caps_model(true, true, true);
        apply(
            &mut model,
            view_core::events::UiEvent::DefaultColorsSet {
                fg: Some(0x00C8_C8C8),
                bg: Some(0x0011_2233),
                sp: None,
            },
        );
        let layer = Layer::new(Rect::new(1, 2, 400, 400), native_picker(), model.caps.tier);
        let buf = paint_layer_alone(&model, layer, 12, 5);
        assert_eq!(
            &buf[(2, 1)].symbol(),
            &"╭",
            "the top-left corner lands at the rect origin"
        );
        // `buf.area.width` merely echoes the TestBackend size this test
        // constructed -- it is true even if the painter stopped after one
        // cell. The bottom-right visible cell (the far clipped edge of the
        // 400x400 rect) must still carry the overlay's own painted
        // background rather than the buffer's untouched `Color::Reset`
        // default, proving the paint walk actually reached and filled all
        // the way to the clip boundary instead of bailing out early.
        assert_ne!(
            buf[(11, 4)].bg,
            ratatui::style::Color::Reset,
            "the clipped bottom-right cell was never painted"
        );
    }

    /// A non-overlay kind carries no border charset, and reaching the
    /// native-overlay painter with one would frame a layer that has no
    /// rows to frame. `Layer::new` cannot build that pairing (see
    /// `view_surface::Layer::new`), so what is left to pin here is that the
    /// painter's own guard refuses the rect rather than blanking it.
    #[test]
    fn a_layer_with_no_border_charset_paints_nothing() {
        let model = caps_model(true, true, true);
        let layer = Layer::new(
            Rect::new(1, 2, 24, 7),
            LayerKind::EngineGrid,
            model.caps.tier,
        );
        assert!(
            layer.borders.is_none(),
            "a non-overlay kind carries no frame"
        );
        let theme = Theme::from_hl(model.engine.hl());
        let mut buf = Buffer::empty(ratatui::layout::Rect::new(0, 0, 30, 10));
        paint_native_overlay(
            &layer,
            None,
            &theme,
            ratatui::layout::Rect::new(2, 1, 24, 7),
            &Damage::full(),
            &mut buf,
        );
        for row in 0..10_u16 {
            for col in 0..30_u16 {
                assert_eq!(&buf[(col, row)].symbol(), &" ", "({col},{row})");
            }
        }
    }

    /// A statusline exercising every role the statusline feature row
    /// names: mode, file, the modified marker, both diagnostic roles side
    /// by side, a git branch, and the ruler.
    fn spanful_statusline() -> LayerKind {
        LayerKind::Statusline(view_core::native::views::StatuslineView::from_spans(
            vec![Span::new("-- INSERT --", StyleRole::Mode)],
            vec![
                Span::new("paint.rs", StyleRole::File),
                Span::new(" [+]", StyleRole::Modified),
                Span::plain("  "),
                Span::new("\u{25cf} 2", StyleRole::DiagnosticError),
                Span::plain("  "),
                Span::new("\u{25b2} 1", StyleRole::DiagnosticWarning),
                Span::plain("  "),
                Span::new("main", StyleRole::GitBranch),
            ],
            vec![Span::new("42:7", StyleRole::Ruler)],
        ))
    }

    /// A model whose live highlight table names every chrome group this
    /// test covers with its own distinct color, the way an attached
    /// colorscheme does -- not the coarse Normal/Emphasis buckets `Theme`'s
    /// pre-attach fallback resolves to.
    ///
    /// Every group here is one nvim actually broadcasts through
    /// `hl_group_set`, which is the only way a color reaches this table at
    /// all; `view_core::theme`'s `every_group_is_one_nvim_broadcasts` pins
    /// that set. A group named here that nvim never announces would be
    /// defined by this fixture and by nothing else, so the test would pass
    /// over a mapping no real session can reach.
    fn model_with_distinctly_colored_chrome() -> Model {
        let mut model = caps_model(true, true, true);
        for (id, group, color) in [
            (1_u64, ChromeGroup::ModeMsg, 0x00FF_0000),
            (2, ChromeGroup::StatusLine, 0x00CC_CCCC),
            (3, ChromeGroup::WarningMsg, 0x00FF_A500),
            (4, ChromeGroup::Directory, 0x0000_88FF),
            (5, ChromeGroup::ErrorMsg, 0x00FF_3333),
        ] {
            apply(
                &mut model,
                view_core::events::UiEvent::HlAttrDefine {
                    id,
                    fg: Some(color),
                    bg: None,
                    bold: false,
                    italic: false,
                    underline: false,
                    reverse: false,
                },
            );
            apply(
                &mut model,
                view_core::events::UiEvent::HlGroupSet {
                    name: group.hl_name().to_string(),
                    hl_id: id,
                },
            );
        }
        model
    }

    /// The statusline's chrome-role contract requires its diagnostic
    /// glyphs, mode text, filename, git branch and ruler to read in
    /// distinct colors, not collapse to one flat style. Paints a real span
    /// row through the real terminal compositor and asserts every role's
    /// painted cells carry exactly the style its `chrome_group()` mapping
    /// dictates -- read back through `theme.chrome(...)`, never a
    /// hardcoded color, so the assertion survives a colorscheme change --
    /// and the load-bearing case: `DiagnosticError` and `DiagnosticWarning`
    /// must resolve to genuinely different painted colors rather than both
    /// merely differing from `Plain`. They reach that through `ErrorMsg`
    /// and `WarningMsg` -- the closest builtins nvim actually broadcasts,
    /// since the `Diagnostic*` groups they read like are Lua-defined and
    /// never announced (`view_core::theme::ChromeGroup::ErrorMsg`).
    #[test]
    fn statusline_roles_resolve_to_their_own_distinct_chrome_colors() {
        let model = model_with_distinctly_colored_chrome();
        let theme = Theme::from_hl(model.engine.hl());
        let borders = view_surface::overlay::BorderSet::for_tier(model.caps.tier);
        let width = 46_u16;
        let rect = Rect::new(1, 2, width, 1);
        let layer = Layer::new(rect, spanful_statusline(), model.caps.tier);
        let laid = view_surface::overlay::rows(width, 1, &spanful_statusline(), borders);
        let buf = paint_layer_alone(&model, layer, width + 4, 4);

        let mut seen: std::collections::HashSet<StyleRole> = std::collections::HashSet::new();
        let mut diagnostic_error_fg = None;
        let mut diagnostic_warning_fg = None;
        let mut col = rect.col;
        for span in &laid.lines[0] {
            let span_width = u16::try_from(UnicodeWidthStr::width(span.text.as_str())).unwrap();
            if let Some(group) = span.role.chrome_group() {
                let expected = ratatui_style(theme.chrome(group));
                for c in col..col + span_width {
                    assert_eq!(
                        buf[(c, rect.row)].fg,
                        expected.fg.unwrap(),
                        "role {:?} at column {c} did not resolve through its own chrome group",
                        span.role
                    );
                }
                seen.insert(span.role);
                match span.role {
                    StyleRole::DiagnosticError => diagnostic_error_fg = Some(expected.fg.unwrap()),
                    StyleRole::DiagnosticWarning => {
                        diagnostic_warning_fg = Some(expected.fg.unwrap());
                    }
                    _ => {}
                }
            }
            col += span_width;
        }

        for role in [
            StyleRole::Mode,
            StyleRole::File,
            StyleRole::GitBranch,
            StyleRole::Ruler,
            StyleRole::DiagnosticError,
            StyleRole::DiagnosticWarning,
        ] {
            assert!(
                seen.contains(&role),
                "role {role:?} never appeared in the laid-out row"
            );
        }
        assert_ne!(
            diagnostic_error_fg, diagnostic_warning_fg,
            "DiagnosticError and DiagnosticWarning must resolve to visually distinct \
             colors, not collapse to one"
        );
        // both colors came off the live table, not off a fallback: a role
        // mapped to a group nvim never broadcasts would still satisfy the
        // assertion above (two fallbacks can differ), which is exactly how
        // the Diagnostic* mapping shipped unnoticed
        let theme = Theme::from_hl(model.engine.hl());
        for group in [ChromeGroup::ErrorMsg, ChromeGroup::WarningMsg] {
            assert_eq!(
                theme.chrome(group).fg,
                Some(match group {
                    ChromeGroup::ErrorMsg => 0x00FF_3333,
                    _ => 0x00FF_A500,
                }),
                "{} resolved from its fallback rather than the colorscheme's own \
                 highlight, so this test would pass over an unreachable mapping",
                group.hl_name()
            );
        }
    }

    /// The title set into an overlay's top border reads as a label, not as
    /// more border: it takes `FloatTitle`'s own color and renders bold,
    /// while the horizontal runs on either side of it keep the frame's
    /// deliberately dimmed one. Painting the row in a single style is what
    /// made the one word naming the overlay its least legible text.
    /// Run over the same capability permutations as
    /// `a_native_overlay_paints_exactly_the_rows_the_layout_pass_produced`,
    /// which now expect the identical painted style from all three: the
    /// probed color bit stopped deciding an overlay's colors when it was
    /// found to strip them from every ssh session, where `COLORTERM` never
    /// arrives.
    #[test]
    fn an_overlay_title_paints_brighter_and_bolder_than_the_frame_around_it() {
        for (sync, truecolor, kitty) in [
            (true, true, true),
            (false, true, false),
            (false, false, false),
        ] {
            let mut model = caps_model(sync, truecolor, kitty);
            // italic and underline are set on the group deliberately: the
            // title must carry its group's whole resolved style the way a
            // content row's roles do, not just the foreground
            apply(
                &mut model,
                view_core::events::UiEvent::HlAttrDefine {
                    id: 9,
                    fg: Some(0x00FF_EE00),
                    bg: None,
                    bold: false,
                    italic: true,
                    underline: true,
                    reverse: false,
                },
            );
            apply(
                &mut model,
                view_core::events::UiEvent::HlGroupSet {
                    name: ChromeGroup::FloatTitle.hl_name().to_string(),
                    hl_id: 9,
                },
            );
            let theme = Theme::from_hl(model.engine.hl());
            let rect = Rect::new(1, 2, 24, 7);
            let layer = Layer::new(rect, native_picker(), model.caps.tier);
            let buf = paint_layer_alone(&model, layer, 30, 10);

            // the top edge is `<corner><rule> Files <rule...><corner>` on
            // every tier, so the title's own glyphs start three columns
            // into the rect whichever charset drew it
            let title_cell = &buf[(rect.col + 3, rect.row)];
            let edge_cell = &buf[(rect.col, rect.row)];
            let at = format!("truecolor={truecolor}");
            assert_eq!(
                title_cell.symbol(),
                "F",
                "the assertion must be reading the title's own cells ({at})"
            );
            assert_eq!(
                title_cell.fg,
                rgb(theme.chrome(ChromeGroup::FloatTitle).fg.unwrap()),
                "the title resolves through FloatTitle, not the border color ({at})"
            );
            assert_ne!(
                edge_cell.fg, title_cell.fg,
                "the corner keeps the frame's dimmed color; a shared style is the \
                 defect ({at})"
            );
            assert!(
                title_cell.modifier.contains(Modifier::ITALIC)
                    && title_cell.modifier.contains(Modifier::UNDERLINED),
                "the group's own attributes reach the title, not only its fg ({at})"
            );
            assert!(
                !edge_cell.modifier.contains(Modifier::ITALIC),
                "the frame keeps its own style; the title's attributes are the \
                 title's ({at})"
            );
            assert!(
                title_cell.modifier.contains(Modifier::BOLD),
                "the title is bold on every tier: with no color established it is \
                 the only distinction a terminal can carry ({at})"
            );
            assert!(
                !edge_cell.modifier.contains(Modifier::BOLD),
                "only the title is bold, not the run of border it sits in ({at})"
            );
        }
    }
}
