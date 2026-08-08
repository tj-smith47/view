# Wire capture: file-tree open-file

Captured live against the pinned engine per "capture, never recall." Source
of truth for the `nvim_exec_lua` call `EngineHandle::open_file` issues to
open a tree entry's file, the seam `<CR>` on a selected tree file routes
through.

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
misparsing the path as ex-command syntax, so the capture instead drives the
chunk against every hostile-character case `OPEN_FILE_CHUNK`'s doc names
and observes the resulting buffer.

The Lua chunk under test:

```lua
local path = ...
vim.cmd.edit(vim.fn.fnameescape(path))
```

## 1. Hostile filenames, escaped

Five files created on disk, each opened via the chunk above in the same
running instance (so buffer count strictly increases, proving no case
silently reused or corrupted a prior buffer):

```
case="a file.txt"                              ok=true  buf_name=".../a file.txt"                              first_line="hello from a file.txt"
case="100%.txt"                                ok=true  buf_name=".../100%.txt"                                first_line="hello from 100%.txt"
case="#tag.txt"                                ok=true  buf_name=".../#tag.txt"                                first_line="hello from #tag.txt"
case="+weird.txt"                               ok=true  buf_name=".../+weird.txt"                              first_line="hello from +weird.txt"
case="both space and % and # and +weird.txt"    ok=true  buf_name=".../both space and % and # and +weird.txt"  first_line="hello from both space and % and # and +weird.txt"
```

`before_bufs`/`after_bufs` (dropped from the table above for width) confirm
each case actually opened a distinct new buffer (`1→1` only for the very
first case, which reuses nvim's initial empty scratch buffer the way
ordinary `:edit` always does; every case after strictly increments). Every
`buf_name` matches the literal on-disk path byte-for-byte, and every
`first_line` matches that specific file's own content -- none of the four
hostile characters (space, leading `+`, bare `%`, bare `#`) caused nvim to
open the wrong file, merge into an existing buffer, or silently no-op.

## 2. The same path, unescaped (negative control)

The same `100%.txt` case run through `vim.cmd.edit(path)` directly, with
`vim.fn.fnameescape` removed:

```
unescaped case ok=false err=...E499: Empty file name for '%' or '#', only works with ":p:h"
```

Confirms `fnameescape` is load-bearing, not decorative: without it, `:edit`
on a path containing a bare `%` does not open the wrong file, it errors
outright, because nvim's command-line parser expands an unescaped `%` to
the current buffer's name before the path ever reaches `:edit`'s own
argument handling.

## Conclusions for the implementation

- `EngineHandle::open_file(&self, path: &str)` issues `nvim_exec_lua` with
  the chunk above as a fire-and-forget `notify` (matching
  `RegisterMappings`'s calling convention: no generation to correlate, no
  reply awaited), reusing an already-open buffer for `path` the same way an
  ordinary `:edit` would rather than duplicating it.
- `vim.fn.fnameescape` is required, not optional hardening: case 2 shows the
  unescaped form does not degrade gracefully, it errors, for the single
  most common hostile character (`%`) a real project tree is likely to
  contain (percent-encoded asset names, `100%.txt`-style report files).
- Live-verified in `crates/view-engine/tests/open_file_live.rs`, which
  drives `EngineHandle::open_file` itself (not a reimplemented chunk) for
  each hostile case above and asserts the resulting buffer's name and
  content.
