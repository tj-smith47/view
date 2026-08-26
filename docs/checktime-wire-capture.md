# Wire capture: driving `:checktime` for out-of-band write detection

Captured live against the pinned engine per "capture, never recall." Source
of truth for `CHECKTIME_CHUNK`, the `nvim_exec_lua` chunk `RpcCall::Checktime`
issues to drive nvim's own out-of-band-write decision (spec §10.1).

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
`nvim --clean --embed` -- `--embed`, not `--headless`, because view always
attaches a UI (`nvim_ui_attach`) before issuing any RPC call, and (see
capture 0 below) that materially changes `:checktime`'s blocking behavior
versus a headless connection with no UI attached. The same hermetic
`HOME`/`XDG_*` isolation `EngineConfig::isolated()` uses is applied. Every
capture issues `nvim_exec_lua` running the exact chunk text
`CHECKTIME_CHUNK` embeds, with a `time.sleep(1.1)` between an initial write
and the "external" write in every case that needs two distinct disk mtimes
(coarse-grained filesystem mtime resolution can otherwise leave two writes
inside the same clock tick indistinguishable to nvim's own check).

The chunk takes a LIST of paths and answers a `results` array in the same
order (see capture 8 for why): every call resolves the loaded-buffer set
once, so a burst of writes costs nvim's main loop one buffer scan rather
than one per path.

## 0. Without a `FileChangedShell` handler, `:checktime` on a modified buffer BLOCKS the connection when a UI is attached

This is the finding that makes the whole design non-optional. Buffer loaded,
edited (modified), file changed on disk, then a bare `nvim_command("checktime")`
issued with no `FileChangedShell` autocmd registered at all:

```
--headless (no UI attached):
  nvim_command("checktime") -> returns immediately, err=None result=None

--embed + nvim_ui_attach (view's actual runtime shape):
  nvim_command("checktime") -> TIMED OUT (client gave up after 5s)
  nvim_eval("1+1") issued right after -> still answers (result=2), once the
    client's own timeout freed the socket for another request to queue behind it
```

Under `--headless` nvim has nowhere to route the W12 "file changed and buffer
changed" prompt, so it silently continues. Under an attached UI (view's real
shape) the prompt routes through the UI's own message/cmdline channel and
blocks the single-threaded RPC dispatch waiting for an answer nothing sends --
exactly the stall `RpcCall::Checktime` must never risk, since a stalled nvim
stalls every other in-flight RPC call on the same connection, not just this
one. `CHECKTIME_CHUNK` therefore ALWAYS registers a scoped, one-shot
`FileChangedShell` autocmd for the target buffer and always sets `v:fcs_choice`
from inside it, for the lifetime of one `:checktime` call, before ever issuing
the command -- there is no path through the chunk that calls `:checktime`
unguarded.

## 1. No loaded buffer for the changed path

```
CHUNK(paths={"case1_no_buffer.txt"}, force=false)
  -> { results = { { found = false } } }
```

Nothing else runs -- `:checktime` itself is never issued. This is the
"nothing to conflict with" case: no UI.

## 2. Loaded, UNMODIFIED buffer, a genuine external change

Buffer opened unmodified; the file is overwritten on disk from outside.

```
CHUNK(paths={"case2_unmodified.txt"}, force=false)
  -> { results = { { found = true, fired = false, modified = false } } }
  lines after -> ["changed-externally"]
```

`fired = false` but the buffer's own text visibly changed to the new disk
content: `:checktime` on an unmodified buffer with a genuine external change
reloads it SILENTLY, without ever invoking `FileChangedShell` -- regardless of
the `'autoread'` option (default off in `--clean`; not set anywhere in this
capture). This is nvim's own unconditional behavior for the unmodified case,
not something `'autoread'` gates. Confirms the "silent reload" leg of the
brief's falsifiable check needs no extra logic beyond calling `:checktime`.

## 3. Loaded, MODIFIED buffer, a genuine external change -- the conflict case

Buffer opened, edited (`modified = true`), then the file changes on disk.

```
CHUNK(paths={"case3_modified.txt"}, force=false)
  -> { results = { { found = true, fired = true, modified = true } } }
  lines after -> ["originallocal-edit"]
```

`fired = true`: `FileChangedShell` DOES trigger for a modified buffer facing
a genuine external change (the case case 0 shows would otherwise block).
The chunk's handler sets `v:fcs_choice = ''` (do nothing) because
`vim.bo[bufnr].modified` was true when it ran, so nvim neither reloads nor
touches the buffer -- `lines` is exactly the local edit, untouched. This is
the signal `RpcCall::Checktime`'s caller decodes as `CheckTimeOutcome::Conflict`
and raises the conflict prompt for: `fired` alone already implies the buffer
was modified, since the chunk only ever reaches the modified branch of its own
handler when `vim.bo[bufnr].modified` was true at the moment `FileChangedShell`
ran.

## 4. Self-write: nvim's own `:w`, `checktime` immediately after -- a no-op

Buffer opened, then nvim itself writes it (`nvim_buf_call` + `:write`, the
same mechanism `AiFsWrite`'s `silent keepalt write!` and a user's bare `:w`
both use). `:checktime` runs right after, with nothing else touching the file:

```
CHUNK(paths={"case4_self_write.txt"}, force=false)
  -> { results = { { found = true, fired = false, modified = false } } }
```

`fired = false` and the content is untouched: nvim's own `:checktime`
compares the file's current mtime against the mtime IT recorded at its own
last write, not merely "did the buffer's file change since it was
*loaded*" -- since nothing has touched the file since nvim's own write,
there is genuinely nothing to notice. This is nvim's *own* mtime bookkeeping
doing the self-write suppression, not anything `CHECKTIME_CHUNK` computes
itself.

## 5. Self-write, THEN a local edit -- still a no-op, no false conflict

Same as case 4, but after nvim's own write the buffer is edited again before
`:checktime` runs (the race the disconfirm test targets: an agent's routed
write, immediately followed by unrelated typing, must not read as an
external conflict).

```
CHUNK(paths={"case5_self_write_then_edit.txt"}, force=false)
  -> { results = { { found = true, fired = false, modified = true } } }
```

`modified = true` (the same shape as case 3's conflict) but `fired = false`:
`FileChangedShell` never fires because the file's on-disk mtime still matches
what nvim itself last wrote -- the local edit alone does not change what is on
disk. This is the proof that `fired`, not `modified`, is what must gate the
conflict UI: a naive `found && modified` check would misread this exact case
as a conflict.

## 6. Buffer opened through a symlinked directory; the watcher observes the realpath'd write

Mirrors `docs/hidden-buffer-wire-capture.md`'s canon() resolution: a buffer is
opened via `<workdir>/linked_dir/target.txt` (a symlink to
`<workdir>/real_target_dir`), and the external write lands on the realpath
`<workdir>/real_target_dir/target.txt` -- the spelling a Linux fs-watcher
reports, since `notify` resolves symlinked roots to their real path.

```
CHUNK(paths={"<workdir>/real_target_dir/target.txt"}, force=false)
  -> { results = { { found = true, fired = false, modified = false } } }
  lines after -> ["changed-through-realpath"]
```

`found = true` despite the buffer's own name being the *unresolved* symlinked
spelling: `CHECKTIME_CHUNK`'s `canon()` is byte-identical to
`PREVIEW_CHUNK`/`LOAD_HIDDEN_CHUNK`'s own (`vim.uv.fs_realpath(p) or
vim.fn.fnamemodify(p, ':p')`), so both the buffer's name and the watcher's
realpath'd event path resolve to the same key before comparison.

## 7. `force = true`: driving the user's "reload, discarding local edits" answer

Naive idea (rejected): call the chunk a second time with
`vim.bo[bufnr].modified` cleared first, hoping a second `:checktime` reloads
it the way case 2 did unprompted. Captured and found NOT to work:

```
CHUNK(paths={"force_reject.txt"}, force=false)
  -> { results = { { found = true, fired = true, modified = true } } }
"clear modified, checktime again"
  -> { results = { { found = true, fired = false, modified = false } } }
  lines after -> ["originallocal-edit"]
```

The second call's `fired = false` and the lines are UNCHANGED -- nvim's own
`:checktime` already "noticed and dispositioned" that mtime on the first
call (case 3's `FileChangedShell` firing), so a second `:checktime` against
the same still-current mtime is itself a no-op, regardless of the buffer's
modified flag. `:checktime` cannot be re-driven to force a reload of a
disposition it already made.

The working mechanism, verified instead: `force = true` skips the
`:checktime`/`FileChangedShell` dance entirely and issues an explicit
`nvim_buf_call(bufnr, function() vim.cmd('edit!') end)`, which re-reads the
file unconditionally regardless of any prior checktime state:

```
CHUNK(paths={"force_discard.txt"}, force=false)  -- establish the conflict
  -> { results = { { found = true, fired = true, modified = true } } }
CHUNK(paths={"force_discard.txt"}, force=true)
  -> { results = { { found = true, forced = true, ok = true, modified = false } } }
  lines after -> ["changed-externally"]
```

`forced = true` is what makes a forced reply structurally distinguishable
from a probe reply on the wire itself: the force branch never reports
`fired`, so nothing downstream can read the reload the user just asked for as
a fresh conflict to prompt about again.

Also verified safe as a no-op-equivalent when nothing external actually
changed (`force = true` on an untouched, unmodified buffer just re-reads its
own unchanged content, `modified` stays `false`), and as a `found = false`
when the buffer went away between the prompt and the answer:

```
CHUNK(paths={"force_noop.txt"}, force=true)
  -> { results = { { found = true, forced = true, ok = true, modified = false } } }
  lines after -> ["steady"]
CHUNK(paths={"force_no_buffer.txt"}, force=true)
  -> { results = { { found = false } } }
```

## 7a. `ok = false`: the forced reload that did not complete

`pcall`'s own result, carried rather than discarded. Captured by registering
a `BufReadPost` autocmd on the target buffer that raises, then forcing:

```
CHUNK(paths={"force_fails.txt"}, force=true) with a raising BufReadPost autocmd
  -> { results = { { found = true, forced = true, ok = false, modified = false } } }
```

The user's answer to the conflict prompt is destructive ("discard my local
edits"), so a forced reload that raised must be reported rather than read as
a completed discard. `ok` is what `CheckTimeOutcome::ReloadFailed` decodes
from.

`ok = false` does not say which content the buffer is left holding, and no
captured shape leaves the user's local edit. In this capture the
`BufReadPost` autocmd raises *after* `:edit!` has already read the file, so
the buffer holds the external content:

```
  lines after -> ["changed-externally"]
```

`:edit!` clears the buffer before it reads, so a raise earlier in the
re-read leaves an empty buffer rather than the local edit. Every shape
answers `ok = false` with `modified = false`, so nothing on the wire
separates them -- which is why the notice `update/watch.rs` records tells
the user to check the buffer rather than claiming any side survived.

## 7e. `gone = true`: the path is not a readable file the reload could read

`:edit!` against a missing path is a *success* in nvim -- it opens a new,
empty file -- so a forced reload of a deleted file answers `ok = true` and
leaves the buffer empty, with nothing on the wire to say anything went
wrong. Captured against the branch as it stood before this case existed:

```
PREVIOUS CHUNK(paths={"force_deleted_old.txt"}, force=true)
  -> { results = { { found = true, forced = true, ok = true, modified = false } } }
  lines after -> [""]
```

That is silent data loss on the ordinary path: an agent removes a file the
user has unsaved edits in, `FileChangedShell` raises the conflict prompt,
the user answers "reload", and the buffer is emptied with no notice and one
`:w` away from recreating the file empty.

The force branch therefore stats before it reloads, and does not reload at
all when the file is gone:

```
CHUNK(paths={"force_deleted.txt"}, force=false)  -- the prompt the user answers
  -> { results = { { found = true, fired = true, modified = true } } }
CHUNK(paths={"force_deleted.txt"}, force=true)
  -> { results = { { found = true, forced = true, gone = true } } }
  lines after    -> ["local-edit"]
  modified after -> true
  file recreated -> false
```

The buffer keeps the user's edits, the file is not recreated, and
`gone = true` decodes to `CheckTimeOutcome::FileGone`, which owes the user a
notice rather than `Reloaded`'s silence. The `gone` branch answers neither
`ok` nor `modified`: nothing ran, so there is nothing to report about it.

### Existing is not the same as readable

"Is anything here" is the wrong question. Every kind of path a file can be
replaced by, driven through the chunk on a buffer holding a local edit:

| the path becomes | probe, before the hoist | forced | buffer after |
|---|---|---|---|
| deleted | `fired = true` | `gone = true` | `["local-edit"]` |
| a dangling symlink | `fired = true` | `gone = true` | `["local-edit"]` |
| a directory | `fired = false` | `gone = true` | `["local-edit"]` |
| a FIFO | `fired = true` | `gone = true` | `["local-edit"]` |
| a symlink to a file | `fired = true` | `ok = true, modified = false` | `["changed-externally"]` |
| rewritten in place | `fired = true` | `ok = true, modified = false` | `["changed-externally"]` |

The probe column is what the branch answered while the guard lived inside
the force arm; case 10 is where it went and why. Every buffer here carries a
local edit, which is the only reason the probe column reads back at all
rather than blocking -- see case 10a.

`fs_stat` follows symlinks, so `st.type == 'file'` keeps every ordinary
reload -- direct or through a symlink -- on the reloading path, and takes
every other shape off it with one predicate.

The FIFO row is why the predicate is `type ~= 'file'` and not an existence
check. `:edit!` on a named pipe blocks reading it and never returns, inside
`nvim_exec_lua`, on nvim's single-threaded main loop -- the whole connection
with it. Driven against the previous existence-only guard, under a bounded
harness that SIGKILLs the child rather than letting it wedge anything:

```
PREVIOUS CHUNK(paths={"force_fifo_old.txt"}, force=true)
  -> err='TIMEOUT' res=None after 15.0s
  connection answering afterwards? ('TIMEOUT', None)
```

The follow-up `nvim_get_mode` timing out too is the point: it is not one
slow call, it is the editor. This is case 0's hazard class -- an operation
that blocks the connection -- reachable from a prompt the user is invited to
answer, since `rm f && mkfifo f` raises `FileChangedShell` on the modified
buffer (`fired = true`, above).

An unreadable regular file (mode `000`) is not in the table: this host runs
as root, where the mode bits do not apply. It stays on the reloading path by
design -- it is a file, `:edit!` fails on it rather than blocking, and
`ok = false` is the honest answer for a re-read that was refused.

### The stat is taken twice

A path that stops being a readable file between the first stat and the
`:edit!` cannot be caught before the fact -- nvim offers nothing atomic
here. What happens next depends on what the path became, and only one of the
two shapes is nvim's problem. Driven by a `BufReadPre` autocmd, which runs
inside exactly that window:

```
unlink(p) at BufReadPre
  -> pcall_ok = false
     err = 'Vim(edit):E200: *ReadPre autocommands made the file unreadable'

unlink(p) + mkdir(p) at BufReadPre
  -> pcall_ok  = true
     err       = nil
     modified  = false
     lines     = { '" Netrw Directory Listing  (netrw v184)', ... }
```

A path that merely vanishes is caught by nvim itself. A path that becomes a
directory is one `:edit!` is glad to open: it succeeds, replaces the user's
unsaved buffer with a netrw listing, and clears `modified` -- so `pcall`
alone answers `ok = true`, and the buffer the user is about to lose reads
back as the discard they asked for, reported with silence.

The second `fs_stat`, after the reload, is what closes that: `ok` is `false`
unless the reload ran *and* the path is still a regular file, which turns the
raced case into "the reload did not finish, check the buffer" -- true, and
the only honest thing left to say. It is deliberately not folded into `gone`,
whose own notice promises the buffer still holds the user's edits: by then
`:edit!` has already run, and that promise would be the lie this case makes.

## 8. One batched call over several paths at once

The reason the chunk takes a list: each call resolves every loaded buffer's
name through `vim.uv.fs_realpath` ONCE and then answers each requested path
from that map, so a burst of external writes costs nvim's single-threaded
main loop one buffer scan instead of one per path. Three paths, one call, one
scan -- a missing buffer, a silent reload, and a conflict, each dispositioned
independently:

```
CHUNK(paths={"batch_a_no_buffer.txt", "batch_b_unmodified.txt", "batch_c_modified.txt"},
      force=false)
  -> { results = { { found = false },
                   { found = true, fired = false, modified = false },
                   { found = true, fired = true,  modified = true } } }
  batch_b lines after -> ["changed-externally"]
  batch_c lines after -> ["local-edit"]
```

`results` is positional: entry `i` answers `paths[i]`, which is what lets the
reply carry no path strings of its own (the waiter already holds the list it
sent, the same way `Waiter::Preview` holds its own `path`).

## 9. An empty path list

```
CHUNK(paths={}, force=false)
  -> { results = {} }
```

Answers rather than errors, so a caller never has to special-case it.

## 10. The probe reaches the same unreadable paths, with no user in the loop

Case 7e's guard sat inside the force branch. The probe branch reaches every
path in that table too -- the watcher issues it automatically on any
create/modify event under a watched root -- and reaches the two worst rows
on terms the forced call never does.

### 10a. An unmodified buffer, path replaced by a pipe: `:checktime` itself blocks

`:checktime` on an unmodified buffer does the re-read *itself*, without ever
consulting `FileChangedShell`. Against a pipe it blocks on the open, inside
`nvim_exec_lua`, on nvim's single-threaded main loop. Driven against the
branch as it stood, under the same bounded harness case 7e's FIFO row used:

```
PREVIOUS CHUNK(paths={a_fifo.txt}, force=false)
  -> err='TIMEOUT' result=None after 15.0s
  connection answering afterwards? nvim_get_mode -> 'TIMEOUT'
```

The follow-up call timing out too is the point, exactly as in case 7e: it is
not one slow call, it is the editor. The chunk's own `FileChangedShell`
handler cannot save it -- control never reaches the callback, because
`:checktime` blocks before raising the autocmd.

A *modified* buffer is safe here: the handler sets `v:fcs_choice = ''`,
which short-circuits nvim's read before it opens anything. That is why the
forced-branch FIFO row -- which layers a local edit on the buffer first --
answers `fired = true` rather than hanging, and why an unmodified buffer is
the reachable shape. It is also the common one: a user is not editing every
file that is open.

### 10b. An unmodified buffer, path replaced by a socket or a device: `E321` aborts the whole chunk

The same read, refused rather than blocked. `:checktime` raises, and an
unprotected `vim.cmd` takes the entire Lua chunk down with it:

```
PREVIOUS CHUNK(paths={b_socket.txt}, force=false)
  -> err=[0, 'Lua: ... Vim(checktime):E321: Could not reload ".../b_socket.txt"']
     result=None
PREVIOUS CHUNK(paths={b_chardev.txt}, force=false)
  -> err=[0, 'Lua: ... Vim(checktime):E321: Could not reload ".../b_chardev.txt"']
     result=None
```

Two things are lost with it. The augroup cleanup on the next line never
runs, so the one-shot `FileChangedShell` autocmd stays armed on that buffer
and will set `fcs_choice = 'reload'` on some later, unrelated change:

```
  augroup 'view_checktime_probe' after the raise -> 1 autocmd still registered
```

And because the chunk is batched (case 8), one odd path costs every sibling
in the same call its answer -- including a genuine conflict, which the
caller's own error handling degrades to `NoBuffer`, so the user never sees
the prompt:

```
PREVIOUS CHUNK(paths={c_conflict.txt, c_socket.txt}, force=false)
  -> err=[0, 'Lua: ... Vim(checktime):E321: Could not reload ".../c_socket.txt"']
     result=None            # c_conflict.txt's own `fired = true` never arrives
```

### 10c. The stat hoisted above the split, and the probe's own `pcall`

One predicate over both branches -- taken inside the arm that found a
buffer, so a path with nothing loaded for it still costs no syscall -- plus
a `pcall` around the probe's `:checktime`, which catches raises and only
raises (case 10d). The same batch, answered:

```
CHUNK(paths={c_conflict.txt, c_socket.txt}, force=false)
  -> { results = { { found = true, fired = true, modified = true },
                   { found = true, gone = true, modified = false } } }
  augroup 'view_checktime_probe' after -> no such group
```

The augroup is answerable only from a path whose autocmd never fired: a
one-shot that fired has already deleted itself, and reads back clean whether
the cleanup ran or not. A batch ending on a path nothing touched is what
makes the cleanup observable, since the group is re-created with
`clear = true` per path and only the last one's autocmd can survive the
loop:

```
CHUNK(paths={k_conflict.txt, k_socket.txt, k_device.txt, k_quiet.txt}, force=false)
  -> { results = { { found = true, fired = true,  modified = true },
                   { found = true, gone = true,   modified = false },
                   { found = true, gone = true,   modified = false },
                   { found = true, fired = false, modified = false } } }
  augroup 'view_checktime_probe' after -> no such group
```

Every shape a file can be replaced by, probed with the buffer unmodified and
again with a local edit on it. `lines after` is the buffer's own content
once the probe returned:

| the path becomes | probe, unmodified buffer | lines after | probe, modified buffer | lines after |
|---|---|---|---|---|
| rewritten in place | `fired = false, modified = false` | `["changed-externally"]` | `fired = true, modified = true` | `["originallocal-edit"]` |
| a symlink to a file | `fired = false, modified = false` | `["changed-externally"]` | `fired = true, modified = true` | `["originallocal-edit"]` |
| deleted | `gone = true, modified = false` | `["original"]` | `gone = true, modified = true` | `["originallocal-edit"]` |
| a dangling symlink | `gone = true, modified = false` | `["original"]` | `gone = true, modified = true` | `["originallocal-edit"]` |
| a directory | `gone = true, modified = false` | `["original"]` | `gone = true, modified = true` | `["originallocal-edit"]` |
| a FIFO | `gone = true, modified = false` | `["original"]` | `gone = true, modified = true` | `["originallocal-edit"]` |
| a char device | `gone = true, modified = false` | `["original"]` | `gone = true, modified = true` | `["originallocal-edit"]` |
| a socket | `gone = true, modified = false` | `["original"]` | `gone = true, modified = true` | `["originallocal-edit"]` |

The rows that used to answer `fired = true` -- deleted, dangling, FIFO --
answered a conflict prompt whose only offer was a reload that could never
happen. `gone` says so directly instead, and the directory row, which used
to answer `fired = false` and say nothing at all, now says it too.

The forced call over the identical shapes, unchanged by the hoist except
that the entries no longer carry `forced`:

| the path becomes | forced | lines after |
|---|---|---|
| rewritten in place | `forced = true, ok = true, modified = false` | `["changed-externally"]` |
| a symlink to a file | `forced = true, ok = true, modified = false` | `["changed-externally"]` |
| deleted | `gone = true, modified = true` | `["originallocal-edit"]` |
| a dangling symlink | `gone = true, modified = true` | `["originallocal-edit"]` |
| a directory | `gone = true, modified = true` | `["originallocal-edit"]` |
| a FIFO | `gone = true, modified = true` | `["originallocal-edit"]` |
| a char device | `gone = true, modified = true` | `["originallocal-edit"]` |
| a socket | `gone = true, modified = true` | `["originallocal-edit"]` |

`modified` is what the two `gone` shapes differ by, and it is the whole
reason the entry carries it: a modified buffer is holding edits that exist
nowhere else, an unmodified one is holding what it last read. Only one of
those two sentences is ever true, and `update/watch.rs` picks by this flag.

The ordinary rows are unmoved by the hoist -- the stat is taken, finds a
regular file, and every branch runs exactly as cases 1, 2, 3 and 8 captured:

```
CHUNK(paths={g_nobuffer, g_unmodified, g_modified}, force=false)
  -> { results = { { found = false },
                   { found = true, fired = false, modified = false },
                   { found = true, fired = true,  modified = true } } }
  g_unmodified lines after -> ["changed-externally"]
  g_modified   lines after -> ["originallocal-edit"]
CHUNK(paths={}, force=false)
  -> { results = {} }
```

### 10d. The one unreadable shape no stat can see, and what the `pcall` is for

`st.type == 'file'` asks what kind of path it is, and a regular file the
process may not *open* answers "a file". `:checktime` on an unmodified
buffer performs the re-read itself, the open is refused, and nvim raises
`E321` out of the command -- past the stat, which had nothing wrong to
report.

Driven with the fixture made unreadable by mode bits, against a child
dropped to an unprivileged uid (`setpriv --reuid=65534 --regid=65534
--clear-groups nvim --clean --embed`), because mode bits are advisory to a
privileged process and this host runs as root. Two paths in the call: the
unreadable one, then one nothing touched.

```
$ timeout -s KILL 90 python3 t_perm.py
uid seen by nvim:  65534
stat type:         file
nvim can open?     EACCES: permission denied: .../unreadable.txt
CHUNK(paths={unreadable.txt, quiet.txt}, force=false)
  -> { results = { { found = true, fired = false, modified = false },
                   { found = true, fired = false, modified = false } } }
  connection alive?  { mode = 'n', blocking = false }
  augroup after   -> 0
```

The same fixture with the `pcall` alone removed -- the stat guard left
exactly as it ships:

```
$ timeout -s KILL 90 python3 t_perm.py nopcall
CHUNK err=[0, 'Lua: ... Vim(checktime):E321: Could not reload ".../unreadable.txt"']
     result=None
  augroup after -> 1
```

Both losses of case 10b, reproduced from a path the stat cannot reject: the
whole batch's answer, and the one-shot autocmd still armed. `HandledSilently`
is the honest reading of the caught raise -- nothing was read, and nothing
about the buffer changed for the user to be told about.

### 10e. What the `pcall` does not catch

The window between the stat and the command has a blocking half as well as a
raising one, and `pcall` answers only the second. Driven deterministically
with a `BufReadPre` autocmd that swaps a pipe in after the stat, while
`:checktime` is already reading:

```
$ timeout -s KILL 120 python3 t_fiforace.py
CHUNK err='TIMEOUT' res=None after 15.0s
connection alive? nvim_get_mode -> 'TIMEOUT'
$ pgrep -c nvim
0
```

Reaching it means winning microseconds rather than replacing a file, and
nothing on this side can close it -- nvim offers nothing atomic between the
stat and the command. What notices it is the heartbeat: its own
`nvim_get_mode` probe stops being answered exactly as the follow-up call
above does, and `HEARTBEAT_WEDGE_THRESHOLD` (10s) turns that silence into a
verdict, so a wedged engine surfaces rather than hanging quietly.

## 11. What each save shape raises at the kernel level

Not a `nvim_exec_lua` capture: this one is the layer *below* the chunk, the
filesystem events `view-ai`'s watch nominates from. It is here because the
question "does forwarding removals make view cry wolf over an ordinary save"
is answered by which events a save actually raises, and that had been
answered from memory.

Captured with `inotifywait -m -r --format '%e %f' .` over a scratch
directory holding `watched.txt`, with a `MARK-*` file touched before each
shape so the three are separable in one stream:

```
$ inotifywait -m -r --format '%e %f' . &
$ printf 'changed\n' > watched.txt.tmp && mv watched.txt.tmp watched.txt
CREATE watched.txt.tmp
OPEN watched.txt.tmp
MODIFY watched.txt.tmp
CLOSE_WRITE,CLOSE watched.txt.tmp
MOVED_FROM watched.txt.tmp
MOVED_TO watched.txt

$ rm watched.txt && printf 'changed-again\n' > watched.txt
DELETE watched.txt
CREATE watched.txt
OPEN watched.txt
MODIFY watched.txt
CLOSE_WRITE,CLOSE watched.txt

$ rm watched.txt
DELETE watched.txt
```

| save shape | raises `DELETE`? | what the watch sees |
| --- | --- | --- |
| temp file + `rename` over the target (the atomic save) | no | `MOVED_TO`, which `notify` maps to `Modify(Name(To))` -- `is_modify()`, the arm that existed before removals were forwarded |
| unlink, then write the target again | **yes**, followed by `CREATE`/`MODIFY` | `Remove(File)` on its own if the coalesce window closes between the two halves, otherwise coalesced with the create |
| plain `rm` | yes, and nothing after it | `Remove(File)` -- the shape with no create or modify to ride along with |

Two consequences the tests are built on:

- An atomic save is nominated with or without removal forwarding, so
  `an_atomic_save_over_a_watched_file_reloads_rather_than_reporting_it_gone`
  cannot be the falsifiable half of "forwarding removals does not cry wolf".
  `a_save_that_unlinks_before_rewriting_reloads_rather_than_reporting_it_gone`
  is: it takes the nomination while the path is genuinely absent, which only
  arrives when `is_remove()` is forwarded.
- The unlink-then-rewrite is the one save shape whose nomination can reach
  the probe while the path is still missing. The probe answers `gone` for it
  correctly -- the file *was* gone -- which is why that answer is not what
  reaches the user: the fold confirms it with a second probe one grace
  period later (`view_ai::FILE_GONE_GRACE`, two coalesce windows), and a
  save that finishes inside that grace is readable again by then, so nothing
  is said. A path still unreadable at the second probe is announced --
  including a build that clears its outputs and writes them seconds later,
  where the file really is gone for the whole grace -- and its notice is
  retracted by any later answer that reads the path, so a file deleted for
  real and restored minutes afterward loses its notice rather than standing
  for the full transient timeout.

  Only the reply that second probe itself provokes may announce anything.
  The probe is asked for by `Effect::ReprobeExternalWrite`, comes back as
  `Msg::ConfirmExternalRemoval` once the grace has passed, and drives a
  `checktime` of its own whose `request_id` the fold remembers; a `gone`
  answer wearing any other id is a first look like any other. That is what
  ends the episode at its answer rather than leaving a record behind: a
  record that outlived its notice would let the next unlink-then-rewrite
  save be announced on its first look, which is the flash this whole
  sequence exists to prevent.

## Production chunk shape

```lua
local paths, force = ...
local function canon(p)
  if p == '' then return p end
  return vim.uv.fs_realpath(p) or vim.fn.fnamemodify(p, ':p')
end
local loaded = {}
for _, b in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_is_loaded(b) then
    loaded[canon(vim.api.nvim_buf_get_name(b))] = b
  end
end
local results = {}
for i, path in ipairs(paths) do
  local canonical = canon(path)
  local bufnr = loaded[canonical]
  if bufnr == nil then
    results[i] = { found = false }
  else
    local st = vim.uv.fs_stat(canonical)
    if st == nil or st.type ~= 'file' then
      results[i] = { found = true, gone = true,
        modified = vim.bo[bufnr].modified }
    elseif force then
      local reloaded = pcall(vim.api.nvim_buf_call, bufnr,
        function() vim.cmd('edit!') end)
      local after = vim.uv.fs_stat(canonical)
      local ok = reloaded and after ~= nil and after.type == 'file'
      results[i] = { found = true, forced = true, ok = ok,
        modified = vim.bo[bufnr].modified }
    else
      local fired = false
      local group = vim.api.nvim_create_augroup('view_checktime_probe',
        { clear = true })
      vim.api.nvim_create_autocmd('FileChangedShell', {
        group = group,
        buffer = bufnr,
        once = true,
        callback = function()
          fired = true
          if vim.bo[bufnr].modified then
            vim.v.fcs_choice = ''
          else
            vim.v.fcs_choice = 'reload'
          end
        end,
      })
      local checked = pcall(vim.cmd, 'checktime ' .. bufnr)
      pcall(vim.api.nvim_del_augroup_by_id, group)
      results[i] = { found = true, fired = checked and fired,
        modified = vim.bo[bufnr].modified }
    end
  end
end
return { results = results }
```

Each `results` entry decodes into one `CheckTimeOutcome`, and the six
outcomes are the whole vocabulary -- a reply that reports both a fresh
conflict and a completed forced reload is unrepresentable rather than merely
unexpected:

| entry | outcome | UI |
|---|---|---|
| `found = false` | `NoBuffer` | nothing -- no buffer loaded (cases 1, 7) |
| `found = true, fired = false` | `HandledSilently` | nothing -- nvim's own unmodified reload (cases 2/6/8) or a self-write no-op (cases 4/5) |
| `found = true, fired = true` | `Conflict` | the conflict prompt (cases 3/8) |
| `found = true, forced = true, ok = true` | `Reloaded` | nothing -- the answer the user already gave, carried out (case 7) |
| `found = true, forced = true, ok = false` | `ReloadFailed` | a notice: the discard the user asked for did not happen (case 7a) |
| `found = true, gone = true, modified = <bool>` | `FileGone` | a notice: the path is not a readable file, so nothing was read; `modified` picks which of the two true sentences it gets (cases 7e, 10) |

`gone` carries no `forced` of its own and is decoded ahead of it: the stat
that answers it sits above the force split, so a probe and a forced call
reach it on identical terms.

`force = true` is issued only in answer to the user's own "reload, discard
local edits" choice on an already-open conflict prompt, never as part of the
watcher's own probe -- so a forced call always carries exactly one path.
`gone` is the one outcome a probe and a forced call share, which is why the
stat that decides it is taken once, above the split, rather than twice.
