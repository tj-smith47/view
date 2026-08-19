# Wire capture: `nvim_buf_attach` notification shape

Captured live against the pinned engine per "capture, never recall." Source
of truth for `RpcCall::BufAttach`/`BufDetach` and the `nvim_buf_lines_event`/
`nvim_buf_detach_event` decode `crate::handle` routes into
`Msg::BufTextChanged`/`Msg::BufDetached`.

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
one `nvim --clean --headless --listen <socket>`, connects over the unix
socket, and issues raw msgpack-RPC requests across a single continuous
session, draining whatever notifications arrive on the same connection
after each request. Every section below is one contiguous run of that one
session, in the order shown -- not spliced from separate captures -- so
buffer numbers, changedtick values, and detach events are all consistent
with each other exactly as nvim produced them. `nvim_buf_attach`/
`nvim_buf_detach` are channel-scoped (`:help api-buffer-updates`): the
connection that issues the attach is the one that receives every subsequent
notification, with no `nvim_ui_attach` required first -- confirmed by every
capture below, none of which ever attaches a UI.

## 1. `send_buffer: false` never fires an initial whole-buffer event

Buffer 1 (nvim's default buffer, resolved as `Ext(0,[1])` on the wire) reset
to `["line1", "line2", "line3"]`, then attached with `send_buffer: false`:

```
nvim_buf_attach(0, false, {}) -> true
  notification: nvim_buf_changedtick_event(buf=Ext(0,[1]), 3)
  -- NO nvim_buf_lines_event fires
```

Contrast, same session, buffer reset to `["a", "b", "c"]` and re-attached
(after detaching first) with `send_buffer: true` instead:

```
nvim_buf_attach(0, true, {}) -> true
  notification: nvim_buf_lines_event(buf=Ext(0,[1]), 4, 0, -1, ["a", "b", "c"], false)
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
- `more` -- `bool`, documented (`:help api-buffer-updates`) to mean this
  notification is one of several still to arrive for a single logical
  change. Every capture in this document produced `more: false`, including
  a 200-line `:%s` substitution (section 4 below) -- this session never
  exhibited `more: true` live. Treat it as a real wire value to read, not
  as evidence it never fires under other conditions this capture did not
  exercise (a `:%s` across window/fold boundaries, or a substitution nvim's
  own batching heuristics split differently, might still produce it).

## 3. One edit produces exactly one event, bounding only the changed range

Buffer reset to `["one", "two", "three", "four"]`, attached with
`send_buffer: false` (the changedtick_event from attach drained and
discarded first).

Single-line replace (`nvim_buf_set_lines(0, 1, 2, false, ["TWO-EDITED"])`):

```
nvim_buf_lines_event(buf, 6, 1, 2, ["TWO-EDITED"], false)
```

Exactly one notification; `firstline: 1, lastline: 2` bounds only the
replaced line; `linedata` contains only the new line's own content, never
the other three lines still sitting untouched in the buffer. This is the
brief's falsifiable check, confirmed live.

Insert two new lines at position 2 (grows the buffer; old range is empty
since nothing is replaced, only inserted):

```
nvim_buf_set_lines(0, 2, 2, false, ["NEW-A", "NEW-B"])
  -> nvim_buf_lines_event(buf, 7, 2, 2, ["NEW-A", "NEW-B"], false)
```

`firstline == lastline == 2`: an empty old range at the insertion point,
`linedata` carrying both new lines. `firstline`/`lastline` name the OLD
range being replaced, not a range sized to match `linedata`'s length --
implementers must not assume `lastline - firstline == linedata.len()`.

Delete a line (shrinks the buffer):

```
nvim_buf_set_lines(0, 0, 1, false, [])
  -> nvim_buf_lines_event(buf, 8, 0, 1, [], false)
```

`linedata: []`, the deletion convention `nvim_buf_set_text`'s own empty-
`lines` deletion (`docs/buf-set-text-wire-capture.md`) mirrors.

Final buffer read back after all three edits: `["TWO-EDITED", "NEW-A",
"NEW-B", "three", "four"]` -- matches applying each event in order,
confirming the events alone are sufficient to rebuild buffer state without
a fresh whole-buffer read.

## 4. A 200-line `:%s` produces ONE event, not several -- `more` stays `false`

Same session, same buffer reset to 200 lines (`row0` .. `row199`), attached
with `send_buffer: false`, then a single `:%s/row/ROW/` substituting every
line:

```
nvim_command("%s/row/ROW/")
  -> nvim_buf_lines_event(buf=Ext(0,[1]), tick=10, firstline=0, lastline=200,
                           linedata=<200 lines>, more=false)
```

Exactly one notification for the whole substitution, `firstline: 0,
lastline: 200` bounding the entire replaced range in one shot, `more:
false`. This directly contradicts an earlier draft of this document, which
attributed `more: true` to `:%s`-style batching without having captured it
live -- that claim was never observed and is retracted. If `nvim_buf_attach`
consumers ever need to fold `more: true` continuations, that behavior
remains undemonstrated by this document; treat it as an open question, not
an implemented-and-verified case.

## 5. `nvim_buf_detach_event` on detach; no further events reach a detached buffer

Same session, buffer reset to `["one", "two"]`, attached with
`send_buffer: false`:

```
nvim_buf_detach(0) -> true
  notification: nvim_buf_detach_event(buf=Ext(0,[1]))
nvim_buf_set_lines(0, 0, 1, false, ["A-EDITED"])   -- edit after detach
  -> NO further notification arrives for this edit
```

Method name: `"nvim_buf_detach_event"`, single positional param (`buf`,
the same `Ext` shape). Confirms `RpcCall::BufDetach`'s contract: once
issued, further edits to that buffer produce no more `Msg::BufTextChanged`
-- and nvim's own confirmation event fires even for a SELF-initiated
detach, not only a detach nvim decides to force; `crate::handle`'s reader
thread relies on this by removing the local attach-generation entry
proactively when `buf_detach` is called (not waiting for this event), so a
self-initiated detach's own confirmation, arriving after the entry is
already gone, produces no `Msg::BufDetached` -- only a detach nvim initiates
unasked (section 7 below) finds the entry still present and emits one.

## 6. Two attached buffers never cross-deliver events

Same session: buffer 1 (`["one", "two"]` from section 5) still attached,
plus a freshly created buffer 2 (`Ext(0,[2])`, `["b2-line1"]`) attached with
`send_buffer: false`. Editing ONLY buffer 2:

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

## 7. `:edit!` (nvim-initiated reload) fires `nvim_buf_detach_event` with no request from this connection

Same session: opened a fresh file on disk (`/tmp/wire_capture_edit_bang.txt`,
`["disk-line1", "disk-line2"]`) via `nvim_command("edit <path>")`, resolved
its buffer handle (`Ext(0,[3])`), and attached it with `send_buffer: false`
-- this connection never sent `nvim_buf_detach` for this buffer. The file
was then rewritten on disk out from under nvim, and `:edit!` forced a
reload of the now-attached buffer:

```
nvim_command("edit!")
  -> nvim_buf_detach_event(buf=Ext(0,[3]))
```

One notification, no request from this connection preceded it. This is the
nvim-INITIATED detach case `Msg::BufDetached` exists for: since this
connection never called `buf_detach` for this buffer, the local
attach-generation entry was still present when this event arrived, so
`crate::handle`'s reader thread finds it, removes it, and emits
`Msg::BufDetached { buf, generation }` -- the rebase state machine's only
signal that its subscription died out from under it, distinct from the
silent local-only removal a self-initiated detach performs (section 5).

## 8. `u` (undo) after a large `:%s` is the actual burst trigger, not `:%s` itself

Same session, same 200-line buffer and `:%s/row/ROW/` from section 4 above
(one event, `tick=4`, `firstline=0, lastline=200, more=false` -- reproduced
identically here, confirming section 4's claim still holds). The follow-up
`u` (`nvim_command("undo")`) that reverts it is the burst:

```
nvim_command("undo")
  -> 200x nvim_buf_lines_event(buf=Ext(0,[1]), tick=5..204, firstline=N, lastline=N+1,
                                linedata=[<one line>], more=false)
     (N descending 199 -> 0, one event per line, in that order)
  -> 1x nvim_buf_changedtick_event
```

First three: `(tick=5, firstline=199, lastline=200, linedata=["row199"])`,
`(tick=6, firstline=198, lastline=199, linedata=["row198"])`, `(tick=7,
firstline=197, lastline=198, linedata=["row197"])`. Last three: `(tick=202,
firstline=2, lastline=3, linedata=["row2"])`, `(tick=203, firstline=1,
lastline=2, linedata=["row1"])`, `(tick=204, firstline=0, lastline=1,
linedata=["row0"])`. All 200 are single-line (`lastline - firstline == 1`,
`linedata.len() == 1`), all `more: false`, and `tick` is strictly
consecutive across every one of them (`5, 6, 7, ..., 204`) -- tick-coherent,
confirming these are 200 real, individually-applied edits nvim is replaying
one line at a time, not one logical change nvim merely reports in pieces.
Reading the buffer back afterward confirms the full revert (`["row0",
"row1", "row2", ...]`, 200 lines).

This inverts the naive assumption section 4 might invite: the substitution
that TOUCHES 200 lines produces exactly one event, but UNDOING it produces
200. `:%s` itself is not the burst case a sink-overrun defense needs to
survive -- a single `u` after ANY multi-line change is, since nvim's undo
mechanism reverts line-by-line rather than replaying the original command.
This is the live evidence behind `Msg::BufTextChanged::desynced` and the
reader thread's drop-detection: the burst that can realistically outrun a
bounded sink is an undo (or redo) of a large edit, not a large edit itself.

## Conclusions for the implementation

- `RpcCall::BufAttach` issues `nvim_buf_attach(buf, false, {})` -- `false`
  is load-bearing per section 1.
- `crate::handle`'s notification router gains two method arms:
  `"nvim_buf_lines_event"` (decoded into `Msg::BufTextChanged`, with `buf`
  resolved via `decode_ext_handle` and `generation` filled from the
  connection's own buf-to-generation map, populated by `BufAttach` and
  cleared by `BufDetach`) and `"nvim_buf_detach_event"` (clears that map
  entry so a stray notification racing a detach cannot resurrect it, and
  emits `Msg::BufDetached` only when the entry was still present -- the
  nvim-initiated case in section 7, never the self-initiated case in
  section 5).
- `firstline`/`lastline` carry through to `Msg::BufTextChanged` exactly as
  received -- half-open, old-range semantics, never re-derived from
  `linedata.len()`.
- A dropped or malformed `nvim_buf_lines_event` for an attached buffer
  cannot be silently treated as "nothing changed" -- the next successfully
  decoded event for that buffer must own up to the gap. See
  `Msg::BufTextChanged::desynced`'s own doc comment for the contract.
