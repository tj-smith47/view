# Wire capture: `ext_popupmenu`'s cmdline-sourced vs buffer-sourced distinction

Captured live against the pinned engine per "capture, never recall." Source
of truth for the command palette's completion routing: which field on
`popupmenu_show` tells a cmdline-sourced completion (belongs inside the
palette) apart from an insert-mode buffer completion (belongs in its own
popup at the cursor, never inside the palette).

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
`nvim --embed --clean -n` with the same hermetic `XDG_*`/`HOME` isolation
`EngineConfig::isolated()` uses, attaches as a real UI with
`ext_linegrid`/`ext_cmdline`/`ext_popupmenu`/`ext_messages`/`ext_tabline`,
and drives `nvim_input` as a **notification** -- fire-and-forget, matching
production `RpcCall::Input` exactly. Output below is the flattened `redraw`
batches, in wire order, split into named sections. A trailing
`nvim_eval("1+1")` **request** confirms the session stayed alive and
responsive through both captures (`liveness check (1+1): 2`).

## 1. Cmdline-sourced: `:set nu` + `<Tab>`

Driven: `:set nu`, then `<Tab>` (ex-command option-name completion).

```
['cmdline_show', [[[0, 'set nu', 0]], 6, ':', '', 0, 1, 0]]
['mode_change', ['cmdline_normal', 4]]
['mouse_off', []]
['flush', []]
['popupmenu_show', [[['number', '', '', ''], ['numberwidth', '', '', '']], 0, 0, 4, -1]]
['cmdline_show', [[[0, 'set number', 0]], 10, ':', '', 0, 1, 0]]
['flush', []]
```

`popupmenu_show`'s args are `[items, selected, row, col, grid]`. Here:
`selected=0, row=0, col=4, grid=-1`.

## 2. Buffer-sourced: insert-mode keyword completion (`<C-n>`)

Driven: `ihello help<Esc>` to seed two candidate words sharing a prefix,
back into insert mode, type `he`, then `<C-n>` (keyword completion). A
single-candidate buffer completion auto-completes silently with no
`popupmenu_show` at all, so this capture uses two candidates
(`hello`/`help`) to force a real popup open.

```
['msg_showmode', [[[15, '-- INSERT --', 11]]]]
...
['mode_change', ['insert', 2]]
['flush', []]
['msg_showmode', [[[15, '-- Keyword completion (^N^P) -- Searching...', 11]]]]
['flush', []]
['grid_line', [1, 1, 2, [['l', 0, 2], ['o']], False]]
['msg_showmode', [[[15, '-- Keyword completion (^N^P) ', 11], [16, 'match 1 of 2', 18]]]]
['popupmenu_show', [[['hello', '', '', ''], ['help', '', '', '']], 0, 1, 0, 1]]
```

Here: `selected=0, row=1, col=0, grid=1` -- a real, non-negative grid handle
anchoring the popup to the buffer grid at the cursor, not to the cmdline.

## Conclusions for the implementation

`grid` is the distinguishing field: **`-1` means cmdline-sourced** (the
popup is positioned relative to the command line, not any real grid --
`-1` is not a grid handle nvim ever assigns to a window), and **any
non-negative `grid` means buffer-sourced**, anchored to that grid's cursor
position via `row`/`col` in grid-local coordinates.

This is also a decode bug fix, not purely new logic:
`view-engine::ui_events::decode_popupmenu_show` currently reads `grid` with
`as_u64`, which returns `None` for the negative wire value -- so every
cmdline-sourced `popupmenu_show` currently fails to decode at all and is
silently dropped (falls through to the unknown-event no-op). The palette
work widens `UiEvent::PopupmenuShow.grid` and `PopupmenuState.grid` from
`u64` to `i64` and switches the decode call to `as_i64`, fixing that drop as
a prerequisite for routing on the field this capture pins.

`PopupmenuState::is_cmdline_sourced() -> bool { self.grid < 0 }` is the one
new predicate this capture licenses; `view-surface::render` gates the
`Popupmenu` layer on `!is_cmdline_sourced()` and gates the palette's own
completion rows on `is_cmdline_sourced()`, so a buffer completion and a
cmdline completion can never both land in the same layer.
