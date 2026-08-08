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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

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
