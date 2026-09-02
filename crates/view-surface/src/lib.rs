//! The render model: what to draw, independent of any frontend.
//!
//! [`render`] turns a [`Model`] into a [`Surface`]: an ordered list of
//! [`Layer`]s plus the real terminal cursor's position and shape. Pure
//! data, no drawing here; `view-tui` is the only crate that turns a
//! `Surface` into pixels.

pub mod cache;
pub mod overlay;

pub use cache::SurfaceCache;

use unicode_width::UnicodeWidthStr;
use view_core::events::{saturate_u16, PmItem};
use view_core::model::{
    CmdlineState, Model, Overlay, OverlayKind, PopupmenuState, TablineState, TermCaps,
};
use view_core::native::geometry::{OverlayBox, OverlayRect};
use view_core::native::palette::PaletteState;
use view_core::native::prompt::PromptState;
use view_core::native::speculate::PredictedCell;
use view_core::native::views::{
    AiPanelView, PaletteView, PickerView, PromptView, Span, StatuslineView, TreeView,
};

use crate::overlay::BorderSet;

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
    /// A rect at `row`/`col` of `width` x `height`, in whatever coordinate
    /// space the caller is working in (this type carries no opinion about
    /// grid space versus terminal space).
    ///
    /// `Rect` is `#[non_exhaustive]`, so this is how any other crate builds
    /// one; the fields stay public because reading them is not a
    /// compatibility hazard the way a struct literal is.
    #[must_use]
    pub fn new(row: u16, col: u16, width: u16, height: u16) -> Self {
        Self {
            row,
            col,
            width,
            height,
        }
    }

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
    /// The glyphs this layer's frame is drawn from, or `None` for a layer
    /// that carries no frame of its own.
    ///
    /// Resolved here, at render time, from the terminal capabilities
    /// [`Model::caps`] already holds, rather than at paint time: a `Surface`
    /// is then a complete description of the frame, and every consumer of
    /// one (the terminal painter, the oracle's rasterizer, a golden
    /// snapshot) draws the same border without a second capability lookup
    /// they could each answer differently.
    pub borders: Option<overlay::BorderSet>,
}

/// What a [`Layer`] paints.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum LayerKind {
    /// The embedded engine's grid, full-frame at z0.
    EngineGrid,
    /// The command line, present while nvim's command line is open.
    Cmdline(CmdlineState),
    /// One notice's toast box: its physical lines, already selected by
    /// `Messages::visible_toasts` (persistent error/warn boxes kept, the
    /// most recent transient ones filling what room remains) and split on
    /// the entry's own embedded line breaks -- one row per span-vec, in
    /// display order top to bottom.
    ///
    /// One layer per visible notice rather than one for the stack, which is
    /// what lets the top slot's box leave to the right while the ones below
    /// it slide up: two rects moving in different directions cannot be one
    /// rect. `slot` is this box's position in the painted stack counted from
    /// the top, the departing box included for as long as it is still on
    /// screen; `x_offset` is how many cells right of its home column it has
    /// travelled, `0` for every box that is not leaving. `rect` already
    /// carries both, so a painter never re-derives a position from them --
    /// they are what a test reads to tell one frame of the motion from the
    /// next.
    ///
    /// Each row is a single [`view_core::native::views::StyleRole::Plain`]
    /// span (a toast has no per-segment structure to preserve), kept as a
    /// span-vec rather than a `String` so the layer honestly carries the
    /// same overlay-row shape every other overlay layer does.
    Toast {
        lines: Vec<Vec<Span>>,
        slot: usize,
        x_offset: u16,
    },
    /// The open tabs, present once nvim has sent a `tabline_update`.
    Tabline(TablineState),
    /// The completion popup menu, present while it is open.
    Popupmenu(PopupmenuState),
    /// The pre-content startup shell: a themed statusline placeholder bar
    /// plus a static "waiting for nvim" indicator, painted over the
    /// (empty, pre-attach) `EngineGrid` layer. Present only while
    /// `Model::content_painted` is `false`; `render()` drops it for good
    /// once the first grid `Flush` arrives. No animation lives here or in
    /// its `view-tui` painter: the runtime loop is timer-free, so this is a
    /// fixed glyph, never a frame that advances on its own clock.
    Shell,
    /// A fuzzy picker's prompt line and candidate rows.
    Picker(PickerView),
    /// A file tree's visible entries.
    Tree(TreeView),
    /// A native statusline's three composed segments.
    Statusline(StatuslineView),
    /// A prompt's question, typed answer, and fixed choices.
    Prompt(PromptView),
    /// A command palette's prompt line, commands, and their bindings.
    Palette(PaletteView),
    /// The display-only glyphs
    /// [`view_core::native::speculate::SpeculateState::pending`] is holding
    /// ahead of the engine, painted over [`LayerKind::EngineGrid`] at the
    /// cells they are predicted for.
    ///
    /// Absent -- not merely empty -- from a `Surface` whenever nothing is
    /// pending, which is every frame outside a typing burst. A frame with no
    /// prediction to show therefore hands a painter and damage tracking
    /// nothing to do and forces no rebuild; what it does still spend is a
    /// frame builder re-asking an empty pending list and finding no layer to
    /// reconcile.
    ///
    /// Each cell's `row`/`col` stay in the engine grid's own coordinate
    /// space, the space `SpeculateState` produced them in, rather than being
    /// rebased onto the layer's rect: a painter already resolves the chrome
    /// offset for [`LayerKind::EngineGrid`], and one coordinate space for
    /// both grid-content layers is one fewer place the two can disagree
    /// about where a cell is. The rect is the bounding box of these cells in
    /// terminal space -- what the layer covers, for damage tracking and
    /// clipping -- never the whole grid: a full-grid rect would mark every
    /// row dirty on every keystroke of exactly the fast-typing burst
    /// speculation exists to accelerate.
    ///
    /// Every cell here is inside the live grid. A prediction that named a
    /// cell the grid does not have is dropped by [`render`] rather than
    /// clamped into range, per [`PredictedCell`]'s own contract: a clamped
    /// prediction paints a glyph the user never typed at the last real
    /// column and leaves it there until an authoritative redraw touches that
    /// exact cell.
    Speculated(Vec<PredictedCell>),
    /// The agent panel's composer line and transcript rows.
    Ai(AiPanelView),
}

/// The exact indicator text a [`LayerKind::Shell`] layer puts on screen.
///
/// Nothing else in the product paints this string, so its presence on a
/// terminal identifies the pre-attach shell frame specifically -- which is
/// what the oracle's ordering assertion and the bench matrix's
/// `shell_visible_cold_ms` boundary both match on. It lives here, beside the
/// layer whose content it is, because three separate hand-written copies
/// had already drifted apart: the reference rasterizer and the oracle test
/// both carried `"waiting for nvim"` while the painter wrote
/// `"view: waiting for nvim..."`, so a boundary matching the full literal
/// against the raster would never have fired.
pub const SHELL_PLACEHOLDER: &str = "view: waiting for nvim...";

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

impl LayerKind {
    /// Whether this kind is one of the native overlays [`overlay::rows`]
    /// lays out, and therefore the kind of layer that carries a frame.
    ///
    /// Matched exhaustively (`LayerKind` is `#[non_exhaustive]` only to
    /// consumers outside this crate), so a new variant cannot be added
    /// without deciding which side of the line it falls on, here and in
    /// `overlay::body` alike.
    #[must_use]
    pub const fn is_native_overlay(&self) -> bool {
        match self {
            Self::Picker(_)
            | Self::Tree(_)
            | Self::Statusline(_)
            | Self::Prompt(_)
            | Self::Palette(_)
            | Self::Ai(_) => true,
            Self::EngineGrid
            | Self::Cmdline(_)
            | Self::Toast { .. }
            | Self::Tabline(_)
            | Self::Popupmenu(_)
            | Self::Speculated(_)
            | Self::Shell => false,
        }
    }
}

impl Layer {
    /// The one [`Layer`] constructor: `kind` decides whether the layer
    /// carries a frame, and `caps` decides the charset it is drawn from
    /// when it does (see [`BorderSet::for_caps`]).
    ///
    /// Deriving the frame rather than accepting it is what makes the two
    /// silent-blank mismatches unrepresentable. A native overlay handed no
    /// charset painted nothing at all (`view-tui`'s painter has no frame to
    /// draw and refuses to draw half of one), and an engine grid handed a
    /// charset framed nothing (`overlay::rows` has no body to lay out) --
    /// in both directions a caller got an empty rect with nothing failing
    /// loudly. `kind` is the only fact either outcome ever depended on, so
    /// it is the only fact the caller supplies.
    #[must_use]
    pub fn new(rect: Rect, kind: LayerKind, caps: TermCaps) -> Self {
        let borders = kind.is_native_overlay().then(|| BorderSet::for_caps(caps));
        Self {
            rect,
            kind,
            borders,
        }
    }
}

/// What to paint: an ordered (z ascending) list of layers plus the real
/// terminal cursor spec.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct Surface {
    pub layers: Vec<Layer>,
    pub cursor: Option<CursorSpec>,
}

impl Surface {
    /// A surface holding exactly `layers`, with no terminal cursor.
    ///
    /// [`render`] builds a live frame from a [`Model`]; this builds one from
    /// a description, for a consumer that already knows the exact layers it
    /// wants painted -- a golden pinning how a layer is framed, or a
    /// scripted screen handed straight to a rasterizer. `Surface` is
    /// `#[non_exhaustive]`, so without this it cannot be built outside this
    /// crate at all, and every such consumer would have to route through a
    /// `Model` it has no other use for.
    #[must_use]
    pub fn from_layers(layers: Vec<Layer>) -> Self {
        Self {
            layers,
            cursor: None,
        }
    }

    /// Whether this frame carries anything a live prediction put there: the
    /// question a measurement asks of a terminal write it has to attribute.
    ///
    /// Answered from the built frame rather than from
    /// [`SpeculateState::pending`](view_core::native::speculate::SpeculateState::pending)
    /// directly, and the two are not the same question. The pending list is
    /// what the reconciler holds; this layer is what survived
    /// [`render`]'s off-grid filter and reached the painter, so a prediction
    /// naming a cell the grid does not have -- pending, never painted --
    /// answers `false` here, which is what a write-attribution needs.
    ///
    /// # A caret-only frame counts
    ///
    /// The predicted glyphs are written once and then sit unchanged in the
    /// painter's shadow, so a later frame taken while the same prediction is
    /// still pending emits no glyph bytes for it at all -- only the cursor
    /// escape, which [`cursor_spec`] has standing one past the last predicted
    /// cell. That write is still the prediction's doing (it names a column
    /// the engine's own cursor is not at), it still reaches the terminal, and
    /// it is counted: an attribution that ignored it would report a paint
    /// nothing explains on exactly the frames speculation is holding the
    /// caret out ahead.
    ///
    /// It needs no test of its own here because the caret cannot be advanced
    /// without this layer being present: `speculated_col` advances only for a
    /// pending cell that is on-grid, and every on-grid pending cell is in
    /// this layer (see `a_caret_advance_cannot_happen_without_the_layer`).
    #[must_use]
    pub fn carries_speculation(&self) -> bool {
        self.layers
            .iter()
            .any(|layer| matches!(&layer.kind, LayerKind::Speculated(cells) if !cells.is_empty()))
    }
}

/// Builds the [`Surface`] for one frame from `model`.
///
/// The tabline is the only persistent chrome: when it is showing (more
/// than one tab open), [`Model::chrome_rows`] reserves one row for it and
/// this offsets the `EngineGrid` layer, the cursor, and every grid-space
/// overlay (cmdline, messages, popupmenu) down by that many rows, so the
/// tabline's row is never shared with buffer content. Overlays otherwise
/// paint directly over the grid at rest: they are transient (present only
/// while their originating state is `Some`/non-empty) and vanish the frame
/// after nvim clears that state.
///
/// # Z-order
///
/// Ascending: the engine grid (with any speculation directly over it), then
/// the persistent chrome (shell placeholder, tabline, statusline), then the
/// native overlays in stack order, and last nvim's own transient surfaces --
/// cmdline (or the palette standing in for it), messages, popupmenu. Those
/// three are last because they are ephemeral notice: they exist only while
/// nvim is actively showing something and vanish the frame after, so a
/// native overlay that outranked them would silently swallow whatever the
/// editor was trying to say for as long as it stayed open.
///
/// Total: any `Model`, including a hostile or partially-initialized one,
/// yields a valid `Surface`. Never panics.
///
/// "Valid" is bounded by the engine grid's own reported `(grid_w, grid_h)`,
/// not by the terminal frame: the `EngineGrid` layer's rect is always
/// exactly `grid_w`x`grid_h` at `chrome_rows()`'s offset, by construction,
/// but for one transient frame -- a `TablineUpdate` crossing the 1-tab
/// chrome-reservation boundary before nvim's matching `GridResize` round
/// trips -- that offset rect can extend past `model.term_height`. This
/// function does not clip to the frame; the paint layer's `clip_to_frame`
/// is what keeps that transient from indexing past the terminal buffer.
#[must_use]
pub fn render(model: &Model) -> Surface {
    let engine = &model.engine;
    let (grid_w, grid_h) = engine.grid().size();
    let offset = model.chrome_rows();

    let mut layers = vec![Layer::new(
        Rect::new(offset, 0, grid_w, grid_h),
        LayerKind::EngineGrid,
        model.caps,
    )];

    // directly above the grid it predicts and below everything else: a
    // prediction is a guess about buffer content, so it belongs where buffer
    // content is, and an overlay that opened over it (a prompt, a picker,
    // the cmdline) is authoritative chrome that must never be shown through
    // a stale glyph underneath it
    if let Some(layer) = speculated_layer(model, (grid_w, grid_h), offset) {
        layers.insert(SPECULATED_LAYER_INDEX, layer);
    }

    if !model.content_painted {
        // sized from the real terminal, not the (still 0x0 pre-attach)
        // engine grid: the very first shell paint happens before nvim has
        // ever sent a grid_resize, so grid_w/grid_h are not yet meaningful
        // dimensions to paint a placeholder into
        layers.push(Layer::new(
            Rect::new(0, 0, model.term_width, model.term_height),
            LayerKind::Shell,
            model.caps,
        ));
    }

    if let Some(tabline) = &engine.tabline {
        // chrome_rows() is the single source for the tabline-visibility
        // rule (bare nvim's default `showtabline`: a single tab shows no
        // tabline row at all, so the grid keeps the full terminal height);
        // this layer's placement must never disagree with the row
        // reservation chrome_rows() feeds into grid_target(), or a row
        // gets reserved with nothing painted into it or the tabline paints
        // over buffer content
        if offset > 0 {
            layers.push(Layer::new(
                Rect::new(0, 0, grid_w, 1).clamp_to(grid_w, grid_h),
                LayerKind::Tabline(tabline.clone()),
                model.caps,
            ));
        }
    }
    if model.statusline_rows() > 0 {
        // `grid_target()` already shrank the engine's own grid by
        // `statusline_rows()`, so this row sits immediately below whatever
        // `grid_h` the engine actually reported -- never recomputed from
        // `term_height`, which would disagree the moment a resize is still
        // in flight to nvim.
        layers.push(Layer::new(
            Rect::new(
                offset.saturating_add(grid_h),
                0,
                grid_w,
                model.statusline_rows(),
            ),
            LayerKind::Statusline(engine.statusline.view(grid_w)),
            model.caps,
        ));
    }
    // native overlays sit above the grid and the persistent chrome, and
    // within the stack the tail is the one holding focus, so painting in
    // stack order puts the focused overlay on top of the ones it opened
    // over. They stop below nvim's own transient surfaces, which are pushed
    // after this: a toast, a completion menu or a cmdline is ephemeral
    // notice the user is being shown right now, and a right-pinned panel
    // covering the exact top-right corner the messages box pins itself to
    // hid every one of them -- including the panel's own review and
    // permission notices, which travel that same Messages layer.
    layers.extend(
        model
            .overlays()
            .iter()
            .filter_map(|open| native_layer(model, open)),
    );
    // whether a Prompt overlay currently holds the stack's top: it already
    // renders this exact cmdline state as its own floating input line (see
    // `overlay::prompt_body`), so painting it a second time here, in either
    // shape below, would double-paint the same typed text
    let prompt_open = matches!(
        model.overlays().last().map(|open| &open.kind),
        Some(OverlayKind::Prompt(_))
    );
    if let Some(cmdline) = &engine.cmdline {
        if prompt_open {
            // nothing to add: the Prompt overlay already covers this
        } else if model.palette_enabled {
            // only a cmdline-sourced popupmenu (`is_cmdline_sourced`) ever
            // renders inside the palette; a buffer-anchored completion
            // (insert-mode keyword/LSP completion) keeps its own
            // `Popupmenu` layer below, at the cursor, never here
            let completion = engine
                .popupmenu
                .as_ref()
                .filter(|pm| pm.is_cmdline_sourced())
                .cloned();
            // a plugin that draws its own cmdline menu never drives nvim's
            // popup menu, so `popupmenu_show` never fires and the palette
            // would stand empty beside it; the rows view read off that float
            // are the second source, and the engine's own still wins
            // whenever it has one (`PaletteState::with_absorbed`)
            let state = match (completion, engine.absorbed_rows()) {
                (None, Some(absorbed)) => {
                    PaletteState::with_absorbed(cmdline.clone(), absorbed.clone())
                }
                (completion, _) => PaletteState::new(cmdline.clone(), completion),
            };
            let rect = palette_rect(model, offset);
            layers.push(Layer::new(
                Rect::new(rect.row, rect.col, rect.width, rect.height),
                LayerKind::Palette(state.view()),
                model.caps,
            ));
        } else {
            layers.push(overlay_layer(
                Rect::new(grid_h.saturating_sub(1), 0, grid_w, 1),
                (grid_w, grid_h),
                offset,
                LayerKind::Cmdline(cmdline.clone()),
                model.caps,
            ));
        }
    }
    layers.extend(toast_layers(model, (grid_w, grid_h), offset));
    if let Some(pm) = &engine.popupmenu {
        // mirrors, term for term, the condition the cmdline block above
        // actually built a `completion` under: only when every one of
        // those held true did this same popupmenu's rows already reach a
        // palette box. Anything less -- palette off, a Prompt on top, or
        // (defensively) no cmdline open at all despite a cmdline-sourced
        // grid sentinel -- means nothing painted it, so it keeps its own
        // layer rather than vanishing with nowhere its rows were shown.
        let consumed_by_palette = model.palette_enabled
            && !prompt_open
            && engine.cmdline.is_some()
            && pm.is_cmdline_sourced();
        if !consumed_by_palette {
            if pm.is_cmdline_sourced() {
                if let Some(layer) =
                    cmdline_popupmenu_layer(model, grid_h, offset, engine.cmdline.as_ref(), pm)
                {
                    layers.push(layer);
                }
            } else {
                let row = saturate_u16(pm.row);
                let col = saturate_u16(pm.col);
                let width = popupmenu_width(&pm.items).min(grid_w).max(1);
                let height = u16::try_from(pm.items.len()).unwrap_or(u16::MAX).max(1);
                layers.push(overlay_layer(
                    Rect::new(row, col, width, height),
                    (grid_w, grid_h),
                    offset,
                    LayerKind::Popupmenu(pm.clone()),
                    model.caps,
                ));
            }
        }
    }

    let cursor = cursor_spec(model, offset, &layers);
    Surface { layers, cursor }
}

/// The widest of the given (already-selected-as-visible) physical lines, in
/// terminal display cells (not characters: a wide character, e.g. a CJK
/// ideograph, occupies two cells, and sizing this layer by char count
/// instead would clip or misalign exactly that content). Widest *line*, not
/// widest total: summing every line's width together (as if they shared one
/// row) would size the box far wider than any row it actually paints needs
/// to be.
fn messages_width(lines: &[Vec<Span>]) -> u16 {
    lines
        .iter()
        .map(|spans| spans.iter().map(|s| s.text.width()).sum::<usize>())
        .max()
        .and_then(|w| u16::try_from(w).ok())
        .unwrap_or(u16::MAX)
}

/// One framed toast box's outer size: the widest line it holds plus the
/// frame, clipped to the grid.
///
/// The floors match `Messages::visible_toasts`' own: a pre-attach frame
/// (`grid_w`/`grid_h` still 0, e.g. a native toast pushed before the
/// engine's first `GridResize`) still reserves a box rather than vanishing
/// until real grid content arrives, and `paint::toast`'s
/// width/height-under-2 guard is what degrades a grid too small for the
/// frame to a blank fill instead of a panic.
fn toast_box(lines: &[Vec<Span>], grid_w: u16) -> (u16, u16) {
    let width = messages_width(lines)
        .min(grid_w.saturating_sub(2))
        .max(1)
        .saturating_add(2);
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .max(1)
        .saturating_add(2);
    (width, height)
}

/// The toast stack: one framed box per visible notice, oldest at the top,
/// plus the box a dismissal is still carrying off to the right.
///
/// The stack is already in its final state here -- the departed notice is
/// out of `Messages::entries` -- so the motion decides only where the boxes
/// are drawn. The departing box keeps the row it held; the boxes at and
/// below the slot it vacated sit `y_shift` rows lower than the home they
/// are settling into, and the ones above it do not move at all. `y_shift`
/// runs from the departing box's full height down to nothing over the
/// motion, which is the slide up, and the same eased fraction drives that
/// box's own `x_offset` -- one motion, on one clock, not two that share a
/// duration.
///
/// The departing box is pushed last, so it composites over the stack
/// arriving underneath it instead of being cleared by it.
fn toast_layers(model: &Model, bounds: (u16, u16), offset: u16) -> Vec<Layer> {
    let (grid_w, _) = bounds;
    let stack = model.engine.messages.visible_toasts(model.toast_rows());
    let leaving = model.toast_motion.as_ref().map(|motion| {
        let (lines, slot) = motion.exiting();
        let (width, height) = toast_box(lines, grid_w);
        Leaving {
            lines: lines.to_vec(),
            slot: slot.min(stack.len()),
            x_offset: motion.cells_of(width),
            y_shift: height.saturating_sub(motion.cells_of(height)),
        }
    });
    if stack.is_empty() && leaving.is_none() {
        return Vec::new();
    }
    let vacated = leaving.as_ref().map_or(usize::MAX, |l| l.slot);
    let y_shift = leaving.as_ref().map_or(0, |l| l.y_shift);
    let mut layers = Vec::with_capacity(stack.len().saturating_add(1));
    let mut row: u16 = 0;
    let mut vacated_row: u16 = 0;
    for (i, lines) in stack.into_iter().enumerate() {
        let (_, height) = toast_box(&lines, grid_w);
        if i == vacated {
            vacated_row = row;
        }
        let slot = if i >= vacated { i.saturating_add(1) } else { i };
        let at = if i >= vacated {
            row.saturating_add(y_shift)
        } else {
            row
        };
        layers.push(toast_layer(lines, slot, 0, at, bounds, offset, model.caps));
        row = row.saturating_add(height);
    }
    if let Some(leaving) = leaving {
        let at = if leaving.slot < layers.len() {
            vacated_row
        } else {
            row
        };
        let layer = toast_layer(
            leaving.lines,
            leaving.slot,
            leaving.x_offset,
            at,
            bounds,
            offset,
            model.caps,
        );
        // a box that has travelled its own width is entirely past the right
        // edge; it leaves the stack rather than sitting in it as an empty
        // rect the paint shadow still has to pair against
        if layer.rect.width > 0 {
            layers.push(layer);
        }
    }
    layers
}

/// The departing box's presentation state for one frame of the motion.
struct Leaving {
    lines: Vec<Vec<Span>>,
    slot: usize,
    x_offset: u16,
    y_shift: u16,
}

/// One toast box as a [`Layer`]: right-anchored to the grid, shifted
/// `x_offset` cells further right while it is on its way out.
fn toast_layer(
    lines: Vec<Vec<Span>>,
    slot: usize,
    x_offset: u16,
    row: u16,
    bounds: (u16, u16),
    offset: u16,
    caps: TermCaps,
) -> Layer {
    let (grid_w, _) = bounds;
    let (width, height) = toast_box(&lines, grid_w);
    let col = grid_w.saturating_sub(width).saturating_add(x_offset);
    overlay_layer(
        Rect::new(row, col, width, height),
        bounds,
        offset,
        LayerKind::Toast {
            lines,
            slot,
            x_offset,
        },
        caps,
    )
}

/// The widest popup menu item's [`PmItem::display_text`], in terminal
/// display cells (see [`messages_width`] for why cells rather than chars).
fn popupmenu_width(items: &[PmItem]) -> u16 {
    items
        .iter()
        .map(|i| i.display_text().width())
        .max()
        .and_then(|w| u16::try_from(w).ok())
        .unwrap_or(u16::MAX)
}

/// The command palette's placement: a wide box pinned near the top of the
/// terminal, wide enough to hold a typed command plus its completion rows
/// without wrapping, short enough to leave the buffer grid visible under
/// it.
///
/// One function rather than a literal at each call site, because [`render`]
/// (which paints the box) and [`cursor_spec`] (which places the caret
/// inside it) both have to resolve to the exact same rect; two separately
/// written `OverlayBox::new(..)` calls are two chances for that number to
/// drift apart the next time either one is edited.
fn palette_box() -> OverlayBox {
    OverlayBox::new(70, 50)
}

/// [`palette_box`] resolved against the terminal, then shifted down by
/// `offset` (the reserved chrome rows): resolving against a terminal
/// already shrunk by `offset` before centering, rather than centering
/// against the full terminal and adding `offset` on top, keeps the box
/// clear of the tabline row instead of centering through it. [`render`]
/// (which paints the box) and [`palette_cursor`] (which places the caret
/// inside it) both resolve through this one function, so the two can never
/// disagree about where the box actually is.
fn palette_rect(model: &Model, offset: u16) -> Rect {
    let rect = palette_box().rect(model.term_width, model.term_height.saturating_sub(offset));
    Rect::new(
        rect.row.saturating_add(offset),
        rect.col,
        rect.width,
        rect.height,
    )
}

/// Where a cmdline-sourced popupmenu (`pm.grid < 0`) paints when the
/// palette is off and nothing else already absorbed its rows.
///
/// Per the live wire capture
/// (`docs/palette-popupmenu-source-wire-capture.md`), `pm.row`/`pm.col` are
/// cmdline-relative, not grid coordinates: `row` counts lines below the
/// cmdline's own row, `col` counts cells into the typed content only (not
/// `firstc`/`prompt`). Routing them through [`overlay_layer`]'s grid-space
/// clamp-then-offset would misread them as an absolute grid position, which
/// is exactly the bug this fixes -- so this builds the terminal-absolute
/// rect directly instead. The raw (non-palette) `Cmdline` layer this menu
/// completes always paints on the grid's own last row, leaving no room to
/// grow downward, so a `row` that would run past the bottom of the terminal
/// flips to grow upward from the cmdline instead -- the same flip any popup
/// placement makes when its preferred side does not fit. `None` when the
/// popupmenu claims a cmdline source but no cmdline is actually open:
/// fabricating a position for that combination would show content nothing
/// else claims to be showing.
fn cmdline_popupmenu_layer(
    model: &Model,
    grid_h: u16,
    offset: u16,
    cmdline: Option<&CmdlineState>,
    pm: &PopupmenuState,
) -> Option<Layer> {
    let cmdline = cmdline?;
    let prefix_cols = cmdline.firstc.chars().count() + cmdline.prompt.chars().count();
    let cmdline_row = grid_h.saturating_sub(1).saturating_add(offset);
    let width = popupmenu_width(&pm.items).min(model.term_width).max(1);
    let height = u16::try_from(pm.items.len()).unwrap_or(u16::MAX).max(1);
    let row_offset = saturate_u16(pm.row);
    let below = cmdline_row.saturating_add(1).saturating_add(row_offset);
    let row = if below.saturating_add(height) <= model.term_height {
        below
    } else {
        cmdline_row
            .saturating_sub(height)
            .saturating_sub(row_offset)
    };
    let col = u16::try_from(prefix_cols)
        .unwrap_or(u16::MAX)
        .saturating_add(saturate_u16(pm.col));
    let rect = Rect::new(row, col, width, height).clamp_to(model.term_width, model.term_height);
    Some(Layer::new(
        rect,
        LayerKind::Popupmenu(pm.clone()),
        model.caps,
    ))
}

/// Builds one grid-space overlay [`Layer`]: `rect` is first clamped to
/// `bounds` (the grid's own coordinate space, which is what wire-derived
/// positions like a popup menu's `(row, col)` are expressed in), then
/// translated down by `offset` (the reserved chrome rows) to land in the
/// terminal's own coordinate space. Clamping before translating means a
/// hostile or stale position/size from wire-derived state can never place a
/// layer outside the current grid, regardless of whether chrome is
/// currently reserved.
fn overlay_layer(
    rect: Rect,
    bounds: (u16, u16),
    offset: u16,
    kind: LayerKind,
    caps: TermCaps,
) -> Layer {
    let clamped = rect.clamp_to(bounds.0, bounds.1);
    Layer::new(
        Rect {
            row: clamped.row.saturating_add(offset),
            ..clamped
        },
        kind,
        caps,
    )
}

/// Where both frame builders place the [`LayerKind::Speculated`] layer:
/// immediately above the [`LayerKind::EngineGrid`] layer that is always this
/// list's first entry.
///
/// A constant rather than a rule each builder re-derives, because
/// [`cache::SurfaceCache`] inserts this layer into an already-built frame and
/// a position that disagreed with [`render`]'s by one would be a z-order the
/// equivalence guard reports as a whole-frame divergence. Both sides insert
/// *at* this index, so neither can drift from the other by growing the
/// layers around it; the engine grid layer every frame opens with is what
/// keeps the index inside the list for `Vec::insert`.
pub(crate) const SPECULATED_LAYER_INDEX: usize = 1;

/// The [`LayerKind::Speculated`] layer for whatever `model` currently has
/// pending, or `None` when nothing is pending, or when every pending
/// prediction names a cell outside the live `grid`.
///
/// Off-grid predictions are dropped here, one by one, rather than the layer
/// being clamped as a whole: predictions on a wrapped line are a mix of
/// cells the grid has and cells it does not, and clamping the rect around
/// all of them would drag the survivors' glyphs to the grid edge (see
/// [`PredictedCell`]).
fn speculated_layer(model: &Model, grid: (u16, u16), offset: u16) -> Option<Layer> {
    let (grid_w, grid_h) = grid;
    let cells: Vec<PredictedCell> = model
        .speculate
        .pending()
        .iter()
        .copied()
        .filter(|cell| cell.row < grid_h && cell.col < grid_w)
        .collect();
    let top = cells.iter().map(|cell| cell.row).min()?;
    let bottom = cells.iter().map(|cell| cell.row).max()?;
    let left = cells.iter().map(|cell| cell.col).min()?;
    let right = cells.iter().map(|cell| cell.col).max()?;
    Some(Layer::new(
        Rect::new(
            top.saturating_add(offset),
            left,
            right.saturating_sub(left).saturating_add(1),
            bottom.saturating_sub(top).saturating_add(1),
        ),
        LayerKind::Speculated(cells),
        model.caps,
    ))
}

/// The framed [`Layer`] for one open native overlay, or `None` for an
/// overlay carrying no paintable content.
///
/// The rect comes from [`Model::overlay_rect`], the same resolution
/// `update()`'s mouse hit-test routes a click through, so paint and routing
/// cannot disagree about where an overlay is. The border charset comes from
/// the terminal's own probed capabilities, resolved once here rather than
/// per painter.
fn native_layer(model: &Model, open: &Overlay) -> Option<Layer> {
    let cells = model.overlay_rect(open);
    let kind = layer_kind(model, &open.kind, cells.height, cells.width)?;
    Some(Layer::new(
        Rect::new(cells.row, cells.col, cells.width, cells.height),
        kind,
        model.caps,
    ))
}

/// The paint-facing layer content for one overlay's feature state, or
/// `None` for an overlay with nothing to paint.
///
/// `OverlayKind` is `#[non_exhaustive]` and defined in another crate, so
/// the wildcard arm is mandatory rather than a choice -- cross-crate
/// exhaustiveness checking is not available here, and a feature variant
/// reaching this build without a mapping paints nothing rather than failing
/// to compile.
///
/// Takes `model` (unused by every other arm) because [`OverlayKind::Ai`] is
/// a unit marker, not a payload: the session state it renders lives in
/// [`Model::ai_panel`] instead, so that arm is the one place this function
/// reaches past the overlay stack for what to paint.
fn layer_kind(model: &Model, kind: &OverlayKind, height: u16, width: u16) -> Option<LayerKind> {
    match kind {
        OverlayKind::Prompt(state) => Some(LayerKind::Prompt(state.view())),
        OverlayKind::Picker(state) => Some(LayerKind::Picker(state.view())),
        OverlayKind::Tree(state) => Some(LayerKind::Tree(state.view())),
        // a message-history browse is presented the same way the palette
        // itself is: rows in a centered box, no fields of its own the
        // palette's LayerKind doesn't already carry
        OverlayKind::MessageHistory(state) => Some(LayerKind::Palette(state.view())),
        // a titled box with a message line and a fixed answer list is what
        // the confirm prompt's own layer already draws; the busy modal
        // carries no field that shape does not
        OverlayKind::EngineBusy(state) => Some(LayerKind::Prompt(state.view())),
        // The panel's own resolved size, not the terminal's: the panel
        // derives its transcript window from the height, and a window sized
        // to the whole terminal would page past rows the panel never
        // showed. The width is what its composer wraps at, and one taken
        // from the terminal would break the prompt past the frame's edge.
        OverlayKind::Ai => Some(LayerKind::Ai(
            model
                .ai_panel()
                .view(usize::from(height), usize::from(width)),
        )),
        _ => None,
    }
}

/// The active mode's cursor shape, decoded from the last `mode_info_set`.
/// Falls back to [`CursorShape::Block`] before the first `mode_info_set`
/// arrives, for an unrecognized shape string (matching nvim's own
/// fallback), or whenever `cursor_style_enabled` is `false` -- nvim's own
/// signal that per-mode cursor styling must not be applied at all, not
/// merely a "no shape decoded yet" state.
fn shape_from_mode(model: &Model) -> CursorShape {
    if !model.engine.mode.cursor_style_enabled {
        return CursorShape::Block;
    }
    model
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
        })
}

/// The command line's cursor column, in grid space: `firstc` plus `prompt`
/// plus `pos` characters into the typed content (nvim's `pos` counts into
/// the content only, not the `firstc`/`prompt` prefix rendered before it).
/// Live-verified against a real nvim's `cmdline_show` traffic for
/// `:call input("name: ")`, whose `pos` came back counting from the start
/// of the typed answer, not from the start of the `prompt` label.
fn cmdline_cursor_col(cmdline: &CmdlineState) -> u16 {
    let prefix_len = cmdline.firstc.chars().count() + cmdline.prompt.chars().count();
    let pos = usize::try_from(cmdline.pos).unwrap_or(usize::MAX);
    u16::try_from(prefix_len.saturating_add(pos)).unwrap_or(u16::MAX)
}

/// The real terminal cursor: position plus shape, offset by `offset`
/// (reserved chrome rows) to land in the terminal's own coordinate space.
/// While the command line is open, the cursor tracks its `pos` on the
/// bottom grid row instead of the grid's own cursor (matching the
/// cmdheight=0 floating UX external UIs give: the engine grid's cursor
/// position is stale while the command line owns input). `None` when the
/// grid has no cells to place it in (a freshly started `Model` before the
/// first resize).
///
/// Takes the frame's own `layers` so the agent panel's caret lands on the
/// row this frame painted its composer on, rather than on a second resolve
/// of the panel's rect and a second render of its view.
fn cursor_spec(model: &Model, offset: u16, layers: &[Layer]) -> Option<CursorSpec> {
    let (width, height) = model.engine.grid().size();
    if width == 0 || height == 0 {
        return None;
    }
    // ahead of every branch below, because it is the only one that answers
    // where the keys are actually going: with an overlay holding them, the
    // engine's own grid cursor -- and a cmdline left open behind that
    // overlay -- is stale state about an editor nobody is typing into.
    if let Some(spec) = overlay_cursor(model, layers) {
        return Some(spec);
    }
    let shape = shape_from_mode(model);
    // each branch below already resolves in, and adds `offset` in, exactly
    // the coordinate space its own source rect came from -- a shared tail
    // add here double-counts `offset` for the palette branch, whose rect
    // (via `palette_rect`) is offset-inclusive already.
    let (row, col) = if let Some(cmdline) = &model.engine.cmdline {
        if model.palette_enabled {
            palette_cursor(model, offset, cmdline)
        } else {
            let col = cmdline_cursor_col(cmdline).min(width.saturating_sub(1));
            (height.saturating_sub(1).saturating_add(offset), col)
        }
    } else {
        let (row, col) = model.engine.grid().cursor();
        (
            row.saturating_add(offset),
            speculated_col(model, (width, height), row, col),
        )
    };
    Some(CursorSpec { row, col, shape })
}

/// The column the cursor is shown at while predictions are painted on its
/// own row: one past the rightmost of them, or the engine's own column when
/// there are none.
///
/// Display-only and stateless, exactly as the predicted glyphs are, and
/// derived from the same pending list on every frame -- so the engine's
/// cursor is authoritative again the instant that list empties, whether it
/// emptied by reconciliation, by invalidation or by the age bound.
///
/// Without it the feature would hide half the latency it was built to hide:
/// the eye tracks the caret, and a caret parked on the first glyph of a
/// burst -- inverted under it, with a block cursor -- advertises the round
/// trip the glyphs to its right have already skipped, then jumps by the
/// whole burst when the redraw lands.
///
/// Predictions the [`LayerKind::Speculated`] layer would drop as off-grid
/// are skipped here too, so the caret never advances past a glyph nothing
/// painted; a caret that would land past the last column stays on it, since
/// a cursor outside the grid is not a wrong guess a redraw corrects but a
/// position no terminal has.
fn speculated_col(model: &Model, grid: (u16, u16), row: u16, col: u16) -> u16 {
    let (grid_w, grid_h) = grid;
    model
        .speculate
        .pending()
        .iter()
        .filter(|cell| cell.row == row && cell.row < grid_h && cell.col < grid_w)
        .map(|cell| cell.col)
        .filter(|predicted| *predicted >= col)
        .max()
        .map_or(col, |last| {
            last.saturating_add(1).min(grid_w.saturating_sub(1))
        })
}

/// The palette's own cursor position: past its box's frame and pad (see
/// [`overlay::interior_origin`], the same arithmetic [`overlay::rows`]
/// frames every layer's content with), past the fixed `"> "` prefix the
/// palette's header row always draws before the query, then
/// [`cmdline_cursor_col`] cells further in -- the same column the raw
/// bottom-line cmdline placed its cursor at, measured from the palette
/// box's own origin instead of the grid's. Resolves through [`palette_rect`],
/// the same call [`render`] paints the box through, so the caret can never
/// land somewhere the box itself was not drawn.
fn palette_cursor(model: &Model, offset: u16, cmdline: &CmdlineState) -> (u16, u16) {
    let rect = palette_rect(model, offset);
    let (row_off, col_off) = overlay::interior_origin(rect.width, rect.height);
    let prefix_cols =
        u16::try_from(format!("{} ", overlay::PROMPT_MARK).chars().count()).unwrap_or(2);
    let row = rect.row.saturating_add(row_off);
    let col = rect
        .col
        .saturating_add(col_off)
        .saturating_add(prefix_cols)
        .saturating_add(cmdline_cursor_col(cmdline))
        .min(rect.col.saturating_add(rect.width).saturating_sub(1));
    (row, col)
}

/// The shape a caret in a view-owned text field takes: a bar, the one nvim
/// itself asks for in insert mode, at the same quarter-cell width its own
/// `ver25` default uses.
///
/// Not read from the engine's active mode, unlike [`shape_from_mode`]: these
/// fields are typed into whatever mode the buffer beneath them happens to be
/// left in, and inheriting that mode's shape paints a normal-mode block over
/// an insertion point.
///
/// It does honour `cursor_style_enabled` -- nvim's own signal that per-mode
/// cursor styling must not be applied at all, which is what a user gets from
/// `set guicursor=`. A shape that flipped to a bar on entering the panel and
/// back on leaving it is precisely the per-mode styling that user turned
/// off, so with the signal clear this falls back to the same plain block
/// [`shape_from_mode`] gives them everywhere else.
fn text_field_shape(model: &Model) -> CursorShape {
    if model.engine.mode.cursor_style_enabled {
        CursorShape::Vertical(25)
    } else {
        CursorShape::Block
    }
}

/// The caret of whichever native overlay owns the keyboard, or `None` while
/// the engine owns it.
///
/// One ownership question for every arm: [`Model::focused_overlay`] is what
/// `update::route_key` routes by (`Model::focus` names the same overlay), so
/// the caret cannot land in a surface the keys do not. Asking it once here
/// is also why a busy modal -- which takes no keys of its own -- cannot pull
/// the caret off the prompt underneath it, the way a separate "is the top
/// overlay a prompt" test did.
///
/// `None` for a focused overlay with no typed field of its own: the tree's
/// rows and the message-history browse are selection surfaces whose keys
/// move a selection each paints itself, and there is no insertion point in
/// either for a caret to name.
fn overlay_cursor(model: &Model, layers: &[Layer]) -> Option<CursorSpec> {
    let overlay = model.focused_overlay()?;
    match &overlay.kind {
        OverlayKind::Ai => ai_cursor(model, layers),
        OverlayKind::Prompt(state) => {
            let (row, col) = prompt_cursor(model, overlay, state);
            Some(CursorSpec {
                row,
                col,
                shape: shape_from_mode(model),
            })
        }
        OverlayKind::Picker(state) => {
            let (row, col) = query_cursor(model.overlay_rect(overlay), state.query());
            Some(CursorSpec {
                row,
                col,
                shape: text_field_shape(model),
            })
        }
        _ => None,
    }
}

/// Where the caret stands in a `{PROMPT_MARK} {query}` header row -- the
/// picker's typed query -- given the overlay's own rect: past the mark and
/// past what has been typed, on the first interior row, which is where
/// [`overlay::picker_body`] puts that header.
///
/// The picker is not a selection-only surface: its query is a text field
/// that grows and backspaces on every key, and a caret left on the buffer
/// while it does is the same defect the panel's composer had.
fn query_cursor(rect: OverlayRect, query: &str) -> (u16, u16) {
    let (row_off, col_off) = overlay::interior_origin(rect.width, rect.height);
    let prefix_cols =
        u16::try_from(format!("{} ", overlay::PROMPT_MARK).chars().count()).unwrap_or(2);
    let typed = u16::try_from(query.width()).unwrap_or(u16::MAX);
    let col = rect
        .col
        .saturating_add(col_off)
        .saturating_add(prefix_cols)
        .saturating_add(typed)
        .min(rect.col.saturating_add(rect.width).saturating_sub(1));
    (rect.row.saturating_add(row_off), col)
}

/// The agent panel's own caret, in terminal space, on every frame the panel
/// is the thing keys reach.
///
/// Where inside the panel it lands -- the composer's insertion point, or the
/// digit answering a pending question -- is [`overlay::ai_caret`]'s call,
/// made against the painted view. The shape is decided here from that same
/// view: a bar only where something is actually inserted, so the panel never
/// advertises a text field while a question is standing in front of it.
///
/// Reads the panel's rows off the frame's own `layers` rather than
/// re-rendering the view, which is sound because a frame with any overlay
/// open is never served from `SurfaceCache`'s reuse path (it requires
/// `overlays().is_empty()`), so these layers are always this frame's.
///
/// Adds no `offset` of its own: the layer's rect came from
/// [`Model::overlay_rect`], which has already put the chrome rows into it.
fn ai_cursor(model: &Model, layers: &[Layer]) -> Option<CursorSpec> {
    let (rect, view) = layers.iter().find_map(|layer| match &layer.kind {
        LayerKind::Ai(view) => Some((layer.rect, view)),
        _ => None,
    })?;
    let (row, col) = overlay::ai_caret(view, rect.width, rect.height)?;
    let shape = if view.pending_permission.is_empty() {
        text_field_shape(model)
    } else {
        CursorShape::Block
    };
    Some(CursorSpec {
        row: rect.row.saturating_add(row),
        col: rect.col.saturating_add(col),
        shape,
    })
}

/// The confirm-prompt's own cursor position: on the second header row
/// [`overlay::prompt_body`] always draws before its choice list (the
/// message line first, then `"> "` plus the answer typed so far), past the
/// end of what has been typed -- correct because [`PromptState::accepts`]
/// never permits mid-string editing, only append/backspace/submit/cancel,
/// so the caret is always at the end of `input`. Resolves through
/// [`Model::overlay_rect`], the same rect [`native_layer`] already paints
/// the Prompt overlay's own box at (offset-unaware, like every other native
/// overlay today) -- matching that painted position takes priority over
/// also closing the box's own separate chrome-offset gap, which is not this
/// fix's scope.
///
/// Takes the overlay its caller already resolved rather than looking one up
/// itself: the stack's top and the overlay holding the keyboard are not the
/// same overlay once a busy modal is standing over the prompt.
fn prompt_cursor(model: &Model, overlay: &Overlay, state: &PromptState) -> (u16, u16) {
    let view = state.view();
    let rect = model.overlay_rect(overlay);
    let (row_off, col_off) = overlay::interior_origin(rect.width, rect.height);
    let prefix_cols =
        u16::try_from(format!("{} ", overlay::PROMPT_MARK).chars().count()).unwrap_or(2);
    let input_len = u16::try_from(view.input.chars().count()).unwrap_or(u16::MAX);
    let row = rect.row.saturating_add(row_off).saturating_add(1);
    let col = rect
        .col
        .saturating_add(col_off)
        .saturating_add(prefix_cols)
        .saturating_add(input_len)
        .min(rect.col.saturating_add(rect.width).saturating_sub(1));
    (row, col)
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
        model.engine.apply_grid(GridOp::Resize { width, height });
        // past the startup window: a foreign message is parked rather than
        // stacked until it closes (`view_core::native::toast::StartupHold`),
        // and every test here is about where a toast paints rather than
        // about when one is shown
        let _ = model
            .engine
            .messages
            .resolve_startup_hold(view_core::native::toast::HoldOutcome::Release);
        model
    }

    /// Every toast layer in the frame, in paint order, as
    /// `(slot, x_offset, row, texts)` -- the whole of what one frame of the
    /// stack says about where its boxes are.
    fn toasts(surface: &Surface) -> Vec<(usize, u16, u16, Vec<String>)> {
        surface
            .layers
            .iter()
            .filter_map(|layer| match &layer.kind {
                LayerKind::Toast {
                    lines,
                    slot,
                    x_offset,
                } => Some((
                    *slot,
                    *x_offset,
                    layer.rect.row,
                    lines
                        .iter()
                        .map(|spans| spans.iter().map(|s| s.text.as_str()).collect())
                        .collect(),
                )),
                _ => None,
            })
            .collect()
    }

    /// What each toast box in the frame reads, top to bottom.
    fn toast_texts(surface: &Surface) -> Vec<Vec<String>> {
        toasts(surface)
            .into_iter()
            .map(|(_, _, _, texts)| texts)
            .collect()
    }

    /// A model on a terminal that interpolates, holding three notices: the
    /// stack the motion tests dismiss the top of.
    fn model_with_three_toasts(tier: view_core::model::Tier) -> Model {
        let mut model = model_with_grid(20, 12);
        model.caps.tier = tier;
        for text in ["first", "second", "third"] {
            apply(
                &mut model,
                UiEvent::MsgShow {
                    kind: "echomsg".into(),
                    content: vec![(0, text.into())],
                    replace_last: false,
                },
            );
        }
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

    /// A live confirm-class prompt paints as its own layer, over the grid,
    /// carrying the question and the choices parsed off the paired
    /// cmdline_show -- the same event pair captured live in
    /// docs/prompt-overlay-wire-capture.md.
    #[test]
    fn a_confirm_prompt_overlay_paints_a_prompt_layer_with_its_choices() {
        let mut model = model_with_grid(40, 12);
        model.term_width = 40;
        model.term_height = 12;

        apply(
            &mut model,
            UiEvent::MsgShow {
                kind: "confirm".into(),
                content: vec![(0, "Save changes?".into())],
                replace_last: false,
            },
        );
        apply(
            &mut model,
            UiEvent::CmdlineShow {
                content: vec![],
                pos: 0,
                firstc: String::new(),
                prompt: "[Y]es, (N)o: ".into(),
                indent: 0,
                level: 1,
            },
        );

        let surface = render(&model);
        let prompt_layer = surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::Prompt(_)))
            .expect("a prompt overlay is open, so render() must contribute its layer");
        let LayerKind::Prompt(view) = &prompt_layer.kind else {
            unreachable!()
        };
        assert_eq!(view.message, "Save changes?");
        assert_eq!(view.choices, vec!["Yes".to_string(), "No".to_string()]);
    }

    /// An open tree sidebar paints its own layer, carrying the rows
    /// `TreeState::view` built -- the `layer_kind` mapping this test
    /// exists to prove, since a tree overlay with no entry here would
    /// compile clean (`OverlayKind` is `#[non_exhaustive]`, so the mapping
    /// match's wildcard arm swallows an unmapped variant silently) and
    /// simply never paint.
    #[test]
    fn an_open_tree_overlay_paints_a_tree_layer_with_its_title() {
        use view_core::native::geometry::{Anchor, OverlayBox};
        use view_core::native::tree::TreeState;

        let mut model = model_with_grid(40, 12);
        model.term_width = 40;
        model.term_height = 12;
        let tree = TreeState::open(std::path::PathBuf::from("/tmp/example"));
        model.push_overlay(
            OverlayBox::new(30, 100).with_anchor(Anchor::Left),
            OverlayKind::Tree(tree),
        );

        let surface = render(&model);
        let tree_layer = surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::Tree(_)))
            .expect("a tree overlay is open, so render() must contribute its layer");
        let LayerKind::Tree(view) = &tree_layer.kind else {
            unreachable!()
        };
        assert!(!view.title.is_empty(), "a tree layer must carry a title");
    }

    /// The busy modal paints, and paints its choices -- the same silent-
    /// wildcard hazard the tree test above exists for, applied to the
    /// overlay a user only ever sees when something has already gone wrong.
    #[test]
    fn an_open_engine_busy_overlay_paints_a_prompt_layer_carrying_its_choices() {
        use view_core::native::geometry::OverlayBox;
        use view_core::native::supervision::{EngineBusyState, SinceStamp, WedgeKind};

        let mut model = model_with_grid(60, 20);
        model.term_width = 60;
        model.term_height = 20;
        model.push_overlay(
            OverlayBox::new(60, 30),
            OverlayKind::EngineBusy(EngineBusyState::new(
                WedgeKind::Dead,
                SinceStamp::new(std::time::Duration::from_secs(4)),
            )),
        );

        let surface = render(&model);
        let layer = surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::Prompt(_)))
            .expect("an open busy modal must contribute a layer");
        let LayerKind::Prompt(view) = &layer.kind else {
            unreachable!()
        };
        assert_eq!(view.title, WedgeKind::Dead.title());
        assert!(
            view.message.contains(WedgeKind::Dead.notice()),
            "the modal must say what is wrong: {}",
            view.message
        );
        assert_eq!(
            view.choices,
            vec![
                "[<F5>] Restart".to_string(),
                "[<C-q>] Quit view".to_string()
            ],
            "the painted rows must be the ones the wedge actually offers"
        );
    }

    /// The rows a user reads are the keys they can press, and for the
    /// interrupt that key is the one they would already have reached for.
    #[test]
    fn a_busy_modal_paints_the_interrupt_under_the_key_that_sends_it() {
        use view_core::native::geometry::OverlayBox;
        use view_core::native::supervision::{EngineBusyState, SinceStamp, WedgeKind};

        let mut model = model_with_grid(60, 20);
        model.term_width = 60;
        model.term_height = 20;
        model.push_overlay(
            OverlayBox::new(60, 30),
            OverlayKind::EngineBusy(EngineBusyState::new(
                WedgeKind::ReadSide,
                SinceStamp::new(std::time::Duration::from_secs(31)),
            )),
        );

        let surface = render(&model);
        let layer = surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::Prompt(_)))
            .expect("an open busy modal must contribute a layer");
        let LayerKind::Prompt(view) = &layer.kind else {
            unreachable!()
        };
        assert_eq!(
            view.choices,
            vec![
                "[<C-c>] Interrupt".to_string(),
                "[<F5>] Restart".to_string(),
                "[<Esc>] Dismiss".to_string()
            ]
        );
    }

    #[test]
    fn engine_only_model_renders_one_grid_layer_with_block_cursor_at_grid_cursor() {
        let mut model = model_with_grid(10, 5);
        model
            .engine
            .apply_grid(GridOp::CursorGoto { row: 2, col: 4 });

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
    fn cmdline_cursor_col_clamps_to_the_last_grid_column_when_pos_overruns_grid_width() {
        let mut model = model_with_grid(10, 8);
        apply(
            &mut model,
            UiEvent::CmdlineShow {
                content: vec![(0, "a very long typed command line".to_string())],
                pos: 30,
                firstc: ":".to_string(),
                prompt: String::new(),
                indent: 0,
                level: 1,
            },
        );

        let surface = render(&model);

        assert_eq!(
            surface.cursor,
            Some(CursorSpec {
                row: 7,
                col: 9,
                shape: CursorShape::Block,
            }),
            "pos=30 into a 10-wide grid must clamp to the last column (9), not overrun it"
        );
    }

    #[test]
    fn insert_mode_mode_info_yields_vertical_cursor_shape() {
        let mut model = model_with_grid(10, 5);
        model.engine.mode.cursor_style_enabled = true;
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
        model.engine.mode.cursor_style_enabled = true;
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
        model.engine.mode.cursor_style_enabled = true;
        model.engine.mode.modes = vec![ModeInfo {
            cursor_shape: "unknown-shape".to_string(),
            ..Default::default()
        }];
        model.engine.mode.current_idx = 0;

        let surface = render(&model);

        assert_eq!(surface.cursor.map(|c| c.shape), Some(CursorShape::Block));
    }

    #[test]
    fn cursor_style_disabled_forces_block_regardless_of_mode_info() {
        let mut model = model_with_grid(10, 5);
        // cursor_style_enabled left false (the struct default): nvim's own
        // contract for that flag is "do not restyle the cursor per mode at
        // all", so a non-block mode_info must still be ignored
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
            Some(CursorShape::Block),
            "cursor_style_enabled=false must force Block even with a vertical mode_info present"
        );
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
                current: view_core::events::TabHandle(1),
                tabs: vec![
                    view_core::events::TabEntry {
                        tab: view_core::events::TabHandle(1),
                        name: "a".into(),
                    },
                    view_core::events::TabEntry {
                        tab: view_core::events::TabHandle(2),
                        name: "b".into(),
                    },
                ],
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

    #[test]
    fn single_tab_renders_no_tabline_layer_and_reserves_no_row() {
        let mut model = model_with_grid(10, 5);
        apply(
            &mut model,
            UiEvent::TablineUpdate {
                current: view_core::events::TabHandle(1),
                tabs: vec![view_core::events::TabEntry {
                    tab: view_core::events::TabHandle(1),
                    name: "a".into(),
                }],
            },
        );

        let surface = render(&model);

        assert!(!surface
            .layers
            .iter()
            .any(|l| matches!(l.kind, LayerKind::Tabline(_))));
        assert_eq!(surface.layers[0].rect.row, 0, "no chrome, no offset");
    }

    #[test]
    fn more_than_one_tab_offsets_the_grid_and_cursor_below_the_reserved_row() {
        let mut model = model_with_grid(10, 5);
        model
            .engine
            .apply_grid(GridOp::CursorGoto { row: 2, col: 4 });
        apply(
            &mut model,
            UiEvent::TablineUpdate {
                current: view_core::events::TabHandle(1),
                tabs: vec![
                    view_core::events::TabEntry {
                        tab: view_core::events::TabHandle(1),
                        name: "a".into(),
                    },
                    view_core::events::TabEntry {
                        tab: view_core::events::TabHandle(2),
                        name: "b".into(),
                    },
                ],
            },
        );

        let surface = render(&model);

        let grid_layer = surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::EngineGrid))
            .expect("grid layer present");
        assert_eq!(grid_layer.rect.row, 1, "grid offset below the tabline row");
        let tabline_layer = surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::Tabline(_)))
            .expect("tabline layer present");
        assert_eq!(tabline_layer.rect.row, 0);
        assert_eq!(
            surface.cursor.map(|c| (c.row, c.col)),
            Some((3, 4)),
            "cursor offset by the same reserved row as the grid"
        );
    }

    #[test]
    fn cmdline_cursor_tracks_pos_past_firstc_on_the_grids_bottom_row() {
        let mut model = model_with_grid(20, 8);
        apply(
            &mut model,
            UiEvent::CmdlineShow {
                content: vec![(0, "e".to_string()), (0, "cho".to_string())],
                pos: 4,
                firstc: ":".to_string(),
                prompt: String::new(),
                indent: 0,
                level: 1,
            },
        );

        let surface = render(&model);

        assert_eq!(
            surface.cursor,
            Some(CursorSpec {
                row: 7,
                col: 5, // ":" (1) + pos (4)
                shape: CursorShape::Block,
            })
        );
    }

    #[test]
    fn messages_layer_is_sized_to_content_width_and_anchored_top_right() {
        let mut model = model_with_grid(20, 8);
        apply(
            &mut model,
            UiEvent::MsgShow {
                kind: "echomsg".into(),
                content: vec![(0, "hi".into())],
                replace_last: false,
            },
        );

        let surface = render(&model);

        let messages = surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::Toast { .. }))
            .expect("messages layer present");
        assert_eq!(messages.rect.row, 0);
        assert_eq!(
            messages.rect.width, 4,
            "sized to \"hi\" (2) plus the 2-cell border frame, not a fixed width"
        );
        assert_eq!(
            messages.rect.col, 16,
            "right-anchored: grid width (20) minus framed width (2 content + 2 border)"
        );
    }

    #[test]
    fn messages_layer_keeps_a_persistent_error_line_when_transient_lines_overflow_the_box() {
        // 6 rows, not 3: each notice is its own framed box costing its
        // line plus two frame rows, so a grid with room for two of the
        // three boxes is what puts the eviction priority this test targets
        // in play rather than the single-box floor
        let mut model = model_with_grid(20, 6);
        apply(
            &mut model,
            UiEvent::MsgShow {
                kind: "echoerr".into(),
                content: vec![(0, "an error".into())],
                replace_last: false,
            },
        );
        apply(
            &mut model,
            UiEvent::MsgShow {
                kind: "echomsg".into(),
                content: vec![(0, "old info".into())],
                replace_last: false,
            },
        );
        apply(
            &mut model,
            UiEvent::MsgShow {
                kind: "echomsg".into(),
                content: vec![(0, "new info".into())],
                replace_last: false,
            },
        );

        let surface = render(&model);

        assert_eq!(
            toast_texts(&surface),
            vec![vec!["an error".to_string()], vec!["new info".to_string()]],
            "the persistent error must survive the overflow; the oldest \
             transient box is evicted instead"
        );
    }

    /// The stack's whole motion, frame by frame: the notice that left is
    /// still on screen and travelling right, the ones under it are still
    /// below the rows they are settling into and rise into them, and
    /// nothing above the vacated slot moves. One clock drives both halves,
    /// which is what a frame-by-frame read is here to hold -- two motions
    /// that merely shared a duration would be free to disagree on any one
    /// frame.
    #[test]
    fn the_top_toast_exits_right_while_the_rest_slide_up_in_one_motion() {
        let mut model = model_with_three_toasts(view_core::model::Tier::Full);
        let first = model.engine.messages.entries[0].id();
        let _ = update(&mut model, Msg::ToastExpired { id: first });
        let mut frames = Vec::new();
        loop {
            frames.push(toasts(&render(&model)));
            if model.toast_motion.is_none() {
                break;
            }
            let _ = update(&mut model, Msg::AnimTick);
        }

        // the rows the survivors settle into, read off the frame the motion
        // arrived at rather than the one it left: what they rise toward is
        // where the stack now is, not where they stood behind the notice
        let settled: Vec<u16> = frames
            .last()
            .expect("a settled frame")
            .iter()
            .map(|(_, _, row, _)| *row)
            .collect();
        let moving: Vec<_> = frames
            .iter()
            .take_while(|f| f.len() == 3)
            .cloned()
            .collect();
        assert!(
            !moving.is_empty() && moving.len() < frames.len(),
            "the departing box travels for some frames and then leaves: \
             {frames:?}"
        );
        for (i, frame) in moving.iter().enumerate() {
            let (slot, _, _, texts) = frame[2].clone();
            assert_eq!(
                (slot, texts),
                (0, vec!["first".to_string()]),
                "the departing box holds the slot it is vacating, painted \
                 last so it composites over the stack arriving beneath it"
            );
            for (n, (slot, x_offset, row, _)) in frame[..2].iter().enumerate() {
                assert_eq!(
                    (*slot, *x_offset),
                    (n + 1, 0),
                    "only the departing box travels sideways"
                );
                assert!(
                    *row >= settled[n],
                    "frame {i} box {n} must never rise past its home row \
                     {}: {frame:?}",
                    settled[n]
                );
            }
        }
        let rights: Vec<u16> = moving.iter().map(|f| f[2].1).collect();
        let ups: Vec<u16> = moving.iter().map(|f| f[0].2).collect();
        assert!(
            rights.windows(2).all(|w| w[0] <= w[1]) && rights[0] < rights[rights.len() - 1],
            "the exit accelerates rightward and never reverses: {rights:?}"
        );
        assert!(
            ups.windows(2).all(|w| w[0] >= w[1]) && ups[0] > ups[ups.len() - 1],
            "the stack rises over the same frames and never sinks: {ups:?}"
        );
        assert_eq!(
            ups[0].saturating_sub(settled[0]),
            3,
            "the stack starts a full box below the home it settles into"
        );

        let last = frames.last().expect("a settled frame");
        assert_eq!(
            last.clone(),
            vec![
                (0, 0, settled[0], vec!["second".to_string()]),
                (1, 0, settled[1], vec!["third".to_string()]),
            ],
            "the motion settles on exactly the frame the stack was already in"
        );
    }

    /// Below the full tier the state-first frame is the only frame: the
    /// stack paints its final state the instant the notice leaves, with no
    /// departing box and nothing offset.
    /// The motion's first frame is the stack the user was already looking
    /// at (the spec's state-first rule): what it interpolates from must be
    /// what was on screen, never a notice the row budget was not showing.
    /// The oldest transient is both what the budget evicts first and what
    /// the slot queue arms first, so the two collide on any stack too tall
    /// for its terminal.
    #[test]
    fn a_notice_the_row_budget_never_showed_leaves_without_a_motion() {
        let mut model = model_with_grid(20, 9);
        model.caps.tier = view_core::model::Tier::Full;
        for text in ["first", "second", "third", "fourth"] {
            apply(
                &mut model,
                UiEvent::MsgShow {
                    kind: "echomsg".into(),
                    content: vec![(0, text.into())],
                    replace_last: false,
                },
            );
        }
        let before = toasts(&render(&model));
        assert_eq!(
            toast_texts(&render(&model)),
            vec![
                vec!["second".to_string()],
                vec!["third".to_string()],
                vec!["fourth".to_string()],
            ],
            "the budget shows three of the four boxes"
        );

        let first = model.engine.messages.entries[0].id();
        let _ = update(&mut model, Msg::ToastExpired { id: first });
        assert_eq!(
            model.toast_motion, None,
            "a notice that was never painted has nothing to slide out of"
        );
        assert_eq!(
            toasts(&render(&model)),
            before,
            "the stack the budget was already showing does not move"
        );
    }

    #[test]
    fn below_full_tier_the_stack_jumps_to_its_final_state() {
        let full = {
            let mut model = model_with_three_toasts(view_core::model::Tier::Full);
            let first = model.engine.messages.entries[0].id();
            let _ = update(&mut model, Msg::ToastExpired { id: first });
            while model.toast_motion.is_some() {
                let _ = update(&mut model, Msg::AnimTick);
            }
            toasts(&render(&model))
        };
        for tier in [
            view_core::model::Tier::Standard,
            view_core::model::Tier::Basic,
        ] {
            let mut model = model_with_three_toasts(tier);
            let first = model.engine.messages.entries[0].id();
            let _ = update(&mut model, Msg::ToastExpired { id: first });
            assert_eq!(model.toast_motion, None, "{tier:?} interpolates nothing");
            assert_eq!(
                toasts(&render(&model)),
                full,
                "{tier:?} paints on its first frame exactly what the full \
                 tier arrives at on its last"
            );
        }
    }

    #[test]
    fn popupmenu_is_sized_to_the_widest_items_display_text() {
        let mut model = model_with_grid(40, 10);
        apply(
            &mut model,
            UiEvent::PopupmenuShow {
                items: vec![
                    view_core::events::PmItem {
                        word: "foo".into(),
                        ..Default::default()
                    },
                    view_core::events::PmItem {
                        word: "foobarbaz".into(),
                        ..Default::default()
                    },
                ],
                selected: 0,
                row: 1,
                col: 2,
                grid: 0,
            },
        );

        let surface = render(&model);

        let popup = surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::Popupmenu(_)))
            .expect("popupmenu layer present");
        assert_eq!(
            popup.rect.width, 9,
            "sized to \"foobarbaz\", not a fixed 30"
        );
        assert_eq!(popup.rect.height, 2);
        assert_eq!(popup.rect.row, 1);
        assert_eq!(popup.rect.col, 2);
    }

    #[test]
    fn fresh_model_before_first_flush_renders_a_shell_layer_sized_to_the_terminal() {
        let mut model = Model::with_term_size(80, 24);
        // the opt-in only startup itself performs in production; every
        // other consumer's Model::new()/with_term_size defaults to
        // content_painted: true (ordinary steady state)
        model.content_painted = false;
        let surface = render(&model);

        let shell = surface
            .layers
            .iter()
            .find(|l| l.kind == LayerKind::Shell)
            .expect("Shell layer present before the first grid Flush");
        assert_eq!(
            shell.rect,
            Rect {
                row: 0,
                col: 0,
                width: 80,
                height: 24,
            },
            "Shell is sized to the real terminal, not the still-empty engine grid"
        );
    }

    #[test]
    fn shell_layer_is_dropped_for_good_once_content_painted_flips_true() {
        // content_painted: true is the default already; this test pins that
        // an ordinary model never renders Shell, symmetric with the
        // opted-in false case above
        let model = Model::with_term_size(80, 24);
        let surface = render(&model);

        assert!(
            !surface.layers.iter().any(|l| l.kind == LayerKind::Shell),
            "Shell must not render once real grid content has arrived"
        );
    }

    #[test]
    fn shell_layer_paints_underneath_a_native_toast_message() {
        // z-order: EngineGrid, then Shell, then Messages -- a pre-attach
        // overflow toast (recorded as a "native"-kind entry, see
        // EngineModel::record_native_notice) must land on top of the shell
        // placeholder, never be hidden underneath it. Built through
        // record_native_notice, the same choke point production code uses,
        // rather than a raw Messages::push -- there is no bypass entry
        // point left for a fixture to reach past classification through.
        let mut model = Model::with_term_size(80, 24);
        model.content_painted = false;
        let _ = model
            .engine
            .record_native_notice("dropped a key".to_string(), false);
        let surface = render(&model);

        let shell_idx = surface
            .layers
            .iter()
            .position(|l| l.kind == LayerKind::Shell)
            .expect("Shell layer present");
        let messages_idx = surface
            .layers
            .iter()
            .position(|l| matches!(l.kind, LayerKind::Toast { .. }))
            .expect("Messages layer present");
        assert!(
            shell_idx < messages_idx,
            "Shell must paint before (underneath) Messages in z-order"
        );
    }

    /// The agent panel is a right-pinned, full-height box covering the exact
    /// top-right corner the messages box pins itself to, so an order that put
    /// it on top hid every toast, completion menu and cmdline nvim raised
    /// while it was open -- the panel's own review and permission notices
    /// included, since those travel that same Messages layer. Both halves are
    /// pinned here: the transient surfaces above the panel, and the panel
    /// still above the grid and the statusline it is meant to cover, so an
    /// ordering that simply hoisted everything would fail too.
    #[test]
    fn nvims_transient_surfaces_paint_above_an_open_native_panel() {
        use view_core::native::geometry::{Anchor, OverlayBox};

        let mut model = model_with_grid(80, 23);
        model.term_width = 80;
        model.term_height = 24;
        model.statusline_enabled = true;
        model.push_overlay(
            OverlayBox::new(30, 100).with_anchor(Anchor::Right),
            OverlayKind::Ai,
        );
        apply(
            &mut model,
            UiEvent::MsgShow {
                kind: "emsg".into(),
                content: vec![(0, "E492: Not an editor command: bogus".into())],
                replace_last: false,
            },
        );
        apply(
            &mut model,
            UiEvent::CmdlineShow {
                content: vec![(0, "set nu".into())],
                pos: 6,
                firstc: ":".into(),
                prompt: String::new(),
                indent: 0,
                level: 1,
            },
        );
        apply(
            &mut model,
            UiEvent::PopupmenuShow {
                items: vec![view_core::events::PmItem::default()],
                selected: -1,
                row: 2,
                col: 3,
                grid: 1,
            },
        );

        let surface = render(&model);
        let position =
            |matches: fn(&LayerKind) -> bool| surface.layers.iter().position(|l| matches(&l.kind));
        let statusline = position(|k| matches!(k, LayerKind::Statusline(_)))
            .expect("the statusline feature is on, so its layer must be present");
        let panel = position(|k| matches!(k, LayerKind::Ai(_))).expect("the panel overlay is open");
        let cmdline = position(|k| matches!(k, LayerKind::Cmdline(_)))
            .expect("a cmdline is open and the palette is off");
        let messages = position(|k| matches!(k, LayerKind::Toast { .. }))
            .expect("an error message is showing");
        let popupmenu = position(|k| matches!(k, LayerKind::Popupmenu(_)))
            .expect("a buffer-anchored completion menu is showing");

        assert_eq!(surface.layers[0].kind, LayerKind::EngineGrid);
        assert!(
            statusline < panel,
            "the panel must still cover the persistent chrome it overlaps"
        );
        assert!(panel < cmdline, "an open cmdline must outrank the panel");
        assert!(panel < messages, "a toast must outrank the panel");
        assert!(
            panel < popupmenu,
            "a completion menu must outrank the panel"
        );
    }

    /// A model whose grid fills the terminal, with the agent panel open as
    /// the runtime opens it (right-pinned, full height) and `typed` already
    /// on its composer. The grid cursor is put somewhere no panel row can be
    /// mistaken for, so a caret that stayed with the engine is visible as
    /// exactly that.
    fn model_with_panel(typed: &str) -> Model {
        use view_core::native::geometry::{Anchor, OverlayBox};

        let mut model = model_with_grid(80, 24);
        model.term_width = 80;
        model.term_height = 24;
        model
            .engine
            .apply_grid(GridOp::CursorGoto { row: 3, col: 7 });
        model.push_overlay(
            OverlayBox::new(30, 100).with_anchor(Anchor::Right),
            OverlayKind::Ai,
        );
        model.ai_panel_mut().push_input(typed);
        // A real session has this set: `mode_info_set` reports it true for
        // any non-empty `guicursor`, which is nvim's default. The struct's
        // own `Default` is false, so leaving it would test the one
        // configuration (`set guicursor=`) where a bar is forbidden.
        model.engine.mode.cursor_style_enabled = true;
        model
    }

    /// The cell the caret is on, and the text of the row it is on, read off
    /// the panel layer the same frame painted -- so an assertion about the
    /// caret is an assertion about painted cells rather than about the
    /// arithmetic that produced them.
    fn caret_row_text(surface: &Surface) -> (CursorSpec, String) {
        let cursor = surface.cursor.expect("the frame must place a caret");
        let layer = surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::Ai(_)))
            .expect("the panel overlay is open");
        let rows = overlay::rows(
            layer.rect.width,
            layer.rect.height,
            &layer.kind,
            layer.borders.expect("a native overlay carries a charset"),
        );
        let row = usize::from(cursor.row - layer.rect.row);
        (cursor, overlay::line_text(&rows.lines[row]))
    }

    /// The composer is where the panel's keys land, so it is where the real
    /// terminal caret has to be: on the row the prompt was painted on, one
    /// cell past its last character, wearing the bar shape an insertion
    /// point wears rather than the buffer's own leftover mode shape.
    #[test]
    fn the_caret_stands_at_the_composers_insertion_point_while_the_panel_owns_input() {
        let mut model = model_with_panel("hello");
        model.ai_panel_mut().focused = true;

        let surface = render(&model);
        let (cursor, row) = caret_row_text(&surface);
        let layer = surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::Ai(_)))
            .expect("the panel overlay is open");
        let col = usize::from(cursor.col - layer.rect.col);

        assert!(
            row.contains("> hello"),
            "the caret's row must be the composer's own: {row:?}"
        );
        assert!(
            row.chars()
                .take(col)
                .collect::<String>()
                .ends_with("> hello"),
            "the caret must stand one cell past the last character typed: {row:?}"
        );
        assert_eq!(
            cursor.shape,
            CursorShape::Vertical(25),
            "an insertion point wears the bar nvim's own insert mode asks for, \
             never the block the buffer beneath was left in"
        );
    }

    /// The panel is non-modal: open is not entered, and every key reaches
    /// nvim while it is not. A caret on the composer there points at a field
    /// that takes nothing.
    #[test]
    fn an_open_but_unentered_panel_leaves_the_caret_on_the_engine_grid() {
        let model = model_with_panel("hello");

        assert_eq!(
            render(&model).cursor,
            Some(CursorSpec {
                row: 3,
                col: 7,
                shape: CursorShape::Block,
            }),
            "an un-entered panel must leave the caret where nvim's own cursor is"
        );
    }

    /// A pending question holds the panel's keys, and the composer refuses
    /// every printable while it stands, so the caret goes where the keyboard
    /// is actually being read: the digit that answers. A bar left on a
    /// composer that eats what is typed into it is worse than no caret.
    #[test]
    fn a_pending_permission_moves_the_caret_onto_the_digit_that_answers_it() {
        use view_core::native::ai_event::{PermissionOption, PermissionOptionKind};
        use view_core::native::ai_panel::PermissionPrompt;

        let mut model = model_with_panel("hello");
        model.ai_panel_mut().focused = true;
        model.ai_panel_mut().pending_permission = Some(PermissionPrompt::new(
            1,
            "call-1",
            Some("Write file".to_string()),
            Some("edit".to_string()),
            vec![PermissionOption {
                option_id: "allow".to_string(),
                name: "Allow".to_string(),
                kind: PermissionOptionKind::AllowOnce,
            }],
        ));

        let surface = render(&model);
        let (cursor, row) = caret_row_text(&surface);
        let layer = surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::Ai(_)))
            .expect("the panel overlay is open");
        let col = usize::from(cursor.col - layer.rect.col);

        assert!(
            row.contains("1 Allow"),
            "the caret's row must be the first option's: {row:?}"
        );
        assert_eq!(
            row.chars().nth(col),
            Some('1'),
            "the caret stands on the digit that answers, not past it: {row:?}"
        );
        assert_eq!(
            cursor.shape,
            CursorShape::Block,
            "nothing is being inserted, so nothing may advertise an insertion point"
        );
    }

    /// A question the agent raised with no options of its own still has to
    /// read a key, so the caret still belongs in the panel -- on the
    /// question, since there is no option row to stand on.
    #[test]
    fn a_pending_permission_with_no_options_keeps_the_caret_on_its_question() {
        use view_core::native::ai_panel::PermissionPrompt;

        let mut model = model_with_panel("hello");
        model.ai_panel_mut().focused = true;
        model.ai_panel_mut().pending_permission = Some(PermissionPrompt::new(
            1,
            "call-1",
            Some("Write file".to_string()),
            Some("edit".to_string()),
            Vec::new(),
        ));

        let surface = render(&model);
        let (cursor, row) = caret_row_text(&surface);
        let layer = surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::Ai(_)))
            .expect("the panel overlay is open");

        assert!(
            row.contains("Permission requested"),
            "the caret's row must be the question's own: {row:?}"
        );
        assert_eq!(
            cursor.col - layer.rect.col,
            overlay::interior_origin(layer.rect.width, layer.rect.height).1,
            "the question starts at the frame's first interior column"
        );
        assert_eq!(cursor.shape, CursorShape::Block);
    }

    /// A user who ran `set guicursor=` told nvim never to restyle their
    /// cursor per mode, and a bar that appeared on entering the panel is
    /// exactly that restyling. The caret still moves to the composer; only
    /// its shape defers.
    #[test]
    fn a_composer_caret_keeps_the_plain_block_when_cursor_styling_is_off() {
        let mut model = model_with_panel("hello");
        model.ai_panel_mut().focused = true;
        model.engine.mode.cursor_style_enabled = false;

        let (cursor, row) = caret_row_text(&render(&model));

        assert!(
            row.contains("> hello"),
            "the caret still belongs on the composer: {row:?}"
        );
        assert_eq!(
            cursor.shape,
            CursorShape::Block,
            "a user who turned per-mode cursor styling off must not be given a bar"
        );
    }

    /// The picker's query is a text field like the panel's composer -- every
    /// key appends to or backspaces it -- so the same caret rule holds: the
    /// insertion point is in the picker's own box, not on the buffer behind
    /// it.
    #[test]
    fn the_caret_stands_at_the_pickers_query_while_the_picker_owns_input() {
        use view_core::native::picker::{PickerState, Source};

        let mut model = model_with_grid(80, 24);
        model.term_width = 80;
        model.term_height = 24;
        model
            .engine
            .apply_grid(GridOp::CursorGoto { row: 3, col: 7 });
        model.engine.mode.cursor_style_enabled = true;
        let mut picker = PickerState::open(Source::Buffers);
        picker.edit_query("s");
        picker.edit_query("r");
        picker.edit_query("c");
        model.push_overlay(OverlayBox::new(60, 40), OverlayKind::Picker(picker));

        let surface = render(&model);
        let cursor = surface.cursor.expect("the picker must place a caret");
        let layer = surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::Picker(_)))
            .expect("the picker overlay is open");
        let rows = overlay::rows(
            layer.rect.width,
            layer.rect.height,
            &layer.kind,
            layer.borders.expect("a native overlay carries a charset"),
        );
        let row = overlay::line_text(&rows.lines[usize::from(cursor.row - layer.rect.row)]);
        let col = usize::from(cursor.col - layer.rect.col);

        assert!(
            row.contains("> src"),
            "the caret's row must be the query's own: {row:?}"
        );
        assert!(
            row.chars().take(col).collect::<String>().ends_with("> src"),
            "the caret must stand one cell past the last character typed: {row:?}"
        );
        assert_eq!(
            cursor.shape,
            CursorShape::Vertical(25),
            "a typed query is an insertion point, so it wears the bar"
        );
    }

    /// A review's keys are buffer-local nvim mappings on the file under
    /// review, so the caret belongs in that buffer -- the panel beside it is
    /// open and un-entered like any other.
    #[test]
    fn a_review_pending_in_the_buffer_leaves_the_caret_on_the_engine_grid() {
        use view_core::native::ai_panel::DiffReviewState;

        let mut model = model_with_panel("hello");
        model.ai_panel_mut().focused = false;
        model.ai_panel_mut().pending_diff = Some(DiffReviewState::new(
            1,
            std::path::PathBuf::from("src/main.rs"),
            1,
            Vec::new(),
        ));

        assert_eq!(
            render(&model).cursor.map(|c| (c.row, c.col)),
            Some((3, 7)),
            "review keys are the buffer's, so the caret stays on the buffer"
        );
    }

    /// The trust prompt the first `:View ai` raises, and every other
    /// focus-taking overlay above the panel: the keys are theirs while they
    /// are open, so the caret is theirs too.
    #[test]
    fn a_prompt_over_the_entered_panel_takes_the_caret_back_from_the_composer() {
        let mut model = model_with_panel("hello");
        model.ai_panel_mut().focused = true;
        apply(
            &mut model,
            UiEvent::MsgShow {
                kind: "confirm".into(),
                content: vec![(0, "Trust this project?".into())],
                replace_last: false,
            },
        );
        apply(
            &mut model,
            UiEvent::CmdlineShow {
                content: vec![],
                pos: 0,
                firstc: String::new(),
                prompt: "[Y]es, (N)o: ".into(),
                indent: 0,
                level: 1,
            },
        );

        let surface = render(&model);
        let cursor = surface.cursor.expect("the prompt must place a caret");
        let panel = surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::Ai(_)))
            .expect("the panel is still open beneath the prompt");
        let inside = cursor.row >= panel.rect.row
            && cursor.col >= panel.rect.col
            && cursor.col < panel.rect.col + panel.rect.width;

        assert!(
            !inside,
            "a prompt over the panel owns the keys, so the caret must not be in the panel"
        );
        assert_eq!(
            cursor.shape,
            shape_from_mode(&model),
            "the prompt answers in the editor's own mode shape, not the panel's bar"
        );
    }

    /// A cmdline left open behind the panel is state about an editor nobody
    /// is typing into: the panel took the keyboard after it opened, and a
    /// caret that tracked its `pos` would sit on the bottom grid row while
    /// the characters landed in the composer.
    #[test]
    fn a_cmdline_open_behind_the_entered_panel_does_not_take_the_caret() {
        let mut model = model_with_panel("hello");
        model.ai_panel_mut().focused = true;
        apply(
            &mut model,
            UiEvent::CmdlineShow {
                content: vec![(0, "set nu".into())],
                pos: 6,
                firstc: ":".into(),
                prompt: String::new(),
                indent: 0,
                level: 1,
            },
        );

        let (_, row) = caret_row_text(&render(&model));

        assert!(
            row.contains("> hello"),
            "the panel holds the keys, so the caret stays on its composer: {row:?}"
        );
    }

    /// The busy modal answers its own two keys and lets every other one
    /// through (see `Model::takes_focus`), so an entered panel underneath it
    /// is still what typing reaches.
    #[test]
    fn a_busy_modal_over_the_entered_panel_leaves_the_caret_in_the_composer() {
        use std::time::Duration;
        use view_core::native::supervision::{EngineBusyState, SinceStamp, WedgeKind};

        let mut model = model_with_panel("hello");
        model.ai_panel_mut().focused = true;
        model.push_overlay(
            view_core::native::geometry::OverlayBox::new(50, 30),
            OverlayKind::EngineBusy(EngineBusyState::new(
                WedgeKind::ReadSide,
                SinceStamp::new(Duration::from_secs(31)),
            )),
        );

        let (_, row) = caret_row_text(&render(&model));

        assert!(
            row.contains("> hello"),
            "the busy modal takes no keys, so the caret stays in the composer: {row:?}"
        );
    }

    /// A dead engine's modal is the one that offers to restart rather than
    /// wait, and it takes no keys either. The caret must not follow the
    /// modal's kind -- an entered panel underneath it is still what typing
    /// reaches, whichever wedge raised it.
    #[test]
    fn a_dead_engine_modal_over_the_entered_panel_also_leaves_the_caret_in_the_composer() {
        use std::time::Duration;
        use view_core::native::supervision::{EngineBusyState, SinceStamp, WedgeKind};

        let mut model = model_with_panel("hello");
        model.ai_panel_mut().focused = true;
        model.push_overlay(
            view_core::native::geometry::OverlayBox::new(50, 30),
            OverlayKind::EngineBusy(EngineBusyState::new(
                WedgeKind::Dead,
                SinceStamp::new(Duration::from_secs(31)),
            )),
        );

        let (_, row) = caret_row_text(&render(&model));

        assert!(
            row.contains("> hello"),
            "a dead-engine modal takes no keys either: {row:?}"
        );
    }

    /// The panel can be squeezed until it has no interior rows at all -- a
    /// short pane, a tabline above it and a statusline below leave the
    /// frame's two border rows and nothing between them. The caret then
    /// has no composer row to stand on, and it stays on the frame's own cell
    /// rather than falling back to the buffer nobody is typing into.
    #[test]
    fn a_panel_squeezed_to_no_interior_rows_still_keeps_its_caret_inside_the_frame() {
        use view_core::native::geometry::{Anchor, OverlayBox};

        let mut model = model_with_grid(80, 4);
        model.term_width = 80;
        model.term_height = 4;
        model.engine.mode.cursor_style_enabled = true;
        model
            .engine
            .apply_grid(GridOp::CursorGoto { row: 1, col: 7 });
        model.statusline_enabled = true;
        apply(
            &mut model,
            UiEvent::TablineUpdate {
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
                current: view_core::events::TabHandle(1),
            },
        );
        model.push_overlay(
            OverlayBox::new(30, 100).with_anchor(Anchor::Right),
            OverlayKind::Ai,
        );
        model.ai_panel_mut().push_input("hello");
        model.ai_panel_mut().focused = true;

        let surface = render(&model);
        let cursor = surface.cursor.expect("the panel must still place a caret");
        let layer = surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::Ai(_)))
            .expect("the panel overlay is open");

        assert_eq!(
            layer.rect.height, 2,
            "this geometry is the one that leaves the frame no interior rows"
        );
        assert!(
            cursor.row >= layer.rect.row
                && cursor.row < layer.rect.row + layer.rect.height
                && cursor.col >= layer.rect.col
                && cursor.col < layer.rect.col + layer.rect.width,
            "the caret must stay inside the surface that holds the keys"
        );
    }

    /// A busy modal over a confirm prompt takes no keys either, so the
    /// prompt underneath it is still what the answer reaches -- and the
    /// caret must not follow the stack's top away from it.
    #[test]
    fn a_busy_modal_over_a_prompt_leaves_the_caret_on_the_prompt() {
        use std::time::Duration;
        use view_core::native::supervision::{EngineBusyState, SinceStamp, WedgeKind};

        let mut model = model_with_panel("hello");
        apply(
            &mut model,
            UiEvent::MsgShow {
                kind: "confirm".into(),
                content: vec![(0, "Trust this project?".into())],
                replace_last: false,
            },
        );
        apply(
            &mut model,
            UiEvent::CmdlineShow {
                content: vec![],
                pos: 0,
                firstc: String::new(),
                prompt: "[Y]es, (N)o: ".into(),
                indent: 0,
                level: 1,
            },
        );
        let prompt_only = render(&model).cursor.expect("the prompt places a caret");
        model.push_overlay(
            view_core::native::geometry::OverlayBox::new(50, 30),
            OverlayKind::EngineBusy(EngineBusyState::new(
                WedgeKind::ReadSide,
                SinceStamp::new(Duration::from_secs(31)),
            )),
        );

        assert_eq!(
            render(&model).cursor,
            Some(prompt_only),
            "the modal takes no keys, so the prompt keeps the caret it had"
        );
    }

    /// The composer wraps, and the caret follows the wrap: a prompt past the
    /// panel's width puts it on the last row painted, one cell past the tail
    /// the wrap kept.
    #[test]
    fn a_wrapped_prompt_puts_the_caret_on_its_last_row() {
        let typed = "wordword ".repeat(12);
        let mut model = model_with_panel(&typed);
        model.ai_panel_mut().focused = true;

        let surface = render(&model);
        let (cursor, row) = caret_row_text(&surface);
        let layer = surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::Ai(_)))
            .expect("the panel overlay is open");
        let LayerKind::Ai(view) = &layer.kind else {
            unreachable!()
        };
        let tail = view
            .input
            .last()
            .expect("the composer painted rows")
            .clone();

        assert!(
            view.input.len() > 1,
            "the prompt must be long enough to have wrapped"
        );
        assert!(
            row.contains(tail.trim_end()),
            "the caret must be on the composer's last row: {row:?}"
        );
        assert_eq!(
            cursor.row - layer.rect.row,
            u16::try_from(view.input.len()).unwrap(),
            "one row per composer row, below the frame's own top edge"
        );
    }

    #[test]
    fn a_palette_enabled_model_renders_a_palette_layer_and_no_raw_cmdline_layer() {
        let mut model = model_with_grid(80, 24);
        model.term_width = 80;
        model.term_height = 24;
        model.palette_enabled = true;
        apply(
            &mut model,
            UiEvent::CmdlineShow {
                content: vec![(0, "set nu".to_string())],
                pos: 6,
                firstc: ":".to_string(),
                prompt: String::new(),
                indent: 0,
                level: 1,
            },
        );

        let surface = render(&model);

        assert!(
            !surface
                .layers
                .iter()
                .any(|l| matches!(l.kind, LayerKind::Cmdline(_))),
            "the raw bottom-line Cmdline layer must not paint once the palette is on"
        );
        let palette = surface
            .layers
            .iter()
            .find_map(|l| match &l.kind {
                LayerKind::Palette(view) => Some(view),
                _ => None,
            })
            .expect("palette_enabled must produce a Palette layer while the cmdline is open");
        assert_eq!(palette.query, ":set nu");
    }

    /// `PaletteState::query` must show `cmdline.prompt` (e.g. `:call
    /// input("New file: ")`'s label), not just `firstc` plus the typed
    /// text -- and the cursor, already prompt-aware via
    /// `cmdline_cursor_col`, must land past that same label rather than the
    /// two disagreeing about how wide the prefix is.
    #[test]
    fn a_prompt_labeled_cmdline_shows_its_label_in_the_palette_and_the_cursor_lands_past_it() {
        let mut model = model_with_grid(80, 24);
        model.term_width = 80;
        model.term_height = 24;
        model.palette_enabled = true;
        apply(
            &mut model,
            UiEvent::CmdlineShow {
                content: vec![(0, "foo".to_string())],
                pos: 3,
                firstc: String::new(),
                prompt: "New file: ".to_string(),
                indent: 0,
                level: 1,
            },
        );

        let surface = render(&model);

        let palette = surface
            .layers
            .iter()
            .find_map(|l| match &l.kind {
                LayerKind::Palette(view) => Some(view),
                _ => None,
            })
            .expect("palette layer must be present");
        assert_eq!(
            palette.query, "New file: foo",
            "the prompt label must show in the palette, not just factor into the cursor math"
        );

        // palette_box() on an 80x24 terminal with no chrome offset centers
        // to row 6, col 12 (see
        // the_palette_cursor_lands_inside_its_own_box_not_on_the_grids_bottom_row).
        // interior_origin adds (1, 2). The "> " prefix is 2 cells, then
        // cmdline_cursor_col counts the full 10-char "New file: " prompt
        // plus pos=3.
        let cursor = surface.cursor.expect("cmdline open, cursor must be Some");
        assert_eq!(cursor.row, 6 + 1);
        assert_eq!(cursor.col, 12 + 2 + 2 + 10 + 3);
    }

    #[test]
    fn a_prompt_overlay_suppresses_both_the_raw_cmdline_and_the_palette_layer() {
        // drives the same MsgShow(confirm) + CmdlineShow pair
        // a_confirm_prompt_overlay_paints_a_prompt_layer_with_its_choices
        // uses to open a real Prompt overlay: this is the one live path
        // that leaves `engine.cmdline` Some() with a Prompt on top of the
        // overlay stack, which is exactly the ordering this test exists to
        // pin.
        let mut model = model_with_grid(80, 24);
        model.term_width = 80;
        model.term_height = 24;
        model.palette_enabled = true;
        apply(
            &mut model,
            UiEvent::MsgShow {
                kind: "confirm".into(),
                content: vec![(0, "Save changes?".into())],
                replace_last: false,
            },
        );
        apply(
            &mut model,
            UiEvent::CmdlineShow {
                content: vec![],
                pos: 0,
                firstc: String::new(),
                prompt: "[Y]es, (N)o: ".into(),
                indent: 0,
                level: 1,
            },
        );

        let surface = render(&model);
        assert!(
            surface
                .layers
                .iter()
                .any(|l| matches!(l.kind, LayerKind::Prompt(_))),
            "the Prompt overlay itself must still paint"
        );

        assert!(
            !surface
                .layers
                .iter()
                .any(|l| matches!(l.kind, LayerKind::Cmdline(_))),
            "a Prompt on top must suppress the raw Cmdline layer, not just the palette"
        );
        assert!(
            !surface
                .layers
                .iter()
                .any(|l| matches!(l.kind, LayerKind::Palette(_))),
            "a Prompt on top must suppress the palette layer -- the Prompt already shows the input line itself"
        );

        // The prompt's box is sized to its own content and capped at its
        // 60% share, centered, on an 80x24 terminal with no chrome offset:
        // "Save changes?" (13) plus frame and padding (4) is 17, under the
        // 48-cell share, so width = 17, height = 24*40/100 = 9,
        // row = (24 - 9) / 2 = 7, col = (80 - 17) / 2 = 31.
        // interior_origin for a 17x9 rect is (1, 2). The input line is the
        // *second* header row (message first, then "> " + input), so one
        // more row past interior_origin; the confirm prompt's typed answer
        // is empty, so the "> " prefix (2 cells) is the whole offset.
        let cursor = surface
            .cursor
            .expect("a cmdline is open behind the prompt, cursor must be Some");
        assert_eq!(
            cursor.row,
            7 + 1 + 1,
            "the cursor must target the prompt's own input row, not the grid's stale bottom row"
        );
        assert_eq!(cursor.col, 31 + 2 + 2);
    }

    /// Per the wire capture, a cmdline-sourced popupmenu's `row`/`col` are
    /// relative to the cmdline, not the grid -- and with the palette off,
    /// the raw Cmdline layer always paints on the grid's own last row, so
    /// the popup must anchor near that row (flipping to grow upward, since
    /// nothing fits below the terminal's last row) rather than the wire
    /// values being read as grid-absolute coordinates.
    #[test]
    fn a_cmdline_sourced_popupmenu_positions_relative_to_the_bottom_cmdline_row_when_the_palette_is_off(
    ) {
        let mut model = model_with_grid(80, 24);
        model.term_width = 80;
        model.term_height = 24;
        model.palette_enabled = false;
        apply(
            &mut model,
            UiEvent::CmdlineShow {
                content: vec![(0, "set nu".to_string())],
                pos: 6,
                firstc: ":".to_string(),
                prompt: String::new(),
                indent: 0,
                level: 1,
            },
        );
        apply(
            &mut model,
            UiEvent::PopupmenuShow {
                items: vec![view_core::events::PmItem {
                    word: "number".into(),
                    ..Default::default()
                }],
                selected: 0,
                row: 0,
                col: 4,
                grid: -1,
            },
        );

        let surface = render(&model);

        assert!(
            surface
                .layers
                .iter()
                .any(|l| matches!(l.kind, LayerKind::Cmdline(_))),
            "the palette is off, so the raw bottom-line Cmdline layer must still paint"
        );
        let popup = surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::Popupmenu(_)))
            .expect(
                "a cmdline-sourced popupmenu must still paint its own layer while the palette \
                 is off",
            );

        // the cmdline sits on the grid's last row (23 on a 24-row grid with
        // no chrome offset); nothing fits below it, so the popup flips to
        // grow upward from there. `col`, per the wire-capture doc, is
        // measured from the start of the typed content -- firstc (1 cell,
        // ":") plus pm.col (4).
        assert_eq!(popup.rect.row, 22);
        assert_eq!(popup.rect.col, 5);
        assert!(
            popup.rect.row + popup.rect.height <= 24,
            "the popup must stay on screen, not paint past the bottom of the terminal"
        );
    }

    #[test]
    fn a_cmdline_sourced_popupmenu_renders_inside_the_palette_and_not_as_its_own_layer() {
        let mut model = model_with_grid(80, 24);
        model.term_width = 80;
        model.term_height = 24;
        model.palette_enabled = true;
        apply(
            &mut model,
            UiEvent::CmdlineShow {
                content: vec![(0, "set nu".to_string())],
                pos: 6,
                firstc: ":".to_string(),
                prompt: String::new(),
                indent: 0,
                level: 1,
            },
        );
        apply(
            &mut model,
            UiEvent::PopupmenuShow {
                items: vec![view_core::events::PmItem {
                    word: "number".into(),
                    ..Default::default()
                }],
                selected: 0,
                row: 0,
                col: 0,
                grid: -1,
            },
        );

        let surface = render(&model);

        assert!(
            !surface
                .layers
                .iter()
                .any(|l| matches!(l.kind, LayerKind::Popupmenu(_))),
            "a cmdline-sourced popupmenu must not also paint its own Popupmenu layer"
        );
        let palette = surface
            .layers
            .iter()
            .find_map(|l| match &l.kind {
                LayerKind::Palette(view) => Some(view),
                _ => None,
            })
            .expect("palette layer must be present");
        assert_eq!(
            palette
                .rows
                .iter()
                .map(|r| r.label.clone())
                .collect::<Vec<_>>(),
            vec!["number".to_string()],
            "the cmdline-sourced popupmenu's candidates must show up as palette rows"
        );
    }

    /// The command palette's popupmenu-routing contract, falsifiable: an
    /// insert-mode buffer completion (a non-negative `grid`) must show in
    /// its own popupmenu at the cursor, never inside the palette box --
    /// this is the assertion a routing bug that pointed every popupmenu
    /// into the palette would fail, by name, without touching any other
    /// test in this file.
    #[test]
    fn a_buffer_sourced_popupmenu_never_renders_inside_the_palette_and_keeps_its_own_layer() {
        let mut model = model_with_grid(80, 24);
        model.term_width = 80;
        model.term_height = 24;
        model.palette_enabled = true;
        apply(
            &mut model,
            UiEvent::PopupmenuShow {
                items: vec![view_core::events::PmItem {
                    word: "helper".into(),
                    ..Default::default()
                }],
                selected: 0,
                row: 3,
                col: 5,
                grid: 1,
            },
        );

        let surface = render(&model);

        let popup = surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::Popupmenu(_)))
            .expect(
                "a buffer-sourced (non-negative grid) popupmenu must keep its own Popupmenu \
                 layer even while the palette is enabled",
            );
        let LayerKind::Popupmenu(state) = &popup.kind else {
            unreachable!()
        };
        assert_eq!(state.items[0].word, "helper");
        assert!(
            !surface
                .layers
                .iter()
                .any(|l| matches!(l.kind, LayerKind::Palette(_))),
            "no cmdline is open, so no palette should render at all"
        );
    }

    #[test]
    fn the_palette_cursor_lands_inside_its_own_box_not_on_the_grids_bottom_row() {
        let mut model = model_with_grid(80, 24);
        model.term_width = 80;
        model.term_height = 24;
        model.palette_enabled = true;
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

        // palette_box() is OverlayBox::new(70, 50), centered, on an 80x24
        // terminal with no chrome offset: width = 80*70/100 = 56,
        // height = 24*50/100 = 12, row = (24 - 12) / 2 = 6,
        // col = (80 - 56) / 2 = 12. interior_origin for a 56x12 rect is
        // (1, 2) (border row, border + one pad column). The header line is
        // "> hello" -- prefix "> " is 2 cells, then
        // cmdline_cursor_col(":", "", pos=5) = 6.
        let cursor = surface.cursor.expect("cmdline open, cursor must be Some");
        assert_eq!(
            cursor.row,
            6 + 1,
            "cursor must sit on the palette's header row, not the grid's bottom row"
        );
        assert_eq!(cursor.col, 12 + 2 + 2 + 6);
    }

    /// Both the box paint call and the cursor resolve through the same
    /// [`palette_rect`], so a tabline's reserved chrome row cannot make them
    /// disagree the way they used to when the cursor added `offset` a
    /// second time on top of an already offset-shifted box.
    #[test]
    fn a_reserved_chrome_row_shifts_the_palette_box_and_its_cursor_by_the_same_amount() {
        let mut model = model_with_grid(80, 24);
        model.term_width = 80;
        model.term_height = 24;
        model.palette_enabled = true;
        apply(
            &mut model,
            UiEvent::TablineUpdate {
                current: view_core::events::TabHandle(1),
                tabs: vec![
                    view_core::events::TabEntry {
                        tab: view_core::events::TabHandle(1),
                        name: "a".into(),
                    },
                    view_core::events::TabEntry {
                        tab: view_core::events::TabHandle(2),
                        name: "b".into(),
                    },
                ],
            },
        );
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

        let palette_layer = surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::Palette(_)))
            .expect("palette layer present");
        // one row reserved for the tabline: the box's terminal-relative
        // height budget shrinks to 23 rows before centering, then the
        // whole box shifts down by that one reserved row --
        // row = (23 - 11) / 2 + 1 = 7.
        assert_eq!(palette_layer.rect.row, 7);

        let cursor = surface.cursor.expect("cmdline open, cursor must be Some");
        assert_eq!(
            cursor.row,
            palette_layer.rect.row + 1,
            "cursor must land on the palette's header row, in the same \
             chrome-shifted coordinate space as the box it sits inside"
        );
    }

    /// The statusline's layer emission, at this crate's own seam rather
    /// than only through a pty: the bar is a full-width row placed
    /// immediately below the grid `grid_target()` already shrank for it,
    /// and it exists exactly while the feature does.
    #[test]
    fn the_statusline_feature_emits_one_full_width_bar_below_the_grid() {
        let mut model = model_with_grid(40, 11);
        model.term_width = 40;
        model.term_height = 12;
        model.statusline_enabled = true;

        let surface = render(&model);
        let bars: Vec<&Layer> = surface
            .layers
            .iter()
            .filter(|l| matches!(l.kind, LayerKind::Statusline(_)))
            .collect();
        assert_eq!(bars.len(), 1, "exactly one statusline layer: {bars:?}");
        assert_eq!(
            bars[0].rect,
            Rect::new(model.chrome_rows() + 11, 0, 40, 1),
            "the bar takes the row immediately under the engine grid, full width"
        );
        assert!(
            bars[0].borders.is_some(),
            "a statusline is a native overlay kind and carries the tier's charset, \
             even at the height where the layout pass draws no edge cells"
        );

        model.statusline_enabled = false;
        assert!(
            render(&model)
                .layers
                .iter()
                .all(|l| !matches!(l.kind, LayerKind::Statusline(_))),
            "the feature off means no layer at all, not an empty one"
        );
    }

    /// Queues one prediction, shared with `cache`'s tests so both sides of
    /// the frame-builder pair drive speculation through the same fixture.
    pub(crate) fn predict(model: &mut Model, key: char, cursor: (u16, u16), millis: u64) {
        let stamp =
            view_core::native::speculate::SpecStamp::new(std::time::Duration::from_millis(millis));
        assert!(
            model
                .speculate
                .predict("insert", key, cursor, stamp)
                .is_some(),
            "{key:?} at {cursor:?} is a plain insert-mode character"
        );
    }

    /// The caret leads the burst it belongs to: with three glyphs painted
    /// ahead of the engine's own cursor, the cursor is shown one past the
    /// last of them, which is where the next character will land.
    #[test]
    fn a_pending_burst_carries_the_cursor_past_its_last_predicted_cell() {
        let mut model = model_with_grid(40, 12);
        model
            .engine
            .apply_grid(GridOp::CursorGoto { row: 4, col: 6 });
        for (i, key) in ['a', 'b', 'c'].into_iter().enumerate() {
            predict(&mut model, key, (4, 6), 10 + i as u64);
        }

        let cursor = render(&model).cursor.expect("a sized grid places a cursor");

        assert_eq!(cursor.row, 4);
        assert_eq!(
            cursor.col, 9,
            "the caret sat on the burst instead of leading it"
        );
    }

    /// And gives it straight back: once the redraw the burst was waiting for
    /// retires the last prediction, the engine's own cursor is authoritative
    /// again on the very next frame.
    #[test]
    fn the_engine_cursor_is_authoritative_again_the_frame_after_the_burst_settles() {
        let mut model = model_with_grid(40, 12);
        model
            .engine
            .apply_grid(GridOp::CursorGoto { row: 4, col: 6 });
        predict(&mut model, 'a', (4, 6), 10);
        predict(&mut model, 'b', (4, 6), 11);

        model.speculate.reconcile(&[UiEvent::GridLine {
            grid: 1,
            row: 4,
            col_start: 6,
            cells: vec![
                view_core::events::GridCell {
                    text: "a".to_string(),
                    hl_id: 0,
                    repeat: 1,
                },
                view_core::events::GridCell {
                    text: "b".to_string(),
                    hl_id: 0,
                    repeat: 1,
                },
            ],
        }]);
        model
            .engine
            .apply_grid(GridOp::CursorGoto { row: 4, col: 8 });

        let cursor = render(&model).cursor.expect("a sized grid places a cursor");
        assert!(model.speculate.pending().is_empty());
        assert_eq!((cursor.row, cursor.col), (4, 8));
    }

    /// A prediction on some other row says nothing about where this row's
    /// caret belongs: a burst that wrapped, or one left over from a cursor
    /// the engine has since moved, must not drag the caret across the
    /// screen.
    #[test]
    fn a_prediction_on_another_row_leaves_the_cursor_where_the_engine_put_it() {
        let mut model = model_with_grid(40, 12);
        model
            .engine
            .apply_grid(GridOp::CursorGoto { row: 4, col: 6 });
        predict(&mut model, 'a', (7, 20), 10);

        let cursor = render(&model).cursor.expect("a sized grid places a cursor");

        assert_eq!((cursor.row, cursor.col), (4, 6));
    }

    /// Nor does a prediction the engine's cursor has already moved past --
    /// the caret only ever leads, never rewinds.
    #[test]
    fn a_prediction_behind_the_engine_cursor_never_pulls_the_caret_back() {
        let mut model = model_with_grid(40, 12);
        model
            .engine
            .apply_grid(GridOp::CursorGoto { row: 4, col: 20 });
        predict(&mut model, 'a', (4, 6), 10);

        let cursor = render(&model).cursor.expect("a sized grid places a cursor");

        assert_eq!((cursor.row, cursor.col), (4, 20));
    }

    /// The caret is a position on the terminal, not a guess about content,
    /// so the off-grid prediction a painter drops cannot push it past the
    /// last column either.
    #[test]
    fn the_caret_never_leaves_the_grid_for_a_prediction_at_its_edge() {
        let mut model = model_with_grid(40, 12);
        model
            .engine
            .apply_grid(GridOp::CursorGoto { row: 4, col: 38 });
        predict(&mut model, 'a', (4, 38), 10);
        predict(&mut model, 'b', (4, 38), 11);

        let cursor = render(&model).cursor.expect("a sized grid places a cursor");

        assert_eq!(
            (cursor.row, cursor.col),
            (4, 39),
            "the caret must stay on the grid the terminal actually has"
        );
    }

    fn speculated_cells(surface: &Surface) -> Option<(Rect, Vec<PredictedCell>)> {
        surface.layers.iter().find_map(|l| match &l.kind {
            LayerKind::Speculated(cells) => Some((l.rect, cells.clone())),
            _ => None,
        })
    }

    /// The resting frame, which is every frame outside a typing burst: no
    /// prediction pending means no layer at all, not an empty one, so a
    /// painter and damage tracking have nothing to do with the feature.
    #[test]
    fn nothing_pending_emits_no_speculated_layer() {
        let model = model_with_grid(40, 12);

        assert!(
            speculated_cells(&render(&model)).is_none(),
            "an empty pending list must not reach the surface as an empty layer"
        );
    }

    /// The predicted glyphs reach the surface at the cells they were
    /// predicted for, and the layer's rect is the terminal-space box around
    /// them -- chrome offset included, and never the whole grid, since the
    /// rect is what a painter treats as dirty.
    #[test]
    fn pending_predictions_emit_one_speculated_layer_over_their_own_cells() {
        let mut model = model_with_grid(40, 11);
        model.term_width = 40;
        model.term_height = 12;
        apply(
            &mut model,
            UiEvent::TablineUpdate {
                current: view_core::events::TabHandle(1),
                tabs: vec![
                    view_core::events::TabEntry {
                        tab: view_core::events::TabHandle(1),
                        name: "a".into(),
                    },
                    view_core::events::TabEntry {
                        tab: view_core::events::TabHandle(2),
                        name: "b".into(),
                    },
                ],
            },
        );
        assert_eq!(model.chrome_rows(), 1, "two tabs reserve the tabline row");
        for key in ['h', 'i'] {
            predict(&mut model, key, (4, 7), 0);
        }

        let (rect, cells) = speculated_cells(&render(&model)).expect("two predictions are pending");
        assert_eq!(
            cells
                .iter()
                .map(|c| (c.row, c.col, c.glyph))
                .collect::<Vec<_>>(),
            vec![(4, 7, 'h'), (4, 8, 'i')],
            "cells stay in grid coordinates, the space they were predicted in"
        );
        assert_eq!(
            rect,
            Rect::new(5, 7, 2, 1),
            "the rect is the cells' own box, shifted down by the reserved chrome row"
        );
    }

    /// The drop-never-clamp contract `PredictedCell` states: a prediction
    /// that ran off the grid (a wrapped line, a `textwidth` break, a resize
    /// under a pending burst) must vanish, because clamping it into the last
    /// real column paints a glyph the user typed somewhere else and leaves
    /// it there for up to `SPECULATION_MAX_AGE`.
    #[test]
    fn a_prediction_past_the_grid_edge_is_dropped_rather_than_clamped() {
        let mut model = model_with_grid(10, 4);
        predict(&mut model, 'a', (1, 9), 0);
        predict(&mut model, 'b', (1, 9), 0);
        predict(&mut model, 'c', (3, 2), 0);
        predict(&mut model, 'd', (9, 0), 0);

        let (rect, cells) = speculated_cells(&render(&model)).expect("two predictions are on-grid");
        assert_eq!(
            cells
                .iter()
                .map(|c| (c.row, c.col, c.glyph))
                .collect::<Vec<_>>(),
            vec![(1, 9, 'a'), (3, 2, 'c')],
            "the column past the last one and the row past the last one are both gone; \
             neither is clamped onto a cell the grid does have"
        );
        assert_eq!(
            rect,
            Rect::new(1, 2, 8, 3),
            "the rect boxes the surviving cells, not the dropped ones"
        );
    }

    /// Every pending prediction being off-grid is the same case as nothing
    /// pending: a layer with no cells has nothing to paint, and its rect
    /// would dirty rows for no reason.
    #[test]
    fn predictions_entirely_off_the_grid_emit_no_layer_at_all() {
        let mut model = model_with_grid(10, 4);
        predict(&mut model, 'a', (7, 0), 0);

        assert!(speculated_cells(&render(&model)).is_none());
    }

    /// The resting frame carries nothing to attribute, and the burst frame
    /// does: the two answers a write-attribution is built on.
    #[test]
    fn only_a_frame_that_paints_a_prediction_carries_speculation() {
        let mut model = model_with_grid(40, 12);
        assert!(
            !render(&model).carries_speculation(),
            "a frame with nothing pending must not be attributable to speculation"
        );

        predict(&mut model, 'a', (2, 3), 0);
        assert!(
            render(&model).carries_speculation(),
            "a painted predicted glyph is exactly what the counter attributes a write to"
        );
    }

    /// [`Surface::from_layers`] lets a consumer outside this crate describe
    /// a frame directly, so a layer holding no cells is a state the type
    /// admits even though [`render`] never builds one. It paints nothing,
    /// so it attributes nothing.
    #[test]
    fn a_speculated_layer_with_no_cells_attributes_no_write() {
        let surface = Surface::from_layers(vec![Layer::new(
            Rect::new(0, 0, 1, 1),
            LayerKind::Speculated(Vec::new()),
            TermCaps::default(),
        )]);

        assert!(!surface.carries_speculation());
    }

    /// The narrow half of the contract: pending is not painted. A prediction
    /// the grid cannot hold is dropped before the painter sees it, so the
    /// write that frame carries owes nothing to speculation and must fall
    /// back to the counter that catches paints nothing explains.
    #[test]
    fn a_prediction_the_painter_dropped_is_not_a_speculated_paint() {
        let mut model = model_with_grid(10, 4);
        predict(&mut model, 'a', (7, 0), 0);

        assert!(
            !model.speculate.pending().is_empty(),
            "the fixture needs a prediction that is pending but off-grid"
        );
        assert!(
            !render(&model).carries_speculation(),
            "reading the pending list instead of the built frame would attribute a write to a \
             glyph no painter ever wrote"
        );
    }

    /// What makes the caret-only frame need no separate attribution: the
    /// caret cannot stand at a predicted column on a frame this layer is
    /// absent from, so counting the layer counts every write the prediction
    /// positioned.
    #[test]
    fn a_caret_advance_cannot_happen_without_the_layer() {
        let mut checked = 0;
        for cursor_col in [0_u16, 5, 9] {
            for predicted in [(1_u16, cursor_col), (1, 9), (1, 12), (7, 0)] {
                let mut model = model_with_grid(10, 4);
                model.engine.apply_grid(GridOp::CursorGoto {
                    row: 1,
                    col: cursor_col,
                });
                predict(&mut model, 'a', predicted, 0);
                let surface = render(&model);
                let caret = surface.cursor.expect("a sized grid places a cursor");
                if caret.col != cursor_col {
                    checked += 1;
                    assert!(
                        surface.carries_speculation(),
                        "the caret moved to {} for a prediction at {predicted:?} on a frame the \
                         attribution calls unspeculated",
                        caret.col
                    );
                }
            }
        }
        assert!(
            checked > 0,
            "no case in the sweep advanced the caret; the implication was proven over nothing"
        );
    }

    /// Z-order, both halves: above the grid it predicts, and below chrome
    /// that opened over it. A prediction painted over an open picker would
    /// show a glyph from the buffer underneath, inside a box the user is
    /// typing a query into.
    #[test]
    fn the_speculated_layer_sits_above_the_grid_and_below_every_overlay() {
        use view_core::native::geometry::{Anchor, OverlayBox};
        use view_core::native::tree::TreeState;

        let mut model = model_with_grid(40, 12);
        model.term_width = 40;
        model.term_height = 12;
        model.content_painted = true;
        predict(&mut model, 'a', (2, 3), 0);
        model.push_overlay(
            OverlayBox::new(30, 100).with_anchor(Anchor::Left),
            OverlayKind::Tree(TreeState::open(std::path::PathBuf::from("/tmp/example"))),
        );

        let surface = render(&model);
        let kinds: Vec<&LayerKind> = surface.layers.iter().map(|l| &l.kind).collect();
        assert_eq!(kinds.len(), 3, "grid, prediction, overlay -- nothing else");
        assert!(
            matches!(kinds[0], LayerKind::EngineGrid),
            "the engine grid is the bottom layer, got {:?}",
            kinds[0]
        );
        assert!(
            matches!(kinds[1], LayerKind::Speculated(_)),
            "the prediction paints over the grid, not under it, got {:?}",
            kinds[1]
        );
        assert!(
            matches!(kinds[2], LayerKind::Tree(_)),
            "the overlay stays above both, got {:?}",
            kinds[2]
        );
    }
}
