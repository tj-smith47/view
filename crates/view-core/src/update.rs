//! The pure state transition: `Msg` in, `Model` mutated, `Effect`s out.

use crate::events::{clamp_dim, saturate_u16, UiEvent};
use crate::grid::GridOp;
use crate::hl::HlAttr;
use crate::model::{CmdlineState, Focus, Model, PopupmenuState, TablineState};
use crate::msg::{Effect, EngineRequest, Key, MouseInput, Msg, ReplyValue, RpcCall};

/// Applies one message to `model`, returning the effects the executor must
/// carry out. Never blocks and never performs I/O: every side effect crosses
/// the boundary as a returned [`Effect`] instead of being performed here.
#[must_use]
pub fn update(model: &mut Model, msg: Msg) -> Vec<Effect> {
    match msg {
        Msg::Key(Key { notation }) => {
            // any keypress is "the user is reading again": gives a
            // transient (info-kind) toast a readable duration bounded by
            // real activity instead of a wall-clock timer the runtime
            // never delivers to `update`; runs regardless of focus, since
            // the semantic is user activity, not specifically engine input
            let cmdline_open = model.engine.cmdline.is_some();
            if model
                .engine
                .messages
                .dismiss_transient_on_keypress(cmdline_open)
            {
                model.dirty = true;
            }
            match model.focus {
                Focus::Engine => vec![Effect::Rpc(RpcCall::Input { notation })],
                // no native overlay currently claims focus: every key is
                // consumed here rather than dispatched to an overlay update
                // arm, except <Esc> which always returns focus to Engine. The
                // routing seam exists so overlays can take focus without
                // touching this key path.
                Focus::Native(_) => {
                    if notation == "<Esc>" {
                        model.focus = Focus::Engine;
                    }
                    Vec::new()
                }
            }
        }
        Msg::Paste(text) => match model.focus {
            // never replayed as nvim_input keystrokes: one undo unit, no
            // mapping interference, matching nvim_paste's own contract
            Focus::Engine => vec![Effect::Rpc(RpcCall::Paste { text })],
            Focus::Native(_) => Vec::new(),
        },
        Msg::Mouse(input) => match model.focus {
            Focus::Engine => mouse_effect(model, input),
            Focus::Native(_) => Vec::new(),
        },
        Msg::Redraw(events) => {
            let mut effects = Vec::new();
            for ev in events {
                effects.extend(apply_ui_event(model, ev));
            }
            effects
        }
        // loop plumbing tokens: the loop resolves these into Redraw/EngineDown
        // before update() ever sees them, so both arms are no-ops here;
        // EngineReady is consumed even earlier, by startup's pre-attach
        // draining loop, before the steady-state loop this match belongs to
        // ever starts, so this arm is unreachable in practice but kept for
        // the same defensive-totality reason
        Msg::RedrawReady | Msg::EngineStopped(_) | Msg::EngineReady => Vec::new(),
        Msg::EngineDown(exit) => {
            model.running = false;
            vec![Effect::Quit {
                exit_code: exit.code.unwrap_or(1),
            }]
        }
        Msg::EngineRequest(EngineRequest::VimEnter { token }) => vec![Effect::Reply {
            token,
            value: ReplyValue::Nil,
        }],
        Msg::Resized { width, height } => {
            // an already-applied size is a no-op, not a repeat: the
            // frontend may fold a resize in ahead of this message to keep
            // the next frame's paint area current, and re-running the arm
            // would then dirty the model and re-issue TryResize for a
            // change that already happened
            if (model.term_width, model.term_height) == (width, height) {
                return Vec::new();
            }
            model.term_width = width;
            model.term_height = height;
            // the paint area is sourced from these fields, so the frame that
            // renders them is this frontend's own concern: `grid_target`
            // clamps, so a resize that leaves the grid unchanged draws no
            // engine redraw at all and would otherwise never repaint
            model.dirty = true;
            let (grid_width, grid_height) = model.grid_target();
            vec![Effect::Rpc(RpcCall::TryResize {
                width: grid_width,
                height: grid_height,
            })]
        }
        Msg::HlProbeReply { generation, fg, bg } => {
            // guards the write, not just the read: without this, a
            // reordered stale reply (an older generation's probe answered
            // after a newer one) would overwrite the newer reply's already-
            // correct confirmed state, permanently losing it since only one
            // slot is kept -- see HlTable::confirmed's doc comment
            if generation == model.engine.hl().probe_generation() {
                model
                    .engine
                    .confirm_hl_defaults(crate::hl::ProbedDefaults { generation, fg, bg });
                // the paint loop's `if model.dirty` gate is the only thing
                // that triggers a repaint; without this, a probe reply that
                // arrives after the frame it corrects has already painted
                // (the paint loop never awaits RPC, so this is the common
                // case) would sit applied-but-unpainted until some other,
                // unrelated event happens to mark dirty next
                model.dirty = true;
            }
            Vec::new()
        }
    }
}

/// Maps one terminal mouse event to an `RpcCall::InputMouse` effect,
/// translating `input.row` from raw terminal cell coordinates into engine
/// grid coordinates by subtracting [`Model::chrome_rows`]. A row that lands
/// inside the reserved chrome (the tabline) belongs to that chrome, not the
/// grid; no native chrome click handling exists yet, so such a click is
/// dropped rather than forwarded at a wrapped-around row.
fn mouse_effect(model: &Model, input: MouseInput) -> Vec<Effect> {
    let chrome = model.chrome_rows();
    if input.row < chrome {
        return Vec::new();
    }
    vec![Effect::Rpc(RpcCall::InputMouse {
        button: input.button,
        action: input.action,
        modifier: input.modifier,
        row: input.row - chrome,
        col: input.col,
    })]
}

/// Applies one decoded redraw sub-event to `model`, returning any effects
/// it produces. Only [`UiEvent::TablineUpdate`] can produce one: crossing
/// the 1-tab chrome-reservation boundary (either direction) changes the
/// grid target size, which the loop's executor must forward to the engine
/// as a `TryResize` the same way a terminal resize does.
fn apply_ui_event(model: &mut Model, ev: UiEvent) -> Vec<Effect> {
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
            model.engine.cmdline = Some(CmdlineState {
                content,
                pos,
                firstc,
                prompt,
                indent,
                level,
            });
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
            model.engine.messages.push(kind, content, replace_last);
            Vec::new()
        }
        UiEvent::MsgClear => {
            model.engine.messages.clear();
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
        UiEvent::Unknown { .. } => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::events::UiEvent;
    use crate::model::OverlayId;
    use crate::msg::{ExitInfo, ReplyToken};

    fn model() -> Model {
        Model::new()
    }

    #[test]
    fn redraw_batch_applies_to_grid_and_sets_dirty_only_on_flush() {
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::Redraw(vec![
                UiEvent::GridResize {
                    grid: 1,
                    width: 10,
                    height: 3,
                },
                UiEvent::GridLine {
                    grid: 1,
                    row: 0,
                    col_start: 0,
                    cells: vec![crate::events::GridCell {
                        text: "h".into(),
                        hl_id: 0,
                        repeat: 1,
                    }],
                },
            ]),
        );
        assert!(effects.is_empty());
        assert!(!m.dirty, "no Flush yet: must not request paint");
        let effects = update(&mut m, Msg::Redraw(vec![UiEvent::Flush]));
        assert!(effects.is_empty());
        assert!(m.dirty);
        assert_eq!(m.engine.grid().row_text(0).trim_end(), "h");
    }

    #[test]
    fn hl_group_set_records_the_name_to_hl_id_mapping() {
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::HlGroupSet {
                name: "StatusLine".to_string(),
                hl_id: 41,
            }]),
        );
        assert!(effects.is_empty());
        assert_eq!(m.engine.hl().group("StatusLine"), Some(41));
    }

    #[test]
    fn hl_group_set_overwrites_a_prior_mapping_for_the_same_name() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::HlGroupSet {
                name: "StatusLine".to_string(),
                hl_id: 1,
            }]),
        );
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::HlGroupSet {
                name: "StatusLine".to_string(),
                hl_id: 168,
            }]),
        );
        assert_eq!(m.engine.hl().group("StatusLine"), Some(168));
    }

    /// The `hl_attr_define` counterpart to the mapping test above: a
    /// redefinition that changes exactly one attribute must land, for every
    /// attribute a resolved style reads.
    ///
    /// `HlTable::define_attr` drops a redefinition it judges identical to
    /// what it already holds, so any field missing from `HlAttr`'s equality
    /// turns that field's redefinition into a silent no-op: the stored
    /// attributes keep the old value, every cell holding the id keeps
    /// painting the old style, and nothing anywhere reports it. A state
    /// assertion rather than a clipped-versus-full paint comparison,
    /// because those composite both sides from one model and so read the
    /// same dropped mutation twice, agreeing with each other about the
    /// wrong screen.
    #[test]
    fn an_hl_attr_redefinition_lands_for_every_field_a_resolved_style_reads() {
        type AttrProbe = fn(&HlAttr) -> bool;
        type StyleProbe = fn(&crate::theme::ResolvedStyle) -> bool;
        const ID: u64 = 7;
        let base = UiEvent::HlAttrDefine {
            id: ID,
            fg: Some(0x111111),
            bg: Some(0x222222),
            bold: false,
            italic: false,
            underline: false,
            reverse: false,
        };
        // each case redefines the baseline with exactly one field moved, so
        // a field dropped from the equality can only be caught by its own
        // case and never masked by another field changing alongside it
        let cases: [(&str, UiEvent, AttrProbe, StyleProbe); 6] = [
            (
                "fg",
                UiEvent::HlAttrDefine {
                    id: ID,
                    fg: Some(0x999999),
                    bg: Some(0x222222),
                    bold: false,
                    italic: false,
                    underline: false,
                    reverse: false,
                },
                |a| a.fg == Some(0x999999),
                |s| s.fg == Some(0x999999),
            ),
            (
                "bg",
                UiEvent::HlAttrDefine {
                    id: ID,
                    fg: Some(0x111111),
                    bg: Some(0x888888),
                    bold: false,
                    italic: false,
                    underline: false,
                    reverse: false,
                },
                |a| a.bg == Some(0x888888),
                |s| s.bg == Some(0x888888),
            ),
            (
                "bold",
                UiEvent::HlAttrDefine {
                    id: ID,
                    fg: Some(0x111111),
                    bg: Some(0x222222),
                    bold: true,
                    italic: false,
                    underline: false,
                    reverse: false,
                },
                |a| a.bold,
                |s| s.bold,
            ),
            (
                "italic",
                UiEvent::HlAttrDefine {
                    id: ID,
                    fg: Some(0x111111),
                    bg: Some(0x222222),
                    bold: false,
                    italic: true,
                    underline: false,
                    reverse: false,
                },
                |a| a.italic,
                |s| s.italic,
            ),
            (
                "underline",
                UiEvent::HlAttrDefine {
                    id: ID,
                    fg: Some(0x111111),
                    bg: Some(0x222222),
                    bold: false,
                    italic: false,
                    underline: true,
                    reverse: false,
                },
                |a| a.underline,
                |s| s.underline,
            ),
            (
                "reverse",
                UiEvent::HlAttrDefine {
                    id: ID,
                    fg: Some(0x111111),
                    bg: Some(0x222222),
                    bold: false,
                    italic: false,
                    underline: false,
                    reverse: true,
                },
                |a| a.reverse,
                |s| s.reverse,
            ),
        ];

        for (field, redefine, attr_probe, style_probe) in cases {
            let mut m = model();
            let _ = update(&mut m, Msg::Redraw(vec![base.clone()]));
            let style_before =
                crate::theme::Theme::from_hl(m.engine.hl()).style_for(ID, m.engine.hl());
            // the baseline definition is itself a change, so its damage is
            // drained before the redefinition under test runs
            let _ = m.take_paint_damage();

            let _ = update(&mut m, Msg::Redraw(vec![redefine]));

            let stored = m.engine.hl().attr(ID).expect("the id stays defined");
            assert!(
                attr_probe(&stored),
                "{field}: the redefinition never reached the stored attributes ({stored:?})"
            );
            let style_after =
                crate::theme::Theme::from_hl(m.engine.hl()).style_for(ID, m.engine.hl());
            assert!(
                style_probe(&style_after),
                "{field}: the redefinition never reached the resolved style ({style_after:?})"
            );
            assert_ne!(
                style_before, style_after,
                "{field}: every field here changes the style cells paint with"
            );
            assert!(
                m.take_paint_damage().full,
                "{field}: a landed redefinition restyles every cell holding the id"
            );
        }
    }

    #[test]
    fn key_in_engine_focus_becomes_rpc_input_effect() {
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::Key(Key {
                notation: "<C-x>".into(),
            }),
        );
        assert!(matches!(
            &effects[..],
            [Effect::Rpc(RpcCall::Input { notation })] if notation == "<C-x>"
        ));
    }

    #[test]
    fn a_keypress_dismisses_an_already_flushed_transient_toast_and_marks_dirty() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![
                UiEvent::MsgShow {
                    kind: "echomsg".into(),
                    content: vec![(0, "info".into())],
                    replace_last: false,
                },
                UiEvent::Flush,
            ]),
        );
        assert_eq!(m.engine.messages.entries.len(), 1);
        m.dirty = false;

        let _ = update(
            &mut m,
            Msg::Key(Key {
                notation: "l".into(),
            }),
        );
        assert!(
            m.engine.messages.entries.is_empty(),
            "a transient toast that already survived one Flush must be dismissed on the next keypress"
        );
        assert!(
            m.dirty,
            "dismissing a visible toast must mark the model dirty for a repaint"
        );
    }

    #[test]
    fn a_keypress_never_dismisses_a_persistent_toast() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![
                UiEvent::MsgShow {
                    kind: "echoerr".into(),
                    content: vec![(0, "boom".into())],
                    replace_last: false,
                },
                UiEvent::Flush,
            ]),
        );

        let _ = update(
            &mut m,
            Msg::Key(Key {
                notation: "l".into(),
            }),
        );
        assert_eq!(
            m.engine.messages.entries.len(),
            1,
            "an error/warn-kind toast must persist across a keypress"
        );
    }

    #[test]
    fn a_keypress_does_not_dismiss_a_transient_toast_shown_in_the_same_batch_pre_flush() {
        // no Flush yet: the toast has not necessarily been painted even
        // once, so it must survive this keypress and only be dismissed on
        // the one after -- guarantees at least one visible frame
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::MsgShow {
                kind: "echomsg".into(),
                content: vec![(0, "info".into())],
                replace_last: false,
            }]),
        );

        let _ = update(
            &mut m,
            Msg::Key(Key {
                notation: "l".into(),
            }),
        );
        assert_eq!(m.engine.messages.entries.len(), 1);
    }

    // routing table: focus x input-kind -> effect. Pins the seam native
    // overlays build on, using a test-only Focus::Native(OverlayId)
    // placeholder in the absence of a real overlay implementation.

    #[test]
    fn paste_in_engine_focus_becomes_rpc_paste_effect() {
        let mut m = model();
        let effects = update(&mut m, Msg::Paste("hello\nworld".into()));
        assert!(matches!(
            &effects[..],
            [Effect::Rpc(RpcCall::Paste { text })] if text == "hello\nworld"
        ));
    }

    #[test]
    fn mouse_in_engine_focus_becomes_rpc_input_mouse_effect() {
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::Mouse(MouseInput {
                button: "left".into(),
                action: "press".into(),
                modifier: String::new(),
                row: 5,
                col: 10,
            }),
        );
        assert!(matches!(
            &effects[..],
            [Effect::Rpc(RpcCall::InputMouse {
                button,
                action,
                modifier,
                row: 5,
                col: 10,
            })] if button == "left" && action == "press" && modifier.is_empty()
        ));
    }

    #[test]
    fn mouse_row_is_offset_by_reserved_chrome_rows_before_reaching_the_engine() {
        use crate::events::{TabEntry, TabHandle};
        let mut m = model();
        // opening a second tab reserves one chrome row for the tabline
        // (Model::chrome_rows), so a click on terminal row 3 must land on
        // grid row 2, not grid row 3.
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::TablineUpdate {
                current: TabHandle(1),
                tabs: vec![
                    TabEntry {
                        tab: TabHandle(1),
                        name: "a".into(),
                    },
                    TabEntry {
                        tab: TabHandle(2),
                        name: "b".into(),
                    },
                ],
            }]),
        );
        let effects = update(
            &mut m,
            Msg::Mouse(MouseInput {
                button: "left".into(),
                action: "press".into(),
                modifier: String::new(),
                row: 3,
                col: 0,
            }),
        );
        assert!(matches!(
            &effects[..],
            [Effect::Rpc(RpcCall::InputMouse { row: 2, .. })]
        ));
    }

    #[test]
    fn mouse_click_on_a_reserved_chrome_row_is_dropped_not_forwarded() {
        use crate::events::{TabEntry, TabHandle};
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::TablineUpdate {
                current: TabHandle(1),
                tabs: vec![
                    TabEntry {
                        tab: TabHandle(1),
                        name: "a".into(),
                    },
                    TabEntry {
                        tab: TabHandle(2),
                        name: "b".into(),
                    },
                ],
            }]),
        );
        let effects = update(
            &mut m,
            Msg::Mouse(MouseInput {
                button: "left".into(),
                action: "press".into(),
                modifier: String::new(),
                row: 0,
                col: 0,
            }),
        );
        assert!(
            effects.is_empty(),
            "click on the tabline row must not reach the engine grid"
        );
    }

    #[test]
    fn key_in_native_focus_is_consumed_and_esc_returns_engine_focus() {
        let mut m = model();
        m.focus = Focus::Native(OverlayId(1));
        let effects = update(
            &mut m,
            Msg::Key(Key {
                notation: "x".into(),
            }),
        );
        assert!(
            effects.is_empty(),
            "native focus consumes keys, never forwards to the engine"
        );
        assert!(
            matches!(m.focus, Focus::Native(_)),
            "a non-Esc key must not change focus"
        );

        let effects = update(
            &mut m,
            Msg::Key(Key {
                notation: "<Esc>".into(),
            }),
        );
        assert!(
            effects.is_empty(),
            "Esc returns focus without forwarding to the engine"
        );
        assert!(
            matches!(m.focus, Focus::Engine),
            "Esc must return focus to Engine"
        );
    }

    #[test]
    fn paste_and_mouse_in_native_focus_are_consumed_not_forwarded() {
        let mut m = model();
        m.focus = Focus::Native(OverlayId(1));
        assert!(update(&mut m, Msg::Paste("x".into())).is_empty());
        assert!(update(
            &mut m,
            Msg::Mouse(MouseInput {
                button: "left".into(),
                action: "press".into(),
                modifier: String::new(),
                row: 0,
                col: 0,
            })
        )
        .is_empty());
    }

    #[test]
    fn mouse_on_off_events_set_the_model_flag_the_terminal_reads_for_capture() {
        let mut m = model();
        assert!(!m.engine.mouse_on, "mouse capture must default off");
        let _ = update(&mut m, Msg::Redraw(vec![UiEvent::MouseOn]));
        assert!(m.engine.mouse_on);
        let _ = update(&mut m, Msg::Redraw(vec![UiEvent::MouseOff]));
        assert!(!m.engine.mouse_on);
    }

    #[test]
    fn engine_down_maps_signal_and_code_to_exit_effects() {
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::EngineDown(ExitInfo {
                code: Some(5),
                by_signal: false,
            }),
        );
        assert!(matches!(&effects[..], [Effect::Quit { exit_code: 5 }]));
        assert!(!m.running);
    }

    #[test]
    fn engine_down_without_code_exits_one_and_signal_code_passes_through() {
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::EngineDown(ExitInfo {
                code: None,
                by_signal: false,
            }),
        );
        assert!(matches!(&effects[..], [Effect::Quit { exit_code: 1 }]));
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::EngineDown(ExitInfo {
                code: Some(137),
                by_signal: true,
            }),
        );
        assert!(matches!(&effects[..], [Effect::Quit { exit_code: 137 }]));
    }

    #[test]
    fn loop_tokens_are_noops_and_engine_request_always_replies() {
        let mut m = model();
        assert!(update(&mut m, Msg::RedrawReady).is_empty());
        assert!(update(&mut m, Msg::EngineStopped(None)).is_empty());
        let effects = update(
            &mut m,
            Msg::EngineRequest(EngineRequest::VimEnter {
                token: ReplyToken { msgid: 9 },
            }),
        );
        assert!(matches!(
            &effects[..],
            [Effect::Reply {
                token: ReplyToken { msgid: 9 },
                value: ReplyValue::Nil
            }]
        ));
    }

    #[test]
    fn resize_marks_the_model_dirty_so_the_new_area_is_actually_painted() {
        let mut m = model();
        m.dirty = false;
        let _ = update(
            &mut m,
            Msg::Resized {
                width: 120,
                height: 40,
            },
        );
        assert!(
            m.dirty,
            "a resize changes the paint area, so the frontend must repaint on its own \
             rather than waiting for an engine redraw that a clamped grid_target may never produce"
        );
    }

    #[test]
    fn resize_produces_try_resize_effect() {
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::Resized {
                width: 120,
                height: 40,
            },
        );
        assert!(matches!(
            &effects[..],
            [Effect::Rpc(RpcCall::TryResize {
                width: 120,
                height: 40
            })]
        ));
    }

    // fixtures below are shaped from crates/view-engine/src/ui_events.rs's
    // decode test fixtures, which are themselves copied from the live
    // capture at ~/.claude/tmp/capture-*.log (see that module's tests for
    // the raw wire fixtures); these tests exercise state application only, not
    // decoding.

    #[test]
    fn mode_events_update_state_without_dirty() {
        use crate::events::ModeInfo;
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::Redraw(vec![
                UiEvent::ModeInfoSet {
                    cursor_style_enabled: true,
                    modes: vec![
                        ModeInfo {
                            name: "normal".into(),
                            cursor_shape: "block".into(),
                            ..ModeInfo::default()
                        },
                        ModeInfo {
                            name: "insert".into(),
                            cursor_shape: "vertical".into(),
                            cell_percentage: 25,
                            ..ModeInfo::default()
                        },
                    ],
                },
                UiEvent::ModeChange {
                    mode: "insert".into(),
                    mode_idx: 1,
                },
            ]),
        );
        assert!(effects.is_empty());
        assert!(!m.dirty, "mode events alone must not request paint");
        assert!(m.engine.mode.cursor_style_enabled);
        assert_eq!(m.engine.mode.current, "insert");
        assert_eq!(
            m.engine
                .mode
                .active_cursor()
                .map(|c| c.cursor_shape.as_str()),
            Some("vertical")
        );
    }

    #[test]
    fn mode_state_active_cursor_is_none_before_mode_info_set() {
        let m = model();
        assert!(m.engine.mode.active_cursor().is_none());
    }

    #[test]
    fn cmdline_show_pos_hide_set_and_clear_state() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::CmdlineShow {
                content: vec![(0, "q".into())],
                pos: 1,
                firstc: ":".into(),
                prompt: "".into(),
                indent: 0,
                level: 1,
            }]),
        );
        assert!(!m.dirty);
        let cmdline = m.engine.cmdline.as_ref().expect("cmdline must be set");
        assert_eq!(cmdline.content, vec![(0, "q".to_string())]);
        assert_eq!(cmdline.firstc, ":");

        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::CmdlinePos { pos: 2, level: 1 }]),
        );
        assert_eq!(m.engine.cmdline.as_ref().unwrap().pos, 2);

        let _ = update(&mut m, Msg::Redraw(vec![UiEvent::CmdlineHide]));
        assert!(m.engine.cmdline.is_none());
    }

    #[test]
    fn a_confirm_questions_toast_lives_exactly_as_long_as_the_prompt_does() {
        // the event order is nvim 0.12.4's own, observed over a real RPC
        // session driving `confirm('Save changes?', "&Yes\n&No")`: the
        // question arrives once as msg_show, the answer line separately as
        // cmdline_show, and a key answering none of the choices re-arms the
        // prompt with cmdline_hide + cmdline_show and NO second msg_show.
        let mut m = model();
        let prompt = || UiEvent::CmdlineShow {
            content: vec![],
            pos: 0,
            firstc: String::new(),
            prompt: "[Y]es, (N)o: ".into(),
            indent: 0,
            level: 1,
        };
        let question = || UiEvent::MsgShow {
            kind: "confirm".into(),
            content: vec![(0, "Save changes?".into())],
            replace_last: false,
        };
        let shown = |m: &Model| m.engine.messages.visible_lines(4);

        let _ = update(&mut m, Msg::Redraw(vec![question(), UiEvent::Flush]));
        let _ = update(&mut m, Msg::Redraw(vec![prompt(), UiEvent::Flush]));
        assert_eq!(shown(&m), vec!["Save changes?"]);

        let _ = update(
            &mut m,
            Msg::Key(Key {
                notation: "q".into(),
            }),
        );
        assert_eq!(
            shown(&m),
            vec!["Save changes?"],
            "a key the prompt refuses must not take the question away with it"
        );

        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::CmdlineHide, UiEvent::Flush]),
        );
        let _ = update(&mut m, Msg::Redraw(vec![prompt(), UiEvent::Flush]));
        let _ = update(
            &mut m,
            Msg::Key(Key {
                notation: "y".into(),
            }),
        );
        assert_eq!(shown(&m), vec!["Save changes?"]);

        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::CmdlineHide, UiEvent::Flush]),
        );
        let _ = update(
            &mut m,
            Msg::Key(Key {
                notation: "j".into(),
            }),
        );
        assert!(
            shown(&m).is_empty(),
            "with the prompt closed the question is ordinary transient text"
        );
    }

    #[test]
    fn cmdline_pos_without_prior_show_is_a_noop() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::CmdlinePos { pos: 3, level: 1 }]),
        );
        assert!(m.engine.cmdline.is_none());
    }

    #[test]
    fn msg_show_appends_and_replace_last_overwrites() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::MsgShow {
                kind: "echomsg".into(),
                content: vec![(73, "first".into())],
                replace_last: false,
            }]),
        );
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::MsgShow {
                kind: "echomsg".into(),
                content: vec![(73, "second".into())],
                replace_last: false,
            }]),
        );
        assert_eq!(m.engine.messages.entries.len(), 2);

        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::MsgShow {
                kind: "echomsg".into(),
                content: vec![(73, "replaced".into())],
                replace_last: true,
            }]),
        );
        assert_eq!(m.engine.messages.entries.len(), 2);
        assert_eq!(
            m.engine.messages.entries.last().unwrap().content,
            vec![(73, "replaced".to_string())]
        );

        let _ = update(&mut m, Msg::Redraw(vec![UiEvent::MsgClear]));
        assert!(m.engine.messages.entries.is_empty());
    }

    #[test]
    fn tabline_update_sets_current_and_tabs() {
        use crate::events::{TabEntry, TabHandle};
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::TablineUpdate {
                current: TabHandle(2),
                tabs: vec![
                    TabEntry {
                        tab: TabHandle(1),
                        name: "[No Name]".into(),
                    },
                    TabEntry {
                        tab: TabHandle(2),
                        name: "[No Name]".into(),
                    },
                ],
            }]),
        );
        assert!(!m.dirty);
        let tabline = m.engine.tabline.as_ref().expect("tabline must be set");
        assert_eq!(tabline.current, TabHandle(2));
        assert_eq!(tabline.tabs.len(), 2);
    }

    #[test]
    fn popupmenu_show_select_hide_set_and_clear_state() {
        use crate::events::PmItem;
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::PopupmenuShow {
                items: vec![PmItem {
                    word: "foobar".into(),
                    ..PmItem::default()
                }],
                selected: 0,
                row: 0,
                col: 11,
                grid: 1,
            }]),
        );
        assert_eq!(m.engine.popupmenu.as_ref().unwrap().selected, 0);

        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::PopupmenuSelect { selected: 1 }]),
        );
        assert_eq!(m.engine.popupmenu.as_ref().unwrap().selected, 1);

        let _ = update(&mut m, Msg::Redraw(vec![UiEvent::PopupmenuHide]));
        assert!(m.engine.popupmenu.is_none());
    }

    #[test]
    fn popupmenu_select_without_prior_show_is_a_noop() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::PopupmenuSelect { selected: 0 }]),
        );
        assert!(m.engine.popupmenu.is_none());
    }

    #[test]
    fn resize_target_shrinks_by_chrome_rows_once_more_than_one_tab_is_open() {
        use crate::events::{TabEntry, TabHandle};
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::TablineUpdate {
                current: TabHandle(1),
                tabs: vec![
                    TabEntry {
                        tab: TabHandle(1),
                        name: "a".into(),
                    },
                    TabEntry {
                        tab: TabHandle(2),
                        name: "b".into(),
                    },
                ],
            }]),
        );
        let effects = update(
            &mut m,
            Msg::Resized {
                width: 80,
                height: 24,
            },
        );
        assert!(matches!(
            &effects[..],
            [Effect::Rpc(RpcCall::TryResize {
                width: 80,
                height: 23
            })]
        ));
    }

    #[test]
    fn tabline_crossing_the_one_tab_boundary_round_trips_the_reserved_row() {
        use crate::events::{TabEntry, TabHandle};
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Resized {
                width: 80,
                height: 24,
            },
        );

        let opened = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::TablineUpdate {
                current: TabHandle(1),
                tabs: vec![
                    TabEntry {
                        tab: TabHandle(1),
                        name: "a".into(),
                    },
                    TabEntry {
                        tab: TabHandle(2),
                        name: "b".into(),
                    },
                ],
            }]),
        );
        assert!(
            matches!(
                &opened[..],
                [Effect::Rpc(RpcCall::TryResize {
                    width: 80,
                    height: 23
                })]
            ),
            "opening a second tab must reserve the tabline row: {opened:?}"
        );

        let closed = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::TablineUpdate {
                current: TabHandle(1),
                tabs: vec![TabEntry {
                    tab: TabHandle(1),
                    name: "a".into(),
                }],
            }]),
        );
        assert!(
            matches!(
                &closed[..],
                [Effect::Rpc(RpcCall::TryResize {
                    width: 80,
                    height: 24
                })]
            ),
            "closing back to one tab must release the reserved row: {closed:?}"
        );
    }

    #[test]
    fn tabline_update_within_the_same_tab_count_bucket_emits_no_resize() {
        use crate::events::{TabEntry, TabHandle};
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::TablineUpdate {
                current: TabHandle(1),
                tabs: vec![
                    TabEntry {
                        tab: TabHandle(1),
                        name: "a".into(),
                    },
                    TabEntry {
                        tab: TabHandle(2),
                        name: "b".into(),
                    },
                ],
            }]),
        );
        // renaming a tab (still 2 tabs) never crosses the boundary
        let effects = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::TablineUpdate {
                current: TabHandle(1),
                tabs: vec![
                    TabEntry {
                        tab: TabHandle(1),
                        name: "a-renamed".into(),
                    },
                    TabEntry {
                        tab: TabHandle(2),
                        name: "b".into(),
                    },
                ],
            }]),
        );
        assert!(effects.is_empty());
    }

    /// A style change has no rows of its own: the highlight table sits
    /// behind every cell's resolved style, so any change to it can restyle
    /// the whole screen while no grid cell's text moves. Damage clipped to
    /// grid rows alone would repaint whatever row happened to change and
    /// leave the rest of the screen in the previous colors.
    #[test]
    fn a_highlight_change_with_no_grid_change_damages_the_whole_frame() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::GridResize {
                grid: 1,
                width: 8,
                height: 4,
            }]),
        );
        let _ = m.take_paint_damage();

        for ev in [
            UiEvent::DefaultColorsSet {
                fg: Some(0xF8F8F2),
                bg: Some(0x282A36),
                sp: None,
            },
            UiEvent::HlAttrDefine {
                id: 3,
                fg: Some(0xFF0000),
                bg: None,
                bold: false,
                italic: false,
                underline: false,
                reverse: false,
            },
            UiEvent::HlGroupSet {
                name: "StatusLine".to_string(),
                hl_id: 3,
            },
        ] {
            let _ = update(&mut m, Msg::Redraw(vec![ev.clone()]));
            assert!(
                m.take_paint_damage().full,
                "{ev:?} must damage every cell, not none of them"
            );
        }

        let generation = m.engine.hl().probe_generation();
        let _ = update(
            &mut m,
            Msg::HlProbeReply {
                generation,
                fg: Some(0xF8F8F2),
                bg: Some(0x282A36),
            },
        );
        assert!(
            m.take_paint_damage().full,
            "an accepted probe reply re-derives the theme and must damage every cell"
        );
    }

    /// The one write in the highlight table that marks changed without
    /// comparing values first, and the state that makes the difference
    /// visible: nvim resends the same default colors, so neither colour
    /// moves, but the probe generation those colours carry moves anyway and
    /// strands a `confirmed` reply that was matching an instant earlier.
    /// The derived theme changes with no value in the write itself
    /// changing, so a value comparison here would leave the whole screen
    /// painted in a theme the model no longer holds.
    ///
    /// Reaching this live needs `nvim_get_hl`'s answer to differ from the
    /// `default_colors_set` that opened the same generation, so the state
    /// is built from an explicit event sequence rather than left to a
    /// live path to happen upon.
    #[test]
    fn resending_identical_default_colors_still_damages_the_frame() {
        let mut m = model();
        let resend = UiEvent::DefaultColorsSet {
            fg: Some(0xFFFFFF),
            bg: Some(0),
            sp: None,
        };
        let _ = update(&mut m, Msg::Redraw(vec![resend.clone()]));
        let generation = m.engine.hl().probe_generation();
        // the reply disambiguates the wire-ambiguous zero and reports a fg
        // the wire never carried, so the theme it derives is distinguishable
        // from the one the raw wire values derive on their own
        let _ = update(
            &mut m,
            Msg::HlProbeReply {
                generation,
                fg: Some(0xF8F8F2),
                bg: Some(0x123456),
            },
        );
        let before = crate::theme::Theme::from_hl(m.engine.hl());
        let _ = m.take_paint_damage();

        let _ = update(&mut m, Msg::Redraw(vec![resend]));

        assert_eq!(
            (m.engine.hl().default_fg(), m.engine.hl().default_bg()),
            (Some(0xFFFFFF), Some(0)),
            "the resent colours must be identical, or this proves nothing"
        );
        let after = crate::theme::Theme::from_hl(m.engine.hl());
        assert_ne!(
            before, after,
            "the bumped generation strands the matching reply and re-derives the theme"
        );
        assert!(
            m.take_paint_damage().full,
            "a re-derived theme restyles every cell, however little the write itself changed"
        );
    }

    /// Installing a whole highlight table is a style change like any other:
    /// every painted cell resolves through the table, so the frame after
    /// the swap must repaint all of them even though no grid row moved.
    /// The installed table is drained clean first, so the only thing that
    /// can supply the damage is the install itself.
    #[test]
    fn installing_a_whole_highlight_table_damages_the_frame() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::GridResize {
                grid: 1,
                width: 8,
                height: 4,
            }]),
        );
        let _ = m.take_paint_damage();

        let mut replacement = crate::hl::HlTable::new();
        replacement.define_attr(
            3,
            HlAttr {
                fg: Some(0x00FF00),
                bg: None,
                bold: false,
                italic: false,
                underline: false,
                reverse: false,
            },
        );
        assert!(
            replacement.take_dirty(),
            "the replacement must arrive carrying no damage of its own"
        );

        m.engine.replace_hl(replacement);

        assert_eq!(
            m.engine.hl().attr(3).map(|a| a.fg),
            Some(Some(0x00FF00)),
            "the replacement must actually be installed"
        );
        assert!(
            m.take_paint_damage().full,
            "a swapped table restyles every cell on screen at once"
        );
    }

    /// A first-ever definition of an `hl_id` damages the whole frame the
    /// same way a redefinition does, and the pessimism is deliberate: the
    /// highlight table holds no record of which ids the painted cells
    /// reference, so it cannot tell a definition that restyles nothing from
    /// one that restyles cells already on screen resolving that id through
    /// the `Normal` fallback. Skipping the mark for an id the table has not
    /// seen would trade one whole-frame composite for a silently stale
    /// screen wherever nvim's own define-before-use ordering does not hold,
    /// which the table cannot check, against a damage contract that
    /// over-reports rather than under-reports.
    #[test]
    fn a_first_definition_of_a_previously_unknown_hl_id_still_damages_the_frame() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::GridResize {
                grid: 1,
                width: 8,
                height: 4,
            }]),
        );
        let _ = m.take_paint_damage();
        assert!(
            m.engine.hl().attr(12).is_none(),
            "the id must be one the table has never held"
        );

        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::HlAttrDefine {
                id: 12,
                fg: Some(0x00FF00),
                bg: None,
                bold: false,
                italic: false,
                underline: false,
                reverse: false,
            }]),
        );

        assert!(
            m.take_paint_damage().full,
            "a first definition may still restyle cells already holding the id"
        );
    }

    /// The other direction: a definition that changes nothing must not cost
    /// a repaint. The value comparison this pins is defensive rather than
    /// load-bearing -- the pinned engine was measured never to resend an
    /// unchanged definition (see [`HlTable::define_attr`]) -- but an engine
    /// that did would turn every resend into a whole-frame repaint, giving
    /// back the frames damage clipping exists to save.
    #[test]
    fn a_highlight_definition_that_changes_nothing_produces_no_damage() {
        let mut m = model();
        let define = UiEvent::HlAttrDefine {
            id: 3,
            fg: Some(0xFF0000),
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            reverse: false,
        };
        let group = UiEvent::HlGroupSet {
            name: "StatusLine".to_string(),
            hl_id: 3,
        };
        let _ = update(&mut m, Msg::Redraw(vec![define.clone(), group.clone()]));
        let _ = m.take_paint_damage();

        let _ = update(&mut m, Msg::Redraw(vec![define, group]));
        let damage = m.take_paint_damage();
        assert!(!damage.full, "an identical redefinition is not a repaint");
        assert!(damage.rows.is_empty(), "and touches no grid row either");
    }

    #[test]
    fn default_colors_set_bumps_generation_and_emits_a_matching_probe_effect() {
        let mut m = model();
        assert_eq!(m.engine.hl().probe_generation(), 0);
        let effects = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::DefaultColorsSet {
                fg: Some(0xFFFFFF),
                bg: Some(0),
                sp: None,
            }]),
        );
        assert_eq!(m.engine.hl().probe_generation(), 1);
        assert!(matches!(
            effects.as_slice(),
            [Effect::Rpc(RpcCall::GetDefaultHl { generation: 1 })]
        ));
    }

    #[test]
    fn a_second_default_colors_set_bumps_generation_again() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::DefaultColorsSet {
                fg: None,
                bg: Some(0),
                sp: None,
            }]),
        );
        let effects = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::DefaultColorsSet {
                fg: None,
                bg: Some(0x282a36),
                sp: None,
            }]),
        );
        assert_eq!(m.engine.hl().probe_generation(), 2);
        assert!(matches!(
            effects.as_slice(),
            [Effect::Rpc(RpcCall::GetDefaultHl { generation: 2 })]
        ));
    }

    #[test]
    fn hl_probe_reply_matching_the_current_generation_is_accepted_and_marks_dirty() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::DefaultColorsSet {
                fg: Some(0x111111),
                bg: Some(0),
                sp: None,
            }]),
        );
        m.dirty = false;
        let effects = update(
            &mut m,
            Msg::HlProbeReply {
                generation: 1,
                fg: Some(0x111111),
                bg: None,
            },
        );
        assert!(effects.is_empty());
        assert!(m.dirty, "an accepted probe reply must trigger a repaint");
        let confirmed = m.engine.hl().confirmed().expect("reply must be recorded");
        assert_eq!(confirmed.generation, 1);
        assert_eq!(confirmed.fg, Some(0x111111));
        assert_eq!(confirmed.bg, None);
    }

    /// The write-time half of the generation guard: a reply for a
    /// superseded generation must never overwrite a newer (or absent)
    /// confirmed value, even though nothing else has happened between the
    /// two `DefaultColorsSet`s to make the stale reply's arrival implausible
    /// on a real connection (out-of-order delivery is not assumed to be
    /// impossible, only rare).
    #[test]
    fn hl_probe_reply_for_a_stale_generation_is_dropped_without_marking_dirty() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::DefaultColorsSet {
                fg: None,
                bg: Some(0),
                sp: None,
            }]),
        );
        // a second DefaultColorsSet bumps the generation to 2 before the
        // generation-1 probe's reply ever arrives
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::DefaultColorsSet {
                fg: None,
                bg: Some(0x282a36),
                sp: None,
            }]),
        );
        m.dirty = false;
        let effects = update(
            &mut m,
            Msg::HlProbeReply {
                generation: 1,
                fg: None,
                bg: None,
            },
        );
        assert!(effects.is_empty());
        assert!(
            !m.dirty,
            "a stale-generation reply must not trigger a repaint"
        );
        assert!(
            m.engine.hl().confirmed().is_none(),
            "a stale-generation reply must not be recorded"
        );
    }

    /// A colorscheme switch mid-session: `Normal` goes from confirmed
    /// transparent to a wire-ambiguous zero that will turn out to be
    /// genuinely black, once its own probe replies. Between the switch's
    /// `DefaultColorsSet` and that reply, `Theme::from_hl` must keep
    /// reading the *old* confirmed value (still transparent) rather than
    /// painting the new, not-yet-disambiguated wire zero -- only once the
    /// matching reply lands does the theme converge on black.
    #[test]
    fn colorscheme_switch_to_ambiguous_zero_holds_the_prior_confirmed_value_until_its_own_probe_replies(
    ) {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::DefaultColorsSet {
                fg: Some(0xF8F8F2),
                bg: Some(0),
                sp: None,
            }]),
        );
        let _ = update(
            &mut m,
            Msg::HlProbeReply {
                generation: 1,
                fg: Some(0xF8F8F2),
                bg: None,
            },
        );
        assert_eq!(crate::theme::Theme::from_hl(m.engine.hl()).bg, None);

        // the colorscheme switch: a second DefaultColorsSet, still an
        // ambiguous wire zero, bumps the generation and starts a fresh probe
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::DefaultColorsSet {
                fg: Some(0xFFFFFF),
                bg: Some(0),
                sp: None,
            }]),
        );
        assert_eq!(
            crate::theme::Theme::from_hl(m.engine.hl()).bg,
            None,
            "must keep the prior confirmed transparent value while the new probe is in flight"
        );

        let _ = update(
            &mut m,
            Msg::HlProbeReply {
                generation: 2,
                fg: Some(0xFFFFFF),
                bg: Some(0),
            },
        );
        assert_eq!(
            crate::theme::Theme::from_hl(m.engine.hl()).bg,
            Some(0),
            "must converge on the new theme's genuinely-black bg once its probe replies"
        );
    }
}
