//! Typed nvim `ext_linegrid` `redraw` sub-events, decoded by `view-engine`
//! and consumed by [`crate::update::update`]. Pure data: no RPC, no wire
//! decoding here.

/// One decoded `redraw` sub-event.
///
/// nvim batches many of these per `redraw` notification; unrecognized event
/// names decode to [`UiEvent::Unknown`] rather than being dropped, since new
/// event kinds arrive across nvim versions and callers may still want to see
/// the name.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum UiEvent {
    /// A grid was resized to `width` x `height` cells.
    GridResize { grid: u64, width: u64, height: u64 },
    /// A run of cells was written to `grid` starting at `(row, col_start)`.
    GridLine {
        grid: u64,
        row: u64,
        col_start: u64,
        cells: Vec<GridCell>,
    },
    /// The cursor moved to `(row, col)` on `grid`.
    GridCursorGoto { grid: u64, row: u64, col: u64 },
    /// A region of `grid` scrolled by `rows` (positive = down, negative = up).
    GridScroll {
        grid: u64,
        top: u64,
        bot: u64,
        left: u64,
        right: u64,
        rows: i64,
    },
    /// `grid` was cleared to the default background.
    GridClear { grid: u64 },
    /// A highlight attribute id was (re)defined.
    HlAttrDefine {
        id: u64,
        fg: Option<u32>,
        bg: Option<u32>,
        bold: bool,
        italic: bool,
        underline: bool,
        reverse: bool,
    },
    /// The default foreground/background/special colors changed. `None`
    /// means the color is unset (nvim's `-1` sentinel on the wire).
    DefaultColorsSet {
        fg: Option<u32>,
        bg: Option<u32>,
        sp: Option<u32>,
    },
    /// nvim finished a batch of updates; safe to repaint.
    Flush,
    /// The set of editor modes and their cursor/highlight properties
    /// changed (fires once at attach and again whenever `guicursor` is set).
    ModeInfoSet {
        cursor_style_enabled: bool,
        modes: Vec<ModeInfo>,
    },
    /// The active editor mode changed. `mode_idx` indexes into the most
    /// recent [`UiEvent::ModeInfoSet`] `modes` list.
    ModeChange { mode: String, mode_idx: u64 },
    /// The command line is showing `content` at cursor position `pos`.
    CmdlineShow {
        content: Vec<(u64, String)>,
        pos: u64,
        firstc: String,
        prompt: String,
        indent: u64,
        level: u64,
    },
    /// The command line cursor moved to `pos` without its content changing.
    CmdlinePos { pos: u64, level: u64 },
    /// The command line closed.
    CmdlineHide,
    /// A message was shown. `replace_last` means it supersedes the most
    /// recently shown message instead of appending a new one (nvim's
    /// progress-indicator convention, e.g. successive search-match counts).
    MsgShow {
        kind: String,
        content: Vec<(u64, String)>,
        replace_last: bool,
    },
    /// Every shown message was cleared.
    MsgClear,
    /// The open tabs changed. `current` is the active tab.
    TablineUpdate {
        current: TabHandle,
        tabs: Vec<TabEntry>,
    },
    /// The completion popup menu is showing `items` at `(row, col)` on
    /// `grid`, with `selected` the zero-based highlighted index (`-1` for
    /// none).
    PopupmenuShow {
        items: Vec<PmItem>,
        selected: i64,
        row: u64,
        col: u64,
        grid: u64,
    },
    /// The popup menu's highlighted item changed without reshowing it.
    PopupmenuSelect { selected: i64 },
    /// The popup menu closed.
    PopupmenuHide,
    /// An event name this decoder does not yet model.
    Unknown { name: String },
}

/// One mode's cursor and highlight properties, from a `mode_info_set`
/// dict. Fields absent on a given mode (e.g. the mouse-only hover modes
/// carry no cursor fields at all) decode to their zero value rather than
/// failing the whole event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModeInfo {
    pub name: String,
    pub short_name: String,
    pub cursor_shape: String,
    pub cell_percentage: u64,
    pub blinkwait: u64,
    pub blinkon: u64,
    pub blinkoff: u64,
    pub attr_id: u64,
}

/// A `Tabpage` handle, unwrapped from nvim's msgpack-RPC `Ext` encoding
/// into a plain integer so `view-core` never has to model `Ext` itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TabHandle(pub u64);

/// One entry in [`UiEvent::TablineUpdate`]'s `tabs` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabEntry {
    pub tab: TabHandle,
    pub name: String,
}

/// One completion candidate in [`UiEvent::PopupmenuShow`]'s `items` list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PmItem {
    pub word: String,
    pub kind: String,
    pub menu: String,
    pub info: String,
}

/// One cell in a [`UiEvent::GridLine`] run.
///
/// `hl_id` carries over from the previous cell in the same line when the
/// wire tuple omits it, so callers never re-implement that carry-over rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridCell {
    pub text: String,
    pub hl_id: u64,
    pub repeat: u64,
}

/// Largest grid dimension accepted from the wire. Far beyond any physical
/// terminal, small enough that a malformed or desynced `grid_resize`
/// cannot make the grid allocate unboundedly.
const MAX_GRID_DIM: u16 = 2048;

/// Saturates a wire dimension into `u16` and clamps it to [`MAX_GRID_DIM`].
#[must_use]
pub fn clamp_dim(dim: u64) -> u16 {
    saturate_u16(dim).min(MAX_GRID_DIM)
}

// a plain `as u16` cast would wrap out-of-range wire values back into
// range (65536 becomes 0), turning a malformed coordinate into a write at
// a real cell; saturating keeps it out of range so Grid ignores it
/// Saturates a wire `u64` coordinate into `u16`, clamping to `u16::MAX`
/// instead of truncating.
#[must_use]
pub fn saturate_u16(v: u64) -> u16 {
    u16::try_from(v).unwrap_or(u16::MAX)
}
