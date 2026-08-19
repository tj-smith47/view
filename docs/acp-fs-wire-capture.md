# Wire capture: agent-initiated file reads and writes through nvim

Captured live against the pinned engine per "capture, never recall." Source
of truth for `AI_FS_READ_CHUNK` and `AI_FS_WRITE_CHUNK`, the two
`nvim_exec_lua` chunks `EngineHandle::ai_fs_read`/`EngineHandle::ai_fs_write`
issue for `RpcCall::AiFsRead`/`RpcCall::AiFsWrite` -- the calls that answer
an agent's `fs/read_text_file` and `fs/write_text_file`.

The path-to-buffer half of both round trips is `RpcCall::LoadHidden`'s, not
this document's: see `docs/hidden-buffer-wire-capture.md` for how a path
resolves onto an existing buffer or a freshly `bufadd`-ed hidden one, and
for the refcounted release that deletes it again. Every capture below starts
from a buffer that resolve already produced.

## Engine identity

```
$ nvim --version | head -3
NVIM v0.12.4
Build type: Release
LuaJIT 2.1.1787058514
```

Matches `.engine-pin` (`v0.12.4`).

## Capture method

A standalone Python msgpack-rpc client (no `pynvim`; not installed) spawns
`nvim --clean --headless --listen <socket>` with the same hermetic
`HOME`/`XDG_*` isolation `EngineConfig::isolated()` uses, connects over the
unix socket, and issues raw msgpack-RPC requests -- both bare API calls
(`nvim_buf_set_lines`, `nvim_buf_delete`, `nvim_get_option_value`) and
`nvim_exec_lua` running the exact chunk text each constant embeds.

## 1. A whole-file read answers with the buffer's lines and whether the file ends in a newline

Fixture `eol.txt` holds `"alpha\nbravo\ncharlie\n"`; `noeol.txt` holds the
same three lines with no trailing newline.

```
load_hidden(eol.txt)   -> { created = true, changedtick = 2, buf = 2 }
AI_FS_READ_CHUNK(2, nil, nil)
  -> { ok = true, lines = ['alpha', 'bravo', 'charlie'], eol = true }

load_hidden(noeol.txt) -> { created = true, changedtick = 2, buf = 3 }
AI_FS_READ_CHUNK(3, nil, nil)
  -> { ok = true, lines = ['alpha', 'bravo', 'charlie'], eol = false }
```

Both files produce the identical `lines` array: nvim's line list carries no
record of the final newline at all, so a client that joined `lines` with
`\n` and always appended one would hand the agent a byte the file does not
contain -- and an agent that read, edited one line, and wrote back would
silently append a newline to every file it touched. The distinguishing fact
lives in the buffer's `endofline` option, which `bufload` sets from the file
it read:

```
nvim_get_option_value('endofline', {buf = 3})  -> false    (noeol.txt)
nvim_get_option_value('endofline', {buf = 2})  -> true     (eol.txt)
```

`eol` in the reply is that option, not a re-derivation of it.

## 2. `line`/`limit` map onto `nvim_buf_get_lines`'s 0-indexed, end-exclusive window

The wire's `line` is 1-based and its `limit` is a count
(`docs/acp-v1-wire-capture.md`, `fs/read_text_file` case 1);
`nvim_buf_get_lines(buf, first, last, false)` takes a 0-indexed `first` and
an exclusive `last`. Captured against `eol.txt` (3 lines) and `noeol.txt`:

```
AI_FS_READ_CHUNK(2, 2, 1)     -> { ok = true, lines = ['bravo'],            eol = true }
AI_FS_READ_CHUNK(2, 1, 2)     -> { ok = true, lines = ['alpha', 'bravo'],   eol = true }
AI_FS_READ_CHUNK(2, 3, 99)    -> { ok = true, lines = ['charlie'],          eol = true }
AI_FS_READ_CHUNK(2, 99, nil)  -> { ok = true, lines = [],                   eol = true }
AI_FS_READ_CHUNK(2, 0, nil)   -> { ok = true, lines = ['alpha','bravo','charlie'], eol = true }
AI_FS_READ_CHUNK(2, nil, 0)   -> { ok = true, lines = [],                   eol = true }
```

Four behaviors this pins:

- A `line` past the end answers an empty window, not an error and not the
  whole file: `strict_indexing = false` is what makes an out-of-range start
  clamp instead of throwing, and an agent asking for line 99 of a 3-line
  file has asked a well-formed question whose answer is "nothing."
- A `limit` running past the end is clamped the same way (`line = 3,
  limit = 99` yields one line, not an error).
- `line = 0` -- which the schema's own `minimum: 0` admits despite the
  1-based description -- reads from the first line. The chunk's guard is
  `line > 1`, so both `0` and `1` mean "start at index 0" and neither
  under-runs into a negative index.
- `limit = 0` answers an empty window rather than being read as "no limit."
  This is the case a `limit and limit > 0` guard would get wrong: it would
  silently promote an explicit request for zero lines into a whole-file
  read.

The `eol` flag is computed against the *window*, not the file:

```
AI_FS_READ_CHUNK(3, 3, 1)     -> { ok = true, lines = ['charlie'],          eol = false }
AI_FS_READ_CHUNK(3, 1, 2)     -> { ok = true, lines = ['alpha', 'bravo'],   eol = true }
```

Both are reads of `noeol.txt`. The first window reaches the file's last
line, so the file's own missing trailing newline is what the window ends
with (`eol = false`). The second stops before it, so the window ends
mid-file, where a newline genuinely does follow (`eol = true`). A chunk that
returned `vim.bo[buf].endofline` unconditionally would strip the newline
after `bravo` in the second case.

## 3. `nil` crosses from msgpack into Lua as `vim.NIL`, not `nil`

```
nvim_exec_lua("local a = ... return { is_nil = a == nil,
                                      type_is_number = type(a) == 'number',
                                      tostring = tostring(a) }", [nil])
  -> { is_nil = false, type_is_number = false, tostring = 'vim.NIL' }
```

The same fact `BUF_SET_TEXT_CHUNK`'s `expected` guard already depends on
(`docs/buf-set-text-wire-capture.md`): an absent `line`/`limit` arrives as a
userdata sentinel that is *not* `nil`, so `if line then` and `if line ~= nil
then` both read "no window requested" as a window request. Every optional
argument in both chunks here is therefore tested with
`type(x) == 'number'`/`type(x) == 'boolean'`, never against `nil`.

## 4. Buffer truth: a modified buffer answers with its own text, never the file on disk

```
nvim_buf_set_lines(2, 0, 1, false, ['ALPHA-UNSAVED'])
AI_FS_READ_CHUNK(2, nil, nil)
  -> { ok = true, lines = ['ALPHA-UNSAVED', 'bravo', 'charlie'], eol = true }
nvim_get_option_value('modified', {buf = 2})   -> true
```

while the file on disk still holds:

```
'alpha\nbravo\ncharlie\n'
```

This is the wire's own "including unsaved changes in the editor" made
literal, and it holds for a buffer a real window is showing, not only for a
hidden one:

```
nvim_command('edit big.txt')
load_hidden(big.txt) -> { created = false, buf = 3 }      -- the window's own buffer
nvim_buf_set_lines(3, 0, 1, false, ['EDITED-IN-WINDOW'])
AI_FS_READ_CHUNK(3, 1, 1) -> { ok = true, lines = ['EDITED-IN-WINDOW'], eol = true }
  -- disk line 1 is still 'line1\n'
```

`created = false` there is what keeps the release safe: the hold's
ownership gate never authorizes deleting a buffer the user's own window
opened, and the release chunk's `win_findbuf` check refuses it a second
time.

```
release_hidden(window-held buffer) -> { deleted = false }
```

## 5. An invalid buffer handle answers a refusal, never a thrown error

A scratch buffer created and then force-deleted:

```
nvim_create_buf(false, true)          -> buf = 4
nvim_buf_delete(4, {force = true})    -> ok
AI_FS_READ_CHUNK(4, nil, nil)         -> { ok = false }
AI_FS_WRITE_CHUNK(4, nil, ['x'], true)
  -> { applied = false, saved = false, message = 'no such buffer' }
```

Both answer as data rather than as an `nvim_exec_lua` error, unlike a bare
`nvim_buf_set_text` against a deleted handle
(`docs/buf-set-text-wire-capture.md` case 4, `Invalid buffer id: 2`). The
`nvim_buf_is_valid` guard is what buys that: the reader thread still
degrades an error reply to the same refusal, but the ordinary case -- a
buffer released between this call's resolve and its apply -- reaches the
agent as a refusal it can read rather than as an opaque Lua traceback.

## 6. The write's `expected` tick refuses without touching the buffer or the file

Fixture `multibyte.txt` holds `"héllo wörld\nsecond\n"`, resolved at
`changedtick = 2`:

```
AI_FS_WRITE_CHUNK(5, 42, ['nope'], true)
  -> { applied = false, saved = false, message = 'the buffer changed' }
  -- disk unchanged: 'héllo wörld\nsecond\n'

AI_FS_WRITE_CHUNK(5, 2, ['réwritten', 'second line'], true)
  -> { applied = true, saved = true, changedtick = 4 }
nvim_buf_get_lines(5, 0, -1, false) -> ['réwritten', 'second line']
  -- disk now:  'réwritten\nsecond line\n'
nvim_get_option_value('modified', {buf = 5}) -> false
```

The guard runs before the first `nvim_buf_set_lines`, so a refused write
leaves both the buffer and the file exactly as they were -- the same
all-or-nothing contract `BUF_SET_TEXT_CHUNK` states, reached the same way
(checked inside the chunk, where the check and the apply cannot be separated
by a keystroke, rather than on the Rust side of the wire).

`modified = false` after the write is what lets the paired release delete
the hidden buffer again: nvim refuses to delete a modified buffer (case 9
below, and `docs/hidden-buffer-wire-capture.md` case 7), so the save is
also what keeps the buffer count flat across repeated agent writes.

## 7. `endofline`/`fixendofline` decide the file's trailing newline

Same buffer, two writes differing only in the `eol` argument:

```
AI_FS_WRITE_CHUNK(5, nil, ['no', 'trailing'], false)
  -> { applied = true, saved = true, changedtick = 6 }
  -- disk: 'no\ntrailing'

AI_FS_WRITE_CHUNK(5, nil, ['with', 'trailing'], true)
  -> { applied = true, saved = true, changedtick = 8 }
  -- disk: 'with\ntrailing\n'
```

Both options are set, not just `endofline`: `fixendofline` is what nvim
consults to decide whether to *add* a missing final newline on write, so
setting `endofline = false` alone still produces a trailing newline on disk.
Together they make an agent's exact `content` string round-trip -- the
`content` an agent sends is reproduced byte for byte, whether or not it ends
in `\n`.

## 8. A write creates the file, and the directory above it

`brand_new.txt` does not exist; `no_such_dir/deep.txt` has no parent
directory either.

```
load_hidden(brand_new.txt)  -> { buf = 6, created = true, changedtick = 2 }
os.path.exists(brand_new.txt) -> False
AI_FS_WRITE_CHUNK(6, 2, ['created', 'by the agent'], true)
  -> { applied = true, saved = true, changedtick = 4 }
os.path.exists(brand_new.txt) -> True, holding 'created\nby the agent\n'

load_hidden(no_such_dir/deep.txt) -> { buf = 7, created = true, changedtick = 2 }
AI_FS_WRITE_CHUNK(7, nil, ['deep'], true)
  -> { applied = true, saved = true, changedtick = 4 }
os.path.exists(no_such_dir/deep.txt) -> True, holding 'deep\n'
```

The first is the wire's own "The Client MUST create the file if it doesn't
exist," satisfied by `bufadd` + `bufload` naming a buffer for a path with no
file behind it and `:write` creating it. The second needs the chunk's
`vim.fn.mkdir(..., 'p')`: without it `:write` answers `E212: Can't open file
for writing: not a directory` and the agent's write fails for a reason it
cannot act on -- creating a file in a new directory is an ordinary thing an
agent does, and the directory is not a second decision for the user to make.

## 9. A save nvim cannot perform reports `saved = false` and loses nothing

The only unwritable target that is unwritable for a non-root process too: a
path whose parent is a regular file, which `mkdir -p` cannot fix.

```
load_hidden(blocker/child.txt) -> { buf = 2, created = true, changedtick = 1 }
AI_FS_WRITE_CHUNK(2, 1, ['content the agent asked for'], true)
  -> { applied = true, saved = false,
       message = "E212: Can't open file for writing: not a directory" }
nvim_buf_get_lines(2, 0, -1, false) -> ['content the agent asked for']
nvim_get_option_value('modified', {buf = 2}) -> true
nvim_buf_delete(2, {})  -> refused: 'Failed to unload buffer.'
```

Three things this pins. The `pcall` around the `:write` is what turns the
failure into a reply instead of an `nvim_exec_lua` error, so the agent is
told *which* failure (`E212`, with the operating system's own wording)
rather than "the request could not be answered". The buffer keeps the
content the agent asked for, so nothing the agent sent is discarded on the
way. And the buffer stays modified, which means the paired release cannot
delete it -- the one case where an agent write leaves a hidden buffer
behind, and the safe half of the trade: the alternative is deleting a buffer
holding content that reached no file.

The raw `pcall` error is a full Lua traceback
(`[string "<nvim>"]:16: Lua: [string "vim/_core/editor"]:355: nvim_exec2(),
line 1: Vim(write):E212: ... stack traceback: ...`). The chunk reduces it to
its first line and then to the `E<number>:` substring within that line, so
what crosses the wire is the diagnostic and not the call stack of the chunk
that produced it.

## 10. An agent write is one undo step, never joined onto the user's last edit

```
load_hidden(eol.txt) -> { buf = 2, created = false, changedtick = 3 }
AI_FS_WRITE_CHUNK(2, nil, ['one', 'two'], true)
nvim_buf_get_lines(2, 0, -1, false)          -> ['one', 'two']
nvim_buf_call(2, function() vim.cmd('undo') end)
nvim_buf_get_lines(2, 0, -1, false)          -> ['alpha', 'bravo', 'charlie']
```

One `u` reverts the whole agent write and nothing else. The chunk issues no
`undojoin` at all, which is the difference from `BUF_SET_TEXT_CHUNK`'s
per-hunk join (`docs/buf-set-text-wire-capture.md` case 2): a diff review's
hunks are one user decision spread over several calls and belong in one undo
entry, while an agent-initiated `fs/write_text_file` is its own event
arriving at a moment the user did not choose, and joining it onto whatever
the user last typed would make a single `u` revert both.

## 11. Why the write replaces lines rather than issuing a `BufSetText` edit

`RpcCall::BufSetText` addresses an edit by 0-indexed byte columns
(`docs/buf-set-text-wire-capture.md` cases 1 and 9). Expressing "replace the
whole buffer" in that form requires the byte length of the buffer's current
last line, which the client does not have: `Msg::HiddenBufferLoaded` carries
a buffer handle and a `changedtick`, not the text. Reaching for it would
cost an extra read round trip whose answer could be stale by the time the
write ran -- the exact race the `expected` tick exists to close, reopened in
order to satisfy an API shape.

`nvim_buf_set_lines(buf, 0, -1, false, lines)` is nvim's own whole-buffer
replacement and needs no column arithmetic at all. It shares everything the
column form was wanted for: the same `changedtick` guard in the same chunk,
the same single undo entry, the same `b:changedtick` bump reported back.
What it does not share is `BufSetText`'s bottom-to-top ordering (case 8),
which exists for multi-hunk batches and has no meaning for a single edit
spanning the whole buffer.
