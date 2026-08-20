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

## 7e. `gone = true`: the file the reload would have read is not there

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

A file removed between the stat and the `:edit!` is not covered by the stat
-- it is the same race as before this case existed, narrowed to that window,
and nvim's own `:edit!` has no atomic alternative to offer.

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
  elseif force then
    if vim.uv.fs_stat(canonical) == nil then
      results[i] = { found = true, forced = true, gone = true }
    else
      local ok = pcall(vim.api.nvim_buf_call, bufnr, function() vim.cmd('edit!') end)
      results[i] = { found = true, forced = true, ok = ok, modified = vim.bo[bufnr].modified }
    end
  else
    local fired = false
    local group = vim.api.nvim_create_augroup('view_checktime_probe', { clear = true })
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
    vim.cmd('checktime ' .. bufnr)
    pcall(vim.api.nvim_del_augroup_by_id, group)
    results[i] = { found = true, fired = fired, modified = vim.bo[bufnr].modified }
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
| `found = true, forced = true, gone = true` | `FileGone` | a notice: the file is gone, nothing was reloaded, the buffer is the only copy left (case 7e) |

`force = true` is issued only in answer to the user's own "reload, discard
local edits" choice on an already-open conflict prompt, never as part of the
watcher's own probe -- so a forced call always carries exactly one path.
