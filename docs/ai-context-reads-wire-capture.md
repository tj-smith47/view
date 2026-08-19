# Wire capture: the four context-read executors

Captured live against the pinned engine per "capture, never recall." Source
of truth for `CURRENT_BUFFER_TEXT_CHUNK`, `CURSOR_CONTEXT_CHUNK`,
`DIAGNOSTIC_ENTRIES_CHUNK`, and `QUICKFIX_ENTRIES_CHUNK` -- the
`nvim_exec_lua` chunks `EngineHandle::read_current_buffer_text`,
`read_cursor_context`, `read_diagnostic_entries`, and
`read_quickfix_entries` issue for `RpcCall::ReadCurrentBufferText`,
`ReadCursorContext`, `ReadDiagnosticEntries`, and `ReadQuickfixEntries`
respectively (declared by an earlier task; this task implements their
engine-side execution).

## Engine identity

```
$ nvim --version | head -3
NVIM v0.12.4
Build type: Release
LuaJIT 2.1.1785763465
```

Matches `.engine-pin` (`v0.12.4`).

## Capture method

A standalone Python msgpack-rpc client spawns `nvim --clean --headless
--listen <socket>` with the same hermetic `HOME`/`XDG_*` isolation
`EngineConfig::isolated()` uses, connects over the unix socket, and issues
`nvim_exec_lua` requests running the exact chunk text each `nvim_api.rs`
constant embeds.

## `nvim_win_get_cursor` and `getpos`: nvim's own mixed indexing, verbatim

```
nvim_win_get_cursor(0) -> [1, 0]
```

Row is 1-indexed, column is 0-indexed BYTE -- nvim's own documented mixed
convention (`:help nvim_win_get_cursor`). `CursorRead.line`/`.col` carry
these through verbatim, not renormalized.

## Selection: `mode()` decides "active", not stale `'<`/`'>` marks

With a UI attached, `gg0v` then `llll` (select the first 5 columns of line
1, char-wise):

```
mode() -> "v"
getpos("v") -> [0, 1, 1, 0]     -- anchor: line 1, col 1 (1-indexed)
getpos(".") -> [0, 1, 5, 0]     -- cursor: line 1, col 5 (1-indexed)
nvim_buf_get_text(0, 0, 0, 0, 5, {}) -> ["hello"]
```

After `<Esc>` (leaving visual mode):

```
mode() -> "n"
getpos("'<") -> [0, 1, 1, 0]    -- the exited selection's marks PERSIST
getpos("'>") -> [0, 1, 5, 0]
```

Because the marks persist after the mode that set them ends, the executor
checks `vim.api.nvim_get_mode().mode` (`"v"`, `"V"`, or blockwise `"\22"`)
at read time and only reads `getpos('v')`/`getpos('.')` while one of those
modes is current -- reading `'<`/`'>` unconditionally would report a
selection as "active" long after the user left it, live-confirmed by the
mark values above still being populated post-`<Esc>`.

A backward selection (`gg$v` then `0`, selecting right-to-left) reports the
anchor after the cursor in raw `getpos` terms; the chunk reorders the pair
so `selection_start <= selection_end` and reads the text forward regardless
of selection direction -- both directions produce the same `(1, 1)` range
and forward-ordered text (`"hello world"`) in the captures backing
`read_cursor_context_with_an_active_backward_selection`.

## `vim.diagnostic.get(0)`: 0-indexed, flat, closed severity range

```lua
vim.diagnostic.set(ns, 0, {
  { lnum = 0, col = 2, severity = vim.diagnostic.severity.ERROR, message = 'bad thing' },
  { lnum = 1, col = 0, severity = vim.diagnostic.severity.WARN, message = 'warn thing' },
})
vim.diagnostic.get(0)
```

```
[ { col: 2, end_lnum: 0, end_col: 5, severity: 1, message: 'bad thing',
    _extmark_id: 1, source: 'test', namespace: 3, lnum: 0, bufnr: 1 },
  { bufnr: 1, end_lnum: 1, end_col: 0, severity: 2, message: 'warn thing',
    _extmark_id: 2, namespace: 3, lnum: 1, col: 0 } ]
```

`lnum`/`col` are 0-indexed byte positions (the diagnostic API's own
convention, distinct from `getqflist`'s 1-indexed one below).
`severity` is `vim.diagnostic.severity`'s closed `1`(Error)..`4`(Hint)
range. `DIAGNOSTIC_ENTRIES_CHUNK` projects only the four fields
`DiagnosticEntry` models (`line`, `col`, `severity`, `message`), dropping
the rest (`_extmark_id`, `source`, `namespace`, `bufnr`, `end_lnum`,
`end_col`) rather than carrying wire-only bookkeeping past the engine
boundary.

## `getqflist()`: 1-indexed, and carries `bufnr` rather than `filename`

```lua
vim.fn.setqflist({}, ' ', {
  title = 'capture',
  items = {
    { filename = '/tmp/foo.txt', lnum = 3, col = 5, text = 'first entry' },
    { bufnr = 0, lnum = 1, col = 0, text = 'no-buffer entry' },
  },
})
vim.fn.getqflist()
```

```
[ { lnum: 3, bufnr: 3, end_lnum: 0, pattern: '', valid: 1, vcol: 0, nr: 0,
    module: '', type: '', end_col: 0, col: 5, text: 'first entry' },
  { lnum: 1, bufnr: 0, ..., col: 0, text: 'no-buffer entry' } ]
```

Note there is no `filename` key at all -- only `bufnr`, live-confirmed even
for an item originally `setqflist`'d with a `filename` field (nvim resolves
it to a `bufnr` on ingest and does not carry the string back out).
`QUICKFIX_ENTRIES_CHUNK` resolves each entry's path itself via
`vim.api.nvim_buf_get_name(item.bufnr)`, falling back to an empty string
for `bufnr == 0` (an entry with no buffer at all) -- the same "no name is
an empty string, not an omitted field" convention `PREVIEW_CHUNK` and
`CURRENT_BUFFER_TEXT_CHUNK` already use. `lnum`/`col` are `getqflist`'s own
1-indexed values, passed through verbatim (intentionally NOT renormalized
against the diagnostics read's 0-indexed convention -- each chunk keeps its
own nvim source's indexing).

## Current buffer text: same "no name is an empty string" convention

```lua
-- unnamed scratch buffer
{ path = vim.api.nvim_buf_get_name(buf), text = table.concat(...) }
  -> { path = '', text = '' }

-- after :edit /tmp/realfile.txt + an unsaved nvim_buf_set_lines
  -> { path = '/tmp/realfile.txt', text = 'alpha\nbeta' }
```

Confirms nvim's own in-memory (possibly unsaved) buffer content is what
crosses back, never a re-read of the file on disk -- the same contract the
picker preview pane's `PREVIEW_CHUNK` already proves for `PreviewBuffer`.
