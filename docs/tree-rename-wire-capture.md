# Wire capture: file-tree rename

Captured live against the pinned engine per "capture, never recall." Source
of truth for the `nvim_exec_lua` call `EngineHandle::rename_file` issues to
rename a file on disk without orphaning any buffer nvim has open for it.

## Engine identity

```
$ nvim --version | head -3
NVIM v0.12.4
Build type: Release
LuaJIT 2.1.1785192264
```

Matches `.engine-pin` (`v0.12.4`) exactly.

## Capture method

`nvim --clean -l <script>.lua` runs a Lua script directly inside an
embedded, headless nvim instance (no RPC round trip needed: the chunk under
test never touches the wire itself, only the `vim.*` APIs it calls once
inside nvim), with the same hermetic `HOME`/`XDG_*` isolation
`EngineConfig::isolated()` uses. The script `load()`s the exact chunk text
below as a Lua function and calls it with the same two positional
arguments (`old_path`, `new_path`) `nvim_exec_lua`'s vararg convention
passes, exercising byte-for-byte the same source `nvim_api.rs` embeds.

The Lua chunk under test:

```lua
local old_path, new_path = ...
if vim.uv.fs_stat(new_path) then
  return { ok = false }
end
local function canon(p)
  return vim.uv.fs_realpath(p) or vim.fn.fnamemodify(p, ':p')
end
local wanted = canon(old_path)
local rc = vim.fn.rename(old_path, new_path)
if rc ~= 0 then
  return { ok = false }
end
for _, buf in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_is_loaded(buf) and canon(vim.api.nvim_buf_get_name(buf)) == wanted then
    vim.api.nvim_buf_set_name(buf, new_path)
    break
  end
end
return { ok = true }
```

`wanted` is resolved from `old_path` before the rename happens, while the
file still exists to resolve a real path for -- resolving it afterward would
have `fs_realpath` fail (the file is gone from that location) and fall back
to `fnamemodify`, which still works, but there is no reason to depend on the
fallback when the precise path is available a moment earlier. The same
canonicalizing match `docs/picker-preview-wire-capture.md` uses for buffer
lookup by path is reused here for the same reason: a symlinked tree root
would otherwise never byte-match `nvim_buf_get_name`'s resolved name.

## 1. Rename with a modified, unsaved buffer open

Driven: `:edit old.txt`, then `nvim_buf_set_lines` to modify it in memory
without saving, then the chunk above with `(old.txt, new.txt)`.

```
before rename: name=.../old.txt modified=true
chunk result: ok=true
after rename: name=.../new.txt modified=true
buffer lines: line one|modified line two
src exists: false
dst exists: true
```

**This is the load-bearing case.** The buffer's name follows the rename
(`old.txt` to `new.txt`), its `modified` flag survives the retarget
(`true` before, `true` after), and its unsaved in-memory content is
untouched (`modified line two`, never written to either path on disk). A
rename that lost the modified flag, or left the buffer pointing at
`old.txt`, would fail exactly this assertion -- the shape the falsifiable
rename test in `crates/view-engine/tests/` asserts against.

## 2. Rename onto an existing destination

Two files exist, `a.txt` (`aaa`) and `b.txt` (`bbb`). The chunk runs with
`(a.txt, b.txt)`.

```
chunk result: ok=false
source still readable: true
dest content unchanged: bbb
```

A raw `vim.fn.rename` silently overwrites an existing destination and
reports success (confirmed separately, without the collision guard present
above) -- exactly the data-loss failure mode a file-tree rename must refuse
rather than reproduce. The `vim.uv.fs_stat(new_path)` guard at the top of
the chunk is what turns that into an explicit `ok = false` with both files
left untouched, rather than a silent overwrite the tree would have no way
to warn the user about.

## 3. Rename with no buffer open for the source path

A file exists on disk with nothing open for it. The chunk runs with
`(c.txt, d.txt)`.

```
chunk result: ok=true
dest exists: true
```

The buffer-retarget loop simply finds nothing to retarget and the plain
filesystem rename still succeeds -- the common case for a tree entry that
was never opened.

## Conclusions for the implementation

- `EngineHandle::rename_file(&self, old_path: &str, new_path: &str,
  generation: u64)` issues `nvim_exec_lua` with the chunk above (both paths
  as positional varargs, matching `REGISTER_MAPPINGS_CHUNK`/`PREVIEW_CHUNK`'s
  calling convention), tagged `Waiter::Rename { generation }`, mirroring
  `request_preview`'s shape: async, never blocks, decodes on the reader
  thread, routes to `pump` as `Msg::TreeRenameReply { generation, ok }` (new
  `Held::Rename` slot in `damage.rs`, alongside `Held::Preview`).
- `decode_rename_reply` reads the `ok` key and degrades to `false` for any
  reply shape this crate has not actually observed from the pinned engine
  (a non-map result, a missing key, an error reply), the same "safe default
  over a stuck generation" precedent `decode_preview_reply` and
  `decode_hl_probe_reply` already follow -- a rename this decoder cannot
  confirm succeeded must not trigger the rescan that assumes it did.
- `ok: false` is not routed to any fallback the way `PickerPreviewReply`'s
  `loaded: false` is: a refused rename (collision, or the underlying
  `vim.fn.rename` failing for any other reason `errno` would explain, e.g. a
  read-only filesystem) has nothing else on `view-core`'s side to try, so
  `update()` surfaces it as a notice and leaves the tree's state exactly as
  it was before the rename was issued.
- `BufFilePost` does not fire from `nvim_buf_set_name` on its own (confirmed
  separately by manually issuing `doautocmd BufFilePost` and observing it
  is the manual call, not the rename, that triggers the callback) -- the
  tree's post-rename rescan is therefore issued explicitly by `update()`'s
  `Msg::TreeRenameReply` arm via `TreeState::request_rescan`, rather than
  relying on the autocmd bridge's ordinary write callbacks to notice the
  change.
