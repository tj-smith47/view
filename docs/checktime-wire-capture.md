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
`nvim --clean --embed --listen <socket>` -- `--embed`, not `--headless`,
because view always attaches a UI (`nvim_ui_attach`) before issuing any RPC
call, and (see capture 0 below) that materially changes `:checktime`'s
blocking behavior versus a headless connection with no UI attached. The same
hermetic `HOME`/`XDG_*` isolation `EngineConfig::isolated()` uses is applied.
Every capture issues `nvim_exec_lua` running the exact chunk text
`CHECKTIME_CHUNK` embeds, with a `time.sleep(1.1)` between an initial write
and the "external" write in every case that needs two distinct disk mtimes
(coarse-grained filesystem mtime resolution can otherwise leave two writes
inside the same clock tick indistinguishable to nvim's own check).

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
CHECKTIME_CHUNK(path="case1_no_buffer.txt", force=false)
  -> { found = false }
```

Nothing else runs -- `:checktime` itself is never issued. This is the
"nothing to conflict with" case: no UI.

## 2. Loaded, UNMODIFIED buffer, a genuine external change

Buffer opened unmodified; the file is overwritten on disk from outside.

```
CHECKTIME_CHUNK(path="case2_unmodified.txt", force=false)
  -> { found = true, fired = false, modified_before = false,
       modified_after = false, lines = ["changed-externally"] }
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
CHECKTIME_CHUNK(path="case3_modified.txt", force=false)
  -> { found = true, fired = true, modified_before = true,
       modified_after = true, lines = ["originallocal-edit"] }
```

`fired = true`: `FileChangedShell` DOES trigger for a modified buffer facing
a genuine external change (the case case 0 shows would otherwise block).
The chunk's handler sets `v:fcs_choice = ''` (do nothing) because
`vim.bo[bufnr].modified` was true when it ran, so nvim neither reloads nor
touches the buffer -- `lines` is exactly the local edit, untouched, and
`modified_after` is unchanged from `modified_before`. This is the signal
`RpcCall::Checktime`'s caller uses to raise the conflict prompt: `fired = true`
(equivalently, `found && modified_after` together with `fired`, since `fired`
alone already implies the buffer was modified -- the chunk only ever reaches
the modified branch of its own handler when `vim.bo[bufnr].modified` was true
at the moment `FileChangedShell` ran).

## 4. Self-write: nvim's own `:w`, `checktime` immediately after -- a no-op

Buffer opened, then nvim itself writes it (`nvim_command("write")`, the same
mechanism `AiFsWrite`'s `silent keepalt write!` and a user's bare `:w` both
use). `:checktime` runs right after, with nothing else touching the file:

```
CHECKTIME_CHUNK(path="case4_self_write.txt", force=false)
  -> { found = true, fired = false, modified_before = false,
       modified_after = false, lines = ["original"] }
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
CHECKTIME_CHUNK(path="case5_self_write_then_edit.txt", force=false)
  -> { found = true, fired = false, modified_before = true,
       modified_after = true, lines = ["originalmore-local-edits"] }
```

`modified_before = true` (the same shape as case 3's conflict) but
`fired = false`: `FileChangedShell` never fires because the file's on-disk
mtime still matches what nvim itself last wrote -- the local edit alone
does not change what is on disk. This is the proof that `fired`, not
`modified`, is what must gate the conflict UI: a naive
`found && modified_after` check would misread this exact case as a conflict.

## 6. Buffer opened through a symlinked directory; the watcher observes the realpath'd write

Mirrors `docs/hidden-buffer-wire-capture.md`'s canon() resolution: a buffer is
opened via `<workdir>/linked_dir/target.txt` (a symlink to
`<workdir>/real_target_dir`), and the external write lands on the realpath
`<workdir>/real_target_dir/target.txt` -- the spelling a Linux fs-watcher
reports, since `notify` resolves symlinked roots to their real path.

```
CHECKTIME_CHUNK(path="<workdir>/real_target_dir/target.txt", force=false)
  -> { found = true, fired = false, modified_before = false,
       modified_after = false, lines = ["changed-through-realpath"] }
```

`found = true` despite the buffer's own name being the *unresolved* symlinked
spelling: `CHECKTIME_CHUNK`'s `canon()` is byte-identical to
`PREVIEW_CHUNK`/`LOAD_HIDDEN_CHUNK`'s own (`vim.uv.fs_realpath(p) or
vim.fn.fnamemodify(p, ':p')`), so both the buffer's name and the watcher's
realpath'd event path resolve to the same key before comparison.

## 7. `force = true`: driving the user's "reload, discarding local edits" answer

Naive idea (rejected): call `CHECKTIME_CHUNK` a second time with
`vim.bo[bufnr].modified` cleared first, hoping a second `:checktime` reloads
it the way case 2 did unprompted. Captured and found NOT to work:

```
probe (force=false)        -> { fired = true, modified = true,  lines = ["originallocal-edit"] }
"clear modified, checktime again" -> { fired = false, modified = false, lines = ["originallocal-edit"] }
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
CHECKTIME_CHUNK(path="force_discard2.txt", force=true)  -- after case-3-shape conflict
  -> { found = true, fired = true, forced = true, ok = true,
       modified = false, lines = ["changed-externally"] }
```

Also verified safe as a no-op-equivalent when nothing external actually
changed (`force = true` on an untouched, unmodified buffer just re-reads its
own unchanged content, `modified` stays `false`):

```
CHECKTIME_CHUNK(path="force_noop.txt", force=true)
  -> { found = true, forced = true, ok = true, modified = false,
       lines = ["steady"] }
```

## Production chunk shape

```lua
local path, force = ...
local function canon(p)
  if p == '' then return p end
  return vim.uv.fs_realpath(p) or vim.fn.fnamemodify(p, ':p')
end
local wanted = canon(path)
local bufnr = nil
for _, b in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_is_loaded(b) and canon(vim.api.nvim_buf_get_name(b)) == wanted then
    bufnr = b
    break
  end
end
if bufnr == nil then
  return { found = false }
end
if force then
  local ok = pcall(vim.api.nvim_buf_call, bufnr, function() vim.cmd('edit!') end)
  return { found = true, fired = true, modified = vim.bo[bufnr].modified }
end
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
return { found = true, fired = fired, modified = vim.bo[bufnr].modified }
```

`found`/`fired`/`modified` is the reply `RpcCall::Checktime`'s caller decodes
into `Msg::CheckTimeReply`. The three-way branch the brief's falsifiable
check names:

| `found` | `fired` | outcome |
|---|---|---|
| `false` | -- | no buffer loaded; no UI (case 1) |
| `true` | `false` | silently handled by nvim itself -- a real unmodified reload (case 2/6) or a self-write no-op (case 4/5); no UI either way |
| `true` | `true` | genuine conflict: buffer has local edits nvim did not touch; conflict prompt |

`force = true` is issued only in answer to the user's own "reload, discard
local edits" choice on an already-open conflict prompt, never as part of the
watcher's own probe.
