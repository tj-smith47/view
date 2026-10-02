//! The picker preview pane's disk-read fallback: plain `std::fs` I/O, never
//! RPC. Only reached for a candidate `EngineHandle::request_preview`
//! answered `loaded: false` for -- nvim has no buffer open for that path, so
//! there is no in-memory content an RPC round trip could disagree with a
//! disk read over (see `docs/picker-preview-wire-capture.md`'s conclusions
//! and the crate's "nvim owns all buffer text" hard rule: this module never
//! reads a path a buffer might also hold open).

use std::path::Path;

/// Reads `path` from disk and splits it into lines, or `None` for a path
/// that does not exist, cannot be read, or is not valid UTF-8 -- the
/// preview pane shows nothing for any of the three rather than a
/// misleading placeholder, and the caller (`Msg::PickerPreviewFile`'s
/// applier) does not need to tell them apart.
///
/// Splits on `\n` and strips a trailing `\r` per line, matching nvim's own
/// line-splitting convention for a file with CRLF endings (`:help
/// 'fileformat'`) so a fallback-read preview's line count agrees with what
/// opening the same file in view would show.
#[must_use]
pub fn read_file(path: &Path) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(
        text.split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
            .collect(),
    )
}

/// How many bytes one window read keeps. A thousand lines of a source file
/// fit many times over; a file with no line breaks (a minified bundle, a
/// binary) would otherwise be held whole as its one line, and the pane
/// cuts a line at its own width anyway.
const WINDOW_BYTES: u64 = 1 << 20;

/// Reads `count` lines of `path` from the 1-based line `first` on, fewer
/// where the file ends first, or `None` for a path that does not exist or
/// cannot be read. The file is streamed: lines before `first` are skipped
/// without being kept, and reading stops after the last line wanted or
/// after [`WINDOW_BYTES`] kept, whichever comes first, the last line cut
/// where the bound falls. Bytes that are not UTF-8 show as U+FFFD, and a
/// line ending in `\r\n` loses both, as nvim splits a CRLF file.
#[must_use]
pub fn read_window(path: &Path, first: u64, count: u64) -> Option<Vec<String>> {
    use std::io::{BufRead, Read};
    let mut reader = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    let mut to_skip = first.saturating_sub(1);
    while to_skip > 0 {
        let buf = reader.fill_buf().ok()?;
        if buf.is_empty() {
            return Some(Vec::new());
        }
        let used = match buf.iter().position(|b| *b == b'\n') {
            Some(at) => {
                to_skip -= 1;
                at + 1
            }
            None => buf.len(),
        };
        reader.consume(used);
    }
    let mut kept = reader.take(WINDOW_BYTES);
    let mut lines = Vec::new();
    let mut line = Vec::new();
    while u64::try_from(lines.len()).is_ok_and(|n| n < count) {
        line.clear();
        if kept.read_until(b'\n', &mut line).ok()? == 0 {
            break;
        }
        let text = line.strip_suffix(b"\n").unwrap_or(&line);
        let text = text.strip_suffix(b"\r").unwrap_or(text);
        lines.push(String::from_utf8_lossy(text).into_owned());
    }
    Some(lines)
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
        let window = read_window(&path, 4500, 1000).expect("a readable file");
        assert_eq!(window.len(), 1000);
        assert_eq!(window[0], "line 4500");
        assert_eq!(window[500], "line 5000");
        assert_eq!(read_window(&path, 9900, 1000).expect("tail").len(), 101);
        assert_eq!(read_window(&path, 20_000, 1000), Some(Vec::new()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_file_with_no_newline_is_read_only_up_to_the_byte_bound() {
        let path = scratch_path("one-line.min.js");
        let len = usize::try_from(WINDOW_BYTES).expect("fits") * 3;
        std::fs::write(&path, "x".repeat(len)).expect("write fixture");
        let window = read_window(&path, 1, 1000).expect("a readable file");
        assert_eq!(window.len(), 1);
        assert_eq!(window[0].len() as u64, WINDOW_BYTES);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bytes_that_are_not_utf8_show_as_replacement_characters() {
        let path = scratch_path("binary.bin");
        std::fs::write(&path, b"\xff\xfeab\r\n\x00c\n").expect("write fixture");
        assert_eq!(
            read_window(&path, 1, 1000),
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

    #[test]
    fn a_missing_path_reads_as_none() {
        let path = scratch_path("does-not-exist.txt");
        assert_eq!(read_file(&path), None);
    }

    #[test]
    fn an_existing_file_splits_into_lines() {
        let path = scratch_path("lines.txt");
        std::fs::write(&path, "one\ntwo\nthree").expect("write scratch file");
        assert_eq!(
            read_file(&path),
            Some(vec![
                "one".to_string(),
                "two".to_string(),
                "three".to_string()
            ])
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_trailing_crlf_line_ending_is_stripped() {
        let path = scratch_path("crlf.txt");
        std::fs::write(&path, "one\r\ntwo\r\n").expect("write scratch file");
        assert_eq!(
            read_file(&path),
            Some(vec!["one".to_string(), "two".to_string(), String::new()])
        );
        let _ = std::fs::remove_file(&path);
    }
}
