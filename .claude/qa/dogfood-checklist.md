# Dogfood QA checklist — every input produces observable output

Guided manual sweep for a live session (real terminal, real config, SSH
counts). Each row is an exact input and the visible result that must
happen. **Anything that ends in nothing on screen is a bug, no
exceptions.** Set `VIEW_LOG=/path/to/log` before launching so timestamps
pin anything weird.

The automated version of the invariants here is the visual acceptance
sweep (`task acceptance`); this doc is the human pass over the same
ground plus judgment calls a script can't make.

## Entry points

- [ ] `:View ai` → panel toggles. In an untrusted dir the trust prompt
  comes first; after **Yes** the panel **opens** (regression: the
  re-dispatched invoke used to dead-end in a notice).
- [ ] `:View picker` / `:View tree` / `:View notifications` bare →
  files picker / tree / message history, not a scolding toast.
- [ ] `:View ai bogusverb` and `:View bogusfeature` → visible notice
  naming valid forms.
- [ ] `:View ` + `<Tab>` → completion lists features; `:View ai <Tab>`
  lists verbs.

## Trust prompt

- [ ] Answer **No** → visible confirmation something happened, not just
  the box vanishing.
- [ ] `<Esc>` on the prompt → same; then `:View ai` again re-prompts
  (the pending invoke is not silently gone forever).
- [ ] Box is themed: float background from the active theme, selection
  is a normal highlight (never bare reverse video), width fits the
  question.

## AI panel

- [ ] Long transcript → newest lines visible by default; `PageUp`
  scrolls back, an indicator shows you're off the tail, scrolling to
  the bottom resumes follow.
- [ ] `<CR>` on an **empty** prompt → feedback, not silence.
- [ ] Prompt submitted with the adapter missing or broken
  (`ai.adapter` pointed at a bogus path) → error names the adapter,
  not a hang.
- [ ] Close the panel while an agent is mid-turn → a visible signal the
  session is still alive; reopening shows progress.
- [ ] `<S-Left>` / `<S-Right>` and `<C-w><` / `<C-w>>` both resize the
  focused panel/tree; width survives close/reopen within the session; a
  terminal resize with panel + tree open doesn't corrupt geometry.
- [ ] `[keys] sidebar_wider` / `sidebar_narrower` in `view.toml` rebind
  them; an unreadable entry leaves that action on its defaults and
  raises a notice.
- [ ] Typing in the prompt echoes instantly; input longer than the
  panel width keeps its cursor and tail visible.
- [ ] Submit a prompt → its own marker spins until the agent's first
  word, then stands back down to `❯`; a turn that ends without an
  answer leaves the marker still, never spinning forever.

## Composer

- [ ] Type into the prompt → the caret is on the character you are
  about to type, not parked at the panel edge; it stays there after a
  `<BS>`, after the transcript grows under it, and after a resize.
- [ ] Paste a multi-line block → the composer shows it as the lines it
  was copied as, and the echoed prompt in the transcript breaks in the
  same columns. A trailing newline leaves the caret on an empty last
  row rather than submitting.
- [ ] Paste something far longer than the panel → it still lands whole,
  the newest rows are the visible ones, and typing after it is not
  sluggish.

## Permissions

- [ ] Agent asks permission → the prompt lists its options numbered
  `1`…`9` with `press a number, <Esc> cancels`; press the digit and the
  turn continues, `<Esc>` cancels it visibly.
- [ ] The digits answer the prompt **whatever** was focused a moment
  ago — they are never swallowed by a composer or an overlay while a
  question is pending, and the caret sits on the prompt, not the
  composer, while it is.
- [ ] Choose an *always* option → the next request of the same kind is
  answered without asking again, and a new session asks again from
  scratch.

## Inline review

- [ ] Agent proposes an edit → the diff is drawn **in the file's own
  buffer** (`DiffDelete` on what goes, `DiffAdd` virtual lines for what
  arrives), not in a modal. `[ai.review] open_target = "split"` opens
  it beside what you were reading instead.
- [ ] The header above the current hunk names its keys and is
  **readable end to end** at 120 columns with the panel open —
  including the way out, `<leader>hq leave`.
- [ ] `<leader>ha` / `<leader>hA` / `<leader>hx`, `]c` / `[c`,
  `<leader>hq` all act, and each one's outcome is a transcript line.
  `:View review <Tab>` completes the same verbs; `:View review bogus`
  is refused with a notice naming the real ones.
- [ ] Type over a hunk → it goes stale and its header offers
  `<leader>hR` instead of the accept; keep editing and even that goes.
- [ ] After the review ends, any mapping of yours it displaced
  (`]c`, `[c`, `<leader>h*`) does what it did before.

## Leaving

- [ ] `:q` / `:qa` → the shell prompt comes back on a clean terminal:
  no leftover alt screen, cursor visible, echo on, `reset` not needed.
- [ ] `kill -TERM` / `kill -HUP` the view process, and Ctrl+C at the
  shell → the same clean terminal, and no nvim left behind.

## Known-silent suspects (outside the fixed surface — flag on sight)

- [ ] Unmapped keys inside picker / tree / message-history overlays →
  swallowed silently, or answered with a notice?
- [ ] `<leader>ff` — view claims it over an existing Telescope mapping.
  Decide whether that's wanted; `native.picker = false` in view.toml
  hands it back.
- [ ] Sticky vs transient: a `:bogus` error toast must survive a
  keypress; an `:echo` must expire in a few seconds.
- [ ] Anything that flashes and vanishes before it can be read → note
  the wall-clock time; the `VIEW_LOG` line at that timestamp names the
  event.
