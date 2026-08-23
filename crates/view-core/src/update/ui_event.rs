//! The fold from one decoded UI event to the model it changes.
//!
//! Its own file because it is the widest match in the crate -- every grid,
//! message, cmdline, popupmenu and tabline event nvim can send -- and it
//! changes for reasons the message fold beside it never shares.

use crate::events::{clamp_dim, saturate_u16, UiEvent};
use crate::grid::GridOp;
use crate::hl::HlAttr;
use crate::model::{CmdlineState, Model, OverlayKind, PopupmenuState, TablineState};
use crate::msg::{Effect, RpcCall};
use crate::native::prompt::PromptState;
use crate::native::statusline::SegmentUpdate;

/// Applies one decoded redraw sub-event to `model`, returning any effects
/// it produces. Only [`UiEvent::TablineUpdate`] can produce one: crossing
/// the 1-tab chrome-reservation boundary (either direction) changes the
/// grid target size, which the loop's executor must forward to the engine
/// as a `TryResize` the same way a terminal resize does.
pub(super) fn apply_ui_event(model: &mut Model, ev: UiEvent) -> Vec<Effect> {
    match ev {
        UiEvent::GridResize { width, height, .. } => {
            // clamp untrusted wire dimensions: a desynced or malformed
            // grid_resize must not allocate unboundedly, and a plain `as
            // u16` cast would silently truncate 65536 to 0
            model.engine.apply_grid(GridOp::Resize {
                width: clamp_dim(width),
                height: clamp_dim(height),
            });
            Vec::new()
        }
        UiEvent::GridLine {
            row,
            col_start,
            cells,
            ..
        } => {
            model.engine.apply_grid(GridOp::PutLine {
                row: saturate_u16(row),
                col_start: saturate_u16(col_start),
                cells: cells
                    .into_iter()
                    .map(|c| (c.text, c.hl_id, c.repeat))
                    .collect(),
            });
            Vec::new()
        }
        UiEvent::GridCursorGoto { row, col, .. } => {
            model.engine.apply_grid(GridOp::CursorGoto {
                row: saturate_u16(row),
                col: saturate_u16(col),
            });
            Vec::new()
        }
        UiEvent::GridScroll {
            top,
            bot,
            left,
            right,
            rows,
            ..
        } => {
            model.engine.apply_grid(GridOp::Scroll {
                top: saturate_u16(top),
                bot: saturate_u16(bot),
                left: saturate_u16(left),
                right: saturate_u16(right),
                rows: i32::try_from(rows).unwrap_or(if rows > 0 { i32::MAX } else { i32::MIN }),
            });
            Vec::new()
        }
        UiEvent::GridClear { .. } => {
            model.engine.apply_grid(GridOp::Clear);
            Vec::new()
        }
        UiEvent::HlAttrDefine {
            id,
            fg,
            bg,
            bold,
            italic,
            underline,
            reverse,
        } => {
            model.engine.define_hl_attr(
                id,
                HlAttr {
                    fg,
                    bg,
                    bold,
                    italic,
                    underline,
                    reverse,
                },
            );
            Vec::new()
        }
        UiEvent::DefaultColorsSet { fg, bg, .. } => {
            // the generation comes back from the write that opened it, so
            // the emitted probe carries the exact generation these colors
            // own; a stale reply for an older generation is dropped by the
            // Msg::HlProbeReply arm above, and Theme::from_hl falls back to
            // the (possibly still wire-ambiguous) raw values until a
            // matching reply lands
            let generation = model.engine.set_hl_default_colors(fg, bg);
            vec![Effect::Rpc(RpcCall::GetDefaultHl { generation })]
        }
        UiEvent::HlGroupSet { name, hl_id } => {
            model.engine.set_hl_group(name, hl_id);
            Vec::new()
        }
        UiEvent::Flush => {
            model.dirty = true;
            // idempotent past the first Flush: see Model::content_painted's
            // doc comment for why this never resets
            model.content_painted = true;
            // records that one full paint cycle has happened, so a
            // transient toast pushed in this same batch is guaranteed at
            // least one visible frame before dismiss_transient_on_keypress
            // can drop it (see Messages::note_flush)
            model.engine.messages.note_flush();
            Vec::new()
        }
        UiEvent::ModeInfoSet {
            cursor_style_enabled,
            modes,
        } => {
            model.engine.mode.cursor_style_enabled = cursor_style_enabled;
            model.engine.mode.modes = modes;
            Vec::new()
        }
        UiEvent::ModeChange { mode, mode_idx } => {
            model.engine.mode.current = mode;
            model.engine.mode.current_idx = mode_idx;
            Vec::new()
        }
        UiEvent::CmdlineShow {
            content,
            pos,
            firstc,
            prompt,
            indent,
            level,
        } => {
            let cmdline = CmdlineState {
                content,
                pos,
                firstc,
                prompt,
                indent,
                level,
            };
            // covers both the prompt's first arrival and every re-arm after
            // an unmatched key: the two are wire-identical, so re-learning
            // unconditionally on every CmdlineShow is simpler than trying
            // to tell them apart
            if let Some(ov) = model.focused_overlay_mut() {
                if let OverlayKind::Prompt(p) = &mut ov.kind {
                    p.learn_cmdline(&cmdline);
                    // the choices arrive here, one event after the question
                    // the box was first sized to, and a confirm() whose
                    // widest label beats its question needs the extra cells
                    ov.geometry = p.overlay_box();
                }
            }
            model.engine.cmdline = Some(cmdline);
            Vec::new()
        }
        UiEvent::CmdlinePos { pos, level } => {
            if let Some(cmdline) = &mut model.engine.cmdline {
                cmdline.pos = pos;
                cmdline.level = level;
            }
            Vec::new()
        }
        UiEvent::CmdlineHide => {
            model.engine.cmdline = None;
            Vec::new()
        }
        UiEvent::MsgShow {
            kind,
            content,
            replace_last,
        } => {
            // classification, push, history, and expiry-scheduling all
            // happen inside record_message -- the wire event owns no copy
            // of that sequence, only the three fields the choke point needs
            let effects = model.engine.record_message(kind, content, replace_last);
            // a confirm-class msg_show while a Prompt overlay is already
            // open replaces its state wholesale rather than being skipped:
            // a genuine re-arm of the SAME question (an unmatched key)
            // never sends a second msg_show at all (see
            // docs/prompt-overlay-wire-capture.md, section 1), so every
            // msg_show this branch ever sees while a Prompt overlay is open
            // names a distinct new question -- routine when nvim's own Lua
            // resolves one confirm and immediately raises another with no
            // intervening keystroke, as plugin bootstraps do. Replacing
            // both the message and the (still-Pending) answer together
            // keeps them from a prior question's leftovers, so the paired
            // CmdlineShow that follows learns choices for the same
            // question this state's message now names.
            let prompt_state = model
                .engine
                .messages
                .entries
                .last()
                .and_then(PromptState::from_entry);
            if let Some(state) = prompt_state {
                match model.focused_overlay_mut() {
                    // the geometry travels with the state: a box sized to
                    // the question it replaced would leave the new one
                    // truncated or floating in empty cells
                    Some(ov) if matches!(ov.kind, OverlayKind::Prompt(_)) => {
                        ov.geometry = state.overlay_box();
                        ov.kind = OverlayKind::Prompt(state);
                    }
                    _ => {
                        model.push_overlay(state.overlay_box(), OverlayKind::Prompt(state));
                    }
                }
            }
            effects
        }
        UiEvent::MsgClear => {
            model.engine.messages.clear();
            Vec::new()
        }
        UiEvent::MsgShowmode { content } => {
            let text: String = content.iter().map(|(_, t)| t.as_str()).collect();
            model.engine.statusline.apply(SegmentUpdate::Mode(text));
            model.dirty = true;
            Vec::new()
        }
        UiEvent::MsgShowcmd { content } => {
            let text: String = content.iter().map(|(_, t)| t.as_str()).collect();
            model.engine.statusline.apply(SegmentUpdate::Showcmd(text));
            model.dirty = true;
            Vec::new()
        }
        UiEvent::MsgRuler { content } => {
            let text: String = content.iter().map(|(_, t)| t.as_str()).collect();
            model.engine.statusline.apply(SegmentUpdate::Ruler(text));
            model.dirty = true;
            Vec::new()
        }
        UiEvent::TablineUpdate { current, tabs } => {
            let before = model.chrome_rows();
            model.engine.tabline = Some(TablineState { current, tabs });
            let after = model.chrome_rows();
            if before == after {
                Vec::new()
            } else {
                let (grid_width, grid_height) = model.grid_target();
                vec![Effect::Rpc(RpcCall::TryResize {
                    width: grid_width,
                    height: grid_height,
                })]
            }
        }
        UiEvent::PopupmenuShow {
            items,
            selected,
            row,
            col,
            grid,
        } => {
            model.engine.popupmenu = Some(PopupmenuState {
                items,
                selected,
                row,
                col,
                grid,
            });
            Vec::new()
        }
        UiEvent::PopupmenuSelect { selected } => {
            if let Some(pm) = &mut model.engine.popupmenu {
                pm.selected = selected;
            }
            Vec::new()
        }
        UiEvent::PopupmenuHide => {
            model.engine.popupmenu = None;
            Vec::new()
        }
        UiEvent::MouseOn => {
            model.engine.mouse_on = true;
            Vec::new()
        }
        UiEvent::MouseOff => {
            model.engine.mouse_on = false;
            Vec::new()
        }
        UiEvent::UiSend { content } => forward_ui_send(&content),
        // no cell content and no chrome state: the window's own cells arrive
        // as `grid_line`, and the only reader of a viewport is
        // `native::speculate`, which the host folds a batch through before
        // this applier ever sees it
        UiEvent::WinViewport { .. } | UiEvent::Unknown { .. } => Vec::new(),
    }
}

/// The forwarding policy for one `nvim_ui_send` payload.
///
/// A whitelist, and narrow on purpose: view is not a terminal host. It writes
/// to the terminal but reads its stdin through crossterm's key decoder, so
/// any sequence the terminal answers would come back as keystrokes typed into
/// the buffer. Only a self-contained OSC 52 clipboard *write* is safe to pass
/// through -- it provokes no reply -- and it is also the one payload with a
/// real cost to dropping, since it is how nvim's own clipboard provider
/// performs a `"+y` (see [`Effect::TermWrite`]).
///
/// nvim's own startup queries never reach here at all: view claims the tty
/// only after nvim's `nvim.tty` defaults have stopped looking for one (see
/// [`RpcCall::ClaimStdoutTty`]). What a *plugin* sends is what this drops.
/// Widening the whitelist means first giving view a reader that recognises
/// DA1/OSC/DCS/APC replies on stdin and feeds them back through
/// `nvim_ui_term_event`; until that exists, forwarding a query would corrupt
/// the buffer rather than answer it. One consequence is named rather than
/// hidden: a `"+p` against a user-supplied OSC 52 provider sends a read query
/// (`OSC 52 ; c ; ?`) that view drops, so nvim times it out -- unchanged from
/// the behaviour before `stdout_tty` was claimed at all.
fn forward_ui_send(content: &str) -> Vec<Effect> {
    osc52_writes(content)
        .map(|escape| Effect::TermWrite {
            bytes: escape.as_bytes().to_vec(),
        })
        .collect()
}

const OSC52_PREFIX: &str = "\x1b]52;";

/// Every complete `OSC 52 ; {selection} ; {base64}` clipboard-write escape in
/// `content`, in the order they appear.
///
/// The scan covers `content` as a whole rather than testing its prefix,
/// because one `nvim_ui_send` payload may hold several concatenated sequences
/// (nvim's own startup probe writes `OSC 11` and a DSR in one call): a write
/// that is merely not first still has to be found, and a second write behind
/// the first still has to be forwarded.
fn osc52_writes(content: &str) -> impl Iterator<Item = &str> {
    content
        .match_indices(OSC52_PREFIX)
        .filter_map(|(start, _)| osc52_write_at(content, start))
}

/// The clipboard-write escape beginning at `start`, or `None` when what
/// begins there is anything else.
///
/// Both fields are charset-checked, and that check is the trust boundary
/// [`forward_ui_send`] describes rather than a tidiness pass. A frame is only
/// as inert as its contents: `OSC 52 ; c ; aGk=<ESC>[5n BEL` is a well-formed
/// write by its delimiters alone, but a terminal abandons the OSC string at
/// the bare `ESC` and executes the `DSR` hiding behind it, answering onto a
/// stdin that view reads as typed keys. Rejecting anything outside base64
/// (and outside the selection alphabet) leaves no room to hide a query --
/// including the bare `?` of a read query, which is a payload this must
/// refuse whatever else changes here.
fn osc52_write_at(content: &str, start: usize) -> Option<&str> {
    // `ST` per nvim's own provider; `BEL` is the other terminator OSC
    // sequences are written with, and costs one more `find` to accept
    const TERMINATORS: [&str; 2] = ["\x1b\\", "\x07"];
    const SELECTION_ALPHABET: &[u8] = b"cpqs01234567*+";
    let body = &content[start + OSC52_PREFIX.len()..];
    let (end, terminator) = TERMINATORS
        .iter()
        .filter_map(|t| body.find(t).map(|at| (at, *t)))
        .min()?;
    let (selection, payload) = body[..end].split_once(';')?;
    if !selection.bytes().all(|b| SELECTION_ALPHABET.contains(&b)) {
        return None;
    }
    if !payload
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
    {
        return None;
    }
    Some(&content[start..start + OSC52_PREFIX.len() + end + terminator.len()])
}
