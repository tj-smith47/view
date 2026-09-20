//! What one window's own status segments read.
//!
//! The session-wide [`StatuslineState`](crate::native::statusline::StatuslineState)
//! answers for the window the cursor is in and for nothing else, and under
//! tiles every frame draws segments of its own. So the bridge reports one
//! of these per window that changed, and the model keeps the last one it
//! heard for each.

/// One window's buffer identity, cursor position and diagnostic counts, as
/// the bridge's `window` trigger group reports them.
///
/// Keyed by [`WinHandle`](crate::events::WinHandle) rather than by grid,
/// because the trigger runs in Lua where a window handle is the only
/// identity nvim offers. The painter maps a grid to its handle through
/// [`GridRegistry::window_handle`](crate::grid::registry::GridRegistry::window_handle).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WindowStatus {
    /// The buffer the window is showing.
    pub buf: u64,
    /// That buffer's tail name, empty for one that has never been named.
    pub name: String,
    /// Whether the buffer has unsaved changes.
    pub modified: bool,
    /// The cursor's line, 1-based as nvim reports it.
    pub row: u32,
    /// The cursor's column, 1-based as nvim reports it.
    pub col: u32,
    /// Error-severity diagnostics in the buffer.
    pub errors: u32,
    /// Warning-severity diagnostics in the buffer.
    pub warnings: u32,
}
