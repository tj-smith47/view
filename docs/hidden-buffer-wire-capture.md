# Wire capture: hidden-buffer creation, reuse, and deletion

Captured live against the pinned engine per "capture, never recall." Source
of truth for `LOAD_HIDDEN_CHUNK`, the `nvim_exec_lua` chunk
`EngineHandle::load_hidden` issues for `RpcCall::LoadHidden`, and for
`EngineHandle::release_hidden`'s `nvim_buf_delete` call.

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
(`nvim_create_buf`, `nvim_buf_delete`) and `nvim_exec_lua` running the exact
chunk text `LOAD_HIDDEN_CHUNK` embeds.

## 1. `nvim_create_buf(false, false)` never lists the buffer

```
nvim_create_buf(false, false) -> buf=2
nvim_get_option_value('buflisted', {buf=2}) -> false
```

A buflisted-filtered scan (the same filter `BUFFER_LIST_CHUNK` uses for the
picker's `Source::Buffers`) over `nvim_list_bufs()` right after creating and
naming the buffer never includes it -- only nvim's own default buffer (`1`)
shows up. Confirms the falsifiable check: a hidden buffer never reaches
`Msg::PickerBufferList`.

## 2. `nvim_buf_set_lines` marks the buffer modified -- `bufload` does not

Loading a file's content into a freshly created buffer via
`vim.api.nvim_buf_set_lines` (the only way to populate a buffer whose
content this chunk itself reads through `vim.fn.readfile`, since
`nvim_create_buf` starts the buffer empty) sets `modified = true`, unlike
`vim.fn.bufload`, which does not. The
chunk resets `vim.bo[buf].modified = false` immediately after the initial
load specifically to undo this side effect: a buffer that merely mirrors
what is already on disk must not read as having unsaved changes nobody
made.

## 3. The existing-buffer lookup, scanned before `nvim_create_buf`

Reusing `PREVIEW_CHUNK`'s own canonicalized name-match scan over
`nvim_list_bufs()` (symlink-safe, `loaded buffer wins over disk`):

```
-- first call, path has no buffer yet
LOAD_HIDDEN_CHUNK(path) -> { buf = 2, created = true,  changedtick = 2 }

-- second call, same path
LOAD_HIDDEN_CHUNK(path) -> { buf = 2, created = false, changedtick = 2 }
```

The second call's scan finds the buffer the first call named via
`nvim_buf_set_name` and returns it unchanged -- no second `nvim_create_buf`,
no second read of the file. `created` tells only which call made the
buffer; both calls return the identical handle.

## 4. A path with no file on disk yet resolves to an empty, unmodified buffer

```
LOAD_HIDDEN_CHUNK(new_file_path) -> { buf = 3, created = true, changedtick = 2 }
nvim_buf_get_lines(3, 0, -1, false) -> ['']
nvim_get_option_value('modified', {buf=3}) -> false
```

`vim.fn.readfile` on a nonexistent path fails; the chunk's `pcall` catches
that and falls back to an empty `lines` table, which is what the file will
be created as once something writes to it -- the new-file proposal's own
case.

## 5. The existing-buffer lookup finds a buffer regardless of its modified state

```
nvim_buf_set_lines(2, 0, 1, false, ['EDITED'])   -- simulates an accepted hunk write
nvim_get_option_value('modified', {buf=2}) -> true
LOAD_HIDDEN_CHUNK(path) -> { buf = 2, created = false, changedtick = 3 }
```

A second `load_hidden` for the same path after edits have already landed in
the buffer still resolves to the same buffer, never a fresh reload from
disk that would discard those edits -- the scan matches on buffer identity
(name), not on modified state.

## 6. `nvim_buf_delete` on an unmodified, window-invisible buffer succeeds silently

```
nvim_buf_delete(buf, {}) -> ok, buffer gone from nvim_list_bufs()
```

## 7. `nvim_buf_delete` refuses a MODIFIED buffer, content left untouched

```
nvim_buf_delete(buf, {}) -> error: 'Failed to unload buffer.'
nvim_buf_get_lines(buf, 0, -1, false) -> unchanged, still ['EDITED', ...]
```

No `force` is ever passed. This is the safety net `release_hidden` relies
on rather than reimplementing: a hold whose buffer has unsaved accepted
edits when the refcount reaches zero is not deleted -- nvim's own default
`force: false` refuses it, and the edits survive as an orphaned, still-loaded,
unlisted buffer nvim will hand back to whatever next names this path (a
`load_hidden` retry, or a real `:edit`), rather than being silently
discarded.

## 8. `nvim_buf_delete` does NOT refuse a buffer that is visible in a window

An earlier capture session recorded this as a refusal (mirroring case 7's
modified-buffer refusal). Re-captured against the same pinned engine, in the
exact shape `release_hidden` actually faces -- a buffer opened normally via
`:edit` (this crate's own `OPEN_FILE_CHUNK`, not a bare
`nvim_win_set_buf`), the sole window showing it, ui-attached:

```
:edit path -> current buffer = 1, win_findbuf(1) = [win] -- confirms visible
nvim_buf_delete(1, {}) -> Ok(Nil), no error
nvim_list_bufs() -> [1] -- one buffer still present...
nvim_get_current_buf() -> 2 -- ...but it is a fresh empty buffer, not the
                               deleted one: the window's real content is gone
```

The same result holds with a second, unrelated buffer already open (deleting
the window's current buffer just switches the window to that other buffer
instead of a fresh one). Deleting a buffer nvim itself has no window context
for (the API caller passes no window) does not raise "Failed to unload
buffer." here -- nvim substitutes a replacement into every window that was
showing it and proceeds. `release_hidden` cannot rely on nvim refusing this
the way it reliably refuses a modified buffer (case 7): it must check
`vim.fn.win_findbuf` itself and skip the delete outright when the list is
non-empty, never attempting it and hoping for a refusal.

## 9. `RELEASE_HIDDEN_CHUNK`'s own `win_findbuf` guard, verified against both refusal shapes

```
-- window-visible buffer: guard trips, delete never attempted
win_findbuf(buf) -> [win]
RELEASE_HIDDEN_CHUNK(buf) -> (no call to nvim_buf_delete)
nvim_get_current_buf() -> unchanged, still the same buffer
nvim_buf_get_lines(buf, ...) -> unchanged, the user's real content

-- window-invisible, modified buffer: guard passes, delete attempted and
-- refused by nvim itself (case 7's own refusal), pcall swallows the error
win_findbuf(buf) -> []
RELEASE_HIDDEN_CHUNK(buf) -> pcall(nvim_buf_delete, buf, {}) fails silently
nvim_list_bufs() -> buf still present, content unchanged

-- window-invisible, unmodified buffer: guard passes, delete succeeds
win_findbuf(buf) -> []
RELEASE_HIDDEN_CHUNK(buf) -> buffer gone from nvim_list_bufs()
```

The window-visibility check runs in Lua, before ever calling
`nvim_buf_delete`, rather than trusting nvim to refuse on its own (case 8
disproved that trust) -- the modified-buffer case still relies on nvim's own
refusal (case 7), which held up under this same re-capture.
