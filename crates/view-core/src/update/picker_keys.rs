//! The keys the focused picker answers: moving the selection, opening the
//! selected result, closing, and every character edited into the query.

use crate::model::{Model, OverlayKind};
use crate::msg::{Effect, OpenIn, RpcCall};
use crate::native::picker::Picked;
use crate::native::submit_hold::hold_for_open;

use super::route::picker_query;
use super::surfaces::picker_preview_request;

/// What one picker key does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickerAction {
    Move(isize),
    Open(OpenIn),
    Erase,
    Close,
}

/// Every key the focused picker answers itself besides the characters it
/// types into the query, what it does, and how it reads in
/// `docs/keymaps.md`. [`picker_key`] dispatches from it, and
/// `every_documented_picker_key_answers_a_real_keystroke` presses each.
pub(crate) const PICKER_KEYS: &[(&str, PickerAction, &str)] = &[
    ("<Down>", PickerAction::Move(1), "select the next result"),
    ("<C-n>", PickerAction::Move(1), "select the next result"),
    ("<C-j>", PickerAction::Move(1), "select the next result"),
    ("<Up>", PickerAction::Move(-1), "select the previous result"),
    (
        "<C-p>",
        PickerAction::Move(-1),
        "select the previous result",
    ),
    (
        "<C-k>",
        PickerAction::Move(-1),
        "select the previous result",
    ),
    (
        "<CR>",
        PickerAction::Open(OpenIn::Current),
        "open the selected result in the window you came from",
    ),
    (
        "<C-v>",
        PickerAction::Open(OpenIn::Vertical),
        "open the selected result in a vertical split",
    ),
    (
        "<C-x>",
        PickerAction::Open(OpenIn::Horizontal),
        "open the selected result in a horizontal split",
    ),
    (
        "<C-t>",
        PickerAction::Open(OpenIn::Tab),
        "open the selected result in a new tab",
    ),
    (
        "<BS>",
        PickerAction::Erase,
        "delete the last character of the query",
    ),
    ("<Esc>", PickerAction::Close, "close the picker"),
];

/// Answers `notation` for the picker holding the focus. A key that is in
/// [`PICKER_KEYS`] does what the table says, a key that types a character
/// edits the query, and any other key does nothing.
pub(super) fn picker_key(model: &mut Model, notation: &str) -> Vec<Effect> {
    let action = PICKER_KEYS
        .iter()
        .find(|(key, ..)| *key == notation)
        .map(|(_, action, _)| *action);
    let rows = list_rows(model);
    let Some(OverlayKind::Picker(p)) = model.focused_overlay_mut().map(|ov| &mut ov.kind) else {
        return Vec::new();
    };
    match action {
        None if crate::native::keys::notation_char(notation).is_none() => Vec::new(),
        None | Some(PickerAction::Erase) => {
            let generation = p.edit_query(notation);
            vec![picker_query(p, generation)]
        }
        Some(PickerAction::Move(delta)) => {
            if !p.move_selection(delta) {
                return Vec::new();
            }
            p.keep_in_view(rows);
            let effects = picker_preview_request(p);
            model.dirty = true;
            effects
        }
        Some(PickerAction::Close) => {
            model.pop_focused_overlay();
            model.dirty = true;
            // the matcher drops its session, so a scan of a large tree
            // stops walking once nothing reads it
            vec![Effect::PickerClose]
        }
        Some(PickerAction::Open(how)) => {
            let Some(target) = p.selected_target() else {
                return Vec::new();
            };
            model.pop_focused_overlay();
            model.dirty = true;
            let mut effects = open_chosen(model, target, how);
            effects.push(Effect::PickerClose);
            effects
        }
    }
}

/// Opens `target` in the window `how` names, holding the keys typed behind
/// it until nvim answers. The windows view claims go with the open, so a
/// choice made with the cursor in a sidebar never opens inside one.
pub(super) fn open_chosen(model: &mut Model, target: Picked, how: OpenIn) -> Vec<Effect> {
    let claimed = model
        .engine
        .grids()
        .native_window_claims()
        .into_iter()
        .map(|(win, _)| win)
        .collect();
    let (generation, mut effects) = hold_for_open(model);
    effects.push(Effect::Rpc(RpcCall::OpenPicked {
        target,
        how,
        claimed,
        generation,
    }));
    effects
}

/// How many result rows the focused picker's list shows: its frame less
/// the border on each side, the query line and the rule under it.
fn list_rows(model: &Model) -> usize {
    model.focused_overlay().map_or(0, |overlay| {
        usize::from(model.overlay_rect(overlay).height).saturating_sub(4)
    })
}

#[cfg(test)]
mod tests;
