//! `VIEW_REDRAW_LOG=<path>`: one line per layout-shaping redraw event in
//! the order nvim sent it, and per outgoing grid resize request, for
//! reconstructing the op sequence behind a screen whose grids disagree
//! with nvim's.
//!
//! The variable is read once, on the first call. Unset, every entry point
//! is one `OnceLock` read that finds `None` and formats nothing.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use view_core::events::UiEvent;

static SINK: OnceLock<Option<Mutex<File>>> = OnceLock::new();
static SEQ: AtomicU64 = AtomicU64::new(0);

fn sink() -> Option<&'static Mutex<File>> {
    SINK.get_or_init(|| {
        let path = std::env::var_os("VIEW_REDRAW_LOG")?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
            .map(Mutex::new)
    })
    .as_ref()
}

/// Whether `VIEW_REDRAW_LOG` named a file this process could open.
#[must_use]
pub fn enabled() -> bool {
    sink().is_some()
}

/// Writes one line, numbered in the sequence every line of the log shares.
/// The payload is built only when a log is open.
pub fn note(payload: impl FnOnce() -> String) {
    let Some(file) = sink() else {
        return;
    };
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let line = format!("{seq} {}\n", payload());
    let mut file = file.lock().unwrap_or_else(PoisonError::into_inner);
    let _ = file.write_all(line.as_bytes());
}

/// One line per event of a decoded batch that sizes, places, clears or
/// writes a grid, and one per `flush`.
pub(crate) fn batch(events: &[UiEvent]) {
    if !enabled() {
        return;
    }
    for ev in events {
        if let Some(line) = describe(ev) {
            note(|| line);
        }
    }
}

/// How many events one drain handed the runtime, after compaction.
pub(crate) fn drained(events: &[UiEvent]) {
    if !events.is_empty() {
        note(|| format!("drain events={}", events.len()));
    }
}

fn describe(ev: &UiEvent) -> Option<String> {
    Some(match ev {
        UiEvent::GridResize {
            grid,
            width,
            height,
        } => format!("grid_resize grid={grid} width={width} height={height}"),
        UiEvent::GridLine {
            grid,
            row,
            col_start,
            cells,
        } => {
            let span: u64 = cells.iter().map(|c| c.repeat).sum();
            format!("grid_line grid={grid} row={row} col={col_start} len={span}")
        }
        UiEvent::GridScroll {
            grid,
            top,
            bot,
            left,
            right,
            rows,
        } => format!(
            "grid_scroll grid={grid} top={top} bot={bot} left={left} right={right} rows={rows}"
        ),
        UiEvent::GridClear { grid } => format!("grid_clear grid={grid}"),
        UiEvent::GridDestroy { grid } => format!("grid_destroy grid={grid}"),
        UiEvent::WinPos {
            grid,
            win,
            startrow,
            startcol,
            width,
            height,
        } => format!(
            "win_pos grid={grid} win={} row={startrow} col={startcol} width={width} height={height}",
            win.0
        ),
        UiEvent::WinFloatPos {
            grid,
            screen_row,
            screen_col,
            ..
        } => format!("win_float_pos grid={grid} row={screen_row} col={screen_col}"),
        UiEvent::WinHide { grid } => format!("win_hide grid={grid}"),
        UiEvent::WinClose { grid } => format!("win_close grid={grid}"),
        UiEvent::Flush => "flush".to_string(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use view_core::events::GridCell;

    #[test]
    fn a_line_is_summarised_by_its_span_and_unlogged_kinds_write_nothing() {
        let line = UiEvent::GridLine {
            grid: 4,
            row: 2,
            col_start: 3,
            cells: vec![
                GridCell {
                    text: "a".into(),
                    hl_id: 0,
                    repeat: 2,
                },
                GridCell {
                    text: "b".into(),
                    hl_id: 0,
                    repeat: 5,
                },
            ],
        };
        assert_eq!(
            describe(&line).as_deref(),
            Some("grid_line grid=4 row=2 col=3 len=7")
        );
        assert_eq!(describe(&UiEvent::MsgClear), None);
    }
}
