# Wire capture: `nvim_buf_set_text` byte columns and `undojoin` semantics

Captured live against the pinned engine per "capture, never recall." Source
of truth for `BUF_SET_TEXT_CHUNK`, the `nvim_exec_lua` chunk
`EngineHandle::set_buf_text` issues to apply agent-proposed edits via
`RpcCall::BufSetText`.

## Engine identity

```
$ nvim --version | head -3
NVIM v0.12.4
Build type: Release
LuaJIT 2.1.1785763465
```

Matches `.engine-pin` (`v0.12.4`).

## Capture method

A standalone Python msgpack-rpc client (no `pynvim`; not installed) spawns
`nvim --clean --headless --listen <socket>` with the same hermetic
`HOME`/`XDG_*` isolation `EngineConfig::isolated()` uses, connects over the
unix socket, and issues raw msgpack-RPC requests -- both bare API calls
(`nvim_buf_set_text`, `nvim_command`) and `nvim_exec_lua` running the exact
chunk text `BUF_SET_TEXT_CHUNK` embeds.

## 1. Columns are 0-indexed BYTE offsets, not character offsets

Buffer set to `["héllo wörld", "line2"]` (`é` and `ö` are each 2-byte UTF-8
sequences, so `"héllo"` is 5 characters but 6 bytes).

```
nvim_buf_set_text(0, 0, 0, 0, 6, ["X"])   -- byte length of "héllo"
  -> lines become ["X wörld", "line2"]     -- the whole word replaced, correct
```

Reset, then the same edit with the CHARACTER length instead of the byte
length:

```
nvim_buf_set_text(0, 0, 0, 0, 5, ["Y"])   -- character length of "héllo"
  -> lines become ["Yo wörld", "line2"]    -- only 5 of the word's 6 bytes
                                             consumed, leaving a stray "o"
```

Confirms: `end_col`/`start_col` are byte offsets. A caller passing character
counts corrupts any line containing a multi-byte character, silently.

## 2. `undojoin` genuinely links two calls into one undo step

Buffer reset to `["line1", "line2"]`.

```
nvim_buf_set_text(0, 0, 0, 0, 5, ["LINE1"])   -- first edit, no undojoin
nvim_command("undojoin")
nvim_buf_set_text(0, 1, 0, 1, 5, ["LINE2"])   -- second edit, joined
  -> lines: ["LINE1", "LINE2"]
nvim_command("undo")
  -> lines: ["line1", "line2"]                -- ONE undo reverted BOTH edits
```

Negative control, same reset, `undojoin` omitted before the second call:

```
nvim_buf_set_text(0, 0, 0, 0, 5, ["LINE1"])
nvim_buf_set_text(0, 1, 0, 1, 5, ["LINE2"])
  -> lines: ["LINE1", "LINE2"]
nvim_command("undo")
  -> lines: ["LINE1", "line2"]                -- ONE undo reverted ONLY the second
```

## 3. The same behavior holds wrapped in `BUF_SET_TEXT_CHUNK` (via `nvim_exec_lua`)

The production chunk (`buf, undojoin, edits` varargs; loops over `edits`,
running `vim.cmd('undojoin')` first when `undojoin` is true) was captured
against the identical two scenarios above through `nvim_exec_lua` rather
than bare API calls, and produced byte-identical results: joined batch
reverts as one `undo`, unjoined batch reverts one edit per `undo`. Multiple
edits in a single `edits` array (two hunks, one call) were also captured
and applied correctly in one pass.

## 4. A stale buffer handle surfaces as an `Err`, never a panic or a silent no-op

A scratch buffer created via `nvim_create_buf(false, true)` and then
deleted via `nvim_buf_delete(buf, {force = true})`:

```
nvim_buf_set_text(<deleted-buf>, 0, 0, 0, 0, ["x"])
  -> error: [1, "Invalid buffer id: 2"]
```

The same call wrapped in `BUF_SET_TEXT_CHUNK` via `nvim_exec_lua`:

```
nvim_exec_lua(BUF_SET_TEXT_CHUNK, [<deleted-buf>, false, [edit]])
  -> error: [0, "Lua: [string \"<nvim>\"]:5: Invalid buffer id: 2\n
             stack traceback:\n\t[C]: in function 'nvim_buf_set_text'\n
             \t[string \"<nvim>\"]:5: in main chunk"]
```

Both are `request`-shaped errors (not a dropped notification, not a crash):
`EngineHandle::set_buf_text` issues this as a `request_timeout`, so this
error crosses back as `EngineError::Remote`, live-verified by
`crates/view-engine/tests/buf_set_text_live.rs`'s
`stale_buffer_handle_surfaces_as_an_error_not_a_panic`.

## 5. `undojoin: true` throws `E790` after ANY undo, not just with nothing to join onto

Corrected claim (fix round 1): an earlier version of this doc claimed
`E790` was specific to "no prior undoable change to join onto" and
effectively "never happens" against production usage. That was wrong.
`:help undojoin` documents `E790` ("undojoin is not allowed after undo")
for the case where the immediately preceding action was itself an `undo`
-- which is exactly what happens whenever the user presses `u` right
before an agent's next accepted hunk arrives with `undojoin: true`, a
completely ordinary interleaving `BufSetText`'s own contract does not (and
cannot) rule out:

```
nvim_buf_set_text(0, 0, 0, 0, 5, ["LINE1"])   -- first edit
nvim_command("undo")                          -- user presses u
nvim_command("undojoin")                      -- next hunk tries to join
  -> error: [0, "Lua: [string \"vim/_core/editor\"]:355: nvim_exec2(), line 1:
             Vim(undojoin):E790: undojoin is not allowed after undo\n..."]
```

Issuing `undojoin: true` as the very first edit against a buffer whose most
recent action was itself an ordinary edit (e.g. the `nvim_buf_set_lines`
reset every test here performs, with no undo in between) does NOT error --
that narrower case was the only one the earlier version of this doc
actually captured. The undo-right-before case above is the one that
matters for `BufSetText`'s real fallback contract, captured in full in
section 7 below.

## 6. A multi-row edit is applied start-row-first, never swapped

Buffer set to `["one", "two", "three"]`, a single edit spanning row 0
(after `"o"`) through row 2 (through `"th"`):

```
nvim_buf_set_text(0, 0, 1, 2, 2, ["X"])
  -> lines become ["oXree"]                  -- correct: prefix "o" + "X" + suffix "ree"
```

The same call with `start_row`/`end_row` swapped (the row-order bug this
section exists to rule out):

```
nvim_buf_set_text(0, 2, 1, 0, 2, ["X"])
  -> error: [0, "'start' is higher than 'end'"]
```

Every other edit captured in this document starts and ends on the same
row, so `start_row == end_row` there and a row swap would be invisible.
This case is what `crates/view-engine/tests/buf_set_text_live.rs`'s
`set_buf_text_applies_a_multi_row_edit_without_swapping_start_and_end_row`
pins.

## 7. `undojoin: true` right after an undo falls back to applying unjoined, never drops the edit

Buffer reset to `["line1", "line2"]`; first edit `undojoin: false`, then the
user undoes it, then a second edit arrives with `undojoin: true`:

```
BUF_SET_TEXT_CHUNK(0, false, [{0,0,0,5,["LINE1"]}])   -- applies
nvim_command("undo")                                   -- back to ["line1", "line2"]
BUF_SET_TEXT_CHUNK(0, true, [{0,0,0,5,["LINE1-AGAIN"]}])
```

Without a `pcall` guard around `vim.cmd('undojoin')`, this throws `E790`
(section 5) and the whole chunk aborts before its `for` loop ever runs --
the edit is silently dropped, not just the join:

```
  -> error: [0, "Lua: ...E790: undojoin is not allowed after undo..."]
  -> nvim_buf_get_lines: ["line1", "line2"]    -- edit never applied at all
```

With `pcall(vim.cmd, 'undojoin')` (the shipped form), the `E790` is
swallowed and the loop still runs:

```
  -> nvim_buf_get_lines: ["LINE1-AGAIN", "line2"]   -- edit applied
nvim_command("undo")
  -> nvim_buf_get_lines: ["line1", "line2"]          -- its own undo step, unjoined
```

Live-verified by `buf_set_text_live.rs`'s
`undojoin_true_after_an_undo_falls_back_to_applying_unjoined`.

## 8. A batch applies in descending position order regardless of listed order

Buffer set to `["aaa bbb ccc"]`, two edits on the same line: replace bytes
`[0,3)` with `"XXXX"` (grows the line by 1 byte) and bytes `[8,11)` with
`"YYYY"`. Applied in the order listed (ascending, first-edit-first):

```
nvim_buf_set_text(0, 0, 0, 0, 3, ["XXXX"])
nvim_buf_set_text(0, 0, 8, 0, 11, ["YYYY"])
  -> lines become ["XXXX bbbYYYYc"]     -- CORRUPTED: the first edit's growth
                                           shifted the second edit's columns,
                                           which still addressed the ORIGINAL
                                           byte offsets
```

The same two edits applied bottom-to-top (descending `(start_row,
start_col)`, the order `EngineHandle::set_buf_text` now sorts into
regardless of how the caller listed them):

```
nvim_buf_set_text(0, 0, 8, 0, 11, ["YYYY"])
nvim_buf_set_text(0, 0, 0, 0, 3, ["XXXX"])
  -> lines become ["XXXX bbb YYYY"]     -- correct: both edits land where addressed
```

Live-verified by `buf_set_text_live.rs`'s
`set_buf_text_applies_edits_in_position_order_regardless_of_batch_order`.
Per `TextEdit`'s own doc, this sort is only sound for non-overlapping
edits -- an overlapping batch is unsupported and unspecified.

## 9. `start_col` is also a byte offset, not just `end_col`

Every capture above with a nonzero `start_col` used `0`, where byte and
character offsets coincide. Buffer set to `["héllo wörld"]`, replacing
`"llo"` (byte offset 3, after `"h"` (1 byte) + `"é"` (2 bytes)) through
byte offset 6:

```
nvim_buf_set_text(0, 0, 3, 0, 6, ["LLO"])
  -> lines become ["héLLO wörld"]     -- correct: "é" left intact, "llo" replaced
```

A caller that mistakenly used the CHARACTER offset (`2`, since `h` and `é`
are two characters) for `start_col` would splice into the middle of `é`'s
2-byte encoding instead. Live-verified by `buf_set_text_live.rs`'s
`text_edit_start_col_is_a_byte_offset_not_a_character_offset`.
