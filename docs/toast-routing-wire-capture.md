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

## Finding: noice's health errors never reach `msg_show` at all

Captured over a full `compat/fixtures/heavy` launch with `VIEW_LOG` pointed
at a file (`VIEW_COMPAT_LOG` in the harness, since `make_hermetic` sweeps
every host variable that is not an allow-listed passthrough, `VIEW_LOG`
included). The plan this task was written against assumed noice's three
health errors arrive as three view toasts once `HOLD_NOTIFY_CHUNK` has taken
`vim.notify` over. They do not.

```
$ grep -c msg_show ~/.cache/view-compat-noice.log
0
$ # the same launch, asked what nvim-notify is holding
nvim-notify-history=3
```

`noice/util/init.lua`'s `M.notify` calls `require("notify").notify(...)`
directly whenever the `notify` module is present -- which it is in the heavy
fixture -- so it never goes through `vim.notify` and view's takeover cannot
see it. All three errors land in nvim-notify's own history and are drawn as
a float. On screen: `Noice can't work when the GUI has` is drawn by that
float, and view's own float detector raises its anonymous-family notice
(`view: a plugin is drawing over the message area`) about it.

Three consequences, each acted on rather than worked around:

- The startup hold catches nothing on this path. Its closing sentence
  (`Startup messages from other plugins are in the message history.`) is
  therefore conditional on something actually having been parked, rather
  than a claim the notice always makes.
- The deliverable that turns the row green is the claimant probe below plus
  the composition suppression, not the hold.
- A discriminating kind does exist on the *other* path -- a plugin calling
  `vim.notify(msg, vim.log.levels.ERROR)` reaches `msg_show` as `echoerr` --
  but `echoerr` is a persistent kind, and persistent kinds are never held.
  The hold was not widened to cover it: an error the user is meant to read
  is exactly what a notice about surfaces must not stand in for.

## Finding: `package.loaded` is the only portable claimant reading

Probed live against the heavy fixture, which loads plugins through
lazy.nvim:

```
$ # after VimEnter, at the first SafeState
package.loaded['noice'] ~= nil   -->  true
```

No plugin-manager API is consulted. lazy.nvim, packer, vim-plug and a plain
`runtimepath` drop all end in the same registry, and the registry answers
what is *loaded* rather than what is installed -- a plugin present on disk
but never required has taken no surface.

The chunk that asks it, verbatim `PROBE_CLAIMANTS_CHUNK`:

```lua
local channel, modules = ...
local group = vim.api.nvim_create_augroup(
  'view_bridge_claimants', { clear = true })
local reported, first = {}, true
local deadline = vim.uv.now() + 60000
vim.api.nvim_create_autocmd('SafeState', {
  group = group,
  callback = function()
    local loaded, fresh = {}, false
    for _, name in ipairs(modules) do
      if package.loaded[name] ~= nil then
        loaded[#loaded + 1] = name
        if not reported[name] then
          reported[name] = true
          fresh = true
        end
      end
    end
    if first or fresh then
      first = false
      pcall(vim.rpcnotify, channel, 'view_bridge', 'claimants', loaded)
    end
    if #loaded == #modules or vim.uv.now() > deadline then
      pcall(vim.api.nvim_del_augroup_by_id, group)
    end
  end,
})```

`SafeState` rather than an inline read at `VimEnter`: a manager that
finishes its own deferred loading on a timer after `VimEnter` (lazy.nvim's
`VeryLazy`) would be read too early otherwise. And repeated rather than
`once`, because a cold first launch clones the whole stack over the network
and idles many times before the plugin it is about to load exists -- the
notify goes out only on the first answer or a changed one, and the group
deletes itself once every module is found or the session is a minute old. The live proof
that the chunk answers, and answers differently for a session that loaded
the module and one that did not, is
`crates/view/tests/bridge_live.rs`'s
`the_claimant_probe_answers_what_the_session_actually_loaded`.

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
