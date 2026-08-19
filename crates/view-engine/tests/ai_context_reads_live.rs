//! Live-nvim proof of the engine-side executors for the four context reads
//! `RpcCall` declared (`ReadCurrentBufferText`, `ReadCursorContext`,
//! `ReadDiagnosticEntries`, `ReadQuickfixEntries`): each
//! `EngineHandle::read_*` method issues its own `nvim_exec_lua` chunk and
//! decodes the reply into the `view_core::native::ai_context` type
//! `EngineReadSnapshot`'s builders take. See
//! `docs/ai-context-reads-wire-capture.md` for the wire evidence these
//! assertions mirror.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use view_core::native::ai_context::DiagnosticSeverity;
use view_engine::process::{Engine, EngineConfig};

/// Spawns an isolated engine with a UI attached, the same load-bearing
/// attach `buf_set_text_live.rs`'s own `spawn` documents: without it,
/// `nvim_input`-driven mode changes (entering visual mode, for the
/// selection tests below) are never actually processed by nvim's main
/// loop before a follow-up synchronous request reads the resulting state.
fn spawn() -> Engine {
    let engine = Engine::spawn(EngineConfig::isolated()).expect("spawn engine");
    engine.handle.ui_attach(80, 24).expect("attach ui");
    engine
}

fn set_lines(engine: &Engine, lines: &[&str]) {
    engine
        .handle
        .request(
            "nvim_buf_set_lines",
            vec![
                rmpv::Value::from(0),
                rmpv::Value::from(0),
                rmpv::Value::from(-1),
                rmpv::Value::from(false),
                rmpv::Value::Array(lines.iter().map(|l| rmpv::Value::from(*l)).collect()),
            ],
        )
        .expect("set buffer lines");
}

/// An unnamed scratch buffer's current text reads back as an empty path
/// and the joined line content, never the file on disk (there is none).
#[test]
fn read_current_buffer_text_reads_the_unnamed_scratch_buffer() {
    let engine = spawn();
    set_lines(&engine, &["alpha", "beta"]);

    let read = engine
        .handle
        .read_current_buffer_text()
        .expect("read current buffer text");

    assert_eq!(read.path, PathBuf::new());
    assert_eq!(read.text, "alpha\nbeta");
}

/// A named, edited buffer's text reads back nvim's own in-memory content
/// (the unsaved edit), never a stale on-disk read -- the same
/// nvim-owns-buffer-text contract the picker preview pane's read already
/// proves for `PreviewBuffer`.
#[test]
fn read_current_buffer_text_reads_modified_content_over_a_named_buffer() {
    let engine = spawn();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp");
    std::fs::create_dir_all(&root).expect("create scratch root");
    let path = root.join(format!("ai-context-read-{}.txt", std::process::id()));
    std::fs::write(&path, "on disk\n").expect("seed on-disk file");
    let path_str = path.to_string_lossy().into_owned();

    engine
        .handle
        .open_file(&path_str)
        .expect("open the seeded file");
    set_lines(&engine, &["unsaved edit"]);

    let read = engine
        .handle
        .read_current_buffer_text()
        .expect("read current buffer text");

    assert_eq!(read.text, "unsaved edit");
    assert!(
        read.path.ends_with(path.file_name().expect("file name")),
        "expected {:?} to end with the opened file's name",
        read.path
    );
}

/// With no visual selection active, the cursor read carries
/// `nvim_win_get_cursor`'s position, renormalized to `EngineReadSnapshot`'s
/// shared 1-indexed convention (col 0 on the wire reads back as 1 here),
/// and no selection.
#[test]
fn read_cursor_context_with_no_active_selection() {
    let engine = spawn();
    set_lines(&engine, &["hello world", "second line"]);

    let (cursor, selection) = engine
        .handle
        .read_cursor_context()
        .expect("read cursor context");

    assert_eq!((cursor.line, cursor.col), (1, 1));
    assert_eq!(selection, None);
}

/// While a forward charwise visual selection is active, the read carries
/// both the cursor and the selected text plus its `(start_line, end_line)`
/// range.
#[test]
fn read_cursor_context_with_an_active_forward_selection() {
    let engine = spawn();
    set_lines(&engine, &["hello world", "second line"]);

    engine.handle.input("gg0v").expect("enter visual mode");
    engine.handle.input("llll").expect("extend selection");

    let (cursor, selection) = engine
        .handle
        .read_cursor_context()
        .expect("read cursor context");

    assert_eq!(
        cursor.col, 5,
        "wire col 4 renormalized to the 1-indexed convention"
    );
    let selection = selection.expect("a selection is active");
    assert_eq!(selection.text, "hello");
    assert_eq!(selection.range, (1, 1));
}

/// A backward selection (anchor after the cursor) is reported with its
/// endpoints reordered so `range.0 <= range.1` and `text` reads forward,
/// regardless of which direction the user actually selected in.
#[test]
fn read_cursor_context_with_an_active_backward_selection() {
    let engine = spawn();
    set_lines(&engine, &["hello world", "second line"]);

    engine
        .handle
        .input("gg$v")
        .expect("enter visual mode at eol");
    engine.handle.input("0").expect("select backward to col 0");

    let (_cursor, selection) = engine
        .handle
        .read_cursor_context()
        .expect("read cursor context");

    let selection = selection.expect("a selection is active");
    assert_eq!(selection.text, "hello world");
    assert_eq!(selection.range, (1, 1));
}

/// Leaving visual mode (`<Esc>`) retires the selection from this read even
/// though nvim's own `'<`/`'>` marks stay set to the exited selection --
/// this read keys off live mode, not stale marks.
#[test]
fn read_cursor_context_after_leaving_visual_mode_reports_no_selection() {
    let engine = spawn();
    set_lines(&engine, &["hello world"]);

    engine.handle.input("gg0v").expect("enter visual mode");
    engine.handle.input("llll").expect("extend selection");
    engine.handle.input("\x1b").expect("leave visual mode");

    let (_cursor, selection) = engine
        .handle
        .read_cursor_context()
        .expect("read cursor context");

    assert_eq!(selection, None);
}

/// A charwise selection ending on a multi-byte character reads its full
/// text, never `None`: `"aé bc"`'s `é` is a 2-byte UTF-8 sequence, and
/// `getpos('.')`'s own byte column at that position is the FIRST byte of
/// `é`, not the exclusive end `nvim_buf_get_text` needs. Fixed regression
/// coverage for the bug where passing that raw column straight through as
/// the exclusive end sliced off `é`'s second byte, producing a string that
/// failed UTF-8 validation and silently decoded as no selection at all.
#[test]
fn read_cursor_context_selection_ending_on_a_multibyte_character_reads_the_full_character() {
    let engine = spawn();
    set_lines(&engine, &["a\u{e9} bc"]);

    engine.handle.input("gg0v").expect("enter visual mode");
    engine.handle.input("l").expect("extend selection onto é");

    let (_cursor, selection) = engine
        .handle
        .read_cursor_context()
        .expect("read cursor context");

    let selection = selection.expect("a selection is active, not silently dropped");
    assert_eq!(selection.text, "a\u{e9}");
}

/// A linewise (`V`) selection reads every full line it spans, ignoring both
/// endpoints' columns entirely -- linewise selections have no columns, by
/// nvim's own definition of the mode.
#[test]
fn read_cursor_context_with_a_linewise_selection_reads_whole_lines() {
    let engine = spawn();
    set_lines(&engine, &["alpha", "beta", "gamma"]);

    engine
        .handle
        .input("gg0V")
        .expect("enter linewise visual mode");
    engine.handle.input("j").expect("extend to the next line");

    let (_cursor, selection) = engine
        .handle
        .read_cursor_context()
        .expect("read cursor context");

    let selection = selection.expect("a selection is active");
    assert_eq!(selection.text, "alpha\nbeta");
    assert_eq!(selection.range, (1, 2));
}

/// A blockwise (`<C-v>`) selection reads the column-range rectangle it
/// spans, per line, clamped to each line's own length -- never the charwise
/// span between the two endpoints, which would pull in text the block never
/// actually covers.
#[test]
fn read_cursor_context_with_a_blockwise_selection_reads_the_rectangle() {
    let engine = spawn();
    set_lines(&engine, &["alpha", "beta", "gamma"]);

    engine
        .handle
        .input("gg0<C-v>")
        .expect("enter blockwise visual mode");
    engine.handle.input("jl").expect("extend the block");

    let (_cursor, selection) = engine
        .handle
        .read_cursor_context()
        .expect("read cursor context");

    let selection = selection.expect("a selection is active");
    assert_eq!(selection.text, "al\nbe");
    assert_eq!(selection.range, (1, 2));
}

/// A blockwise selection's rectangle is bounded by SCREEN columns
/// (`virtcol`), not byte columns: a multi-byte character on the block's
/// first row must not split the rectangle mid-character, per
/// `docs/ai-context-reads-wire-capture.md`'s "Fix round 2" capture, matched
/// here against nvim's own yank (`normal! y` + `getreg`) as the oracle.
#[test]
fn read_cursor_context_with_a_blockwise_selection_over_a_multibyte_character() {
    let engine = spawn();
    set_lines(&engine, &["éxyz", "abcd"]);

    engine
        .handle
        .input("gg0<C-v>")
        .expect("enter blockwise visual mode");
    engine.handle.input("jl").expect("extend the block");

    let (_cursor, selection) = engine
        .handle
        .read_cursor_context()
        .expect("read cursor context");

    let selection = selection.expect("a selection is active");
    assert_eq!(selection.text, "éx\nab");
    assert_eq!(selection.range, (1, 2));
}

/// The same screen-column rectangle, this time with the multi-byte
/// character inside the block rather than at its very start -- confirms
/// the per-line byte conversion holds for every row, not just the first.
#[test]
fn read_cursor_context_with_a_blockwise_selection_anchored_on_a_multibyte_character() {
    let engine = spawn();
    set_lines(&engine, &["aébc", "wxyz"]);

    engine
        .handle
        .input("gg0<C-v>")
        .expect("enter blockwise visual mode");
    engine.handle.input("jll").expect("extend the block");

    let (_cursor, selection) = engine
        .handle
        .read_cursor_context()
        .expect("read cursor context");

    let selection = selection.expect("a selection is active");
    assert_eq!(selection.text, "aéb\nwxy");
    assert_eq!(selection.range, (1, 2));
}

/// A `$`-block (`<C-v>` extended with `$`) extends every row to its own
/// actual end rather than the shared screen-column upper bound -- nvim's
/// `getcurpos()` `curswant` field (`MAXCOL`, `2147483647`) is what marks
/// this case, and it must not be read as an ordinary rectangle whose bound
/// happens to reach the longer row's length.
#[test]
fn read_cursor_context_with_a_dollar_blockwise_selection_reads_every_line_to_its_own_end() {
    let engine = spawn();
    set_lines(&engine, &["alpha", "be"]);

    engine
        .handle
        .input("gg0<C-v>")
        .expect("enter blockwise visual mode");
    engine
        .handle
        .input("j$")
        .expect("extend the block to a dollar-block");

    let (_cursor, selection) = engine
        .handle
        .read_cursor_context()
        .expect("read cursor context");

    let selection = selection.expect("a selection is active");
    assert_eq!(selection.text, "alpha\nbe");
    assert_eq!(selection.range, (1, 2));
}

/// An ordinary (non-`$`) block whose shared screen-column upper bound
/// exceeds one row's own length still yanks that row in full, from the low
/// column to its own end -- distinct from the `$`-block case above, and
/// already covered by round 1's `math.min(hi0, #line)` clamp once `hi0` is
/// derived from `virtcol2col` instead of a raw byte column.
#[test]
fn read_cursor_context_with_a_blockwise_selection_where_a_row_is_shorter_than_the_rectangle() {
    let engine = spawn();
    set_lines(&engine, &["alphabet", "be", "gammaxyz"]);

    engine
        .handle
        .input("gg0<C-v>")
        .expect("enter blockwise visual mode");
    engine
        .handle
        .input("jjllll")
        .expect("extend the block across all three rows");

    let (_cursor, selection) = engine
        .handle
        .read_cursor_context()
        .expect("read cursor context");

    let selection = selection.expect("a selection is active");
    assert_eq!(selection.text, "alpha\nbe\ngamma");
    assert_eq!(selection.range, (1, 3));
}

/// No diagnostics posted reads back an empty list, not an error -- the
/// ordinary case for a freshly opened buffer.
#[test]
fn read_diagnostic_entries_with_none_posted_is_empty() {
    let engine = spawn();
    set_lines(&engine, &["hello world"]);

    let entries = engine
        .handle
        .read_diagnostic_entries()
        .expect("read diagnostic entries");

    assert!(entries.is_empty());
}

/// Every field of a posted diagnostic round-trips through
/// `vim.diagnostic.get(0)`, with `line`/`col` renormalized from that API's
/// 0-indexed wire values onto `EngineReadSnapshot`'s shared 1-indexed
/// convention (`lnum = 0` reads back as `line == 1`, `col = 2` as
/// `col == 3`).
#[test]
fn read_diagnostic_entries_decodes_every_severity() {
    let engine = spawn();
    set_lines(&engine, &["hello world", "second line"]);

    engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from(
                    "\
local ns = vim.api.nvim_create_namespace('ai-context-read-test')
vim.diagnostic.set(ns, 0, {
  { lnum = 0, col = 2, severity = vim.diagnostic.severity.ERROR, message = 'error message' },
  { lnum = 1, col = 0, severity = vim.diagnostic.severity.WARN, message = 'warn message' },
})",
                ),
                rmpv::Value::Array(vec![]),
            ],
        )
        .expect("post diagnostics");

    let mut entries = engine
        .handle
        .read_diagnostic_entries()
        .expect("read diagnostic entries");
    entries.sort_by_key(|e| e.line);

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].line, 1);
    assert_eq!(entries[0].col, 3);
    assert_eq!(entries[0].severity, DiagnosticSeverity::Error);
    assert_eq!(entries[0].message, "error message");
    assert_eq!(entries[1].line, 2);
    assert_eq!(entries[1].col, 1);
    assert_eq!(entries[1].severity, DiagnosticSeverity::Warning);
    assert_eq!(entries[1].message, "warn message");
}

/// An empty quickfix list reads back as an empty `Vec`, not an error.
#[test]
fn read_quickfix_entries_with_none_set_is_empty() {
    let engine = spawn();

    let entries = engine
        .handle
        .read_quickfix_entries()
        .expect("read quickfix entries");

    assert!(entries.is_empty());
}

/// `getqflist()` carries no `filename` field of its own (only `bufnr`,
/// live-verified against the pinned engine) -- the executor resolves each
/// entry's path via `nvim_buf_get_name`, and an entry with `bufnr == 0`
/// resolves to an empty path rather than erroring the whole read.
#[test]
fn read_quickfix_entries_resolves_paths_from_bufnr() {
    let engine = spawn();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp");
    std::fs::create_dir_all(&root).expect("create scratch root");
    let path = root.join(format!("ai-context-qf-{}.txt", std::process::id()));
    std::fs::write(&path, "qf target\n").expect("seed on-disk file");
    let path_str = path.to_string_lossy().into_owned();

    engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from(
                    "\
local path = ...
vim.cmd('edit ' .. vim.fn.fnameescape(path))
vim.fn.setqflist({}, ' ', {
  title = 'ai-context-read-test',
  items = {
    { filename = path, lnum = 3, col = 5, text = 'first entry' },
    { bufnr = 0, lnum = 1, col = 0, text = 'no-buffer entry' },
  },
})",
                ),
                rmpv::Value::Array(vec![rmpv::Value::from(path_str.as_str())]),
            ],
        )
        .expect("post quickfix list");

    let entries = engine
        .handle
        .read_quickfix_entries()
        .expect("read quickfix entries");

    assert_eq!(entries.len(), 2);
    assert!(entries[0]
        .path
        .ends_with(path.file_name().expect("file name")));
    assert_eq!(entries[0].line, 3);
    assert_eq!(entries[0].col, 5);
    assert_eq!(entries[0].text, "first entry");
    assert_eq!(entries[1].path, PathBuf::new());
    assert_eq!(entries[1].line, 1);
    assert_eq!(entries[1].text, "no-buffer entry");
}
