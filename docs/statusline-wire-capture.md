# Wire capture: statusline segment events

Captured live against the pinned engine per "capture, never recall." Source
of truth for `view_core::native::statusline`'s `UiEvent` inputs.

## Engine identity

```
$ nvim --version | head -1
NVIM v0.12.4
```

Matches `.engine-pin` (`v0.12.4`).

## Capture method

Same standalone Python msgpack-rpc client pattern as
`docs/toast-routing-wire-capture.md`: `nvim --embed --clean`, hermetic
`XDG_*`/`HOME` isolation, `select.select()` + raw `os.read()` on the child's
stdout fd (a buffered-`io` + `os.set_blocking` read hangs against this
child -- abandoned), `nvim_input` driven as a fire-and-forget notification,
full ext-option set at attach.

## Finding: content chunk shape is `[attr_id, text_chunk, hl_id]`, a 3-tuple

Confirmed against the pinned engine's own doc
(`share/nvim/runtime/doc/api-ui-events.txt`, `msg_show`'s `content` field
doc, which `msg_showmode`/`msg_showcmd`/`msg_ruler` explicitly inherit):

```
content
    Array of `[attr_id, text_chunk, hl_id]` tuples, ...
```

Live capture matches exactly:

```
['msg_showmode', [[[15, 'recording @q', 11]]]]
```

`view-engine`'s existing `decode_content_chunks` (used today for `msg_show`
and `cmdline_show`) already discards the third field and returns
`Vec<(u64, String)>` -- `(attr_id, text)`. This is reused verbatim for the
three new decode functions; no new chunk-decoding logic is needed.

## Finding: `msg_showmode` carries `recording @q` verbatim (spec §9 MUST)

```
=== start macro recording (qq) ===
['msg_showcmd', [[[0, 'qq', 0]]]]
['flush', []]
['msg_showmode', [[[15, 'recording @q', 11]]]]
['msg_showcmd', [[]]]
['flush', []]
```

Rendering `msg_showmode`'s content verbatim satisfies "macro recording must
always be visible" with no separate derivation: nvim itself puts the literal
string `recording @q` into the event.

## Finding: `msg_showmode` concatenates mode text and macro text with no separator

Recording a macro while in insert mode produces a single content chunk whose
text is the two states run together with no space:

```
=== qq then enter insert mode ===
...
['msg_showmode', [[[15, '-- INSERT --recording @q', 11]]]]
```

The statusline segment must render `msg_showmode`'s content as one opaque
string -- it is not safe to assume "mode" and "macro" are independently
extractable sub-fields; nvim has already fused them by the time they reach
the wire.

## Finding: redraw batching can carry multiple calls of the same event name in one tuple

Setting `laststatus=0` and moving the cursor produced a batch entry with two
`msg_showmode` calls back-to-back within one flush cycle:

```
['msg_showmode', [[[15, 'recording @q', 11]]], [[[15, 'recording @q', 11]]]]
```

This is the same batching nvim already uses for `grid_line` (multiple
`[grid_line, args...]` calls fused into one `[name, args1, args2, ...]`
tuple). `view-engine`'s existing `decode_redraw` already handles this
generically (`for tuple in arg_tuples { events.push(decode_event(...)) }`,
`ui_events.rs:19-36`) -- it emits one `UiEvent` per call in wire order. No
special-casing is needed for the new statusline events: `StatuslineState`
applies each event as it arrives, so a same-flush duplicate is simply
applied twice with identical effect (last write wins, matching every other
`Ui*Event` state field on `Model`).

## Finding: `msg_showcmd` carries pending keys and pending operators

```
=== pending count (leading 12, no terminator) ===
['msg_showcmd', [[[0, '12', 0]]]]

=== clear pending count (Esc) ===
['msg_showcmd', [[[0, '12^[', 0]]]]
['msg_showcmd', [[]]]

=== pending operator (d, no motion yet) ===
['msg_showcmd', [[[0, 'd', 0]]]]
['mode_change', ['operator', 7]]
```

Empty content (`[[]]`) hides the segment, same convention as `msg_showmode`.
No separate "pending operator" event exists; `d` awaiting a motion is
indistinguishable on the wire from any other pending-keys `msg_showcmd`
payload -- the statusline does not need a distinct operator-pending branch.

## Finding: `msg_ruler` fires on cursor move and carries the full ruler text

```
=== set laststatus=0, force ruler event ===
['msg_ruler', [[[1, '0,0-1         All', 63]]]]

=== cursor move (l) === (from the populate-buffer section)
['msg_ruler', [[[1, '1,26          All', 63]]]]

=== search forward for repeated token (cat) ===
['msg_ruler', [[[1, '1,5           All', 63]]]]
```

`laststatus=0` (required by the option-supersession design, already
implemented in `view-native::supersede`) makes nvim emit `msg_ruler` on
every cursor move
exactly as the brief assumes -- no additional engine configuration needed.

## Finding: `search_count` is a real, reachable `msg_show` kind, captured live

The prior wire-capture pass could not trigger `search_count` (a `:put`
quoting failure). This session avoided that by populating the buffer with
`i...<Esc>` insert instead of `:put`:

```
=== search forward for repeated token (cat) ===
['msg_show', ['search_cmd', [[0, '/cat                 ', 0]], False, False, False, 1, '']]
['msg_show', ['search_count', [[0, '/cat          W [1/2]', 0]], True, False, False, 2, '']]

=== next match (n) ===
['msg_show', ['search_count', [[0, '/cat            [2/2]', 0]], True, False, False, 4, '']]
```

`search_count`'s content is plain text (`/cat [N/M]` or `/cat W [N/M]` when
the search wrapped) already routed through `Route::Statusline` in
`toast.rs`'s `route()`. It arrives through the existing `UiEvent::MsgShow`
path (today's decoder), not a new event -- `StatuslineState` consumes it via the
same `MsgShow{kind: "search_count", ..}` case, no new `UiEvent` variant.

## Conclusion for the implementation

- New `UiEvent` variants needed: `MsgShowmode { content: Vec<(u64, String)> }`,
  `MsgShowcmd { content: Vec<(u64, String)> }`,
  `MsgRuler { content: Vec<(u64, String)> }` -- each decoded with the existing
  `decode_content_chunks` helper, registered in `decode_event`'s match arm
  exactly like `msg_show`.
- `search_count` needs no new variant; it is consumed from the existing
  `UiEvent::MsgShow` case already routed by `toast::route()`.
- Empty content on any of the three hides that segment -- `StatuslineState`
  must treat `content.is_empty()` as "clear this segment," not "no update."
- `msg_showmode`'s text is opaque and pre-fused by nvim (mode + macro run
  together with no separator) -- the segment renders it verbatim rather than
  trying to split mode from macro state.
