//! `VIEW_REDRAW_LOG=<path>`: one line per layout-shaping redraw event in
//! the order nvim sent it, and per outgoing grid resize request, for
//! reconstructing the op sequence behind a screen whose grids disagree
//! with nvim's.
//!
//! [`init`] reads the variable and opens the file once, at startup, handing
//! it to a writer thread of its own; [`finish`] hands that thread the last
//! lines and waits for them to reach the file. Unset, every entry point is
//! one `OnceLock` read that finds `None` and formats nothing.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::thread::JoinHandle;

use view_core::events::UiEvent;

/// The number the next line carries, and the channel to the thread that
/// owns the file. Numbering and sending share one lock, so the channel
/// carries lines in the order they are numbered; the send never blocks, so
/// no caller waits on the disk.
struct Log {
    seq: u64,
    lines: Option<Sender<String>>,
    writer: Option<JoinHandle<()>>,
}

/// Starts the thread that writes each text it receives to `file`, until
/// every sender is gone or a write fails.
fn spawn(mut file: Box<dyn std::io::Write + Send>) -> std::io::Result<Log> {
    let (lines, texts) = mpsc::channel::<String>();
    let writer = std::thread::Builder::new()
        .name("redraw-log".into())
        .spawn(move || {
            for text in texts {
                if file.write_all(text.as_bytes()).is_err() {
                    break;
                }
            }
        })?;
    Ok(Log {
        seq: 0,
        lines: Some(lines),
        writer: Some(writer),
    })
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
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|file| spawn(Box::new(file)))
        .map(|log| Some(Mutex::new(log)))
        .map_err(|source| OpenError { path, source })
}

/// Hands the writer thread the last lines and waits until they reach the
/// file. A line written after this is dropped.
pub fn finish() {
    if let Some(sink) = sink() {
        close(sink);
    }
}

fn close(sink: &Mutex<Log>) {
    let writer = {
        let mut log = sink.lock().unwrap_or_else(PoisonError::into_inner);
        log.lines = None;
        log.writer.take()
    };
    if let Some(writer) = writer {
        let _ = writer.join();
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

/// Numbers `lines` under the log's lock and sends them to the writer
/// thread as one text, which that thread writes to the file in one call.
fn write(sink: &Mutex<Log>, lines: impl IntoIterator<Item = String>) {
    let mut log = sink.lock().unwrap_or_else(PoisonError::into_inner);
    let text = numbered(&mut log.seq, lines);
    if let Some(lines) = log.lines.as_ref().filter(|_| !text.is_empty()) {
        let _ = lines.send(text);
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
        let sink = Mutex::new(spawn(Box::new(writes.clone())).unwrap());
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
        close(&sink);
        assert_eq!(
            *writes.0.lock().unwrap(),
            vec![
                "0 grid_resize grid=2 width=40 height=20\n1 flush\n".to_string(),
                "2 drain events=3\n".to_string(),
            ]
        );
        assert_eq!(numbered(&mut 7, ["a".into(), "b".into()]), "7 a\n8 b\n");
    }

    /// A file whose first write waits until the test lets it through.
    struct Held {
        file: std::fs::File,
        released: mpsc::Receiver<()>,
    }

    impl std::io::Write for Held {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _ = self.released.recv();
            self.file.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.file.flush()
        }
    }

    /// A file that takes a while to accept each write, as a slow disk does.
    struct Slow(std::fs::File);

    impl std::io::Write for Slow {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            std::thread::sleep(std::time::Duration::from_millis(50));
            self.0.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.0.flush()
        }
    }

    /// `write` returns while the file has not accepted a byte, and what
    /// it sent reaches the file in number order once the file does.
    ///
    /// Disconfirm: writing to the file on the caller's thread leaves the
    /// writes stuck on the held file past the deadline.
    #[test]
    fn a_write_returns_while_the_file_is_held() {
        let dir = ScratchDir::new("redraw-log").unwrap();
        let path = dir.path().join("redraw.log");
        let (release, released) = mpsc::channel();
        let file = std::fs::File::create(&path).unwrap();
        let sink = Mutex::new(spawn(Box::new(Held { file, released })).unwrap());
        let (done, returned) = mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(|| {
                write(&sink, ["a".to_string()]);
                write(&sink, ["b".to_string(), "c".to_string()]);
                let _ = done.send(());
            });
            let in_time = returned
                .recv_timeout(view_test_support::host_deadline(
                    std::time::Duration::from_secs(2),
                ))
                .is_ok();
            let unwritten = std::fs::read_to_string(&path).unwrap();
            drop(release);
            assert!(in_time, "a write waited on the held file");
            assert_eq!(unwritten, "", "the file took a write while held");
        });
        close(&sink);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "0 a\n1 b\n2 c\n");
    }

    /// `close` returns only once every line sent before it is in the file.
    ///
    /// Disconfirm: `close` hanging up without joining the writer reads the
    /// file before the slow write lands.
    #[test]
    fn closing_waits_for_the_last_lines_to_reach_the_file() {
        let dir = ScratchDir::new("redraw-log").unwrap();
        let path = dir.path().join("redraw.log");
        let file = std::fs::File::create(&path).unwrap();
        let sink = Mutex::new(spawn(Box::new(Slow(file))).unwrap());
        write(&sink, ["a".to_string(), "b".to_string()]);
        write(&sink, ["c".to_string()]);
        close(&sink);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "0 a\n1 b\n2 c\n");
        write(&sink, ["d".to_string()]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "0 a\n1 b\n2 c\n");
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
