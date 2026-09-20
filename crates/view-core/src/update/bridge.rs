//! The `view_bridge` channel's arms.
//!
//! nvim raises no redraw event for a buffer's name, its filetype, its
//! diagnostics or a window's cursor position, so view installs autocmds of
//! its own and they arrive here. Every one of them changes text on screen
//! and nothing else, which is why none returns an RPC of its own beyond the
//! tree's git refresh.

use crate::events::WinHandle;
use crate::model::{Model, WindowStatus};
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
///
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
