# Wire capture: the `vim.notify` takeover (`HOLD_NOTIFY_CHUNK`)

Captured live against the pinned engine per "capture, never recall." Source of
truth for what "re-pointed at the engine default" (spec §5.5) means on the
wire, and for what `view_engine::nvim_api::HOLD_NOTIFY_CHUNK` reproduces.

## Engine identity

```
$ nvim --version | head -3
NVIM v0.12.4
Build type: Release
LuaJIT 2.1.1787058514
```

Matches `.engine-pin` (`v0.12.4`) exactly. `nvim_exec_lua` answers
`vim.version().api_level` = `14`.

## Capture method

A standalone Python msgpack-rpc client, the same shape
`docs/toast-routing-wire-capture.md` uses: `nvim --embed --clean` (never
`--headless`, which suppresses `ext_messages` redraws entirely), hermetic
`XDG_*`/`HOME` isolation, full ext-option set at attach
(`ext_linegrid`/`ext_cmdline`/`ext_popupmenu`/`ext_messages`/`ext_tabline`).
Every `msg_*` event in each redraw batch is drained between calls, so each
block below is the whole message traffic one `vim.notify` call produced.

Two axes are walked on both sides of the takeover, because a reproduction is
only proven over what was varied: **level**, all five `vim.log.levels` values,
and **opts**, the four shapes the pinned default distinguishes
(`{ _truncate = true }`, `{ _truncate = false }`, no opts at all, and an opts
table carrying something the default ignores).

## Finding: the engine default is not recoverable once a plugin patches it

`vim.notify` is a field of the `vim` table itself, and the module a `require`
reaches for it *is* that table:

```
$ nvim --headless --clean -c 'lua ...' -c q
require vim._core.editor: true table
m.notify == vim.notify: true
m == vim (same table?): true
```

So after `vim.notify = plugin_fn`:

```
after patch: vim.notify == orig   false
m.notify == orig                  false
```

There is no second reference to restore from. A takeover therefore either
saves the function standing when it runs, or reproduces the default's
behaviour. Saving is wrong here: nvim-notify's own documented setup is
`vim.notify = require('notify')` in `init.lua`, and the plan is applied at
`VimEnter`, so the saved function is very often the plugin's -- the exact
inverse of the takeover. `HOLD_NOTIFY_CHUNK` reproduces, and this capture is
what it reproduces from.

## Finding: the default's wire output, one call per `vim.log.levels` value

`vim.log.levels` on the pin:

```
{'TRACE': 0, 'DEBUG': 1, 'INFO': 2, 'WARN': 3, 'ERROR': 4, 'OFF': 5}
```

`debug.getinfo(vim.notify)` → `source = 'vim/_core/editor'`,
`linedefined = 548`, `nparams = 3`.

Each call below is `vim.notify('default-<level>', vim.log.levels.<LEVEL>)`,
and the line under it is the entire `msg_*` traffic it produced:

```
=== default vim.notify @ TRACE ===
  ['msg_show', ['echomsg', [[0, 'default-trace', 0]], False, True, False, 1, '']]
=== default vim.notify @ DEBUG ===
  ['msg_show', ['echomsg', [[0, 'default-debug', 0]], False, True, False, 2, '']]
=== default vim.notify @ INFO ===
  ['msg_show', ['echomsg', [[0, 'default-info', 0]], False, True, False, 3, '']]
=== default vim.notify @ WARN ===
  ['msg_show', ['echomsg', [[45, 'default-warn', 26]], False, True, False, 4, '']]
=== default vim.notify @ ERROR ===
  ['msg_show', ['echoerr', [[25, 'default-error', 6]], False, True, False, 5, '']]
```

Three distinct shapes, not five: `TRACE`/`DEBUG`/`INFO` are one unhighlighted
`echomsg`, `WARN` is an `echomsg` under `WarningMsg` (hl id 26), and `ERROR` is
a `msg_show` of kind `echoerr` under hl id 6. A call passing no level at all
takes the unhighlighted arm:

```
vim.notify('no-level')                                   -> [True]
    ['msg_show', ['echomsg', [[0, 'no-level', 0]], False, True, False, 1, '']]
vim.notify('multi\nline')                                -> [True]
    ['msg_show', ['echomsg', [[0, 'multi\nline', 0]], False, True, False, 3, '']]
```

The default does not stringify its argument. A non-string `msg` is refused by
`nvim_echo` itself, from inside the default:

```
vim.notify({ 1, 2 })
  -> [False, '[string "vim/_core/editor"]:550: Invalid chunk: expected Array with 1 or 2 Strings']
```

`HOLD_NOTIFY_CHUNK` passes `msg` through unconverted for that reason: a
takeover that stringified would hand nvim a message the caller never wrote,
and would accept a call the engine rejects.

## Finding: `opts` is not ignored -- `_truncate` reaches `nvim_echo`

The reproduction target, read from the pinned tree rather than recalled:

```
$ sed -n '548,555p' \
    "$VIMRUNTIME/lua/vim/_core/editor.lua"
function vim.notify(msg, level, opts) -- luacheck: no unused args
  local chunks = { { msg, level == vim.log.levels.WARN and 'WarningMsg' or nil } }
  vim.api.nvim_echo(
    chunks,
    true,
    { err = level == vim.log.levels.ERROR, _truncate = opts and opts._truncate }
  )
end
```

`opts` carries one term the default forwards, `_truncate`, so a reproduction
that took `(msg, level, _)` would be a partial one. The opts axis, walked
against the default before the takeover is installed (200 `T`s, wider than the
80-column capture UI, so a truncation would be visible in the echoed length):

```
=== default @ INFO opts={ _truncate = true } ===
  ['msg_show', kind='echomsg', attr=0, hl=0, len=208, head='default-TTTTTTTTTT']
=== default @ INFO opts={ _truncate = false } ===
  ['msg_show', kind='echomsg', attr=0, hl=0, len=208, head='default-TTTTTTTTTT']
=== default @ INFO opts=nil opts ===
  ['msg_show', kind='echomsg', attr=0, hl=0, len=208, head='default-TTTTTTTTTT']
=== default @ INFO opts={ title = 't' } ===
  ['msg_show', kind='echomsg', attr=0, hl=0, len=14, head='default-titled']
```

`_truncate` is **inert on this pin over `ext_messages`**: the flag shortens
what nvim itself draws in the message area, and an externalized consumer
receives the whole text either way, so all three long-message rows are the
same 208 characters. That is a measured result, not a reason to drop the term
-- `HOLD_NOTIFY_CHUNK` forwards it anyway, so the reproduction stays true term
for term if a later pin gives the flag an observable effect. Nothing else in
this repo would catch that change.

## The chunk, and its output beside the default's

verbatim `HOLD_NOTIFY_CHUNK`:

```lua
local function notify(msg, level, opts)
  local chunks =
    { { msg, level == vim.log.levels.WARN and 'WarningMsg' or nil } }
  vim.api.nvim_echo(chunks, true, {
    err = level == vim.log.levels.ERROR,
    _truncate = opts and opts._truncate,
  })
end
local function hold()
  if vim.notify ~= notify then
    vim.notify = notify
  end
end
hold()
local group = vim.api.nvim_create_augroup(
  'view-hold-notify', { clear = true })
vim.api.nvim_create_autocmd('SafeState', {
  group = group,
  callback = hold,
})
```

Run in the same session, immediately after the block above, then one
`vim.notify('held-<level>', …)` per level:

```
=== held vim.notify @ TRACE ===
  ['msg_show', ['echomsg', [[0, 'held-trace', 0]], False, True, False, 10, '']]
=== held vim.notify @ DEBUG ===
  ['msg_show', ['echomsg', [[0, 'held-debug', 0]], False, True, False, 11, '']]
=== held vim.notify @ INFO ===
  ['msg_show', ['echomsg', [[0, 'held-info', 0]], False, True, False, 12, '']]
=== held vim.notify @ WARN ===
  ['msg_show', ['echomsg', [[45, 'held-warn', 26]], False, True, False, 13, '']]
=== held vim.notify @ ERROR ===
  ['msg_show', ['echoerr', [[25, 'held-error', 6]], False, True, False, 14, '']]
```

and the same opts axis, run against the held function in the same session:

```
=== held @ INFO opts={ _truncate = true } ===
  ['msg_show', kind='echomsg', attr=0, hl=0, len=205, head='held-TTTTTTTTTTTTT']
=== held @ INFO opts={ _truncate = false } ===
  ['msg_show', kind='echomsg', attr=0, hl=0, len=205, head='held-TTTTTTTTTTTTT']
=== held @ INFO opts=nil opts ===
  ['msg_show', kind='echomsg', attr=0, hl=0, len=205, head='held-TTTTTTTTTTTTT']
=== held @ INFO opts={ title = 't' } ===
  ['msg_show', kind='echomsg', attr=0, hl=0, len=11, head='held-titled']
```

Kind, attr id and hl id match the default's row for row on both axes; the
lengths differ by exactly the three characters between the `default-` and
`held-` tags. Only the message text and the monotonically increasing message
id differ. That is the whole claim behind "re-pointed at the engine default":
a consumer reading `msg_show` cannot tell the two apart.

One divergence, in the error text and not in the message traffic: a non-string
`msg` is refused by `nvim_echo` from inside the held function too, with the
same message, but the location names the takeover's own chunk rather than the
runtime file.

```
held      -> [0, 'Lua: [string "<nvim>"]:4: Invalid chunk: expected Array with 1 or 2 Strings ...']
default   -> [False, '[string "vim/_core/editor"]:550: Invalid chunk: expected Array with 1 or 2 Strings']
```

Unavoidable for any reproduction -- a Lua function knows where it was
compiled -- and it reaches only a caller that already passed an argument the
API rejects.

`vim.notify_once` routes through the held function too -- it calls `vim.notify`
by name rather than holding its own reference:

```
notify_once -> ['msg_show', ['echomsg', [[0, 'once-routed', 0]], False, True, False, 1, '']]
```

## Finding: the guard holds, and a plain assignment does not

With the chunk above installed, a plugin patches the function and nvim is
given one turn on its main loop:

```
### a plugin re-patches vim.notify after the takeover
_G.__plugin installed, vim.notify == _G.__plugin: true
after SafeState, vim.notify is the plugin's: false
=== vim.notify after the re-assert ===
  ['msg_show', ['echomsg', [[0, 'after-repatch', 0]], False, True, False, 19, '']]
the plugin function saw: nil
```

The same session driven from a chunk whose last line is a plain
`vim.notify = notify` with no autocmd -- the falsifiable control:

```
### a plugin re-patches vim.notify after the takeover
_G.__plugin installed, vim.notify == _G.__plugin: true
after SafeState, vim.notify is the plugin's: true
=== vim.notify after the re-assert ===
  (nothing)
the plugin function saw: after-repatch
```

Nothing reaches `ext_messages` at all: the message goes to the plugin, which
is where a one-shot takeover leaves it.

Re-applying the chunk replaces its guard rather than stacking a second one
(`clear = true` on the augroup), and an idle transition that changed nothing
writes nothing:

```
chunk err (2nd apply): None
autocmds on the group: 1
unchanged across SafeState: true
```
