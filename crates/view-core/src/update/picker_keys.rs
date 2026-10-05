//! The keys the focused picker answers: moving the selection, opening the
//! selected result, and every other key edited into the query.

use crate::model::{Model, OverlayKind};
use crate::msg::{Effect, OpenIn, RpcCall};
use crate::native::submit_hold::hold_for_open;

use super::route::picker_query;
use super::surfaces::picker_preview_request;

/// Every key the focused picker answers itself, and what it does.
/// `docs/keymaps.md` carries the rendered table, and
/// `every_documented_picker_key_answers_a_real_keystroke` presses each.
/// Test-only: nothing in a running session reads it.
#[cfg(test)]
pub(crate) const PICKER_KEYS: &[(&str, &str)] = &[
    ("<Down>", "select the next result"),
    ("<C-n>", "select the next result"),
    ("<C-j>", "select the next result"),
    ("<Up>", "select the previous result"),
    ("<C-p>", "select the previous result"),
    ("<C-k>", "select the previous result"),
    (
        "<CR>",
        "open the selected result in the window you came from",
    ),
    ("<C-v>", "open the selected result in a vertical split"),
    ("<C-x>", "open the selected result in a horizontal split"),
    ("<C-t>", "open the selected result in a new tab"),
    ("<BS>", "delete the last character of the query"),
    ("<Esc>", "close the picker"),
];

/// What one picker key does.
enum Action {
    Move(isize),
    Open(OpenIn),
}

/// The action `notation` names, or `None` for a key that edits the query.
fn action(notation: &str) -> Option<Action> {
    Some(match notation {
        "<Down>" | "<C-n>" | "<C-j>" => Action::Move(1),
        "<Up>" | "<C-p>" | "<C-k>" => Action::Move(-1),
        "<CR>" => Action::Open(OpenIn::Current),
        "<C-v>" => Action::Open(OpenIn::Vertical),
        "<C-x>" => Action::Open(OpenIn::Horizontal),
        "<C-t>" => Action::Open(OpenIn::Tab),
        _ => return None,
    })
}

/// Answers `notation` for the picker holding the focus.
pub(super) fn picker_key(model: &mut Model, notation: &str) -> Vec<Effect> {
    let Some(OverlayKind::Picker(p)) = model.focused_overlay_mut().map(|ov| &mut ov.kind) else {
        return Vec::new();
    };
    match action(notation) {
        None => {
            let generation = p.edit_query(notation);
            vec![picker_query(p, generation)]
        }
        Some(Action::Move(delta)) => {
            if !p.move_selection(delta) {
                return Vec::new();
            }
            let effects = picker_preview_request(p);
            model.dirty = true;
            effects
        }
        Some(Action::Open(how)) => {
            let Some(target) = p.selected_target() else {
                return Vec::new();
            };
            model.pop_focused_overlay();
            model.dirty = true;
            let (generation, mut effects) = hold_for_open(model);
            effects.push(Effect::Rpc(RpcCall::OpenPicked {
                target,
                how,
                previous_window: false,
                generation,
            }));
            effects.push(Effect::PickerClose);
            effects
        }
    }
}

#[cfg(test)]
mod tests;
