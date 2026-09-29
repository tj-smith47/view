//! `VIEW_REDRAW_LOG=<path>`: one line per layout-shaping redraw event in
//! the order nvim sent it, and per outgoing grid resize request, for
//! reconstructing the op sequence behind a screen whose grids disagree
//! with nvim's.
//!
//! [`init`] reads the variable and opens the file once, at startup. Unset,
//! every entry point is one `OnceLock` read that finds `None` and formats
//! nothing.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::sync::{Mutex, OnceLock, PoisonError};

use view_core::events::UiEvent;

/// The open file and the number its next line carries. The number lives
/// behind the file's own lock, so lines from different threads are numbered
/// in the order they reach the file.
struct Log {
    file: Box<dyn std::io::Write + Send>,
    seq: u64,
}

static SINK: OnceLock<Option<Mutex<Log>>> = OnceLock::new();

/// A `VIEW_REDRAW_LOG` path this process could not open for append.
#[derive(Debug, thiserror::Error)]
#[error(
    "cannot open VIEW_REDRAW_LOG path {}: {source}, redraw logging disabled",
    path.to_string_lossy()
)]
pub struct OpenError {
    path: OsString,
    source: std::io::Error,
}

/// Opens the file `VIEW_REDRAW_LOG` names, once per process. A later call
/// does nothing.
///
/// # Errors
///
/// Returns [`OpenError`] when the path cannot be opened for append. The log
/// then stays off for the life of the process.
pub fn init() -> Result<(), OpenError> {
    let mut failed = None;
    SINK.get_or_init(|| match open(std::env::var_os("VIEW_REDRAW_LOG")) {
        Ok(log) => log,
        Err(e) => {
            failed = Some(e);
            None
        }
    });
    failed.map_or(Ok(()), Err)
}

fn open(path: Option<OsString>) -> Result<Option<Mutex<Log>>, OpenError> {
    let Some(path) = path else {
        return Ok(None);
    };
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => Ok(Some(Mutex::new(Log {
            file: Box::new(file),
            seq: 0,
        }))),
        Err(source) => Err(OpenError { path, source }),
    }
}

fn sink() -> Option<&'static Mutex<Log>> {
    SINK.get()?.as_ref()
}

/// Whether [`init`] opened a log.
#[must_use]
pub fn enabled() -> bool {
    sink().is_some()
}

/// Writes one line, numbered in the sequence every line of the log shares.
/// The payload is built only when a log is open.
pub fn note(payload: impl FnOnce() -> String) {
    if let Some(sink) = sink() {
        write(sink, [payload()]);
    }
}

/// One line per event of a decoded batch that sizes, places, clears or
/// writes a grid, and one per `flush`, written to the file at once.
pub(crate) fn batch(events: &[UiEvent]) {
    if let Some(sink) = sink() {
        let lines: Vec<String> = events.iter().filter_map(describe).collect();
        write(sink, lines);
    }
}

/// How many events one drain handed the runtime, after compaction.
pub(crate) fn drained(events: &[UiEvent]) {
    if !events.is_empty() {
        note(|| format!("drain events={}", events.len()));
    }
}

/// Numbers `lines` under the log's lock and hands them to the file in one
/// write, so a full redraw costs the reader thread one syscall.
fn write(sink: &Mutex<Log>, lines: impl IntoIterator<Item = String>) {
    let mut log = sink.lock().unwrap_or_else(PoisonError::into_inner);
    let text = numbered(&mut log.seq, lines);
    if !text.is_empty() {
        let _ = log.file.write_all(text.as_bytes());
    }
}

fn numbered(seq: &mut u64, lines: impl IntoIterator<Item = String>) -> String {
    let mut text = String::new();
    for line in lines {
        let _ = writeln!(text, "{seq} {line}");
        *seq += 1;
    }
    text
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
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use view_core::events::GridCell;
    use view_test_support::ScratchDir;

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

    /// Each `write` call the log receives, as the bytes of that call.
    #[derive(Clone, Default)]
    struct Writes(std::sync::Arc<Mutex<Vec<String>>>);

    impl std::io::Write for Writes {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(buf).into_owned());
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A batch reaches the file in one write, and the next write carries on
    /// from the number the batch left.
    ///
    /// Disconfirm: numbering each write from a counter of its own numbers
    /// the note after the batch `0`, and writing each line on its own
    /// splits the batch into two writes.
    #[test]
    fn a_batch_is_one_numbered_write_and_the_sequence_carries_on() {
        let writes = Writes::default();
        let sink = Mutex::new(Log {
            file: Box::new(writes.clone()),
            seq: 0,
        });
        write(
            &sink,
            [
                UiEvent::GridResize {
                    grid: 2,
                    width: 40,
                    height: 20,
                },
                UiEvent::MsgClear,
                UiEvent::Flush,
            ]
            .iter()
            .filter_map(describe),
        );
        write(&sink, ["drain events=3".to_string()]);
        assert_eq!(
            *writes.0.lock().unwrap(),
            vec![
                "0 grid_resize grid=2 width=40 height=20\n1 flush\n".to_string(),
                "2 drain events=3\n".to_string(),
            ]
        );
        assert_eq!(numbered(&mut 7, ["a".into(), "b".into()]), "7 a\n8 b\n");
    }

    /// Disconfirm: `open` answering an unopenable path with no log and no
    /// error fails before the message is read.
    #[test]
    fn a_path_that_cannot_be_opened_is_named_in_the_error() {
        let dir = ScratchDir::new("redraw-log").unwrap();
        let path = dir.path().join("missing").join("redraw.log");
        let shown = open(Some(path.clone().into()))
            .err()
            .expect("a path under a missing directory opened, or was dropped in silence")
            .to_string();
        assert!(
            shown.starts_with(&format!(
                "cannot open VIEW_REDRAW_LOG path {}: ",
                path.display()
            )),
            "{shown}"
        );
        assert!(shown.ends_with(", redraw logging disabled"), "{shown}");
        assert!(open(None).unwrap().is_none());
    }
}
