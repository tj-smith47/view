//! Application state `update()` reads and mutates. No I/O, no rendering.

use std::path::PathBuf;

use crate::events::{ModeInfo, PmItem, TabEntry, TabHandle};
use crate::grid::{Grid, GridOp};
use crate::hl::{HlAttr, HlTable, ProbedDefaults};
use crate::native::geometry::{self, OverlayBox, OverlayRect};
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
    /// [`Model::push_overlay`] and [`Model::pop_focused_overlay`], for the same
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
    /// The `ext_*` surfaces this session actually attached with, written
    /// once by the binary at attach ([`Model::attach_surfaces`]) from the
    /// same `[native]` answers `statusline_enabled` and `palette_enabled`
    /// come from.
    ///
    /// Private, read through [`Model::owns`] and
    /// [`Model::attached_surfaces`]: "did this session externalize that
    /// surface?" is a question a conflict notice, a restart's re-attach and
    /// the `cmdheight` takeover all ask, and each answering it from its own
    /// copy of the `[native]` table is how the answers come to disagree.
    /// Defaults to the full set, matching the config-absent session that
    /// attaches everything.
    ext_surfaces: Vec<crate::native::ext::Ext>,
    /// Whether the `[native]` table [`Model::attach_surfaces`] recorded was
    /// read from a `view.toml` at all.
    ///
    /// Beside `ext_surfaces` rather than derived from it, because it is the
    /// one thing that set cannot answer: a config that could not be parsed
    /// fails *open*, attaching every surface, so a user who wrote
    /// `palette = false` into a file with a typo above it gets exactly the
    /// set of someone who wrote nothing. Telling them to set a line they
    /// already set would be false, so a notice about an owned surface reads
    /// this before naming a remedy (`update::surface_conflict`).
    ///
    /// Defaults to `true`: an absent config is read successfully -- it says
    /// nothing, which is the full experience -- and only the fail-open leg
    /// in `crates/view/src/main.rs` clears it.
    config_was_read: bool,
    /// Which float identities have been seen drawing over a surface view
    /// owns, so the second sighting adds to one notice instead of raising a
    /// second one. Session-lifetime, like the conflict it records: the
    /// remedy is a config line that takes effect at the next start.
    pub(crate) surface_conflicts: crate::native::surfaces::SurfaceConflicts,
    /// The working directory a relative [`crate::native::picker::Source`]
    /// resolves against, learned once at startup
    /// ([`Model::with_cwd`]) since `update()` has no filesystem access to
    /// ask for it itself. Empty until startup sets it.
    pub cwd: PathBuf,
    /// Whether this session's project has been granted AI agent access,
    /// resolved once at startup from `view-ai`'s own trust store
    /// (`crates/view/src/main.rs` seeds it the same way `cwd` and
    /// `statusline_enabled` are) and folded forward by
    /// `Msg::AiTrustResolved` once the user answers the gate
    /// `Msg::FeatureInvoke`'s `ai` arm opens. Plain data: `update()` reads
    /// it and nothing else, never the `view-ai` store this fact came from
    /// -- see `Effect::AiTrustSet`'s own doc for why the crossing has to be
    /// a bool rather than a call.
    pub ai_trusted: bool,
    /// Whether the `ai` feature is on at all, resolved once at startup from
    /// `view-ai`'s own `AiConfig` (`crates/view/src/main.rs` seeds it the
    /// same way `ai_trusted` is, right beside it). Checked by
    /// `Msg::FeatureInvoke`'s `ai` arm ahead of the trust gate: a disabled
    /// feature must not prompt for trust either, since there is nothing to
    /// trust it for. Defaults to `true`, matching `AiConfig`'s own
    /// default-on: a session that never seeds this (most tests, and a
    /// startup with no `view.toml` to read) gets the full experience, the
    /// same "absent config changes nothing" contract every other native
    /// feature's enabled bit holds.
    pub ai_enabled: bool,
    /// The agent panel's share of the terminal width, in percent: seeded
    /// once at startup from `[ai] panel_width` and stepped by the panel's
    /// own resize keys for the rest of the session.
    ///
    /// Held here rather than only on the open overlay's own
    /// [`OverlayBox`] because the panel outlives its overlay: a width
    /// chosen, then the panel closed and reopened, must come back the width
    /// the user left it at, the same way [`Model::ai_panel`] keeps the
    /// session behind a hidden sidebar.
    pub ai_panel_width_pct: u16,
    /// Where a review's file is shown when no window already has it open,
    /// seeded once at startup from `[ai.review] open_target` on the same
    /// terms as [`Model::ai_panel_width_pct`]. Plain data, carried into
    /// every `RpcCall::ReviewShow` this session issues; see
    /// [`crate::msg::ReviewOpenTarget`] for why the default is the current
    /// window.
    pub ai_review_open_target: crate::msg::ReviewOpenTarget,
    /// The tree sidebar's share of the terminal width, in percent, on the
    /// same terms as [`Model::ai_panel_width_pct`]: seeded from
    /// `[native] tree_width` and stepped by the sidebar's resize keys.
    pub tree_width_pct: u16,
    /// Which keys each rebindable action answers to, seeded once at
    /// startup from `[keys]`. One set for every surface, so no two can
    /// answer the same key differently.
    pub key_bindings: crate::native::keys::KeyBindings,
    /// The first key of a chord, waiting on the keystroke that decides what
    /// it meant, or `None` while none is part way through.
    ///
    /// Consumed by the very next key a focused sidebar handles, whatever
    /// that key turns out to mean: a prefix that outlived the keystroke
    /// after it would silently re-point some later, unrelated press at a
    /// resize.
    pub(crate) pending_chord: Option<String>,
    /// Supervision's memory of the current wedge episode -- which wedge the
    /// user has already been offered a modal for, so a dismissed one stays
    /// dismissed while the banner behind it keeps re-asserting; see
    /// [`crate::native::supervision::SupervisionState`].
    pub supervision: crate::native::supervision::SupervisionState,
    /// The display-only glyphs typing is predicted to produce before the
    /// engine's own redraw confirms them; see
    /// [`crate::native::speculate::SpeculateState`].
    ///
    /// Part of the model rather than state the painter keeps on the side,
    /// because `view_surface::render` is defined as a function of this
    /// struct alone: a speculated cell painted from anywhere else would make
    /// an incrementally updated frame differ from a rebuilt one, which is
    /// exactly what the surface cache's equivalence guard exists to catch.
    pub speculate: crate::native::speculate::SpeculateState,
    /// The agent session's transcript, pending permission, pending edits,
    /// and stats -- session lifetime, not overlay lifetime. Kept here rather
    /// than inside [`OverlayKind::Ai`] so that closing the sidebar only
    /// hides it: a `session/update` chunk streamed while the panel is
    /// closed still has somewhere to fold, and reopening finds the session
    /// exactly as the user left it rather than a fresh, empty one.
    pub ai_panel: crate::native::ai_panel::AiPanelState,
    /// The agent's own file reads and writes that are part way through.
    ///
    /// Beside the panel rather than inside it for the reason the panel sits
    /// beside the overlay: an agent reads and writes files whether or not
    /// anything about it is on screen, and a request that lost its record
    /// when the sidebar closed would leave the agent waiting on an answer
    /// nothing was left to send.
    pub(crate) ai_fs: crate::native::ai_fs::AiFsState,
    /// The next `request_id` for a `RpcCall::Checktime` this crate issues.
    ///
    /// Its own counter, not [`Model::next_hidden_generation`]'s: that one is
    /// shared specifically because two different callers both resolve
    /// through the identical `Msg::HiddenBufferLoaded` reply and a second
    /// counter would let them collide on it. `Msg::CheckTimeReply` is a
    /// distinct reply type nothing else answers into, so there is no
    /// collision to avoid sharing against -- a counter of its own keeps
    /// that ownership legible rather than borrowing one whose doc explains a
    /// reason that does not apply here.
    checktime_generation: u64,
    /// The confirming probes asked for by
    /// [`crate::msg::Effect::ReprobeExternalWrite`] and not yet answered:
    /// the `request_id` of the `RpcCall::Checktime` each one drove, beside
    /// the path it was asked about. Written through
    /// [`Model::expect_file_gone_confirmation`],
    /// [`Model::take_file_gone_confirmation`] and
    /// [`Model::forget_file_gone_confirmation`].
    ///
    /// What it buys is the difference between a first `FileGone` answer and
    /// a confirmed one: the first is not yet evidence -- a save that unlinks
    /// its target before rewriting it can be probed between its two halves
    /// -- so it schedules a re-probe instead of speaking, and only the reply
    /// that re-probe itself provoked is allowed to announce anything.
    ///
    /// Keyed on the request rather than on the path alone so an entry can
    /// never outlive the episode that made it: request ids are minted once
    /// and never reused, so an entry whose reply never arrives (the engine
    /// died holding it) can match nothing later, and a fresh episode for the
    /// same path replaces it rather than inheriting it. That bounds the
    /// `Vec` at one entry per path that has had a second look asked for and
    /// not yet resolved -- an entry whose reply can never arrive stays,
    /// inert, until a fresh episode for its path replaces it -- and the
    /// bound is enforced by replace-on-insert rather than asserted here.
    pending_file_gone_probes: Vec<(u64, PathBuf)>,
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
                float_absorption: crate::native::surfaces::FloatAbsorption::default(),
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
            ext_surfaces: crate::native::ext::ALL.to_vec(),
            config_was_read: true,
            surface_conflicts: crate::native::surfaces::SurfaceConflicts::default(),
            cwd: PathBuf::new(),
            ai_trusted: false,
            ai_enabled: true,
            ai_panel_width_pct: geometry::DEFAULT_PANEL_WIDTH_PCT,
            ai_review_open_target: crate::msg::ReviewOpenTarget::default(),
            tree_width_pct: geometry::DEFAULT_PANEL_WIDTH_PCT,
            key_bindings: crate::native::keys::KeyBindings::default(),
            pending_chord: None,
            supervision: crate::native::supervision::SupervisionState::default(),
            speculate: crate::native::speculate::SpeculateState::default(),
            ai_panel: crate::native::ai_panel::AiPanelState::new(),
            ai_fs: crate::native::ai_fs::AiFsState::default(),
            checktime_generation: 0,
            pending_file_gone_probes: Vec::new(),
        }
    }

    /// Records the `ext_*` set this session attached with.
    ///
    /// Called by the binary once the `[native]` table has been read and
    /// before `nvim_ui_attach` is issued, with exactly the list handed to
    /// the attach: a session that recorded a different set from the one it
    /// sent would answer [`Self::owns`] about a surface nvim never gave it.
    pub fn attach_surfaces(&mut self, surfaces: Vec<crate::native::ext::Ext>) {
        self.ext_surfaces = surfaces;
    }

    /// Whether this session externalized `surface`, so view renders it and
    /// the user's plugins see it taken.
    ///
    /// `false` means nvim still draws that surface into the grid itself,
    /// which is what turning the owning native feature off asks for.
    #[must_use]
    pub fn owns(&self, surface: crate::native::ext::Ext) -> bool {
        self.ext_surfaces.contains(&surface)
    }

    /// Records that this session never read a `view.toml`'s `[native]`
    /// table -- the file exists and could not be parsed, so the surfaces
    /// [`Self::attach_surfaces`] recorded are the fail-open default rather
    /// than an answer to what the user wrote.
    ///
    /// Called from the same fail-open arm that raises the notice about the
    /// unreadable file, so the two can never disagree about which happened.
    pub fn note_config_unread(&mut self) {
        self.config_was_read = false;
    }

    /// Whether the `[native]` switches this session attached with were
    /// actually read from the user's config, on the terms
    /// [`Self::note_config_unread`] sets.
    ///
    /// The one caller is the notice that would otherwise name a `[native]`
    /// line as a remedy: see [`Self::note_config_unread`] for why that
    /// line is a lie on the fail-open leg.
    #[must_use]
    pub fn config_was_read(&self) -> bool {
        self.config_was_read
    }

    /// The whole attached set, for the one caller that has to re-send it:
    /// a replacement engine attaches with the surfaces the session it
    /// replaces was given, never with the default set.
    #[must_use]
    pub fn attached_surfaces(&self) -> &[crate::native::ext::Ext] {
        &self.ext_surfaces
    }

    /// The next `request_id` for a `RpcCall::Checktime` this crate issues,
    /// from its own counter rather than [`Model::next_hidden_generation`]:
    /// `Msg::CheckTimeReply` is a reply type nothing else answers into, so
    /// there is no collision with another caller to avoid by sharing one.
    pub fn next_checktime_request_id(&mut self) -> u64 {
        self.checktime_generation += 1;
        self.checktime_generation
    }

    /// Records that the `RpcCall::Checktime` numbered `request_id` is the
    /// confirming second look at `path`, so its reply -- and no other -- may
    /// announce that the path is gone.
    ///
    /// Any earlier probe of the same path is dropped as it goes in: a second
    /// nomination arriving while the first probe is still in flight starts
    /// the episode over rather than stacking a duplicate, which is what
    /// keeps one entry per path the true bound rather than a hopeful one.
    pub(crate) fn expect_file_gone_confirmation(
        &mut self,
        request_id: u64,
        path: &std::path::Path,
    ) {
        self.forget_file_gone_confirmation(path);
        self.pending_file_gone_probes
            .push((request_id, path.to_path_buf()));
    }

    /// Whether the reply numbered `request_id` is the confirming probe of
    /// `path`, consuming the record if it is.
    ///
    /// Consumed for the bound rather than for correctness: replies wear
    /// ids minted after any record already written, so a resolved record
    /// left in place could never confirm a later episode -- but it would
    /// sit in the `Vec` until the same path was probed again, and the
    /// bound the field's doc gives counts entries still awaiting
    /// resolution, not everything ever asked.
    #[must_use]
    pub(crate) fn take_file_gone_confirmation(
        &mut self,
        request_id: u64,
        path: &std::path::Path,
    ) -> bool {
        let before = self.pending_file_gone_probes.len();
        self.pending_file_gone_probes
            .retain(|(id, p)| !(*id == request_id && p == path));
        self.pending_file_gone_probes.len() != before
    }

    /// Drops any confirming probe outstanding for `path`, so the next
    /// `FileGone` answer it gives is confirmed from the start again rather
    /// than believed on the strength of an answer the path has since
    /// contradicted.
    pub(crate) fn forget_file_gone_confirmation(&mut self, path: &std::path::Path) {
        self.pending_file_gone_probes.retain(|(_, p)| p != path);
    }

    /// The next generation for a `RpcCall::LoadHidden` this crate issues,
    /// from the one counter every hidden-buffer resolve shares.
    ///
    /// The single point of increment on purpose: a diff review's resolve
    /// and an agent filesystem request's resolve are answered by the same
    /// `Msg::HiddenBufferLoaded`, and two counters would let both wear the
    /// same number and each fold the other's reply. See
    /// [`AiPanelState::next_hidden_generation`](crate::native::ai_panel::AiPanelState::next_hidden_generation).
    pub fn next_hidden_generation(&mut self) -> u64 {
        self.ai_panel_mut().next_hidden_generation()
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

    /// This model with `cwd` pre-filled from the process's working
    /// directory at startup, before any picker ever opens.
    ///
    /// A builder step rather than a second constructor, so it composes with
    /// [`Model::with_term_size`]: startup needs both, and a constructor
    /// that could only supply one of them is what left the binary
    /// assigning the field directly -- two ways to establish the same
    /// state, one of which no test ever exercised.
    #[must_use]
    pub fn with_cwd(mut self, cwd: PathBuf) -> Self {
        self.cwd = cwd;
        self
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

    /// Who owns input this frame: the topmost focus-taking overlay, or the
    /// engine when none is open.
    ///
    /// Derived from [`Model::overlays`] rather than stored alongside it. A
    /// stored focus is a second fact that has to agree with overlay
    /// presence, and every close is a chance to leave it naming an overlay
    /// that is gone, which routes keys into nothing. Derivation makes that
    /// state unrepresentable.
    #[must_use]
    pub fn focus(&self) -> Focus {
        match self.focused_overlay() {
            Some(overlay) => Focus::Native(overlay.id),
            None => Focus::Engine,
        }
    }

    /// Whether an overlay of this kind takes the keyboard while it is open,
    /// with no further state consulted.
    ///
    /// Every kind does except [`OverlayKind::EngineBusy`] and
    /// [`OverlayKind::Ai`] -- `Ai` unconditionally here, since this form
    /// cannot see [`Model::ai_panel`]'s own `focused` flag. It is what it
    /// is: the pure, kind-only question `open_ai_panel`'s insert-beneath
    /// check needs, evaluated against whatever overlay already sits on top
    /// of the stack -- never `OverlayKind::Ai` itself, since that call only
    /// runs while the panel is closed. Every other caller wants
    /// [`Self::takes_focus_now`] instead, which layers the panel's own
    /// state on top of this for `Ai`.
    ///
    /// `EngineBusy` is raised by view noticing something rather than by the
    /// user asking for it, and is on screen precisely when the engine may
    /// be slow to answer. A user who keeps typing at a long operation has
    /// always had those keystrokes queued and applied on catch-up, so an
    /// annunciator that consumed them would turn a slow operation into lost
    /// work. It answers its own choice keys, and every other key routes as
    /// though it were not there.
    pub(crate) const fn takes_focus(kind: &OverlayKind) -> bool {
        !matches!(kind, OverlayKind::EngineBusy(_) | OverlayKind::Ai)
    }

    /// Whether `kind` takes the keyboard right now, on this model -- what
    /// every focus-resolution method below actually wants.
    ///
    /// Identical to [`Self::takes_focus`] for every kind but
    /// [`OverlayKind::Ai`]: the panel is non-modal by design (see that
    /// variant's own doc), so its mere presence on the stack must not
    /// redirect the engine's own keystrokes. It takes the keyboard only
    /// once the user has deliberately entered it -- `ai_entered`, read from
    /// [`crate::native::ai_panel::AiPanelState::focused`] -- never by side
    /// effect of an agent auto-opening it. Takes the flag as a plain `bool`
    /// rather than `&self`, so [`Self::focused_overlay_mut`] and
    /// [`Self::pop_focused_overlay`] can read `ai_panel.focused` once,
    /// ahead of borrowing `overlays` mutably, instead of needing both
    /// borrows live at the same time.
    const fn takes_focus_now(kind: &OverlayKind, ai_entered: bool) -> bool {
        match kind {
            OverlayKind::Ai => ai_entered,
            other => Self::takes_focus(other),
        }
    }

    /// The overlay [`Self::focus`] names, or `None` while the engine owns
    /// the keyboard.
    ///
    /// The kind-carrying form of [`Self::focus`], for a caller that has to
    /// know *which* feature holds the keys rather than merely that some
    /// overlay does -- `view-surface` places the real terminal caret in the
    /// surface that owns input, and an `OverlayId` alone cannot say which
    /// surface that is.
    #[must_use]
    pub fn focused_overlay(&self) -> Option<&Overlay> {
        let ai_entered = self.ai_panel.focused;
        self.overlays
            .iter()
            .rev()
            .find(|overlay| Self::takes_focus_now(&overlay.kind, ai_entered))
    }

    /// The topmost focus-taking overlay, for a feature that needs to fold
    /// its own state forward as input arrives.
    #[must_use]
    pub fn focused_overlay_mut(&mut self) -> Option<&mut Overlay> {
        let ai_entered = self.ai_panel.focused;
        self.overlays
            .iter_mut()
            .rev()
            .find(|overlay| Self::takes_focus_now(&overlay.kind, ai_entered))
    }

    /// Removes the overlay at `pos` and hands it back, releasing the mouse
    /// capture it held.
    ///
    /// The single removal point, so no closing path can forget the release:
    /// a capture left pointing at a closed overlay's id routes drags to an
    /// overlay that is no longer on screen, and every future one that
    /// happens to be assigned the same id.
    fn take_overlay_at(&mut self, pos: usize) -> Overlay {
        let removed = self.overlays.remove(pos);
        if let Some(MouseCapture::Overlay(held)) = self.mouse_capture {
            if removed.id == held {
                self.mouse_capture = None;
            }
        }
        removed
    }

    /// Closes the overlay [`Model::focus`] names, wherever it sits in the
    /// stack, and returns it.
    ///
    /// Not the top of the stack: a non-focus-taking overlay may be sitting
    /// above it, and popping that instead would close an annunciator the
    /// user never addressed while leaving the overlay they did address open.
    pub fn pop_focused_overlay(&mut self) -> Option<Overlay> {
        let ai_entered = self.ai_panel.focused;
        let pos = self
            .overlays
            .iter()
            .rposition(|overlay| Self::takes_focus_now(&overlay.kind, ai_entered))?;
        Some(self.take_overlay_at(pos))
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

    /// Re-sizes every open prompt overlay's box to the question it holds
    /// right now.
    ///
    /// A prompt's content outlives the box it was pushed with: a fresh
    /// question replaces a standing one in place (both locally-raised
    /// prompts do this rather than stacking a duplicate), and a box still
    /// sized to the previous question truncates the new one. Total over the
    /// stack rather than keyed to one prompt, because a replacement can
    /// land on an overlay that is not the focused one.
    pub fn resize_prompt_overlays(&mut self) {
        for overlay in &mut self.overlays {
            if let OverlayKind::Prompt(p) = &overlay.kind {
                overlay.geometry = p.overlay_box();
            }
        }
    }

    /// The open AI-trust prompt, wherever it sits in the stack -- not only
    /// when it is topmost, for the same reason [`Model::picker_mut`] looks
    /// past the top: a blocked engine `Prompt` can take focus above it (see
    /// `OverlayKind::Prompt`'s stacking rule in `update/mod.rs`'s
    /// `open_ai_trust_prompt`), and a second `:View ai` invoke while that is
    /// topmost must still find and replace the trust prompt underneath
    /// rather than stacking a duplicate beside it -- `focused_overlay_mut`
    /// alone only ever sees whatever overlay is on top.
    #[must_use]
    pub fn ai_trust_prompt_mut(&mut self) -> Option<&mut crate::native::prompt::PromptState> {
        self.overlays
            .iter_mut()
            .find_map(|overlay| match &mut overlay.kind {
                OverlayKind::Prompt(p) if p.ai_trust_project_root().is_some() => Some(p),
                _ => None,
            })
    }

    /// The open external-write conflict prompt for `path`, wherever it sits
    /// in the stack, on the same "not only when topmost" terms
    /// [`Model::ai_trust_prompt_mut`] documents. Keyed by `path`, unlike
    /// that single global prompt: two different files can each have their
    /// own pending conflict at once, and a caller resolving one `Checktime`
    /// reply must never touch another file's still-open prompt.
    #[must_use]
    pub fn external_write_conflict_prompt_mut(
        &mut self,
        path: &std::path::Path,
    ) -> Option<&mut crate::native::prompt::PromptState> {
        self.overlays
            .iter_mut()
            .find_map(|overlay| match &mut overlay.kind {
                OverlayKind::Prompt(p) if p.external_write_conflict_path() == Some(path) => Some(p),
                _ => None,
            })
    }

    /// Closes the open external-write conflict prompt for `path`, wherever
    /// it sits in the stack, and does nothing if there is none.
    ///
    /// Keyed by path and not by focus: the prompt whose question has gone
    /// stale is rarely the one the user is looking at, and closing whatever
    /// happens to hold focus would dismiss a different file's conflict --
    /// or an unrelated overlay -- while leaving the stale one open.
    ///
    /// Reports whether a prompt was standing, like the sibling closers: the
    /// withdrawal takes a question off the screen, so it is one of the two
    /// things that can make an unreadable path's reply worth a repaint --
    /// the other being the notice that replaces it, which the caller learns
    /// about from its own answer.
    #[must_use]
    pub(crate) fn close_external_write_conflict_prompt(&mut self, path: &std::path::Path) -> bool {
        let Some(pos) = self.overlays.iter().position(|overlay| {
            matches!(&overlay.kind, OverlayKind::Prompt(p)
                if p.external_write_conflict_path() == Some(path))
        }) else {
            return false;
        };
        self.take_overlay_at(pos);
        true
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
    /// [`OverlayKind::Tree`]'s stacking doc); [`Model::pop_focused_overlay`] alone
    /// would close the wrong overlay in that case.
    pub fn close_tree(&mut self) -> bool {
        let Some(pos) = self
            .overlays
            .iter()
            .position(|overlay| matches!(overlay.kind, OverlayKind::Tree(_)))
        else {
            return false;
        };
        self.take_overlay_at(pos);
        true
    }

    /// The agent session's persistent state: transcript, pending
    /// permission, pending edits, stats. Always present -- session lifetime
    /// is model lifetime, not overlay lifetime (see [`Model::ai_panel`]'s
    /// doc) -- so a streamed `session/update` chunk always has somewhere to
    /// fold, whether or not the sidebar is currently shown.
    ///
    /// No `#[must_use]` of its own: [`crate::native::ai_panel::AiPanelState`]
    /// already carries one, and a second on the accessor that returns it is
    /// clippy's own `double_must_use`.
    pub fn ai_panel_mut(&mut self) -> &mut crate::native::ai_panel::AiPanelState {
        &mut self.ai_panel
    }

    /// Read-only counterpart to [`Model::ai_panel_mut`], for reading the
    /// session without asserting mutable access to it. See that method's
    /// doc for why this carries no `#[must_use]` of its own either.
    pub fn ai_panel(&self) -> &crate::native::ai_panel::AiPanelState {
        &self.ai_panel
    }

    /// Whether the agent sidebar overlay is currently on the stack -- the
    /// overlay's own visibility, distinct from [`Model::ai_panel`], which
    /// answers whether a session exists at all (always, once
    /// [`Model::new`] has run). [`open_ai_panel`](crate::update)'s no-op
    /// check and the interleave tests both need this narrower question:
    /// the session can be live while the sidebar is closed.
    #[must_use]
    pub fn ai_panel_overlay_open(&self) -> bool {
        self.overlays
            .iter()
            .any(|overlay| matches!(overlay.kind, OverlayKind::Ai))
    }

    /// Closes the agent panel overlay, wherever it sits in the stack, and
    /// reports whether one was found to close. See [`Model::close_tree`]'s
    /// doc for why this searches by kind rather than closing only the
    /// topmost overlay. Hides the sidebar only: the session state in
    /// [`Model::ai_panel`] is untouched, so reopening finds it exactly as
    /// it was left.
    pub fn close_ai_panel(&mut self) -> bool {
        let Some(pos) = self
            .overlays
            .iter()
            .position(|overlay| matches!(overlay.kind, OverlayKind::Ai))
        else {
            return false;
        };
        self.take_overlay_at(pos);
        // the single authoritative closing point, so every caller that
        // closes the panel clears it the same way `mouse_capture` above
        // already is, rather than each having to remember to also clear
        // `AiPanelState::focused` itself
        self.ai_panel.focused = false;
        true
    }

    /// The open interrupt/restart modal's state, wherever it sits in the
    /// stack. Found by kind rather than by position for the same reason
    /// [`Model::picker_mut`] is: the runtime keeps folding the wedge it is
    /// showing, and a modal that a later overlay landed on top of must still
    /// receive that refresh.
    #[must_use]
    pub fn engine_busy(&self) -> Option<&crate::native::supervision::EngineBusyState> {
        self.overlays
            .iter()
            .find_map(|overlay| match &overlay.kind {
                OverlayKind::EngineBusy(state) => Some(state),
                _ => None,
            })
    }

    /// The open interrupt/restart modal's state, mutably; see
    /// [`Model::engine_busy`].
    #[must_use]
    pub fn engine_busy_mut(&mut self) -> Option<&mut crate::native::supervision::EngineBusyState> {
        self.overlays
            .iter_mut()
            .find_map(|overlay| match &mut overlay.kind {
                OverlayKind::EngineBusy(state) => Some(state),
                _ => None,
            })
    }

    /// Closes the interrupt/restart modal, wherever it sits in the stack,
    /// and reports whether one was found to close -- the same by-kind
    /// removal [`Model::close_tree`] performs, and for the same reason.
    pub fn close_engine_busy(&mut self) -> bool {
        let Some(pos) = self
            .overlays
            .iter()
            .position(|overlay| matches!(overlay.kind, OverlayKind::EngineBusy(_)))
        else {
            return false;
        };
        self.take_overlay_at(pos);
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
    ///
    /// Crate-private: a gesture's owner is decided by `update()`'s mouse
    /// arm alone, which is the one place that sees the press and the
    /// matching release. A consumer outside this crate setting an owner
    /// would hand nvim a release it never saw a press for.
    pub(crate) fn capture_mouse(&mut self, target: MouseCapture) {
        self.mouse_capture = Some(target);
    }

    /// Ends the gesture in flight, so the next event routes by position
    /// again. Crate-private for the same reason as
    /// [`Model::capture_mouse`].
    pub(crate) fn release_mouse(&mut self) {
        self.mouse_capture = None;
    }

    /// The cells `overlay` covers on the current terminal.
    ///
    /// The one place an overlay's share of the terminal becomes cells, for
    /// hit-testing and for painting alike: two resolutions reading two
    /// terminal sizes would let a click land on a rect the user is not
    /// looking at.
    ///
    /// Resolved against the rows left over after the persistent chrome
    /// ([`Model::chrome_rows`] above, [`Model::statusline_rows`] below) and
    /// then shifted down past the former, rather than against the whole
    /// terminal: a full-height side panel resolved against `term_height`
    /// covers the tabline and the statusline as well, and both of those
    /// carry live editor state the user still needs while the panel is open
    /// -- the ruler and the search count among it. The share stays a share
    /// of what an overlay may actually have.
    #[must_use]
    pub fn overlay_rect(&self, overlay: &Overlay) -> OverlayRect {
        let top = self.chrome_rows();
        let content = self
            .term_height
            .saturating_sub(top)
            .saturating_sub(self.statusline_rows());
        let rect = overlay.geometry.rect(self.term_width, content);
        OverlayRect {
            row: rect.row.saturating_add(top),
            ..rect
        }
    }

    /// Steps the agent panel one notch wider or narrower, reporting whether
    /// the width actually moved (it does not at either end of the range).
    ///
    /// Both halves in one call on purpose: the session width and the open
    /// overlay's own geometry are the same number stored twice, and a
    /// caller that updated one of them would leave a reopened panel
    /// disagreeing with the one on screen.
    pub(crate) fn resize_ai_panel(&mut self, widen: bool) -> bool {
        let next = geometry::step_panel_width(self.ai_panel_width_pct, widen);
        let moved = next != self.ai_panel_width_pct;
        self.ai_panel_width_pct = next;
        self.rewidth(|kind| matches!(kind, OverlayKind::Ai), next);
        moved
    }

    /// Steps the tree sidebar one notch, on the same terms as
    /// [`Model::resize_ai_panel`].
    pub(crate) fn resize_tree(&mut self, widen: bool) -> bool {
        let next = geometry::step_panel_width(self.tree_width_pct, widen);
        let moved = next != self.tree_width_pct;
        self.tree_width_pct = next;
        self.rewidth(|kind| matches!(kind, OverlayKind::Tree(_)), next);
        moved
    }

    /// Re-widths every open overlay `is_target` names. A no-op when the
    /// panel being resized is closed, which is why the session width above
    /// is written either way.
    fn rewidth(&mut self, is_target: impl Fn(&OverlayKind) -> bool, width_pct: u16) {
        for overlay in &mut self.overlays {
            if is_target(&overlay.kind) {
                overlay.geometry.width_pct = width_pct;
            }
        }
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
    /// The floating windows view has taken over and the rows it took off
    /// them, for a plugin drawing its own completion menu on the command
    /// line view renders (`update::surface_conflict`'s absorption).
    ///
    /// On the engine's own model rather than beside
    /// [`Model::surface_conflicts`], because what it holds is window
    /// handles: those are per-connection allocations, and a replacement
    /// engine's are somebody else's numbers.
    pub(crate) float_absorption: crate::native::surfaces::FloatAbsorption,
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

    /// The rows view took off a plugin's own cmdline completion float, or
    /// `None` while it is absorbing none.
    ///
    /// The palette's second row source, read by `view-surface::render` and
    /// written only by the reply to an `RpcCall::ReadFloatRows`: no paint
    /// asks the engine anything to get them.
    #[must_use]
    pub fn absorbed_rows(&self) -> Option<&crate::native::palette::AbsorbedRows> {
        self.float_absorption.rows()
    }

    /// Drops every piece of this model the engine both raises and retracts,
    /// for a connection being replaced.
    ///
    /// The shape of the defect is one: an engine killed with such a state
    /// raised never sends the event that takes it back, so it stays painted
    /// over the replacement's own first frame -- a command line the user was
    /// typing reads as an editor that lost its prompt, a pending `d2` reads
    /// as a replacement that is still holding it. Every field with that
    /// lifetime belongs here, and every field of this struct is accounted
    /// for one way or the other:
    ///
    /// | field | how it is taken down | forgotten |
    /// |---|---|---|
    /// | `cmdline` | `cmdline_hide` | yes |
    /// | `popupmenu` | `popupmenu_hide` | yes |
    /// | `float_absorption` | the plugin's window dies with the connection | yes |
    /// | `tabline` | the next `tabline_update` | yes |
    /// | `mouse_on` | `mouse_off` | yes |
    /// | `statusline`'s `msg_*` segments | the same event, empty | yes, via [`crate::native::statusline::StatuslineState::forget_engine_segments`] |
    /// | `statusline`'s bridge segments | view's own bridge, re-fired on install | no |
    /// | `grid` | a fresh attach redraws every cell | no |
    /// | `hl` | the replacement's own table replaces it | no |
    /// | `mode` | the replacement announces its modes on attach | no |
    /// | `messages`, `toast_history` | scrollback, not a point-in-time state | no |
    ///
    /// `mouse_on` is in the list because nvim only emits `mouse_on`/
    /// `mouse_off` when its own view of the mouse state changes, and that
    /// view is per-process: the replacement is always a freshly spawned
    /// child ([`crate::model::Model`]'s only caller of this restarts through
    /// `Engine::spawn_recovering`), so it starts from mouse-off and
    /// announces `mouse_on` only if it wants one. A stale `true` left here
    /// would swallow the host terminal's own selection gestures for the rest
    /// of the session with nothing ever to correct it.
    pub fn forget_overlays(&mut self) {
        self.cmdline = None;
        self.popupmenu = None;
        let _ = self.float_absorption.forget();
        self.tabline = None;
        self.mouse_on = false;
        self.statusline.forget_engine_segments();
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
        let route = crate::native::toast::route_under_hold(&kind, self.messages.startup_hold());
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
        // strictly after the scrollback record above, which is what makes
        // "nothing is ever dropped" true on every path through the hold: the
        // message is in the ring before it leaves the stack
        if route == crate::native::toast::Route::HistoryOnly {
            self.messages.hold(id, replace_last);
            return Vec::new();
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

    /// [`Self::record_native_notice`], except that the identical line
    /// already standing is left alone rather than stacked on, and a
    /// standing line from the same `family` with different wording is
    /// withdrawn before the new one lands.
    ///
    /// For a notice raised by something that repeats on its own schedule: a
    /// path that is still unreadable when the next detection window closes
    /// answers the same way it did the last time, and a byte-identical copy
    /// per window buries whatever stood above it and evicts real history
    /// out of the bounded ring behind `:messages`. The standing line keeps
    /// its own expiry rather than being reissued.
    ///
    /// The family withdrawal is for the wording, not the repeat: one fact
    /// can be told two ways (a path's notice names what the buffer holds,
    /// which changes when the user types), and the older wording is not
    /// merely redundant beside the newer one -- it is false the moment the
    /// clause it names stops being true. Keyed on `family`, the opening
    /// both wordings share, on exactly the terms
    /// [`Self::withdraw_native_notice`] uses.
    ///
    /// Deliberately not what [`Self::record_native_notice`] does for
    /// everyone: a repeat is sometimes the whole message -- a replacement
    /// connection failing exactly the way the connection it replaced did is
    /// news, and collapsing it would say the new one never failed.
    pub fn record_native_notice_once(
        &mut self,
        family: &str,
        text: String,
    ) -> Vec<crate::msg::Effect> {
        self.record_native_notice_once_as("native", family, text)
    }

    /// [`Self::record_native_notice_once`] for a notice that has to still be
    /// on screen after the next keystroke: the `"native_sticky"` kind, which
    /// routes [`crate::native::toast::Route::Sticky`] and so survives both
    /// the idle expiry and `Messages::dismiss_transient_on_keypress`.
    ///
    /// For a notice whose subject the user's own typing keeps producing. A
    /// plugin drawing over an owned surface is the case: the keystroke that
    /// summons the float is the keystroke that would dismiss the transient
    /// line about it, and the detection that follows re-raises it ~150 ms
    /// later, so a transient line cycles on and off for as long as the user
    /// types instead of standing to be read. It leaves the way every sticky
    /// entry does -- replaced by its own family, cleared by nvim, or
    /// dismissed deliberately ([`Messages::dismiss_sticky`]).
    pub fn record_native_notice_sticky_once(
        &mut self,
        family: &str,
        text: String,
    ) -> Vec<crate::msg::Effect> {
        self.record_native_notice_once_as("native_sticky", family, text)
    }

    /// The shared body of [`Self::record_native_notice_once`] and
    /// [`Self::record_native_notice_sticky_once`]: same de-duplication, same
    /// family withdrawal, differing only in the `kind` the entry carries and
    /// therefore in the lifetime `toast::route` gives it.
    fn record_native_notice_once_as(
        &mut self,
        kind: &str,
        family: &str,
        text: String,
    ) -> Vec<crate::msg::Effect> {
        let content = vec![(0, text)];
        // every entry, not just the tail: anything at all landing between
        // two detections -- one ordinary nvim message is enough -- takes
        // the last slot, and a tail-only test would then stack the copy it
        // exists to suppress. Scanning is exact rather than approximate,
        // because `entries` holds only what is still standing: an expired
        // transient is retain-removed from it, so a notice that has aged
        // out is gone and the next detection speaks again
        if self
            .messages
            .entries
            .iter()
            .any(|e| e.is_native() && !e.condition && e.content == content)
        {
            return Vec::new();
        }
        self.messages
            .entries
            .retain(|e| !is_standing_native_notice(e, family));
        self.record_message(kind.to_string(), content, false)
    }

    /// Retracts every standing one-shot native notice whose line starts
    /// with `prefix`, and reports whether one was showing.
    ///
    /// The counterpart to [`Self::record_native_notice_once`]: a notice
    /// that asserts something is currently true -- a path that cannot be
    /// read -- is worth keeping up only while it is still true, and the
    /// thing that disproves it arrives long before the notice would have
    /// aged out on its own. By prefix rather than by whole line so the one
    /// call retracts whichever of a family's wordings is up (the same path
    /// says one thing for a modified buffer and another for an unmodified
    /// one).
    ///
    /// A raised condition is left alone: its lifetime belongs to
    /// [`Messages::set_native_condition`], which retracts it by itself when
    /// the condition ends.
    #[must_use]
    pub fn withdraw_native_notice(&mut self, prefix: &str) -> bool {
        let before = self.messages.entries.len();
        self.messages
            .entries
            .retain(|e| !is_standing_native_notice(e, prefix));
        self.messages.entries.len() != before
    }

    /// Whether a one-shot native notice whose line starts with `prefix` is
    /// standing right now, on exactly the terms
    /// [`Self::withdraw_native_notice`] would take one down.
    ///
    /// The standing line is the only durable record of what the user has
    /// already been told, and it retires itself: an expired transient is
    /// retain-removed from `entries`, so this answers `false` again the
    /// moment the notice leaves the screen.
    #[must_use]
    pub fn has_native_notice(&self, prefix: &str) -> bool {
        self.messages
            .entries
            .iter()
            .any(|e| is_standing_native_notice(e, prefix))
    }
}

/// Whether `entry` is a standing one-shot native notice whose first line
/// starts with `prefix`. A raised condition never counts: its lifetime
/// belongs to [`Messages::set_native_condition`] rather than to whatever
/// raised the notice beside it.
fn is_standing_native_notice(entry: &MessageEntry, prefix: &str) -> bool {
    entry.is_native()
        && !entry.condition
        && entry
            .content
            .first()
            .is_some_and(|(_, line)| line.starts_with(prefix))
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
    /// replaced -- never auto-dismissed by incidental user activity and
    /// never evicted from the visible toast stack merely because other
    /// messages arrived after them (`Messages::visible_lines`) -- matching
    /// real nvim's own hit-enter-prompt convention that an error blocks
    /// until acknowledged. The acknowledgement itself is
    /// [`Messages::dismiss_sticky`], which is a deliberate gesture rather
    /// than the ambient activity a transient entry dies of. Every other kind
    /// is transient.
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
    /// locally-raised condition flag, since none exists yet) names a kind
    /// that stands until it is replaced or dismissed. `toast::route` matches
    /// on this directly -- it classifies the `kind` string before any
    /// `MessageEntry` exists to ask.
    ///
    /// nvim's error/warning kinds, plus the one view raises itself:
    /// `"native_sticky"` is not a wire kind and can arrive only through
    /// [`EngineModel::record_native_notice_sticky_once`], whose doc carries
    /// the argument for why that notice cannot be transient.
    #[must_use]
    pub fn is_persistent_kind(kind: &str) -> bool {
        matches!(
            kind,
            "emsg" | "echoerr" | "wmsg" | "lua_error" | "rpc_error" | "shell_err" | "native_sticky"
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

    /// Whether view raised this entry itself rather than decoding it from
    /// nvim's `msg_show`.
    ///
    /// The `"native"` kinds are the marker -- `"native_sticky"` differing
    /// only in lifetime ([`EngineModel::record_native_notice_sticky_once`]),
    /// so every family withdrawal and every de-duplication ranges over both.
    /// Both are reachable from outside `model.rs` only through
    /// [`EngineModel::record_native_notice`] and its siblings and
    /// [`Messages::set_native_condition`], so no wire message can wear one.
    #[must_use]
    pub fn is_native(&self) -> bool {
        Self::is_native_kind(&self.kind)
    }

    /// The kind-only half of [`is_native`](Self::is_native), for the same
    /// reason [`is_persistent_kind`](Self::is_persistent_kind) exists:
    /// `toast::route_under_hold` classifies a `kind` string before any
    /// `MessageEntry` has been built to ask.
    #[must_use]
    pub fn is_native_kind(kind: &str) -> bool {
        matches!(kind, "native" | "native_sticky")
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
    /// Foreign transient messages parked by the startup hold: recorded to
    /// scrollback like any other, but taking no toast slot and painting
    /// nothing until `resolve_startup_hold` decides their fate. See
    /// [`crate::native::toast::StartupHold`].
    held: Vec<MessageEntry>,
    /// Whether a message arriving now is parked. `Pending` from
    /// `Default::default`, which is what makes this mechanism win a race it
    /// would otherwise start after: the first redraw batch after attach can
    /// already carry a plugin's setup-time complaint.
    startup_hold: crate::native::toast::StartupHold,
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
    /// "Most recent" means the most recent entry nvim itself produced.
    /// Anything view synthesized locally is skipped over, whether it is a
    /// raised condition notice (see `set_native_condition`) or a one-shot
    /// one, because nvim's replace targets nvim's own previous line:
    /// overwriting a view notice would both drop something nvim never put
    /// there and leave the line nvim meant to replace standing as a
    /// duplicate. A one-shot notice reaches the tail as readily as a
    /// condition does -- `clear` keeps it across an `msg_clear` that empties
    /// everything around it.
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
            if let Some(last) = self
                .entries
                .iter_mut()
                .rev()
                .find(|e| !e.condition && !e.is_native())
            {
                *last = entry;
                return id;
            }
        }
        self.entries.push(entry);
        id
    }

    /// Whether the startup hold is still parking foreign transient
    /// messages. Read by [`EngineModel::record_message`] on every message,
    /// which is why it is a `Copy` enum read and nothing more.
    #[must_use]
    pub fn startup_hold(&self) -> crate::native::toast::StartupHold {
        self.startup_hold
    }

    /// The messages the startup hold is holding, oldest first. Never
    /// authoritative for anything painted -- `entries` is -- and here for
    /// the assertions that have to see the parking rather than infer it
    /// from an empty stack.
    #[must_use]
    pub fn held(&self) -> &[MessageEntry] {
        &self.held
    }

    /// Moves the entry `id` names out of the visible stack and into the
    /// held set: the second half of a `Route::HistoryOnly` record, run
    /// after [`EngineModel::record_message`] has already written it to
    /// scrollback, so no path through the hold can lose a message.
    ///
    /// Once the hold has collapsed nothing will ever release what it parks,
    /// so a message arriving then is dropped from the stack without being
    /// parked at all -- the scrollback record is the whole of what it gets,
    /// which is what the standing notice tells the user.
    ///
    /// `replace_last` is applied a second time, here, on the same terms
    /// [`Self::push`] applies it to the visible stack. It has to be: parking
    /// takes the entry off that stack, so the next line of a coalescing
    /// sequence finds nothing there to overwrite and appends instead, and a
    /// startup progress line that nvim coalesced into one toast would drain
    /// as one toast per step. The hold decides *when* a message is shown,
    /// never how many of it there are.
    pub(crate) fn hold(&mut self, id: MessageId, replace_last: bool) {
        let Some(index) = self.entries.iter().position(|e| e.id == id) else {
            return;
        };
        let entry = self.entries.remove(index);
        if self.startup_hold != crate::native::toast::StartupHold::Pending {
            return;
        }
        if replace_last {
            if let Some(last) = self
                .held
                .iter_mut()
                .rev()
                .find(|e| !e.condition && !e.is_native())
            {
                *last = entry;
                return;
            }
        }
        self.held.push(entry);
    }

    /// Resolves the startup hold, once, and reports whether anything the
    /// user can see changed.
    ///
    /// [`HoldOutcome::Release`](crate::native::toast::HoldOutcome::Release)
    /// drains the held set onto the stack in arrival order and **re-stamps
    /// every drained entry to the current flush generation**. Without the
    /// re-stamp a drained entry carries the generation it was pushed at,
    /// many flushes ago, and `dismiss_transient_on_keypress` -- which keeps
    /// a transient only while `shown_at_flush == current` -- drops it on the
    /// very keypress that released it, painting zero frames. The stamp is
    /// the same convention `UiEvent::Flush` maintains for a freshly pushed
    /// toast.
    ///
    /// [`HoldOutcome::Collapse`](crate::native::toast::HoldOutcome::Collapse)
    /// leaves the held set where it is -- in the history ring alone -- and
    /// keeps parking, so a late complaint from the same startup joins it
    /// rather than landing on top of the notice that explains it. The
    /// `Release` that later ends a collapsed hold discards the held set
    /// instead of draining it: the decision that it stays in the history was
    /// already taken, and re-raising it on the first keypress would restore
    /// the wall the notice replaced.
    ///
    /// A hold already `Off` answers `false` and changes nothing: the three
    /// triggers race each other by design and only the first is the
    /// decision.
    #[must_use]
    pub fn resolve_startup_hold(&mut self, outcome: crate::native::toast::HoldOutcome) -> bool {
        use crate::native::toast::{HoldOutcome, StartupHold};
        if self.startup_hold == StartupHold::Off {
            return false;
        }
        if outcome == HoldOutcome::Collapse {
            if self.startup_hold == StartupHold::Collapsed {
                return false;
            }
            self.startup_hold = StartupHold::Collapsed;
            return false;
        }
        let collapsed = self.startup_hold == StartupHold::Collapsed;
        self.startup_hold = StartupHold::Off;
        if collapsed {
            // a claimant was named and the notice on screen already says
            // where these went; releasing them onto the stack now would put
            // the wall of startup errors back up, one keypress late
            self.held.clear();
            return false;
        }
        if self.held.is_empty() {
            return false;
        }
        let current = self.flush_generation;
        for mut entry in self.held.drain(..) {
            entry.shown_at_flush = current;
            self.entries.push(entry);
        }
        true
    }

    /// Drops every message nvim showed, per `msg_clear`, and keeps every
    /// locally-synthesized one.
    ///
    /// `msg_clear` states that the messages *nvim* put up are over, which is
    /// a fact about nvim's own message state and says nothing about a line
    /// view raised itself. A native notice's lifetime belongs to the
    /// mechanism that raised it -- `Effect::ScheduleToastExpiry` for a
    /// one-shot notice, the condition itself for a raised condition -- and
    /// letting an unrelated engine redraw retract one is how a notice
    /// vanishes before it was ever read.
    ///
    /// The distinction is load-bearing for the notice a swap recovery shows:
    /// the redraw that takes nvim's recovery report off the buffer is
    /// answered with exactly this event, so a wholesale clear would erase the
    /// account of the recovery together with the report it was written to
    /// replace.
    pub fn clear(&mut self) {
        self.entries.retain(MessageEntry::is_native);
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

    /// Drops the question an answered prompt was asking, so its box leaves
    /// with the prompt instead of lingering over the buffer as ordinary
    /// text. The counterpart to the `cmdline_open` guard in
    /// [`Self::dismiss_transient_on_keypress`], which holds that question
    /// up for exactly as long as the cmdline asking it is open: this is
    /// what ends it at the same moment, rather than at whatever unrelated
    /// keystroke happens next.
    #[must_use]
    pub fn dismiss_answered_prompt(&mut self) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| !e.is_prompt());
        self.entries.len() != before
    }

    /// Drops every standing error/warning entry -- the deliberate way out of
    /// a sticky toast. Returns whether anything was actually dropped, so the
    /// caller knows whether to mark the model dirty for a repaint.
    ///
    /// The counterpart to [`Self::dismiss_transient_on_keypress`], and
    /// deliberately not folded into it: that one fires on *any* keypress,
    /// and an error dismissed by the next motion is an error the user never
    /// read. Stickiness is what makes an error legible; a way out is what
    /// keeps it from occluding the buffer forever once it has been.
    ///
    /// A raised condition (see [`Self::set_native_condition`]) survives: it
    /// asserts that something *is currently true*, so clearing it would
    /// state a falsehood until whoever raised it noticed and re-raised it.
    /// It is retracted by that raiser, when the condition ends.
    #[must_use]
    pub fn dismiss_sticky(&mut self) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| !e.is_persistent() || e.condition);
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
    /// itself, since the engine, not view, owns resolving it, and retires
    /// on the `cmdline_hide` that key comes back as -- the event, not a
    /// later keystroke, so a cancelled question leaves the screen the
    /// moment it is cancelled. A prompt nvim's own Lua answered, with no
    /// key of view's forwarded to it, has no such event to be told apart
    /// from a re-arm and still closes lazily on the next key. Mouse input
    /// is the exception, routing by position through [`Model::overlay_at`]
    /// rather than by focus.
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
    /// The interrupt/restart modal a wedged or lost engine escalates into,
    /// carrying which wedge opened it and how long that wedge has lasted.
    /// Pushed on top of whatever is already open, including a blocked-engine
    /// `Prompt`: an engine that has stopped answering cannot resolve that
    /// prompt either, so the recovery offer outranks it. See
    /// [`crate::native::supervision::EngineBusyState`].
    EngineBusy(crate::native::supervision::EngineBusyState),
    /// The agent panel, anchored flush right and full height -- the mirror
    /// of [`OverlayKind::Tree`]'s left-anchored sidebar, sitting beside an
    /// active buffer rather than centered over it. Pushed like a picker or
    /// tree, with the identical beneath-a-blocked-prompt fallback. Unlike
    /// every other overlay here it does not take focus merely by being open
    /// (see [`Model::takes_focus`]): a running agent session and the engine
    /// the user is editing in are two peers the user works with at once,
    /// not a question blocking one or the other, so an engine keystroke
    /// while the panel is only visible still reaches the engine.
    ///
    /// It does take focus, the same as any other overlay here, once the
    /// user has deliberately entered it (`open`/`focus`/`toggle`, see
    /// [`crate::native::ai_panel::AiPanelState::focused`]) -- consulted by
    /// [`Model::takes_focus_now`], not by this static form. A permission
    /// request blocking the issuing agent's own turn answers through that
    /// real focus (`route_key`'s `Focus::Native(OverlayKind::Ai)` arm),
    /// never through a side channel ahead of the ordinary routing that
    /// every other focus-taking overlay already goes through.
    ///
    /// A unit marker, not a payload: the session state it renders lives in
    /// [`Model::ai_panel`] instead, so that closing this overlay (and
    /// dropping this variant) never drops the transcript, pending
    /// permission, or pending edits underneath it. See
    /// [`crate::native::ai_panel::AiPanelState`].
    Ai,
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
/// (BSU/ESU gates on `caps.sync`, the border charset on
/// `caps.unicode_boxes`, never on tier alone).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermCaps {
    pub tier: Tier,
    pub sync: bool,
    pub truecolor: bool,
    pub kitty_kbd: bool,
    /// Whether the terminal accounts for a box-drawing glyph as one cell,
    /// which is what the border charset asks about.
    ///
    /// A cell-accounting fact, not a legibility one: the probe behind it
    /// writes one `╭` and reads the cursor column back, so a terminal not
    /// decoding UTF-8 is what a `false` here has actually been shown to
    /// mean. A font that lacks the glyph still advances one column and
    /// renders tofu, and no capture has separated that case from a working
    /// one -- see `docs/terminal-probe-wire-capture.md`, "What D and E
    /// prove, and what they do not".
    pub unicode_boxes: bool,
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
            unicode_boxes: false,
        }
    }

    /// The same capabilities with [`Self::unicode_boxes`] set to what the
    /// box-glyph probe answered.
    ///
    /// Set beside [`Self::from_probe`] rather than through it because
    /// `tier` does not derive from it: the border charset is the one thing
    /// this bit gates, and a terminal's cell accounting is independent of
    /// the color depth, synchronization and keyboard-protocol answers a
    /// tier is made of. Left `false` by every caller that never asked the
    /// question, which is the same floor an unanswered probe resolves to.
    #[must_use]
    pub fn with_unicode_boxes(self, unicode_boxes: bool) -> Self {
        Self {
            unicode_boxes,
            ..self
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

    fn showed(model: &mut Model, kind: &str, text: &str) {
        let _ = model
            .engine
            .record_message(kind.to_string(), vec![(0, text.to_string())], false);
    }

    fn stack(model: &Model) -> Vec<String> {
        model
            .engine
            .messages
            .entries
            .iter()
            .map(|entry| entry.lines().join(""))
            .collect()
    }

    fn history(model: &Model) -> Vec<String> {
        model
            .engine
            .toast_history
            .entries()
            .map(|entry| entry.lines().join(""))
            .collect()
    }

    /// The whole point of the window: a plugin's setup-time complaints are
    /// recorded exactly as they would have been and take no toast slot, so
    /// the notice that explains them is what the user reads.
    #[test]
    fn a_foreign_startup_message_is_parked_but_still_recorded() {
        let mut model = Model::new();
        showed(
            &mut model,
            "echomsg",
            "noice.nvim: Noice needs ext_messages",
        );
        assert!(stack(&model).is_empty(), "{:?}", stack(&model));
        assert_eq!(model.engine.messages.held().len(), 1);
        assert_eq!(
            history(&model),
            vec!["noice.nvim: Noice needs ext_messages".to_string()],
            "parked is not dropped: the history overlay is where the notice sends the user"
        );
    }

    /// A message that overwrites the one before it keeps doing so while it
    /// is parked, so the hold changes when a startup progress line is shown
    /// and never how many of it there are.
    ///
    /// `replace_last` is nvim's own coalescing -- a plugin redrawing
    /// "Installing 3/9" over "Installing 2/9" sends it -- and the visible
    /// stack applies it by overwriting the tail. Parking removes the entry
    /// from that stack, so without the same rule inside the held set the
    /// next line finds nothing to overwrite and the release drains one toast
    /// per step of a progress bar that was only ever one line.
    #[test]
    fn a_coalescing_sequence_held_then_released_drains_as_one_entry() {
        let mut model = Model::new();
        for step in 1..=9 {
            let _ = model.engine.record_message(
                "echomsg".to_string(),
                vec![(0, format!("Installing {step}/9"))],
                step > 1,
            );
        }
        assert_eq!(
            model.engine.messages.held().len(),
            1,
            "{:?}",
            model.engine.messages.held()
        );
        assert!(model
            .engine
            .messages
            .resolve_startup_hold(crate::native::toast::HoldOutcome::Release));
        assert_eq!(stack(&model), vec!["Installing 9/9".to_string()]);
        // the history is the one place every step survives, exactly as it
        // does for a coalescing sequence the hold never touched
        assert_eq!(history(&model).len(), 9);
    }

    /// The exclusions that keep the window honest. A line view raises about
    /// itself is never a claimant's, and an error is not something a notice
    /// about surfaces can stand in for.
    ///
    /// Ranged over both states that park, because they are different
    /// windows and only one of them ever gives what it parked back: a
    /// collapsed hold keeps parking for the rest of the launch and discards
    /// the set at release, so an error that got past the pending window and
    /// into the collapsed one would be a message no user ever sees on the
    /// stack.
    #[test]
    fn view_s_own_lines_and_every_error_paint_through_the_hold() {
        use crate::native::toast::{HoldOutcome, StartupHold};
        for hold in [StartupHold::Pending, StartupHold::Collapsed] {
            for (kind, text) in [
                ("native", "view: view.toml line 3: unknown key"),
                (
                    "native_sticky",
                    "view: a plugin is drawing over the command line",
                ),
                ("emsg", "E492: Not an editor command"),
                ("echoerr", "noice.nvim: Noice can't work"),
            ] {
                let mut model = Model::new();
                if hold == StartupHold::Collapsed {
                    assert!(!model
                        .engine
                        .messages
                        .resolve_startup_hold(HoldOutcome::Collapse));
                }
                assert_eq!(model.engine.messages.startup_hold(), hold);
                showed(&mut model, kind, text);
                assert_eq!(
                    stack(&model),
                    vec![text.to_string()],
                    "kind {kind} was parked under {hold:?}"
                );
                assert!(
                    model.engine.messages.held().is_empty(),
                    "kind {kind} was parked under {hold:?}"
                );
            }
        }
    }

    /// Releasing drains in arrival order and re-stamps, because the very
    /// next thing the keypress that released does is dismiss transients
    /// that have not had a frame -- which, unstamped, is all of them.
    #[test]
    fn releasing_drains_in_order_and_restamps_so_the_releasing_key_cannot_wipe_it() {
        let mut model = Model::new();
        showed(&mut model, "echomsg", "one");
        showed(&mut model, "echomsg", "two");
        for _ in 0..4 {
            model.engine.messages.note_flush();
        }
        assert!(model
            .engine
            .messages
            .resolve_startup_hold(crate::native::toast::HoldOutcome::Release));
        assert_eq!(stack(&model), vec!["one".to_string(), "two".to_string()]);
        let _ = model.engine.messages.dismiss_transient_on_keypress(false);
        assert_eq!(
            stack(&model),
            vec!["one".to_string(), "two".to_string()],
            "the key that released them took them straight back off"
        );
    }

    /// A collapsed hold has a notice standing that says where the parked
    /// messages went, so the key that ends the window discards them rather
    /// than putting the wall back up one keystroke late.
    #[test]
    fn a_collapsed_hold_keeps_parking_and_never_drains_what_it_parked() {
        use crate::native::toast::HoldOutcome;
        let mut model = Model::new();
        showed(&mut model, "echomsg", "one");
        assert!(!model
            .engine
            .messages
            .resolve_startup_hold(HoldOutcome::Collapse));
        showed(&mut model, "echomsg", "late");
        assert!(stack(&model).is_empty(), "{:?}", stack(&model));
        assert!(!model
            .engine
            .messages
            .resolve_startup_hold(HoldOutcome::Release));
        assert!(stack(&model).is_empty(), "{:?}", stack(&model));
        assert_eq!(history(&model).len(), 2, "both are still readable");
    }

    /// Three triggers race by design; only the first is the decision.
    #[test]
    fn a_resolved_hold_never_resolves_again() {
        use crate::native::toast::HoldOutcome;
        let mut model = Model::new();
        assert!(!model
            .engine
            .messages
            .resolve_startup_hold(HoldOutcome::Release));
        showed(&mut model, "echomsg", "after");
        assert_eq!(stack(&model), vec!["after".to_string()]);
        assert!(!model
            .engine
            .messages
            .resolve_startup_hold(HoldOutcome::Collapse));
        showed(&mut model, "echomsg", "still after");
        assert_eq!(stack(&model).len(), 2);
    }

    /// A model nothing has told about an attach answers for the session
    /// that attaches everything, which is what a `Model` built by a test,
    /// a bench or an oracle run is standing in for.
    #[test]
    fn an_unattached_model_owns_every_surface() {
        let model = Model::new();
        for &surface in crate::native::ext::ALL {
            assert!(model.owns(surface), "{surface:?} must default to owned");
        }
        assert_eq!(
            model.attached_surfaces().len(),
            crate::native::ext::ALL.len(),
            "and nothing else"
        );
    }

    #[test]
    fn owns_answers_from_the_set_the_attach_recorded() {
        let mut model = Model::new();
        model.attach_surfaces(vec![crate::native::ext::Ext::LineGrid]);
        assert!(model.owns(crate::native::ext::Ext::LineGrid));
        assert!(
            !model.owns(crate::native::ext::Ext::Messages),
            "a surface left out of the attach is nvim's to draw"
        );
        assert_eq!(
            model.attached_surfaces(),
            [crate::native::ext::Ext::LineGrid]
        );
    }

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
            // the one kind view raises itself: a line the keystroke that
            // re-detects its subject would otherwise wipe
            "native_sticky",
        ] {
            assert!(
                entry(kind, vec![]).is_persistent(),
                "{kind} must be persistent"
            );
            assert_eq!(
                crate::native::toast::route(kind),
                crate::native::toast::Route::Sticky,
                "{kind} must route sticky, or it expires on a timer anyway"
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

    /// A full-height side panel takes its share of the rows an overlay may
    /// actually have, never of the whole terminal. Resolved against
    /// `term_height` it covered the tabline and the statusline as well, and
    /// the statusline's right zone is where nvim's ruler and search count
    /// live: with the panel open, `/foo` reported its `1/12` into cells
    /// nothing could see for as long as the panel stayed open.
    #[test]
    fn a_full_height_overlay_stops_short_of_the_persistent_chrome() {
        use crate::native::geometry::{Anchor, OverlayBox};

        let mut m = Model::with_term_size(80, 24);
        m.statusline_enabled = true;
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
        m.push_overlay(
            OverlayBox::new(30, 100).with_anchor(Anchor::Right),
            OverlayKind::Ai,
        );

        let overlay = m.overlays().last().expect("the panel was just pushed");
        let rect = m.overlay_rect(overlay);
        assert_eq!(rect.row, 1, "the tabline owns row 0");
        assert_eq!(
            rect.height, 22,
            "24 rows less the tabline's and the statusline's"
        );
        assert!(
            !rect.contains(0, 79),
            "a click on the tabline is not a click on the panel"
        );
        assert!(
            !rect.contains(23, 79),
            "the statusline row stays the statusline's, so the ruler and the search count stay readable"
        );
    }

    /// The names declared in `source`'s struct block opening with `header`.
    ///
    /// A doc line always carries a `/` before its first colon, and an
    /// attribute carries none at all, so "the text before the first colon
    /// is a bare snake_case word" is the whole discriminator a field
    /// declaration needs here.
    fn declared_fields(source: &str, header: &str) -> Vec<String> {
        assert!(source.contains(header), "{header} is no longer declared");
        let after = source.split_once(header).expect("just found above").1;
        assert!(after.contains("\n}"), "{header} is never closed");
        let body = after.split_once("\n}").expect("just found above").0;
        let mut names = Vec::new();
        for line in body.lines() {
            let declaration = line.trim().strip_prefix("pub ").unwrap_or(line.trim());
            let Some((name, _)) = declaration.split_once(':') else {
                continue;
            };
            if !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            {
                names.push(name.to_string());
            }
        }
        assert!(!names.is_empty(), "{header} parsed to no fields at all");
        names
    }

    /// The doc comment sitting directly above `signature` in `source`.
    fn doc_above(source: &str, signature: &str) -> String {
        assert!(
            source.contains(signature),
            "{signature} is no longer declared"
        );
        let head = source
            .split_once(signature)
            .expect("just found above")
            .0
            .trim_end();
        let mut doc: Vec<&str> = head
            .lines()
            .rev()
            .take_while(|line| line.trim_start().starts_with("///"))
            .collect();
        doc.reverse();
        assert!(!doc.is_empty(), "{signature} carries no doc comment");
        doc.join("\n")
    }

    /// Whether `doc` names `field`, in either spelling the two tables use.
    fn classified(doc: &str, field: &str) -> bool {
        doc.contains(&format!("`{field}`")) || doc.contains(&format!("Self::{field}`"))
    }

    #[test]
    fn every_field_the_engine_raises_is_classified_where_it_is_dropped() {
        let model = include_str!("model.rs");
        let statusline = include_str!("native/statusline.rs");
        let mut unclassified = Vec::new();

        let overlays = doc_above(model, "pub fn forget_overlays(&mut self)");
        for field in declared_fields(model, "pub struct EngineModel {") {
            if !classified(&overlays, &field) {
                unclassified.push(format!("EngineModel::{field} (forget_overlays)"));
            }
        }

        let segments = doc_above(statusline, "pub fn forget_engine_segments(&mut self)");
        for field in declared_fields(statusline, "pub struct StatuslineState {") {
            if !classified(&segments, &field) {
                unclassified.push(format!("StatuslineState::{field} (forget_engine_segments)"));
            }
        }

        assert!(
            unclassified.is_empty(),
            "a connection being replaced drops some of this state and keeps \
             the rest, and these fields say which they are nowhere:\n  \
             {}\nAdd a row to the table above the named method saying how \
             the field is taken down and whether the replacement inherits \
             it -- a field nobody classified is the one that stays painted \
             over the replacement's first frame",
            unclassified.join("\n  ")
        );
    }
}
