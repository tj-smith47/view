//! The `view_bridge` channel's arms.
//!
//! nvim raises no redraw event for a buffer's name, its filetype, its
//! diagnostics or a window's cursor position, so view installs autocmds of
//! its own and they arrive here. Most of them change text on screen and
//! nothing else, so they return no RPC beyond the tree's git refresh; the
//! tab-row option is the one that can move a row the engine's own grid is
//! laid out against.

use crate::events::WinHandle;
use crate::model::{BufferEntry, Model, WindowStatus};
use crate::msg::{Effect, RpcCall};
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
/// A value that moves the row across nvim's own threshold moves the grid
/// with it, the same `TryResize` a second tabpage opening produces
/// (`ui_event::apply_ui_event`'s `TablineUpdate` arm): the engine lays its
/// windows out against the rows view leaves it, and a grid a row too tall
/// paints its last line under the bar.
///
/// A reading that leaves the row where it stands paints nothing either,
/// for [`on_buffer_list`]'s reason: under tiles the row is up whatever the
/// option says, and past one tabpage `1` and `2` are the same picture.
pub(super) fn on_showtabline(model: &mut Model, value: u8) -> Vec<Effect> {
    if model.showtabline == value {
        return Vec::new();
    }
    let before = model.chrome_rows();
    model.showtabline = value;
    if before == model.chrome_rows() {
        return Vec::new();
    }
    model.dirty = true;
    let (width, height) = model.grid_target();
    vec![Effect::Rpc(RpcCall::TryResize { width, height })]
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
/// Compared before it is stored: the trigger fires per event-loop tick the
/// cursor moved in, and a tick that left the reported fields where they
/// were is a repaint of an unchanged picture.
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
    model.dirty = true;
    Vec::new()
}

/// Marks the rows a tile's frame edges stand on changed, so the frame that
/// follows repaints the segments this update moved.
///
/// nvim redraws nothing for a cursor that moved inside a window, and the
/// edge rows lie outside the window's own grid, so without this the frame
/// is painted with every edge row clipped out of its damage and the
/// segments keep the reading they had.
fn damage_frame_edges(model: &mut Model, win: WinHandle) {
    if model.look.panes != crate::model::Panes::Tiles {
        return;
    }
    let Some(slot) = model.engine.grids().window_slot(win) else {
        return;
    };
    for row in model.look.edge_rows(slot) {
        model.engine.grids_mut().mark_global_row(row);
    }
}
