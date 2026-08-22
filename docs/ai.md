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

## The panel

The panel is a transcript. Your prompts, the agent's replies, and the
agent's reasoning each speak in their own voice -- reasoning renders under
a `Thinking:` prefix, never folded into the reply around it -- and the
header's first row keeps the session's accounting as the agent reports it:

```
context 8153/200000, cost 0.14 USD
```

Tool calls appear as they start and update in place as they finish. When
the panel is open but not focused, it says so and names the way in
(`:View ai focus`).

The prompt you are composing sits under the header, and it wraps rather
than running off the edge -- a prompt several sentences long grows the
composer downward, indented under its own `>`, with the transcript giving
up the rows:

```
> summarize the retry policy in the client, then rewrite it to
  back off exponentially and cap at thirty seconds, and leave the
  existing tests passing
```

The composer never takes more than half the rows the panel has left after
its border, header, and whichever of the accounting row and the crash
banner are showing -- and it scrolls inside them after that, so the end of
what you are typing is always the last row on screen, and the transcript is
never left with fewer rows than the composer, however long the prompt gets.
The transcript loses its last row only on a panel too short to hold that
chrome and one line at once, where the prompt row is all there is space
for.

The transcript follows its newest line, the way a terminal does, so a
session hours long still shows you what just happened. To read back over
it, the focused panel scrolls:

| key | moves |
| --- | --- |
| `<PageUp>` / `<PageDown>` | a full window |
| `<C-u>` / `<C-d>` | half a window |

Scrolling up holds the window where you left it -- an agent that keeps
talking no longer drags the text out from under you -- and the last row
says so:

```
-- more below, <PageDown> follows again --
```

A page lands on the line directly above or below the one it came from --
nothing is stepped over and nothing repeats -- so paging up to the start of
a session and back down shows you every line of it.

Scrolling back down to the newest line resumes following, and so does
submitting a prompt, so an answer never streams in off screen. These are
named keys the composer cannot type, so a half-written prompt survives a
scroll.

`<C-d>` is the one key with two jobs: while the crash banner is up it
dismisses the banner (which is what the banner itself says), and it scrolls
once the banner is gone. A review or an unanswered permission request owns
the panel's keys while it is up, so none of the four scroll then -- inside
a review they answer the way every other key a review owns does, with the
notice naming the ways out.

## Panel width

The focused panel resizes where it stands, 5% of the terminal per press:

| key | does |
| --- | --- |
| `<S-Right>` | one notch wider |
| `<S-Left>` | one notch narrower |

The width holds between 15% and 70% and lasts the session -- close the panel
and reopen it and it comes back the width you left it at. Both are named
keys the composer cannot type, so a half-written prompt survives a resize,
and unlike the scroll keys they work while a review or a permission request
is up: a width decides nothing either of those owns, and a diff too narrow
to read is exactly when you want it.

The width a session *starts* at is `view.toml`'s, and the file tree has the
same key beside it:

```toml
[ai]
panel_width = 40           # percent of the terminal; 15..70, default 30

[native]
tree_width = 25
```

Both are optional, and neither can fail your config: a whole number
outside the range opens at the nearest end, and a value that is not a
whole number at all opens at the default and tells you so.

## Reviewing an agent's edits

An edit the agent proposes through ACP does not touch your buffer until
you accept it. The proposal opens as a hunk-by-hunk review:

```
a accept  A accept all  x reject  R re-diff  ] next  [ prev  q close
```

While a review is open it owns the keys; anything unmapped answers with a
notice naming the open review and both ways out, rather than doing
nothing. An accepted review is one undo entry -- a single `u` retracts the
whole thing, never joined onto your own preceding edit.

## How an agent's writes reach your buffers

An agent can touch a file two ways, and view treats them differently:

- **Routed through ACP** (`fs/write_text_file`): the agent asks, view
  applies the change to the buffer nvim already holds (so unsaved edits
  elsewhere in that buffer are never clobbered) and saves it. You see this
  the same way you see any other buffer change.
- **Out-of-band** -- an agent's own shell tool (`sed`, `cat >`, a build
  script, `git checkout`) writing or removing a file directly. No ACP
  message describes this; no client, view included, can see it as it
  happens. view catches it a different way: while an agent session is
  running, a filesystem watcher over the trusted project root notices the
  write -- or the removal -- and drives nvim's own `:checktime` for it, the
  same mechanism that already runs when you switch back to view after
  editing a file in another terminal.

### When the watcher is running, and what it covers

The watcher exists for the agent's blind spot, so it lives and dies with
the agent session:

| | detected? |
| --- | --- |
| an agent session is running | yes, anywhere under the trusted project root |
| no agent session has started yet, or the last one ended | no |
| a path outside the trusted project root | no |
| `.git/`, `target/`, `node_modules/`, `.venv/`, or anything your `.gitignore` covers | no |

A file being deleted counts as a change like any other: it is noticed on
the same terms, and answered by the notice below rather than by a prompt. A
tool that *replaces* a file is a save, not a deletion, and reads as one:
whether it renames a temp file over the target or unlinks the target and
writes it again, what you get is the ordinary reload. A save of the second
kind can be caught between its two halves -- the file really is missing for
the moment between the unlink and the rewrite -- so view never speaks on the
strength of one look: a path that reads as missing is checked once more, a
fraction of a second later, and only a path that is still missing then is
mentioned at all. For a save that finishes promptly -- every shape above --
nothing flashes up and disappears again. A file that stays missing for
longer than that is genuinely missing as far as anything can tell, so it is
mentioned, and its notice comes down as soon as any later check reads the
path again.

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
fs.inotify.max_user_watches); writes under it, and under everything not
yet registered, will not be noticed
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

If the path is no longer a readable file -- removed, or now holding a
directory, a symlink to nothing, a socket, a device, or a pipe -- nothing is
read from it at all, whether view noticed the write by itself or you asked
for the reload. Nothing is reloaded and nothing is lost; view says so, once
the second look above confirms it, rather than leaving you to find out at
your next `:w`.

With unsaved edits in the buffer, those edits are now the only copy:

```
/home/you/project/src/lib.rs is no longer a readable file on disk -- nothing
was reloaded, and your buffer still holds your edits
```

Without them, the buffer is holding whatever it last read, and view says
that instead rather than crediting you with edits you never made:

```
/home/you/project/src/lib.rs is no longer a readable file on disk -- nothing
was reloaded, and the buffer still holds the content it last read
```

This case never opens the prompt: a path that cannot be read has no reload
to offer, so there is no question to put to you.

If the re-read is refused for some other reason, view says that too rather
than letting it pass for a completed discard. It does not guess what you
are left looking at, because a re-read that fails part way through can
leave the new content or an empty buffer, so the notice tells you to check
the buffer before you save over the file.

Nothing about the *outcome* depends on which tool made the change: once
view knows a file changed on disk, an agent's shell command, a `git`
operation in another terminal, and a second editor all take the identical
path through nvim's own file-changed handling. What differs is only whether
view found out at the time -- see the table above.
