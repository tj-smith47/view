//! Where one terminal mouse event goes: the overlay under the pointer, the
//! grid whose pane covers the cell, or nowhere at all.
//!
//! Two translations happen here and nowhere else, because this is the only
//! place that holds both halves. The terminal's row becomes an engine row
//! by dropping the chrome rows view reserved above the grid, and the engine
//! position becomes a grid and a position inside it by asking the pane
//! registry what the pointer visually hit. Under `ext_multigrid` the two
//! are inseparable: the same `(row, col)` names a different cell in every
//! window, so a screen coordinate sent with the wrong grid lands in the
//! wrong buffer rather than nowhere.

use crate::grid::registry::GridId;
use crate::model::{Model, MouseCapture};
use crate::msg::{Effect, MouseInput, RpcCall};

/// Routes one mouse event to the surface that owns it.
///
/// A press claims the gesture, and the `drag`s and the `release` that
/// follow go wherever the press went, however far the pointer travels.
/// Routing every event by its own position instead would truncate any drag
/// crossing an overlay edge or a window separator: the engine would see a
/// press with no release and stay stuck mid-selection, or a release for a
/// press it never saw. `wheel` and `move` carry no gesture, so they always
/// route by position and leave an in-flight capture alone.
pub(super) fn route(model: &mut Model, input: MouseInput) -> Vec<Effect> {
    let owner = match input.action.as_str() {
        "press" => {
            let owner = position_owner(model, &input);
            if let Some(owner) = owner {
                model.capture_mouse(owner);
            }
            owner
        }
        "drag" | "release" => {
            // a gesture whose press was never seen (input started mid-drag)
            // has no owner to honor, so it falls back to position
            let owner = model
                .mouse_capture()
                .or_else(|| position_owner(model, &input));
            if input.action == "release" {
                model.release_mouse();
            }
            owner
        }
        _ => position_owner(model, &input),
    };
    match owner {
        // no overlay carries a mouse handler, so an overlay claiming the
        // event is the whole of that routing; nothing claiming it at all is
        // a cell view's own chrome owns and the engine has no window under
        None | Some(MouseCapture::Overlay(_)) => Vec::new(),
        Some(MouseCapture::Engine(grid)) => effect(model, input, grid),
    }
}

/// Which surface the pointer is over: the topmost overlay covering the
/// cell, the grid whose pane covers it, or nothing at all.
///
/// Nothing is the answer for a row the tabline reserved and for a cell
/// between windows that view's own separator chrome owns. Neither has a
/// handler and neither is a position the engine has a window at, so a
/// press there starts no gesture -- but a `drag` or `release` crossing one
/// never reaches this function, because the press it belongs to already
/// named an owner.
fn position_owner(model: &Model, input: &MouseInput) -> Option<MouseCapture> {
    if let Some(id) = model.overlay_at(input.row, input.col) {
        return Some(MouseCapture::Overlay(id));
    }
    let offset = model.look.grid_offset();
    let row = input
        .row
        .checked_sub(model.chrome_rows())?
        .checked_sub(offset)?;
    let col = input.col.checked_sub(offset)?;
    let (grid, _, _) = model.engine.grids().hit_test(col, row)?;
    Some(MouseCapture::Engine(grid))
}

/// Maps one mouse event already routed to `grid` into the
/// `RpcCall::InputMouse` that delivers it there.
///
/// Clamped into the grid rather than hit-tested again: this event may be
/// the `release` of a gesture that has since wandered onto a separator or
/// up into the tabline, and while the pointer is inside the grid the clamp
/// is the same translation the hit test made.
fn effect(model: &Model, input: MouseInput, grid: GridId) -> Vec<Effect> {
    let offset = model.look.grid_offset();
    let row = input
        .row
        .saturating_sub(model.chrome_rows())
        .saturating_sub(offset);
    let Some((col, row)) =
        model
            .engine
            .grids()
            .clamp_into(grid, input.col.saturating_sub(offset), row)
    else {
        return Vec::new();
    };
    vec![Effect::Rpc(RpcCall::InputMouse {
        button: input.button,
        action: input.action,
        modifier: input.modifier,
        grid,
        row,
        col,
    })]
}
