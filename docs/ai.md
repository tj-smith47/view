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
  this; no client, view included, can see it as it happens. view still
  catches it: a filesystem watcher over the trusted project root notices
  the write and drives nvim's own `:checktime` for it, the same mechanism
  that already runs when you switch back to view after editing a file in
  another terminal.

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

Nothing about this depends on which tool made the external change -- an
agent's shell command, a `git` operation run in another terminal, or you
editing the same file in a second editor are all the same case to view.
