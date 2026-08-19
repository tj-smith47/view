//! Plain-data carrier for the RPC reads a prompt submission has already
//! completed. No I/O, no engine handle, no tokio type lives here: the crate
//! that issues each read (and decodes its own error into an omitted field)
//! populates [`EngineReadSnapshot`], and the crate that turns it into ACP
//! content blocks (`view-ai`) never has a reason to reach for a live RPC
//! call of its own.

use std::path::PathBuf;

/// The current buffer's path and nvim-authoritative text, read the same way
/// the picker preview pane reads a candidate's text -- never the file on
/// disk, so an unsaved edit is what a context block carries.
///
/// `#[non_exhaustive]`: a buffer read is a natural place to grow (filetype,
/// modified flag) without every existing constructor call becoming a
/// compile error the day it does.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentBufferRead {
    pub path: PathBuf,
    pub text: String,
}

impl CurrentBufferRead {
    #[must_use]
    pub fn new(path: PathBuf, text: String) -> Self {
        Self { path, text }
    }
}

/// The active visual selection's text and its `(start_line, end_line)`
/// range. Absent (rather than an empty string) is how "no selection is
/// active" is represented, matching the statusline's own "empty content
/// hides the segment" convention.
///
/// `range`'s two lines are both 1-indexed, matching what the editor shows
/// the user -- the one indexing convention every position field on this
/// snapshot shares, documented once on [`EngineReadSnapshot`] itself.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionRead {
    pub text: String,
    pub range: (u32, u32),
}

impl SelectionRead {
    #[must_use]
    pub fn new(text: String, range: (u32, u32)) -> Self {
        Self { text, range }
    }
}

/// The cursor's buffer-space line and column, read from nvim's own
/// position -- never the painted grid's cursor, which is a viewport-
/// relative screen coordinate rather than a place in the buffer.
///
/// `line`/`col` are both 1-indexed, matching what the editor shows the
/// user (`:help line()`'s own convention) -- the one indexing convention
/// every position field on this snapshot shares, documented once on
/// [`EngineReadSnapshot`] itself. `col` is still a byte offset, not a
/// character offset; only the 0-vs-1 origin is normalized here, and only
/// `view-engine`'s own reply decoders perform that normalization -- this
/// type carries no wire-shape knowledge of its own.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorRead {
    pub line: u32,
    pub col: u32,
}

impl CursorRead {
    #[must_use]
    pub fn new(line: u32, col: u32) -> Self {
        Self { line, col }
    }
}

/// One diagnostic as `vim.diagnostic.get(0)` reports it, with `line`/`col`
/// renormalized from that API's own 0-indexed wire convention onto the
/// 1-indexed one every [`EngineReadSnapshot`] position field shares (see
/// [`CursorRead`]'s own doc) -- `vim.diagnostic.get`'s raw 0-indexed values
/// never reach this type.
///
/// `#[non_exhaustive]`: the report this models is a 1:1 mapping onto
/// nvim's own diagnostic entries, which carry more than these four fields
/// (`end_lnum`, `source`, `code` among them) -- of every type here, this is
/// the one most likely to grow.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticEntry {
    pub line: u32,
    pub col: u32,
    pub severity: DiagnosticSeverity,
    pub message: String,
}

impl DiagnosticEntry {
    #[must_use]
    pub fn new(line: u32, col: u32, severity: DiagnosticSeverity, message: String) -> Self {
        Self {
            line,
            col,
            severity,
            message,
        }
    }
}

/// Closed: nvim's `vim.diagnostic.severity` table has exactly these four
/// levels, so a mapping of it can be total rather than defaulting an
/// unrecognized fifth value to one of these and misreporting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
    Hint,
}

/// One entry as `getqflist()` reports it. `line`/`col` need no
/// renormalization to reach [`EngineReadSnapshot`]'s shared 1-indexed
/// convention (see [`CursorRead`]'s own doc): `getqflist()` is already
/// 1-indexed on the wire, unlike [`DiagnosticEntry`]'s source.
///
/// `#[non_exhaustive]` for the same reason as [`DiagnosticEntry`]: nvim's
/// own quickfix entries carry more than these four fields (`bufnr`, `type`,
/// `valid` among them).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickfixEntry {
    pub path: PathBuf,
    pub line: u32,
    pub col: u32,
    pub text: String,
}

impl QuickfixEntry {
    #[must_use]
    pub fn new(path: PathBuf, line: u32, col: u32, text: String) -> Self {
        Self {
            path,
            line,
            col,
            text,
        }
    }
}

/// Everything a prompt submission has already read from the engine, by the
/// time context assembly runs. A read that errored, or that never ran
/// because nothing was there to read (no selection, an empty quickfix
/// list), is represented by the absent/empty state of its own field --
/// there is no separate error variant, because the consumer's response to
/// "this read failed" and "there was nothing here" is identical: omit the
/// block.
///
/// Every line/column position anywhere in this snapshot -- `cursor`,
/// `selection`, `diagnostics`, `quickfix` alike -- is 1-indexed, matching
/// what the editor shows the user. Each engine source has its own native wire
/// convention (`nvim_win_get_cursor` is 0-indexed columns,
/// `vim.diagnostic.get` is 0-indexed both, `getqflist` is already
/// 1-indexed), but by the time a value reaches this type it has been
/// renormalized onto the one shared convention -- deliberately, so the
/// same physical buffer position renders identically in a prompt's context
/// regardless of which of the three reads reported it. `view-engine`'s own
/// reply decoders own that renormalization; nothing here or downstream
/// (`view-ai`'s prose renderers included) re-derives or re-adjusts an
/// index.
///
/// `#[non_exhaustive]`: a future context provider (open buffers, a git
/// diff) adds a field here. Built from `Self::default()` plus the `with_*`
/// setters below, the same builder shape `AgentLaunch` already uses for a
/// `#[non_exhaustive]` struct another crate constructs.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineReadSnapshot {
    pub current_buffer: Option<CurrentBufferRead>,
    pub selection: Option<SelectionRead>,
    pub cursor: Option<CursorRead>,
    pub diagnostics: Vec<DiagnosticEntry>,
    pub quickfix: Vec<QuickfixEntry>,
}

impl EngineReadSnapshot {
    #[must_use]
    pub fn with_current_buffer(mut self, buffer: CurrentBufferRead) -> Self {
        self.current_buffer = Some(buffer);
        self
    }

    #[must_use]
    pub fn with_selection(mut self, selection: SelectionRead) -> Self {
        self.selection = Some(selection);
        self
    }

    #[must_use]
    pub fn with_cursor(mut self, cursor: CursorRead) -> Self {
        self.cursor = Some(cursor);
        self
    }

    #[must_use]
    pub fn with_diagnostics(mut self, diagnostics: Vec<DiagnosticEntry>) -> Self {
        self.diagnostics = diagnostics;
        self
    }

    #[must_use]
    pub fn with_quickfix(mut self, quickfix: Vec<QuickfixEntry>) -> Self {
        self.quickfix = quickfix;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default snapshot represents "nothing read yet, or every read
    /// came back empty" -- every field absent/empty, never a placeholder
    /// value a consumer could mistake for a real read.
    #[test]
    fn default_snapshot_has_every_field_absent_or_empty() {
        let snapshot = EngineReadSnapshot::default();

        assert_eq!(snapshot.current_buffer, None);
        assert_eq!(snapshot.selection, None);
        assert_eq!(snapshot.cursor, None);
        assert!(snapshot.diagnostics.is_empty());
        assert!(snapshot.quickfix.is_empty());
    }
}
