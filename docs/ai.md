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
agent's reasoning each speak in their own voice -- each entry opens with
its own marker and paints in its own color from your colorscheme, and
reasoning is never folded into the reply around it:

```
❯ summarize the retry policy
● The client retries five times with a fixed 200ms delay.
◦ checking whether the cap is configurable
```

A prompt appears the moment you send it, not when the agent gets around to
mentioning it. The header's first row keeps the session's accounting as the
agent reports it:

```
context 8153/200000, cost 0.14 USD
```

Tool calls appear as they start and update in place as they finish, and
their marker says where each one stands -- `·` waiting, a spinner while it
runs, `✓` done, `✗` failed:

```
✓ Read src/client.rs
⠹ Run tests
```

A call the agent never finished -- a session that crashes mid-call leaves
one -- settles back to `·`: the panel never invents an outcome the agent
did not report, and never animates a marker for work nobody is doing.

The agent's plan reads the same way, one row per task: `·` not started,
`▸` under way, `✓` done.

The panel is non-modal: `<Esc>` steps out of the composer and leaves the
panel on screen beside your buffer, and while you are out of it every key
reaches the editor exactly as it did before. When the panel is open but not
focused it says so and names the way in (`:View ai focus`), and `<leader>ai`
puts you back in it -- the same key reads the panel before it acts:

| the panel is | `<leader>ai` |
| --- | --- |
| closed | opens it and puts the cursor in the composer |
| open, and you are in it | closes it |
| open, and you are not in it | puts you back in it |

So the key never dead-ends. Closing it, whichever way you get there, leaves
the agent session running -- reopen and the transcript is where you left it.

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

## Answering a permission request

An agent that needs your say-so before a tool call blocks on it, and the
request opens the panel where it can be seen. Every option the agent
offered gets a row, numbered in the order the agent sent them, with the
wire's own word for what it does in brackets:

```
Permission requested for Edit Taskfile.yml
  1 Deny (reject_once)
  2 Allow Once (allow_once)
  3 Always Allow (allow_always)
press a number, <Esc> cancels
```

You answer with the digit. `<Esc>` answers too -- it cancels the request,
which is the one answer that exists whatever the agent offered. No letter
answers a prompt, deliberately: a diff review pends alongside the prompt on
every agent edit and owns letters of its own, so `a` accepts the hunk you
are looking at and never the question you are not.

The colors carry the same split as the words: the always-allow row is not
painted as another allow, because it is the one answer whose consequence
outlives the question.

### What "Always Allow" grants

Answering always-allow records a standing grant for that tool's kind, for
this session only. A later request of the same kind is answered for you,
and says so on the transcript rather than passing in silence:

```
‼ auto-allowed edit (standing grant)
```

A different kind still asks. The grant is never written to disk, so a new
session -- including one that came back after a crash -- starts with none.

view keeps this itself because the pinned adapter does not: it accepts the
always-allow answer and then asks again on every later call. Reporting the
grant on the transcript is what keeps that answerable -- what view answered
on your behalf is on the record with everything else the session did.

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
