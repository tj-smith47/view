//! Crossterm mouse event to nvim `nvim_input_mouse` vocabulary encoding.

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use view_core::msg::MouseInput;

/// Encodes a crossterm [`MouseEvent`] as a [`MouseInput`] in nvim's
/// `nvim_input_mouse` vocabulary (`button`/`action` strings per
/// `nvim --api-info`'s `nvim_input_mouse(String button, String action,
/// String modifier, Integer grid, Integer row, Integer col)` signature).
///
/// `row`/`col` pass through `ev.row`/`ev.column` unchanged: crossterm
/// already reports these in terminal cell coordinates, zero-based, the same
/// units nvim's redraw events use. `update()` is what maps them into engine
/// grid coordinates (subtracting reserved chrome rows), since only it has
/// that model state; this function stays chrome-agnostic like
/// [`crate::keys::encode_key`] stays engine-agnostic.
///
/// Total over every [`MouseEventKind`], unlike [`crate::keys::encode_key`]
/// (which returns `None` for key codes with no nvim equivalent): every
/// mouse event kind has an nvim button/action pair, so there is no "no
/// encoding" case for this function to signal, and it returns a plain
/// [`MouseInput`] rather than an `Option`.
#[must_use]
pub fn encode_mouse(ev: &MouseEvent) -> MouseInput {
    let (button, action) = match ev.kind {
        MouseEventKind::Down(b) => (mouse_button(b), "press"),
        MouseEventKind::Up(b) => (mouse_button(b), "release"),
        MouseEventKind::Drag(b) => (mouse_button(b), "drag"),
        // action is documented as ignored for "move"; using the same
        // string as the button keeps a decoded event readable in logs
        MouseEventKind::Moved => ("move", "move"),
        MouseEventKind::ScrollDown => ("wheel", "down"),
        MouseEventKind::ScrollUp => ("wheel", "up"),
        MouseEventKind::ScrollLeft => ("wheel", "left"),
        MouseEventKind::ScrollRight => ("wheel", "right"),
    };
    MouseInput {
        button: button.to_string(),
        action: action.to_string(),
        modifier: mouse_modifier(ev.modifiers),
        row: ev.row,
        col: ev.column,
    }
}

/// Maps a crossterm [`MouseButton`] to its nvim button name.
fn mouse_button(button: MouseButton) -> &'static str {
    match button {
        MouseButton::Left => "left",
        MouseButton::Right => "right",
        MouseButton::Middle => "middle",
    }
}

/// Builds nvim's mouse modifier string in the same `C-`/`S-`/`M-` order
/// [`crate::keys::encode_key`] uses for key notation, so the two encoders
/// read consistently. `SUPER`/`HYPER`/`META` are not folded in, matching
/// `encode_key`'s existing precedent of only surfacing Ctrl/Shift/Alt.
fn mouse_modifier(modifiers: KeyModifiers) -> String {
    let mut out = String::new();
    if modifiers.contains(KeyModifiers::CONTROL) {
        out.push_str("C-");
    }
    if modifiers.contains(KeyModifiers::SHIFT) {
        out.push_str("S-");
    }
    if modifiers.contains(KeyModifiers::ALT) {
        out.push_str("M-");
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn ev(kind: MouseEventKind, mods: KeyModifiers) -> MouseEvent {
        MouseEvent {
            kind,
            column: 10,
            row: 4,
            modifiers: mods,
        }
    }

    #[test]
    fn press_drag_release_map_to_button_and_action() {
        let m = encode_mouse(&ev(
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::NONE,
        ));
        assert_eq!((m.button.as_str(), m.action.as_str()), ("left", "press"));

        let m = encode_mouse(&ev(
            MouseEventKind::Drag(MouseButton::Right),
            KeyModifiers::NONE,
        ));
        assert_eq!((m.button.as_str(), m.action.as_str()), ("right", "drag"));

        let m = encode_mouse(&ev(
            MouseEventKind::Up(MouseButton::Middle),
            KeyModifiers::NONE,
        ));
        assert_eq!(
            (m.button.as_str(), m.action.as_str()),
            ("middle", "release")
        );
    }

    #[test]
    fn scroll_directions_map_to_the_wheel_button() {
        for (kind, expected_action) in [
            (MouseEventKind::ScrollDown, "down"),
            (MouseEventKind::ScrollUp, "up"),
            (MouseEventKind::ScrollLeft, "left"),
            (MouseEventKind::ScrollRight, "right"),
        ] {
            let m = encode_mouse(&ev(kind, KeyModifiers::NONE));
            assert_eq!(m.button, "wheel");
            assert_eq!(m.action, expected_action);
        }
    }

    #[test]
    fn moved_maps_to_the_move_button() {
        let m = encode_mouse(&ev(MouseEventKind::Moved, KeyModifiers::NONE));
        assert_eq!(m.button, "move");
    }

    #[test]
    fn row_and_col_pass_through_unchanged() {
        let m = encode_mouse(&ev(MouseEventKind::Moved, KeyModifiers::NONE));
        assert_eq!((m.row, m.col), (4, 10));
    }

    #[test]
    fn modifiers_combine_in_c_s_m_order() {
        let m = encode_mouse(&ev(
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT | KeyModifiers::ALT,
        ));
        assert_eq!(m.modifier, "C-S-M-");
    }

    #[test]
    fn no_modifiers_is_an_empty_string() {
        let m = encode_mouse(&ev(
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::NONE,
        ));
        assert!(m.modifier.is_empty());
    }
}
