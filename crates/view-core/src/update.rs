//! The pure state transition: `Msg` in, `Model` mutated, `Effect`s out.

use crate::events::{clamp_dim, saturate_u16, UiEvent};
use crate::grid::GridOp;
use crate::hl::HlAttr;
use crate::model::{Focus, Model};
use crate::msg::{Effect, EngineRequest, Key, Msg, ReplyValue, RpcCall};

/// Applies one message to `model`, returning the effects the executor must
/// carry out. Never blocks and never performs I/O: every side effect crosses
/// the boundary as a returned [`Effect`] instead of being performed here.
#[must_use]
pub fn update(model: &mut Model, msg: Msg) -> Vec<Effect> {
    match msg {
        Msg::Key(Key { notation }) => match model.focus {
            Focus::Engine => vec![Effect::Rpc(RpcCall::Input { notation })],
        },
        Msg::Redraw(events) => {
            for ev in events {
                apply_ui_event(model, ev);
            }
            Vec::new()
        }
        // loop plumbing tokens: the loop resolves these into Redraw/EngineDown
        // before update() ever sees them, so both arms are no-ops here
        Msg::RedrawReady | Msg::EngineStopped => Vec::new(),
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
            vec![Effect::Rpc(RpcCall::TryResize { width, height })]
        }
    }
}

fn apply_ui_event(model: &mut Model, ev: UiEvent) {
    match ev {
        UiEvent::GridResize { width, height, .. } => {
            // clamp untrusted wire dimensions: a desynced or malformed
            // grid_resize must not allocate unboundedly, and a plain `as
            // u16` cast would silently truncate 65536 to 0
            model.engine.grid.apply(GridOp::Resize {
                width: clamp_dim(width),
                height: clamp_dim(height),
            });
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
        }
        UiEvent::GridCursorGoto { row, col, .. } => {
            model.engine.grid.apply(GridOp::CursorGoto {
                row: saturate_u16(row),
                col: saturate_u16(col),
            });
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
        }
        UiEvent::GridClear { .. } => model.engine.grid.apply(GridOp::Clear),
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
        }
        UiEvent::DefaultColorsSet { fg, bg, .. } => {
            model.engine.hl.default_fg = fg;
            model.engine.hl.default_bg = bg;
        }
        UiEvent::Flush => model.dirty = true,
        UiEvent::Unknown { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::events::UiEvent;
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
}
