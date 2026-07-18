//! Application state `update()` reads and mutates. No I/O, no rendering.

use crate::events::{ModeInfo, PmItem, TabEntry, TabHandle};
use crate::grid::Grid;
use crate::hl::HlTable;

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
                hl: HlTable {
                    default_fg: None,
                    default_bg: None,
                    attrs: std::collections::HashMap::new(),
                },
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
    pub grid: Grid,
    pub hl: HlTable,
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

/// nvim mode state: the cursor/highlight property table from the last
/// `mode_info_set`, plus the active mode from the last `mode_change`.
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct ModeState {
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
}

/// The message log built from `msg_show`/`msg_clear`. A log rather than a
/// single `Option`, since nvim can show several messages in sequence
/// (`:messages` history) before any are cleared.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Messages {
    pub entries: Vec<MessageEntry>,
}

impl Messages {
    /// Records one `msg_show`. `replace_last` overwrites the most recent
    /// entry instead of appending, matching nvim's progress-indicator
    /// convention (e.g. successive search-match counts share one line);
    /// with no prior entry to replace, it appends instead.
    pub fn push(&mut self, entry: MessageEntry, replace_last: bool) {
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
