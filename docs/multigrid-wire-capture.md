# Wire capture: the `ext_multigrid` redraw vocabulary

Captured live against the pinned engine: every value below reflects an actual
run of the pinned binary. Source of truth for the event names, argument tuples
and orderings a multigrid compositor in view has to decode, and for the
window-layout ground truth a pane registry derived from those events is checked
against.

Everything below was produced by
`crates/view-oracle/tests/multigrid_capture.rs`, which re-runs the whole
capture and fails when the doc and the live stream part. It leaves each arm's
complete transcript under `target/multigrid-capture/`, which is what the
quotations here are taken from. Nothing is transcribed by hand from
documentation or memory.

## Engine identity

```
$ nvim --version | head -3
NVIM v0.12.4
Build type: Release
LuaJIT 2.1.1787058514
```

Matches `.engine-pin` (`v0.12.4`).

## Capture method

Three attached UIs, each a real `nvim --embed` child spawned with
`EngineConfig::isolated`'s own `--clean -n` and its inspectable `env_plan`,
reached over a bare `EngineHandle::start` connection so the raw `redraw`
notification params are still readable. Every other live driver in this tree
routes redraw traffic through the damage pump, which decodes it; a decoded
stream cannot answer what an undecoded event's argument tuple looks like, which
is this document's whole subject.

| Arm | `nvim_ui_attach` options |
| --- | --- |
| multigrid | `UI_EXT_OPTIONS` + `ext_multigrid` |
| single-grid | `UI_EXT_OPTIONS` (the vocabulary the capture used; the knob today selects `ui_ext_options_shipped()`) |
| no-messages probe | `ext_linegrid` + `ext_multigrid` only |

`UI_EXT_OPTIONS` is `ext_linegrid`, `ext_cmdline`, `ext_popupmenu`,
`ext_messages`, `ext_tabline`.

The first two arms drive the identical script at 80x24 and differ only in that
one option, so every delta below is a fact about `ext_multigrid` itself; the
script contributes none of it. `mousetime=0` is load-bearing: the three presses
land on one cell, and with a nonzero mousetime the gap between them; two
blocking evals long; puts the host's speed into nvim's multi-click detection,
so a loaded runner captures `msg_showcmd` rows an idle one does not:

```
attach
:vsplit
:split
:wincmd w
:resize 5
:close
:only
:vsplit
:set mouse=a mousetime=0
nvim_input_mouse("left", "press", "", grid=0, row=3, col=60)
:wincmd h
nvim_input_mouse("left", "press", "", grid=<right window>, row=3, col=19)
:wincmd h
nvim_input_mouse("left", "press", "", grid=1, row=3, col=60)
nvim_ui_try_resize(70, 20)
nvim_open_win(scratch, false, {relative='editor', row=2, col=4,
              width=20, height=3, border='single'})
nvim_win_close(float, true)
:tabnew
:tabclose
fill the buffer with 200 lines
scroll one line with <C-e>
```

The third arm exists because two of the questions cannot be answered from
inside the first two: `msg_set_pos` positions a message *grid*, which an
`ext_messages` UI has none of, and `win_external_pos` needs a window actually
taken out of the layout.

Argument values below are rendered verbatim from the wire, with two
conventions: a string is quoted so an empty one is distinguishable from `nil`,
and a `Window`/`Buffer`/`Tabpage` handle (a msgpack `Ext` whose payload is
itself an encoded integer) is written `ext(type:handle)`. Declared parameter
names and types come from the same engine's own `api_info().ui_events`, and
none of it is this document's author's own invention.

## The grid-1 question

**Grid 1 keeps receiving `grid_line` under `ext_multigrid`, and what it carries
is chrome only, with no window text in it.**

Grid-line traffic by grid id across the whole script:

| Grid | multigrid arm | single-grid arm |
| --- | --- | --- |
| 1 (global) | 68 | 362 |
| 2, 4, 5, 6, 7, 8 (windows) | 218 combined | 0 |

Every grid-1 line the multigrid arm emitted is a statusline, a window
separator, or the last-row ruler area (the second tuple below is wrapped for
width; every other quotation in this document is one wire line):

```
grid_line [1, 0, 40, [["│", 12]], false]
grid_line [1, 23, 0, [["[", 41], ["N"], ["o"], [" "], ["N"], ["a"], ["m"],
                      ["e"], ["]"], [" ", 41, 13], ["0"], [","], ["0"],
                      ["-"], ["1"], [" ", 41, 10], ["A"], ["l", 41, 2]],
           false]
```

The same script under single-grid puts all of it, buffer text included, on grid
1:

```
grid_line [1, 2, 4, [["┌", 30], ["─", 30, 20], ["┐"]], false]
grid_line [1, 3, 4, [["│", 30], [" ", 30, 20], ["│"]], false]
```

Two consequences for a compositor:

- Grid 1 is a real, paintable layer under multigrid, and no stub. A compositor
  that stops applying grid-1 lines once multigrid is negotiated loses every
  statusline and separator.
- `grid_clear` names only grid 1, in both arms. `grid_cursor_goto` names only
  window grids under multigrid (2, 4, 5, 6, 8) and only grid 1 under
  single-grid, so the cursor's grid is the one place where the two vocabularies
  do not overlap at all.

## The events

### `grid_resize`

Declared `[["Integer","grid"],["Integer","width"],["Integer","height"]]`, since
5.

```
grid_resize [1, 80, 24]
grid_resize [2, 80, 23]
grid_resize [5, 29, 19]
```

Grid 1 is resized to the full terminal size; a window grid is resized to that
window's own text area. Both arms resize grid 1; only the multigrid arm ever
resizes any other grid.

### `grid_line`

Declared
`[["Integer","grid"],["Integer","row"],["Integer","col_start"],["Array","data"],["Boolean","wrap"]]`,
since 5.

```
grid_line [2, 0, 0, [[" ", 0, 80]], false]
grid_line [7, 0, 0, [["┌", 30], ["─", 30, 20], ["┐"]], false]
```

`data` is a list of cell tuples, each `[text]`, `[text, hl_id]` or
`[text, hl_id, repeat]`, with `hl_id` carried forward when omitted. The arity
is unchanged from single-grid; only the `grid` field's meaning moves.

### `grid_clear`

Declared `[["Integer","grid"]]`, since 5. Observed for grid 1 only, in both
arms, and always paired with the `grid_resize [1, ...]` of a UI refresh.

```
grid_clear [1]
```

### `grid_scroll`

Declared
`[["Integer","grid"],["Integer","top"],["Integer","bot"],["Integer","left"],["Integer","right"],["Integer","rows"],["Integer","cols"]]`,
since 5. The clearest single illustration of the whole delta: one `<C-e>` in
the right-hand window of a `:vsplit`.

```
multigrid:   grid_scroll [5, 0, 19, 0, 29, 1, 0]
single-grid: grid_scroll [1, 0, 19, 41, 70, 1, 0]
```

Under multigrid the region is the window grid's own full width (`left = 0`,
`right = 29`); under single-grid it is that window's column span inside the
global grid (`left = 41`, `right = 70`).

### `grid_cursor_goto`

Declared `[["Integer","grid"],["Integer","row"],["Integer","col"]]`, since 5.

```
grid_cursor_goto [2, 0, 0]
```

Row and column are relative to the named grid, so under multigrid the screen
position is only recoverable by adding the grid's `win_pos` or `win_float_pos`
origin.

### `grid_destroy`

Declared `[["Integer","grid"]]`, since 6. Multigrid only.

```
grid_destroy [4]
```

### `win_pos`

Declared
`[["Integer","grid"],["Window","win"],["Integer","startrow"],["Integer","startcol"],["Integer","width"],["Integer","height"]]`,
since 6. Multigrid only.

```
win_pos [2, ext(1:1000), 0, 0, 80, 23]
win_pos [5, ext(1:1002), 0, 41, 29, 19]
```

The one event that binds a grid id to a window handle and to a screen origin.
`width`/`height` repeat the grid's size, and were observed equal to the
matching `grid_resize` in every cycle of this capture.

### `win_float_pos`

Declared
`[["Integer","grid"],["Window","win"],["String","anchor"],["Integer","anchor_grid"],["Float","anchor_row"],["Float","anchor_col"],["Boolean","mouse_enabled"],["Integer","zindex"],["Integer","compindex"],["Integer","screen_row"],["Integer","screen_col"]]`,
since 6. Multigrid only. Eleven fields on this engine, four more than the seven
older frontends assume.

```
win_float_pos [7, ext(1:1004), "NW", 1, 2.0, 4.0, true, 50, 1, 2, 4]
```

`anchor_row`/`anchor_col` are floats on the wire even when whole-numbered;
`screen_row`/`screen_col` are integers and are the resolved position after the
anchor is applied. The float's grid holds its border: a `width=20, height=3`
window with `border='single'` produced `grid_resize [7, 22, 5]`.

### `win_external_pos`

Declared `[["Integer","grid"],["Window","win"]]`, since 6. Not emitted by
either main arm. It arrives once a window is actually taken out of the layout,
which the pinned engine accepts from a multigrid UI:

```
lua vim.api.nvim_win_set_config(0, {external=true, width=20, height=5})
  -> accepted

win_external_pos [4, ext(1:1001)]
```

### `win_hide`

Declared `[["Integer","grid"]]`, since 6. Multigrid only. Emitted for the grids
of a tab page that stops being current; the grids stay alive.

```
win_hide [6]
win_hide [5]
```

There is no paired "show" event: the grids come back through `win_pos` alone
when the tab page is closed.

### `win_close`

Declared `[["Integer","grid"]]`, since 6. Multigrid only, and always followed
by `grid_destroy` for the same grid in the same cycle.

```
win_close [4]
grid_destroy [4]
```

### `win_viewport`

Declared
`[["Integer","grid"],["Window","win"],["Integer","topline"],["Integer","botline"],["Integer","curline"],["Integer","curcol"],["Integer","line_count"],["Integer","scroll_delta"]]`,
since 7. Emitted by **both** arms, and this is the trap: under single-grid it
reports grid ids (2, 4, 5, 6, 8) that are never `grid_resize` d and never
receive a single `grid_line`. A grid id read off `win_viewport` is therefore
not evidence that a paintable grid exists.

```
win_viewport [2, ext(1:1000), 0, 2, 0, 0, 1, 0]
```

### `win_viewport_margins`

Declared
`[["Integer","grid"],["Window","win"],["Integer","top"],["Integer","bottom"],["Integer","left"],["Integer","right"]]`,
since 12. Multigrid only, and the first event that ever names a new grid (see
the id space below).

```
win_viewport_margins [2, ext(1:1000), 0, 0, 0, 0]
win_viewport_margins [7, ext(1:1004), 1, 1, 1, 1]
```

The float's margins are its border thickness.

### `msg_set_pos`

Declared
`[["Integer","grid"],["Integer","row"],["Boolean","scrolled"],["String","sep_char"],["Integer","zindex"],["Integer","compindex"]]`,
since 6.

**Never emitted while `ext_messages` is attached**, which is the whole of
`UI_EXT_OPTIONS`. The no-messages probe is the control that makes that a
finding worth recording, distinct from a mere absence:

```
multigrid + ext_messages:  no msg_set_pos in the entire script
multigrid, no ext_messages:
  msg_set_pos [0, 23, false, " ", 0, 0]
  msg_set_pos [3, 23, false, " ", 200, 0]
  msg_set_pos [3, 21, false, " ", 200, 1]
```

Grid 3 is the message grid. Its absence from the `ext_messages` arms is also
why the window grid ids there skip 3 (see below). The first tuple, with
`grid = 0`, arrives before any message grid exists.

### `flush`

No arguments, both arms, unchanged.

```
flush []
```

### Everything else

`chdir`, `default_colors_set`, `hl_attr_define`, `hl_group_set`, `mode_change`,
`mode_info_set`, `mouse_on`, `option_set`, `set_icon`, `set_title`,
`tabline_update` and `update_menu` appeared identically in both arms and carry
no grid field. `ext_multigrid` changes nothing about them.

## The grid id space

Observed allocation order across the multigrid script:

| Grid | What it is | Created at | Destroyed at |
| --- | --- | --- | --- |
| 1 | the global grid | attach | never |
| 2 | the first window | attach | `:only` |
| 3 | the message grid | only without `ext_messages` | n/a |
| 4 | `:vsplit`'s new window | `:vsplit` | `:close` |
| 5 | `:split`'s new window | `:split` | never |
| 6 | the second `:vsplit`'s window | `:vsplit` | never |
| 7 | the float | `nvim_open_win` | `nvim_win_close` |
| 8 | the new tab page's window | `:tabnew` | `:tabclose` |

Three facts a registry depends on:

- **The first window grid is 2**, with `ext_messages` attached. Without it, 3
  is taken by the message grid and the window ids continue from 4. A registry
  must never assume a contiguous window-grid range.
- **Ids are not reused after `grid_destroy`.** Grid 4 was destroyed at `:close`
  and grid 2 at `:only`; the next windows opened took 5, 6, 7 and 8. Nothing in
  the capture reissued a destroyed id.
- **A grid is named before it is sized; something other than `win_pos` names
  it.** Every new grid's first appearance was a `win_viewport_margins` naming
  it, ahead of its own `grid_resize`; `win_pos` and `win_float_pos` always
  arrived after it. A `win_pos` *can* still precede a `grid_resize` for a grid
  that already exists, which happens whenever a window is moved and resized in
  the same cycle:

```
win_pos     [2, ext(1:1000), 0, 41, 39, 23]
grid_resize [4, 40, 23]
grid_resize [2, 39, 23]
win_pos     [4, ext(1:1001), 0, 0, 40, 23]
```

So a decoder must tolerate a `win_pos` for a grid it has no size for, and must
not treat `win_pos` as a grid's creation point.

## Ordering inside one cycle

The `:vsplit` cycle in full, minus its `grid_line` traffic, is representative
of every layout change:

```
win_viewport_margins [4, ext(1:1001), 0, 0, 0, 0]
win_viewport_margins [4, ext(1:1001), 0, 0, 0, 0]
win_viewport_margins [2, ext(1:1000), 0, 0, 0, 0]
win_pos              [2, ext(1:1000), 0, 41, 39, 23]
tabline_update       ...
grid_resize          [4, 40, 23]
grid_resize          [2, 39, 23]
win_pos              [4, ext(1:1001), 0, 0, 40, 23]
win_viewport         [4, ext(1:1001), 0, 2, 0, 0, 1, 0]
win_viewport         [2, ext(1:1000), 0, 2, 0, 0, 1, 0]
grid_cursor_goto     [4, 0, 0]
flush                []
```

Nothing is ordered by grid; a single cycle interleaves several grids freely,
and `win_viewport_margins` repeats for the same grid within one cycle. The only
ordering a decoder may rely on is that everything between two `flush` events
describes one consistent frame.

Closing a window and closing a tab page differ, and both matter:

```
:close                    :tabclose
win_close     [4]         grid_destroy [8]
...                       win_pos      [6, ext(1:1003), 0, 0, 40, 19]
grid_destroy  [4]         win_pos      [5, ext(1:1002), 0, 41, 29, 19]
win_pos       [5, ...]    ...
```

A closed window sends `win_close` then `grid_destroy`. A closed tab page sends
`grid_destroy` for its grid with **no** `win_close`, and revives the previously
hidden grids with bare `win_pos` events.

## The mouse and resize calls

### `nvim_input_mouse`

Declared
`[["String","button"],["String","action"],["String","modifier"],["Integer","grid"],["Integer","row"],["Integer","col"]]`.

Both addressings work under multigrid, and each reads `row`/`col` in its own
coordinate space.

`grid=0` reads them as global screen coordinates and lets nvim resolve the
window itself:

```
current window before: 1003   (the left window of a :vsplit)
nvim_input_mouse("left", "press", "", grid=0, row=3, col=60)
current window after:  1002   (the right window)
```

The redraw that followed named the right window's grid, distinct from grid 1:

```
win_viewport     [5, ext(1:1002), 0, 2, 0, 0, 1, 0]
grid_cursor_goto [5, 0, 0]
```

A window's own grid id reads them as coordinates *inside* that grid. The same
screen cell, addressed the other way, reaches the same window; column 19 of
grid 5, which sits at screen column 41, is screen column 60, while column 19
addressed globally would have landed in the left window:

```
right-hand window: grid=5 at screen column 41
current window before: 1003
nvim_input_mouse("left", "press", "", grid=5, row=3, col=19)
current window after:  1002
```

The global grid may be named outright, or through the sentinel instead, with
the same global coordinates and the same outcome. **Both arms answer this**,
which is what makes it usable: the single-grid session, where grid 1 is the
whole picture, and the multigrid session, where grid 1 is the chrome between
windows and is what a UI has to send between its attach and the first
`win_pos`.

```
multigrid arm:                                       single-grid arm:
current window before: 1003                          current window before: 1003
nvim_input_mouse("left","press","",grid=1,row=3,col=60)  (identical call)
current window after:  1002                          current window after:  1002
```

So a UI that tracks the layout may address the grid it hit, one that does not
may keep sending `0`, and either may name grid 1 for the screen; the grid and
the coordinates form a single paired choice; they are never chosen
independently.

### `nvim_ui_try_resize` and `nvim_ui_try_resize_grid`

Declared `[["Integer","width"],["Integer","height"]]` and
`[["Integer","grid"],["Integer","width"],["Integer","height"]]`.

** `nvim_ui_try_resize_grid` is not required for per-grid sizing.** A single
`nvim_ui_try_resize(70, 20)` made the engine resize every grid itself, and
re-announce the whole UI option set on the way:

```
option_set           ["ext_cmdline", true]
option_set           ["ext_popupmenu", true]
option_set           ["ext_tabline", true]
option_set           ["ext_wildmenu", false]
option_set           ["ext_messages", true]
default_colors_set   [14738154, 1316379, 16711680, 0, 0]
grid_resize          [1, 70, 20]
win_viewport_margins [6, ext(1:1003), 0, 0, 0, 0]
win_viewport_margins [5, ext(1:1002), 0, 0, 0, 0]
win_viewport_margins [5, ext(1:1002), 0, 0, 0, 0]
grid_clear           [1]
win_pos              [6, ext(1:1003), 0, 0, 40, 19]
win_pos              [5, ext(1:1002), 0, 41, 29, 19]
tabline_update       ...
grid_resize          [6, 40, 19]
grid_resize          [5, 29, 19]
win_viewport         [6, ext(1:1003), 0, 2, 0, 0, 1, 0]
win_viewport         [5, ext(1:1002), 0, 2, 0, 0, 1, 0]
grid_cursor_goto     [5, 0, 0]
flush                []
```

`nvim_ui_try_resize_grid` remains available for a UI that wants a window grid
larger than its on-screen box. view does not call it.

## Side-by-side delta

| Event | multigrid | single-grid |
| --- | --- | --- |
| `grid_resize` | grid 1 and every window grid | grid 1 only |
| `grid_line` | grid 1 (chrome) and window grids (text) | grid 1 only |
| `grid_clear` | grid 1 | grid 1 |
| `grid_scroll` | window grid, grid-local region | grid 1, window's screen region |
| `grid_cursor_goto` | window grids only | grid 1 only |
| `grid_destroy` | yes | never |
| `win_pos` | yes | never |
| `win_float_pos` | yes | never |
| `win_external_pos` | on an external window | never |
| `win_hide` | yes | never |
| `win_close` | yes | never |
| `win_viewport` | yes, grid is paintable | yes, grid is **not** paintable |
| `win_viewport_margins` | yes | never |
| `msg_set_pos` | only without `ext_messages` | never |
| everything else | identical | identical |

Six event names exist in the multigrid arm and in no part of the single-grid
arm: `grid_destroy`, `win_close`, `win_float_pos`, `win_hide`, `win_pos`,
`win_viewport_margins`.

## What view's decoder has no variant for

`view_engine::ui_events::decode_redraw` answers `UiEvent::Unknown` for every
name below, and that is what the capture test asserts on. All five (`chdir`,
`option_set`, `set_icon`, `set_title`, `update_menu`) are already unknown under
single-grid and are unrelated to this work.

Every name that is the multigrid delta now decodes: the placement vocabulary
the pane registry applies (`grid_destroy`, `win_close`, `win_float_pos`,
`win_hide`, `win_pos`), `win_external_pos` from the third arm, and
`win_viewport_margins`. Nothing paints from the margins; the cells inside them
arrive as `grid_line` like any other; but the event is decoded so a `layout`
trace names the grid each line belongs to.

### Names view's decoder has no variant for

```
chdir
option_set
set_icon
set_title
update_menu
```

## Window-layout ground truth

Read back through `nvim_list_wins`, `nvim_win_get_position`,
`nvim_win_get_width`/`_height`, `nvim_win_get_config().relative` and
`nvim_win_get_tabpage` at each scenario's settle point, independently of the
redraw stream. A pane set derived from `win_pos` and `win_float_pos` is checked
against this, and not against the events it came from.

`pos` is `[row, col]` and `size` is `width x height`. `relative` is empty for a
split and names the anchor for a float. `height` is the window's text height,
which excludes the statusline row that `win_pos`'s `height` also excludes, and
the float at `pos=[2,4] size=20x3` is the inside of the border whose own grid
measured 22x5.

The rows are formatted field by field, and not serialized as JSON: a vimscript
dictionary comes back in its own hash order, and a committed artifact must not
rest on that staying the same across builds.

### Window-layout ground truth, multigrid arm

```
attach: win=1000 tab=1 pos=[0,0] size=80x23 relative=
vsplit: win=1001 tab=1 pos=[0,0] size=40x23 relative= | win=1000 tab=1 pos=[0,41] size=39x23 relative=
split: win=1002 tab=1 pos=[0,0] size=40x11 relative= | win=1001 tab=1 pos=[12,0] size=40x11 relative= | win=1000 tab=1 pos=[0,41] size=39x23 relative=
wincmd w: win=1002 tab=1 pos=[0,0] size=40x11 relative= | win=1001 tab=1 pos=[12,0] size=40x11 relative= | win=1000 tab=1 pos=[0,41] size=39x23 relative=
resize 5: win=1002 tab=1 pos=[0,0] size=40x17 relative= | win=1001 tab=1 pos=[18,0] size=40x5 relative= | win=1000 tab=1 pos=[0,41] size=39x23 relative=
close: win=1002 tab=1 pos=[0,0] size=40x23 relative= | win=1000 tab=1 pos=[0,41] size=39x23 relative=
only: win=1002 tab=1 pos=[0,0] size=80x23 relative=
vsplit again: win=1003 tab=1 pos=[0,0] size=40x23 relative= | win=1002 tab=1 pos=[0,41] size=39x23 relative=
set mouse=a: win=1003 tab=1 pos=[0,0] size=40x23 relative= | win=1002 tab=1 pos=[0,41] size=39x23 relative=
mouse press: win=1003 tab=1 pos=[0,0] size=40x23 relative= | win=1002 tab=1 pos=[0,41] size=39x23 relative=
grid-addressed mouse press: win=1003 tab=1 pos=[0,0] size=40x23 relative= | win=1002 tab=1 pos=[0,41] size=39x23 relative=
global-grid-addressed mouse press: win=1003 tab=1 pos=[0,0] size=40x23 relative= | win=1002 tab=1 pos=[0,41] size=39x23 relative=
nvim_ui_try_resize: win=1003 tab=1 pos=[0,0] size=40x19 relative= | win=1002 tab=1 pos=[0,41] size=29x19 relative=
nvim_open_win float: win=1003 tab=1 pos=[0,0] size=40x19 relative= | win=1002 tab=1 pos=[0,41] size=29x19 relative= | win=1004 tab=1 pos=[2,4] size=20x3 relative=editor
float close: win=1003 tab=1 pos=[0,0] size=40x19 relative= | win=1002 tab=1 pos=[0,41] size=29x19 relative=
tabnew: win=1003 tab=1 pos=[0,0] size=40x19 relative= | win=1002 tab=1 pos=[0,41] size=29x19 relative= | win=1005 tab=2 pos=[0,0] size=70x19 relative=
tabclose: win=1003 tab=1 pos=[0,0] size=40x19 relative= | win=1002 tab=1 pos=[0,41] size=29x19 relative=
fill buffer: win=1003 tab=1 pos=[0,0] size=40x19 relative= | win=1002 tab=1 pos=[0,41] size=29x19 relative=
scroll: win=1003 tab=1 pos=[0,0] size=40x19 relative= | win=1002 tab=1 pos=[0,41] size=29x19 relative=
```

Reading it against the events: at `vsplit`, window 1001 sits at `pos=[0,0]` 40
wide and window 1000 at `pos=[0,41]` 39 wide, which is exactly what
`win_pos [4, ext(1:1001), 0, 0, 40, 23]` and
`win_pos [2, ext(1:1000), 0, 41, 39, 23]` announced, with the separator column
40 belonging to grid 1. At `nvim_open_win float`, window 1004 is
`relative=editor` at `pos=[2,4]`, matching `win_float_pos`'s
`screen_row`/`screen_col` of `2, 4`, distinct from its float `anchor_row` and
`anchor_col`.

## The pinned name sets

These three lists are what the capture test compares against the live stream on
every run. A name the pinned engine stops emitting, or one an engine-pin bump
adds, fails the test here.

### Captured event names, multigrid arm

```
chdir
default_colors_set
flush
grid_clear
grid_cursor_goto
grid_destroy
grid_line
grid_resize
grid_scroll
hl_attr_define
hl_group_set
mode_change
mode_info_set
mouse_on
option_set
set_icon
set_title
tabline_update
update_menu
win_close
win_float_pos
win_hide
win_pos
win_viewport
win_viewport_margins
```

### Captured event names, single-grid arm

```
chdir
default_colors_set
flush
grid_clear
grid_cursor_goto
grid_line
grid_resize
grid_scroll
hl_attr_define
hl_group_set
mode_change
mode_info_set
mouse_on
option_set
set_icon
set_title
tabline_update
update_menu
win_viewport
```

### Captured event names, multigrid without `ext_messages`

```
chdir
default_colors_set
flush
grid_clear
grid_cursor_goto
grid_line
grid_resize
hl_attr_define
hl_group_set
mode_change
mode_info_set
mouse_on
msg_set_pos
option_set
set_icon
set_title
update_menu
win_external_pos
win_pos
win_viewport
win_viewport_margins
```
