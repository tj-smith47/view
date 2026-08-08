# Wire capture: buffer-content lookup for the picker preview pane

Captured live against the pinned engine per "capture, never recall." Source
of truth for the `nvim_exec_lua` call `EngineHandle::request_preview` issues
to resolve the picker preview pane's text for a candidate path.

## Engine identity

```
$ nvim --version | head -3
NVIM v0.12.4
Build type: Release
LuaJIT 2.1.1785192264
```

Matches `.engine-pin` (`v0.12.4`) exactly.

## Capture method

A standalone Python msgpack-rpc client (no `pynvim`; not installed) spawns
`nvim --clean --headless --listen <socket>` with the same hermetic
`XDG_*`/`HOME` isolation `EngineConfig::isolated()` uses, connects over the
unix socket, and issues `nvim_exec_lua` as a **request** with the candidate
path as a positional vararg, the same calling convention
`REGISTER_MAPPINGS_CHUNK`/`BUFFER_LIST_CHUNK` already use (constant Lua
source, no interpolated caller data). No UI attach is needed: buffer content
is not redraw-derived state.

The Lua chunk under test:

```lua
local path = ...
for _, buf in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_is_loaded(buf) and vim.api.nvim_buf_get_name(buf) == path then
    return { loaded = true, lines = vim.api.nvim_buf_get_lines(buf, 0, -1, false) }
  end
end
return { loaded = false }
```

## 1. Baseline: no buffer open for the path

A file exists on disk at the candidate path (`disk line one` / `disk line
two`) but no buffer has been opened for it in this session.

```
err: None
res: {'loaded': False}
```

`loaded: False` carries no `lines` key at all -- the decoder must not assume
`lines` is present when `loaded` is false, and this is exactly the signal
that tells the caller to fall back to a disk read.

## 2. Buffer opened, unmodified

Driven: `:edit <path>` against the same on-disk file as case 1.

```
edit err: None
err: None
res: {'loaded': True, 'lines': ['disk line one', 'disk line two']}
```

An unmodified, freshly opened buffer's content matches the file verbatim --
the RPC path and the disk-fallback path would agree here, so this case alone
cannot distinguish a correct RPC-backed preview from a buggy disk-only one.
Case 3 is the one that can.

## 3. Buffer opened, modified without saving

Driven from case 2's buffer: `nvim_buf_set_lines(0, 0, -1, false, {three new
lines})`, no `:write`.

```
set_lines err: None
err: None
res: {'loaded': True, 'lines': ['modified line one', 'modified line two', 'modified line three']}
```

The reply reflects the in-memory buffer content, not the still-unmodified
file on disk. **This is the load-bearing case**: it proves the Lua chunk's
`nvim_buf_get_lines` call reads nvim's authoritative text, and it is the
shape the falsifiable preview test asserts against -- a disk-read
implementation would return `['disk line one', 'disk line two']` here
instead, and must fail the test.

## 4. Path with no buffer and no file on disk

```
err: None
res: {'loaded': False}
```

Same `{'loaded': False}` shape as case 1: a path with nothing to preview
either way degrades to "no buffer," and the disk-fallback read (a plain
`std::fs::read` in `view-native`, not RPC) is left to report its own
not-found outcome rather than the RPC layer inventing one.

## Conclusions for the implementation

- `EngineHandle::request_preview(&self, path: &str, generation: u64)` issues
  `nvim_exec_lua` with the chunk above (path as the sole positional vararg),
  tagged `Waiter::Preview { generation }`, mirroring `request_buffer_list`'s
  `Waiter::BufferList` shape exactly: async, never blocks, decodes on the
  reader thread, routes to `pump` as `Msg::PickerPreviewReply { generation,
  path, loaded, lines }` (new `Held::Preview` slot in `damage.rs`, alongside
  `Held::BufferList`).
- `decode_preview_reply` reads `loaded` first; when `loaded` is `true` it
  requires `lines` to be present and decodes it as `Vec<String>`, and when
  `loaded` is `false` (or the reply errors, following
  `decode_buffer_list_reply`'s "error degrades to a safe default"
  precedent) it returns `loaded: false` with no lines, never inventing
  placeholder content.
- `loaded: false` is not an error for the picker: it is the caller's signal
  to issue a plain disk read (`view-native::picker::preview::read_file`,
  legal `std::fs` I/O in `view-native`, not RPC) via
  `Effect::PickerPreviewFallback`, off the paint loop, in the `view` bin
  crate's `Executor` (the one place allowed to depend on both
  `view-engine` and `view-native`). `view-native` itself never opens an RPC
  connection.
- An error reply on this call degrades to `loaded: false` (triggering the
  disk-fallback path) rather than leaving the preview pane stuck on stale
  content from a prior generation, the same "safe default over a stuck
  generation" precedent `decode_buffer_list_reply`/`decode_hl_probe_reply`
  already follow.
