# Wire capture: floats that land on a view-owned surface

Captured live against the pinned engine and the compat harness's heavy
fixture, per "capture, never recall". Source of truth for what a plugin's
floating window actually carries when it draws over a surface view owns:
its `nvim_win_get_config` verbatim, the identity of the buffer behind it,
how a selection is expressed, which autocmds fire, and what the window does
per keystroke while a hide is standing on it.

Four subjects, three of which claim something view draws and one of which
claims nothing: nvim-cmp's cmdline completion menu, nvim-notify's toast,
noice's error float, and telescope's picker. The picker is the negative
control. A detector that flags it is a detector that flags every float.

## Engine and plugin identity

Read out of the running session (`VFC.header()`), not from a manifest:

```
nvim 0.12.4 api_level=14
lines=29 columns=100 cmdheight=0
ui ext_cmdline=true ext_popupmenu=true ext_messages=true ext_multigrid=false
```

Matches `.engine-pin` (`v0.12.4`). The session runs inside a 100x30 pty
(the compat harness's own geometry) and nvim reports 29 lines: one screen
row is view's own chrome, and the nvim grid is the other 29.

Plugin pins, printed from the fixture's own `lazy-lock.json` inside the
session:

```
"nvim-cmp":       { "branch": "main",   "commit": "2ffe79f1f021def8dd1fcd81deb16f1bb0d989f3" }
"cmp-buffer":     { "branch": "main",   "commit": "b74fab3656eea9de20a9b8116afa3cfc4ec09657" }
"nvim-notify":    { "branch": "master", "commit": "8701bece920b38ea289b457f902e2ad184131a5d" }
"noice.nvim":     { "branch": "main",   "commit": "7bfd942445fb63089b59f97ca487d605e715f155" }
"nui.nvim":       { "branch": "main",   "commit": "de740991c12411b663994b2860f1a4fd0937c130" }
"telescope.nvim": { "branch": "master", "commit": "427b576c16792edad01a92b89721d923c19ad60f" }
"plenary.nvim":   { "branch": "master", "commit": "74b06c6c75e4eeb3108ec01852001636d85a932b" }
```

Native features: every one of them on. The state runs with `native = {}`,
which materializes view's own defaults rather than the heavy fixture's
`view.toml` (which turns the takeovers off so unrelated scenarios measure
one thing at a time). That matters here and nowhere else: view has to own
the cmdline, the popupmenu and the messages for a float over them to be a
claim at all.

## Capture method

`scripts/acceptance/capture-surface-floats.toml` drives the session under
`task compat`; `scripts/acceptance/capture-surface-floats.lua` is loaded
into it through the harness probe channel and writes
`target/surface-float-capture.txt`:

```
$ task compat -- scripts/acceptance/capture-surface-floats.toml
compat: capture-surface-floats (heavy, present) ... OK (82 steps, 29.1s)
```

Three properties of that arrangement are load-bearing:

- **Real keystrokes.** Every key arrives through the pty, one send per key,
  because nvim-cmp reacts to `CmdlineChanged` and a burst write delivers a
  multi-column jump its own change handler treats as paste-like input.
- **Engine-side clock.** Intervals come from `vim.uv.hrtime()` inside the
  session and from a 1 ms libuv sampler armed there, so no figure below
  carries the probe subprocess's round trip.
- **The chunk finds itself.** A pty spawn's environment is swept down to an
  allowlist before the child starts (`make_hermetic` in `view-oracle`), so
  no variable set outside survives to name the file. The scenario locates
  it by walking up from the session's cwd to the repo.

One deviation from the charter's wording is recorded here rather than
papered over: the heavy fixture pins nvim-cmp with `cmp-buffer` as its only
source and configures insert mode alone, so no cmdline float exists until
one is asked for. The chunk calls the pinned cmp's own
`cmp.setup.cmdline(':' / '/', { mapping = cmp.mapping.preset.cmdline(),
sources = { { name = 'buffer' } } })` at capture time. Same plugin, same
version, same view layer (`lua/cmp/view/custom_entries_view.lua`) and the
same window machinery (`lua/cmp/utils/window.lua`); only the candidate
source differs from a config that also installs `cmp-cmdline`. Every
geometry and identity value below is therefore the pinned plugin's, and the
candidates are words the capture itself typed into the buffer
(`prefabricated`, `preflight`).

## nvim-cmp: the cmdline completion menu

At `:pref`, with two candidates standing:

```
== cmp-cmdline-colon
  mode=c cmdline=pref
  win 1003 (buf 2)
    config: { anchor = "NW", border = "none", col = 0, external = false,
              focusable = true, height = 2, hide = false, mouse = true,
              relative = "editor", row = 26, style = "minimal", width = 20,
              zindex = 1001 }
    filetype="cmp_menu" buftype="nofile" name="" cursorline=false winblend=0
    lines: { " preflight      Text   ", " prefabricated  Text   " }
    cursor: {1, 0}
    (no extmarks in any namespace)
```

The same window across the four cmdline shapes the capture walks, one
cmdline session each:

| cmdline typed | win | row | col | width | height | zindex | filetype |
|---|---|---|---|---|---|---|---|
| `:pref` | 1003 | 26 | 0 | 20 | 2 | 1001 | `cmp_menu` |
| `:e pre` | 1005 | 26 | 1 | 20 | 2 | 1001 | `cmp_menu` |
| `:se pre` | 1006 | 26 | 2 | 20 | 2 | 1001 | `cmp_menu` |
| `/pre` | 1007 | 26 | 0 | 20 | 2 | 1001 | `cmp_menu` |
| `:zqx` | none | | | | | | |

Four facts to read off that table:

- `col` tracks the start of the word being completed inside the cmdline,
  not the cmdline's own origin: 0 for `:pref`, 1 for `:e pre`, 2 for
  `:se pre`.
- `row` is not a constant either. It is bottom-anchored: with two
  candidates the menu occupies grid rows 26 and 27, with one it is
  `row = 27, height = 1`. The bottom edge lands on row 27, one row short of
  the grid's last row (28): cmp holds a row back for what it believes is the
  cmdline, and clamps its own height to keep it (`window.lua`:
  `if vim.o.lines and vim.o.lines <= info.row + info.height + 1`). With view
  attached that reserved row is not the cmdline; view drew the cmdline on its
  own chrome row, off the nvim grid entirely.
- The buffer is anonymous and unfiled (`name=""`, `buftype=nofile`) and
  carries exactly one identifying mark: `filetype = cmp_menu`, set by
  `custom_entries_view.lua`'s `buffer_option('filetype', 'cmp_menu')`.
- A prefix with no candidates produces no window at all, rather than an
  empty one:

```
== cmp-cmdline-no-candidates
  mode=c cmdline=zqx
  (no floating windows)
```

### The selection, before and after one `<C-n>`

At `/pre`, two candidates, one window (1007) throughout:

```
== cmp-cmdline-search                 cursorline=false  cursor: {1, 0}  cmdline=pre
== cmp-cmdline-search-after-c-n       cursorline=true   cursor: {1, 0}  cmdline=prefabricated
== cmp-cmdline-search-after-two-c-n   cursorline=true   cursor: {2, 0}  cmdline=preflight
```

The selection is a **window cursor row plus the `cursorline` window
option**, and nothing else. The menu buffer carries no extmarks in any
namespace, in any state, so an absorbed selection is read as
`nvim_win_get_cursor(win)[1]` gated on `vim.wo[win].cursorline`: false
means "menu open, nothing selected", true means "row N is the selection".
The buffer lines are the rendered rows verbatim, abbreviation column and
kind column included (`" preflight      Text   "`), so an absorbing
consumer takes the text and re-renders rather than replaying the padding.

### Churn: what one keystroke does to the window

Five keys typed at `:`, one send each, with a snapshot between every pair.
The hide the absorption would perform is applied after the fourth key, and
the fifth key is the one that narrows two candidates to one, so the plugin
is forced to re-set the window's config while the hide is standing:

| after key | cmdline | win | row,col,w,h | `hide` | WinNew | WinClosed |
|---|---|---|---|---|---|---|
| (`:`) | | none | | | 0 | 0 |
| `p` | `p` | 1003 | 26,0,20,2 | false | 0 | 0 |
| `r` | `pr` | 1003 | 26,0,20,2 | false | 0 | 0 |
| `e` | `pre` | 1003 | 26,0,20,2 | false | 0 | 0 |
| `f` | `pref` | 1003 | 26,0,20,2 | false | 0 | 0 |
| (hide applied) | `pref` | 1003 | 26,0,20,2 | **true** | 0 | 0 |
| `a` | `prefa` | 1003 | 27,0,20,1 | **true** | 0 | 0 |

(A second float, 1004, stands open from the second key onward. It is not
cmp's: it is noice's own health-check error, described below, and it never
moves. The counts above are the cmp menu's.)

The numbers the cadence bound is written from:

```
-- hide win 1003 config now: { ..., hide = true, height = 2, row = 26, ... }
-- reshow win=1003 result=still-hidden samples=277
  same-window reconfigure: 26,0,20,2 -> 27,0,20,1 hide=true
      keystroke->reconfigure=31.371ms  hide->reconfigure=50.355ms
  replacement: none observed
```

- keys typed at `:`: **5**
- distinct window ids across those 5 keys: **1** (1003, reused)
- `WinNew`/`WinClosed` pairs those 5 keys produced: **0**
- hide calls: **1**
- re-shows observed: **0** (277 samples over ~1.2 s, `hide` still true)
- replacement windows opened: **0**
- same-window reconfigures observed while hidden: **1**
- keystroke to that reconfiguration: **31.371 ms**

So cmp reconfigures rather than recreates, and its reconfiguration does not
clear a `hide` view set: the style table cmp passes to
`nvim_win_set_config` (`lua/cmp/utils/window.lua`, `window.open`) carries no
`hide` key, and the pinned engine leaves the flag alone. A hide taken once
per cmdline session holds for the whole session, and the double-chrome
window a consumer has to close is the 31 ms between the key and the
plugin's reaction, not a per-key race.

The window identity is stable *within* a cmdline session and never across
one: 1003, 1005, 1006, 1007 for the four cmdlines above. Each of those
windows is gone by the time `CmdlineLeave` is recorded, and none of them
announced its departure (see the cross-cutting section below).

## nvim-notify: the toast

`require('notify')('float-capture-toast')`, called directly so view's held
`vim.notify` cannot route it to view's own toast instead:

```
== nvim-notify-toast
  win 1008 (buf 4)
    config: { anchor = "NE", border = { "╭", "─", "╮", "│", "╯", "─", "╰", "│" },
              col = 100, external = false, focusable = true, height = 3,
              hide = false, mouse = true, relative = "editor", row = 0,
              style = "minimal", width = 50, zindex = 50 }
    filetype="notify" buftype="nofile" name="" cursorline=false winblend=0
    lines: { "", "", "float-capture-toast" }
    cursor: {1, 0}
    ns "nvim-notify" (id 16): 4 marks
      { 1, 0, 0, { ..., virt_text = { { " " }, { " ", "NotifyINFOIcon4" },
                    { "         ", "NotifyINFOTitle4" } },
                   virt_text_pos = "win_col", virt_text_win_col = 0 } }
      { 2, 0, 0, { ..., virt_text = { { " " }, { "05:32:25", "NotifyINFOTitle4" },
                    { " " } }, virt_text_pos = "right_align" } }
      { 3, 1, 0, { ..., virt_text = { { "━━━ (50 cells) ━━━", "NotifyINFOBorder4" } },
                   virt_text_pos = "win_col", virt_text_win_col = 0 } }
      { 4, 2, 0, { end_col = 19, end_row = 2, hl_group = "NotifyINFOBody4",
                   priority = 50 } }
```

Top-right corner, anchored `NE` at `row = 0, col = 100` (the grid's own
width), 50 by 3, `zindex = 50`. The title, timestamp and rule are virtual
text in the `nvim-notify` namespace (id 16), not buffer lines: the buffer
holds two empty lines and the message. Two reads 22.2 ms apart returned an
identical config, so the default animation stage does not move the window
between consecutive observations of a settled toast, though the window is
short-lived: `WinClosed` for 1008 arrives about 7 s later.

## noice: the error float

Two of them were captured, and neither was staged geometry.

The first arrived unprompted, roughly one second into the session, from
noice's own 1 s health checker reacting to view holding `vim.notify` at the
engine default:

```
  win 1004 (buf 3)
    config: { anchor = "NE", border = { "╭", "─", "╮", "│", "╯", "─", "╰", "│" },
              col = 100, height = 8, hide = false, relative = "editor",
              row = 0, style = "minimal", width = 96, zindex = 50 }
    filetype="markdown" buftype="nofile" name=""
    lines: { "", "", "`vim.notify` has been overwritten by another plugin?", "",
             "Either disable the other plugin or set `config.notify.enabled = false` in your **Noice** config.",
             "  - plugin: unknown", "  - file: nvim>", "  - line: 1" }
    ns "notify-treesitter-override" (id 19): 21 marks
    ns "nvim-notify" (id 16): 4 marks   (title virt_text "noice.nvim")
```

The second is the one the capture asked for, through noice's own notify
path (`require('noice.util').notify(msg, vim.log.levels.ERROR)`):

```
== noice-error
  win 1009 (buf 5)
    config: { anchor = "NE", border = { "╭", "─", "╮", "│", "╯", "─", "╰", "│" },
              col = 100, height = 3, hide = false, relative = "editor",
              row = 0, style = "minimal", width = 50, zindex = 50 }
    filetype="markdown" buftype="nofile" name=""
    lines: { "", "", "float-capture-error" }
    ns "nvim-notify" (id 16): 4 marks   (title virt_text "noice.nvim")
    ns "notify-treesitter-override" (id 19): 1 marks
```

noice's error float **is** an nvim-notify window: same anchor, same
`zindex`, same namespace, same virtual-text furniture. Two fields part
them, and both come from noice's own `on_open` in
`lua/noice/util/init.lua`: the buffer `filetype` is `markdown` rather than
`notify`, and the title virtual text is the literal string `noice.nvim`. An
identity that names the plugin is available for this float and for no other
one captured here.

## telescope: the picker (negative control)

`:Telescope help_tags` with the query `nvim_buf`, four floating windows at
once:

| win | filetype | buftype | row | col | w | h | zindex | focusable |
|---|---|---|---|---|---|---|---|---|
| 1010 | `TelescopeResults` | nofile | 2 | 11 | 78 | 21 | 50 | true |
| 1011 | (empty) | nofile | 1 | 10 | 80 | 23 | 50 | false |
| 1012 | `TelescopePrompt` | prompt | 25 | 11 | 78 | 1 | 50 | false |
| 1013 | (empty) | nofile | 24 | 10 | 80 | 3 | 50 | false |

The two windows with no filetype are drawn border chrome: their buffer
lines are the box-drawing characters themselves
(`"╭─── Results ───╮"`, `"╭─── Help ───╮"`), and they are
`focusable = false, mouse = false`.

Selection, before and after one `<C-n>`:

```
== telescope-picker            win 1010 cursor: {250, 1}
    ns "telescope_selection" (id 27): 2 marks
      { 1, 249, 0, { end_row = 249, hl_group = "TelescopeSelectionCaret", priority = 200 } }
      { 2, 249, 1, { end_row = 250, hl_group = "TelescopeSelection", priority = 4096, hl_eol = true } }
== telescope-picker-after-c-n  win 1010 cursor: {201, 0}
    ns "telescope_selection" (id 27): 2 marks
      { 1, 200, 0, { end_row = 200, hl_group = "TelescopeSelectionCaret", priority = 200 } }
      { 2, 200, 1, { end_row = 201, hl_group = "TelescopeSelection", priority = 4096, hl_eol = true } }
```

Telescope's selection is carried by extmarks in its own
`telescope_selection` namespace (the cursor moves with them; the marks are
what renders), the inverse of cmp's cursor-plus-`cursorline` with no
extmarks at all. The results buffer carries a second namespace,
`telescope_matching` (id 30), holding one mark per matched character: 408
of them before the `<C-n>`, 416 after. The prompt window carries two more
(`telescope_prompt`, holding the ` 50 / 13068` counter as right-aligned
virtual text, and `telescope_prompt_prefix`). Four namespaces across the
picker; cmp's menu buffer has none at all.

## What every float here has in common

Three cross-cutting facts, each of which constrains a consumer more than
any single geometry value.

**`WinNew` never fires. Not once, for any of the four.** The whole capture
recorded zero `WinNew` and zero `WinResized` events. All four plugins open
their windows with `noautocmd = true`: nvim-cmp in `lua/cmp/utils/window.lua`
(`s.noautocmd = true`), nvim-notify in `lua/notify/windows/init.lua`
(`win_opts.noautocmd = true`) which is also the window noice's error float
lands in, telescope through plenary's popup, whose `noautocmd` defaults to
true (`lua/plenary/popup/init.lua`: `if_nil(vim_options.noautocmd, true)`).
A watcher built on `WinNew` observes nothing at all.

**`WinClosed` fires for the toast, the error and the picker, and never for
cmp's menu.** Six `WinClosed` events in the whole capture: 1004 and 1009
(noice), 1008 (notify), and 1010, 1012, 1013 (telescope). None for 1003,
1005, 1006 or 1007, every one of which had vanished by the next observation
without an event, and none for telescope's results border (1011) inside the
observation window. That absence is not the engine suppressing the event on
a `noautocmd` window: cmp closes with `nvim_win_hide`, and a direct check on
this engine shows `nvim_win_hide` and `nvim_win_close` both fire `WinClosed`
on a float opened with `noautocmd = true`.

```
$ nvim --headless -c 'lua <open two floats, one noautocmd, hide one, close one>' -c 'qa!'
w1=1003 w2=1004 w3=1005 | WinNew: WinClosed:1003 WinClosed:1004 WinClosed:1005
```

The practical reading: cmp's menu is invisible to window autocmds at both
ends of its life, so a watcher cannot learn of its arrival *or* its
departure from `WinNew`/`WinClosed`. The other three floats announce only
their departure.

**The events that do fire are cmdline and insert events, and they precede
the window.** Every cmp float in this capture was preceded by a
`CmdlineChanged` fired while the float set was still empty, and followed by
the window existing at the next observation. The ordering is: keystroke,
`CmdlineChanged`, then (about 31 ms later, per the churn measurement above)
the window configured. Telescope's picker produced `CursorMovedI`,
`TextChangedI` and `WinScrolled` on its own windows, never on the editing
window.

## What distinguishes a claiming float

|  | cmp cmdline menu | nvim-notify toast | noice error | telescope picker |
|---|---|---|---|---|
| `zindex` | **1001** | 50 | 50 | 50 |
| buffer `filetype` | **`cmp_menu`** | `notify` | `markdown` | `TelescopeResults` / `TelescopePrompt` / (empty) |
| buffer `name` | `""` | `""` | `""` | `""` |
| `buftype` | `nofile` | `nofile` | `nofile` | `nofile`, prompt window `prompt` |
| `anchor` | NW | **NE** | **NE** | NW |
| `relative` | editor | editor | editor | editor |
| rect (row..row+h-1, col..col+w-1) | 26..27, 0..19 | 0..2, 51..100 | 0..2, 51..100 | 1..26, 10..89 (four windows) |
| `focusable` | true | true | true | true on results, false on the other three |
| `border` | `"none"` | rounded | rounded | `"none"`, drawn in a sibling window |
| selection carrier | win cursor + `cursorline` | none | none | extmark ns `telescope_selection` |
| extmarks on the buffer | **none** | ns `nvim-notify` | ns `nvim-notify` + treesitter | 4 telescope namespaces |
| plugin named by the float | no | no | **yes** (title virt_text `noice.nvim`) | by filetype prefix |
| mode while open | **`c`** | any | any | `i` |
| survives view's `hide` | **yes** | not measured | not measured | not measured |

**The falsifiable check is met, four times over.** A claiming float is
distinguishable from the negative control by `zindex` (1001 against 50),
by buffer `filetype` (`cmp_menu` against `TelescopeResults` /
`TelescopePrompt`), by the mode standing while it is open (`c` against
`i`), and by how a selection is expressed (window cursor against extmark).
The identity fields are the strong ones and the geometry is the weak one,
which is the opposite of the shape a detector would naively take.

Two warnings for anything built on this, both of them geometric:

- **A rect test against view's own palette gets both answers wrong.**
  view's palette box is 70% by 50%, centered inside the terminal minus its
  chrome row (`palette_box` and `palette_rect` in `view-surface`,
  `OverlayBox::rect` in `view-core`). On this session's 100x30 terminal that
  resolves to width `share(100, 70) = 70`, height `share(29, 50) = 14`, at
  terminal row `(29 - 14) / 2 + 1 = 8`, column `(100 - 70) / 2 = 15`:
  terminal rows 8..21, columns 15..84, which is nvim-grid rows 7..20 (the
  grid starts one terminal row down, which is why the session reports
  `lines=29` on a 30-row pty). Read against that box, cmp's menu (grid rows
  26..27) does not intersect the palette at all, while telescope's picker
  (grid rows 1..26, columns 10..89) covers it entirely. Overlap with the
  drawn palette is therefore evidence of nothing: the claim cmp makes is on
  the *completion surface* view owns (the externalized popupmenu that feeds
  that palette), not on the palette's cells. These rows are computed from
  the source constants rather than captured, and they are the only figures
  in this document that are.
- **"Near the bottom of the grid" barely separates them either.** cmp's
  menu bottom edge is row 27; telescope's lowest chrome window (the Help
  box, 1013) spans rows 24..26. One row apart. A threshold rule keyed on
  the last few grid rows flags the picker.

## What this settles for the consumers

- A float watcher cannot be built on `WinNew`, and for cmp's menu it cannot
  be built on `WinClosed` either. `CmdlineEnter` / `CmdlineChanged` /
  `CmdlineLeave` are the events that bracket that float, and the window
  exists roughly 31 ms after the keystroke that summons it. `WinClosed` is a
  teardown edge for the toast, the error float and the picker only.
- Detection has to read identity (`filetype`, `zindex`) with the mode and
  the surface ownership, not rect overlap with what view draws.
- Naming the plugin is possible for noice (its own title virtual text) and
  for telescope (its filetype prefix), and impossible for cmp and
  nvim-notify beyond the filetype itself, which is what the best-effort
  naming rule in the migration-integrity plan already anticipated.
- Absorption is viable on the pinned versions: `nvim_win_set_config(win,
  { hide = true })` is accepted on cmp's menu window, the plugin's own next
  reconfigure preserves it, no replacement window appears, and the window
  id is stable for the life of the cmdline session. The absorbed rows are
  the buffer's lines and the absorbed selection is
  `nvim_win_get_cursor(win)[1]` gated on `vim.wo[win].cursorline`.
