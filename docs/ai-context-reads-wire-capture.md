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

## Fix round 1 (review-driven): a charwise selection ending on a multi-byte character

`getpos('.')`'s own byte column is the byte offset of the FIRST byte of the
character under the cursor, 1-indexed -- not an exclusive end. The
original chunk passed that raw column straight through as
`nvim_buf_get_text`'s exclusive end column, which truncates mid-character
whenever the selection ends on a multi-byte one. Buffer `["a\xe9 bc"]`
(`"aé bc"`, `é` a 2-byte UTF-8 sequence), `gg0v` then `l` (select `a` and
extend one char onto `é`):

```
mode() -> "v"
getpos('v') -> [0, 1, 1, 0]     -- anchor: line 1, col 1
getpos('.') -> [0, 1, 2, 0]     -- cursor: line 1, col 2 (the FIRST byte of é)

-- naive (buggy): raw getpos col used directly as the exclusive end
nvim_buf_get_text(0, 0, 0, 0, 2, {}) -> ["a\xc3"]   -- INVALID UTF-8, truncated
                                                        mid-character
```

That invalid byte sequence is exactly what made the bug silent rather than
loud: `Value::as_str` on the reply fails UTF-8 validation, and
`decode_cursor_context_reply`'s `_ => None` fallback for a malformed
`selection_*` triple turned a real, active selection into "no selection at
all" with no error anywhere.

The fix computes the char-index at that byte offset (`vim.fn.charidx`) and
the byte offset one character further (`vim.fn.byteidx(line, charidx +
1)`), and uses THAT as the exclusive end instead:

```
line = "a\xc3\xa9 bc"
charidx(line, 1) -> 1          -- byte 1 (0-indexed) is inside char index 1 (é)
byteidx(line, 2) -> 3          -- start of char index 2 (the space), 0-indexed
nvim_buf_get_text(0, 0, 0, 0, 3, {}) -> ["a\xc3\xa9"]   -- "aé", correct
```

Live-verified end to end through the actual (fixed) `CURSOR_CONTEXT_CHUNK`
by `ai_context_reads_live.rs`'s
`read_cursor_context_selection_ending_on_a_multibyte_character_reads_the_full_character`.

## Fix round 1 (review-driven): linewise and blockwise selections were computed as charwise spans

The original chunk read every visual submode (`v`, `V`, blockwise `\22`)
through the same charwise `nvim_buf_get_text(srow-1, scol-1, erow-1, ecol)`
call, which is wrong for the other two: linewise has no meaningful columns,
and blockwise's rectangle is not the same span as the charwise text between
its two corners.

Linewise (`V`), buffer `["alpha", "beta", "gamma"]`, `gg0V` then `j`:

```
mode() -> "V"
getpos('v') -> [0, 1, 1, 0]
getpos('.') -> [0, 2, 1, 0]
-- correct: every full line from srow to erow, columns ignored entirely
nvim_buf_get_lines(0, 0, 2, false) -> ["alpha", "beta"]
```

Blockwise (`<C-v>`, mode byte `\x16`), same buffer, `gg0<C-v>` then `jl`:

```
mode() -> "\x16"
getpos('v') -> [0, 1, 1, 0]
getpos('.') -> [0, 2, 2, 0]
-- correct: the column-range rectangle (cols 1-2) clamped per line, joined with \n
row 1 "alpha"[1:2] -> "al"
row 2 "beta"[1:2]  -> "be"
-> "al\nbe"
```

The charwise interpretation the original chunk would have produced for
either case is visibly wrong by comparison: linewise as charwise stops
mid-line (`getpos('.')`'s column is 1, so a charwise read would truncate
`"beta"` to its first byte), and blockwise as charwise pulls in the entire
first line's tail plus the second line's head rather than the rectangle.

Live-verified through the fixed `CURSOR_CONTEXT_CHUNK` by
`ai_context_reads_live.rs`'s
`read_cursor_context_with_a_linewise_selection_reads_whole_lines` and
`read_cursor_context_with_a_blockwise_selection_reads_the_rectangle`.

## Fix round 2 (review-driven): blockwise is a SCREEN-column rectangle, not a byte-column one

Round 1's blockwise fix above clamped `getpos`'s raw BYTE columns per line --
correct only because its own capture buffer (`"alpha"`/`"beta"`/`"gamma"`) is
ASCII, where byte column and screen column never diverge. Any line containing
a multi-byte character exposes the gap: nvim's real blockwise rectangle is
defined in `virtcol()` (screen-column) terms, held constant across every row,
and each row's own byte offset for a given screen column depends on how many
multi-byte characters precede it on THAT line.

Buffer `["\xe9xyz", "abcd"]` (`"éxyz"`, `é` a 2-byte UTF-8 sequence),
`gg0<C-v>` then `jl`:

```
mode() -> "\x16"
getpos('v') -> [0, 1, 1, 0]      -- byte col 1
getpos('.') -> [0, 2, 2, 0]      -- byte col 2
virtcol('v') -> 1
virtcol('.') -> 2                -- SCREEN col 2, same as the byte col here only
                                     because line 2 ("abcd") has no multi-byte chars
-- round 1 (buggy): byte columns 1..2 applied verbatim to every row
row 1 "éxyz"[byte 1:2]  -> "\xe9"     -- one BYTE of é, not the whole character
row 2 "abcd"[byte 1:2]  -> "ab"
-> "\xe9\nab"                          -- WRONG, and INVALID UTF-8 besides

-- correct: nvim's own yank oracle (normal! y after the same selection)
getreg('"') -> "\xe9x\nab"    ("éx\nab")
getregtype('"') -> "\x162"    (blockwise, width 2)
```

nvim's own yank keeps the SCREEN-column bound (2) fixed across both rows:
row 1's screen columns 1-2 are the single character `é` (screen-width 1) plus
`x` (screen-width 1), i.e. the substring `"éx"`; row 2's screen columns 1-2
are the bytes `"ab"`. Round 1's byte-column rectangle instead sliced row 1 at
byte offset 2, landing mid-character inside `é`.

The fix reads `virtcol('v')`/`virtcol('.')` for the shared screen-column
bounds (not `getpos`'s byte columns) and converts each row's own low/high
screen column to that row's own byte column via
`vim.fn.virtcol2col(win, lnum, vcol)`:

```lua
local win = vim.api.nvim_get_current_win()
-- line 1 "éxyz": screen col 1 is byte 1 (é starts there); screen col 2 is
-- byte 3 (é occupies bytes 1-2, so 'x' starts at byte 3)
virtcol2col(win, 1, 1) -> 1
virtcol2col(win, 1, 2) -> 3
virtcol2col(win, 1, 3) -> 4   -- 'y'
virtcol2col(win, 1, 4) -> 5   -- 'z'
-- line 4 "aébc": screen col 2 is byte 2 (é starts there); screen col 3 is
-- byte 4 (é occupies bytes 2-3, so 'b' starts at byte 4)
virtcol2col(win, 4, 1) -> 1   -- 'a'
virtcol2col(win, 4, 2) -> 2   -- é
virtcol2col(win, 4, 3) -> 4   -- 'b'
virtcol2col(win, 4, 4) -> 5   -- 'c'
-- queried past a line's own length, it clamps to the line's own last byte
-- column rather than erroring or extending past it
virtcol2col(win, 2, 10) -> 4  -- line 2 "abcd" is 4 bytes long
```

A second, wider case confirms the same conversion holds for the anchor's own
column, not just the cursor's: buffer `["a\xe9bc", "wxyz"]` (`"aébc"`),
`gg0<C-v>` then `jll` (screen columns 1-3):

```
virtcol('v') -> 1, virtcol('.') -> 3
getreg('"') -> "a\xe9b\nwxy"   ("aéb\nwxy")
getregtype('"') -> "\x163"
```

Row 1's screen columns 1-3 are `"aéb"` (`a` + the 2-byte `é` + `b`, three
screen cells, four bytes); row 2's are `"wxy"` (three bytes, screen and byte
columns coincide with no multi-byte characters present). Live-verified end to
end through the fixed `CURSOR_CONTEXT_CHUNK` by
`read_cursor_context_with_a_blockwise_selection_over_a_multibyte_character`
and `read_cursor_context_with_a_blockwise_selection_anchored_on_a_multibyte_character`.

### The `$`-block case: `curswant == MAXCOL` extends every row to its own end

Pressing `$` while in blockwise Visual (a "`$`-block") is a distinct nvim
mode where every row extends to its own actual end, not to the shared
screen-column upper bound -- the block's right edge becomes ragged, tracking
each line's own length. `getcurpos()` (`getcurpos()[5]` in Lua's 1-indexed
list access; `getcurpos()[4]` in VimL's 0-indexed one) carries this as its
`curswant` field, set to nvim's `MAXCOL` sentinel (`2147483647`) exactly when
`$` was the last motion:

Buffer `["alpha", "be"]`, `gg0<C-v>` then `j$`:

```
mode() -> "\x16"
virtcol('v') -> 1, virtcol('.') -> 3     -- cursor sits on "be"'s own last column
getcurpos()[5] -> 2147483647             -- MAXCOL: a $-block
getreg('"') -> "alpha\nbe"               -- BOTH rows in full, not clamped to
                                             the shorter row's own screen width
getregtype('"') -> "\x165"
```

Without the `curswant` check, the naive screen-column rectangle (cols 1-3)
would clamp row 1 to `"alp"` -- visibly wrong against the oracle, which
extends row 1 to its actual end (`"alpha"`) exactly as it does row 2.
Live-verified by
`read_cursor_context_with_a_dollar_blockwise_selection_reads_every_line_to_its_own_end`.

### An ordinary (non-`$`) block still clamps a short row to its own end

Distinct from the `$`-block case above but easy to conflate with it: even a
plain blockwise selection whose shared screen-column upper bound exceeds one
row's own length still yanks that row in full, from the low column to its
own end, rather than nothing or a padded/truncated slice. Buffer
`["alphabet", "be", "gammaxyz"]`, `gg0<C-v>` then `jjllll` (screen columns
1-5, spanning three rows where the middle one is only two columns wide):

```
virtcol('v') -> 1, virtcol('.') -> 5
getcurpos()[5] -> 5                        -- NOT MAXCOL; an ordinary block
getreg('"') -> "alpha\nbe\ngamma"
getregtype('"') -> "\x165"
```

Row 2 (`"be"`, 2 bytes) contributes its entire content (`"be"`) despite the
block's screen-column bound reaching 5 -- the same `math.min(hi0, #line)`
clamp round 1 already applied to byte columns carries over unchanged once
`hi0` is derived from `virtcol2col` instead, so no new clamping logic beyond
round 1's was needed for this case. Live-verified by
`read_cursor_context_with_a_blockwise_selection_where_a_row_is_shorter_than_the_rectangle`.

The existing ASCII-uniform-width regression case
(`read_cursor_context_with_a_blockwise_selection_reads_the_rectangle`,
`["alpha","beta","gamma"]`, `gg0<C-v>jl` -> `"al\nbe"`) is unchanged by this
fix -- re-verified against the same oracle, `virtcol` and byte column agree
on every row when no row contains a multi-byte character, which is exactly
why an ASCII-only capture was insufficient to catch the original bug.

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
1-indexed values, unmodified by this chunk. (Fix round 1 correction: an
earlier version of this note claimed each chunk deliberately keeps its own
source's indexing all the way out to `EngineReadSnapshot`. That was wrong
for `QuickfixEntry` and `DiagnosticEntry` alike -- see "Fix round 1: one
shared 1-indexed convention" below for the corrected, actual contract.)

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

## Fix round 1 (review-driven): one shared 1-indexed convention across all three reads

The three chunks above cross the wire in three different native
conventions -- `nvim_win_get_cursor`'s column is 0-indexed, `vim.diagnostic
.get`'s `lnum`/`col` are both 0-indexed, `getqflist`'s are already
1-indexed -- and an earlier version of this document treated that as
something each read should keep verbatim all the way out to
`EngineReadSnapshot`. That was a mistake: it meant the identical physical
buffer position rendered as three different numbers depending on which of
the three reads reported it (e.g. a diagnostic on the same character the
cursor sits on would show `col: 2` from one read and `col: 3` from the
other), which is confusing for an agent reading a prompt's attached
context and has no benefit to compensate.

The corrected contract: `view-engine`'s own reply decoders (not the Lua
chunks, which still emit each source's native wire values) renormalize
every line/column onto ONE shared 1-indexed convention before it ever
reaches `CursorRead`/`DiagnosticEntry`/`QuickfixEntry`. Concretely:

```
cursor.col:               wire value + 1   (0-indexed -> 1-indexed)
diagnostic.line/.col:     wire value + 1   (both 0-indexed -> 1-indexed)
quickfix.line/.col:       wire value       (already 1-indexed, unchanged)
cursor.line:               wire value       (nvim_win_get_cursor row is already
                                              1-indexed, unchanged)
selection_start/_end:      wire value       (getpos rows are already 1-indexed,
                                              unchanged)
```

Live-verified: `read_cursor_context_with_no_active_selection`'s cursor col
0 on the wire (an empty buffer, column 0) now reads back as `col == 1`;
`read_cursor_context_with_an_active_forward_selection`'s wire col 4 reads
back as `col == 5`; `read_diagnostic_entries_decodes_every_severity`'s
`lnum = 0, col = 2` / `lnum = 1, col = 0` read back as `line == 1, col ==
3` / `line == 2, col == 1`. `view-ai::acp::driver`'s
`cursor_diagnostic_and_quickfix_render_the_same_physical_position_identically`
pins that a `Cursor`, `Diagnostics`, and `QuickfixList` block all built
from the same physical position (line 5, column 3) render the identical
numbers in their prose -- the renderer forwards whatever it is given and
performs no index math of its own, so this only holds because the
normalization already happened upstream.
