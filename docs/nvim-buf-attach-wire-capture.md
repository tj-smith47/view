# Wire capture: `nvim_buf_attach` notification shape

Captured live against the pinned engine per "capture, never recall." Source
of truth for `RpcCall::BufAttach`/`BufDetach` and the `nvim_buf_lines_event`
decode `crate::handle` routes into `Msg::BufTextChanged`.

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
`nvim --clean --headless --listen <socket>`, connects over the unix socket,
and issues raw msgpack-RPC requests (`nvim_buf_attach`, `nvim_buf_detach`,
`nvim_buf_set_lines`), draining whatever notifications arrive on the same
connection afterward. `nvim_buf_attach`/`nvim_buf_detach` are channel-scoped
(`:help api-buffer-updates`): the connection that issues the attach is the
one that receives every subsequent notification, with no `nvim_ui_attach`
required first -- confirmed by every capture below, none of which ever
attaches a UI.

## 1. `send_buffer: false` never fires an initial whole-buffer event

Buffer 0 reset to `["line1", "line2", "line3"]`, then attached with
`send_buffer: false`:

```
nvim_buf_attach(0, false, {}) -> true
  notification: nvim_buf_changedtick_event(buf=Ext(0,[1]), 3)
  -- NO nvim_buf_lines_event fires
```

Contrast, same buffer state, attached with `send_buffer: true` instead:

```
nvim_buf_attach(0, true, {}) -> true
  notification: nvim_buf_lines_event(buf=Ext(0,[1]), 5, 0, -1,
                                      ["a", "b", "c"], false)
```

Confirms the brief's latency claim: `send_buffer: false` is what keeps
attach itself from streaming the whole buffer -- `RpcCall::BufAttach` must
pass `false`, never the default-looking `true`, or every attach costs an
event proportional to buffer size before a single keystroke has happened.

## 2. `nvim_buf_lines_event`'s exact payload shape

Method name: `"nvim_buf_lines_event"`. Positional params, in order:

```
[buf, changedtick, firstline, lastline, linedata, more]
```

- `buf` -- a msgpack `Ext` value (type 0), payload the msgpack-encoded
  buffer number itself (`Ext(0, [1])` decodes to buffer `1`), the same
  `Ext` shape `ui_events.rs::decode_ext_handle` already unwraps for
  `Tabpage`/`Window` handles.
- `changedtick` -- `u64`, nvim's own edit counter for this buffer.
- `firstline`/`lastline` -- 0-indexed, half-open `[firstline, lastline)`
  range of the OLD buffer the edit replaced -- not the new range `linedata`
  occupies after applying (see the insert case below, where the two
  diverge).
- `linedata` -- array of strings, the new content for the replaced range.
  Empty for a pure deletion.
- `more` -- `bool`, `true` when this notification is one of several still
  to arrive for a single logical change (`:help api-buffer-updates`
  documents this for `:%s` -style batched edits); every capture below
  produced a single `nvim_buf_lines_event` per `nvim_buf_set_lines` call
  with `more: false`.

## 3. One edit produces exactly one event, bounding only the changed range

Buffer reset to `["one", "two", "three", "four"]`, attached with
`send_buffer: false` (the changedtick_event from attach drained and
discarded first).

Single-line replace (`nvim_buf_set_lines(0, 1, 2, false, ["TWO-EDITED"])`):

```
nvim_buf_lines_event(buf, 8, 1, 2, ["TWO-EDITED"], false)
```

Exactly one notification; `firstline: 1, lastline: 2` bounds only the
replaced line; `linedata` contains only the new line's own content, never
the other three lines still sitting untouched in the buffer. This is the
brief's falsifiable check, confirmed live.

Insert two new lines at position 2 (grows the buffer; old range is empty
since nothing is replaced, only inserted):

```
nvim_buf_set_lines(0, 2, 2, false, ["NEW-A", "NEW-B"])
  -> nvim_buf_lines_event(buf, 9, 2, 2, ["NEW-A", "NEW-B"], false)
```

`firstline == lastline == 2`: an empty old range at the insertion point,
`linedata` carrying both new lines. `firstline`/`lastline` name the OLD
range being replaced, not a range sized to match `linedata`'s length --
implementers must not assume `lastline - firstline == linedata.len()`.

Delete a line (shrinks the buffer):

```
nvim_buf_set_lines(0, 0, 1, false, [])
  -> nvim_buf_lines_event(buf, 10, 0, 1, [], false)
```

`linedata: []`, the deletion convention `nvim_buf_set_text`'s own empty-
`lines` deletion (`docs/buf-set-text-wire-capture.md`) mirrors.

Final buffer read back after all three edits: `["TWO-EDITED", "NEW-A",
"NEW-B", "three", "four"]` -- matches applying each event in order,
confirming the events alone are sufficient to rebuild buffer state without
a fresh whole-buffer read.

## 4. `nvim_buf_detach_event` on detach; no further events reach a detached buffer

```
nvim_buf_detach(0) -> true
  notification: nvim_buf_detach_event(buf=Ext(0,[1]))
nvim_buf_set_lines(0, 0, 1, false, ["A-EDITED"])   -- edit after detach
  -> NO further notification arrives for this edit
```

Method name: `"nvim_buf_detach_event"`, single positional param (`buf`,
the same `Ext` shape). Confirms `RpcCall::BufDetach`'s contract: once
issued, further edits to that buffer produce no more `Msg::BufTextChanged`.

## 5. Two attached buffers never cross-deliver events

Buffer 1 (`["one", ...]` from capture #3) and a freshly created buffer 2
(`["b2-line1"]`) both attached with `send_buffer: false` on the same
connection. Editing ONLY buffer 2:

```
nvim_buf_set_lines(<buf2>, 0, 1, false, ["b2-EDITED"])
  -> nvim_buf_lines_event(buf=Ext(0,[2]), 3, 0, 1, ["b2-EDITED"], false)
```

Exactly one notification, naming buffer 2's own `Ext` handle -- nothing
arrives referencing buffer 1. `buf` is nvim's own disambiguator on the
wire; the generation-stamping this task adds on top (`Msg::BufTextChanged`'s
`generation` field) is client-side bookkeeping for which rebase state
machine a `buf` maps to at the moment its `BufAttach` was issued, layered
over an already-unambiguous wire signal, not a workaround for wire
ambiguity.

## Conclusions for the implementation

- `RpcCall::BufAttach` issues `nvim_buf_attach(buf, false, {})` -- `false`
  is load-bearing per capture #1.
- `crate::handle`'s notification router gains two new method arms:
  `"nvim_buf_lines_event"` (decoded into `Msg::BufTextChanged`, with `buf`
  resolved via `decode_ext_handle` and `generation` filled from the
  connection's own buf-to-generation map, populated by `BufAttach` and
  cleared by `BufDetach`) and `"nvim_buf_detach_event"` (clears that map
  entry so a stray notification racing a detach cannot resurrect it).
- `firstline`/`lastline` carry through to `Msg::BufTextChanged` exactly as
  received -- half-open, old-range semantics, never re-derived from
  `linedata.len()`.
