# Wire capture: file-tree open-file

Captured live against the pinned engine per "capture, never recall." Source
of truth for the `nvim_exec_lua` call `EngineHandle::open_file` issues to
open a tree entry's file, the seam `<CR>` on a selected tree file routes
through.

An earlier revision of this capture recorded the previous chunk shape,
`vim.cmd.edit(vim.fn.fnameescape(path))`, and concluded `fnameescape` was
load-bearing. That shape did not survive Windows: `fnameescape` escapes
`\`, which is Windows' own path separator, so every Windows path failed to
open (disconfirmed on a real Windows host -- `open_file_opens_hostile_character_filenames`
failed there before the chunk below replaced it). This capture records the
shipped replacement.

## Engine identity

```
$ nvim --version | head -3
NVIM v0.12.4
Build type: Release
LuaJIT 2.1.1785192264
```

Matches `.engine-pin` (`v0.12.4`) exactly.

## Capture method

`nvim --clean -l <script>.lua` runs a Lua script directly inside an
embedded, headless nvim instance, with the same hermetic `HOME`/`XDG_*`
isolation `EngineConfig::isolated()` uses. `open_file` is fire-and-forget
(`EngineHandle::notify`, no reply) -- there is no reply shape to capture,
only that the chunk actually opens the intended file rather than nvim
misparsing the path as ex-command syntax, so the capture drives the chunk
against every hostile-character case `OPEN_FILE_CHUNK`'s doc names and
observes the resulting buffer.

The Lua chunk under test (verbatim `OPEN_FILE_CHUNK`):

```lua
local path = ...
vim.api.nvim_cmd({ cmd = 'edit', args = { path }, magic = { file = false, bar = false } }, {})
```

## 1. Hostile filenames through the shipped chunk

Eleven files created on disk, each opened via the chunk above in the same
running instance. `match=true` requires all three: the call returned
without error, the resulting buffer's name is byte-for-byte the on-disk
path, and the buffer's first line is that specific file's own content.

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

The buffer count strictly increments after the first case (which reuses
nvim's initial empty scratch buffer the way ordinary `:edit` always does),
proving no case silently reused or corrupted a prior buffer. `+42`,
`++enc.txt` and `-dash.txt` are the classic argument-shaped names --
line-number, option and flag syntax to a re-parsed `:edit` -- and all pass
through the args list as ordinary filenames.

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

- The two halves of the shipped shape protect different characters. The
  args list alone already carries a space, a leading `+`/`-`, and `|`
  safely -- with `magic` fully defaulted they still open correctly.
  `magic.file = false` is what protects exactly `%`, `#` and `\`: with it
  left on, `%` and `#` expand to the previous buffer's path *inside* the
  filename, and `\` is eaten as an escape.
- The failure mode is silent, not loud. Every corrupted case returns
  `ok=true` and opens a new, empty, wrongly-named buffer -- worse than the
  old escaped shape's failure mode (`E499`, a visible error), because
  nothing tells the user the file on screen is not the file they selected.

## Conclusions for the implementation

- `EngineHandle::open_file(&self, path: &str)` issues `nvim_exec_lua` with
  the chunk above as a fire-and-forget `notify` (matching
  `RegisterMappings`'s calling convention: no generation to correlate, no
  reply awaited), reusing an already-open buffer for `path` the same way an
  ordinary `:edit` would rather than duplicating it.
- Both halves of the chunk are load-bearing and neither subsumes the
  other: dropping the args-list shape re-exposes the space/`+` class,
  dropping `magic.file = false` re-exposes the silent `%`/`#`/`\` class.
  The capture above measures each half separately.
- `fnameescape` must not return: it escapes `\` and therefore breaks every
  Windows path, the bug that retired the previous chunk shape.
- Live-verified in `crates/view-engine/tests/open_file_live.rs`, which
  drives `EngineHandle::open_file` itself (not a reimplemented chunk) for
  each hostile case above and asserts the resulting buffer's name and
  content.
