//! Where one bracketed paste goes when a native surface, not the engine,
//! owns the keyboard.
//!
//! Sits beside `route_key` rather than inside it: a paste is one insertion
//! of text and never a key, so no surface here may read what it carries as
//! the notation of anything -- a pasted `<CR>` is five characters of a
//! prompt, which is the whole reason a terminal brackets one.

use crate::model::{Model, OverlayKind};
use crate::msg::{Effect, RpcCall};

use super::picker_query;

/// Delivers one bracketed paste to whichever native surface owns the
/// keyboard: the picker takes it into its filter, a `vim.fn.input()` prompt
/// takes it as typed keys, the agent panel's composer takes it at its
/// cursor, and a surface that composes no text answers with a notice rather
/// than swallowing it.
///
/// One insertion, never a submission, whatever the text ends with: that a
/// pasted trailing newline does not press `<CR>` is the whole reason
/// bracketed paste exists, and the composer's own `<CR>` arm is the only
/// thing that starts a turn.
pub(super) fn paste_into_focused_surface(model: &mut Model, text: &str) -> Vec<Effect> {
    // A gesture that carried nothing changes nothing: no re-query for the
    // picker (which would drop its selection back to the first row the user
    // had already moved off), no notice for a surface that could not have
    // shown the text anyway, and no repaint.
    if text.is_empty() {
        return Vec::new();
    }
    // One read of the focused overlay, and every surface that answers the
    // paste itself answers from inside it: what falls out is the single
    // question the panel below still has to ask of `model`.
    let panel_has_the_keyboard = match model.focused_overlay_mut().map(|ov| &mut ov.kind) {
        Some(OverlayKind::Picker(p)) => {
            let generation = p.paste_query(text);
            return vec![picker_query(p, generation)];
        }
        // the prompt's own keys reach nvim the same way (see this match's
        // `Prompt` arm in `route_key`), because the engine is blocked inside
        // `vim.fn.input()` waiting for exactly them
        Some(OverlayKind::Prompt(p)) if p.takes_typed_text() => {
            let keys = as_prompt_keys(text);
            if keys.is_empty() {
                return Vec::new();
            }
            return vec![Effect::Rpc(RpcCall::Input { notation: keys })];
        }
        Some(OverlayKind::Ai) => true,
        _ => false,
    };
    if panel_has_the_keyboard && !model.ai_panel().an_owner_holds_the_keys() {
        // Verbatim, control characters and all: the shape a paste reads in
        // is a question for the wrap that paints it (`AiPanelState`'s own,
        // which ends a row at each line break), never for the text held
        // here, so what the agent is sent stays exactly what was copied.
        model.ai_panel_mut().input.push_str(text);
        model.dirty = true;
        return Vec::new();
    }
    let notice = if panel_has_the_keyboard {
        // The only state left that owns the panel's keys, so the notice is
        // named directly rather than chosen; see
        // `AiPanelState::an_owner_holds_the_keys`.
        PERMISSION_PASTE_NOTICE
    } else {
        NO_TEXT_INPUT_NOTICE
    };
    model.dirty = true;
    model.engine.record_native_notice(notice.to_string(), false)
}

/// `text` as the keys `nvim_input` types into a prompt blocked in
/// `vim.fn.input()`: nvim's own `<lt>` escape for a literal `<`, every
/// control character as the space it would paint as, and no trailing
/// whitespace on a pasted path.
///
/// Through the typeahead rather than as [`RpcCall::Paste`]: the engine is
/// blocked in its own input loop, which is where its keys are already
/// forwarded to and the only place this prompt is reading from.
///
/// The control characters are the point. Left as they are, a pasted newline
/// is the `<CR>` that submits the prompt with half a path in it -- the very
/// thing bracketed paste exists to prevent.
fn as_prompt_keys(text: &str) -> String {
    let mut keys = String::with_capacity(text.len());
    for ch in text.trim_end().chars() {
        match ch {
            '<' => keys.push_str("<lt>"),
            c if c.is_control() => keys.push(' '),
            c => keys.push(c),
        }
    }
    keys
}

/// What a paste is answered with while an unanswered permission request
/// owns the entered panel's keys. The offered options are on screen above
/// the composer with their own keys, so this names the decision rather
/// than restating them.
///
/// `<Esc>` is deliberately never offered: at a pending permission it is an
/// answer -- the request is settled `Cancelled` and the agent's tool call
/// is gone (see `route_key`'s `OverlayKind::Ai` arm) -- so advising it
/// would spend a decision the reader had not made, for a paste.
pub(super) const PERMISSION_PASTE_NOTICE: &str =
    "view: the agent is waiting on this request -- answer it, and the composer takes text again";

/// What a paste answers with at a focused native surface that composes no
/// text: the tree, a confirm prompt, an `inputlist()` prompt. States that
/// the text went nowhere and offers no key, because the key that leaves one
/// of these surfaces leaves the reader pasting into the buffer they were
/// not aiming at -- and at one of them it answers a question instead.
pub(super) const NO_TEXT_INPUT_NOTICE: &str = "view: this surface takes no pasted text";
