//! Frame-to-frame [`Surface`] reuse, checked against a from-scratch
//! rebuild.
//!
//! [`SurfaceCache::render`] is the per-frame entry point for a caller that
//! paints repeatedly from the same evolving [`Model`] (the runtime loop,
//! the oracle's session drivers). [`crate::render`] stays the from-scratch
//! reference: it builds the whole frame from nothing but the `Model`, and
//! in debug builds every frame this module produces is asserted equal to
//! it, component for component. A cached renderer without that guard is a
//! silent-drift machine -- a stale layer looks exactly like a quiet frame --
//! so the guard is the design, and the release build runs the identical
//! code path with only the assertion compiled out (never a third variant).

use view_core::model::Model;

use crate::{cursor_spec, render, speculated_layer, LayerKind, Surface, SPECULATED_LAYER_INDEX};

/// Holds the previously rendered frame so the next one can reuse it.
///
/// The paint path's dominant cost is cache and TLB residency -- proportional
/// to memory touched per frame, not to instructions executed -- so the win
/// here is touching almost nothing when almost nothing changed: a steady
/// typing frame updates only the cursor (plus the statusline bar's row when
/// that feature is on, and the speculated layer when a prediction is
/// pending) instead of re-cloning every chrome state into a fresh
/// allocation.
#[derive(Debug, Default)]
pub struct SurfaceCache {
    frame: Option<Frame>,
    /// Frames rendered through this cache, so the equivalence guard can
    /// name the frame a divergence appeared on.
    frames: u64,
    /// Frames that could not reuse the previous surface. Observable
    /// alongside `frames` so a test can pin that reuse actually happened
    /// (or was correctly refused) rather than inferring it from timing.
    rebuilds: u64,
}

/// One cached frame: the surface handed out last time plus the inputs it
/// was built from.
#[derive(Debug)]
struct Frame {
    surface: Surface,
    inputs: Inputs,
}

impl Frame {
    fn rebuild(model: &Model) -> Self {
        Self {
            surface: render(model),
            inputs: Inputs::capture(model),
        }
    }
}

/// Everything [`crate::render`] reads that can change which layers exist,
/// where they sit, or what they carry -- except the inputs the reuse path
/// re-resolves itself every frame (the cursor, the statusline view, and the
/// speculated layer).
///
/// The grid's cell content is deliberately absent: a `Surface` never
/// carries grid cells (painters read them from the `Model` directly), so
/// grid edits cannot invalidate a cached surface. The overlay stack is
/// tracked only as a presence bit: comparing a stack of full feature
/// states (a picker's candidate list, a tree's entries) per frame would
/// touch the very memory this cache exists to avoid touching, and every
/// keystroke routed at an open overlay mutates its state anyway, so frames
/// with overlays open rebuild from scratch -- exactly what they did before
/// this cache existed.
#[derive(Debug)]
struct Inputs {
    grid: (u16, u16),
    offset: u16,
    term: (u16, u16),
    content_painted: bool,
    palette_enabled: bool,
    tier: view_core::model::Tier,
    statusline_rows: u16,
    had_overlays: bool,
    tabline: Option<view_core::model::TablineState>,
    cmdline: Option<view_core::model::CmdlineState>,
    popupmenu: Option<view_core::model::PopupmenuState>,
    messages: Vec<view_core::model::MessageEntry>,
}

impl Inputs {
    fn capture(model: &Model) -> Self {
        let engine = &model.engine;
        Self {
            grid: engine.grid().size(),
            offset: model.chrome_rows(),
            term: (model.term_width, model.term_height),
            content_painted: model.content_painted,
            palette_enabled: model.palette_enabled,
            tier: model.caps.tier,
            statusline_rows: model.statusline_rows(),
            had_overlays: !model.overlays().is_empty(),
            tabline: engine.tabline.clone(),
            cmdline: engine.cmdline.clone(),
            popupmenu: engine.popupmenu.clone(),
            messages: engine.messages.entries.clone(),
        }
    }

    /// Whether `model` would produce the same layers this snapshot did.
    /// Comparison only -- no clone, no allocation: on the steady-typing
    /// frame every field is a scalar compare or an `is_none` pair.
    fn matches(&self, model: &Model) -> bool {
        let engine = &model.engine;
        !self.had_overlays
            && model.overlays().is_empty()
            && self.grid == engine.grid().size()
            && self.offset == model.chrome_rows()
            && self.term == (model.term_width, model.term_height)
            && self.content_painted == model.content_painted
            && self.palette_enabled == model.palette_enabled
            && self.tier == model.caps.tier
            && self.statusline_rows == model.statusline_rows()
            && self.tabline == engine.tabline
            && self.cmdline == engine.cmdline
            && self.popupmenu == engine.popupmenu
            && self.messages == engine.messages.entries
    }
}

impl SurfaceCache {
    /// An empty cache; the first [`SurfaceCache::render`] builds from
    /// scratch.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The frame for `model`, reusing the previous frame's layers when
    /// nothing that shapes them changed.
    ///
    /// On reuse only the cursor is re-resolved (it moves on nearly every
    /// keystroke and costs a few field reads), plus the statusline bar's
    /// view when that feature is on (its segments track the cursor too;
    /// the fresh view replaces the cached one only when it differs, so an
    /// unchanged bar touches nothing), plus the speculated layer (added,
    /// replaced, or dropped to match what is pending). Any other change --
    /// chrome state, geometry, an overlay opening or closing -- rebuilds the
    /// whole frame through [`crate::render`], which is the exact pre-cache
    /// cost.
    ///
    /// In debug builds the returned frame is asserted equal to a
    /// from-scratch [`crate::render`] of the same `model`, naming the frame
    /// and the first divergent component; release builds return the same
    /// frame unchecked.
    #[must_use]
    pub fn render(&mut self, model: &Model) -> &Surface {
        self.frames = self.frames.wrapping_add(1);
        if self.frame.as_ref().is_some_and(|f| f.inputs.matches(model)) {
            if let Some(frame) = self.frame.as_mut() {
                let cursor = cursor_spec(model, frame.inputs.offset, &frame.surface.layers);
                frame.surface.cursor = cursor;
                if frame.inputs.statusline_rows > 0 {
                    refresh_statusline(&mut frame.surface, model, frame.inputs.grid.0);
                }
                refresh_speculated(
                    &mut frame.surface,
                    model,
                    frame.inputs.grid,
                    frame.inputs.offset,
                );
            }
        } else {
            self.frame = None;
            self.rebuilds = self.rebuilds.wrapping_add(1);
        }
        let frame = self.frame.get_or_insert_with(|| Frame::rebuild(model));
        #[cfg(debug_assertions)]
        assert_equivalent(self.frames, &frame.surface, model);
        &frame.surface
    }
}

/// Re-resolves the statusline layer's view in place. The bar's segments
/// (ruler, showcmd, search count) track typing, so a reused frame must
/// re-ask the state for its view; writing it back only on change keeps an
/// idle bar from dirtying the cached layer at all.
fn refresh_statusline(surface: &mut Surface, model: &Model, grid_w: u16) {
    for layer in &mut surface.layers {
        if let crate::LayerKind::Statusline(view) = &mut layer.kind {
            let fresh = model.engine.statusline.view(grid_w);
            if *view != fresh {
                *view = fresh;
            }
            return;
        }
    }
}

/// Re-resolves the speculated layer in place: adds it when a prediction is
/// now pending, drops it when the last one is gone, replaces its cells when
/// they moved.
///
/// Predictions turn over on every keystroke of a typing burst, so tracking
/// them in [`Inputs`] instead would rebuild the whole frame on exactly the
/// frames speculation exists to make faster. Reconciling in place keeps that
/// frame at reuse cost, and the equivalence guard below is what proves the
/// reconciled frame is the frame [`render`] would have built.
fn refresh_speculated(surface: &mut Surface, model: &Model, grid: (u16, u16), offset: u16) {
    let fresh = speculated_layer(model, grid, offset);
    let at = surface
        .layers
        .iter()
        .position(|layer| matches!(layer.kind, LayerKind::Speculated(_)));
    match (at, fresh) {
        (Some(at), Some(layer)) => {
            if let Some(cached) = surface.layers.get_mut(at) {
                if *cached != layer {
                    *cached = layer;
                }
            }
        }
        (Some(at), None) => {
            surface.layers.remove(at);
        }
        // render()'s own index, not one derived from this frame: an insert
        // after a chrome layer would paint the prediction over chrome that
        // render() puts on top of it.
        (None, Some(layer)) => surface.layers.insert(SPECULATED_LAYER_INDEX, layer),
        (None, None) => {}
    }
}

/// Asserts `produced` equals a from-scratch [`render`] of `model`, naming
/// the frame and the first divergent component. Debug builds only: the
/// point is that a stale reused layer fails loudly in every test, oracle,
/// and pty run instead of shipping as quiet drift.
#[cfg(debug_assertions)]
fn assert_equivalent(frame: u64, produced: &Surface, model: &Model) {
    let full = render(model);
    if *produced == full {
        return;
    }
    debug_assert!(
        false,
        "cached surface diverged from a from-scratch rebuild at frame {frame}: {}",
        divergence_detail(produced, &full)
    );
}

/// The first component where `got` and `want` disagree, kept to rects and
/// kind names rather than full layer payloads so the failure message stays
/// readable.
#[cfg(debug_assertions)]
fn divergence_detail(got: &Surface, want: &Surface) -> String {
    if got.cursor != want.cursor {
        return format!("cursor {:?} != {:?}", got.cursor, want.cursor);
    }
    if got.layers.len() != want.layers.len() {
        return format!(
            "{} layers != {} layers",
            got.layers.len(),
            want.layers.len()
        );
    }
    for (i, (g, w)) in got.layers.iter().zip(&want.layers).enumerate() {
        if g != w {
            return format!(
                "layer {i}: {} at {:?} != {} at {:?}",
                kind_name(&g.kind),
                g.rect,
                kind_name(&w.kind),
                w.rect
            );
        }
    }
    "surfaces differ outside layers and cursor".to_string()
}

#[cfg(debug_assertions)]
fn kind_name(kind: &crate::LayerKind) -> &'static str {
    match kind {
        crate::LayerKind::EngineGrid => "EngineGrid",
        crate::LayerKind::Cmdline(_) => "Cmdline",
        crate::LayerKind::Messages(_) => "Messages",
        crate::LayerKind::Tabline(_) => "Tabline",
        crate::LayerKind::Popupmenu(_) => "Popupmenu",
        crate::LayerKind::Shell => "Shell",
        crate::LayerKind::Picker(_) => "Picker",
        crate::LayerKind::Tree(_) => "Tree",
        crate::LayerKind::Statusline(_) => "Statusline",
        crate::LayerKind::Prompt(_) => "Prompt",
        crate::LayerKind::Palette(_) => "Palette",
        crate::LayerKind::Speculated(_) => "Speculated",
        crate::LayerKind::Ai(_) => "Ai",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::tests::predict;
    use crate::{CursorSpec, LayerKind};
    use view_core::events::UiEvent;
    use view_core::grid::GridOp;
    use view_core::msg::Msg;
    use view_core::native::speculate::{SpecStamp, SPECULATION_MAX_AGE};
    use view_core::update::update;

    fn model_with_grid(width: u16, height: u16) -> Model {
        let mut model = Model::new();
        model.term_width = width;
        model.term_height = height;
        model.engine.apply_grid(GridOp::Resize { width, height });
        model
    }

    /// Non-exhaustive `view-core` state structs cannot be built with
    /// struct-literal syntax from outside their defining crate, so tests
    /// drive them through the same `update()` path production code uses.
    fn apply(model: &mut Model, ev: UiEvent) {
        let _ = update(model, Msg::Redraw(vec![ev]));
    }

    #[test]
    fn a_grid_edit_reuses_the_cached_frame_and_tracks_the_cursor() {
        let mut model = model_with_grid(20, 6);
        let mut cache = SurfaceCache::new();
        let _ = cache.render(&model);

        model.engine.apply_grid(GridOp::PutLine {
            row: 1,
            col_start: 0,
            cells: vec![("x".into(), 0, 1)],
        });
        model
            .engine
            .apply_grid(GridOp::CursorGoto { row: 1, col: 1 });
        let surface = cache.render(&model);

        assert_eq!(
            surface.cursor,
            Some(CursorSpec {
                row: 1,
                col: 1,
                shape: crate::CursorShape::Block,
            })
        );
        assert_eq!(
            (cache.frames, cache.rebuilds),
            (2, 1),
            "a grid-content edit must reuse the cached frame, not rebuild it"
        );
    }

    #[test]
    fn a_cmdline_change_rebuilds_the_frame() {
        let mut model = model_with_grid(20, 6);
        let mut cache = SurfaceCache::new();
        let _ = cache.render(&model);

        apply(
            &mut model,
            UiEvent::CmdlineShow {
                content: vec![(0, "q".to_string())],
                pos: 1,
                firstc: ":".to_string(),
                prompt: String::new(),
                indent: 0,
                level: 1,
            },
        );
        let surface = cache.render(&model);

        assert!(surface
            .layers
            .iter()
            .any(|l| matches!(l.kind, LayerKind::Cmdline(_))));
        assert_eq!((cache.frames, cache.rebuilds), (2, 2));
    }

    #[test]
    fn a_new_message_rebuilds_the_frame() {
        let mut model = model_with_grid(20, 6);
        let mut cache = SurfaceCache::new();
        let _ = cache.render(&model);

        apply(
            &mut model,
            UiEvent::MsgShow {
                kind: "echo".into(),
                content: vec![(0, "written".into())],
                replace_last: false,
            },
        );
        let surface = cache.render(&model);

        assert!(surface
            .layers
            .iter()
            .any(|l| matches!(l.kind, LayerKind::Messages(_))));
        assert_eq!((cache.frames, cache.rebuilds), (2, 2));
    }

    #[test]
    fn a_statusline_update_refreshes_in_place_without_a_rebuild() {
        use view_core::native::statusline::SegmentUpdate;

        let mut model = model_with_grid(20, 6);
        model.statusline_enabled = true;
        let mut cache = SurfaceCache::new();
        let _ = cache.render(&model);

        model
            .engine
            .statusline
            .apply(SegmentUpdate::Ruler("3,7".to_string()));
        let surface = cache.render(&model);

        let view = surface
            .layers
            .iter()
            .find_map(|l| match &l.kind {
                LayerKind::Statusline(view) => Some(view),
                _ => None,
            })
            .expect("the statusline feature is on, so its layer must exist");
        assert!(
            view.right.iter().any(|span| span.text == "3,7"),
            "the reused frame must carry the fresh ruler text"
        );
        assert_eq!(
            (cache.frames, cache.rebuilds),
            (2, 1),
            "a statusline segment change must refresh in place, not rebuild"
        );
    }

    #[test]
    fn an_open_overlay_forces_a_rebuild_every_frame() {
        use view_core::native::geometry::{Anchor, OverlayBox};
        use view_core::native::tree::TreeState;

        let mut model = model_with_grid(40, 12);
        let mut cache = SurfaceCache::new();
        let _ = cache.render(&model);

        model.push_overlay(
            OverlayBox::new(30, 100).with_anchor(Anchor::Left),
            view_core::model::OverlayKind::Tree(TreeState::open(std::path::PathBuf::from(
                "/tmp/example",
            ))),
        );
        let _ = cache.render(&model);
        let _ = cache.render(&model);

        assert_eq!(
            (cache.frames, cache.rebuilds),
            (3, 3),
            "frames with an open overlay must rebuild from scratch every time"
        );
    }

    /// The equivalence guard must be seen to catch: a guard that has never
    /// fired proves nothing. Corrupts the cached frame directly (the only
    /// way to diverge without a real bug) and expects the debug assert,
    /// which names the frame, to refuse the reused surface.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "diverged from a from-scratch rebuild")]
    fn a_corrupted_cached_frame_trips_the_equivalence_guard() {
        let model = model_with_grid(20, 6);
        let mut cache = SurfaceCache::new();
        let _ = cache.render(&model);

        if let Some(frame) = cache.frame.as_mut() {
            frame.surface.layers[0].rect.width -= 1;
        }
        let _ = cache.render(&model);
    }

    fn speculated(surface: &Surface) -> Option<(usize, Vec<char>)> {
        surface
            .layers
            .iter()
            .enumerate()
            .find_map(|(i, l)| match &l.kind {
                LayerKind::Speculated(cells) => Some((i, cells.iter().map(|c| c.glyph).collect())),
                _ => None,
            })
    }

    /// The frame speculation exists for: a keystroke predicted while nothing
    /// else about the frame changed. The prediction must reach the surface
    /// without paying for a from-scratch rebuild, which is what tracking it
    /// as a cache input would have cost on every keystroke of a burst.
    #[test]
    fn a_prediction_reaches_the_reused_frame_without_a_rebuild() {
        let mut model = model_with_grid(20, 6);
        let mut cache = SurfaceCache::new();
        let _ = cache.render(&model);

        predict(&mut model, 'a', (1, 1), 0);
        assert_eq!(speculated(cache.render(&model)), Some((1, vec!['a'])));

        predict(&mut model, 'b', (1, 1), 5);
        assert_eq!(
            speculated(cache.render(&model)),
            Some((1, vec!['a', 'b'])),
            "the layer's cells track pending, and it stays directly above the engine grid"
        );
        assert_eq!(
            (cache.frames, cache.rebuilds),
            (3, 1),
            "only the first frame may rebuild; a prediction refreshes in place"
        );
    }

    /// The other direction, which a refresh that could only add would leave
    /// on screen forever: the last prediction going away takes the layer
    /// with it, on a reused frame.
    #[test]
    fn the_last_prediction_expiring_drops_the_layer_from_the_reused_frame() {
        let mut model = model_with_grid(20, 6);
        let mut cache = SurfaceCache::new();
        predict(&mut model, 'a', (1, 1), 0);
        assert!(speculated(cache.render(&model)).is_some());

        model
            .speculate
            .expire_stale(SpecStamp::new(SPECULATION_MAX_AGE));
        assert_eq!(speculated(cache.render(&model)), None);
        assert_eq!(
            (cache.frames, cache.rebuilds),
            (2, 1),
            "dropping the layer is a refresh too, not a rebuild"
        );
    }

    /// The equivalence guard is what makes the in-place refresh above safe
    /// to have at all, so it must be seen to still fire on a frame with
    /// predictions pending -- the frames where the reuse path now does the
    /// most work.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "diverged from a from-scratch rebuild")]
    fn a_corrupted_frame_with_predictions_pending_still_trips_the_guard() {
        let mut model = model_with_grid(20, 6);
        let mut cache = SurfaceCache::new();
        predict(&mut model, 'a', (1, 1), 0);
        let _ = cache.render(&model);

        if let Some(frame) = cache.frame.as_mut() {
            frame.surface.layers[0].rect.width -= 1;
        }
        predict(&mut model, 'b', (1, 1), 5);
        let _ = cache.render(&model);
    }
}
