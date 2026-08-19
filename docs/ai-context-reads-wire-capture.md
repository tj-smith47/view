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
a character whose BYTE width isn't 1 (any multi-byte UTF-8 sequence, `é`
included) exposes the gap: nvim's real blockwise rectangle is defined in
`virtcol()` (screen-column) terms, held constant across every row, and each
row's own byte offset for a given screen column depends on how many
multi-byte characters precede it on THAT line. This is a byte-width-vs-
screen-column divergence -- a separate axis from a character's own CELL
width (how many screen columns ONE character occupies: tabs and East-Asian-
wide characters can occupy several), which is what "Fix round 3" below
addresses. `é` is multi-byte (2 bytes) yet single-cell (1 screen column),
so it exercises this round's fix but not round 3's -- the two bugs are
independent and a fix for one does not imply the other.

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

## Fix round 3 (review-driven): `virtcol()`'s SCALAR form is a character's END cell, not its START -- wrong for the LOW bound on any multi-CELL character

Round 2's fix above is byte-width-correct but still screen-column-wrong on
its own terms for the LOW bound: `vim.fn.virtcol('v')`/`vim.fn.virtcol('.')`
(no second argument) return the SCALAR form, which `:help virtcol()`
documents as the RIGHTMOST (end) screen column a multi-cell character
occupies, never its start. Round 2's fixtures (`é`, byte-width 2) are all
CELL-width 1 -- a single-cell character's start and end column are the same
number, so the scalar form happened to be indistinguishable from the start
there. A character whose CELL width exceeds 1 (a tab, or an East-Asian-wide
character like `你`/`好`) exposes the gap: the scalar form overshoots the
true low bound by however many cells that character spans.

Severe case, buffer `["\tabc", "wxyzefgh"]`, `gg$<C-v>` then `j` (anchor on
the leading tab, cursor moves down one row):

```
mode() -> "\x16"
virtcol('v') -> 8            -- SCALAR: the tab's END cell (it spans 1-8)
virtcol('v', 1) -> [1, 8]    -- LIST form: the tab's actual [start, end]
virtcol('.') -> 9
virtcol('.', 1) -> [9, 9]

-- round 2 (buggy for this case): lo_vcol from the scalar form
lo_vcol = min(virtcol('v')=8, virtcol('.')=9) = 8
-- row 1 ("\tabc", the tab's OWN row): virtcol2col(win,1,8) resolves back to
--   the tab's own start byte regardless of which of its 8 cells you ask for,
--   so row 1 is unaffected by the bug here
-- row 2 ("wxyzefgh", NOT the tab's row): virtcol2col(win,2,8) resolves to
--   the BYTE column of row 2's OWN 8th character ('h') -- a column that has
--   nothing to do with the tab at all
row 2 sliced from vcol 8 -> "h"     -- WRONG: nvim actually yanks the WHOLE
                                        row, "wxyzefgh"

-- correct: nvim's own yank oracle
getreg('"') -> "\ta\nwxyzefgh"
getregtype('"') -> "\x169"    (blockwise, width 9)
```

This is exactly the shape of bug the review flagged: a rectangle's low bound
is one shared screen column applied to every row, and a multi-cell
character's scalar (end-cell) virtcol is the wrong number to share -- it
overshoots on every OTHER row that doesn't happen to contain that same
character at that position. The fix is the list form's START element for
the low bound specifically; the high bound's existing use of the scalar
(end cell) form was already correct, unchanged:

```lua
local lo_vcol = math.min(vim.fn.virtcol('v', 1)[1], vim.fn.virtcol('.', 1)[1])
local hi_vcol = math.max(vim.fn.virtcol('v'), vim.fn.virtcol('.'))
```

With `lo_vcol = min(1, 9) = 1` (the tab's own start, not its end), row 1
(the tab's own row) still yields `"\ta"` (screen cols 1-9: the whole tab,
cols 1-8, plus `'a'` at col 9), and row 2 now correctly starts from its own
column 1, yielding the full `"wxyzefgh"` (only 8 columns exist; the request
for column 9 simply runs off the end of the row, the same "short row
contributes its own end" behavior "Fix round 2" already established, not a
new case). Live-verified by
`read_cursor_context_with_a_blockwise_selection_anchored_on_a_leading_tab`.

### Sub-case, same fix: nvim pads a partially-covered multi-cell character with spaces, never raw bytes

Closing the low-bound bug alone is not sufficient: whenever the shared
rectangle's low or high screen-column bound lands INSIDE a multi-cell
character (covering only some of its cells, not all), nvim does not emit
that character's raw bytes -- there is no such thing as "half a tab" or
"half of `你`" in a text buffer. Instead it pads the row with one space per
covered screen cell, keeping the block visually rectangular. This is
distinct from -- and layered on top of -- the low-bound fix above.

Right-edge partial coverage, buffer `["你好xy", "abcdef"]`, `gg0<C-v>` then
`jll` (screen columns 1-3; `好` spans columns 3-4, so column 3 covers only
its LEFT half):

```
mode() -> "\x16"
virtcol('v', 1) -> [1, 2]     -- anchor on 你, which spans cols 1-2
virtcol('.', 1) -> [3, 3]     -- cursor lands on row 2's 'c' (ASCII, single-cell)
lo_vcol = min(1, 3) = 1
hi_vcol = max(2, 3) = 3

getreg('"') -> "\xe4\xbd\xa0 \nabc"   ("你 \nabc")
getregtype('"') -> "\x163"
```

Row 1 (`"你好xy"`) covers screen columns 1-3: `你` (cols 1-2) is fully
covered and copied raw; `好` (cols 3-4) has only its column 3 (its left
half) inside the rectangle -- covered cell count 1 -- so it contributes ONE
pad space, not any raw byte of `好`. Row 2 (`"abcdef"`) covers columns 1-3
with no multi-cell character present, so it copies raw: `"abc"`.
Live-verified by
`read_cursor_context_with_a_blockwise_selection_over_a_wide_character`.

Right-edge partial coverage on a tab, buffer `["a\tbcd", "wxyzefgh"]`,
`gg0<C-v>` then `jlll` (screen columns 1-4; the tab spans columns 2-8, so
columns 2-4 are its first three cells only):

```
lo_vcol = 1, hi_vcol = 4
getreg('"') -> "a   \nwxyz"     -- 'a' raw, then 3 pad spaces for the tab's
                                    3 covered cells (columns 2, 3, 4)
getregtype('"') -> "\x164"
```

Live-verified by
`read_cursor_context_with_a_blockwise_selection_over_a_partially_covered_tab`.

Left-edge partial coverage (confirms the padding rule is symmetric, not
right-edge-only): buffer `["abcd", "xy\xe5\xa5\xbdz", "ABCD"]` (`"xy好z"`,
`好` a 3-byte UTF-8 character spanning screen columns 3-4), both endpoints
on the single-cell `'d'`/`'D'` at column 4 (`gg0lll<C-v>jj` -- move to
column 4 in Normal mode first, THEN enter blockwise Visual, so `curswant`
carries a real column rather than nvim's `$`-motion `MAXCOL` sentinel).
Row 2 is never touched by cursor movement at all -- it is a plain interior
row of the three-row block -- so the shared column 4 lands on `好`'s own
RIGHT (second) cell there, never its start:

```
virtcol('v', 1) -> [4, 4]      -- 'd', single-cell
virtcol('.', 1) -> [4, 4]      -- 'D' on row 3, single-cell
lo_vcol = 4, hi_vcol = 4

getreg('"') -> "d\n \nD"     -- row 2: 1 pad space (好's single covered
                                 cell, column 4), nothing else
getregtype('"') -> "\x161"
```

(A first attempt at this capture used `gg$<C-v>j` to land on row 1's `'d'`
by way of `$`, expecting an ordinary 2-cell-wide rectangle -- but `$`
unconditionally sets `curswant` to nvim's `MAXCOL` sentinel even when
pressed before entering Visual mode, silently turning the whole selection
into a `$`-block. That capture is not reused here; this section's numbers
come from the corrected key sequence above, live-verified through the
actual `EngineConfig::isolated()` test harness, not the standalone capture
client.)

Confirmed via a direct `virtcol2col`/`virtcol({lnum,col},1)` probe against
`"xy好z"`: every byte column of `好` (its own 3 UTF-8 bytes) reports the
SAME `[3, 4]` span regardless of which of those bytes -- or which of its two
screen cells -- is queried, which is what lets a single "does this
character's own span fall entirely inside `[lo_vcol, hi_vcol]`?" check
(rather than separate left/right-edge special cases) decide raw-copy versus
pad uniformly for both edges. `vim.fn.virtcol({row, '$'})` (one past a
line's own last real column, confirmed `9` for an 8-column line and `1` for
an empty one) is what bounds the per-row scan so it stops at the row's own
end rather than looping on `virtcol2col`'s past-end-of-line clamp.

The mixed-length-rows short-row case from "Fix round 2"
(`["alphabet","be","gammaxyz"]`, columns 1-5, `"be"` contributing its full 2
columns) is unaffected by this padding rule: running out of row PART WAY
THROUGH the rectangle is not the same as a character being partially
covered, and still contributes nothing extra, not padding -- re-verified
against the same oracle, unchanged:

```
lo_vcol = 1, hi_vcol = 5
getreg('"') -> "alpha\nbe\ngamma"     -- row 2 contributes 2 columns, not 5
getregtype('"') -> "\x165"
```

That result was originally read here as the general rule for every short
row, which it is not: it holds only for a row that reaches INTO the
rectangle. A row ending before the rectangle begins is a different case
with a different answer, captured in "Fix round 4" below.

All four fixtures above (the tab anchor low-bound case, the two right-edge
padding cases, and the left-edge padding case) plus the existing ASCII and
single-cell-multi-byte (`é`) regression cases were captured with a
standalone Python msgpack-rpc client against `nvim --clean --headless
--listen <socket>` (NVIM v0.12.4), using `normal! y` + `getreg('"')` as the
oracle, before writing any fix code.

## Fix round 4 (review-driven): the `$`-block bypassed the padding walker, and a row ending before the block pads to the block's full width

Two residues of round 3, both captured live against the same
`nvim_input` + `y` + `getreg('"')` oracle before any code changed.

### The `$`-block's raw byte slice skipped the padding rule at its LOW bound

Round 3's `blockwise_row_text` early-returned a raw `string.sub` byte slice
for a `$`-block, before reaching the padding walker. A `$`-block's HIGH
bound is per-row by definition, but its LOW bound is still one shared screen
column -- and it splits a multi-cell character exactly as readily as an
ordinary block's does.

Buffer `["abcdefgh", "\txyz"]`, `gg0lll<C-v>` then `j$` (low bound = screen
column 4; row 2's leading tab spans columns 1-8, so column 4 lands inside
it):

```
virtcol('v', 1) -> [4, 4]         -- 'd' on row 1, single-cell
virtcol('.', 1) -> [12, 12]
getcurpos()[5] -> 2147483647      -- MAXCOL: this is a $-block
lo_vcol = 4
virtcol({1,'$'}) -> 9,  virtcol({2,'$'}) -> 12

getreg('"') -> "defgh\n     xyz"  -- row 2: FIVE pad spaces (the tab's
                                     covered cells, columns 4-8), then "xyz"
getregtype('"') -> "\x168"
```

Round 3's chunk returned `"defgh\n\txyz"` for the same selection -- the raw
tab byte, an unsplit character nvim never yanks here. Routing the `$` case
through the same walker with a per-row `hi_vcol = virtcol({row,'$'}) - 1`
reproduces the oracle exactly, with no separate `$` logic left in the
function. Live-verified by
`read_cursor_context_with_a_dollar_blockwise_selection_whose_low_bound_splits_a_tab`.

### A row ending BEFORE the block start pads to the block's full width

Buffer `["alphabet", "ab", "gammaxyz"]`, `gg0llll<C-v>` then `jjll` (screen
columns 5-7). Row 2 (`"ab"`, 2 columns) never reaches column 5 at all:

```
virtcol('v', 1) -> [5, 5]
virtcol('.', 1) -> [7, 7]
lo_vcol = 5, hi_vcol = 7
virtcol({2,'$'}) -> 3            -- row 2 ends at column 2

getreg('"') -> "abe\n   \naxy"   -- row 2: THREE pad spaces, the block's
                                    own width, not the empty string
getregtype('"') -> "\x163"
```

Round 3's walker (`while v <= hi_vcol and v < end_vcol`) exits immediately
when `lo_vcol >= end_vcol`, yielding `""` for that row. An empty row behaves
identically to `"ab"` here -- same buffer with `["alphabet", "", "gammaxyz"]`
and the same keys yields the same `"abe\n   \naxy"`.

The boundary between this case and the round-2 "short row contributes what
it has" case is exact, and it is asymmetric. Two captures pin it, both with
block columns 3-5 (`gg0ll<C-v>jjll`):

```
["abcdefgh", "a",  "gammaxyz"]   virtcol({2,'$'}) -> 2  (row ends at col 1)
  getreg('"') -> "cde\n   \nmma"       -- PADDED to the block's 3 columns

["abcdefgh", "ab", "gammaxyz"]   virtcol({2,'$'}) -> 3  (row ends at col 2)
  getreg('"') -> "cde\n\nmma"          -- NOT padded, empty
```

So the predicate is `virtcol({row,'$'}) < lo_vcol` (the row ends strictly
before the block's first column), not `lo_vcol >= end_vcol`: a row reaching
exactly `lo_vcol - 1` is flush with the block and contributes nothing. This
matches nvim's own `block_prep` short-line test in `ops.c`, which pads only
when the line's total width falls short of the block's start column.
Live-verified by
`read_cursor_context_with_a_blockwise_selection_where_a_row_ends_before_the_block`.

Both fixes live in one predicate ordering: compute `end_vcol` first, let a
`$`-block rewrite `hi_vcol` to `end_vcol - 1`, then apply the
ends-before-the-block padding. That ordering is what makes the two
interact correctly for a `$`-block over a short row, where `hi_vcol` falls
BELOW `lo_vcol` and the pad width clamps to zero -- confirmed against the
oracle rather than assumed:

```
["abcdefgh", "ab"]  gg0lll<C-v> then j$   -> getreg('"') = "cdefgh\n"
["abcdefgh", ""]    gg0lll<C-v> then j$   -> getreg('"') = "abcdefgh\n"
```

(Round 3's chunk returned `"cdefgh\nb"` for the first of those -- the raw
slice clamped `lo0` to the row's length instead of yielding nothing.)

All nine round-4 captures (the two review cases, the empty-row variant, the
two boundary captures, the two `$`-block short-row guards, and the two
unchanged round-2/round-3 controls) were taken against `nvim --clean
--headless --listen <socket>` (NVIM v0.12.4) with a UI attached, driving the
selection through `nvim_input` and reading `getreg('"')` after `y`. The
candidate chunk agreed with the oracle on all nine before any source file
was edited.

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
