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
    // ahead of the size: the tab line is a row the outer grid does not
    // get, so the surface moves first and one resize carries the new
    // height
    let mut effects = follow_the_look_with_the_tabline(model);
    let (width, height) = model.grid_target();
    effects.push(Effect::Rpc(RpcCall::TryResize { width, height }));
    effects.append(&mut look_keyed_holds(model));
    effects.append(&mut request_all(model));
    effects
}

/// The tab line surface the new look derives, taken or handed back on the
/// running UI.
///
/// `[native] tabline` has no fixed default: tiles draws the pill, nvim mode
/// leaves the row to whatever the user's own config puts there. A flip that
/// left the surface where the session started it would give a session
/// flipped to tiles no pill, and one flipped to nvim mode a pill over the
/// tab row the user's own config draws. `ext_tabline_toggle.rs` is the live
/// proof the engine honours the change after the attach.
///
/// Nothing at all when the user spelled the key: that value is theirs under
/// both looks.
fn follow_the_look_with_the_tabline(model: &mut Model) -> Vec<Effect> {
    use crate::native::ext::Ext;
    if !model.tabline_follows_look {
        return Vec::new();
    }
    let on = model.look.panes == Panes::Tiles;
    if model.owns(Ext::Tabline) == on {
        return Vec::new();
    }
    let mut surfaces = model.attached_surfaces().to_vec();
    surfaces.retain(|held| *held != Ext::Tabline);
    if on {
        surfaces.push(Ext::Tabline);
    }
    model.attach_surfaces(surfaces);
    vec![Effect::Rpc(RpcCall::SetUiExt {
        surface: Ext::Tabline,
        on,
    })]
}

/// The holds the new look owes, re-issued.
///
/// An option whose held value is keyed by look (`laststatus`, today's only
/// one) was set by the takeover for the look the session started in, and
/// nothing else would move it: the flip has to re-issue it or nvim keeps
/// drawing a status row the frame no longer paints over, or drops the one
/// it does.
///
/// Read off the channel table rather than named here, so the option a look
/// decides is stated once, and gated on the same `view_draws` predicate the
/// takeover's own session-held walk uses -- a surface the user handed back
/// is one view does not set options for.
fn look_keyed_holds(model: &Model) -> Vec<Effect> {
    use crate::native::channels::{Channel, ChannelValue, Scope, CHANNELS};
    let mut effects = Vec::new();
    for entry in CHANNELS {
        if !crate::native::surfaces::view_draws(entry.surface, model) {
            continue;
        }
        for channel in entry.channels {
            let Channel::Hold {
                option,
                scope,
                value: value @ ChannelValue::ByLook { .. },
            } = *channel
            else {
                continue;
            };
            let name = option.to_string();
            let value = value.wire(model.look);
            effects.push(Effect::Rpc(match scope {
                Scope::Global => RpcCall::HoldOption { name, value },
                Scope::Window => RpcCall::HoldWindowOption { name, value },
            }));
        }
    }
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
    // the row is the half of the flip a person sees before anything else
    // moves, and under an explicit `[native] tabline` it does not move at
    // all, so the line says where it went rather than leaving them to look
    let row = if crate::native::pill::shows(model) {
        "the top row is view's"
    } else {
        "the top row is nvim's"
    };
    match model.detected_look.marker {
        Some(marker) => format!("ui.panes = {mode} ({marker}), {row}"),
        None => format!("ui.panes = {mode}, {row}"),
    }
}

fn unknown(model: &mut Model, verb: &str) -> Vec<Effect> {
    model.dirty = true;
    model
        .engine
        .record_native_notice(format!("view: no such form: ui {verb}"), false)
}
