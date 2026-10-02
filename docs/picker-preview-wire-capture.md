# Wire capture: buffer-content lookup for the picker preview pane

Captured live against the pinned engine: every value below reflects an actual
run of the pinned binary. Source of truth for the `nvim_exec_lua` call
`EngineHandle::request_preview` issues to resolve the picker preview pane's
text for a candidate path.

## Engine identity

```
$ nvim --version | head -3
NVIM v0.12.4
Build type: Release
LuaJIT 2.1.1785192264
```

Matches `.engine-pin` (`v0.12.4`) exactly.

## Capture method

A standalone Python msgpack-rpc client (`pynvim` absent from the environment)
spawns `nvim --clean --headless --listen <socket>` with the same hermetic
`XDG_*`/`HOME` isolation `EngineConfig::isolated()` uses, connects over the
unix socket, and issues `nvim_exec_lua` as a **request** with the candidate
path, the window's 1-based first line, its line count and the per-line byte cap
(`PREVIEW_LINE_BYTES`, 4096, read out of `view-core`) as positional varargs,
the same calling convention `REGISTER_MAPPINGS_CHUNK`/`BUFFER_LIST_CHUNK`
already use (constant Lua source, no interpolated caller data). No UI attach is
needed: buffer content is not redraw-derived state. The chunk is read out of
`crates/view-engine/src/nvim_api.rs` by the capture script, so the bytes sent
are the bytes shipped.

The Lua chunk under test, verbatim `PREVIEW_WINDOW_CHUNK`:

```lua
local path, first, count, cap = ...
local function canon(p)
  if p == '' then
    return p
  end
  return vim.uv.fs_realpath(p) or vim.fn.fnamemodify(p, ':p')
end
local wanted = canon(path)
for _, buf in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_is_loaded(buf)
    and canon(vim.api.nvim_buf_get_name(buf)) == wanted then
    local lines = vim.api.nvim_buf_get_lines(
      buf, first - 1, first - 1 + count, false)
    for i, line in ipairs(lines) do
      if #line > cap then
        lines[i] = line:sub(1, cap + vim.str_utf_start(line, cap + 1))
      end
    end
    return { loaded = true, lines = lines }
  end
end
return { loaded = false }
```

`nvim_buf_get_lines` takes a 0-based start and an exclusive end, so the reply
holds lines `first` through `first + count - 1`, fewer where the buffer ends
first. The picker asks for 1000 lines around the matched line.

Each line longer than `cap` bytes is cut to `cap` bytes, less the start of a
character the cut would split: `vim.str_utf_start(line, cap + 1)` is `0` when
byte `cap + 1` starts a character and the negative distance back to that
character's first byte otherwise. `cap` is `PREVIEW_LINE_BYTES`, the same cut
the disk read makes.

The comparison canonicalizes both sides (`vim.uv.fs_realpath`, falling back to
`vim.fn.fnamemodify(p, ':p')` for a path that doesn't exist on disk yet, e.g.
an unsaved new-file buffer) so a candidate path reached through a symlink still
matches the buffer opened at its resolved target; bare string equality on
`nvim_buf_get_name` would otherwise miss it and wrongly fall back to a stale
on-disk read. The empty-string guard keeps `[No Name]` scratch buffers (whose
`nvim_buf_get_name` is `''`) from false-positive-matching any candidate path
that happens to canonicalize to nvim's cwd, which is what
`fnamemodify('', ':p')` alone would resolve to. The response shapes below
(`{loaded, lines}`) are unchanged by this and remain accurate as captured.

## 1. Baseline: no buffer open for the path

A file exists on disk at the candidate path (`disk line one`/`disk line two`)
but no buffer has been opened for it in this session. Window: line 1, 1000
lines, as in every case unless one says otherwise.

```
err: None
res: {'loaded': False}
```

`loaded: False` carries no `lines` key at all; the decoder must not assume
`lines` is present when `loaded` is false, and this is exactly the signal that
tells the caller to fall back to a disk read.

## 2. Buffer opened, unmodified

Driven: `:edit <path>` against the same on-disk file as case 1.

```
edit err: None
err: None
res: {'loaded': True, 'lines': ['disk line one', 'disk line two']}
```

An unmodified, freshly opened buffer's content matches the file verbatim; the
RPC path and the disk-fallback path would agree here, so this case alone cannot
distinguish a correct RPC-backed preview from a buggy disk-only one. Case 3 is
the one that can.

## 3. Buffer opened, modified without saving

Driven from case 2's buffer:
`nvim_buf_set_lines(0, 0, -1, false, {three new lines})`, no `:write`.

```
set_lines err: None
err: None
res: {'loaded': True, 'lines': ['modified line one', 'modified line two', 'modified line three']}
```

The reply reflects the in-memory buffer content; the still-unmodified file on
disk plays no part. **This is the load-bearing case**: it records the Lua
chunk's `nvim_buf_get_lines` call reads nvim's authoritative text, and it is
the shape the falsifiable preview test asserts against; a disk-read
implementation would return `['disk line one', 'disk line two']` here instead,
and must fail the test.

The same modified buffer, window line 2, one line:

```
err: None
res: {'loaded': True, 'lines': ['modified line two']}
```

The same modified buffer, window line 10, 1000 lines, past its last line:

```
err: None
res: {'loaded': True, 'lines': []}
```

A window past the end answers `loaded: True` with an empty `lines` array.

## 4. Path with no buffer and no file on disk

```
err: None
res: {'loaded': False}
```

Same `{'loaded': False}` shape as case 1: a path with nothing to preview either
way degrades to "no buffer," and the disk-fallback read (a plain `std::fs`
read in `view-native`, outside RPC entirely) is left to report its own
not-found outcome; the RPC layer invents nothing.

## 5. A line longer than the cap

A file whose first line is 4095 `x` bytes, then `€` (three bytes) straddling the
cap, then `tail`, and whose second line is `short`, opened with `:edit`. The
reply's byte lengths, the first line's last three characters and the second
line:

```
err: None
lens: [4095, 5] tail: 'xxx' second: short
```

The first line ends before the character the cut would split, and the line
under it arrives whole.

## Conclusions for the implementation

- `EngineHandle::preview_buffer_window(&self, path, first_line, line_count,
  generation)` issues `nvim_exec_lua` with the chunk above (path, first line,
  line count and `PREVIEW_LINE_BYTES` as positional varargs) through
  `request_preview`, tagged `Waiter::Preview { generation, path }`, mirroring
  `request_buffer_list`'s `Waiter::BufferList` shape exactly: async, blocks
  nothing, decodes on the reader thread, routes to `pump` as
  `Msg::PickerPreviewReply { generation, path, loaded, lines }` (new
  `Held::Preview` slot in `damage.rs`, alongside `Held::BufferList`).
- `decode_preview_reply` reads `loaded` first; when `loaded` is `true` it
  requires `lines` to be present and decodes it as `Vec<String>`, and when
  `loaded` is `false` (or the reply errors, following
  `decode_buffer_list_reply`'s "error degrades to a safe default" precedent) it
  returns `loaded: false` with no lines; placeholder content is never invented.
- `loaded: false` carries no error for the picker: it is the caller's signal to
  issue a plain disk read of the same window
  (`view-native::picker::preview::read_window`, legal `std::fs` I/O in
  `view-native`, outside RPC) via `Effect::PickerPreviewFallbackWindow`, off
  the paint loop, in the `view` bin crate's `Executor` (the one place allowed
  to depend on both `view-engine` and `view-native`). `view-native` itself
  never opens an RPC connection.
- An error reply on this call degrades to `loaded: false` (triggering the
  disk-fallback path), so the preview pane never sits stuck on stale content
  from a prior generation, the same "safe default over a stuck generation"
  precedent `decode_buffer_list_reply`/`decode_hl_probe_reply` already follow.
