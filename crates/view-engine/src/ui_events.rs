//! Typed decoding of nvim's `ext_linegrid` `redraw` notification batches.
//!
//! The event vocabulary itself ([`UiEvent`], [`GridCell`]) lives in
//! `view-core`: it is pure data consumed by `update()`, and decoding is the
//! only part of this crate's job.

use crate::wire::map_find;
use rmpv::Value;
pub use view_core::events::{GridCell, ModeInfo, PmItem, TabEntry, TabHandle, UiEvent, WinHandle};

/// Decodes a `redraw` notification's params into typed [`UiEvent`]s.
///
/// The wire format is `["event_name", [args...], [args...], ...]`, batched:
/// one event name can carry many argument tuples, each producing one
/// `UiEvent`. Malformed tuples inside a recognized event decode to
/// [`UiEvent::Unknown`] with that event's name rather than being dropped or
/// panicking.
#[must_use]
pub fn decode_redraw(params: &[Value]) -> Vec<UiEvent> {
    let mut events = Vec::new();
    for batch in params {
        let Some(items) = batch.as_array() else {
            continue;
        };
        let Some((name_val, arg_tuples)) = items.split_first() else {
            continue;
        };
        let Some(name) = name_val.as_str() else {
            continue;
        };
        for tuple in arg_tuples {
            events.push(decode_event(name, tuple));
        }
    }
    events
}

fn decode_event(name: &str, tuple: &Value) -> UiEvent {
    let unknown = || UiEvent::Unknown {
        name: name.to_string(),
    };
    let Some(args) = tuple.as_array() else {
        return unknown();
    };
    match name {
        "grid_resize" => decode_grid_resize(args).unwrap_or_else(unknown),
        "grid_line" => decode_grid_line(args).unwrap_or_else(unknown),
        "grid_cursor_goto" => decode_grid_cursor_goto(args).unwrap_or_else(unknown),
        "grid_scroll" => decode_grid_scroll(args).unwrap_or_else(unknown),
        "grid_clear" => decode_grid_clear(args).unwrap_or_else(unknown),
        "win_viewport" => decode_win_viewport(args).unwrap_or_else(unknown),
        "hl_attr_define" => decode_hl_attr_define(args).unwrap_or_else(unknown),
        "default_colors_set" => decode_default_colors_set(args).unwrap_or_else(unknown),
        "hl_group_set" => decode_hl_group_set(args).unwrap_or_else(unknown),
        "flush" => UiEvent::Flush,
        "mode_info_set" => decode_mode_info_set(args).unwrap_or_else(unknown),
        "mode_change" => decode_mode_change(args).unwrap_or_else(unknown),
        "cmdline_show" => decode_cmdline_show(args).unwrap_or_else(unknown),
        "cmdline_pos" => decode_cmdline_pos(args).unwrap_or_else(unknown),
        // level/abort carry no state this decoder models: any arity hides
        // the cmdline, same as `flush`/`msg_clear`/`popupmenu_hide` below
        "cmdline_hide" => UiEvent::CmdlineHide,
        "msg_show" => decode_msg_show(args).unwrap_or_else(unknown),
        "msg_clear" => UiEvent::MsgClear,
        "msg_showmode" => decode_msg_showmode(args).unwrap_or_else(unknown),
        "msg_showcmd" => decode_msg_showcmd(args).unwrap_or_else(unknown),
        "msg_ruler" => decode_msg_ruler(args).unwrap_or_else(unknown),
        "tabline_update" => decode_tabline_update(args).unwrap_or_else(unknown),
        "popupmenu_show" => decode_popupmenu_show(args).unwrap_or_else(unknown),
        "popupmenu_select" => decode_popupmenu_select(args).unwrap_or_else(unknown),
        "popupmenu_hide" => UiEvent::PopupmenuHide,
        // no fields on the wire (confirmed via `nvim --api-info`'s
        // `mouse_on()`/`mouse_off()` entries and a live `:set mouse=a`
        // capture), same unconditional mapping as `flush`/`msg_clear` above
        "mouse_on" => UiEvent::MouseOn,
        "mouse_off" => UiEvent::MouseOff,
        _ => unknown(),
    }
}

fn as_u64(v: &Value) -> Option<u64> {
    v.as_u64()
}

fn as_i64(v: &Value) -> Option<i64> {
    v.as_i64()
}

fn decode_grid_resize(args: &[Value]) -> Option<UiEvent> {
    let [grid, width, height, ..] = args else {
        return None;
    };
    Some(UiEvent::GridResize {
        grid: as_u64(grid)?,
        width: as_u64(width)?,
        height: as_u64(height)?,
    })
}

fn decode_grid_cursor_goto(args: &[Value]) -> Option<UiEvent> {
    let [grid, row, col, ..] = args else {
        return None;
    };
    Some(UiEvent::GridCursorGoto {
        grid: as_u64(grid)?,
        row: as_u64(row)?,
        col: as_u64(col)?,
    })
}

fn decode_grid_scroll(args: &[Value]) -> Option<UiEvent> {
    let [grid, top, bot, left, right, rows, ..] = args else {
        return None;
    };
    Some(UiEvent::GridScroll {
        grid: as_u64(grid)?,
        top: as_u64(top)?,
        bot: as_u64(bot)?,
        left: as_u64(left)?,
        right: as_u64(right)?,
        rows: as_i64(rows)?,
    })
}

fn decode_grid_clear(args: &[Value]) -> Option<UiEvent> {
    let [grid, ..] = args else { return None };
    Some(UiEvent::GridClear {
        grid: as_u64(grid)?,
    })
}

fn decode_win_viewport(args: &[Value]) -> Option<UiEvent> {
    // nvim grew `line_count` and `scroll_delta` onto the end of this tuple
    // after the six fields below; `..` takes whatever a build sends rather
    // than making arity part of the match, the same way `grid_line` does
    let [grid, win, topline, botline, curline, curcol, ..] = args else {
        return None;
    };
    Some(UiEvent::WinViewport {
        grid: as_u64(grid)?,
        win: WinHandle(decode_ext_handle(win)?),
        topline: as_u64(topline)?,
        botline: as_u64(botline)?,
        curline: as_u64(curline)?,
        curcol: as_u64(curcol)?,
    })
}

fn decode_grid_line(args: &[Value]) -> Option<UiEvent> {
    // nvim's wire tuple is [grid, row, col_start, cells, wrap]; `wrap` (and
    // any future trailing field) is intentionally ignored via `..` rather
    // than pattern-matched exactly, so a minor-version arity bump degrades
    // gracefully instead of every real grid_line falling through to Unknown.
    let [grid, row, col_start, cells, ..] = args else {
        return None;
    };
    let cell_tuples = cells.as_array()?;
    let mut out = Vec::with_capacity(cell_tuples.len());
    let mut last_hl_id = 0u64;
    for tuple in cell_tuples {
        let fields = tuple.as_array()?;
        let text = fields.first()?.as_str()?.to_string();
        let hl_id = match fields.get(1) {
            Some(v) => as_u64(v)?,
            None => last_hl_id,
        };
        let repeat = match fields.get(2) {
            Some(v) => as_u64(v)?,
            None => 1,
        };
        last_hl_id = hl_id;
        out.push(GridCell {
            text,
            hl_id,
            repeat,
        });
    }
    Some(UiEvent::GridLine {
        grid: as_u64(grid)?,
        row: as_u64(row)?,
        col_start: as_u64(col_start)?,
        cells: out,
    })
}

fn decode_hl_attr_define(args: &[Value]) -> Option<UiEvent> {
    let [id, rgb_attrs, ..] = args else {
        return None;
    };
    let map = rgb_attrs.as_map()?;
    let lookup = |key: &str| map_find(map, key);
    // reject rather than truncate: a wire value past u32 is a malformed
    // color, and `as` would silently fold it onto a valid-looking one
    let fg = lookup("foreground")
        .and_then(as_u64)
        .and_then(|v| u32::try_from(v).ok());
    let bg = lookup("background")
        .and_then(as_u64)
        .and_then(|v| u32::try_from(v).ok());
    let bold = lookup("bold").and_then(Value::as_bool).unwrap_or(false);
    let italic = lookup("italic").and_then(Value::as_bool).unwrap_or(false);
    let underline = lookup("underline")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let reverse = lookup("reverse").and_then(Value::as_bool).unwrap_or(false);
    Some(UiEvent::HlAttrDefine {
        id: as_u64(id)?,
        fg,
        bg,
        bold,
        italic,
        underline,
        reverse,
    })
}

fn decode_default_colors_set(args: &[Value]) -> Option<UiEvent> {
    let [rgb_fg, rgb_bg, rgb_sp, ..] = args else {
        return None;
    };
    Some(UiEvent::DefaultColorsSet {
        fg: as_color(rgb_fg)?,
        bg: as_color(rgb_bg)?,
        sp: as_color(rgb_sp)?,
    })
}

// Outer None = malformed (not an integer, event becomes Unknown); inner
// None = unset color. nvim sends -1 for unset and every set value is
// 24-bit RGB, so any negative maps to None rather than a bogus color.
fn as_color(v: &Value) -> Option<Option<u32>> {
    let n = as_i64(v)?;
    Some(u32::try_from(n).ok())
}

// wire tuple is [name, hl_id] (confirmed via a live `--clean` capture:
// `nvim --embed`, ui_attach with ext_linegrid, dump the raw redraw params
// for the "hl_group_set" batch before decoding) -- the same 2-element
// shape as `mode_change`'s `[mode, mode_idx]`, so this follows that
// decoder's pattern rather than `hl_attr_define`'s map-lookup one.
fn decode_hl_group_set(args: &[Value]) -> Option<UiEvent> {
    let [name, hl_id, ..] = args else {
        return None;
    };
    Some(UiEvent::HlGroupSet {
        name: name.as_str()?.to_string(),
        hl_id: as_u64(hl_id)?,
    })
}

fn decode_mode_info_set(args: &[Value]) -> Option<UiEvent> {
    let [enabled, mode_maps, ..] = args else {
        return None;
    };
    let cursor_style_enabled = enabled.as_bool()?;
    let mode_maps = mode_maps.as_array()?;
    let mut modes = Vec::with_capacity(mode_maps.len());
    for entry in mode_maps {
        let map = entry.as_map()?;
        let lookup = |key: &str| map_find(map, key);
        let string_field = |key: &str| {
            lookup(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let int_field = |key: &str| lookup(key).and_then(as_u64).unwrap_or(0);
        modes.push(ModeInfo {
            name: string_field("name"),
            short_name: string_field("short_name"),
            cursor_shape: string_field("cursor_shape"),
            cell_percentage: int_field("cell_percentage"),
            blinkwait: int_field("blinkwait"),
            blinkon: int_field("blinkon"),
            blinkoff: int_field("blinkoff"),
            attr_id: int_field("attr_id"),
        });
    }
    Some(UiEvent::ModeInfoSet {
        cursor_style_enabled,
        modes,
    })
}

fn decode_mode_change(args: &[Value]) -> Option<UiEvent> {
    let [mode, mode_idx, ..] = args else {
        return None;
    };
    Some(UiEvent::ModeChange {
        mode: mode.as_str()?.to_string(),
        mode_idx: as_u64(mode_idx)?,
    })
}

// cmdline_show/msg_show content chunks are [attr_id, text, hl_id] on the
// wire (confirmed against api-ui-events.txt and the live capture); only
// the first two fields are modeled, per the tolerant trailing-field
// convention used throughout this module.
fn decode_content_chunks(v: &Value) -> Option<Vec<(u64, String)>> {
    let chunks = v.as_array()?;
    let mut out = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let fields = chunk.as_array()?;
        let [attr_id, text, ..] = fields.as_slice() else {
            return None;
        };
        out.push((as_u64(attr_id)?, text.as_str()?.to_string()));
    }
    Some(out)
}

fn decode_cmdline_show(args: &[Value]) -> Option<UiEvent> {
    let [content, pos, firstc, prompt, indent, level, ..] = args else {
        return None;
    };
    Some(UiEvent::CmdlineShow {
        content: decode_content_chunks(content)?,
        pos: as_u64(pos)?,
        firstc: firstc.as_str()?.to_string(),
        prompt: prompt.as_str()?.to_string(),
        indent: as_u64(indent)?,
        level: as_u64(level)?,
    })
}

fn decode_cmdline_pos(args: &[Value]) -> Option<UiEvent> {
    let [pos, level, ..] = args else {
        return None;
    };
    Some(UiEvent::CmdlinePos {
        pos: as_u64(pos)?,
        level: as_u64(level)?,
    })
}

fn decode_msg_show(args: &[Value]) -> Option<UiEvent> {
    let [kind, content, replace_last, ..] = args else {
        return None;
    };
    Some(UiEvent::MsgShow {
        kind: kind.as_str()?.to_string(),
        content: decode_content_chunks(content)?,
        replace_last: replace_last.as_bool()?,
    })
}

/// `msg_showmode`/`msg_showcmd`/`msg_ruler` each carry one `content` array,
/// identical in shape to `msg_show`'s (see `docs/statusline-wire-capture.md`)
/// -- decoded with the same [`decode_content_chunks`] rather than a second
/// copy of its chunk-unpacking loop.
fn decode_msg_showmode(args: &[Value]) -> Option<UiEvent> {
    let [content, ..] = args else {
        return None;
    };
    Some(UiEvent::MsgShowmode {
        content: decode_content_chunks(content)?,
    })
}

fn decode_msg_showcmd(args: &[Value]) -> Option<UiEvent> {
    let [content, ..] = args else {
        return None;
    };
    Some(UiEvent::MsgShowcmd {
        content: decode_content_chunks(content)?,
    })
}

fn decode_msg_ruler(args: &[Value]) -> Option<UiEvent> {
    let [content, ..] = args else {
        return None;
    };
    Some(UiEvent::MsgRuler {
        content: decode_content_chunks(content)?,
    })
}

// Tabpage/Window/Buffer handles arrive as msgpack-RPC Ext values whose
// payload is itself a msgpack-encoded integer; unwrapping it here keeps that
// Ext detail out of view-core, which stays rmpv-free.
fn decode_ext_handle(v: &Value) -> Option<u64> {
    let Value::Ext(_, data) = v else {
        return None;
    };
    let mut cursor = &data[..];
    let inner = rmpv::decode::read_value(&mut cursor).ok()?;
    inner.as_u64()
}

fn decode_tab_handle(v: &Value) -> Option<TabHandle> {
    Some(TabHandle(decode_ext_handle(v)?))
}

fn decode_tabline_update(args: &[Value]) -> Option<UiEvent> {
    let [current, tabs, ..] = args else {
        return None;
    };
    let current = decode_tab_handle(current)?;
    let tab_maps = tabs.as_array()?;
    let mut out = Vec::with_capacity(tab_maps.len());
    for entry in tab_maps {
        let map = entry.as_map()?;
        let tab = map_find(map, "tab")?;
        let name = map_find(map, "name").and_then(Value::as_str)?;
        out.push(TabEntry {
            tab: decode_tab_handle(tab)?,
            name: name.to_string(),
        });
    }
    Some(UiEvent::TablineUpdate { current, tabs: out })
}

fn decode_popupmenu_show(args: &[Value]) -> Option<UiEvent> {
    let [items, selected, row, col, grid, ..] = args else {
        return None;
    };
    let item_arrays = items.as_array()?;
    let mut out = Vec::with_capacity(item_arrays.len());
    for item in item_arrays {
        let fields = item.as_array()?;
        let [word, kind, menu, info, ..] = fields.as_slice() else {
            return None;
        };
        out.push(PmItem {
            word: word.as_str()?.to_string(),
            kind: kind.as_str()?.to_string(),
            menu: menu.as_str()?.to_string(),
            info: info.as_str()?.to_string(),
        });
    }
    Some(UiEvent::PopupmenuShow {
        items: out,
        selected: as_i64(selected)?,
        row: as_u64(row)?,
        col: as_u64(col)?,
        // as_i64, not as_u64: a cmdline-sourced popup sends -1 here (see
        // docs/palette-popupmenu-source-wire-capture.md), and as_u64
        // returns None for a negative wire integer, which would make the
        // `?` short-circuit the whole event to "unknown" and silently
        // drop every cmdline-sourced popupmenu_show.
        grid: as_i64(grid)?,
    })
}

fn decode_popupmenu_select(args: &[Value]) -> Option<UiEvent> {
    let [selected, ..] = args else {
        return None;
    };
    Some(UiEvent::PopupmenuSelect {
        selected: as_i64(selected)?,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use rmpv::Value;

    fn arr(v: Vec<Value>) -> Value {
        Value::Array(v)
    }

    #[test]
    fn decodes_grid_line_with_hl_carryover_and_repeat() {
        // real nvim's grid_line tuple is [grid, row, col_start, cells, wrap]
        // (5 elements) -- the trailing `wrap` is mandatory on the wire even
        // though this decoder doesn't consume it.
        let params = vec![arr(vec![
            Value::from("grid_line"),
            arr(vec![
                Value::from(1),
                Value::from(0),
                Value::from(0),
                arr(vec![
                    arr(vec![Value::from("a"), Value::from(5)]),
                    arr(vec![Value::from("b")]), // carries hl 5
                    arr(vec![Value::from(" "), Value::from(0), Value::from(3)]),
                ]),
                Value::from(false),
            ]),
        ])];
        let evs = decode_redraw(&params);
        assert_eq!(
            evs,
            vec![UiEvent::GridLine {
                grid: 1,
                row: 0,
                col_start: 0,
                cells: vec![
                    GridCell {
                        text: "a".to_string(),
                        hl_id: 5,
                        repeat: 1,
                    },
                    GridCell {
                        text: "b".to_string(),
                        hl_id: 5,
                        repeat: 1,
                    },
                    GridCell {
                        text: " ".to_string(),
                        hl_id: 0,
                        repeat: 3,
                    },
                ],
            }]
        );
    }

    #[test]
    fn decodes_scroll_resize_clear_cursor_flush() {
        let params = vec![
            arr(vec![
                Value::from("grid_resize"),
                arr(vec![1.into(), 80.into(), 24.into()]),
            ]),
            arr(vec![
                Value::from("grid_scroll"),
                arr(vec![
                    1.into(),
                    0.into(),
                    24.into(),
                    0.into(),
                    80.into(),
                    3.into(),
                    0.into(),
                ]),
            ]),
            arr(vec![Value::from("grid_clear"), arr(vec![1.into()])]),
            arr(vec![
                Value::from("grid_cursor_goto"),
                arr(vec![1.into(), 2.into(), 5.into()]),
            ]),
            arr(vec![Value::from("flush"), arr(vec![])]),
        ];
        let evs = decode_redraw(&params);
        assert!(matches!(
            evs[0],
            UiEvent::GridResize {
                grid: 1,
                width: 80,
                height: 24
            }
        ));
        assert!(matches!(evs[1], UiEvent::GridScroll { rows: 3, .. }));
        assert!(matches!(evs[2], UiEvent::GridClear { grid: 1 }));
        assert!(matches!(
            evs[3],
            UiEvent::GridCursorGoto { row: 2, col: 5, .. }
        ));
        assert!(matches!(evs[4], UiEvent::Flush));
    }

    #[test]
    fn unknown_events_are_preserved_not_dropped() {
        let params = vec![arr(vec![Value::from("set_title"), arr(vec![])])];
        let evs = decode_redraw(&params);
        assert!(matches!(&evs[0], UiEvent::Unknown { name } if name == "set_title"));
    }

    /// The viewport event carries the one relocation nvim announces without
    /// resending the relocated cells, so it has to decode to a variant that
    /// still holds `topline` -- and it has to keep decoding on a build that
    /// appends fields to the tuple, which nvim has already done twice.
    #[test]
    fn decodes_win_viewport_including_a_longer_tuple() {
        let six = arr(vec![
            Value::from(1u64),
            Value::Ext(1, vec![7]),
            Value::from(10u64),
            Value::from(34u64),
            Value::from(12u64),
            Value::from(3u64),
        ]);
        let eight = arr(vec![
            Value::from(1u64),
            Value::Ext(1, vec![7]),
            Value::from(10u64),
            Value::from(34u64),
            Value::from(12u64),
            Value::from(3u64),
            Value::from(200u64),
            Value::from(0u64),
        ]);
        let params = vec![arr(vec![Value::from("win_viewport"), six, eight])];

        let evs = decode_redraw(&params);

        let expected = UiEvent::WinViewport {
            grid: 1,
            win: WinHandle(7),
            topline: 10,
            botline: 34,
            curline: 12,
            curcol: 3,
        };
        assert_eq!(evs, vec![expected.clone(), expected]);
    }

    /// A tuple too short to carry a topline is worth nothing to the reader
    /// that needs one, so it degrades to `Unknown` rather than to a viewport
    /// with a fabricated value.
    #[test]
    fn a_truncated_win_viewport_decodes_to_unknown() {
        let params = vec![arr(vec![
            Value::from("win_viewport"),
            arr(vec![Value::from(1u64), Value::Ext(1, vec![7])]),
        ])];
        let evs = decode_redraw(&params);
        assert!(matches!(&evs[0], UiEvent::Unknown { name } if name == "win_viewport"));
    }

    #[test]
    fn decodes_hl_attr_define_with_partial_attrs() {
        // real wire args are [id, rgb_attrs, cterm_attrs, info]; rgb_attrs
        // only carries keys nvim actually set for this attribute, so the
        // decoder must default absent keys rather than requiring all six.
        let rgb_attrs = Value::Map(vec![
            (Value::from("foreground"), Value::from(0x00_ff00_u32)),
            (Value::from("bold"), Value::from(true)),
            (Value::from("underline"), Value::from(true)),
            // background, italic, reverse deliberately absent
        ]);
        let params = vec![arr(vec![
            Value::from("hl_attr_define"),
            arr(vec![
                Value::from(3),
                rgb_attrs,
                Value::Map(vec![]),
                arr(vec![]),
            ]),
        ])];
        let evs = decode_redraw(&params);
        assert_eq!(
            evs,
            vec![UiEvent::HlAttrDefine {
                id: 3,
                fg: Some(0x00_ff00),
                bg: None,
                bold: true,
                italic: false,
                underline: true,
                reverse: false,
            }]
        );
    }

    #[test]
    fn decodes_default_colors_set_with_unset_sentinel() {
        // nvim sends -1 for an unset color; the decoder maps it to None so
        // no consumer can mistake it for a valid 24-bit RGB value.
        let params = vec![arr(vec![
            Value::from("default_colors_set"),
            arr(vec![
                Value::from(-1),
                Value::from(0x0000_0000_u32),
                Value::from(0x00ff_ffff_u32),
                Value::from(0),
                Value::from(15),
            ]),
        ])];
        let evs = decode_redraw(&params);
        assert_eq!(
            evs,
            vec![UiEvent::DefaultColorsSet {
                fg: None,
                bg: Some(0),
                sp: Some(0x00ff_ffff),
            }]
        );
    }

    /// Fixture copied from a live `nvim --embed --clean` capture: `nvim_ui_attach`
    /// with `ext_linegrid`, dumping the raw redraw params for the
    /// `hl_group_set` batch before any decoding. One real batch carries
    /// ~72 `[name, hl_id]` tuples in one call (every builtin UI element
    /// group at once); this fixture keeps the ones this decoder's chrome
    /// consumers actually resolve by name, plus the always-present empty-name
    /// sentinel entry real nvim sends first in every batch.
    #[test]
    fn decodes_hl_group_set_batch() {
        let params = vec![arr(vec![
            Value::from("hl_group_set"),
            arr(vec![Value::from(""), Value::from(0)]),
            arr(vec![Value::from("StatusLine"), Value::from(41)]),
            arr(vec![Value::from("TabLineSel"), Value::from(42)]),
            arr(vec![Value::from("Pmenu"), Value::from(31)]),
        ])];
        let evs = decode_redraw(&params);
        assert_eq!(
            evs,
            vec![
                UiEvent::HlGroupSet {
                    name: String::new(),
                    hl_id: 0,
                },
                UiEvent::HlGroupSet {
                    name: "StatusLine".to_string(),
                    hl_id: 41,
                },
                UiEvent::HlGroupSet {
                    name: "TabLineSel".to_string(),
                    hl_id: 42,
                },
                UiEvent::HlGroupSet {
                    name: "Pmenu".to_string(),
                    hl_id: 31,
                },
            ]
        );
    }

    // fixtures below are copied from a live `nvim --clean` capture
    // (~/.claude/tmp/capture-{cmdline,tabline,popupmenu}.log), driven via
    // crates/view-engine/tests/zz_capture_live.rs and cross-checked against
    // nvim/runtime/doc/api-ui-events.txt's ui_events arities. That harness
    // was scratch-only and is not part of this commit.

    #[test]
    fn decodes_mode_info_set_and_mode_change() {
        // capture-cmdline.log, mode_info_set + mode_change("cmdline_normal")
        // events (attach-time mode table, then entering `:`); the real
        // table has 16 modes, only 2 are reproduced here since the decoder
        // treats each dict identically.
        let normal_mode = Value::Map(vec![
            (Value::from("name"), Value::from("normal")),
            (Value::from("short_name"), Value::from("n")),
            (Value::from("mouse_shape"), Value::from(0)),
            (Value::from("cursor_shape"), Value::from("block")),
            (Value::from("cell_percentage"), Value::from(0)),
            (Value::from("blinkwait"), Value::from(700)),
            (Value::from("blinkon"), Value::from(400)),
            (Value::from("blinkoff"), Value::from(250)),
            (Value::from("hl_id"), Value::from(0)),
            (Value::from("id_lm"), Value::from(0)),
            (Value::from("attr_id"), Value::from(0)),
            (Value::from("attr_id_lm"), Value::from(0)),
        ]);
        let insert_mode = Value::Map(vec![
            (Value::from("name"), Value::from("insert")),
            (Value::from("short_name"), Value::from("i")),
            (Value::from("mouse_shape"), Value::from(0)),
            (Value::from("cursor_shape"), Value::from("vertical")),
            (Value::from("cell_percentage"), Value::from(25)),
            (Value::from("blinkwait"), Value::from(0)),
            (Value::from("blinkon"), Value::from(0)),
            (Value::from("blinkoff"), Value::from(0)),
            (Value::from("hl_id"), Value::from(0)),
            (Value::from("id_lm"), Value::from(0)),
            (Value::from("attr_id"), Value::from(0)),
            (Value::from("attr_id_lm"), Value::from(0)),
        ]);
        let params = vec![
            arr(vec![
                Value::from("mode_info_set"),
                arr(vec![Value::from(true), arr(vec![normal_mode, insert_mode])]),
            ]),
            arr(vec![
                Value::from("mode_change"),
                arr(vec![Value::from("cmdline_normal"), Value::from(4)]),
            ]),
        ];
        let evs = decode_redraw(&params);
        assert_eq!(
            evs,
            vec![
                UiEvent::ModeInfoSet {
                    cursor_style_enabled: true,
                    modes: vec![
                        ModeInfo {
                            name: "normal".into(),
                            short_name: "n".into(),
                            cursor_shape: "block".into(),
                            cell_percentage: 0,
                            blinkwait: 700,
                            blinkon: 400,
                            blinkoff: 250,
                            attr_id: 0,
                        },
                        ModeInfo {
                            name: "insert".into(),
                            short_name: "i".into(),
                            cursor_shape: "vertical".into(),
                            cell_percentage: 25,
                            blinkwait: 0,
                            blinkon: 0,
                            blinkoff: 0,
                            attr_id: 0,
                        },
                    ],
                },
                UiEvent::ModeChange {
                    mode: "cmdline_normal".into(),
                    mode_idx: 4,
                },
            ]
        );
    }

    #[test]
    fn mode_info_entry_missing_cursor_fields_defaults_to_zero() {
        // capture-popupmenu.log, mode_info_set: the mouse-only hover modes
        // (e.g. "cmdline_hover") carry only name/short_name/mouse_shape, no
        // cursor_shape or blink fields at all.
        let hover_mode = Value::Map(vec![
            (Value::from("name"), Value::from("cmdline_hover")),
            (Value::from("short_name"), Value::from("e")),
            (Value::from("mouse_shape"), Value::from(0)),
        ]);
        let params = vec![arr(vec![
            Value::from("mode_info_set"),
            arr(vec![Value::from(true), arr(vec![hover_mode])]),
        ])];
        let evs = decode_redraw(&params);
        assert_eq!(
            evs,
            vec![UiEvent::ModeInfoSet {
                cursor_style_enabled: true,
                modes: vec![ModeInfo {
                    name: "cmdline_hover".into(),
                    short_name: "e".into(),
                    cursor_shape: String::new(),
                    cell_percentage: 0,
                    blinkwait: 0,
                    blinkon: 0,
                    blinkoff: 0,
                    attr_id: 0,
                }],
            }]
        );
    }

    #[test]
    fn decodes_cmdline_show_pos_and_hide() {
        // capture-cmdline.log: cmdline_show(content=[[0,"q",0]], pos=1,
        // firstc=":", prompt="", indent=0, level=1, hl_id=0), then
        // cmdline_pos(pos=0, level=1) after <Left>, then
        // cmdline_hide(level=1, abort=false) after <CR>. The wire's
        // trailing hl_id (chunk-level and cmdline-level) and cmdline_hide's
        // level/abort are intentionally undecoded, matching this module's
        // existing tolerant-trailing-field convention.
        let params = vec![
            arr(vec![
                Value::from("cmdline_show"),
                arr(vec![
                    arr(vec![arr(vec![
                        Value::from(0),
                        Value::from("q"),
                        Value::from(0),
                    ])]),
                    Value::from(1),
                    Value::from(":"),
                    Value::from(""),
                    Value::from(0),
                    Value::from(1),
                    Value::from(0),
                ]),
            ]),
            arr(vec![
                Value::from("cmdline_pos"),
                arr(vec![Value::from(0), Value::from(1)]),
            ]),
            arr(vec![
                Value::from("cmdline_hide"),
                arr(vec![Value::from(1), Value::from(false)]),
            ]),
        ];
        let evs = decode_redraw(&params);
        assert_eq!(
            evs,
            vec![
                UiEvent::CmdlineShow {
                    content: vec![(0, "q".to_string())],
                    pos: 1,
                    firstc: ":".to_string(),
                    prompt: String::new(),
                    indent: 0,
                    level: 1,
                },
                UiEvent::CmdlinePos { pos: 0, level: 1 },
                UiEvent::CmdlineHide,
            ]
        );
    }

    #[test]
    fn decodes_msg_show_and_msg_clear() {
        // capture-tabline.log: msg_show(kind="echomsg",
        // content=[[73,"...deprecated...",26]], replace_last=false,
        // history=true, append=..., id=..., trigger=...); only kind,
        // content, and replace_last are modeled, per this module's
        // tolerant-trailing-field convention.
        let params = vec![
            arr(vec![
                Value::from("msg_show"),
                arr(vec![
                    Value::from("echomsg"),
                    arr(vec![arr(vec![
                        Value::from(73),
                        Value::from("deprecated"),
                        Value::from(26),
                    ])]),
                    Value::from(false),
                    Value::from(true),
                    Value::from(false),
                    Value::Nil,
                    Value::from(""),
                ]),
            ]),
            arr(vec![Value::from("msg_clear"), arr(vec![])]),
        ];
        let evs = decode_redraw(&params);
        assert_eq!(
            evs,
            vec![
                UiEvent::MsgShow {
                    kind: "echomsg".to_string(),
                    content: vec![(73, "deprecated".to_string())],
                    replace_last: false,
                },
                UiEvent::MsgClear,
            ]
        );
    }

    #[test]
    fn decodes_msg_showmode_showcmd_ruler() {
        // docs/statusline-wire-capture.md: `qq` (start macro recording)
        // captured live as
        //   ['msg_showcmd', [[[0, 'qq', 0]]]]
        //   ['msg_showmode', [[[15, 'recording @q', 11]]]]
        //   ['msg_showcmd', [[]]]
        // and `laststatus=0` cursor motion as
        //   ['msg_ruler', [[[1, '0,0-1         All', 63]]]]
        let params = vec![
            arr(vec![
                Value::from("msg_showcmd"),
                arr(vec![arr(vec![arr(vec![
                    Value::from(0),
                    Value::from("qq"),
                    Value::from(0),
                ])])]),
            ]),
            arr(vec![
                Value::from("msg_showmode"),
                arr(vec![arr(vec![arr(vec![
                    Value::from(15),
                    Value::from("recording @q"),
                    Value::from(11),
                ])])]),
            ]),
            arr(vec![Value::from("msg_showcmd"), arr(vec![arr(vec![])])]),
            arr(vec![
                Value::from("msg_ruler"),
                arr(vec![arr(vec![arr(vec![
                    Value::from(1),
                    Value::from("0,0-1         All"),
                    Value::from(63),
                ])])]),
            ]),
        ];
        let evs = decode_redraw(&params);
        assert_eq!(
            evs,
            vec![
                UiEvent::MsgShowcmd {
                    content: vec![(0, "qq".to_string())],
                },
                UiEvent::MsgShowmode {
                    content: vec![(15, "recording @q".to_string())],
                },
                UiEvent::MsgShowcmd { content: vec![] },
                UiEvent::MsgRuler {
                    content: vec![(1, "0,0-1         All".to_string())],
                },
            ]
        );
    }

    #[test]
    fn decodes_tabline_update_with_ext_tab_handles() {
        // capture-tabline.log, tabline_update after `:tabnew<CR>`: current
        // is an Ext(2, [2]) Tabpage handle, tabs holds both tabs' {tab,
        // name} dicts. Ext(2, ..) encodes nvim's Tabpage type; the payload
        // bytes are themselves a msgpack-packed integer handle id.
        let tab1 = Value::Map(vec![
            (Value::from("tab"), Value::Ext(2, vec![1])),
            (Value::from("name"), Value::from("[No Name]")),
        ]);
        let tab2 = Value::Map(vec![
            (Value::from("tab"), Value::Ext(2, vec![2])),
            (Value::from("name"), Value::from("[No Name]")),
        ]);
        let params = vec![arr(vec![
            Value::from("tabline_update"),
            arr(vec![
                Value::Ext(2, vec![2]),
                arr(vec![tab1, tab2]),
                Value::Ext(0, vec![1]),
                arr(vec![]),
            ]),
        ])];
        let evs = decode_redraw(&params);
        assert_eq!(
            evs,
            vec![UiEvent::TablineUpdate {
                current: TabHandle(2),
                tabs: vec![
                    TabEntry {
                        tab: TabHandle(1),
                        name: "[No Name]".to_string(),
                    },
                    TabEntry {
                        tab: TabHandle(2),
                        name: "[No Name]".to_string(),
                    },
                ],
            }]
        );
    }

    #[test]
    fn decodes_popupmenu_show_select_and_hide() {
        // capture-popupmenu.log: popupmenu_show(items=[["foobar","","",""],
        // ["foo","","",""]], selected=0, row=0, col=11, grid=1), then
        // popupmenu_select(selected=1) from a second <C-n>, then
        // popupmenu_hide() after <Esc>.
        let item1 = arr(vec![
            Value::from("foobar"),
            Value::from(""),
            Value::from(""),
            Value::from(""),
        ]);
        let item2 = arr(vec![
            Value::from("foo"),
            Value::from(""),
            Value::from(""),
            Value::from(""),
        ]);
        let params = vec![
            arr(vec![
                Value::from("popupmenu_show"),
                arr(vec![
                    arr(vec![item1, item2]),
                    Value::from(0),
                    Value::from(0),
                    Value::from(11),
                    Value::from(1),
                ]),
            ]),
            arr(vec![
                Value::from("popupmenu_select"),
                arr(vec![Value::from(1)]),
            ]),
            arr(vec![Value::from("popupmenu_hide"), arr(vec![])]),
        ];
        let evs = decode_redraw(&params);
        assert_eq!(
            evs,
            vec![
                UiEvent::PopupmenuShow {
                    items: vec![
                        PmItem {
                            word: "foobar".to_string(),
                            kind: String::new(),
                            menu: String::new(),
                            info: String::new(),
                        },
                        PmItem {
                            word: "foo".to_string(),
                            kind: String::new(),
                            menu: String::new(),
                            info: String::new(),
                        },
                    ],
                    selected: 0,
                    row: 0,
                    col: 11,
                    grid: 1,
                },
                UiEvent::PopupmenuSelect { selected: 1 },
                UiEvent::PopupmenuHide,
            ]
        );
    }

    #[test]
    fn a_cmdline_sourced_popupmenu_grid_sentinel_decodes_instead_of_vanishing() {
        // live-captured against the pinned engine (see
        // docs/palette-popupmenu-source-wire-capture.md): a cmdline-sourced
        // completion (e.g. `:set nu<Tab>`) sends grid: -1, which as_u64
        // used to turn into a decode failure for the whole event -- so this
        // pins the fix rather than only the shape.
        let item = arr(vec![
            Value::from("number"),
            Value::from(""),
            Value::from(""),
            Value::from(""),
        ]);
        let params = vec![arr(vec![
            Value::from("popupmenu_show"),
            arr(vec![
                arr(vec![item]),
                Value::from(0),
                Value::from(0),
                Value::from(4),
                Value::from(-1),
            ]),
        ])];
        let evs = decode_redraw(&params);
        assert_eq!(
            evs,
            vec![UiEvent::PopupmenuShow {
                items: vec![PmItem {
                    word: "number".to_string(),
                    kind: String::new(),
                    menu: String::new(),
                    info: String::new(),
                }],
                selected: 0,
                row: 0,
                col: 4,
                grid: -1,
            }]
        );
    }

    #[test]
    fn popupmenu_select_reports_negative_one_sentinel() {
        // api-ui-events.txt: "selected is a zero-based index ... or -1"
        let params = vec![arr(vec![
            Value::from("popupmenu_select"),
            arr(vec![Value::from(-1)]),
        ])];
        let evs = decode_redraw(&params);
        assert_eq!(evs, vec![UiEvent::PopupmenuSelect { selected: -1 }]);
    }

    #[test]
    fn decodes_mouse_on_and_mouse_off() {
        // shape confirmed via `nvim --api-info`'s `mouse_on()`/`mouse_off()`
        // entries (both zero-parameter) and a live capture: driving
        // `nvim_input(":set mouse=a<CR>")` through a real spawned nvim
        // produced `[["mouse_on"]]` in the following redraw batch.
        let params = vec![
            arr(vec![Value::from("mouse_on"), arr(vec![])]),
            arr(vec![Value::from("mouse_off"), arr(vec![])]),
        ];
        let evs = decode_redraw(&params);
        assert_eq!(evs, vec![UiEvent::MouseOn, UiEvent::MouseOff]);
    }
}
