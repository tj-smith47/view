# AI agents

`<leader>ai` (or `:View ai toggle`) opens the AI panel and, on its first use
per project, asks you to trust the project root before any agent starts:

```
Trust /home/you/project to launch an AI agent? Agents can read and write
files in this project.
```

Trust is per-project and durable -- answer once, and every later session in
the same root skips the prompt. An agent's file reads and writes are
confined to that root for as long as the session runs.

## How an agent's writes reach your buffers

An agent can touch a file two ways, and view treats them differently:

- **Routed through ACP** (`fs/write_text_file`): the agent asks, view
  applies the change to the buffer nvim already holds (so unsaved edits
  elsewhere in that buffer are never clobbered) and saves it. You see this
  the same way you see any other buffer change.
- **Out-of-band** -- an agent's own shell tool (`sed`, `cat >`, a build
  script, `git checkout`) writing a file directly. No ACP message describes
  this; no client, view included, can see it as it happens. view catches it
  a different way: while an agent session is running, a filesystem watcher
  over the trusted project root notices the write and drives nvim's own
  `:checktime` for it, the same mechanism that already runs when you switch
  back to view after editing a file in another terminal.

### When the watcher is running, and what it covers

The watcher exists for the agent's blind spot, so it lives and dies with
the agent session:

| | detected? |
| --- | --- |
| an agent session is running | yes, anywhere under the trusted project root |
| no agent session has started yet, or the last one ended | no |
| a path outside the trusted project root | no |
| `.git/`, `target/`, `node_modules/`, `.venv/`, or anything your `.gitignore` covers | no |

Editing a file in a second editor, or a `git` command in another terminal,
is detected on exactly those terms -- during a session, inside the root,
outside the skipped directories. With no session running, view notices the
change the way nvim always has: when you next write, reload, or switch to
that buffer.

If the watcher cannot cover the whole root -- most often because the
operating system's own limit on watched directories was reached
(`fs.inotify.max_user_watches` on Linux) -- view says so:

```
out-of-band write detection is degraded: the platform's watch limit was
reached while registering /home/you/project/crates (raise
fs.inotify.max_user_watches); writes under it will not be noticed
```

It never goes quiet and leaves you believing detection is on.

Both paths end up in the same place -- nvim's own file-changed handling --
so the outcome depends only on the buffer's own state when the write lands,
never on which path the write took:

| buffer state | outcome |
| --- | --- |
| not open in any buffer | nothing -- no UI, nothing to reconcile |
| open, no unsaved edits of yours | reloaded silently, showing the new content |
| open, with unsaved edits of yours | a prompt, since neither answer is safe to pick for you |

The prompt only appears for the third case, when your own edits and an
external change to the same file both exist at once:

```
/path/to/file.rs changed outside view while the buffer had local edits.
Reload and discard the local edits, or keep them and ignore the external
change?
```

- **Reload** discards your local edits and takes the file's current
  on-disk content.
- **Keep local** leaves your edits exactly as they are and ignores the
  external change; the file on disk is not touched by this choice.

If a reload cannot be carried out -- the file was removed between the
prompt and your answer, or something refused the re-read -- view says that
too, and your local edits stay in the buffer rather than silently being
treated as discarded.

Nothing about the *outcome* depends on which tool made the change: once
view knows a file changed on disk, an agent's shell command, a `git`
operation in another terminal, and a second editor all take the identical
path through nvim's own file-changed handling. What differs is only whether
view found out at the time -- see the table above.
