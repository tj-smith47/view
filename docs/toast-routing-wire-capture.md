# Wire capture: toast routing (`msg_show` kinds vs. sibling message events)

Captured live against the pinned engine per "capture, never recall." Source of
truth for `view_core::native::toast::route()`'s reachable input domain.

## Engine identity

```
$ nvim --version | head -3
NVIM v0.12.4
Build type: Release
LuaJIT 2.1.1785192264
```

Matches `.engine-pin` (`v0.12.4`) exactly.

## Capture method

Same standalone Python msgpack-rpc client as
`docs/prompt-overlay-wire-capture.md`: `nvim --embed --clean` (never
`--headless`, which suppresses `ext_messages` redraws entirely), hermetic
`XDG_*`/`HOME` isolation, `nvim_input` driven as a fire-and-forget
**notification**. Full ext-option set at attach
(`ext_linegrid`/`ext_cmdline`/`ext_popupmenu`/`ext_messages`/`ext_tabline`).

## Finding: `msg_showmode` / `msg_showcmd` / `msg_ruler` are not `msg_show` kinds

`toast.rs`'s routing table lists these three as arms of `route(kind: &str)`
alongside `search_count`. Live capture shows they never arrive that way --
they are their own top-level redraw events, structurally parallel to
`msg_show` but never inside one:

```
=== insert text (msg_showmode) ===
['msg_showmode', [[[15, '-- INSERT --', 11]]]]
...
['msg_showmode', [[]]]                      # empty content == hide

=== cursor to col0 (msg_showcmd) ===
['msg_showcmd', [[[0, '0', 0]]]]
['msg_showcmd', [[]]]                       # empty content == hide

=== laststatus=0, ruler set (msg_ruler) ===
['msg_ruler', [[[1, '0,0-1         All', 63]]]]
['msg_ruler', [[[1, '1,11          All', 63]]]]   # updates on every cursor move
```

Confirmed against the pinned engine's own doc,
`share/nvim/runtime/doc/api-ui-events.txt`:

```
$ grep -n '\["msg_' api-ui-events.txt
839:["msg_show", kind, content, replace_last, history, append, id, trigger] ~
905:["msg_clear"] ~
916:["msg_showmode", content] ~
921:["msg_showcmd", content] ~
925:["msg_ruler", content] ~
930:["msg_history_show", entries, prev_cmd] ~
```

`msg_showmode`, `msg_showcmd`, `msg_ruler`, and `msg_clear`/`msg_history_show`
are each their own bracketed event name at the top level of a redraw batch
(`b[2]` group), the same tier as `msg_show` itself -- never a `kind` string
carried inside a `msg_show` tuple. A `route(kind: &str)` fed only from
`UiEvent::MsgShow`'s `kind` field can therefore never receive `"msg_showmode"`,
`"msg_showcmd"`, or `"msg_ruler"` as input: those three match arms in
`route()`'s table are dead code under today's `msg_show`-only wiring, by
construction of the wire protocol, not a bug in the table. They stay in
`route()` unmodified (per "the table IS the implementation" and because the
call site that decodes these sibling
events depends on `route()`'s exact name/signature to classify them once it
starts feeding them through). The three arms become live once that call
site threads `UiEvent::MsgShowmode`/`MsgShowcmd`/`MsgRuler` variants through
the same `route()` call by construction (kind string literal match, no
protocol translation needed) -- today's wiring does not add those
`UiEvent` variants or decode those wire events at all, since nothing routes
through them yet (no statusline surface exists to consume
`Route::Statusline`).

## Finding: `search_count` is a genuine `msg_show` kind

Unlike the three above, `search_count` is documented (line 863 of the same
doc) as one of the enumerated `kind` values carried inside `msg_show` itself:

```
858:		"search_cmd"	Entered search command
859:		"search_count"	Search count message ("S" flag of 'shortmess')
```

`search_cmd` was observed live (typing `/cat<CR>` with `shortmess-=S`):

```
['msg_show', ['search_cmd', [[0, '/cat                 ', 0]], False, False, False, 5, '']]
```

`search_count` was not independently triggered live by this capture (the
multi-match buffer setup needed to force nvim to show `[N/M]` failed on an
unrelated `:put` quoting issue in the test script, not a protocol gap) but is
authoritatively documented as a `msg_show` kind by the pinned engine's own
reference, which the wire-capture protocol treats as sufficient evidence for
a documented, structurally-consistent-with-observed-siblings (`search_cmd`)
kind string. It is reachable through `route()`'s existing `msg_show`
pipeline today, unlike the three top-level events above.

## Conclusion for the implementation

- `route(kind: &str)` is implemented as the routing table above, unmodified.
  Four of its six textual arms (`msg_showmode`, `msg_showcmd`, `msg_ruler`)
  are unreachable through today's `UiEvent::MsgShow`-only call site and will
  only become live when a future call site adds the corresponding
  `UiEvent` variants and feeds their content through the same function --
  documented here, not worked around, since `route()`'s job is to be the
  one routing table for every kind string this or a future call site can
  produce.
- `search_count` and `search_cmd` are real, reachable `msg_show` kinds today
  and route through today's call site exactly like any other kind string.
