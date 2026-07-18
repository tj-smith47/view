//! Application state `update()` reads and mutates. No I/O, no rendering.

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

/// The embedded engine's half of [`Model`]: its grid, highlight table, and
/// mode state.
#[non_exhaustive]
pub struct EngineModel {
    pub grid: Grid,
    pub hl: HlTable,
    pub mode: ModeState,
}

/// nvim mode state (normal/insert/visual/cmdline/...). Empty for now; a
/// later task fills in fields as mode-dependent behavior is implemented.
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct ModeState {}

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
