//! What the engine owes when the window layout changes.
//!
//! Four unrelated events reach the same question -- a window placement, a
//! winbar margin, a look flip and a terminal resize -- and none of their
//! arms is the place to answer it, so the answer lives here once.
//!
//! The accent the active frame carries belongs to the look as well, and
//! the reply to its colour probe is taken here.

use crate::grid::registry::GridId;
use crate::model::{Look, Model, Panes};
use crate::msg::{Effect, RpcCall};

/// The inner size `grid`'s window owes nvim, or nothing when its slot, the
/// look and its top margin are all where the last request left them.
pub(crate) fn request_for(model: &mut Model, grid: GridId) -> Vec<Effect> {
    let look = model.look;
    model
        .engine
        .grids_mut()
        .pending_inner_request(grid, look)
        .into_iter()
        .map(|(width, height)| {
            Effect::Rpc(RpcCall::TryResizeGrid {
                grid,
                width,
                height,
            })
        })
        .collect()
}

/// The accent probe's reply, ignored once a newer probe has gone out: the
/// engine keeps one slot for the answer.
pub(crate) fn accent_reply(
    model: &mut Model,
    generation: u64,
    function_fg: Option<u32>,
    statement_fg: Option<u32>,
) -> Vec<Effect> {
    if generation == model.engine.hl().probe_generation() {
        model.engine.confirm_accent(function_fg, statement_fg);
        model.dirty = true;
    }
    Vec::new()
}

/// What every window grid owes, in ascending grid order.
pub(crate) fn request_all(model: &mut Model) -> Vec<Effect> {
    let grids = model.engine.grids().window_grids();
    let mut effects = Vec::new();
    for grid in grids {
        effects.append(&mut request_for(model, grid));
    }
    effects
}

/// Switches the look and returns everything the change owes: the outer
/// grid's own size, which the ring comes out of, then each window's inner
/// size inside the slot nvim gives it.
pub(crate) fn set_look(model: &mut Model, look: Look) -> Vec<Effect> {
    if model.look == look {
        return Vec::new();
    }
    model.engine.grids_mut().set_look(look);
    model.look = look;
    model.dirty = true;
    let (width, height) = model.grid_target();
    let mut effects = vec![Effect::Rpc(RpcCall::TryResize { width, height })];
    effects.append(&mut request_all(model));
    effects
}

/// `:View ui panes [tiles|nvim|auto]`: the report with no argument, the
/// switch with one.
pub(crate) fn invoke(model: &mut Model, verb: &str) -> Vec<Effect> {
    let mut words = verb.split_whitespace();
    if words.next() != Some("panes") {
        return unknown(model, verb);
    }
    let Some(word) = words.next() else {
        let notice = report(model);
        model.dirty = true;
        return model.engine.record_native_notice(notice, false);
    };
    let panes = match word {
        "tiles" => Some(Panes::Tiles),
        "nvim" => Some(Panes::Nvim),
        "auto" => model.detected_look.panes,
        _ => return unknown(model, verb),
    };
    let Some(panes) = panes else {
        return unknown(model, verb);
    };
    let mut effects = set_look(model, Look::new(panes, model.look.gaps));
    let notice = report(model);
    effects.append(&mut model.engine.record_native_notice(notice, false));
    effects
}

/// The line `:View ui panes` prints: the mode, and the environment marker
/// behind it where the session detected one.
fn report(model: &Model) -> String {
    let mode = match model.look.panes {
        Panes::Tiles => "tiles",
        _ => "nvim",
    };
    match model.detected_look.marker {
        Some(marker) => format!("ui.panes = {mode} ({marker})"),
        None => format!("ui.panes = {mode}"),
    }
}

fn unknown(model: &mut Model, verb: &str) -> Vec<Effect> {
    model.dirty = true;
    model
        .engine
        .record_native_notice(format!("view: no such form: ui {verb}"), false)
}
