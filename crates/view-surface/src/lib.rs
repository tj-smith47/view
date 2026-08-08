//! The render model: what to draw, independent of any frontend.
//!
//! [`render`] turns a [`Model`] into a [`Surface`]: an ordered list of
//! [`Layer`]s plus the real terminal cursor's position and shape. Pure
//! data, no drawing here; `view-tui` is the only crate that turns a
//! `Surface` into pixels.

pub mod overlay;

use unicode_width::UnicodeWidthStr;
use view_core::events::{saturate_u16, PmItem};
use view_core::model::{CmdlineState, Model, Overlay, OverlayKind, PopupmenuState, TablineState};
use view_core::native::geometry::{Anchor, OverlayBox};
use view_core::native::palette::PaletteState;
use view_core::native::views::{
    PaletteView, PickerView, PromptView, Span, StatuslineView, TreeView,
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
    /// The visible toast box's physical lines, already selected by
    /// `Messages::visible_lines` (persistent error/warn lines kept, the
    /// most recent transient lines filling what room remains) and split on
    /// each entry's own embedded line breaks: one row per span-vec, in
    /// display order top to bottom. Present while any message is visible.
    /// Each row is a single [`view_core::native::views::StyleRole::Plain`]
    /// span (a toast has no per-segment structure to preserve), kept as a
    /// span-vec rather than a `String` so the layer honestly carries the
    /// same overlay-row shape every other overlay layer does.
    Messages(Vec<Vec<Span>>),
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

impl Layer {
    /// A layer that draws no frame of its own: an engine grid, or one of
    /// the chrome overlays that fills its whole rect with content.
    ///
    /// [`overlay::framed`] is the counterpart for a native overlay, which
    /// carries the charset its border is drawn from.
    #[must_use]
    pub fn unframed(rect: Rect, kind: LayerKind) -> Self {
        Self {
            rect,
            kind,
            borders: None,
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

    let mut layers = vec![Layer::unframed(
        Rect::new(offset, 0, grid_w, grid_h),
        LayerKind::EngineGrid,
    )];

    if !model.content_painted {
        // sized from the real terminal, not the (still 0x0 pre-attach)
        // engine grid: the very first shell paint happens before nvim has
        // ever sent a grid_resize, so grid_w/grid_h are not yet meaningful
        // dimensions to paint a placeholder into
        layers.push(Layer::unframed(
            Rect::new(0, 0, model.term_width, model.term_height),
            LayerKind::Shell,
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
            layers.push(Layer::unframed(
                Rect::new(0, 0, grid_w, 1).clamp_to(grid_w, grid_h),
                LayerKind::Tabline(tabline.clone()),
            ));
        }
    }
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
            let state = PaletteState::new(cmdline.clone(), completion);
            let rect = palette_box().rect(model.term_width, model.term_height);
            layers.push(overlay::framed(
                Rect::new(rect.row, rect.col, rect.width, rect.height),
                LayerKind::Palette(state.view()),
                BorderSet::for_tier(model.caps.tier),
            ));
        } else {
            layers.push(overlay_layer(
                grid_h.saturating_sub(1),
                0,
                grid_w,
                1,
                (grid_w, grid_h),
                offset,
                LayerKind::Cmdline(cmdline.clone()),
            ));
        }
    }
    if !engine.messages.entries.is_empty() {
        // `Messages::visible_lines` is the single selection of what
        // actually shows: persistent (error/warn) lines always kept, the
        // remaining row budget filled with the most recent transient
        // lines, one physical line (an entry's content split on its own
        // embedded `\n`s) per visual row rather than one row per
        // `MessageEntry` -- sizing/painting per entry instead squashes
        // every line of a multi-line `emsg` into a single row wide enough
        // to hold all of them concatenated, and leaves the row it should
        // have occupied showing whatever the grid layer painted
        // underneath. Both the layer's geometry and its painted content
        // come from this exact `Vec<String>`, so sizing and painting can
        // never disagree about what is visible -- which requires handing
        // `visible_lines` a budget already shrunk by the two rows
        // `paint_messages` reserves for its own top/bottom border edge:
        // selecting against the full `grid_h` and growing the frame
        // around the result afterward let the row count `visible_lines`
        // chose exceed what the framed interior can hold, so the interior
        // clamp below silently dropped whatever the tail of the selected
        // `Vec` happened to be -- the newest lines, including the
        // always-kept persistent error/warn line -- while
        // `visible_lines`'s own persistent-line-priority eviction never
        // got the chance to make that call itself. `.max(1)` on the row
        // budget matches this block's own width/height floor below: a
        // pre-attach frame (`grid_h` still 0, e.g. a native toast pushed
        // before the engine's first `GridResize`) still reserves its one
        // row rather than vanishing until real grid content arrives, and
        // a grid too short to fit even a bordered single line still
        // selects one physical line -- `paint_message_border`'s own
        // width/height-under-2 guard is what degrades that case to a
        // blank (borderless) fill instead of a panic.
        let visible = engine
            .messages
            .visible_lines(usize::from(grid_h.saturating_sub(2)).max(1));
        let content_width = messages_width(&visible)
            .min(grid_w.saturating_sub(2))
            .max(1);
        let content_height = u16::try_from(visible.len())
            .unwrap_or(u16::MAX)
            .min(grid_h.saturating_sub(2))
            .max(1);
        // the border frame (paint_messages) adds one cell on every edge
        // around the content `visible_lines` already selected -- grown
        // here, not in `visible_lines` itself, so the selection logic
        // above stays free of layout math; `overlay_layer`'s own
        // `clamp_to` still caps the grown rect to the live grid, which by
        // construction never has to remove more than the frame it just
        // added since the budget above already left room for it
        let width = content_width.saturating_add(2);
        let height = content_height.saturating_add(2);
        let col = grid_w.saturating_sub(width);
        layers.push(overlay_layer(
            0,
            col,
            width,
            height,
            (grid_w, grid_h),
            offset,
            LayerKind::Messages(visible),
        ));
    }
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
            let row = saturate_u16(pm.row);
            let col = saturate_u16(pm.col);
            let width = popupmenu_width(&pm.items).min(grid_w).max(1);
            let height = u16::try_from(pm.items.len()).unwrap_or(u16::MAX).max(1);
            layers.push(overlay_layer(
                row,
                col,
                width,
                height,
                (grid_w, grid_h),
                offset,
                LayerKind::Popupmenu(pm.clone()),
            ));
        }
    }
    if model.statusline_rows() > 0 {
        // `grid_target()` already shrank the engine's own grid by
        // `statusline_rows()`, so this row sits immediately below whatever
        // `grid_h` the engine actually reported -- never recomputed from
        // `term_height`, which would disagree the moment a resize is still
        // in flight to nvim.
        layers.push(overlay::framed(
            Rect::new(
                offset.saturating_add(grid_h),
                0,
                grid_w,
                model.statusline_rows(),
            ),
            LayerKind::Statusline(engine.statusline.view(grid_w)),
            BorderSet::for_tier(model.caps.tier),
        ));
    }
    // last, and in stack order: a native overlay sits above every engine
    // overlay, and the stack's tail is the one holding focus, so painting
    // in stack order puts the focused overlay on top of the ones it opened
    // over
    layers.extend(
        model
            .overlays()
            .iter()
            .filter_map(|open| native_layer(model, open)),
    );

    Surface {
        layers,
        cursor: cursor_spec(model, offset),
    }
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
    OverlayBox::new(70, 50).with_anchor(Anchor::Top)
}

/// Builds one grid-space overlay [`Layer`]: `row`/`col`/`width`/`height`
/// are first clamped to `bounds` (the grid's own coordinate space, which is
/// what wire-derived positions like a popup menu's `(row, col)` are
/// expressed in), then translated down by `offset` (the reserved chrome
/// rows) to land in the terminal's own coordinate space. Clamping before
/// translating means a hostile or stale position/size from wire-derived
/// state can never place a layer outside the current grid, regardless of
/// whether chrome is currently reserved.
fn overlay_layer(
    row: u16,
    col: u16,
    width: u16,
    height: u16,
    bounds: (u16, u16),
    offset: u16,
    kind: LayerKind,
) -> Layer {
    let clamped = Rect::new(row, col, width, height).clamp_to(bounds.0, bounds.1);
    Layer::unframed(
        Rect {
            row: clamped.row.saturating_add(offset),
            ..clamped
        },
        kind,
    )
}

/// The framed [`Layer`] for one open native overlay, or `None` for an
/// overlay carrying no paintable content.
///
/// The rect comes from [`Model::overlay_rect`], the same resolution
/// `update()`'s mouse hit-test routes a click through, so paint and routing
/// cannot disagree about where an overlay is. The border charset comes from
/// the terminal's own tier, resolved once here rather than per painter.
fn native_layer(model: &Model, open: &Overlay) -> Option<Layer> {
    let kind = layer_kind(&open.kind)?;
    let cells = model.overlay_rect(open);
    Some(overlay::framed(
        Rect::new(cells.row, cells.col, cells.width, cells.height),
        kind,
        BorderSet::for_tier(model.caps.tier),
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
fn layer_kind(kind: &OverlayKind) -> Option<LayerKind> {
    match kind {
        OverlayKind::Prompt(state) => Some(LayerKind::Prompt(state.view())),
        OverlayKind::Picker(state) => Some(LayerKind::Picker(state.view())),
        OverlayKind::Tree(state) => Some(LayerKind::Tree(state.view())),
        // a message-history browse is presented the same way the palette
        // itself is: rows in a centered box, no fields of its own the
        // palette's LayerKind doesn't already carry
        OverlayKind::MessageHistory(state) => Some(LayerKind::Palette(state.view())),
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
fn cursor_spec(model: &Model, offset: u16) -> Option<CursorSpec> {
    let (width, height) = model.engine.grid().size();
    if width == 0 || height == 0 {
        return None;
    }
    let shape = shape_from_mode(model);
    let prompt_open = matches!(
        model.overlays().last().map(|open| &open.kind),
        Some(OverlayKind::Prompt(_))
    );
    let (row, col) = if let Some(cmdline) = &model.engine.cmdline {
        if !prompt_open && model.palette_enabled {
            palette_cursor(model, cmdline)
        } else {
            let col = cmdline_cursor_col(cmdline).min(width.saturating_sub(1));
            (height.saturating_sub(1), col)
        }
    } else {
        model.engine.grid().cursor()
    };
    Some(CursorSpec {
        row: row.saturating_add(offset),
        col,
        shape,
    })
}

/// The palette's own cursor position: past its box's frame and pad (see
/// [`overlay::interior_origin`], the same arithmetic [`overlay::rows`]
/// frames every layer's content with), past the fixed `"> "` prefix the
/// palette's header row always draws before the query, then
/// [`cmdline_cursor_col`] cells further in -- the same column the raw
/// bottom-line cmdline placed its cursor at, measured from the palette
/// box's own origin instead of the grid's.
fn palette_cursor(model: &Model, cmdline: &CmdlineState) -> (u16, u16) {
    let rect = palette_box().rect(model.term_width, model.term_height);
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
            .find(|l| matches!(l.kind, LayerKind::Messages(_)))
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
        // height 4, not 2: the framed interior is 2 rows shy of grid_h, so
        // a 2-line selection budget needs a 4-row grid to actually reach
        // `visible_lines` (a 2-row grid would floor the budget at 1 via
        // the saturating shrink below, testing the floor instead of the
        // eviction-priority behavior this test targets)
        let mut model = model_with_grid(20, 4);
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

        let LayerKind::Messages(lines) = &surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::Messages(_)))
            .expect("messages layer present")
            .kind
        else {
            unreachable!("just matched Messages above");
        };
        let texts: Vec<String> = lines
            .iter()
            .map(|spans| spans.iter().map(|s| s.text.as_str()).collect())
            .collect();
        assert_eq!(
            texts,
            vec!["an error".to_string(), "new info".to_string()],
            "the persistent error must survive the overflow; the oldest transient line is evicted instead"
        );
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
            .position(|l| matches!(l.kind, LayerKind::Messages(_)))
            .expect("Messages layer present");
        assert!(
            shell_idx < messages_idx,
            "Shell must paint before (underneath) Messages in z-order"
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

    /// The falsifiable check from the palette's own brief: an insert-mode
    /// buffer completion (a non-negative `grid`) must show in its own
    /// popupmenu at the cursor, never inside the palette box -- this is the
    /// assertion a routing bug that pointed every popupmenu into the
    /// palette would fail, by name, without touching any other test in this
    /// file.
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

        // palette_box() is OverlayBox::new(70, 50) anchored Top on an
        // 80x24 terminal: width = 80*70/100 = 56, height = 24*50/100 = 12,
        // row = 0 (Top-anchored), col = (80 - 56) / 2 = 12. interior_origin
        // for a 56x12 rect is (1, 2) (border row, border + one pad column).
        // The header line is "> hello" -- prefix "> " is 2 cells, then
        // cmdline_cursor_col(":", "", pos=5) = 6.
        let cursor = surface.cursor.expect("cmdline open, cursor must be Some");
        assert_eq!(
            cursor.row, 1,
            "cursor must sit on the palette's header row, not the grid's bottom row"
        );
        assert_eq!(cursor.col, 12 + 2 + 2 + 6);
    }
}
