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
}

impl Model {
    /// A freshly started application: an empty grid, an empty highlight
    /// table, engine focus, conservative terminal capabilities, and no
    /// pending paint.
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
            },
            focus: Focus::Engine,
            caps: TermCaps::default(),
            dirty: false,
            running: true,
        }
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
    /// The embedded nvim engine's grid.
    Engine,
    // Native(id) arrives with the first native overlay.
}

/// Detected terminal capabilities.
///
/// `tier` is coarse UX vocabulary; the probed bits are what gates behavior
/// (BSU/ESU gates on `caps.sync`, never on tier alone).
#[non_exhaustive]
pub struct TermCaps {
    pub tier: Tier,
    pub sync: bool,
    pub truecolor: bool,
    pub kitty_kbd: bool,
}

impl Default for TermCaps {
    /// Conservative until detection (a later task) fills this in: no probe
    /// is assumed to have succeeded.
    fn default() -> Self {
        Self {
            tier: Tier::Standard,
            sync: false,
            truecolor: false,
            kitty_kbd: false,
        }
    }
}

/// Coarse terminal capability tier.
#[non_exhaustive]
pub enum Tier {
    Full,
    Standard,
    Basic,
}
