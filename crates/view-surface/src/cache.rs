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

use crate::{
    cursor_spec, grid_origin, render, speculated_layer, LayerKind, Surface, SPECULATED_LAYER_INDEX,
};

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
///
/// A field of `Model` or `EngineModel` that reaches no layer is named here
/// instead, and `every_model_field_is_a_paint_input_or_named_here` fails on
/// one that appears in neither place -- a new field silently absent from
/// this snapshot is a frame reused after the thing it draws has changed:
///
/// - not state at all: `dirty`, `running`, `fatal_reason`, `config_was_read`,
///   `checktime_generation`, `pending_file_gone_probes`, `speculate`,
///   `submit_hold` (input kept from routing reaches a layer only once it
///   is replayed),
///   `supervision`, `claimed_keys`, `key_bindings`, `key_profile_override`,
///   `key_profile_report_requested`
///   (`update()` only records a `:View keys profile` flip or bare report
///   request here; `NativeSession` is what reissues the registration or
///   reports, and no layer reads which profile is live), `cwd`, `colorscheme`,
///   `detected_look` (what `panes = "auto"` answered and the variable that
///   decided it, read by the `:View ui panes` notice and the config report),
///   `mouse_capture`, `mouse_on`, `colon_mapped` (it gates whether a `:` is
///   speculated at all, and `cmdline_speculated` is the state that reaches
///   a layer), `pending_chord` (it closes the same gate, and holds
///   nothing any layer draws), `resize_mode` (its word reaches the frames
///   through `statusline`), `ai_fs` (an agent's file request reaches
///   the screen only as a prompt in the panel, which is an overlay),
///   `key_unanswered`, `key_round_trips`, `key_round_trips_at` and
///   `literal_pending`
///   (the gate's own terms and the link reading that bounds a guess, read
///   only when a `:` is folded or a batch arrives), `next_overlay_id`,
///   `attached` (it decides
///   only when the UI goes on, and the frame that follows is what flips
///   `chrome_painted`, which is here), `stdin_relay` (an attach option
///   the session was started with), `tabline_follows_look` (it decides
///   whether a look flip moves `ext_surfaces`, and the move itself reaches
///   a layer through `offset`), `fit_active` (it decides only whether a
///   `VimEnter` or a look flip sends nvim the fit hook, and the resize
///   that follows reaches a layer through `grids`) and
///   `native_min_pane_size` (`window zoom`
///   reads it to choose which notation to send, and neither the choice
///   nor the option it is read from changes what any layer paints)
/// - read through a field already here: `engine` (this destructures it),
///   `grids` (via `grid`, which is the global grid's size, and via
///   `notice_column`, the one layer its panes, floats and cursor move; the
///   compositor paints the panes themselves off the `Model`),
///   `held` (the same way: `grid` is the painted grid's size, and the
///   panes, the slots and the highlight table held with them are painted
///   off the `Model`), `notice_held` (via `notice_column`, which resolves
///   the held column against the anchor of the moment),
///   `surface_conflicts` (a conflict reaches the screen as a notice on
///   `messages`), `cmdline_floats` (via `listed`, the one of them the
///   palette paints),
///   `hl` and `mode` (painters read them off the
///   `Model` on the reuse path), `window_status` (the tile segments are
///   painted off the `Model` the same way, and the row each one stands on
///   is marked changed where the status changes), `tile_titles` (painted
///   off the `Model` beside `window_status`, and set once at startup),
///   `overlays` (via
///   `had_overlays`),
///   `statusline` (via `statusline_rows`), `toast_history` (only the
///   palette's history view reads it, and that is an overlay),
///   `key_log` (only the key log overlay reads it),
///   `showtabline` (it decides whether the top row exists under
///   `panes = "nvim"`, which is `offset`), `surfaces` (it decides whether
///   the tree draws as a float or in a pane, and both are painted off the
///   `Model` while an overlay is open, which is every frame it is on
///   screen)
/// - a setting no layer's geometry follows on its own:
///   `ai_panel_width_pct`,
///   `ai_review_open_target`, `tree_width_pct`, `ext_surfaces`,
///   `statusline_enabled`, `tree_icons` -- each one only reaches a layer
///   through an open overlay, and an open overlay rebuilds
/// - read through a field already here, second list: `ai_panel`,
///   `ai_trusted` and `ai_enabled` reach the pill's one word, which
///   `agent` carries, and `tabline_shows` decides only which of `tabline`
///   and `buffers` the pill names, both of which are here
#[derive(Debug)]
struct Inputs {
    grid: (u16, u16),
    offset: u16,
    // the window look: it decides the outer grid's placement and whether a
    // frame is drawn at all, and a flip that keeps the grid the same size
    // (one look mode to another at the same ring) reaches no other field
    look: view_core::model::Look,
    term: (u16, u16),
    chrome_painted: bool,
    palette_enabled: bool,
    // the whole capability struct, not the tier alone: the border charset
    // follows `unicode_boxes`, which a probe reply arriving after the first
    // paint flips without moving the tier -- a frame keyed on the tier
    // would then be reused with the charset the session no longer draws in,
    // and every later probed bit joins this comparison for free
    caps: view_core::model::TermCaps,
    statusline_rows: u16,
    had_overlays: bool,
    tabline: Option<view_core::model::TablineState>,
    // the pill's two model-side inputs: the names it draws, and the one
    // word the agent's state resolves to. The state itself is four fields
    // on three structs, and a frame keyed on them would compare a panel
    // transcript to decide whether one word moved
    buffers: Vec<view_core::model::BufferEntry>,
    remote: Option<String>,
    agent: &'static str,
    // `[ui] pill_caps` as forced; `"auto"` follows `caps`, which is here
    pill_caps: Option<view_core::native::pill::PillCaps>,
    cmdline: Option<view_core::model::CmdlineState>,
    // presence alone: what the speculated palette draws is one fixed state
    // (`CmdlineState::bare_colon`), so the only thing a frame can differ by
    // is whether one is up
    cmdline_speculated: bool,
    popupmenu: Option<view_core::model::PopupmenuState>,
    // the grid the palette lists, its revision and its border: its cells
    // inside the border are copied into the palette layer, and the
    // revision moves on every write to them, so nothing here compares a
    // cell
    listed: Option<(view_core::grid::registry::GridId, u64, [u16; 4])>,
    // the whole stack, not its `entries` alone: the pause key changes no
    // entry, only whether the top box carries the mark that says the stack
    // is frozen, and a frame keyed on a projection of a painted struct hands
    // back the frame from before whatever the projection dropped. Compared
    // without the clock the runtime sets before every fold, which moves on
    // every key and would rebuild every frame
    messages: view_core::model::Messages,
    // the frame's only free-running input, and the reason it cannot be
    // inferred from `messages`: a motion frame moves the same entries to
    // different rows, so a cache keyed on the stack's contents alone would
    // hand back the frame before the one that just advanced
    toast_motion: Option<view_core::native::toast::ToastMotion>,
    // where the stack is drawn follows panes, floats, the cursor row and
    // the hunk under review, none of which the fields above compare
    notice_column: Option<view_core::model::NoticeColumn>,
}

/// The pill's agent word for `model`, or the empty string where the pill
/// draws none. Read off `model.ai_panel`, `model.ai_trusted` and
/// `model.ai_enabled`, the three fields it resolves.
fn agent_of(model: &Model) -> &'static str {
    view_core::native::pill::agent_word(model.ai_panel(), model.ai_enabled, model.ai_trusted)
}

/// The grid the palette lists, that grid's revision and the border nvim
/// reported around it, or `None` while it lists none. Read off
/// `model.cmdline_floats`.
fn listed_of(model: &Model) -> Option<(view_core::grid::registry::GridId, u64, [u16; 4])> {
    let grid = view_core::native::palette::listed_grid(model)?;
    let revision = model.engine.painted_grids().grid(grid)?.revision();
    Some((grid, revision, model.cmdline_floats.margins(grid)))
}

impl Inputs {
    fn capture(model: &Model) -> Self {
        let engine = &model.engine;
        Self {
            grid: engine.painted_grid().size(),
            offset: model.chrome_rows(),
            look: model.look,
            term: (model.term_width, model.term_height),
            chrome_painted: model.chrome_painted,
            palette_enabled: model.palette_enabled,
            caps: model.caps,
            statusline_rows: model.statusline_rows(),
            had_overlays: !model.overlays().is_empty(),
            tabline: engine.tabline.clone(),
            buffers: model.buffers.clone(),
            remote: model.remote.clone(),
            pill_caps: model.pill_caps,
            agent: agent_of(model),
            cmdline: engine.cmdline.clone(),
            cmdline_speculated: engine.cmdline_speculated.is_some(),
            popupmenu: engine.popupmenu.clone(),
            listed: listed_of(model),
            messages: engine.messages.clone(),
            toast_motion: model.toast_motion.clone(),
            notice_column: crate::live_notice_column(model),
        }
    }

    /// Whether `model` would produce the same layers this snapshot did.
    /// Comparison only, with no clone: on the steady-typing frame every
    /// field is a scalar compare or an `is_none` pair. While a notice is
    /// up, the notice column is placed afresh, which walks the panes into
    /// a few short vectors and reads each notice's wrap from its cache.
    fn matches(&self, model: &Model) -> bool {
        let engine = &model.engine;
        !self.had_overlays
            && model.overlays().is_empty()
            && self.grid == engine.painted_grid().size()
            && self.offset == model.chrome_rows()
            && self.look == model.look
            && self.term == (model.term_width, model.term_height)
            && self.chrome_painted == model.chrome_painted
            && self.palette_enabled == model.palette_enabled
            && self.caps == model.caps
            && self.statusline_rows == model.statusline_rows()
            && self.tabline == engine.tabline
            && self.buffers == model.buffers
            && self.remote == model.remote
            && self.pill_caps == model.pill_caps
            && self.agent == agent_of(model)
            && self.cmdline == engine.cmdline
            && self.cmdline_speculated == engine.cmdline_speculated.is_some()
            && self.popupmenu == engine.popupmenu
            && self.listed == listed_of(model)
            && self.messages.same_stack(&engine.messages)
            && self.toast_motion == model.toast_motion
            && self.notice_column == crate::live_notice_column(model)
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
                let origin = grid_origin(model);
                let cursor = cursor_spec(model, origin, &frame.surface.layers);
                frame.surface.cursor = cursor;
                if frame.inputs.statusline_rows > 0 {
                    // the width the frame's own layers were built at, which
                    // is not the engine's while the grid is withheld
                    refresh_statusline(&mut frame.surface, model, crate::statusline_width(model));
                }
                refresh_speculated(&mut frame.surface, model, origin);
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
fn refresh_speculated(surface: &mut Surface, model: &Model, origin: (u16, u16)) {
    let fresh = speculated_layer(model, origin);
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
            // a rect and a kind name are all this prints, so two layers
            // that differ only in what they carry would otherwise read as
            // "X != X" -- which is the shape of the one defect this guard
            // exists to catch, a render input `Inputs` does not compare
            let same_place = kind_name(&g.kind) == kind_name(&w.kind) && g.rect == w.rect;
            let payload = if same_place {
                " (same kind and rect: the layer's own content differs, \
                 which is an input `Inputs` does not compare)"
            } else {
                ""
            };
            return format!(
                "layer {i}: {} at {:?} != {} at {:?}{payload}",
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
        crate::LayerKind::Toast { .. } => "Toast",
        crate::LayerKind::Popupmenu(_) => "Popupmenu",
        crate::LayerKind::Shell => "Shell",
        crate::LayerKind::Picker(_) => "Picker",
        crate::LayerKind::Tree(_) => "Tree",
        crate::LayerKind::Statusline(_) => "Statusline",
        crate::LayerKind::Prompt(_) => "Prompt",
        crate::LayerKind::Palette(_) => "Palette",
        crate::LayerKind::Stream(_) => "Stream",
        crate::LayerKind::Speculated(_) => "Speculated",
        crate::LayerKind::Ai(_) => "Ai",
        crate::LayerKind::Pill(_) => "Pill",
        crate::LayerKind::Gutter => "Gutter",
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

    /// Every field `header` declares, by name.
    fn declared_fields(source: &str, header: &str) -> Vec<String> {
        assert!(source.contains(header), "{header} is no longer declared");
        let body = source
            .split_once(header)
            .expect("just found above")
            .1
            .split_once("\n}")
            .expect("the struct is never closed")
            .0;
        let names: Vec<String> = body
            .lines()
            .filter_map(|line| {
                let declaration = line.trim();
                let declaration = match declaration.strip_prefix("pub") {
                    Some(rest) if rest.starts_with(' ') => rest.trim_start(),
                    Some(rest) if rest.starts_with('(') => rest.split_once(") ")?.1,
                    _ => declaration,
                };
                let (name, _) = declaration.split_once(':')?;
                (!name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
                .then(|| name.to_string())
            })
            .collect();
        assert!(!names.is_empty(), "{header} parsed to no fields at all");
        names
    }

    #[test]
    fn the_field_walk_reads_a_field_of_every_visibility() {
        let source = "pub struct S {\n    pub a: u8,\n    pub(crate) b: u8,\n    \
                      pub(super) c: u8,\n    pub(in crate::x) d: u8,\n    e: u8,\n}";
        assert_eq!(
            declared_fields(source, "pub struct S {"),
            ["a", "b", "c", "d", "e"]
        );
    }

    /// The class pin behind `Inputs`: a paint-relevant field added to the
    /// model and forgotten here reuses a frame that no longer draws it, and
    /// nothing fails -- the pause mark is the shipped case.
    /// Every field is either captured or classified in the doc above
    /// `Inputs`, and there is no third answer.
    #[test]
    fn every_model_field_is_a_paint_input_or_named_here() {
        let cache = include_str!("cache.rs");
        let model = include_str!("../../view-core/src/model.rs");
        let capture = cache
            .split_once("struct Inputs {")
            .expect("Inputs is no longer declared")
            .1;
        let classified = cache
            .split_once("#[derive(Debug)]\nstruct Inputs {")
            .expect("Inputs is no longer declared")
            .0;

        let mut missing = Vec::new();
        for (header, owner) in [
            ("pub struct Model {", "Model"),
            ("pub struct EngineModel {", "EngineModel"),
        ] {
            for field in declared_fields(model, header) {
                if !capture.contains(&format!(".{field}"))
                    && !classified.contains(&format!("`{field}`"))
                {
                    missing.push(format!("{owner}::{field}"));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "these model fields are neither captured by `Inputs` nor named in \
             its doc as reaching no layer:\n  {}\nCapture the field if a \
             painter reads it, or add it to the classification above `Inputs` \
             saying why it cannot change a frame",
            missing.join("\n  ")
        );
    }

    /// The byte range of the brace-delimited block `anchor` opens (the
    /// `{`..`}` that follows it), found by depth-counting from the first `{`
    /// after the anchor. Good enough for this crate's plain-Rust bodies
    /// (none of the blocks read below hold a brace inside a string
    /// literal); `None` when the crate no longer carries `anchor`.
    fn block_after(source: &str, anchor: &str) -> Option<std::ops::Range<usize>> {
        let sig_at = source.find(anchor)?;
        let open = sig_at + source[sig_at..].find('{')?;
        let mut depth = 0usize;
        for (offset, ch) in source[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(open..open + offset + 1);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// A painter reads the grid the screen shows, which during a restart
    /// is the dead engine's last frame. A read of the live registry paints
    /// the replacement's cleared grid over it, and nothing fails until a
    /// person sees the blank screen. The held cells carry the dead engine's
    /// highlight ids, which the replacement's first batch redefines, so
    /// they are drawn with the table held beside them.
    ///
    /// view-core's model sources are walked too: the notice column the
    /// compositor paints every frame places its boxes from them, and a
    /// live read there sets a toast over the held tree or cursor line.
    /// So is view-core's `native/`, whose views, geometry, statusline,
    /// speculation and supervision the painters call every frame.
    /// A live read a painter reaches that paints nothing is named in
    /// [`LIVE_READS`] with its grounds.
    ///
    /// Any mention of the `window_status` field is refused, since a loop
    /// over it or the whole map handed to a helper reads it as surely as
    /// a lookup does. The field's own declaration and the module sharing
    /// its name are named in [`DECLARATIONS`]. Comment lines are read as
    /// empty, since no frame is painted from one.
    #[test]
    fn every_painter_reads_the_painted_registry() {
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut dirs = vec![
            crates.join("view-surface/src"),
            crates.join("view-tui/src"),
            crates.join("view-core/src/model"),
            crates.join("view-core/src/native"),
        ];
        let mut files = vec![crates.join("view-core/src/model.rs")];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir)
                .expect("a paint crate's src/")
                .flatten()
            {
                let path = entry.path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension() == Some("rs".as_ref())
                    && path.file_name() != Some("tests.rs".as_ref())
                {
                    files.push(path);
                }
            }
        }
        let mut live = Vec::new();
        for path in &files {
            let source = std::fs::read_to_string(path).expect("a listed source");
            // a doc line keeps its `///`, which ends a function's body below
            let mut flat: String = production(&source)
                .lines()
                .map(|line| match line.trim_start() {
                    doc if doc.starts_with("///") => "///",
                    comment if comment.starts_with("//") => "",
                    _ => line,
                })
                .flat_map(str::chars)
                .filter(|c| !c.is_whitespace())
                .collect();
            for (file, declaration, _grounds) in DECLARATIONS {
                if !path.ends_with(file) {
                    continue;
                }
                if !flat.contains(declaration) {
                    live.push(format!("{file} no longer spells `{declaration}`"));
                }
                flat = flat.replace(declaration, "");
            }
            for (file, signature, read, _grounds) in LIVE_READS {
                if !path.ends_with(file) {
                    continue;
                }
                let Some(at) = flat.find(signature) else {
                    live.push(format!("{file} no longer defines `{signature}`"));
                    continue;
                };
                let body = at + signature.len();
                let end = ["///", "#[", "pubfn", "pub(crate)fn"]
                    .iter()
                    .filter_map(|marker| flat[body..].find(marker))
                    .min()
                    .map_or(flat.len(), |offset| body + offset);
                let Some(hit) = flat[body..end].find(read) else {
                    live.push(format!("`{signature}` in {file} no longer reads {read}"));
                    continue;
                };
                flat.replace_range(body + hit..body + hit + read.len(), "");
            }
            for read in [
                "engine.grids()",
                "engine.grid()",
                "engine.hl()",
                "window_status",
            ] {
                if flat.contains(read) {
                    live.push(format!("{} reads {read}", path.display()));
                }
            }
        }
        assert!(files.len() > 10, "the walk reached {} sources", files.len());
        assert!(
            live.is_empty(),
            "paint through `painted_grids()` / `painted_grid()` / `painted_hl()` / \
             `painted_status()`:\n  {}",
            live.join("\n  ")
        );
    }

    /// Live registry reads a painter reaches that paint nothing: the file,
    /// the function's signature with its whitespace taken out, the read,
    /// and the grounds.
    const LIVE_READS: &[(&str, &str, &str, &str)] = &[
        (
            "view-core/src/model.rs",
            "pubfnfocus(&self)->Focus{",
            "engine.grids()",
            "`Model::focus` routes input, so it reads the live registry: while a \
             frame is held, keys go to the replacement, which has no pane yet",
        ),
        (
            "view-core/src/model/held.rs",
            PAINTED_STATUS,
            "window_status.get(",
            "`Model::painted_status` is the painted read: with nothing held, the \
             painted registry is the live one",
        ),
        (
            "view-core/src/model/held.rs",
            PAINTED_STATUS,
            "window_status.get(",
            "`Model::painted_status` is the painted read: a held layout's slot a \
             live window fills takes that window's report once it arrives",
        ),
        (
            "view-core/src/model/held.rs",
            FORGET_ENGINE_WINDOWS,
            "window_status",
            "`Model::forget_engine_windows` hands the dead engine's statuses to \
             the hold as a restart begins, which is how the held frame gets them",
        ),
        (
            "view-core/src/model/held.rs",
            FORGET_ENGINE_WINDOWS,
            "window_status",
            "`Model::forget_engine_windows` clears the live statuses once the \
             hold has them, since the replacement reuses their handles",
        ),
        (
            "view-core/src/model/held.rs",
            "pub(crate)fnsettle_held(&mutself,at_flush:bool)->Vec<Effect>{",
            "window_status",
            "`Model::settle_held` runs on a flush and checks the replacement's \
             reports against the held layout, painting nothing itself",
        ),
        (
            "view-core/src/native/surfaces.rs",
            "fnlanding(row:i64,col:i64,width:u16,height:u16,anchor:FloatAnchor,\
             model:&Model,)->Option<(bool,bool)>{",
            "engine.grid()",
            "`landing` places a float nvim just sent against the grid that \
             placement lands on, for `claims_at`, and paints nothing",
        ),
        (
            "view-core/src/native/speculate.rs",
            "fnfold_cmdline_key(model:&mutModel,notation:&str,now:SpecStamp){",
            "engine.grids()",
            "`fold_cmdline_key` folds a key on its way to nvim and records the \
             grid the key was typed on, which is the live engine's",
        ),
        (
            "view-core/src/native/speculate.rs",
            "fnfold_cmdline_batch(model:&mutModel,redraw:&[UiEvent],now:SpecStamp)\
             ->Vec<Effect>{",
            "engine.grids()",
            "`fold_cmdline_batch` judges a redraw batch against the live engine \
             that sent it, and paints nothing",
        ),
        (
            "view-core/src/native/speculate.rs",
            "fnfold_keystroke(model:&mutModel,notation:&str,now:SpecStamp){",
            "engine.grids()",
            "`fold_keystroke` predicts a key's glyph from the live engine's cursor, \
             which is where nvim will draw it",
        ),
        (
            "view-core/src/native/submit_hold/refused.rs",
            "pub(super)fnreports_error(model:&Model,events:&[UiEvent])->bool{",
            "engine.grids()",
            "`reports_error` reads a redraw batch for an error the live engine \
             drew into its own message grid, and the column 0 that grid \
             already holds, and paints nothing",
        ),
        (
            "view-core/src/native/submit_hold/refused.rs",
            "fnerror_attr<'a>(model:&'aModel,events:&[UiEvent])->Option<ErrorAttr<'a>>{",
            "engine.hl()",
            "`error_attr` reads how the live engine draws an error, to compare \
             the cells it drew with, and paints nothing",
        ),
    ];

    /// `Model::forget_engine_windows`'s signature with its whitespace
    /// taken out.
    const FORGET_ENGINE_WINDOWS: &str = "pubfnforget_engine_windows(&mutself){";

    /// Spellings of `window_status` that read nothing: the file, the
    /// spelling with its whitespace taken out, and the grounds.
    const DECLARATIONS: &[(&str, &str, &str)] = &[
        (
            "view-core/src/model.rs",
            "pubwindow_status:std::collections::HashMap<crate::events::WinHandle,WindowStatus>,",
            "the field's declaration on `Model`",
        ),
        (
            "view-core/src/model.rs",
            "window_status:std::collections::HashMap::new(),",
            "`Model::new` starting the field empty",
        ),
        (
            "view-core/src/model.rs",
            "modwindow_status;",
            "the module that defines `WindowStatus`, which shares the field's name",
        ),
        (
            "view-core/src/model.rs",
            "pubusewindow_status::{",
            "the module's re-exports",
        ),
    ];

    /// `Model::painted_status`'s signature with its whitespace taken out.
    const PAINTED_STATUS: &str = "pubfnpainted_status(&self,win:WinHandle)->Option<&WindowStatus>{";

    /// Everything in `source` ahead of its test module.
    ///
    /// The boundary is the module, not the first `#[cfg(test)]`: `lib.rs`
    /// carries one on an import, and cutting there would hide the two pane
    /// reads the walk below exists to classify. Test code is out of scope
    /// either way -- no cached frame is served from it.
    fn production(source: &str) -> &str {
        [
            "#[cfg(test)]\nmod tests",
            "#[cfg(test)]\npub(crate) mod tests",
        ]
        .iter()
        .find_map(|boundary| source.split_once(boundary))
        .map_or(source, |(prod, _)| prod)
    }

    /// The half the classification above cannot carry on its own: `grids`
    /// is classified there as reaching no layer, which is true only while
    /// every painter in this crate draws the global grid alone. The day one
    /// reads the pane list, that sentence becomes the projection this
    /// cache's whole class of misses is made of -- a frame keyed on one
    /// grid's size, reused after a window moved.
    ///
    /// One population reads pane geometry without `Inputs` capturing it and
    /// stays correct anyway: the functions [`SurfaceCache::render`]'s
    /// cache-hit branch reaches, which re-run from scratch on *every* frame
    /// this cache returns, hit or miss -- there is no stale copy for them
    /// to serve, so nothing about a moved or hidden pane can outlive one
    /// frame. A reader anywhere else in the crate sits on the
    /// cached-and-reused path instead, where the same read would go stale
    /// the moment a window moves, so it still owes `Inputs` capture.
    ///
    /// `RESOLVED_EVERY_FRAME` names that population, and the first half of
    /// this test is what makes the name a check rather than a claim: each
    /// entry must be reachable by call from the hit branch, so an entry
    /// that stopped being invoked per frame -- or was added on a hope --
    /// fails here by name instead of quietly excusing a cached reader.
    #[test]
    fn a_render_that_reads_panes_must_key_the_cache_on_them() {
        const RESOLVED_EVERY_FRAME: &[&str] = &[
            "cursor_spec",
            "pane_cursor",
            "refresh_statusline",
            "refresh_speculated",
            "speculated_layer",
            "speculated_col",
        ];

        let cache = include_str!("cache.rs");
        let keyed = cache
            .split_once("struct Inputs {")
            .expect("Inputs is no longer declared")
            .1
            .split_once("impl SurfaceCache")
            .expect("Inputs' capture and comparison are no longer declared")
            .0;
        let holds_panes = ["panes_in_z_order", "grids()"]
            .iter()
            .any(|reader| keyed.contains(reader));

        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        // a plain read_dir misses module subdirectories (overlay/ already
        // exists), and a render source added there must not escape the walk
        let mut dirs = vec![src.clone()];
        let mut sources: Vec<(String, String)> = Vec::new();
        while let Some(dir) = dirs.pop() {
            let listing = std::fs::read_dir(&dir).expect("this crate's own src/ must be readable");
            for entry in listing.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    dirs.push(path);
                    continue;
                }
                if path.extension() != Some("rs".as_ref()) {
                    continue;
                }
                let source =
                    std::fs::read_to_string(&path).expect("a listed source must be readable");
                sources.push((path.display().to_string(), production(&source).to_string()));
            }
        }
        assert!(
            !sources.is_empty(),
            "the walk reached no source under {}, so it proves nothing about \
             what this crate paints from",
            src.display()
        );

        // every function the hit branch reaches by call, closed over the
        // bodies it reaches through -- `speculated_layer` and
        // `speculated_col` are reached this way, one and two calls deep
        let hit = block_after(
            production(cache),
            ".is_some_and(|f| f.inputs.matches(model))",
        )
        .expect("`SurfaceCache::render`'s cache-hit branch is no longer declared");
        let mut reached: Vec<String> = vec![production(cache)[hit].to_string()];
        let mut proven: Vec<&&str> = Vec::new();
        loop {
            let before = proven.len();
            for name in RESOLVED_EVERY_FRAME {
                if proven.contains(&name) {
                    continue;
                }
                let call = format!("{name}(");
                if !reached.iter().any(|body| body.contains(&call)) {
                    continue;
                }
                proven.push(name);
                let sig = format!("fn {name}(");
                for (_, source) in &sources {
                    if let Some(body) = block_after(source, &sig) {
                        reached.push(source[body].to_string());
                    }
                }
            }
            if proven.len() == before {
                break;
            }
        }
        let unreached: Vec<&&str> = RESOLVED_EVERY_FRAME
            .iter()
            .filter(|name| !proven.contains(name))
            .collect();
        assert!(
            unreached.is_empty(),
            "{unreached:?} are allowlisted as re-resolved on every frame, but \
             `SurfaceCache::render`'s cache-hit branch reaches no call to them. \
             An entry the hit branch does not reach is served from the cached \
             frame like any other reader, so it must either be called from that \
             branch or leave this list and capture what it reads in `Inputs`."
        );

        for (name, source) in &sources {
            let resolved_every_frame: Vec<std::ops::Range<usize>> = RESOLVED_EVERY_FRAME
                .iter()
                .filter_map(|f| block_after(source, &format!("fn {f}(")))
                .collect();
            for reader in ["panes_in_z_order", ".grids()", ".painted_grids()"] {
                for (offset, _) in source.match_indices(reader) {
                    let re_resolved = resolved_every_frame.iter().any(|r| r.contains(&offset));
                    assert!(
                        re_resolved || holds_panes,
                        "{name} paints from {reader} at byte {offset} while \
                         `Inputs` captures no pane geometry and the read sits \
                         outside {RESOLVED_EVERY_FRAME:?} (the set re-run on \
                         every frame regardless of cache hit or miss), so a \
                         cached frame can survive a window moving, resizing, \
                         hiding or closing. Capture the panes in `Inputs` \
                         (whole, not a projection), or move the read into \
                         (or add it to) the re-resolved set if it is safe."
                    );
                }
            }
        }
    }

    fn model_with_grid(width: u16, height: u16) -> Model {
        let mut model = Model::new();
        model.term_width = width;
        model.term_height = height;
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

    /// A keystroke typed into one window under multigrid, as nvim answers
    /// it with the ruler and showcmd externalized: the window's cell, its
    /// cursor, its viewport and the two message-area readings. None of it
    /// reaches a layer, so every key reuses the cached frame and only the
    /// first frame renders.
    ///
    /// Disconfirm: comparing the message stack with its clock (the derived
    /// `==`), the ruler or the showcmd reading, rebuilds per key.
    #[test]
    fn a_keystroke_in_one_window_reuses_the_cached_frame() {
        let mut model = model_with_grid(20, 6);
        let key = |model: &mut Model, col: u64| {
            // what the runtime sets ahead of every fold
            model.set_now(std::time::SystemTime::now());
            model.set_utc_offset(3600);
            let _ = update(
                model,
                Msg::Redraw(vec![
                    UiEvent::GridLine {
                        grid: 2,
                        row: 0,
                        col_start: col,
                        cells: vec![view_core::events::GridCell {
                            text: "a".into(),
                            hl_id: 0,
                            repeat: 1,
                        }],
                    },
                    UiEvent::GridCursorGoto {
                        grid: 2,
                        row: 0,
                        col: col + 1,
                    },
                    UiEvent::WinViewport {
                        grid: 2,
                        win: view_core::events::WinHandle(1000),
                        topline: 0,
                        botline: 4,
                        curline: 0,
                        curcol: col + 1,
                        line_count: Some(1),
                    },
                    UiEvent::MsgShowcmd { content: vec![] },
                    UiEvent::MsgRuler {
                        content: vec![(0, format!("1,{}", col + 2))],
                    },
                    UiEvent::Flush,
                ]),
            );
        };
        let _ = update(
            &mut model,
            Msg::Redraw(vec![
                UiEvent::GridResize {
                    grid: 2,
                    width: 20,
                    height: 4,
                },
                UiEvent::WinPos {
                    grid: 2,
                    win: view_core::events::WinHandle(1000),
                    startrow: 0,
                    startcol: 0,
                    width: 20,
                    height: 4,
                },
                UiEvent::ModeChange {
                    mode: "insert".into(),
                    mode_idx: 1,
                },
                UiEvent::MsgShowmode {
                    content: vec![(0, "-- INSERT --".into())],
                },
                UiEvent::Flush,
            ]),
        );
        let mut cache = SurfaceCache::new();
        let _ = cache.render(&model);
        for col in 0..3 {
            key(&mut model, col);
            let _ = cache.render(&model);
        }
        assert_eq!(
            (cache.frames, cache.rebuilds),
            (4, 1),
            "a keystroke rebuilt the frame"
        );
    }

    /// The palette paints the rows of a float its command line took, so a
    /// line nvim redraws in that float alone rebuilds the frame.
    ///
    /// Disconfirm: dropping `listed` from `matches` reuses the stale frame.
    #[test]
    fn a_cell_change_in_the_float_the_palette_lists_rebuilds_the_frame() {
        let mut model = model_with_grid(60, 20);
        model.palette_enabled = true;
        let line = |text: &str| UiEvent::GridLine {
            grid: 5,
            row: 0,
            col_start: 0,
            cells: vec![view_core::events::GridCell {
                text: text.into(),
                hl_id: 0,
                repeat: 1,
            }],
        };
        let _ = update(
            &mut model,
            Msg::Redraw(vec![
                UiEvent::CmdlineShow {
                    content: vec![(0, "e ".to_string())],
                    pos: 2,
                    firstc: ":".to_string(),
                    prompt: String::new(),
                    indent: 0,
                    level: 1,
                },
                UiEvent::GridResize {
                    grid: 5,
                    width: 20,
                    height: 3,
                },
                UiEvent::WinFloatPos {
                    grid: 5,
                    win: view_core::events::WinHandle(1008),
                    anchor: view_core::events::FloatAnchor::NorthWest,
                    anchor_grid: 1,
                    zindex: 1001,
                    compindex: 1,
                    screen_row: 10,
                    screen_col: 0,
                },
                line("first"),
                UiEvent::Flush,
            ]),
        );
        let mut cache = SurfaceCache::new();
        let _ = cache.render(&model);
        let _ = update(
            &mut model,
            Msg::Redraw(vec![line("second"), UiEvent::Flush]),
        );
        let painted = format!("{:?}", cache.render(&model).layers);
        assert!(painted.contains("second"), "{painted}");
        assert_eq!(
            (cache.frames, cache.rebuilds),
            (2, 2),
            "the listed float's new line reused the stale frame"
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
            .any(|l| matches!(l.kind, LayerKind::Toast { .. })));
        assert_eq!((cache.frames, cache.rebuilds), (2, 2));
    }

    /// The pause key changes no message entry: it flips one bool, and the
    /// only thing on screen that answers to it is a mark in the top box's
    /// border run. A frame keyed on the entries alone is handed back
    /// unmarked, and the freeze the mark exists to show goes invisible --
    /// which is what a live session showed before this joined `Inputs`.
    #[test]
    fn the_pause_key_rebuilds_the_frame_it_marks() {
        let mut model = model_with_grid(20, 6);
        apply(
            &mut model,
            UiEvent::MsgShow {
                kind: "echo".into(),
                content: vec![(0, "read me".into())],
                replace_last: false,
            },
        );
        let mut cache = SurfaceCache::new();
        let _ = cache.render(&model);

        model.engine.messages.toggle_pause();
        let surface = cache.render(&model);

        assert!(
            surface
                .layers
                .iter()
                .any(|l| matches!(l.kind, LayerKind::Toast { paused: true, .. })),
            "the reused frame must be rebuilt with the mark on: {:?}",
            surface.layers
        );
        assert_eq!((cache.frames, cache.rebuilds), (2, 2));
    }

    /// A box-glyph reply that lands after the first paint moves no tier --
    /// it flips one bool -- and every framed layer in the cached frame is
    /// drawn in the charset that bool decides. Reusing that frame leaves
    /// the session drawing ASCII corners at a terminal that has just said
    /// it draws box glyphs, with nothing failing.
    #[test]
    fn a_late_box_glyph_answer_rebuilds_the_frame_it_reframes() {
        let mut model = model_with_grid(20, 6);
        model.statusline_enabled = true;
        let mut cache = SurfaceCache::new();
        let borders = |surface: &Surface| {
            surface
                .layers
                .iter()
                .find_map(|l| l.borders)
                .expect("the statusline feature is on, so a framed layer exists")
        };
        assert_eq!(borders(cache.render(&model)), crate::BorderSet::ASCII);

        let tier_before = model.caps.tier;
        model.caps = model.caps.with_unicode_boxes(true);
        let surface = cache.render(&model);

        assert_eq!(model.caps.tier, tier_before, "the tier did not move");
        assert_eq!(borders(surface), crate::BorderSet::ROUNDED);
        assert_eq!(
            (cache.frames, cache.rebuilds),
            (2, 2),
            "a charset change cannot be served from the cache"
        );
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

    /// The bar's in-place refresh reads the width the frame around it was
    /// built at, whatever the engine's own is: while the startup hold is on
    /// there is no grid to measure and the bar spans the terminal, so a
    /// refresh at the engine's width would hand back a frame that disagrees
    /// with a rebuild of the same model (the debug equivalence check inside
    /// `render` is what says so).
    #[test]
    fn a_held_frames_statusline_refreshes_at_the_width_it_was_built_at() {
        use view_core::native::statusline::SegmentUpdate;

        let mut model = model_with_grid(20, 6);
        model.statusline_enabled = true;
        model.chrome_painted = false;
        let mut cache = SurfaceCache::new();
        let _ = cache.render(&model);

        model
            .engine
            .statusline
            .apply(SegmentUpdate::Ruler("3,7".to_string()));
        // the assertion is `render`'s own debug equivalence check, which
        // panics on a reused frame a rebuild would not have produced
        let surface = cache.render(&model);

        let layer = surface
            .layers
            .iter()
            .find(|l| matches!(l.kind, LayerKind::Statusline(_)))
            .expect("the statusline feature is on, so its layer must exist");
        assert_eq!(
            layer.rect.width, model.term_width,
            "a withheld frame has no grid to measure, so its bar spans the terminal"
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
