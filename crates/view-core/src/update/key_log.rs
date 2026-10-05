//! The key log overlay: opening and closing it, entering it, and the keys
//! it answers once entered.

use crate::model::Model;
use crate::model::OverlayKind;
use crate::msg::Effect;
use crate::native::geometry::{Anchor, OverlayBox};
use crate::native::key_log::KeyLogView;

/// Every key the entered key log answers itself, and what it does.
/// `docs/keymaps.md` carries the rendered table, and
/// `every_documented_key_log_key_answers_a_real_keystroke` presses each.
/// Test-only: nothing in a running session reads it.
#[cfg(test)]
pub(crate) const KEY_LOG_KEYS: &[(&str, &str)] = &[
    ("j", "select the next older mapping"),
    ("k", "select the next newer mapping"),
    ("<C-d>", "select half a screen further down"),
    ("<C-u>", "select half a screen further up"),
    ("gg", "select the newest mapping"),
    ("G", "select the oldest mapping"),
    (
        "y",
        "copy the selected row, to the system clipboard and over OSC 52",
    ),
    ("<Esc>", "close the log"),
];

/// Opens the key log, or closes it when open.
pub(super) fn toggle(model: &mut Model) -> Vec<Effect> {
    if !model.close_key_log() {
        open(model);
    }
    model.dirty = true;
    Vec::new()
}

/// Gives the key log the keyboard, opening it first when it is closed.
pub(super) fn focus(model: &mut Model) -> Vec<Effect> {
    if model.key_log_view_mut().is_none() {
        open(model);
    }
    if let Some((view, _)) = model.key_log_view_mut() {
        view.enter();
    }
    model.dirty = true;
    Vec::new()
}

/// The widest the log draws, which leaves a displaced mapping's whole
/// description readable on a wide screen.
const MAX_WIDTH: u16 = 120;

/// Opens the log at the bottom of the screen, clear of the frames' border
/// rows, as wide as the screen allows up to [`MAX_WIDTH`] and at most 40
/// percent of its height. [`Model::refresh_key_log`] sizes it to its rows
/// and moves it to the top while the cursor is under it.
fn open(model: &mut Model) {
    let view = KeyLogView::open(model.key_log(), model.utc_offset_secs());
    model.push_overlay(
        // a box against the left or right edge docks, and the tiles beside
        // it give up their width to it
        OverlayBox::new(100, 40)
            .with_max_width(MAX_WIDTH)
            .with_anchor(Anchor::Bottom),
        OverlayKind::KeyLog(view),
    );
    let _ = model.refresh_key_log();
}

/// The entered key log's own keys, `None` for a key it does not answer.
pub(super) fn key(model: &mut Model, notation: &str) -> Option<Vec<Effect>> {
    let page = page(model);
    let (view, log) = model.key_log_view_mut()?;
    let armed = view.take_g();
    let moved = match notation {
        "y" => return Some(super::surfaces::copy_selection(view.selected_text())),
        "j" => view.move_selection(log, 1),
        "k" => view.move_selection(log, -1),
        "<C-d>" => view.move_selection(log, page),
        "<C-u>" => view.move_selection(log, -page),
        "g" if armed => view.select(log, 0),
        "g" => {
            view.arm_g();
            false
        }
        "G" => view.select(log, usize::MAX),
        _ => return None,
    };
    model.dirty |= moved;
    Some(Vec::new())
}

/// Half the rows the open box shows, at least one.
fn page(model: &Model) -> isize {
    let rows = model.focused_overlay().map_or(0, |overlay| {
        model
            .overlay_rect(overlay)
            .height
            .saturating_sub(crate::native::key_log::FRAME_ROWS)
    });
    isize::try_from(rows.div_ceil(2))
        .unwrap_or(isize::MAX)
        .max(1)
}
