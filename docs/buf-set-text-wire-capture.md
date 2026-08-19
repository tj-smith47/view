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

## 5. `undojoin: true` requires a prior undoable change to join onto

`:help undojoin` documents `E790` ("undojoin is not allowed after undo")
for the specific case of joining immediately after an `undo`. Issuing
`undojoin: true` as the very first edit against a buffer whose most recent
action was itself an ordinary edit (e.g. the `nvim_buf_set_lines` reset
every test here performs) does NOT error -- it joins onto that reset,
which is expected and matches production usage: `BufSetText`'s own
contract (see `RpcCall::BufSetText`'s doc) never issues `undojoin: true`
for a hunk with no accepted edit before it in the same batch.
