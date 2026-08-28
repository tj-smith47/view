# Wire capture: buffer-list enumeration for `Source::Buffers`

Captured live against the pinned engine per "capture, never recall." Source
of truth for the `nvim_exec_lua` call `EngineHandle::request_buffer_list`
issues to resolve the picker's `Source::Buffers` corpus.

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
unix socket, and issues `nvim_exec_lua` as a **request** (this call, unlike
`nvim_input`, genuinely needs its reply -- there is nothing to stream and
nothing nvim is blocked inside). No UI attach is needed: the buffer list is
not redraw-derived state.

The Lua chunk under test, verbatim `BUFFER_LIST_CHUNK`:

```lua
local out = {}
for _, buf in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_is_loaded(buf) and vim.bo[buf].buflisted then
    out[#out + 1] = {
      bufnr = buf,
      name = vim.api.nvim_buf_get_name(buf),
      modified = vim.bo[buf].modified,
    }
  end
end
return out
```

## 1. Baseline: one scratch buffer, nothing named

```
err: None
res: [{'bufnr': 1, 'modified': False, 'name': ''}]
```

A fresh session's sole scratch buffer round-trips as one entry with an empty
`name`. `decode_buffer_list_reply` must not choke on an empty string, and
the picker's `PickerItem` label for it needs a placeholder (`[No Name]`,
matching nvim's own convention) rather than an empty row.

## 2. Two named files open, one unlisted `:help` window, current buffer switched

Driven: `:edit a.txt`, `:edit b.txt`, `:help`, `:buffer 1`.

```
err: None
res: [
  {'bufnr': 1, 'modified': False, 'name': '/tmp/view-wire-capture-a.txt'},
  {'bufnr': 2, 'modified': False, 'name': '/tmp/view-wire-capture-b.txt'},
]
```

Three buffers exist on this session (`1`, `2`, and the help buffer opened by
`:help`), but only two come back: the help buffer's `'buftype'` is
`"help"` and nvim does not mark it `buflisted`. **This confirms the
`buflisted` filter in the Lua chunk is load-bearing, not decorative** -- a
picker built without it would surface `:help`/quickfix/terminal scratch
buffers a user never asked to jump to, exactly what `:ls` (and every
Telescope-style buffer picker) also excludes by default. The order returned
is buffer-number order (`nvim_list_bufs`'s own order), not MRU -- the picker
does not attempt recency ranking; nucleo's match score is the only ordering
signal.

## 3. Error shape: malformed Lua

```
err: [1, 'Lua: [string "<nvim>"]:1: \'=\' expected near \'is\'']
res: None
```

`error` is a two-element array `[error_type, message]` (the same
`(Value, Value)` shape every other `nvim_exec_lua`/generic-request error on
this engine takes -- matching `decode_hl_probe_reply`'s and
`decode_mapping_claims`'s existing "error reply degrades to empty/default"
handling). Since the Lua chunk here is fixed, constant source (no
interpolated caller data, same discipline as `REGISTER_MAPPINGS_CHUNK`), a
non-nil error on this call can only mean something is wrong with the engine
connection itself, not with the request's arguments -- `decode_buffer_list_reply`
degrades an error reply to an empty list, the same "confirmed nothing"
default `decode_mapping_claims` uses, rather than leaving a picker session
stuck with no items and no explanation.

## Conclusions for the implementation

- `EngineHandle::request_buffer_list(&self, generation: u64)` issues
  `nvim_exec_lua` with the chunk above, tagged `Waiter::BufferList
  { generation }`, mirroring `request_probe`'s `Waiter::HlProbe` shape
  exactly: async, never blocks, decodes on the reader thread, routes to
  `pump` as `Msg::PickerBufferList { generation, names }` (new `Held` slot in
  `damage.rs`, alongside `Held::Probe`/`Held::Claims`).
- The reply's `name` field is used as-is for a real path; an empty string is
  rendered as `[No Name]` by the picker's `PickerItem` label, not filtered
  out (an unsaved scratch buffer is still a legitimate jump target).
- `buflisted` filtering happens in the Lua chunk itself (cheaper than
  shipping every buffer over the wire and filtering client-side), matching
  `:ls`'s own default buffer set.
- An error reply degrades to an empty buffer list, following the existing
  `decode_hl_probe_reply`/`decode_mapping_claims` "safe default over a stuck
  generation" precedent.
