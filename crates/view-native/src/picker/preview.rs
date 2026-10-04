//! The picker preview pane's disk-read fallback: plain `std::fs` I/O, never
//! RPC. Only reached for a candidate `EngineHandle::request_preview`
//! answered `loaded: false` for. nvim has no buffer open for that path, so
//! there is no in-memory content an RPC round trip could disagree with a
//! disk read over (see `docs/picker-preview-wire-capture.md`'s conclusions
//! and the crate's "nvim owns all buffer text" hard rule: this module never
//! reads a path a buffer might also hold open).

use std::io::BufRead;
use std::path::Path;

use view_core::native::picker::PREVIEW_LINE_BYTES;

/// Reads `count` lines of `path` from the 1-based line `first` on, fewer
/// where the file ends first, or `None` for a path that does not exist or
/// cannot be read, for a path that is no regular file (a named pipe, a
/// device, a directory), or once `superseded` answers true. The file is
/// streamed: lines before `first` are skipped without being kept, and each
/// line kept is cut at [`PREVIEW_LINE_BYTES`] on a character boundary, the
/// rest of it skipped the same way. A short answer therefore always means
/// the file ended. `superseded` is asked once per buffer of the file
/// skipped, so a read nobody waits for any more stops wherever it is in
/// the file.
/// Bytes that are not UTF-8 show as U+FFFD, and a line ending in `\r\n`
/// loses both, as nvim splits a CRLF file (`:help 'fileformat'`), so the
/// preview's lines agree with what opening the file in view shows. The
/// pane shows nothing for `None`, and the caller
/// (`Msg::PickerPreviewFile`'s applier) does not need to tell its causes
/// apart.
#[must_use]
pub fn read_window(
    path: &Path,
    first: u64,
    count: u64,
    superseded: impl Fn() -> bool,
) -> Option<Vec<String>> {
    // opening a named pipe completes the open of a writer waiting on it, and
    // opening a device runs its driver, so neither is opened at all
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    // a path swapped for a pipe or a device after the check above would
    // block a plain open with no end, before `superseded` is ever asked;
    // O_NONBLOCK has no effect on reading a regular file, and O_NOCTTY
    // keeps a swapped-in tty from becoming the controlling terminal
    #[cfg(unix)]
    let file = std::fs::File::from(
        rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::NOCTTY
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .ok()?,
    );
    #[cfg(not(unix))]
    let file = std::fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut reader = std::io::BufReader::new(file);
    let mut to_skip = first.saturating_sub(1);
    while to_skip > 0 {
        if superseded() {
            return None;
        }
        let buf = reader.fill_buf().ok()?;
        if buf.is_empty() {
            return Some(Vec::new());
        }
        let mut used = buf.len();
        for (at, _) in buf.iter().enumerate().filter(|(_, b)| **b == b'\n') {
            to_skip -= 1;
            if to_skip == 0 {
                used = at + 1;
                break;
            }
        }
        reader.consume(used);
    }
    let cap = usize::try_from(PREVIEW_LINE_BYTES).unwrap_or(usize::MAX);
    let mut lines = Vec::new();
    let mut line = Vec::new();
    while u64::try_from(lines.len()).is_ok_and(|n| n < count) {
        if !read_capped_line(&mut reader, cap, &mut line, &superseded)? {
            break;
        }
        lines.push(String::from_utf8_lossy(&line).into_owned());
    }
    Some(lines)
}

/// Reads one line of `reader` into `line`, its `\n` or `\r\n` dropped and
/// its text cut at `cap` bytes on a character boundary, with the rest of
/// the line consumed and never held. `Some(false)` at the end of the file,
/// `None` on a read error or once `superseded` answers true.
fn read_capped_line(
    reader: &mut impl BufRead,
    cap: usize,
    line: &mut Vec<u8>,
    superseded: &impl Fn() -> bool,
) -> Option<bool> {
    line.clear();
    let mut read_any = false;
    let mut cut = false;
    loop {
        let buf = reader.fill_buf().ok()?;
        if buf.is_empty() {
            break;
        }
        read_any = true;
        let newline = buf.iter().position(|b| *b == b'\n');
        let text = &buf[..newline.unwrap_or(buf.len())];
        let room = cap.saturating_sub(line.len());
        line.extend_from_slice(&text[..text.len().min(room)]);
        cut |= text.len() > room;
        let used = newline.map_or(buf.len(), |at| at + 1);
        reader.consume(used);
        if newline.is_some() {
            break;
        }
        if superseded() {
            return None;
        }
    }
    if cut {
        line.truncate(char_floor(line));
    } else if line.last() == Some(&b'\r') {
        line.pop();
    }
    Some(read_any)
}

/// The length of `bytes` without a trailing character the cut left
/// incomplete.
fn char_floor(bytes: &[u8]) -> usize {
    let lead = bytes
        .iter()
        .rposition(|b| b & 0b1100_0000 != 0b1000_0000)
        .filter(|at| bytes.len() - at <= 4);
    let Some(at) = lead else {
        return bytes.len();
    };
    let wants = match bytes[at] {
        b if b >= 0b1111_0000 => 4,
        b if b >= 0b1110_0000 => 3,
        b if b >= 0b1100_0000 => 2,
        _ => 1,
    };
    if bytes.len() - at < wants {
        at
    } else {
        bytes.len()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_window_read_returns_only_the_lines_asked_for() {
        let path = scratch_path("long.txt");
        let text: String = (1..=10_000).map(|n| format!("line {n}\r\n")).collect();
        std::fs::write(&path, text).expect("write fixture");
        let window = read_window(&path, 4500, 1000, || false).expect("a readable file");
        assert_eq!(window.len(), 1000);
        assert_eq!(window[0], "line 4500");
        assert_eq!(window[500], "line 5000");
        assert_eq!(
            read_window(&path, 9900, 1000, || false)
                .expect("tail")
                .len(),
            101
        );
        assert_eq!(read_window(&path, 20_000, 1000, || false), Some(Vec::new()));
        let _ = std::fs::remove_file(&path);
    }

    fn cap() -> usize {
        usize::try_from(PREVIEW_LINE_BYTES).expect("fits")
    }

    /// Writes 2000 lines of 4 KiB each, `MATCH` starting line 1500, the
    /// 500 lines above it more than a megabyte.
    fn long_lines_fixture(name: &str) -> std::path::PathBuf {
        let path = scratch_path(name);
        let text: String = (1..=2000)
            .map(|n| {
                let head = if n == 1500 {
                    "MATCH 1500 ".to_string()
                } else {
                    format!("line {n} ")
                };
                format!("{head:a<4096}\n")
            })
            .collect();
        std::fs::write(&path, text).expect("write fixture");
        path
    }

    #[test]
    fn a_match_below_lines_holding_megabytes_is_in_its_window() {
        let path = long_lines_fixture("long-lines.jsonl");
        let window = read_window(&path, 1000, 1000, || false).expect("a readable file");
        assert_eq!(window.len(), 1000, "a full window, not the file's end");
        assert!(
            window[500].starts_with("MATCH 1500 "),
            "{:.20}",
            window[500]
        );
        assert!(window.iter().all(|line| line.len() <= cap()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_match_below_lines_holding_megabytes_is_marked() {
        use view_core::native::picker::{PickerItem, PickerState, Source, PREVIEW_WINDOW_LINES};
        let path = long_lines_fixture("long-lines-marked.jsonl");
        let dir = path.parent().expect("a scratch dir").to_path_buf();
        let mut state = PickerState::open(Source::LiveGrep { root: dir });
        let gen = state.generation();
        let name = "long-lines-marked.jsonl";
        state.apply_results(gen, vec![PickerItem::grep_match(name, 1500, "MATCH")]);
        let (preview_gen, wanted) = state.refresh_preview().expect("a selection");
        let lines = read_window(
            Path::new(&wanted),
            state.preview_first_line(),
            PREVIEW_WINDOW_LINES,
            || false,
        );
        state.apply_preview(preview_gen, lines.expect("a readable file"));
        let view = state.view();
        let (rows, marked) = view.preview_window(30);
        assert!(rows[marked.expect("the match is marked")].starts_with("MATCH 1500 "));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_line_longer_than_the_cap_is_cut_on_a_character_boundary() {
        for wide in ['é', '€', '😀'] {
            let path = scratch_path("wide.txt");
            let head = "x".repeat(cap() - 1);
            std::fs::write(&path, format!("{head}{wide}tail\r\nnext\r\n")).expect("write fixture");
            assert_eq!(
                read_window(&path, 1, 1000, || false),
                Some(vec![head, "next".to_string()]),
                "{wide} straddles the cut"
            );
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn a_line_exactly_the_cap_long_is_kept_whole() {
        let path = scratch_path("exact.txt");
        let line = format!("{}é", "x".repeat(cap() - 2));
        std::fs::write(&path, format!("{line}\r\nnext")).expect("write fixture");
        assert_eq!(
            read_window(&path, 1, 1000, || false),
            Some(vec![line, "next".to_string()])
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_file_with_no_newline_yields_one_capped_line() {
        let path = scratch_path("one-line.min.js");
        std::fs::write(&path, "x".repeat(3 << 20)).expect("write fixture");
        let window = read_window(&path, 1, 1000, || false).expect("a readable file");
        assert_eq!(window, vec!["x".repeat(cap())]);
        let _ = std::fs::remove_file(&path);
    }

    /// A `superseded` that answers true from its second call on.
    fn superseded_after_one_look() -> impl Fn() -> bool {
        let calls = std::cell::Cell::new(0_u32);
        move || {
            calls.set(calls.get() + 1);
            calls.get() > 1
        }
    }

    #[test]
    fn a_superseded_read_stops_while_skipping_to_its_window() {
        let path = scratch_path("skip.txt");
        let text: String = (1..=100_000).map(|n| format!("line {n}\n")).collect();
        std::fs::write(&path, text).expect("write fixture");
        assert_eq!(
            read_window(&path, 200_000, 1000, || false),
            Some(Vec::new())
        );
        assert_eq!(
            read_window(&path, 200_000, 1000, superseded_after_one_look()),
            None
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_superseded_read_stops_while_skipping_the_rest_of_a_line() {
        let path = scratch_path("one-line-superseded.min.js");
        std::fs::write(&path, "x".repeat(3 << 20)).expect("write fixture");
        assert_eq!(
            read_window(&path, 1, 1000, superseded_after_one_look()),
            None
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bytes_that_are_not_utf8_show_as_replacement_characters() {
        let path = scratch_path("binary.bin");
        std::fs::write(&path, b"\xff\xfeab\r\n\x00c\n").expect("write fixture");
        assert_eq!(
            read_window(&path, 1, 1000, || false),
            Some(vec!["\u{FFFD}\u{FFFD}ab".to_string(), "\0c".to_string()])
        );
        let _ = std::fs::remove_file(&path);
    }

    fn scratch_path(name: &str) -> std::path::PathBuf {
        let nonce = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        );
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("picker-preview-{nonce}"));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir.join(name)
    }

    /// A named pipe no process writes to reads as `None` at once, leaving
    /// no thread parked in its open.
    #[cfg(unix)]
    #[test]
    fn a_named_pipe_with_no_writer_reads_as_none() {
        let _watchdog = view_test_support::watchdog();
        let fifo = scratch_path("pipe");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo is on PATH for this test's own setup");
        assert!(made.success(), "mkfifo {fifo:?} failed");
        let (tx, rx) = std::sync::mpsc::channel();
        let reading = fifo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(read_window(&reading, 1, 1000, || false));
        });
        let read = rx.recv_timeout(view_test_support::host_deadline(
            std::time::Duration::from_secs(5),
        ));
        // a writer arriving lets a read still parked in its open return, so
        // no thread outlives the test
        let release = read
            .is_err()
            .then(|| std::fs::OpenOptions::new().write(true).open(&fifo));
        drop(release);
        let _ = std::fs::remove_file(&fifo);
        assert_eq!(
            read.expect("the read returned without a writer on the pipe"),
            None
        );
    }

    /// Previewing a named pipe never opens it, so a writer blocked in its
    /// own open stays blocked: opening the read end would complete that
    /// open, and the preview closing it again breaks the writer's pipe.
    #[cfg(unix)]
    #[test]
    fn a_writer_waiting_on_a_named_pipe_is_left_waiting() {
        use std::io::Write;
        let _watchdog = view_test_support::watchdog();
        // nothing shows whether the writer has reached its open before the
        // preview runs, so one round catches a preview that opens the pipe
        // only about half the time; each round is independent
        for round in 0..32 {
            let fifo = scratch_path("pipe");
            let made = std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .expect("mkfifo is on PATH for this test's own setup");
            assert!(made.success(), "mkfifo {fifo:?} failed");
            let (entering, entered) = std::sync::mpsc::channel();
            let (tx, rx) = std::sync::mpsc::channel();
            let writing = fifo.clone();
            std::thread::spawn(move || {
                let _ = entering.send(());
                let written = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&writing)
                    .and_then(|mut file| file.write_all(b"x"));
                let _ = tx.send(written.map_err(|e| e.kind()));
            });
            entered.recv().expect("the writer thread started");
            assert_eq!(read_window(&fifo, 1, 1000, || false), None);
            let early = rx.try_recv().ok();
            // a reader of the test's own completes a writer still parked in
            // its open, and holding it open until the write lands keeps that
            // write from failing for want of a reader
            let release = rustix::fs::open(
                &fifo,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::NONBLOCK
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .expect("open the test's own reader");
            let written = early.is_none().then(|| {
                rx.recv_timeout(view_test_support::host_deadline(
                    std::time::Duration::from_secs(5),
                ))
                .expect("the released writer reports")
            });
            drop(release);
            let _ = std::fs::remove_file(&fifo);
            assert_eq!(
                early, None,
                "round {round}: the writer's open returned during the preview"
            );
            assert_eq!(
                written,
                Some(Ok(())),
                "round {round}: the writer's open returned before the test's reader existed"
            );
        }
    }

    #[test]
    fn a_missing_path_reads_as_none() {
        let path = scratch_path("does-not-exist.txt");
        assert_eq!(read_window(&path, 1, 1000, || false), None);
    }

    #[test]
    fn a_last_line_with_no_newline_is_kept() {
        let path = scratch_path("lines.txt");
        std::fs::write(&path, "one\ntwo\nthree").expect("write scratch file");
        assert_eq!(
            read_window(&path, 1, 1000, || false),
            Some(vec![
                "one".to_string(),
                "two".to_string(),
                "three".to_string()
            ])
        );
        let _ = std::fs::remove_file(&path);
    }
}
