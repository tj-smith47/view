//! Typed decoding of nvim's `ext_linegrid` `redraw` notification batches.

use rmpv::Value;

/// One decoded `redraw` sub-event.
///
/// nvim batches many of these per `redraw` notification; unrecognized event
/// names decode to [`UiEvent::Unknown`] rather than being dropped, since new
/// event kinds arrive across nvim versions and callers may still want to see
/// the name.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum UiEvent {
    /// A grid was resized to `width` x `height` cells.
    GridResize { grid: u64, width: u64, height: u64 },
    /// A run of cells was written to `grid` starting at `(row, col_start)`.
    GridLine {
        grid: u64,
        row: u64,
        col_start: u64,
        cells: Vec<GridCell>,
    },
    /// The cursor moved to `(row, col)` on `grid`.
    GridCursorGoto { grid: u64, row: u64, col: u64 },
    /// A region of `grid` scrolled by `rows` (positive = down, negative = up).
    GridScroll {
        grid: u64,
        top: u64,
        bot: u64,
        left: u64,
        right: u64,
        rows: i64,
    },
    /// `grid` was cleared to the default background.
    GridClear { grid: u64 },
    /// A highlight attribute id was (re)defined.
    HlAttrDefine {
        id: u64,
        fg: Option<u32>,
        bg: Option<u32>,
        bold: bool,
        italic: bool,
        underline: bool,
        reverse: bool,
    },
    /// The default foreground/background/special colors changed.
    DefaultColorsSet { fg: i64, bg: i64, sp: i64 },
    /// nvim finished a batch of updates; safe to repaint.
    Flush,
    /// An event name this decoder does not yet model.
    Unknown { name: String },
}

/// One cell in a [`UiEvent::GridLine`] run.
///
/// `hl_id` carries over from the previous cell in the same line when the
/// wire tuple omits it, so callers never re-implement that carry-over rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridCell {
    pub text: String,
    pub hl_id: u64,
    pub repeat: u64,
}

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
            let Some(args) = tuple.as_array() else {
                events.push(UiEvent::Unknown {
                    name: name.to_string(),
                });
                continue;
            };
            events.push(decode_event(name, args));
        }
    }
    events
}

fn decode_event(name: &str, args: &[Value]) -> UiEvent {
    let unknown = || UiEvent::Unknown {
        name: name.to_string(),
    };
    match name {
        "grid_resize" => decode_grid_resize(args).unwrap_or_else(unknown),
        "grid_line" => decode_grid_line(args).unwrap_or_else(unknown),
        "grid_cursor_goto" => decode_grid_cursor_goto(args).unwrap_or_else(unknown),
        "grid_scroll" => decode_grid_scroll(args).unwrap_or_else(unknown),
        "grid_clear" => decode_grid_clear(args).unwrap_or_else(unknown),
        "hl_attr_define" => decode_hl_attr_define(args).unwrap_or_else(unknown),
        "default_colors_set" => decode_default_colors_set(args).unwrap_or_else(unknown),
        "flush" => UiEvent::Flush,
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
    let [grid, width, height] = args else {
        return None;
    };
    Some(UiEvent::GridResize {
        grid: as_u64(grid)?,
        width: as_u64(width)?,
        height: as_u64(height)?,
    })
}

fn decode_grid_cursor_goto(args: &[Value]) -> Option<UiEvent> {
    let [grid, row, col] = args else { return None };
    Some(UiEvent::GridCursorGoto {
        grid: as_u64(grid)?,
        row: as_u64(row)?,
        col: as_u64(col)?,
    })
}

fn decode_grid_scroll(args: &[Value]) -> Option<UiEvent> {
    let [grid, top, bot, left, right, rows, _cols] = args else {
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
    let [grid] = args else { return None };
    Some(UiEvent::GridClear {
        grid: as_u64(grid)?,
    })
}

fn decode_grid_line(args: &[Value]) -> Option<UiEvent> {
    let [grid, row, col_start, cells] = args else {
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
    let lookup = |key: &str| {
        map.iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v)
    };
    let fg = lookup("foreground").and_then(as_u64).map(|v| v as u32);
    let bg = lookup("background").and_then(as_u64).map(|v| v as u32);
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
        fg: as_i64(rgb_fg)?,
        bg: as_i64(rgb_bg)?,
        sp: as_i64(rgb_sp)?,
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
            ]),
        ])];
        let evs = decode_redraw(&params);
        let UiEvent::GridLine { cells, .. } = &evs[0] else {
            unreachable!("wrong event")
        };
        assert_eq!(
            cells
                .iter()
                .map(|c| (c.text.as_str(), c.hl_id, c.repeat))
                .collect::<Vec<_>>(),
            vec![("a", 5, 1), ("b", 5, 1), (" ", 0, 3)]
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
        let params = vec![arr(vec![Value::from("win_viewport"), arr(vec![])])];
        let evs = decode_redraw(&params);
        assert!(matches!(&evs[0], UiEvent::Unknown { name } if name == "win_viewport"));
    }
}
