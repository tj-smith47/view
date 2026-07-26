//! Application state `update()` reads and mutates. No I/O, no rendering.

use crate::events::{ModeInfo, PmItem, TabEntry, TabHandle};
use crate::grid::{Grid, GridOp};
use crate::hl::{HlAttr, HlTable, ProbedDefaults};

/// The complete application state.
#[non_exhaustive]
pub struct Model {
    pub engine: EngineModel,
    pub focus: Focus,
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
}

impl Model {
    /// A freshly started application: an empty grid, an empty highlight
    /// table, engine focus, conservative terminal capabilities, zero
    /// terminal size, and no pending paint.
    #[must_use]
    pub fn new() -> Self {
        Self {
            engine: EngineModel {
                grid: Grid::new(),
                hl: HlTable::new(),
                mode: ModeState::default(),
                cmdline: None,
                messages: Messages::default(),
                tabline: None,
                popupmenu: None,
                mouse_on: false,
            },
            focus: Focus::Engine,
            caps: TermCaps::default(),
            dirty: false,
            running: true,
            term_width: 0,
            term_height: 0,
            content_painted: true,
            fatal_reason: None,
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
            self.term_height.saturating_sub(self.chrome_rows()),
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
    pub tabline: Option<TablineState>,
    pub popupmenu: Option<PopupmenuState>,
    /// Whether nvim currently wants terminal mouse reporting on, from the
    /// last `mouse_on`/`mouse_off` redraw event. The terminal only enables
    /// mouse capture while this is `true`: capturing unconditionally would
    /// swallow the host terminal's own selection/scrollback gestures even
    /// when nvim's `'mouse'` option is off.
    pub mouse_on: bool,
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
}

impl MessageEntry {
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
    /// `"wmsg"`, `"lua_error"`, `"rpc_error"`. These must be read, not
    /// silently lost, so they persist until explicitly cleared or replaced
    /// -- never auto-dismissed by user activity and never evicted from the
    /// visible toast stack merely because other messages arrived after
    /// them (`Messages::visible_lines`) -- matching real nvim's own
    /// hit-enter-prompt convention that an error blocks until acknowledged.
    /// Every other kind is transient.
    #[must_use]
    pub fn is_persistent(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "emsg" | "echoerr" | "wmsg" | "lua_error" | "rpc_error"
        )
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
}

impl Messages {
    /// Records one `msg_show`: `kind`/`content` as decoded off the wire (or
    /// synthesized locally, see `push_native`), stamped with the current
    /// flush generation. `replace_last` overwrites the most recent entry
    /// instead of appending, matching nvim's progress-indicator convention
    /// (e.g. successive search-match counts share one line); with no prior
    /// entry to replace, it appends instead.
    pub fn push(&mut self, kind: String, content: Vec<(u64, String)>, replace_last: bool) {
        let entry = MessageEntry {
            kind,
            content,
            shown_at_flush: self.flush_generation,
        };
        if replace_last {
            if let Some(last) = self.entries.last_mut() {
                *last = entry;
                return;
            }
        }
        self.entries.push(entry);
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
    pub fn push_native(&mut self, text: String, replace_last: bool) {
        self.push("native".to_string(), vec![(0, text)], replace_last);
    }

    /// Marks one full paint cycle as having happened -- one call per
    /// `Flush` UI event -- so that a transient entry's age in frames, and
    /// therefore whether it has survived long enough to be dismissable, is
    /// answerable at all.
    pub fn note_flush(&mut self) {
        self.flush_generation = self.flush_generation.wrapping_add(1);
    }

    /// Drops every transient (non-`is_persistent`) entry that has already
    /// survived at least one full paint cycle since it was shown. Called
    /// from `update` on the user's next keypress: gives an info-level toast
    /// a readable duration bounded by real user activity -- an event the
    /// zero-clock runtime already receives -- rather than a wall-clock
    /// timer the runtime has no mechanism for. An entry pushed in the same
    /// flush generation as the pending keypress has not necessarily been
    /// painted even once yet, so it survives this pass and is only
    /// dismissed on the *next* keypress instead, guaranteeing every
    /// transient toast is visible for at least one frame. Returns whether
    /// anything was actually dropped, so the caller knows whether to mark
    /// the model dirty for a repaint.
    #[must_use]
    pub fn dismiss_transient_on_keypress(&mut self) -> bool {
        let before = self.entries.len();
        let current = self.flush_generation;
        self.entries
            .retain(|e| e.is_persistent() || e.shown_at_flush == current);
        self.entries.len() != before
    }

    /// The physical lines actually visible in a toast box `max_rows` tall:
    /// every persistent (error/warn-kind) entry's lines are always kept, in
    /// their original arrival order; the remaining row budget is filled
    /// with the most recent transient lines, evicting the oldest transient
    /// lines first when the log needs more rows than the box has. Only in
    /// the extreme case where persistent lines alone exceed `max_rows` does
    /// eviction reach into them too (oldest persistent first) -- the sole
    /// remaining way an error/warn line can still be dropped, and never
    /// merely because other messages arrived after it. Without this
    /// priority, a burst of ordinary info messages could silently push an
    /// unread error off the visible stack with neither an explicit
    /// `msg_clear` nor a replace ever happening, which is exactly the
    /// "persist until dismissed or replaced" contract broken by a plain
    /// recency-only trim.
    #[must_use]
    pub fn visible_lines(&self, max_rows: usize) -> Vec<String> {
        let all: Vec<(bool, String)> = self
            .entries
            .iter()
            .flat_map(|e| {
                let persistent = e.is_persistent();
                e.lines().into_iter().map(move |l| (persistent, l))
            })
            .collect();
        let overflow = all.len().saturating_sub(max_rows);
        if overflow == 0 {
            return all.into_iter().map(|(_, l)| l).collect();
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
            .filter_map(|((_, l), k)| k.then_some(l))
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
    pub grid: u64,
}

/// Which surface currently owns input focus.
#[non_exhaustive]
pub enum Focus {
    /// The embedded nvim engine's grid: keys, paste, and mouse route to
    /// `RpcCall`s.
    Engine,
    /// A native overlay identified by `OverlayId` owns input: keys, paste,
    /// and mouse are consumed by that overlay's own `update()` arm instead
    /// of reaching the engine, except `<Esc>` which always returns focus to
    /// `Engine`. No native overlay currently claims this focus; the
    /// variant exists so the routing seam is pinned by tests independent
    /// of any concrete overlay consumer.
    Native(OverlayId),
}

/// Opaque identifier for a native overlay that can hold input focus.
/// Nothing constructs this yet; the newtype exists so `Focus::Native`
/// is representable and the focus vocabulary is stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayId(pub u64);

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
#[derive(Debug, Clone, Copy)]
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
    fn is_persistent_matches_every_error_and_warning_kind_and_only_those() {
        for kind in ["emsg", "echoerr", "wmsg", "lua_error", "rpc_error"] {
            assert!(
                entry(kind, vec![]).is_persistent(),
                "{kind} must be persistent"
            );
        }
        for kind in ["echo", "echomsg", "native", "progress", "quickfix", ""] {
            assert!(
                !entry(kind, vec![]).is_persistent(),
                "{kind} must not be persistent"
            );
        }
    }

    #[test]
    fn dismiss_transient_on_keypress_drops_transient_entries_seen_at_least_one_flush() {
        let mut messages = Messages::default();
        messages.push("echomsg".to_string(), vec![(0, "info".into())], false);
        // not yet flushed: must survive this pass, guaranteeing at least
        // one painted frame before an info toast can be dismissed
        assert!(!messages.dismiss_transient_on_keypress());
        assert_eq!(messages.entries.len(), 1);

        messages.note_flush();
        assert!(messages.dismiss_transient_on_keypress());
        assert!(messages.entries.is_empty());
    }

    #[test]
    fn dismiss_transient_on_keypress_never_drops_a_persistent_entry() {
        let mut messages = Messages::default();
        messages.push("echoerr".to_string(), vec![(0, "boom".into())], false);
        messages.note_flush();
        messages.note_flush();
        assert!(!messages.dismiss_transient_on_keypress());
        assert_eq!(messages.entries.len(), 1);
    }

    #[test]
    fn visible_lines_returns_everything_when_it_fits() {
        let mut messages = Messages::default();
        messages.push("echomsg".to_string(), vec![(0, "a".into())], false);
        messages.push("echomsg".to_string(), vec![(0, "b".into())], false);
        assert_eq!(messages.visible_lines(5), vec!["a", "b"]);
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
        assert_eq!(messages.visible_lines(2), vec!["error", "new info"]);
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
        assert_eq!(messages.visible_lines(1), vec!["second error"]);
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
