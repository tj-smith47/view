//! The pure state transition: `Msg` in, `Model` mutated, `Effect`s out.

use crate::events::{clamp_dim, saturate_u16, UiEvent};
use crate::grid::GridOp;
use crate::hl::HlAttr;
use crate::model::{CmdlineState, Focus, MessageEntry, Model, PopupmenuState, TablineState};
use crate::msg::{Effect, EngineRequest, Key, MouseInput, Msg, ReplyValue, RpcCall};

/// Applies one message to `model`, returning the effects the executor must
/// carry out. Never blocks and never performs I/O: every side effect crosses
/// the boundary as a returned [`Effect`] instead of being performed here.
#[must_use]
pub fn update(model: &mut Model, msg: Msg) -> Vec<Effect> {
    match msg {
        Msg::Key(Key { notation }) => match model.focus {
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
        },
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
        Msg::RedrawReady | Msg::EngineStopped | Msg::EngineReady => Vec::new(),
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
            model.term_width = width;
            model.term_height = height;
            let (grid_width, grid_height) = model.grid_target();
            vec![Effect::Rpc(RpcCall::TryResize {
                width: grid_width,
                height: grid_height,
            })]
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
            model.engine.grid.apply(GridOp::Resize {
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
            model.engine.grid.apply(GridOp::PutLine {
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
            model.engine.grid.apply(GridOp::CursorGoto {
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
            model.engine.grid.apply(GridOp::Scroll {
                top: saturate_u16(top),
                bot: saturate_u16(bot),
                left: saturate_u16(left),
                right: saturate_u16(right),
                rows: i32::try_from(rows).unwrap_or(if rows > 0 { i32::MAX } else { i32::MIN }),
            });
            Vec::new()
        }
        UiEvent::GridClear { .. } => {
            model.engine.grid.apply(GridOp::Clear);
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
            model.engine.hl.attrs.insert(
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
            model.engine.hl.default_fg = fg;
            model.engine.hl.default_bg = bg;
            Vec::new()
        }
        UiEvent::HlGroupSet { name, hl_id } => {
            model.engine.hl.groups.insert(name, hl_id);
            Vec::new()
        }
        UiEvent::Flush => {
            model.dirty = true;
            // idempotent past the first Flush: see Model::content_painted's
            // doc comment for why this never resets
            model.content_painted = true;
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
            model
                .engine
                .messages
                .push(MessageEntry { kind, content }, replace_last);
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
        assert_eq!(m.engine.grid.row_text(0).trim_end(), "h");
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
        assert_eq!(m.engine.hl.groups.get("StatusLine"), Some(&41));
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
        assert_eq!(m.engine.hl.groups.get("StatusLine"), Some(&168));
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
        assert!(update(&mut m, Msg::EngineStopped).is_empty());
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
}
