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
    /// An event name this decoder does not yet model.
    Unknown { name: String },
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
