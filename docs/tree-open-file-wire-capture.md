# Wire capture: file-tree open-file

Captured live against the pinned engine: every value below reflects an actual
run of the pinned binary. Source of truth for the file branch of the
`nvim_exec_lua` request `EngineHandle::open_picked` issues when `<CR>` opens a
file chosen in the file tree or the picker.

An earlier revision of this capture recorded the previous chunk shape,
`vim.cmd.edit(vim.fn.fnameescape(path))`, and concluded `fnameescape` was
load-bearing. That shape did not survive Windows: `fnameescape` escapes `\`,
which is Windows' own path separator, so every Windows path failed to open
(disconfirmed on a real Windows host:
`open_file_opens_hostile_character_filenames` failed there before the chunk
below replaced it). This capture records the shipped replacement.

## Engine identity

```
$ nvim --version | head -3
NVIM v0.12.4
Build type: Release
LuaJIT 2.1.1785192264
```

Matches `.engine-pin` (`v0.12.4`) exactly.

## Capture method

`nvim --clean -l <script>.lua` runs a Lua script directly inside an embedded,
headless nvim instance, with the same hermetic `HOME`/`XDG_*` isolation
`EngineConfig::isolated()` uses. The reply carries no value, so what is
captured is the buffer the open leaves: nvim misparsing the path as ex-command
syntax is the failure this guards against, so the capture drives the
`nvim_cmd` call against every hostile-character case the chunk's doc names and
observes the resulting buffer.

The cases below ran that call alone, with `cmd = 'edit'`. The shipped chunk,
verbatim `OPEN_PICKED_CHUNK`:

```lua
local path, how, line, buffer, claimed = ...
local here = vim.api.nvim_get_current_win
local sidebar, edge = {}, {}
for _, win in ipairs(claimed) do
  sidebar[win] = true
end
for _, held in pairs(vim.g.view_native_windows or {}) do
  sidebar[held.win] = true
  edge[held.win] = held.edge
end
local function docked(win)
  return vim.api.nvim_win_get_config(win).relative == ''
end
local tab = vim.api.nvim_get_current_tabpage()
local function ordinary(win)
  if sidebar[win] or not vim.api.nvim_win_is_valid(win)
      or vim.api.nvim_win_get_tabpage(win) ~= tab or not docked(win) then
    return false
  end
  local buf = vim.api.nvim_win_get_buf(win)
  return vim.bo[buf].buftype == '' and not vim.wo[win].winfixbuf
    and not vim.wo[win].previewwindow
end
local beside = nil
if sidebar[here()] then
  local into, fallback = nil, nil
  local layout = vim.api.nvim_tabpage_list_wins(0)
  local candidates = vim.list_extend({}, _G.view_recent_wins or {})
  candidates[#candidates + 1] = vim.fn.win_getid(vim.fn.winnr('#'))
  for _, win in ipairs(vim.list_extend(candidates, layout)) do
    if into == nil and win ~= 0 and ordinary(win) then
      into = win
    end
  end
  for _, win in ipairs(layout) do
    fallback = fallback or docked(win) and win or nil
  end
  if into ~= nil then
    vim.api.nvim_set_current_win(into)
  elseif how ~= 'tabedit' then
    if not docked(here()) then
      vim.api.nvim_set_current_win(fallback)
    end
    local across = edge[here()] == 'above' or edge[here()] == 'below'
    local toward = across and 'k' or 'h'
    beside = vim.fn.winnr(toward) == vim.fn.winnr() and 'belowright'
      or 'aboveleft'
    how = across and 'split' or 'vsplit'
  end
end
if vim.api.nvim_get_mode().mode:sub(1, 2) == 'no' then
  vim.api.nvim_feedkeys(vim.keycode('<Esc>'), 'ni', false)
end
local function say(text)
  vim.api.nvim_echo({ { text } }, true, { err = true })
end
local ok, err = true, nil
if buffer > 0 then
  if not vim.api.nvim_buf_is_valid(buffer)
      or not vim.bo[buffer].buflisted then
    say('That buffer has been closed')
    return here()
  end
  local split = { vsplit = 'vertical sbuffer ', split = 'sbuffer ',
    tabedit = 'tab sbuffer ' }
  ok, err = pcall(vim.cmd,
    (beside and beside .. ' ' or '') .. (split[how] or 'buffer ') .. buffer)
elseif not vim.uv.fs_stat(path) then
  say(path .. ' no longer exists')
  return here()
else
  ok, err = pcall(vim.api.nvim_cmd, {
    cmd = how, args = { path }, magic = { file = false, bar = false },
    mods = { split = beside },
  }, {})
end
if not ok then
  say(tostring(err):match('E%d+:.*') or tostring(err))
  return here()
end
if line > 0 then
  local last = vim.api.nvim_buf_line_count(0)
  vim.api.nvim_win_set_cursor(0, { math.min(line, last), 0 })
end
vim.cmd.redraw()
return here()
```

## 1. Hostile filenames through the shipped chunk

Eleven files created on disk, each opened via the chunk above in the same
running instance. `match=true` requires all three: the call returned without
error, the resulting buffer's name is byte-for-byte the on-disk path, and the
buffer's first line is that specific file's own content.

```
SHIPPED case="plain.txt"                               ok=true match=true bufs=1->1
SHIPPED case="a file.txt"                              ok=true match=true bufs=1->2
SHIPPED case="100%.txt"                                ok=true match=true bufs=2->3
SHIPPED case="#tag.txt"                                ok=true match=true bufs=3->4
SHIPPED case="+weird.txt"                              ok=true match=true bufs=4->5
SHIPPED case="+42"                                     ok=true match=true bufs=5->6
SHIPPED case="++enc.txt"                               ok=true match=true bufs=6->7
SHIPPED case="-dash.txt"                               ok=true match=true bufs=7->8
SHIPPED case="a|b.txt"                                 ok=true match=true bufs=8->9
SHIPPED case="back\\slash.txt"                         ok=true match=true bufs=9->10
SHIPPED case="both space and % and # and +weird.txt"   ok=true match=true bufs=10->11
```

The buffer count strictly increments after the first case (which reuses nvim's
initial empty scratch buffer the way ordinary `:edit` always does), proving no
case silently reused or corrupted a prior buffer. `+42`, `++enc.txt` and
`-dash.txt` are the classic argument-shaped names; line-number, option and
flag syntax to a re-parsed `:edit`; and all pass through the args list as
ordinary filenames.

## 2. The same call, `magic` left at its default (negative control)

Identical script, identical files, chunk shortened to
`nvim_cmd({ cmd = 'edit', args = { path } }, {})`:

```
MAGIC-ON case="plain.txt"      ok=true match=true
MAGIC-ON case="a file.txt"     ok=true match=true
MAGIC-ON case="100%.txt"       ok=true match=false buf_name=".../100/<previous buffer's full path>.txt"
MAGIC-ON case="#tag.txt"       ok=true match=false buf_name=".../<previous buffer's full path>tag.txt"
MAGIC-ON case="+weird.txt"     ok=true match=true
MAGIC-ON case="+42"            ok=true match=true
MAGIC-ON case="++enc.txt"      ok=true match=true
MAGIC-ON case="-dash.txt"      ok=true match=true
MAGIC-ON case="a|b.txt"        ok=true match=true
MAGIC-ON case="back\\slash.txt" ok=true match=false buf_name=".../backslash.txt"
MAGIC-ON case="both space and % and # and +weird.txt" ok=true match=false
```

Two findings the implementation's doc depends on:

- The two halves of the shipped shape protect different characters. The args
  list alone already carries a space, a leading `+`/`-`, and `|` safely; with
  `magic` fully defaulted they still open correctly. `magic.file = false` is
  what protects exactly `%`, `#` and `\`: with it left on, `%` and `#` expand
  to the previous buffer's path *inside* the filename, and `\` is eaten as an
  escape.
- The failure mode is silent. Every corrupted case returns `ok=true` and opens
  a new, empty, wrongly-named buffer, worse than the old escaped shape's
  failure mode (`E499`, a visible error), because the file on screen differs
  from the one the user selected and nothing tells them so.

## Conclusions for the implementation

- `EngineHandle::open_picked` issues `nvim_exec_lua` with the chunk above as a
  request, reusing an already-open buffer for `path` the same way an ordinary
  `:edit` would: it duplicates nothing. Its answer releases the keys typed
  behind the open, which wait for it.
- Both halves of the chunk are load-bearing and neither subsumes the other:
  dropping the args-list shape re-exposes the space/`+` class, dropping
  `magic.file = false` re-exposes the silent `%`/`#`/`\` class. The capture
  above measures each half separately.
- `fnameescape` must not return: it escapes `\` and therefore breaks every
  Windows path, the bug that retired the previous chunk shape.
- Live-verified in `crates/view-engine/tests/open_file_live.rs`, which drives
  `EngineHandle::open_picked` itself, the real method and no reimplemented
  chunk, for each hostile case above that the host filesystem can represent
  (`|` and `\` cannot exist in Windows filenames, so those two fixtures are
  unix-only) and asserts the resulting buffer's name and content.
