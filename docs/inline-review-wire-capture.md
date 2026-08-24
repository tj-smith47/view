# Wire capture: drawing an agent's proposed diff inside the file it edits

Captured live against the pinned engine per "capture, never recall." Source of
truth for `REVIEW_SHOW_CHUNK` and `REVIEW_CLEAR_CHUNK`, the two `nvim_exec_lua`
chunks `RpcCall::ReviewShow` and `RpcCall::ReviewClear` issue to put a whole
open review into the buffer it proposes to change, and to take it back off.

Both are notifications, not requests: nothing view holds depends on an answer,
and the loop that emits them is the loop that paints. What is recorded here is
therefore not a reply shape -- there is none -- but what the editor holds
afterwards: which extmarks landed on which rows, which mappings the buffer
answers, and (the claim the whole design rests on) that the buffer's own text
and its `changedtick` did not move.

## Engine identity

```
$ nvim --version | head -3
NVIM v0.12.4
Build type: Release
LuaJIT 2.1.1787058514
```

Matches `.engine-pin` (`v0.12.4`).

## Capture method

`Engine::spawn(EngineConfig::isolated())` -- `nvim --embed` under the hermetic
`HOME`/`XDG_*` isolation a real session gets -- followed by
`nvim_ui_attach(80, 24)`, then `EngineHandle::review_show` /
`review_clear` themselves. **Not `--headless -l`**, for the reason
`docs/checktime-wire-capture.md` gives: view always attaches a UI before
issuing any RPC call, and the difference is material. Here it is the whole
point -- a screenless capture can only echo extmark *attributes* back, and the
first version of this capture did exactly that while the deletion highlight
was painting one row too far. Every claim below about what the user sees is a
`screenattr`/`screenstring` read of a rendered screen.

The editor is read back through `nvim_buf_get_extmarks(details = true)`,
`nvim_buf_get_keymap`, `nvim_buf_get_changedtick`, `vim.fn.screenattr` and
`vim.fn.execute('messages')`. The capture harness is the live test's own
(`crates/view-engine/tests/inline_review_live.rs`), so the session shape here
and the session shape the assertions run in cannot drift apart.

The channel id in the capture is `1` -- this embedded connection's own, learned
at the `nvim_get_api_info` handshake and carried on `EngineHandle`. A real
session's differs; nothing in the chunk hard-codes it.

## 1. A three-hunk review, shown

The payload: a replacement of row 1 (`two` -> `TWO`) which is the current hunk
and so carries the header, a pure insertion before row 3, and a stale
replacement of row 5 that proposes nothing. `cursor_row = 1`, `focus = true`,
`open_target = 'current'`.

Read back as `row:end_row:line_hl_group:sign_text:virt_lines_above:virt|lines`:

```
MARK 1:1:DiffDelete:nil:nil:
MARK 1:nil:nil:▶ :false:hunk 1/3 -- <leader>ha accept  ]c next  <leader>hq leave/DiffText|+TWO/DiffAdd
MARK 3:nil:nil:nil:true:+inserted/DiffAdd
MARK 5:5:DiffChange:nil:nil:
```

Four extmarks for three hunks. A hunk that replaces rows gets a range mark
carrying the row highlight -- `DiffDelete`, or `DiffChange` once the buffer has
moved under it (mark 4) -- and a hunk with anything to propose gets a second
mark carrying the virtual lines. The pure insertion (mark 3) has only the
second kind, and it is the one mark with `virt_lines_above = true`: it replaces
no row, so its lines are drawn above the row it inserts before rather than
under the rows it would replace. The header and the `▶` sign appear on the
current hunk alone, which is what makes "which hunk am I about to accept"
answerable without the panel.

The groups are nvim's own `DiffDelete`/`DiffChange`/`DiffAdd`/`DiffText`: every
colorscheme a migrating user already has defines them, so a proposal is legible
under a theme view has never seen.

The range mark's `end_row` is one *below* the hunk's own `old_range` end: the
range is half-open and nvim's `end_row` is inclusive for `line_hl_group`, so
`end_row = m.end_row - 1`. Passing the range end through paints the untouched
row after every hunk as deleted -- invisible in this section (the stored
attribute reads plausible either way) and obvious in the rendered screen below,
which is why that block exists.

`sign_text` reads back as `"▶ "` -- nvim pads a one-cell sign to the two cells
the sign column is wide. `virt_lines_above` reads back as `nil`, not `false`,
on a mark that set no `virt_lines`.

```
KEYMAP [c -> <Cmd>call rpcnotify(1, 'view_invoke', 'review', 'prev')<CR>
KEYMAP \hA -> <Cmd>call rpcnotify(1, 'view_invoke', 'review', 'accept_all')<CR>
KEYMAP \hR -> <Cmd>call rpcnotify(1, 'view_invoke', 'review', 'rediff')<CR>
KEYMAP \ha -> <Cmd>call rpcnotify(1, 'view_invoke', 'review', 'accept')<CR>
KEYMAP \hq -> <Cmd>call rpcnotify(1, 'view_invoke', 'review', 'leave')<CR>
KEYMAP \hx -> <Cmd>call rpcnotify(1, 'view_invoke', 'review', 'reject')<CR>
KEYMAP ]c -> <Cmd>call rpcnotify(1, 'view_invoke', 'review', 'next')<CR>
GLOBAL n-maps after show: 55
```

Seven mappings, read out of `nvim_buf_get_keymap(buf, 'n')` -- the reviewed
buffer's own list, and no other buffer's -- and the global map count is
untouched. `<leader>` is nvim's default `\` here, expanded by
`vim.keymap.set` when the map is set, so the review's keys
follow whatever `mapleader` the user's own config chose. The right-hand side is
literal `rpcnotify` text rather than an opaque Lua callback, which is what lets
`:map`, `maparg()` and any plugin that introspects mappings show exactly what
view installed and why.

```
TEXT unchanged=true state [changedtick 2, modified true] -> [changedtick 2, modified true]
CURSOR row=2 (focus=true, cursor_row=1)
```

The text is byte-identical across the draw and `changedtick` did not move: an
extmark is not an edit. That is the whole reason the decoration can be view's
while the text stays nvim's, and it is why a proposal can be displayed over a
file the user is still editing without a merge to undo later. (`modified=true`
is the capture fixture's own doing -- it built the buffer with
`nvim_buf_set_lines` -- and is unchanged by the draw; the live test asserts the
flag across the call rather than its absolute value.) The cursor lands on row
2, 1-indexed, for the 0-indexed `cursor_row = 1` the payload named.

The same review as the user's screen holds it -- `screenattr` at column 3 and
`screenstring` across the row, after a `redraw`:

```
row  attr  screen
 1     0   |  one|
 2    23   |▶ two|            DiffDelete   the row the hunk replaces
 3    24   |  hunk 1/3 ...|   DiffText     header, a virtual line
 4    21   |  +TWO|           DiffAdd      the proposal, a virtual line
 5     0   |  three|                       untouched, and painted as such
 6    21   |  +inserted|      DiffAdd      the insertion, drawn above its row
 7     0   |  four|
 8     0   |  five|
 9    22   |  six|            DiffChange   the stale hunk
```

Row 5 is the assertion that matters: the row after a hunk carries the same
attribute as an untouched row. Nothing but a rendered read answers it -- this is
the exact cell the half-open/inclusive mismatch paints, and the extmark
attributes above look correct in both versions.

## 2. The user inserts two lines above every hunk

```
MARK 3:3:DiffDelete:nil:nil:
MARK 3:nil:nil:▶ :false:hunk 1/3 -- <leader>ha accept  ]c next  <leader>hq leave/DiffText|+TWO/DiffAdd
MARK 5:nil:nil:nil:true:+inserted/DiffAdd
MARK 7:7:DiffChange:nil:nil:
```

Every mark moved down by exactly two, with no call from view. Extmarks track
edits themselves, so ordinary typing inside a buffer under review costs no RPC
and no redraw call at all, and the decoration cannot drift from the rows it
describes while the user works around it.

## 3. A mark past the end of a shrunk buffer

The buffer is cut to one line, then a payload naming row 40 is shown.

```
line_count=1 keys=7
MARK 1:40:DiffDelete:nil:nil:
MARK 1:nil:nil:▶ :false:hunk 1/1/DiffText|+late/DiffAdd
```

`strict = false` is load-bearing rather than defensive: the row is clamped and
nothing raises. Under the default `strict = true` this call raises, and a raise
here abandons the rest of the chunk -- including the seven `vim.keymap.set`
calls -- leaving a buffer decorated with a review no key could answer. The race
is ordinary: the user deletes lines while a notify carrying rows computed
against the older buffer is still in flight.

## 4. `review_clear`, twice

```
marks=0 buffer keymaps=0
messages after two clears: ""
```

The namespace is emptied and every mapping is gone. A second clear over an
already-clear buffer answers without error: `vim.keymap.del` raises for a
mapping that does not exist, which is what the `pcall` around it absorbs.
Idempotence is what lets a review's teardown run without first proving a show
ever landed.

The empty message history is worth reading precisely: it says the `pcall`
absorbed the delete, not that a raise would have been visible. Measured on
this same session, a notification whose chunk raises reaches nvim's log and
nothing else -- not `:messages`, not `v:errmsg`, and not the connection that
sent it. That is what the `nvim_buf_is_valid` guard at the head of each chunk
exists for, and also why no test can observe its absence: the cost of dropping
it is noise in a log file, paid by whoever debugs the session later.

## 5. Where a file no window shows lands

```
current: wins=1 current_buf_is_proposal=true
split: wins=2 current_buf_is_proposal=true other_still_visible=true
```

With `ai.review.open_target = current` the proposal takes the window the user
is in and the layout is untouched -- the same "show me this file" move the
picker and the file tree already make. With `split` the proposal gets a new
window and whatever was being read stays on screen beside it. Either way the
cursor ends in the proposal, which is what `focus = true` asked for. A file
some window already shows is drawn where it already is: neither target moves it
or splits anything.

## Production chunk shape: review_show

```lua
local buf, marks, cursor_row, focus, target, channel, keys = ...
local ns = vim.api.nvim_create_namespace('view_review')
if not vim.api.nvim_buf_is_valid(buf) then
  return
end
vim.api.nvim_buf_clear_namespace(buf, ns, 0, -1)
for _, m in ipairs(marks) do
  if m.end_row > m.row then
    vim.api.nvim_buf_set_extmark(buf, ns, m.row, 0, {
      end_row = m.end_row - 1,
      line_hl_group = m.stale and 'DiffChange' or 'DiffDelete',
      priority = 100,
      strict = false,
    })
  end
  local virt = {}
  if m.header ~= nil then
    virt[#virt + 1] = { { m.header, 'DiffText' } }
  end
  for _, line in ipairs(m.added) do
    virt[#virt + 1] = { { '+' .. line, 'DiffAdd' } }
  end
  if #virt > 0 then
    vim.api.nvim_buf_set_extmark(buf, ns, m.anchor, 0, {
      virt_lines = virt,
      virt_lines_above = m.end_row == m.row,
      sign_text = m.current and '▶' or nil,
      sign_hl_group = 'DiffText',
      priority = 100,
      strict = false,
    })
  end
end
local displaced = _G.view_review_displaced or {}
_G.view_review_displaced = displaced
local before = nil
if displaced[buf] == nil then
  before = {}
  for _, m in ipairs(vim.api.nvim_buf_get_keymap(buf, 'n')) do
    before[m.lhs] = m
  end
end
for _, k in ipairs(keys) do
  vim.keymap.set('n', k.lhs, string.format(
    "<Cmd>call rpcnotify(%d, 'view_invoke', 'review', '%s')<CR>", channel, k.verb),
    { buffer = buf, silent = true, desc = 'view: review ' .. k.verb })
end
if before ~= nil then
  local taken = {}
  for _, m in ipairs(vim.api.nvim_buf_get_keymap(buf, 'n')) do
    if m.desc ~= nil and m.desc:find('view: review ', 1, true) == 1 and before[m.lhs] ~= nil then
      taken[#taken + 1] = before[m.lhs]
    end
  end
  displaced[buf] = taken
end
if focus then
  local win = vim.fn.win_findbuf(buf)[1]
  if win == nil then
    if target == 'split' then
      vim.cmd('split')
    end
    win = vim.api.nvim_get_current_win()
    vim.api.nvim_win_set_buf(win, buf)
  end
  vim.api.nvim_set_current_win(win)
  local rows = vim.api.nvim_buf_line_count(buf)
  vim.api.nvim_win_set_cursor(win, { math.max(1, math.min(cursor_row + 1, rows)), 0 })
  vim.cmd('normal! zz')
end
```

`m.header` is omitted from the payload rather than sent as nil for a hunk that
carries no header: msgpack nil decodes to `vim.NIL`, which is truthy, so a
present-but-nil key would draw an empty header line on every hunk.

## Production chunk shape: review_clear

```lua
local buf, keys = ...
local ns = vim.api.nvim_create_namespace('view_review')
local displaced = _G.view_review_displaced
local restore = nil
if displaced ~= nil then
  restore = displaced[buf]
  displaced[buf] = nil
end
if not vim.api.nvim_buf_is_valid(buf) then
  return
end
vim.api.nvim_buf_clear_namespace(buf, ns, 0, -1)
for _, k in ipairs(keys) do
  pcall(vim.keymap.del, 'n', k.lhs, { buffer = buf })
end
if restore ~= nil then
  vim.api.nvim_buf_call(buf, function()
    for _, m in ipairs(restore) do
      pcall(vim.fn.mapset, 'n', false, m)
    end
  end)
end
```
