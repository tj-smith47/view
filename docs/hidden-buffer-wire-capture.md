# Wire capture: hidden-buffer creation, reuse, and deletion

Captured live against the pinned engine per "capture, never recall." Source
of truth for `LOAD_HIDDEN_CHUNK`, the `nvim_exec_lua` chunk
`EngineHandle::load_hidden` issues for `RpcCall::LoadHidden`, and for
`EngineHandle::release_hidden`'s `nvim_buf_delete` call.

## Engine identity

```
$ nvim --version | head -3
NVIM v0.12.4
Build type: Release
LuaJIT 2.1.1785763465
```

Matches `.engine-pin` (`v0.12.4`).

## Capture method

A standalone Python msgpack-rpc client (no `pynvim`; not installed) spawns
`nvim --clean --headless --listen <socket>` with the same hermetic
`HOME`/`XDG_*` isolation `EngineConfig::isolated()` uses, connects over the
unix socket, and issues raw msgpack-RPC requests -- both bare API calls
(`nvim_create_buf`, `nvim_buf_delete`) and `nvim_exec_lua` running the exact
chunk text `LOAD_HIDDEN_CHUNK` embeds.

## 1. `nvim_create_buf(false, false)` never lists the buffer

Superseded by case 10: the shipped `LOAD_HIDDEN_CHUNK` no longer calls
`nvim_create_buf` at all. Kept for the unlisted-by-default fact this case
still establishes, which case 10 confirms `bufadd` shares.

```
nvim_create_buf(false, false) -> buf=2
nvim_get_option_value('buflisted', {buf=2}) -> false
```

A buflisted-filtered scan (the same filter `BUFFER_LIST_CHUNK` uses for the
picker's `Source::Buffers`) over `nvim_list_bufs()` right after creating and
naming the buffer never includes it -- only nvim's own default buffer (`1`)
shows up. Confirms the falsifiable check: a hidden buffer never reaches
`Msg::PickerBufferList`.

## 2. `nvim_buf_set_lines` marks the buffer modified -- `bufload` does not

Superseded by case 10: the shipped chunk populates through `bufload`, which
never marks the buffer modified in the first place, so the reset this case
describes no longer exists in the code. Kept as the reason `bufload`
replaced this mechanism at all.

Loading a file's content into a freshly created buffer via
`vim.api.nvim_buf_set_lines` (the only way to populate a buffer whose
content this chunk itself reads through `vim.fn.readfile`, since
`nvim_create_buf` starts the buffer empty) sets `modified = true`, unlike
`vim.fn.bufload`, which does not. The
chunk resets `vim.bo[buf].modified = false` immediately after the initial
load specifically to undo this side effect: a buffer that merely mirrors
what is already on disk must not read as having unsaved changes nobody
made.

## 3. The existing-buffer lookup, scanned before `nvim_create_buf`

Superseded by case 10 for the creation mechanism (`bufadd`, not
`nvim_create_buf`) and by case 13 for the idempotency claim (`bufadd` itself
is idempotent by name; the scan below no longer needs to lean on
`nvim_buf_set_name` for it). Kept for the "scan before creating" ordering,
which the shipped chunk still preserves.

Reusing `PREVIEW_CHUNK`'s own canonicalized name-match scan over
`nvim_list_bufs()` (symlink-safe, `loaded buffer wins over disk`):

```
-- first call, path has no buffer yet
LOAD_HIDDEN_CHUNK(path) -> { buf = 2, created = true,  changedtick = 2 }

-- second call, same path
LOAD_HIDDEN_CHUNK(path) -> { buf = 2, created = false, changedtick = 2 }
```

The second call's scan finds the buffer the first call named via
`nvim_buf_set_name` and returns it unchanged -- no second `nvim_create_buf`,
no second read of the file. `created` tells only which call made the
buffer; both calls return the identical handle.

## 4. A path with no file on disk yet resolves to an empty, unmodified buffer

Superseded by case 10: the shipped chunk reaches this outcome through
`bufadd` resolving the nonexistent path directly, not through a
`vim.fn.readfile` call caught by `pcall` -- that call no longer exists in
the chunk at all. Kept for the outcome itself (empty, unmodified buffer for
a not-yet-existing path), which still holds under `bufadd`+`bufload`.

```
LOAD_HIDDEN_CHUNK(new_file_path) -> { buf = 3, created = true, changedtick = 2 }
nvim_buf_get_lines(3, 0, -1, false) -> ['']
nvim_get_option_value('modified', {buf=3}) -> false
```

`vim.fn.readfile` on a nonexistent path fails; the chunk's `pcall` catches
that and falls back to an empty `lines` table, which is what the file will
be created as once something writes to it -- the new-file proposal's own
case.

## 5. The existing-buffer lookup finds a buffer regardless of its modified state

```
nvim_buf_set_lines(2, 0, 1, false, ['EDITED'])   -- simulates an accepted hunk write
nvim_get_option_value('modified', {buf=2}) -> true
LOAD_HIDDEN_CHUNK(path) -> { buf = 2, created = false, changedtick = 3 }
```

A second `load_hidden` for the same path after edits have already landed in
the buffer still resolves to the same buffer, never a fresh reload from
disk that would discard those edits -- the scan matches on buffer identity
(name), not on modified state.

## 6. `nvim_buf_delete` on an unmodified, window-invisible buffer succeeds silently

```
nvim_buf_delete(buf, {}) -> ok, buffer gone from nvim_list_bufs()
```

## 7. `nvim_buf_delete` refuses a MODIFIED buffer, content left untouched

```
nvim_buf_delete(buf, {}) -> error: 'Failed to unload buffer.'
nvim_buf_get_lines(buf, 0, -1, false) -> unchanged, still ['EDITED', ...]
```

No `force` is ever passed. This is the safety net `release_hidden` relies
on rather than reimplementing: a hold whose buffer has unsaved accepted
edits when the refcount reaches zero is not deleted -- nvim's own default
`force: false` refuses it, and the edits survive as an orphaned, still-loaded,
unlisted buffer nvim will hand back to whatever next names this path (a
`load_hidden` retry, or a real `:edit`), rather than being silently
discarded.

## 8. `nvim_buf_delete` does NOT refuse a buffer that is visible in a window

`nvim_buf_delete` does not refuse a window-visible buffer, contrary to an
earlier reading of this case that assumed it mirrored case 7's
modified-buffer refusal. Re-captured against the same pinned engine, in the
exact shape `release_hidden` actually faces -- a buffer opened normally via
`:edit` (this crate's own `OPEN_FILE_CHUNK`, not a bare
`nvim_win_set_buf`), the sole window showing it, ui-attached:

```
:edit path -> current buffer = 1, win_findbuf(1) = [win] -- confirms visible
nvim_buf_delete(1, {}) -> Ok(Nil), no error
nvim_list_bufs() -> [1] -- one buffer still present...
nvim_get_current_buf() -> 2 -- ...but it is a fresh empty buffer, not the
                               deleted one: the window's real content is gone
```

The same result holds with a second, unrelated buffer already open (deleting
the window's current buffer just switches the window to that other buffer
instead of a fresh one). Deleting a buffer nvim itself has no window context
for (the API caller passes no window) does not raise "Failed to unload
buffer." here -- nvim substitutes a replacement into every window that was
showing it and proceeds. `release_hidden` cannot rely on nvim refusing this
the way it reliably refuses a modified buffer (case 7): it must check
`vim.fn.win_findbuf` itself and skip the delete outright when the list is
non-empty, never attempting it and hoping for a refusal.

## 9. `RELEASE_HIDDEN_CHUNK`'s own `win_findbuf` guard, verified against both refusal shapes

```
-- window-visible buffer: guard trips, delete never attempted
win_findbuf(buf) -> [win]
RELEASE_HIDDEN_CHUNK(buf) -> (no call to nvim_buf_delete)
nvim_get_current_buf() -> unchanged, still the same buffer
nvim_buf_get_lines(buf, ...) -> unchanged, the user's real content

-- window-invisible, modified buffer: guard passes, delete attempted and
-- refused by nvim itself (case 7's own refusal), pcall swallows the error
win_findbuf(buf) -> []
RELEASE_HIDDEN_CHUNK(buf) -> pcall(nvim_buf_delete, buf, {}) fails silently
nvim_list_bufs() -> buf still present, content unchanged

-- window-invisible, unmodified buffer: guard passes, delete succeeds
win_findbuf(buf) -> []
RELEASE_HIDDEN_CHUNK(buf) -> buffer gone from nvim_list_bufs()
```

The window-visibility check runs in Lua, before ever calling
`nvim_buf_delete`, rather than trusting nvim to refuse on its own (case 8
disproved that trust) -- the modified-buffer case still relies on nvim's own
refusal (case 7), which held up under this same re-capture.

## 10. `vim.fn.bufadd` + `vim.fn.bufload` populate through nvim's real read pipeline

`nvim_create_buf` + `readfile` + `nvim_buf_set_lines` (the mechanism cases
1-9 above capture) never runs `BufReadPre`/`BufReadPost`, so filetype
detection, indent/editorconfig settings and the file's own fileformat/EOL
never happen. `bufadd`+`bufload` is nvim's own file-open path minus the
window/current-buffer switch:

```
bufadd(path) -> buf=2
bufload(buf)
getbufvar(buf, '&buflisted') -> 0
getbufvar(buf, '&filetype')  -> 'rust'   (a .rs fixture)
getbufvar(buf, '&modified')  -> 0
nvim_buf_get_lines(buf, 0, -1, false) -> ['fn main() {}']
```

Unlisted exactly like the `nvim_create_buf(false, false)` path (case 1) --
`bufadd` never lists the buffer it creates -- but with filetype detection
and every other `:edit`-triggered autocommand intact.

## 11. `bufload`'s own read is the undo baseline; `nvim_buf_set_lines` is not

```
-- bufadd + bufload
vim.fn.undotree().seq_cur -> 0
:undo                      -> no-op, lines unchanged: ['one', 'two', 'three']

-- nvim_create_buf + nvim_buf_set_lines (cases 1-9's mechanism, contrast)
vim.fn.undotree().seq_cur -> 1
:undo                      -> lines become [''], the buffer's own content is gone
```

`nvim_buf_set_lines` on a freshly created empty buffer is itself an
undoable edit (from empty to the file's content); the very first `u` the
user presses after the file is later opened normally in that same buffer
reverts back to empty, and a save after that truncates the file on disk.
`bufload` reads the file as nvim's own initial buffer state, the same as
opening the file fresh with `:edit` -- there is no "from empty" edit on the
undo tree to revert to.

## 12. `bufload` preserves `fileformat` and `endofline`; the write-back correctly reproduces CRLF

```
-- CRLF fixture ("one\r\ntwo\r\n" on disk)
bufload -> getbufvar(buf, '&fileformat') -> 'dos'
nvim_buf_get_lines -> ['one', 'two']          -- line terminators never appear in-buffer either way
:write -> disk bytes unchanged: "one\r\ntwo\r\n"

-- no-EOL fixture ("a\nb" on disk, no trailing newline)
bufload -> getbufvar(buf, '&endofline')    -> 0   (correctly recorded: source had none)
           getbufvar(buf, '&fixendofline') -> 1   (nvim's own default, independent of this chunk)
:write -> disk bytes become "a\nb\n"              -- fixendofline adds it back
```

The CRLF case round-trips byte-identical through a write because
`fileformat` is read correctly at load time (the old chunk hardcoded
`fileformat=unix` regardless of source, corrupting every line ending on the
next save). The no-EOL case's write gaining a trailing newline is not a
regression this chunk introduces -- `fixendofline` defaults to on and
applies identically to a buffer opened by a genuine `:edit`; what matters
is that `endofline` is read correctly (`false`) rather than hardcoded to
`true` the way the old chunk left it, since that is the flag other code
(and nvim's own write path) actually consults. Neither fixture's content
changes at all through a `load_hidden` -> `release_hidden` cycle with no
write in between -- `release_hidden` never touches the wire in a way that
writes to disk.

## 13. `bufadd` finds a pre-existing UNLOADED buffer by name rather than creating a second one

```
bufadd(path)              -> buf=5  (buflisted=0, loaded=0 -- an unloaded entry, no bufload call yet)
bufadd(path)  -- again    -> buf=5  (same handle, still unloaded until something calls bufload)
```

`buflisted=0` here, matching case 10 and `:help bufadd`: `bufadd` alone never
lists a buffer regardless of whether it created it fresh or found an
existing unloaded one. This matters beyond a typo now that `buflisted` is a
guard input to `RELEASE_HIDDEN_CHUNK`'s belt-and-braces check (case 14) --
every `bufadd`ed buffer starts unprotected by that check, listed only once
something else (a real `:edit`) chooses to list it.

`bufadd` is idempotent on the buffer's name whether or not the existing
entry is loaded -- unlike the old chunk's own `nvim_buf_set_name`, which
(Minor 1's own capture) silently orphaned a same-named unloaded buffer
instead of finding it. This matters for `created`'s ownership meaning: an
unloaded buffer that already existed for this path (the user's own prior
session state, not anything `load_hidden` made) must never be reported as
newly created, or the refcount's "may I delete this" bit would be wrong for
a buffer view never made. `LOAD_HIDDEN_CHUNK`'s own scan is extended to
match on name alone (loaded or not) rather than only loaded buffers, so
this case is caught by the scan and reported `created = false` before
`bufadd` is ever reached.

## 14. `:edit`-ing a hidden buffer's own path adopts it: `buflisted` flips to 1

```
bufadd(path); bufload(buf) -> buf=2, buflisted=0
:edit path                  -> nvim_get_current_buf() = 2 (the same buffer, found by name)
                                getbufvar(2, '&buflisted') -> 1
                                win_findbuf(2) -> non-empty
```

A buffer `load_hidden` created can become a real, listed, user-owned buffer
the moment the user opens the same path normally, without ever becoming a
*new* buffer number. `win_findbuf` alone catches this while the window
stays open; `RELEASE_HIDDEN_CHUNK`'s `buflisted` check is the belt to that
brace for the case where the user has since navigated away from the window
(buffer hidden again, `win_findbuf` empty) but the buffer is now theirs, not
ours, and must survive a release exactly like any other buffer nvim itself
lists.

## 15. `bufadd`'s own identity resolution for a not-yet-existing path is a different, stronger mechanism than `fnamemodify(p, ':p')`

`canonical_hidden_key`'s job is to key `EngineHandle::hidden_bufs` in
agreement with whatever buffer `bufadd` actually resolves a `load_hidden`
call onto -- not to reproduce `fnamemodify(p, ':p')`, which is a different,
weaker function `LOAD_HIDDEN_CHUNK`'s own `canon()` also happens to fall
back to (its scan loop, not `bufadd` itself). The two disagree on exactly
the case that matters here: a symlinked directory, no `.`/`..` component
anywhere in the path, and a leaf that does not exist yet.

```
-- fnamemodify(':p') never resolves a symlink unless a '.'/'..' component
-- forces it to actually walk the directory chain:
fnamemodify(link/nope.rs, ':p')    -> link/nope.rs      -- unresolved, no dot present
fnamemodify(link/./nope.rs, ':p')  -> real/nope.rs       -- resolved, the '.' triggers it

-- bufadd resolves the same no-dot spelling anyway -- its own identity
-- check is not gated on '.'/'..' at all:
bufadd(real/brand-new.rs) -> 2
bufadd(link/brand-new.rs) -> 2                          -- same buffer, no dot involved
nvim_buf_get_name(2)      -> .../real/brand-new.rs        -- stored in resolved form
```

`bufadd`'s resolution is whole-parent-or-nothing: it succeeds only when the
*entire* immediate parent directory exists (equivalent to a `chdir` into it
succeeding, symlinks resolved as a side effect), and otherwise leaves the
path completely unresolved -- it does not fall back to a shallower existing
ancestor when the immediate parent itself does not fully exist:

```
bufadd(real/brand-new4.rs)         -> 3
bufadd(link/sub2/../brand-new4.rs) -> 4   -- sub2 does not exist under link
                                              -- (real/sub2 does not exist either)
same buffer? false                          -- no fallback to resolving through
                                                link alone once sub2 fails
nvim_buf_get_name(4) -> link/sub2/../brand-new4.rs   -- left completely as given
```

`canonical_hidden_key`'s fallback (for a path `std::fs::canonicalize` cannot
resolve outright) mirrors exactly this: canonicalize the path's immediate
parent as a whole, and only that -- succeed and join the file name, or fail
and leave the path exactly as given. No lexical collapsing of `.`/`..` (the
resolved cases above never have any left over -- canonicalizing the parent
removes them as an intrinsic part of resolving it, not a separate textual
pass) and no multi-level ancestor fallback (the `sub2`-missing case above
shows `bufadd` itself has none either -- it does not fall back to resolving
through `link` alone once the deeper component fails).

## 16. `LOAD_HIDDEN_CHUNK`'s own `canon()` must mirror `bufadd`'s resolution too, or its `created` flag lies

Case 15's divergence is not only a Rust-side key problem: `LOAD_HIDDEN_CHUNK`
runs the identical `fnamemodify(p, ':p')`-falling-back `canon()` in its own
existing-buffer scan, on the nvim side, to decide whether a `load_hidden`
call is a reuse or a fresh create. Two spellings through a symlinked
directory, no `.`/`..` component, no fixture that exists yet -- the same
shape as case 15 -- make that scan miss a buffer it should have found, and
the fallthrough branch's `created = true` is unconditional, so a genuine
*reuse* (`bufadd` still resolves onto the identical buffer either way) gets
reported as a *create*:

```
-- old canon(): fs_realpath(p) or fnamemodify(p, ':p')
load via_real: buf=2 created=true
load via_link: buf=2 created=true    -- same buffer, but reported as a second create

-- new canon(): fs_realpath(p) or (fs_realpath(parent) .. '/' .. tail)
load via_real: buf=3 created=true
load via_link: buf=3 created=false   -- same buffer, correctly reported as a reuse
```

A wrongly-`true` `created` is not cosmetic: `RpcCall::ReleaseHidden`'s
delete is gated on `HiddenHold::owned`, which is OR'd from every reply's
`created` flag for that path. Whenever the *first* reply for a path is a
genuine create, the wrong flag from a later reused-spelling reply is
harmless (owned is already `true`). But had the *first* connection to see
this path been a real window's own `:edit`, or a different connection's
`load_hidden` -- both cases this scan exists to catch, per case 15's own
`docs/hidden-buffer-wire-capture.md` context above -- this connection's own
`load_hidden` would still report `created = true` for a buffer it did not
create, and its `release_hidden` would delete a buffer someone else still
has open. Matching `canon()` to `bufadd`'s own parent-realpath resolution
closes this at the scan itself: the match is found directly, and the
fallthrough `bufadd` branch (whose `created = true` is only ever correct
because nothing already matched) is never reached for a path some earlier
call, or a real window, already resolved under a different spelling.
