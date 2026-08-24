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

## Known-silent suspects (outside the fixed surface — flag on sight)

- [ ] Unmapped keys inside picker / tree / message-history overlays →
  swallowed silently, or answered with a notice? (The AI review overlay
  has the notice; siblings may lag.)
- [ ] `<leader>ff` — view claims it over an existing Telescope mapping.
  Decide whether that's wanted; `native.picker = false` in view.toml
  hands it back.
- [ ] Sticky vs transient: a `:bogus` error toast must survive a
  keypress; an `:echo` must expire in a few seconds.
- [ ] Anything that flashes and vanishes before it can be read → note
  the wall-clock time; the `VIEW_LOG` line at that timestamp names the
  event.
