//! Application state `update()` reads and mutates. No I/O, no rendering.

use std::path::PathBuf;

use crate::events::{ModeInfo, PmItem, TabEntry, TabHandle};
use crate::grid::{Grid, GridOp};
use crate::hl::{HlAttr, HlTable, ProbedDefaults};
use crate::native::geometry::{OverlayBox, OverlayRect};
use crate::native::mappings::MappingClaim;
use crate::native::views::Span;

/// The complete application state.
#[non_exhaustive]
pub struct Model {
    pub engine: EngineModel,
    /// Open native overlays, innermost last: the tail is the one on top, the
    /// one holding input focus, and the one `<Esc>` closes. Focus is read
    /// off this stack by [`Model::focus`] rather than stored beside it,
    /// because two overlays genuinely coexist (a confirm prompt can arrive
    /// while a picker is open) and a stored focus would have to be restored
    /// by hand on every close.
    ///
    /// Private, and reachable only through [`Model::overlays`],
    /// [`Model::push_overlay`] and [`Model::pop_overlay`], for the same
    /// reason [`EngineModel::grid`] is: a `pub` field lets a caller push an
    /// `OverlayId` that is already on the stack, and two entries sharing an
    /// id make [`Model::overlay_at`] answer with a token that names two
    /// overlays. Pushing through the accessor is what keeps ids unique.
    overlays: Vec<Overlay>,
    /// The next id [`Model::push_overlay`] hands out. Monotonic and never
    /// reused, so an id captured from an overlay that has since closed can
    /// never alias a live one.
    next_overlay_id: u64,
    /// Who owns the mouse gesture in flight, or `None` while no button is
    /// down; see [`Model::mouse_capture`].
    mouse_capture: Option<MouseCapture>,
    pub caps: TermCaps,
    /// Set by `update()` on `Flush`; cleared by the loop after paint.
    pub dirty: bool,
    pub running: bool,
    /// The real terminal's current width in cells, fed by `Msg::Resized`
    /// and startup wiring ([`Model::with_term_size`]). Independent of the
    /// engine grid's own size: the grid is a chrome-reserved subregion of
    /// this once persistent chrome (the tabline) is showing.
    pub term_width: u16,
    /// The real terminal's current height in cells; see `term_width`.
    pub term_height: u16,
    /// Whether real grid content has ever arrived. Defaults `true` (an
    /// ordinary already-running model, which is what every consumer other
    /// than startup itself constructs and expects to render normally);
    /// startup is the one caller that deliberately flips this to `false`
    /// right after building its very first `Model`, to opt into painting
    /// the placeholder shell (statusline bar plus a static "waiting"
    /// indicator, see `view_surface::LayerKind::Shell`) instead of an
    /// empty grid while the engine attaches. `update()` flips it back to
    /// `true` on the first `Flush`, at which point `render()` drops the
    /// `Shell` layer for good; never reset afterward, since a mid-session
    /// redraw storm is not a second "waiting for nvim" state.
    pub content_painted: bool,
    /// Set from `Msg::EngineStopped`'s payload when the engine's RPC reader
    /// thread stopped reading for a reason other than an ordinary process
    /// exit (see that variant's doc comment). The bin crate reports this to
    /// the user after `runtime::run` returns and the terminal is restored;
    /// nothing paints from it, so it carries no rendering contract.
    pub fatal_reason: Option<String>,
    /// Every default key this session registered, as the engine reported it
    /// back, in registration order. Empty until the mappings are registered
    /// (post-`VimEnter`), and empty for the whole session when no feature
    /// with default keys is enabled.
    ///
    /// Private, and written only through
    /// [`Model::record_claimed_keys`]: the list is what the first-run toast
    /// and doctor answer "what did view claim?" from, and a second recording
    /// appending to the first would report every key twice.
    claimed_keys: Vec<MappingClaim>,
    /// Whether the `statusline` native feature is enabled for this session,
    /// set once at startup from `NativeConfig::enabled("statusline")`
    /// (`crates/view/src/native.rs`'s `NativeSession::load`, the one place
    /// config resolution reaches `Model` -- `view-core` cannot depend on
    /// `view-native` itself, so this is a plain bool rather than a config
    /// handle). Gates [`Model::statusline_rows`], which in turn gates
    /// whether `view-surface::render` reserves a bottom row for the bar.
    pub statusline_enabled: bool,
    /// Whether the `palette` native feature is enabled for this session, set
    /// the same way and at the same place as `statusline_enabled`. Gates
    /// `view-surface::render`'s choice between the centered floating
    /// palette and nvim's own bottom-line command line: `false` restores
    /// the plain bottom-line rendering a user who turned the feature off
    /// with `native.palette = false` expects, rather than leaving them with
    /// no cmdline at all.
    pub palette_enabled: bool,
    /// The working directory a relative [`crate::native::picker::Source`]
    /// resolves against, learned once at startup
    /// ([`Model::with_cwd`]) since `update()` has no filesystem access to
    /// ask for it itself. Empty until startup sets it.
    pub cwd: PathBuf,
}

impl Model {
    /// A freshly started application: an empty grid, an empty highlight
    /// table, no open overlays (so the engine holds focus), conservative
    /// terminal capabilities, zero terminal size, and no pending paint.
    #[must_use]
    pub fn new() -> Self {
        Self {
            engine: EngineModel {
                grid: Grid::new(),
                hl: HlTable::new(),
                mode: ModeState::default(),
                cmdline: None,
                messages: Messages::default(),
                toast_history: crate::native::toast::ToastHistory::new(),
                tabline: None,
                popupmenu: None,
                mouse_on: false,
                statusline: crate::native::statusline::StatuslineState::default(),
            },
            overlays: Vec::new(),
            next_overlay_id: 1,
            mouse_capture: None,
            caps: TermCaps::default(),
            dirty: false,
            running: true,
            term_width: 0,
            term_height: 0,
            content_painted: true,
            fatal_reason: None,
            claimed_keys: Vec::new(),
            statusline_enabled: false,
            palette_enabled: false,
            cwd: PathBuf::new(),
        }
    }

    /// Like [`Model::new`], but with `term_width`/`term_height` pre-filled
    /// from the real terminal size learned at startup, before any grid data
    /// has arrived from the engine. Startup wires this in directly rather
    /// than waiting for the first `Msg::Resized`, since a resize event only
    /// fires on a *change* and the initial size never triggers one.
    #[must_use]
    pub fn with_term_size(width: u16, height: u16) -> Self {
        Self {
            term_width: width,
            term_height: height,
            ..Self::new()
        }
    }

    /// Like [`Model::new`], but with `cwd` pre-filled from the process's
    /// working directory at startup, before any picker ever opens.
    #[must_use]
    pub fn with_cwd(cwd: PathBuf) -> Self {
        Self { cwd, ..Self::new() }
    }

    /// The default keys this session registered; see
    /// [`Model::record_claimed_keys`].
    #[must_use]
    pub fn claimed_keys(&self) -> &[MappingClaim] {
        &self.claimed_keys
    }

    /// Records the engine's answer to one mapping registration, replacing
    /// any earlier answer rather than appending to it.
    ///
    /// Registration happens once per session, so a second answer is a
    /// re-registration and describes the same keys; appending would make the
    /// claim report grow a duplicate row per key.
    pub fn record_claimed_keys(&mut self, claimed: Vec<MappingClaim>) {
        self.claimed_keys = claimed;
    }

    /// Who owns input this frame: the topmost open overlay, or the engine
    /// when none is open.
    ///
    /// Derived from [`Model::overlays`] rather than stored alongside it. A
    /// stored focus is a second fact that has to agree with overlay
    /// presence, and every close is a chance to leave it naming an overlay
    /// that is gone, which routes keys into nothing. Derivation makes that
    /// state unrepresentable.
    #[must_use]
    pub fn focus(&self) -> Focus {
        match self.overlays.last() {
            Some(overlay) => Focus::Native(overlay.id),
            None => Focus::Engine,
        }
    }

    /// The topmost overlay covering the terminal cell at `(row, col)`, or
    /// `None` when the cell belongs to the engine grid.
    ///
    /// Mouse input routes through this rather than through [`Model::focus`]:
    /// an open overlay owns the keyboard outright, but it owns only the
    /// cells it covers, so a click on visible grid outside it still reaches
    /// the engine.
    #[must_use]
    pub fn overlay_at(&self, row: u16, col: u16) -> Option<OverlayId> {
        self.overlays
            .iter()
            .rev()
            .find(|overlay| self.overlay_rect(overlay).contains(row, col))
            .map(|overlay| overlay.id)
    }

    /// The open overlays, bottom of the stack first.
    #[must_use]
    pub fn overlays(&self) -> &[Overlay] {
        &self.overlays
    }

    /// The topmost overlay, the one holding focus, for a feature that needs
    /// to fold its own state forward as input arrives.
    #[must_use]
    pub fn top_overlay_mut(&mut self) -> Option<&mut Overlay> {
        self.overlays.last_mut()
    }

    /// The open picker's state, wherever it sits in the stack -- not only
    /// when it is topmost. A picker keeps running its query underneath a
    /// prompt that opened over it (see `OverlayKind::Picker`'s doc on the
    /// stacking rule), so the matcher worker's streamed reply must still be
    /// able to reach it even while a prompt, not the picker, holds focus.
    #[must_use]
    pub fn picker_mut(&mut self) -> Option<&mut crate::native::picker::PickerState> {
        self.overlays
            .iter_mut()
            .find_map(|overlay| match &mut overlay.kind {
                OverlayKind::Picker(p) => Some(p),
                _ => None,
            })
    }

    /// The open tree's state, wherever it sits in the stack -- not only
    /// when it is topmost, for the same reason [`Model::picker_mut`] looks
    /// past the top: a scan or git-status reply must still be able to reach
    /// the tree while a prompt opened over it holds focus.
    #[must_use]
    pub fn tree_mut(&mut self) -> Option<&mut crate::native::tree::TreeState> {
        self.overlays
            .iter_mut()
            .find_map(|overlay| match &mut overlay.kind {
                OverlayKind::Tree(t) => Some(t),
                _ => None,
            })
    }

    /// Closes the tree overlay, wherever it sits in the stack, and reports
    /// whether one was found to close. Removed by kind rather than only
    /// when topmost, so a toggle key reaches it even in the corner case
    /// where a prompt has landed above it in the meantime (see
    /// [`OverlayKind::Tree`]'s stacking doc); [`Model::pop_overlay`] alone
    /// would close the wrong overlay in that case.
    pub fn close_tree(&mut self) -> bool {
        let Some(pos) = self
            .overlays
            .iter()
            .position(|overlay| matches!(overlay.kind, OverlayKind::Tree(_)))
        else {
            return false;
        };
        let removed = self.overlays.remove(pos);
        if let Some(MouseCapture::Overlay(held)) = self.mouse_capture {
            if removed.id == held {
                self.mouse_capture = None;
            }
        }
        true
    }

    /// Opens `kind` at `geometry` as the new topmost overlay, returning the
    /// id it was assigned. The id is the model's to hand out, never the
    /// caller's to choose, so no two open overlays can share one.
    pub fn push_overlay(&mut self, geometry: OverlayBox, kind: OverlayKind) -> OverlayId {
        let id = self.next_id();
        self.overlays.push(Overlay { id, geometry, kind });
        id
    }

    /// Opens `kind` at `geometry` directly beneath the current topmost
    /// overlay, leaving focus untouched, for a feature that must not steal
    /// focus from whatever already holds it -- a picker opening while a
    /// blocked-engine `Prompt` is topmost is the one caller today (see
    /// [`OverlayKind::Picker`]'s doc on the stacking rule). Falls back to
    /// the ordinary top-of-stack position when nothing is open yet, since
    /// there is no "beneath" to insert under.
    pub fn insert_overlay_beneath_top(
        &mut self,
        geometry: OverlayBox,
        kind: OverlayKind,
    ) -> OverlayId {
        let id = self.next_id();
        let index = self.overlays.len().saturating_sub(1);
        self.overlays.insert(index, Overlay { id, geometry, kind });
        id
    }

    /// Hands out the next unique overlay id; see [`Model::push_overlay`]'s
    /// doc on why callers never choose their own.
    fn next_id(&mut self) -> OverlayId {
        let id = OverlayId(self.next_overlay_id);
        // saturating rather than wrapping: a wrapped counter would reissue
        // an id a live overlay already holds, and no session can open
        // u64::MAX overlays to reach the saturation point
        self.next_overlay_id = self.next_overlay_id.saturating_add(1);
        id
    }

    /// Closes the topmost overlay and returns it, or `None` when none is
    /// open. Any mouse gesture that overlay had captured is released with
    /// it, so a drag whose target closed mid-gesture cannot keep routing to
    /// something that is gone.
    pub fn pop_overlay(&mut self) -> Option<Overlay> {
        let closed = self.overlays.pop();
        if let (Some(overlay), Some(MouseCapture::Overlay(held))) = (&closed, self.mouse_capture) {
            if overlay.id == held {
                self.mouse_capture = None;
            }
        }
        closed
    }

    /// Who owns the mouse gesture in flight: the surface that received the
    /// press, until the matching release. `None` while no button is down.
    ///
    /// A gesture belongs to one surface for its whole life. Routing each
    /// event by the pointer's current cell instead would hand the engine a
    /// release it never saw a press for whenever a drag crosses an overlay
    /// edge, leaving nvim stuck mid-selection.
    #[must_use]
    pub fn mouse_capture(&self) -> Option<MouseCapture> {
        self.mouse_capture
    }

    /// Records `target` as the owner of the gesture starting now.
    pub fn capture_mouse(&mut self, target: MouseCapture) {
        self.mouse_capture = Some(target);
    }

    /// Ends the gesture in flight, so the next event routes by position
    /// again.
    pub fn release_mouse(&mut self) {
        self.mouse_capture = None;
    }

    /// The cells `overlay` covers on the current terminal.
    ///
    /// The one place an overlay's share of the terminal becomes cells, for
    /// hit-testing and for painting alike: two resolutions reading two
    /// terminal sizes would let a click land on a rect the user is not
    /// looking at.
    #[must_use]
    pub fn overlay_rect(&self, overlay: &Overlay) -> OverlayRect {
        overlay.geometry.rect(self.term_width, self.term_height)
    }

    /// Terminal rows reserved for persistent chrome outside the engine
    /// grid: one row for the tabline once more than one tab is open
    /// (matching bare nvim's default `showtabline` threshold), zero
    /// otherwise. Transient overlays (cmdline, messages, popupmenu) paint
    /// over the grid instead and never reserve rows.
    #[must_use]
    pub fn chrome_rows(&self) -> u16 {
        match &self.engine.tabline {
            Some(t) if t.tabs.len() > 1 => 1,
            _ => 0,
        }
    }

    /// Terminal rows reserved for the bottom-row statusline bar: one while
    /// the `statusline` native feature is enabled, zero otherwise. Distinct
    /// from [`Model::chrome_rows`] (a top-row offset for the tabline, not a
    /// total reservation) -- `view-surface::render` uses both together to
    /// find the engine grid's target size and the statusline layer's row.
    #[must_use]
    pub fn statusline_rows(&self) -> u16 {
        u16::from(self.statusline_enabled)
    }

    /// Drains what changed since the last call, so a repaint can clip
    /// compositing to the damaged region. The runtime calls this once per
    /// frame, alongside clearing [`Model::dirty`]; see
    /// [`crate::grid::GridDamage`].
    ///
    /// The one place damage is drained, because it is the one place that
    /// sees every input a composite reads: the grid's own changed rows, and
    /// the highlight table behind every cell's resolved style. A highlight
    /// change has no rows of its own -- it can restyle the whole screen at
    /// once -- so it collapses to whole-frame damage. Draining a paint input
    /// anywhere else would clip a frame against a subset of what it paints
    /// from, which is why [`crate::grid::Grid::take_dirty`] is crate-private.
    #[must_use]
    pub fn take_paint_damage(&mut self) -> crate::grid::GridDamage {
        // both drained unconditionally: a change left in either tracker
        // would resurface as damage on some later frame that no longer
        // needs it
        let hl_changed = self.engine.hl.take_dirty();
        let grid = self.engine.grid.take_dirty();
        if hl_changed {
            crate::grid::GridDamage::full()
        } else {
            grid
        }
    }

    /// The `(width, height)` the engine grid should be resized to, given
    /// the current terminal size and reserved chrome rows. `update()` sends
    /// this as `Effect::Rpc(RpcCall::TryResize)` whenever the terminal size
    /// or the chrome reservation changes.
    #[must_use]
    pub fn grid_target(&self) -> (u16, u16) {
        (
            self.term_width,
            self.term_height
                .saturating_sub(self.chrome_rows() + self.statusline_rows()),
        )
    }
}

impl Default for Model {
    fn default() -> Self {
        Self::new()
    }
}

/// The embedded engine's half of [`Model`]: its grid, highlight table, mode
/// state, and the `ext_cmdline`/`ext_messages`/`ext_tabline`/
/// `ext_popupmenu` overlay states. The four overlay fields are `Option`
/// (`Messages` excepted, which is a log rather than a point-in-time
/// overlay): `None` means nvim has not shown that overlay since the last
/// time it was hidden, matching the `_show`/`_hide` event pairing on the
/// wire.
#[non_exhaustive]
pub struct EngineModel {
    /// The engine grid. Private, and reachable only through
    /// [`EngineModel::grid`] and [`EngineModel::apply_grid`], because it is
    /// one of the two paint inputs that track their own damage: a `pub`
    /// field makes `engine.grid = Grid::new()` compile, which installs a
    /// tracker holding none of the damage the replacement caused and clips
    /// the next frame to nothing.
    grid: Grid,
    /// The highlight table, private for the same reason `grid` is; see
    /// [`EngineModel::hl`] and the mutators beside it. Whole-table
    /// replacement stays available through [`EngineModel::replace_hl`],
    /// which records the damage a replacement causes instead of discarding
    /// it.
    hl: HlTable,
    pub mode: ModeState,
    pub cmdline: Option<CmdlineState>,
    pub messages: Messages,
    /// Bounded scrollback of every message routed through
    /// [`crate::native::toast::route`], newest-first on read; the
    /// palette's message-history view reads it.
    pub toast_history: crate::native::toast::ToastHistory,
    pub tabline: Option<TablineState>,
    pub popupmenu: Option<PopupmenuState>,
    /// Whether nvim currently wants terminal mouse reporting on, from the
    /// last `mouse_on`/`mouse_off` redraw event. The terminal only enables
    /// mouse capture while this is `true`: capturing unconditionally would
    /// swallow the host terminal's own selection/scrollback gestures even
    /// when nvim's `'mouse'` option is off.
    pub mouse_on: bool,
    /// The statusline bar's current segment text, applied from
    /// `msg_showmode`/`msg_showcmd`/`msg_ruler` redraw events, the
    /// `search_count` `msg_show` kind (through [`EngineModel::record_message`]'s
    /// `Route::Statusline` branch), and the bridge's diagnostics/git/buffer
    /// callbacks. Present regardless of whether the feature is enabled --
    /// see [`Model::statusline_enabled`] for the gate that decides whether
    /// `view-surface::render` ever reads it.
    pub statusline: crate::native::statusline::StatuslineState,
}

// the three accessors the compositor reaches for every frame carry
// `#[inline]`: they are field reads, the workspace builds without LTO, and
// without the hint nothing outside this crate can see through them
impl EngineModel {
    /// The engine grid, for reading: its cells, size, and cursor position.
    #[must_use]
    #[inline]
    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    /// Applies one decoded `ext_linegrid` operation to the grid. The only
    /// way to mutate it, so every mutation goes through the tracker that
    /// records which rows it touched.
    #[inline]
    pub fn apply_grid(&mut self, op: GridOp) {
        self.grid.apply(op);
    }

    /// The highlight table, for reading: default colors, per-id attributes,
    /// builtin group mappings, and the probe generation.
    #[must_use]
    #[inline]
    pub fn hl(&self) -> &HlTable {
        &self.hl
    }

    /// Defines (or redefines) one highlight id's attributes, per
    /// `hl_attr_define`.
    pub fn define_hl_attr(&mut self, hl_id: u64, attr: HlAttr) {
        self.hl.define_attr(hl_id, attr);
    }

    /// Associates a builtin UI element name with the `hl_id` it resolves
    /// through, per `hl_group_set`.
    pub fn set_hl_group(&mut self, name: String, hl_id: u64) {
        self.hl.set_group(name, hl_id);
    }

    /// Records new default colors, returning the probe generation the
    /// emitted `nvim_get_hl` call must carry; see
    /// [`HlTable::set_default_colors`] for why dropping it is never correct.
    #[must_use]
    pub fn set_hl_default_colors(&mut self, fg: Option<u32>, bg: Option<u32>) -> u64 {
        self.hl.set_default_colors(fg, bg)
    }

    /// Accepts one probe reply as the confirmed disambiguation of the
    /// current defaults; see [`HlTable::confirm_defaults`] for the
    /// generation check the caller owes first.
    pub fn confirm_hl_defaults(&mut self, probe: ProbedDefaults) {
        self.hl.confirm_defaults(probe);
    }

    /// Installs a whole highlight table, as startup does with one seeded
    /// from a persisted theme, and records that every resolved style on
    /// screen just moved.
    ///
    /// The damage mark is the reason this exists rather than a `pub` field
    /// or a `&mut` accessor: a replacement changes the styles behind every
    /// painted cell while touching no grid row, so a plain assignment would
    /// leave the next frame clipped to whatever rows the grid happened to
    /// damage, painting the new table's colors onto those rows alone.
    pub fn replace_hl(&mut self, hl: HlTable) {
        self.hl = hl;
        self.hl.mark_dirty();
    }

    /// The one place a [`MessageEntry`] is created that also classifies it
    /// (`native::toast::route`), records it to scrollback
    /// (`toast_history`), and schedules its transient-toast expiry.
    /// `Messages::push` alone only stamps an id; a caller that reaches past
    /// this method straight to `messages.push` produces an entry with no
    /// history record and no expiry -- invisible to a future `:messages`
    /// view and, for a transient kind, stuck on screen forever on an idle
    /// editor, the exact bug `native::toast`'s timer design exists to
    /// close. [`UiEvent::MsgShow`](crate::events::UiEvent::MsgShow)
    /// (wire-decoded) and [`Self::record_native_notice`]
    /// (locally-synthesized) are its only two callers, matching
    /// [`MessageEntry::kind`]'s own "off the wire, or synthesized locally"
    /// split.
    pub fn record_message(
        &mut self,
        kind: String,
        content: Vec<(u64, String)>,
        replace_last: bool,
    ) -> Vec<crate::msg::Effect> {
        let route = crate::native::toast::route(&kind);
        if route == crate::native::toast::Route::Statusline {
            // only `search_count` reaches here as a `kind`
            // (`msg_showmode`/`msg_showcmd`/`msg_ruler` arrive as their own
            // `UiEvent` variants instead -- see
            // docs/statusline-wire-capture.md); feeding it into the ordinary
            // message log would strand it there forever, since
            // `Route::Statusline` schedules no expiry
            // (`toast::timeout_for`) and `Messages`' own visible-lines
            // selection does no route-based filtering to hide it either.
            let text: String = content.iter().map(|(_, t)| t.as_str()).collect();
            self.statusline
                .apply(crate::native::statusline::SegmentUpdate::SearchCount(text));
            return Vec::new();
        }
        let id = self.messages.push(kind, content, replace_last);
        // recorded by id, not `.entries.last()`: `push`'s replace path can
        // overwrite an entry that sits before a still-open condition
        // notice, which then occupies the last slot instead
        if let Some(entry) = self.messages.entries.iter().find(|e| e.id() == id) {
            self.toast_history.push(entry);
        }
        // only `Route::Transient` ever schedules a timeout (see
        // `toast::timeout_for`); a prompt/sticky/statusline entry expires
        // some other way or not at all
        match crate::native::toast::timeout_for(route) {
            Some(after) => vec![crate::msg::Effect::ScheduleToastExpiry { id, after }],
            None => Vec::new(),
        }
    }

    /// A locally-synthesized notice -- never from nvim's own `msg_show` --
    /// through the same [`Self::record_message`] every wire-decoded message
    /// goes through, so a native notice gets the same scrollback/expiry
    /// treatment a wire one does. [`Messages::push_native`] itself stays
    /// crate-private: `Messages::set_native_condition` is its one remaining
    /// caller, where persistence comes from the condition flag it sets
    /// after pushing rather than from `route()`'s kind-based
    /// classification this method's callers want -- a call site anywhere
    /// outside this module reaches a native notice through this method or
    /// not at all.
    pub fn record_native_notice(
        &mut self,
        text: String,
        replace_last: bool,
    ) -> Vec<crate::msg::Effect> {
        self.record_message("native".to_string(), vec![(0, text)], replace_last)
    }
}

/// nvim mode state: the cursor/highlight property table from the last
/// `mode_info_set`, plus the active mode from the last `mode_change`.
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct ModeState {
    /// nvim's own `mode_info_set` contract: when `false`, the UI must not
    /// restyle the cursor per mode at all and should render a plain
    /// (block) cursor regardless of what `modes`/`current_idx` describe.
    pub cursor_style_enabled: bool,
    pub modes: Vec<ModeInfo>,
    pub current: String,
    pub current_idx: u64,
}

impl ModeState {
    /// The active mode's cursor/highlight properties, looked up by
    /// `current_idx` into `modes`. `None` before the first `mode_info_set`
    /// arrives, or if `current_idx` is out of range (a desynced index from
    /// a malformed event must not panic on indexing).
    #[must_use]
    pub fn active_cursor(&self) -> Option<&ModeInfo> {
        usize::try_from(self.current_idx)
            .ok()
            .and_then(|idx| self.modes.get(idx))
    }
}

/// The command line's current content and cursor position, present only
/// while nvim's command line is open (`cmdline_show`..`cmdline_hide`).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct CmdlineState {
    pub content: Vec<(u64, String)>,
    pub pos: u64,
    pub firstc: String,
    pub prompt: String,
    pub indent: u64,
    pub level: u64,
}

/// A locally-assigned identity for one [`MessageEntry`], stamped by
/// [`Messages::push`] from a monotonic per-session counter. Exists to name
/// "the same entry, later": `Msg::ToastExpired`'s idle-timeout callback
/// fires well after the push that scheduled it, by which time
/// `Messages::push` may have appended or replaced other entries at
/// arbitrary positions, so the id -- not an index -- is what the expiry
/// handler matches against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MessageId(u64);

/// One shown message: an echo, an error, a search-count indicator, and
/// so on.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageEntry {
    pub kind: String,
    pub content: Vec<(u64, String)>,
    /// `Messages::flush_generation` at the moment this entry was pushed.
    /// Not part of nvim's wire contract -- purely local bookkeeping for
    /// `Messages::dismiss_transient_on_keypress`'s "at least one visible
    /// frame before dismissal" guarantee. Never set directly; every
    /// `MessageEntry` is built by `Messages::push`, which stamps this from
    /// its own counter.
    shown_at_flush: u64,
    /// Whether this entry is the one locally-raised condition notice (see
    /// `Messages::set_native_condition`) rather than a record of something
    /// that happened. Marked rather than matched on text or kind, so
    /// retracting the condition can never take a real message with it.
    condition: bool,
    /// This entry's identity; see [`MessageId`]. Never set directly --
    /// stamped by `Messages::push` from its own counter.
    id: MessageId,
}

impl MessageEntry {
    /// This entry's identity, stamped when it was pushed. `toast`'s
    /// idle-expiry timer names the entry it was scheduled for by this id,
    /// since positions in `Messages::entries` shift as later messages
    /// arrive.
    #[must_use]
    pub fn id(&self) -> MessageId {
        self.id
    }

    /// This entry's content chunks joined into one string, then split into
    /// one entry per physical line. A `msg_show` content chunk can carry an
    /// embedded `\n` for a genuinely multi-line message (a long `emsg`'s
    /// wrapped continuation, live-observed from a real autocommand error,
    /// and documented in nvim's own `api-ui-events.txt`: "Messages can
    /// contain line breaks") rather than always being exactly one visual
    /// line; a caller that joins the chunks and paints the result as a
    /// single row squashes every line break into one toast row wide enough
    /// to hold all of them concatenated. `view_surface::render` (layer
    /// width/height) and `view_tui::paint::paint_messages` (per-row text)
    /// both call this instead of joining `content` themselves, so sizing
    /// and painting can never disagree about how many rows -- or how wide
    /// -- this entry needs. Always yields at least one (possibly empty)
    /// line, so an entry with no content still reserves its own row.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let joined: String = self.content.iter().map(|(_, t)| t.as_str()).collect();
        joined.split('\n').map(str::to_string).collect()
    }

    /// Whether nvim's own `msg_show` `kind` (per `api-ui-events.txt`'s kind
    /// table) names this an error or a warning: `"emsg"`, `"echoerr"`,
    /// `"wmsg"`, `"lua_error"`, `"rpc_error"`, `"shell_err"`. These must be
    /// read, not silently lost, so they persist until explicitly cleared or
    /// replaced -- never auto-dismissed by user activity and never evicted
    /// from the visible toast stack merely because other messages arrived
    /// after them (`Messages::visible_lines`) -- matching real nvim's own
    /// hit-enter-prompt convention that an error blocks until acknowledged.
    /// Every other kind is transient.
    ///
    /// `"shell_err"` is a `:!cmd`'s stderr: the one channel a failing
    /// external command has to explain itself, and the only reason to look
    /// at the output of a command that went wrong.
    ///
    /// A locally-raised condition notice (`Messages::set_native_condition`)
    /// is persistent by the same argument arrived at from the other side:
    /// it describes a state that is still true, and the user activity that
    /// dismisses a transient entry is exactly the activity a stalled engine
    /// is swallowing, so dismissing on a keypress would erase the notice
    /// with the very keystroke it is there to explain. It is retracted by
    /// whoever raised it, when the condition ends.
    #[must_use]
    pub fn is_persistent(&self) -> bool {
        self.condition || Self::is_persistent_kind(&self.kind)
    }

    /// The kind-only half of [`is_persistent`]: whether `kind` alone (no
    /// locally-raised condition flag, since none exists yet) names one of
    /// nvim's error/warning kinds. `toast::route` matches on this directly
    /// -- it classifies the wire `kind` string before any `MessageEntry`
    /// exists to ask.
    #[must_use]
    pub fn is_persistent_kind(kind: &str) -> bool {
        matches!(
            kind,
            "emsg" | "echoerr" | "wmsg" | "lua_error" | "rpc_error" | "shell_err"
        )
    }

    /// Whether this entry is the question text of a cmdline prompt that is
    /// still waiting for an answer -- nvim's `"confirm"` kind, which its own
    /// kind table defines as "message preceding a prompt".
    ///
    /// A third lifetime, neither persistent nor transient. nvim emits the
    /// question once as `msg_show` and the answer line separately as
    /// `cmdline_show`; a key that answers none of the offered choices
    /// re-arms the prompt by re-emitting `cmdline_show` ALONE, so a question
    /// dismissed on that keypress leaves an answer line with nothing to
    /// answer. Persistence is equally wrong in the other direction: nvim
    /// sends no `msg_clear` when the prompt resolves, so a question kept
    /// until explicitly cleared would occlude the buffer forever. Its
    /// lifetime is therefore the prompt's: dismissable by user activity,
    /// but only once the cmdline has closed.
    #[must_use]
    pub fn is_prompt(&self) -> bool {
        self.kind == "confirm"
    }

    /// Whether this entry keeps its rows when the toast box overflows.
    /// An unanswered question ranks with the errors: a burst of info
    /// messages must not push it off screen while the editor is blocked
    /// waiting for it.
    #[must_use]
    fn outranks_transient(&self) -> bool {
        self.is_persistent() || self.is_prompt()
    }
}

/// The message log built from `msg_show`/`msg_clear`. A log rather than a
/// single `Option`, since nvim can show several messages in sequence
/// (`:messages` history) before any are cleared.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Messages {
    pub entries: Vec<MessageEntry>,
    /// Bumped by `note_flush` on every `Flush` UI event; stamped onto each
    /// new entry as `MessageEntry::shown_at_flush`. See
    /// `dismiss_transient_on_keypress`.
    flush_generation: u64,
    /// The next [`MessageId`] `push` stamps; bumped on every call, replace
    /// included, so every pushed entry -- even one that overwrites another
    /// in place -- gets an identity distinct from what stood there before.
    next_message_id: u64,
}

impl Messages {
    /// Stamps and appends one entry: `kind`/`content` as decoded off the
    /// wire or synthesized locally, no classification, no history record,
    /// no expiry. `replace_last` overwrites the most recent entry instead
    /// of appending, matching nvim's progress-indicator convention (e.g.
    /// successive search-match counts share one line); with no prior entry
    /// to replace, it appends instead.
    ///
    /// "Most recent" means the most recent entry nvim itself produced. A
    /// raised condition notice (see `set_native_condition`) sits at the
    /// tail whenever it is up, and nvim's replace targets its own previous
    /// line: overwriting the notice would both drop a condition that is
    /// still true and leave the line nvim meant to replace standing as a
    /// duplicate.
    ///
    /// Crate-private on purpose: this is the raw primitive
    /// [`EngineModel::record_message`] is built from, not an entry point of
    /// its own. `EngineModel::record_message`/`record_native_notice` and
    /// `Messages::push_native`/`set_native_condition` are the only callers,
    /// all inside this crate -- nothing outside it can reach a `MessageEntry`
    /// without also going through classification.
    pub(crate) fn push(
        &mut self,
        kind: String,
        content: Vec<(u64, String)>,
        replace_last: bool,
    ) -> MessageId {
        let id = MessageId(self.next_message_id);
        self.next_message_id = self.next_message_id.saturating_add(1);
        let entry = MessageEntry {
            kind,
            content,
            shown_at_flush: self.flush_generation,
            condition: false,
            id,
        };
        if replace_last {
            if let Some(last) = self.entries.iter_mut().rev().find(|e| !e.condition) {
                *last = entry;
                return id;
            }
        }
        self.entries.push(entry);
        id
    }

    /// Drops every recorded message, per `msg_clear`.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Appends a locally-originated notice -- never from nvim's own
    /// `msg_show` wire event -- through the same overlay `msg_show`
    /// populates, so a native warning (e.g. startup's pre-attach key ring
    /// dropping a keystroke) reaches the user through the one message
    /// surface that already exists rather than a parallel toast mechanism.
    /// `replace_last` behaves exactly as it does for `push`: pass `true` to
    /// update an in-place running count instead of stacking a new entry per
    /// occurrence.
    ///
    /// Crate-private on purpose: this only stamps and stores the entry, with
    /// none of `route`/`toast_history`/expiry-scheduling that every other
    /// native notice needs -- correct here solely because
    /// [`Self::set_native_condition`], the one remaining caller, decides
    /// persistence itself via the `condition` flag it sets right after this
    /// call, rather than through `route()`'s kind-based classification. A
    /// one-shot notice wants that classification, so it goes through
    /// [`crate::model::EngineModel::record_native_notice`] instead, which
    /// this method is not reachable around: there is no `pub` path to a
    /// `kind == "native"` entry from outside this module other than through
    /// it.
    pub(crate) fn push_native(&mut self, text: String, replace_last: bool) {
        self.push("native".to_string(), vec![(0, text)], replace_last);
    }

    /// Raises (`Some`) or retracts (`None`) the one locally-raised
    /// *condition* notice, through the same overlay as `push_native` and
    /// for the same reason: a native condition reaches the user over the
    /// message surface that already exists, never a second one built
    /// alongside it.
    ///
    /// A condition differs from `push_native`'s notice in lifetime, not in
    /// origin. `push_native` records that something happened, and the
    /// record stays true forever; a condition asserts that something *is
    /// true now* -- an engine that has stopped reading view's output, say --
    /// and must disappear by itself the moment it stops being true. At most
    /// one is ever shown, since a second simultaneous condition would need
    /// its own retraction and there is nothing to key one off. It is
    /// persistent while raised (see `MessageEntry::is_persistent`), so the
    /// keypresses that dismiss ordinary transient text leave it alone.
    ///
    /// Idempotent, and cheap enough to call unconditionally on every loop
    /// pass: re-asserting the text already showing changes nothing and
    /// reports so. Returns whether the visible set changed, which is the
    /// caller's cue to repaint.
    #[must_use]
    pub fn set_native_condition(&mut self, text: Option<&str>) -> bool {
        let Some(text) = text else {
            let before = self.entries.len();
            self.entries.retain(|e| !e.condition);
            return self.entries.len() != before;
        };
        if let Some(raised) = self.entries.iter_mut().find(|e| e.condition) {
            let content = vec![(0, text.to_string())];
            if raised.content == content {
                return false;
            }
            raised.content = content;
            return true;
        }
        // raised through `push_native` and marked afterwards, rather than
        // built here: the flush stamp every entry carries keeps exactly one
        // source, and a condition is a native notice in every respect but
        // its lifetime
        self.push_native(text.to_string(), false);
        if let Some(raised) = self.entries.last_mut() {
            raised.condition = true;
        }
        true
    }

    /// Marks one full paint cycle as having happened -- one call per
    /// `Flush` UI event -- so that a transient entry's age in frames, and
    /// therefore whether it has survived long enough to be dismissable, is
    /// answerable at all.
    pub fn note_flush(&mut self) {
        self.flush_generation = self.flush_generation.wrapping_add(1);
    }

    /// Drops every transient entry that has already survived at least one
    /// full paint cycle since it was shown, leaving `is_persistent` entries
    /// and -- while `cmdline_open` -- `is_prompt` ones in place. Called
    /// from `update` on the user's next keypress: gives an info-level toast
    /// a readable duration bounded by real user activity -- an event the
    /// clockless model already receives -- rather than a wall-clock
    /// timer the runtime never delivers to `update`. An entry pushed in the same
    /// flush generation as the pending keypress has not necessarily been
    /// painted even once yet, so it survives this pass and is only
    /// dismissed on the *next* keypress instead, guaranteeing every
    /// transient toast is visible for at least one frame. Returns whether
    /// anything was actually dropped, so the caller knows whether to mark
    /// the model dirty for a repaint.
    #[must_use]
    pub fn dismiss_transient_on_keypress(&mut self, cmdline_open: bool) -> bool {
        let before = self.entries.len();
        let current = self.flush_generation;
        self.entries.retain(|e| {
            e.is_persistent() || (cmdline_open && e.is_prompt()) || e.shown_at_flush == current
        });
        self.entries.len() != before
    }

    /// The physical lines actually visible in a toast box `max_rows` tall:
    /// every entry that outranks transient text -- the error/warn kinds and
    /// an unanswered prompt's question -- keeps its lines, in their original
    /// arrival order; the remaining row budget is filled with the most
    /// recent transient lines, evicting the oldest transient lines first
    /// when the log needs more rows than the box has. Only in the extreme
    /// case where those alone exceed `max_rows` does eviction reach into
    /// them too (oldest first) -- the sole remaining way an error, warning
    /// or question line can still be dropped, and never merely because
    /// other messages arrived after it. Without this priority, a burst of
    /// ordinary info messages could silently push an unread error off the
    /// visible stack with neither an explicit `msg_clear` nor a replace
    /// ever happening, which is exactly the "persist until dismissed or
    /// replaced" contract broken by a plain recency-only trim.
    ///
    /// Each returned line is one span, carrying [`StyleRole::Plain`]
    /// (`crate::native::views::StyleRole`): a toast has no per-segment
    /// structure to preserve, so a single honest span is the whole row --
    /// not a placeholder for styling nobody asked for yet.
    #[must_use]
    pub fn visible_lines(&self, max_rows: usize) -> Vec<Vec<Span>> {
        let all: Vec<(bool, String)> = self
            .entries
            .iter()
            .flat_map(|e| {
                let persistent = e.outranks_transient();
                e.lines().into_iter().map(move |l| (persistent, l))
            })
            .collect();
        let overflow = all.len().saturating_sub(max_rows);
        if overflow == 0 {
            return all.into_iter().map(|(_, l)| vec![Span::plain(l)]).collect();
        }
        let mut remaining = overflow;
        let mut keep = vec![true; all.len()];
        for target_persistent in [false, true] {
            if remaining == 0 {
                break;
            }
            for (i, (persistent, _)) in all.iter().enumerate() {
                if remaining == 0 {
                    break;
                }
                if *persistent == target_persistent && keep[i] {
                    keep[i] = false;
                    remaining -= 1;
                }
            }
        }
        all.into_iter()
            .zip(keep)
            .filter_map(|((_, l), k)| k.then_some(vec![Span::plain(l)]))
            .collect()
    }
}

/// The open tabs, present once nvim has sent at least one `tabline_update`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TablineState {
    pub current: TabHandle,
    pub tabs: Vec<TabEntry>,
}

/// The completion popup menu's current items and selection, present only
/// while it is open (`popupmenu_show`..`popupmenu_hide`).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct PopupmenuState {
    pub items: Vec<PmItem>,
    pub selected: i64,
    pub row: u64,
    pub col: u64,
    pub grid: i64,
}

impl PopupmenuState {
    /// Whether this popup is cmdline-sourced (its completion candidates
    /// come from the command line, e.g. `:set nu<Tab>`) rather than
    /// buffer-anchored (e.g. insert-mode keyword completion). `grid` is the
    /// wire's own distinguishing field, captured live in
    /// `docs/palette-popupmenu-source-wire-capture.md`: `-1` is not a grid
    /// handle nvim ever assigns to a window, so a cmdline-sourced popup
    /// sends it as a sentinel; every buffer-anchored popup carries a real,
    /// non-negative grid handle instead. The command palette renders a
    /// cmdline-sourced popup's items inline as its own completion rows and
    /// must never also let it reach the plain `Popupmenu` layer, or the
    /// same candidates would paint twice in two different places.
    #[must_use]
    pub fn is_cmdline_sourced(&self) -> bool {
        self.grid < 0
    }
}

/// Which surface currently owns input focus. Read from
/// [`Model::focus`]; never stored.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// The embedded nvim engine's grid: keys, paste, and mouse route to
    /// `RpcCall`s.
    Engine,
    /// The native overlay identified by `OverlayId` owns the keyboard: keys
    /// and paste are consumed by that overlay instead of reaching the
    /// engine. Closing it is overlay-kind-specific rather than a generic
    /// `<Esc>` rule: a `Prompt` overlay forwards a key it accepts (its
    /// choice letters, `<CR>`, `<Esc>`) to the engine instead of closing
    /// itself, since the engine, not view, owns resolving it; it closes
    /// later, lazily, on the first key observed once the engine has
    /// actually hidden its cmdline. Mouse input is the exception, routing
    /// by position through [`Model::overlay_at`] rather than by focus.
    Native(OverlayId),
}

/// One open native overlay: which overlay it is, how much of the terminal
/// it covers, and the feature state it paints and routes from.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct Overlay {
    /// Identity, stable for as long as the overlay stays open, so an input
    /// routed to an overlay can be checked against the one that was on top
    /// when the frame was painted.
    pub id: OverlayId,
    /// The share of the terminal it covers; resolved to cells by
    /// [`Model::overlay_rect`].
    pub geometry: OverlayBox,
    /// Which feature this overlay belongs to, and its state.
    pub kind: OverlayKind,
}

/// Which native feature an overlay belongs to, carrying that feature's own
/// state.
///
/// **Feature state lives here, inside the stack element, and never in a
/// parallel `Option` field on [`Model`].** A `Model { picker:
/// Option<PickerState> }` beside the stack is two facts (open-ness and
/// presence) that must agree with nothing making them agree, which is the
/// defect deriving focus from the stack exists to remove. Holding the state
/// in the element makes an overlay that is open with no state, or holds
/// state while closed, unrepresentable.
///
/// `id` and `geometry` are struct fields rather than per-variant payloads
/// for the matching reason: no variant can forget them, and
/// [`Model::focus`] and [`Model::overlay_at`] stay total without a
/// per-feature arm.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum OverlayKind {
    /// A modal confirm-class prompt -- nvim blocked in its own input loop,
    /// waiting for an answer. See [`crate::native::prompt::PromptState`].
    Prompt(crate::native::prompt::PromptState),
    /// A fuzzy picker over files, buffers, or a live grep. Unlike a prompt,
    /// a picker never blocks nvim, so it can sit under a prompt on the
    /// stack: opening a confirm dialog while a picker is open pushes the
    /// prompt on top without closing the picker underneath, and
    /// [`Model::focus`] resolves to the prompt until it closes. The same
    /// rule holds from the other direction too: a `FeatureInvoke` opening a
    /// picker while a blocked-engine `Prompt` is already topmost inserts it
    /// beneath the prompt instead of stealing focus (via
    /// [`Model::insert_overlay_beneath_top`]), rather than pushing on top
    /// of it. See [`crate::native::picker::PickerState`].
    Picker(crate::native::picker::PickerState),
    /// The file tree sidebar, anchored flush left and full height rather
    /// than centered like a picker or prompt (see
    /// [`crate::native::geometry::Anchor::Left`]). Pushed on top like a
    /// picker, with the identical beneath-a-blocked-prompt fallback: `view`
    /// has no simultaneous multi-pane focus model, only a single topmost
    /// focus target, so a tree open beside an active engine buffer is one
    /// stack entry the same way a picker is, not a second pane. Opening a
    /// file from the tree issues `RpcCall::OpenFile` and pops this overlay,
    /// the same "acting on a row closes the picker" shape a picker's own
    /// selection has. See [`crate::native::tree::TreeState`].
    Tree(crate::native::tree::TreeState),
    /// A `:messages`-style browse of `ToastHistory`'s ring, snapshotted at
    /// open time. Centered like a picker; carries no navigation state of
    /// its own beyond the snapshot itself -- no key arm in `Msg::Key`'s
    /// `Focus::Native` match names this variant, so it falls to that
    /// match's generic fallback, which closes it on `<Esc>` the same way
    /// an overlay with no more specific key handling always has. See
    /// [`crate::native::palette::MessageHistoryState`].
    MessageHistory(crate::native::palette::MessageHistoryState),
}

/// Opaque identifier for an open native overlay, handed out by
/// [`Model::push_overlay`]. Unique among open overlays by construction, and
/// never reused, so a token outliving its overlay names nothing rather than
/// aliasing a later one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayId(pub u64);

/// Who owns the mouse gesture in flight; see [`Model::mouse_capture`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseCapture {
    /// The engine grid received the press, so the rest of the gesture is
    /// forwarded to it wherever the pointer travels.
    Engine,
    /// The named overlay received the press.
    Overlay(OverlayId),
}

/// Detected terminal capabilities.
///
/// `tier` is coarse UX vocabulary; the probed bits are what gates behavior
/// (BSU/ESU gates on `caps.sync`, never on tier alone).
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct TermCaps {
    pub tier: Tier,
    pub sync: bool,
    pub truecolor: bool,
    pub kitty_kbd: bool,
}

impl Default for TermCaps {
    /// Conservative defaults used before any capability probe runs: no
    /// probe is assumed to have succeeded. Routed through [`Self::from_probe`]
    /// (all-false) rather than hand-coded, so the tier-derivation formula
    /// still lives in exactly one place and a default of all-false booleans
    /// can never disagree with what `from_probe(false, false, false)` would
    /// derive for `tier`.
    fn default() -> Self {
        Self::from_probe(false, false, false)
    }
}

impl TermCaps {
    /// Builds capabilities from the three probed booleans, deriving `tier`
    /// the same way for every caller (auto-detection and the `--tier`
    /// override both funnel through this, so the derivation rule lives in
    /// exactly one place): `sync && truecolor && kitty_kbd` is `Full`,
    /// `truecolor` alone is `Standard`, anything else is `Basic`.
    ///
    /// `#[non_exhaustive]` keeps `TermCaps` from being struct-literal
    /// constructed outside this crate, but the terminal probe that
    /// discovers these booleans can only live in `view-tui` (only that
    /// crate touches the terminal), so this constructor is the sanctioned
    /// crossing point.
    #[must_use]
    pub fn from_probe(sync: bool, truecolor: bool, kitty_kbd: bool) -> Self {
        let tier = if sync && truecolor && kitty_kbd {
            Tier::Full
        } else if truecolor {
            Tier::Standard
        } else {
            Tier::Basic
        };
        Self {
            tier,
            sync,
            truecolor,
            kitty_kbd,
        }
    }
}

/// Coarse terminal capability tier.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Full,
    Standard,
    Basic,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::events::{TabEntry, TabHandle};

    /// `shown_at_flush` is private bookkeeping the tests below don't
    /// exercise; every construction goes through `Messages::push` so it
    /// gets stamped consistently rather than being touched directly here.
    fn entry(kind: &str, content: Vec<(u64, String)>) -> MessageEntry {
        let mut messages = Messages::default();
        messages.push(kind.to_string(), content, false);
        messages.entries.into_iter().next().unwrap()
    }

    /// `visible_lines` returns one span-row per line; these tests only care
    /// about the text each row carries (a toast has no per-segment styling
    /// to assert on), so this flattens each row back to a plain string.
    fn texts(lines: &[Vec<Span>]) -> Vec<String> {
        lines
            .iter()
            .map(|spans| spans.iter().map(|s| s.text.as_str()).collect())
            .collect()
    }

    #[test]
    fn message_entry_lines_splits_embedded_newlines_into_separate_physical_lines() {
        let e = entry("echoerr", vec![(0, "first line\nsecond line".into())]);
        assert_eq!(e.lines(), vec!["first line", "second line"]);
    }

    #[test]
    fn message_entry_lines_joins_chunks_before_splitting() {
        // a real msg_show can carry the break inside one chunk's own text
        // (a wrapped `emsg` continuation) or split across chunk boundaries
        // (differing highlight per segment); both must land on the correct
        // physical line, so joining happens before splitting, not after
        let e = entry(
            "echoerr",
            vec![(0, "one\ntwo".into()), (1, "-continued".into())],
        );
        assert_eq!(e.lines(), vec!["one", "two-continued"]);
    }

    #[test]
    fn message_entry_lines_single_line_message_yields_exactly_one_line() {
        let e = entry("echomsg", vec![(0, "hello".into())]);
        assert_eq!(e.lines(), vec!["hello"]);
    }

    #[test]
    fn is_persistent_matches_every_error_and_warning_kind_plus_a_raised_condition() {
        for kind in [
            "emsg",
            "echoerr",
            "wmsg",
            "lua_error",
            "rpc_error",
            "shell_err",
        ] {
            assert!(
                entry(kind, vec![]).is_persistent(),
                "{kind} must be persistent"
            );
        }
        for kind in [
            "echo",
            "echomsg",
            "native",
            "progress",
            "quickfix",
            "confirm",
            "shell_out",
            "shell_cmd",
            "shell_ret",
            "",
        ] {
            assert!(
                !entry(kind, vec![]).is_persistent(),
                "{kind} must not be persistent"
            );
        }
        // the arm no kind can reach: a raised condition carries the same
        // "native" kind the loop above requires to be transient, and is
        // persistent on the strength of being a condition alone
        let mut messages = Messages::default();
        assert!(messages.set_native_condition(Some("still true")));
        let raised = messages.entries.first().unwrap();
        assert_eq!(raised.kind, "native");
        assert!(raised.is_persistent());
    }

    #[test]
    fn only_the_confirm_kind_is_bound_to_an_open_prompt() {
        assert!(entry("confirm", vec![]).is_prompt());
        for kind in ["emsg", "echomsg", "shell_err", "wmsg", ""] {
            assert!(
                !entry(kind, vec![]).is_prompt(),
                "{kind} must not be a prompt"
            );
        }
    }

    #[test]
    fn a_confirm_question_survives_a_keypress_that_the_prompt_is_still_waiting_on() {
        // nvim re-arms a confirm prompt on a key that answers none of its
        // choices, and re-emits only `cmdline_show` -- never the `msg_show`
        // carrying the question. Dismissing the question on that keypress
        // leaves the user an answer line with nothing to answer.
        let mut messages = Messages::default();
        messages.push(
            "confirm".to_string(),
            vec![(0, "Save changes?".into())],
            false,
        );
        messages.note_flush();
        assert!(!messages.dismiss_transient_on_keypress(true));
        assert_eq!(messages.entries.len(), 1);
    }

    #[test]
    fn a_confirm_question_is_dismissed_once_its_prompt_has_closed() {
        // the other side of the rule: with the prompt gone the question is
        // ordinary transient text, so it must not outlive user activity the
        // way an error does
        let mut messages = Messages::default();
        messages.push(
            "confirm".to_string(),
            vec![(0, "Save changes?".into())],
            false,
        );
        messages.note_flush();
        assert!(messages.dismiss_transient_on_keypress(false));
        assert!(messages.entries.is_empty());
    }

    #[test]
    fn an_open_prompts_question_outranks_transient_lines_for_the_visible_rows() {
        let mut messages = Messages::default();
        messages.push(
            "confirm".to_string(),
            vec![(0, "Save changes?".into())],
            false,
        );
        messages.push("echomsg".to_string(), vec![(0, "info".into())], false);
        assert_eq!(texts(&messages.visible_lines(1)), vec!["Save changes?"]);
    }

    #[test]
    fn dismiss_transient_on_keypress_drops_transient_entries_seen_at_least_one_flush() {
        let mut messages = Messages::default();
        messages.push("echomsg".to_string(), vec![(0, "info".into())], false);
        // not yet flushed: must survive this pass, guaranteeing at least
        // one painted frame before an info toast can be dismissed
        assert!(!messages.dismiss_transient_on_keypress(false));
        assert_eq!(messages.entries.len(), 1);

        messages.note_flush();
        assert!(messages.dismiss_transient_on_keypress(false));
        assert!(messages.entries.is_empty());
    }

    #[test]
    fn dismiss_transient_on_keypress_never_drops_a_persistent_entry() {
        let mut messages = Messages::default();
        messages.push("echoerr".to_string(), vec![(0, "boom".into())], false);
        messages.note_flush();
        messages.note_flush();
        assert!(!messages.dismiss_transient_on_keypress(false));
        assert_eq!(messages.entries.len(), 1);
    }

    #[test]
    fn a_condition_notice_is_raised_once_and_retracted_once() {
        let mut messages = Messages::default();
        assert!(messages.set_native_condition(Some("engine stalled")));
        assert_eq!(texts(&messages.visible_lines(4)), vec!["engine stalled"]);
        // re-asserting the same condition is not a change, so a caller that
        // asks on every pass never repaints for it
        assert!(!messages.set_native_condition(Some("engine stalled")));
        assert_eq!(messages.entries.len(), 1);

        assert!(messages.set_native_condition(None));
        assert!(messages.entries.is_empty());
        assert!(!messages.set_native_condition(None));
    }

    #[test]
    fn re_raising_a_condition_with_new_text_replaces_it_rather_than_stacking() {
        let mut messages = Messages::default();
        assert!(messages.set_native_condition(Some("first")));
        assert!(messages.set_native_condition(Some("second")));
        assert_eq!(texts(&messages.visible_lines(4)), vec!["second"]);
        assert_eq!(messages.entries.len(), 1);
    }

    #[test]
    fn a_condition_notice_survives_the_keypresses_that_dismiss_transient_text() {
        let mut messages = Messages::default();
        assert!(messages.set_native_condition(Some("engine stalled")));
        messages.push("echomsg".to_string(), vec![(0, "info".into())], false);
        messages.note_flush();
        messages.note_flush();
        assert!(messages.dismiss_transient_on_keypress(false));
        assert_eq!(texts(&messages.visible_lines(4)), vec!["engine stalled"]);
    }

    #[test]
    fn a_progress_message_replaces_its_own_previous_line_not_the_raised_condition() {
        // the canonical wedge is an nvim too busy to read its stdin while
        // still flushing progress lines, so a raised condition and a
        // replacing msg_show overlap exactly
        let mut messages = Messages::default();
        messages.push("progress".to_string(), vec![(0, "[1/57]".into())], false);
        assert!(messages.set_native_condition(Some("engine stalled")));
        messages.push("progress".to_string(), vec![(0, "[2/57]".into())], true);
        assert_eq!(
            texts(&messages.visible_lines(4)),
            vec!["[2/57]", "engine stalled"]
        );
        assert_eq!(messages.entries.len(), 2);
    }

    #[test]
    fn a_replacing_message_with_only_the_condition_present_appends_instead() {
        let mut messages = Messages::default();
        assert!(messages.set_native_condition(Some("engine stalled")));
        messages.push("progress".to_string(), vec![(0, "[1/57]".into())], true);
        assert_eq!(
            texts(&messages.visible_lines(4)),
            vec!["engine stalled", "[1/57]"]
        );
    }

    #[test]
    fn retracting_a_condition_leaves_every_other_entry_alone() {
        let mut messages = Messages::default();
        messages.push("emsg".to_string(), vec![(0, "boom".into())], false);
        assert!(messages.set_native_condition(Some("engine stalled")));
        messages.push("echomsg".to_string(), vec![(0, "info".into())], false);
        assert!(messages.set_native_condition(None));
        assert_eq!(texts(&messages.visible_lines(4)), vec!["boom", "info"]);
    }

    #[test]
    fn a_condition_notice_keeps_its_row_when_a_burst_of_transient_text_overflows_the_box() {
        let mut messages = Messages::default();
        assert!(messages.set_native_condition(Some("engine stalled")));
        for i in 0..5 {
            messages.push("echomsg".to_string(), vec![(0, format!("info {i}"))], false);
        }
        assert_eq!(
            texts(&messages.visible_lines(2)),
            vec!["engine stalled", "info 4"],
            "a stall notice must not be evicted by the messages that follow it"
        );
    }

    #[test]
    fn visible_lines_returns_everything_when_it_fits() {
        let mut messages = Messages::default();
        messages.push("echomsg".to_string(), vec![(0, "a".into())], false);
        messages.push("echomsg".to_string(), vec![(0, "b".into())], false);
        assert_eq!(texts(&messages.visible_lines(5)), vec!["a", "b"]);
    }

    #[test]
    fn visible_lines_evicts_oldest_transient_lines_before_touching_persistent_ones() {
        let mut messages = Messages::default();
        messages.push("echoerr".to_string(), vec![(0, "error".into())], false);
        messages.push("echomsg".to_string(), vec![(0, "old info".into())], false);
        messages.push("echomsg".to_string(), vec![(0, "new info".into())], false);
        // box has room for 2 of the 3 lines: the persistent error must
        // never be the one evicted just because other messages arrived
        // after it, so the oldest transient line ("old info") goes instead
        assert_eq!(texts(&messages.visible_lines(2)), vec!["error", "new info"]);
    }

    #[test]
    fn visible_lines_falls_back_to_evicting_oldest_persistent_when_persistent_alone_overflows() {
        let mut messages = Messages::default();
        messages.push(
            "echoerr".to_string(),
            vec![(0, "first error".into())],
            false,
        );
        messages.push(
            "echoerr".to_string(),
            vec![(0, "second error".into())],
            false,
        );
        assert_eq!(texts(&messages.visible_lines(1)), vec!["second error"]);
    }

    #[test]
    fn with_term_size_prefills_dims_and_new_defaults_to_zero() {
        let m = Model::new();
        assert_eq!((m.term_width, m.term_height), (0, 0));
        let m = Model::with_term_size(80, 24);
        assert_eq!((m.term_width, m.term_height), (80, 24));
    }

    #[test]
    fn chrome_rows_is_zero_without_a_tabline_or_with_one_tab() {
        let mut m = Model::with_term_size(80, 24);
        assert_eq!(m.chrome_rows(), 0);
        m.engine.tabline = Some(TablineState {
            current: TabHandle(1),
            tabs: vec![TabEntry {
                tab: TabHandle(1),
                name: "a".into(),
            }],
        });
        assert_eq!(m.chrome_rows(), 0);
    }

    #[test]
    fn chrome_rows_is_one_once_more_than_one_tab_is_open() {
        let mut m = Model::with_term_size(80, 24);
        m.engine.tabline = Some(TablineState {
            current: TabHandle(1),
            tabs: vec![
                TabEntry {
                    tab: TabHandle(1),
                    name: "a".into(),
                },
                TabEntry {
                    tab: TabHandle(2),
                    name: "b".into(),
                },
            ],
        });
        assert_eq!(m.chrome_rows(), 1);
        assert_eq!(m.grid_target(), (80, 23));
    }

    #[test]
    fn grid_target_matches_term_size_with_no_chrome_reserved() {
        let m = Model::with_term_size(80, 24);
        assert_eq!(m.grid_target(), (80, 24));
    }
}
