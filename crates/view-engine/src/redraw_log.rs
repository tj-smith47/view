//! `VIEW_REDRAW_LOG=<path>`: one line per layout-shaping redraw event in
//! the order nvim sent it, and per outgoing grid resize request, for
//! reconstructing the op sequence behind a screen whose grids disagree
//! with nvim's.
//!
//! [`init`] reads the variable and opens the file once, at startup, handing
//! it to a [`BackgroundWriter`]; [`finish`] hands that thread the last lines
//! and waits a bounded time for them to reach the file. Unset, every entry
//! point is one `OnceLock` read that finds `None` and formats nothing.
//!
//! The RPC reader writes here, so a batch that finds the queue full is
//! dropped and counted, and the next batch that fits is led by a
//! `dropped <n>` line naming how many lines were lost.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::sync::mpsc::TrySendError;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use view_core::events::UiEvent;
use view_proc::writer::{BackgroundWriter, Finished};

/// How many batches may wait for the disk. A batch is one decoded redraw
/// message, so this is several seconds of a busy screen.
const QUEUED_BATCHES: usize = 1024;

/// The number the next line carries, and the writer thread that owns the
/// file. Numbering and queueing share one lock, so the queue carries lines
/// in the order they are numbered; queueing never waits, so no caller
/// waits on the disk.
struct Log {
    seq: u64,
    /// Takes each text with the number of its first line.
    writer: BackgroundWriter<(u64, String), WriteError>,
    /// Lines numbered since the last text the queue took, and refused by
    /// a full queue.
    dropped: u64,
}

/// A write to the `VIEW_REDRAW_LOG` file that failed. The writer stops at
/// it, so the file holds nothing past the batch that starts at the line it
/// names, and may hold part of that batch.
#[derive(Debug, thiserror::Error)]
#[error(
    "VIEW_REDRAW_LOG write failed at line {line}: {source}, \
     the log stops in the batch that starts there"
)]
pub struct WriteError {
    line: u64,
    source: std::io::Error,
}

/// Why the last lines of the `VIEW_REDRAW_LOG` file may be missing.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FinishError {
    /// A write failed and stopped the writer.
    #[error(transparent)]
    Write(#[from] WriteError),
    /// The writer was still writing when the wait ran out.
    #[error("VIEW_REDRAW_LOG was still writing after {wait:?} at exit, the file may end early")]
    Busy {
        /// How long [`finish`] waited.
        wait: Duration,
    },
}

/// Starts the thread that writes each text it receives to `file`, until the
/// log is closed or a write fails.
fn spawn(file: Box<dyn std::io::Write + Send>) -> std::io::Result<Log> {
    spawn_with(file, QUEUED_BATCHES)
}

fn spawn_with(mut file: Box<dyn std::io::Write + Send>, capacity: usize) -> std::io::Result<Log> {
    let writer = BackgroundWriter::start(
        "redraw-log",
        capacity,
        move |(line, text): (u64, String)| {
            file.write_all(text.as_bytes())
                .map_err(|source| WriteError { line, source })
        },
    )?;
    Ok(Log {
        seq: 0,
        writer,
        dropped: 0,
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

/// Hands the writer thread the last lines and waits up to `wait` for them
/// to reach the file. A writer still busy after that is left to the process
/// exit, since a disk that has not answered by then may never answer. A
/// line written after this is dropped.
///
/// # Errors
///
/// Returns [`FinishError::Write`] with the write that stopped the writer,
/// when one did, and [`FinishError::Busy`] when the writer was still
/// writing after `wait`.
pub fn finish(wait: Duration) -> Result<(), FinishError> {
    sink().map_or(Ok(()), |sink| close(sink, wait))
}

fn close(sink: &Mutex<Log>, wait: Duration) -> Result<(), FinishError> {
    // the wait runs with the lock released, since the RPC reader takes it
    let closed = sink
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .writer
        .close();
    match closed.wait(wait) {
        Finished::Failed(failed) => Err(failed.into()),
        Finished::Busy => Err(FinishError::Busy { wait }),
        _ => Ok(()),
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
        write(sink, || [payload()]);
    }
}

/// One line per event of a decoded batch that sizes, places, clears or
/// writes a grid, and one per `flush`, written to the file at once.
pub(crate) fn batch(events: &[UiEvent]) {
    if let Some(sink) = sink() {
        write(sink, || events.iter().filter_map(describe));
    }
}

/// How many events one drain handed the runtime, after compaction.
pub(crate) fn drained(events: &[UiEvent]) {
    if !events.is_empty() {
        note(|| format!("drain events={}", events.len()));
    }
}

/// Numbers the lines `lines` builds under the log's lock and queues them for
/// the writer thread as one text, which that thread writes to the file in
/// one call. Nothing is built once the writer has stopped.
///
/// A full queue drops the text and counts its lines; the next text the
/// queue takes opens with a `dropped <n>` line.
fn write<I: IntoIterator<Item = String>>(sink: &Mutex<Log>, lines: impl FnOnce() -> I) {
    let mut log = sink.lock().unwrap_or_else(PoisonError::into_inner);
    let Log {
        seq,
        writer,
        dropped,
    } = &mut *log;
    if !writer.is_open() {
        return;
    }
    let first = *seq;
    let lost = (*dropped > 0).then(|| format!("dropped {dropped}"));
    let text = numbered(seq, lost.into_iter().chain(lines()));
    if text.is_empty() {
        return;
    }
    let taken = *seq - first;
    match writer.try_send((first, text)) {
        Ok(()) => *dropped = 0,
        // the refused lines give their numbers back, so the file's numbers
        // stay consecutive and the `dropped` line alone states the loss
        Err(TrySendError::Full(_)) => {
            *dropped += taken - u64::from(*dropped > 0);
            *seq = first;
        }
        Err(TrySendError::Disconnected(_)) => {}
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
    use std::sync::mpsc;
    use view_core::events::GridCell;
    use view_test_support::ScratchDir;

    /// [`super::write`] with lines already built.
    fn write(sink: &Mutex<Log>, lines: impl IntoIterator<Item = String>) {
        super::write(sink, || lines);
    }

    /// A file that says when a write reaches it, and holds that write
    /// until the test lets it through.
    struct Gate {
        file: std::fs::File,
        entered: mpsc::Sender<()>,
        released: mpsc::Receiver<()>,
    }

    impl std::io::Write for Gate {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _ = self.entered.send(());
            let _ = self.released.recv();
            self.file.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.file.flush()
        }
    }

    /// Batches a full queue refuses are counted, and the next batch the
    /// queue takes opens with one line naming how many lines were lost.
    ///
    /// Disconfirm: a blocking send in place of `try_send` never returns
    /// from the third write while the file is held, and resetting the count
    /// on a refusal leaves the `dropped` line out.
    #[test]
    fn a_full_queue_drops_batches_and_says_how_many_lines_it_lost() {
        let dir = ScratchDir::new("redraw-log").unwrap();
        let path = dir.path().join("redraw.log");
        let (entered_tx, entered) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let file = Gate {
            file: std::fs::File::create(&path).unwrap(),
            entered: entered_tx,
            released,
        };
        let sink = Mutex::new(spawn_with(Box::new(file), 1).unwrap());
        write(&sink, ["a".to_string()]);
        entered.recv().unwrap();
        write(&sink, ["b".to_string()]);
        write(&sink, ["c".to_string()]);
        write(&sink, ["d".to_string(), "e".to_string()]);
        assert_eq!(sink.lock().unwrap().dropped, 3);
        drop(release);
        // `b` reaching the file means the thread took it off the queue,
        // which leaves room for `f`
        entered.recv().unwrap();
        write(&sink, ["f".to_string()]);
        close(&sink, patient()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "0 a\n1 b\n2 dropped 3\n3 f\n"
        );
    }

    /// Once the writer has stopped, a line is never built.
    ///
    /// Disconfirm: building the lines before checking the writer calls the
    /// closure.
    #[test]
    fn a_stopped_writer_builds_no_lines() {
        let sink = Mutex::new(spawn(Box::new(std::io::sink())).unwrap());
        let _ = sink.lock().unwrap().writer.close();
        let mut built = false;
        super::write(&sink, || {
            built = true;
            ["b".to_string()]
        });
        assert!(!built, "a line was built for a writer that had stopped");
    }

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
        close(&sink, patient()).unwrap();
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
        file: Box<dyn std::io::Write + Send>,
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
    /// Disconfirm: a blocking send in place of `try_send` on a queue of no
    /// capacity leaves the writes stuck on the held file past the deadline.
    #[test]
    fn a_write_returns_while_the_file_is_held() {
        let dir = ScratchDir::new("redraw-log").unwrap();
        let path = dir.path().join("redraw.log");
        let (release, released) = mpsc::channel();
        let file = Box::new(std::fs::File::create(&path).unwrap());
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
        close(&sink, patient()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "0 a\n1 b\n2 c\n");
    }

    /// `close` gives a writer stuck on the disk `wait`, then returns an
    /// error saying the file may end early, leaving the writer where it is.
    ///
    /// Disconfirm: `close` waiting on `done.recv()` with no timeout, as the
    /// join it replaced did, is still waiting at the deadline, and `close`
    /// mapping `Timeout` to `Ok` answers with no error.
    #[test]
    fn closing_returns_after_its_wait_when_the_file_never_answers() {
        let (release, released) = mpsc::channel();
        let file = Box::new(std::io::sink());
        let sink = Mutex::new(spawn(Box::new(Held { file, released })).unwrap());
        write(&sink, ["a".to_string()]);
        let (done, returned) = mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(|| {
                let shown = close(&sink, Duration::from_millis(50)).map_err(|e| e.to_string());
                let _ = done.send(shown);
            });
            let answer = returned.recv_timeout(patient());
            drop(release);
            assert_eq!(
                answer,
                Ok(Err(
                    "VIEW_REDRAW_LOG was still writing after 50ms at exit, \
                        the file may end early"
                        .to_string()
                )),
                "close waited on the held file, or answered a busy writer with Ok"
            );
        });
    }

    /// A file that accepts `accepted` writes and fails every one after.
    struct Full {
        accepted: usize,
    }

    impl std::io::Write for Full {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.accepted == 0 {
                return Err(std::io::Error::other("no space left"));
            }
            self.accepted -= 1;
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The write that stops the writer comes back from `close`, naming the
    /// first line of the text it failed on.
    ///
    /// Disconfirm: the writer breaking out of its loop without keeping the
    /// error leaves `close` answering `Ok`.
    #[test]
    fn a_failed_write_is_returned_with_its_line_number() {
        let sink = Mutex::new(spawn(Box::new(Full { accepted: 1 })).unwrap());
        write(&sink, ["a".to_string(), "b".to_string()]);
        write(&sink, ["c".to_string()]);
        write(&sink, ["d".to_string()]);
        let shown = close(&sink, patient())
            .expect_err("the failed write was dropped in silence")
            .to_string();
        assert_eq!(
            shown,
            "VIEW_REDRAW_LOG write failed at line 2: no space left, \
             the log stops in the batch that starts there"
        );
    }

    /// How long a test lets `close` wait for a writer that is making
    /// progress.
    fn patient() -> Duration {
        view_test_support::host_deadline(Duration::from_secs(2))
    }

    /// `close` returns only once every line sent before it is in the file.
    ///
    /// Disconfirm: `close` hanging up without waiting for the writer reads
    /// the file before the slow write lands.
    #[test]
    fn closing_waits_for_the_last_lines_to_reach_the_file() {
        let dir = ScratchDir::new("redraw-log").unwrap();
        let path = dir.path().join("redraw.log");
        let file = std::fs::File::create(&path).unwrap();
        let sink = Mutex::new(spawn(Box::new(Slow(file))).unwrap());
        write(&sink, ["a".to_string(), "b".to_string()]);
        write(&sink, ["c".to_string()]);
        close(&sink, patient()).unwrap();
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
