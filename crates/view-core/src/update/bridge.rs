//! The `view_bridge` channel's arms.
//!
//! nvim raises no redraw event for a buffer's name, its filetype or its
//! diagnostics, so view installs autocmds of its own and they arrive here.
//! Most of them change text on screen and nothing else, so they return no
//! RPC beyond the tree's git refresh; the tab-row option is the one that
//! can move a row the engine's own grid is laid out against.

use crate::events::{saturate_u32, WinHandle};
use crate::model::{BufferEntry, Model, WindowStatus};
use crate::msg::Effect;
use crate::native::statusline::SegmentUpdate;

use super::surfaces::tree_git_refresh_effect;

/// `vim.diagnostic.count(0)` for the current buffer.
pub(super) fn on_diagnostics(model: &mut Model, errors: u32, warnings: u32) -> Vec<Effect> {
    model
        .engine
        .statusline
        .apply(SegmentUpdate::Diagnostics { errors, warnings });
    model.dirty = true;
    Vec::new()
}

/// The repository's current branch, from the bridge's `vim.system()` lookup.
pub(super) fn on_git_branch(model: &mut Model, branch: String) -> Vec<Effect> {
    model
        .engine
        .statusline
        .apply(SegmentUpdate::GitBranch(branch));
    model.dirty = true;
    tree_git_refresh_effect(model)
}

/// The current buffer's name, unsaved flag and filetype.
pub(super) fn on_buffer(
    model: &mut Model,
    name: String,
    modified: bool,
    filetype: String,
) -> Vec<Effect> {
    model.engine.statusline.apply(SegmentUpdate::Buffer {
        name,
        modified,
        filetype,
    });
    model.dirty = true;
    tree_git_refresh_effect(model)
}

/// Every listed buffer, which is what the pill names while one tabpage is
/// open.
///
/// Compared before it is stored, for the reason
/// [`on_window_status`] compares: `BufEnter` fires on every window the
/// user steps into, and the set it reports is unchanged for all but the
/// one that switched buffers.
pub(super) fn on_buffer_list(model: &mut Model, buffers: Vec<BufferEntry>) -> Vec<Effect> {
    if model.buffers == buffers {
        return Vec::new();
    }
    model.buffers = buffers;
    model.dirty = true;
    Vec::new()
}

/// nvim's own `showtabline`, which decides whether the top row exists at
/// all under `panes = "nvim"`.
///
/// A value that moves the row moves the grid with it through `update()`'s
/// own tail, which compares the row once per fold. A reading that leaves
/// the row where it stands paints nothing: under tiles the option decides
/// nothing, and past one tabpage `1` and `2` are the same picture.
pub(super) fn on_showtabline(model: &mut Model, value: u8) -> Vec<Effect> {
    model.showtabline = value;
    Vec::new()
}

/// Nothing paints on this one: `window zoom` reads
/// [`Model::native_min_pane_size`] the next time it runs, with no repaint
/// involved, so a reading that only moves the floor changes no cell.
pub(super) fn on_min_pane_size(model: &mut Model, width: u16, height: u16) -> Vec<Effect> {
    model.native_min_pane_size = (width, height);
    Vec::new()
}

/// One window's own status, which is what its tile's frame edge reads.
///
/// Compared before it is stored, since a report that left the fields where
/// they were is a repaint of an unchanged picture.
pub(super) fn on_window_status(
    model: &mut Model,
    win: WinHandle,
    status: WindowStatus,
) -> Vec<Effect> {
    if model.window_status.get(&win) == Some(&status) {
        return Vec::new();
    }
    model.window_status.insert(win, status);
    damage_frame_edges(model, win);
    Vec::new()
}

/// The cursor position `win_viewport` carries, folded into the window's
/// status so its tile's ruler follows every motion with no report from the
/// bridge. A window the bridge has not reported yet is left alone: its
/// first report reads the cursor itself.
///
/// `curline` and `curcol` are 0-based, `curcol` in bytes, which is what the
/// report's `nvim_win_get_cursor` reads too, so both put the same number on
/// the ruler.
///
/// `line_count` is folded the same way: nvim resends it when entries are
/// appended from another window under an open quickfix window, which
/// fires no autocmd. An unchanged reading damages nothing, since this runs
/// on every cursor motion.
pub(super) fn on_window_cursor(
    model: &mut Model,
    win: WinHandle,
    curline: u64,
    curcol: u64,
    line_count: Option<u64>,
) {
    let Some(status) = model.window_status.get_mut(&win) else {
        return;
    };
    let row = saturate_u32(curline.saturating_add(1));
    let col = saturate_u32(curcol.saturating_add(1));
    let lines = line_count.map_or(status.lines, saturate_u32);
    if (status.row, status.col, status.lines) == (row, col, lines) {
        return;
    }
    status.row = row;
    status.col = col;
    status.lines = lines;
    damage_frame_edges(model, win);
}

/// Marks the rows a tile's frame edges stand on changed and asks for a
/// frame, so the frame that follows repaints the segments this update
/// moved.
///
/// Only a tile's frame reads a window's status, so under any other look
/// the value is stored and nothing is painted: a cursor motion lands here
/// on every keystroke, and a repaint that changes no cell doubles the
/// paint work per key.
///
/// nvim redraws nothing for a cursor that moved inside a window, and the
/// edge rows lie outside the window's own grid, so without the damage the
/// frame is painted with every edge row clipped out and the segments keep
/// the reading they had.
fn damage_frame_edges(model: &mut Model, win: WinHandle) {
    if model.look.panes != crate::model::Panes::Tiles {
        return;
    }
    model.dirty = true;
    let Some(slot) = model.engine.grids().window_filled(win) else {
        return;
    };
    for row in model.look.edge_rows(slot) {
        model.engine.grids_mut().mark_global_row(row);
    }
}
