# Wire capture: confirm-class prompt overlays

Captured live against the pinned engine per "capture, never recall." Source
of truth for the modal prompt overlay (`OverlayKind::Prompt`) implementation.

## Engine identity

```
$ nvim --version | head -3
NVIM v0.12.4
Build type: Release
LuaJIT 2.1.1785192264
```

Matches `.engine-pin` (`v0.12.4`) exactly.

## Capture method

A standalone Python msgpack-rpc client (no `pynvim`; not installed) spawns
`nvim --embed --clean` with the same hermetic `XDG_*`/`HOME` isolation
`EngineConfig::isolated()` uses, attaches as a real UI with
`ext_linegrid`/`ext_cmdline`/`ext_popupmenu`/`ext_messages`/`ext_tabline`,
and drives `nvim_input` as a **notification** -- fire-and-forget, matching
production `RpcCall::Input` exactly, since a *request* (`nvim_command`,
`nvim_eval`) would block on its own reply while nvim is itself blocked
inside a prompt's input loop and never send one back. Output below is the
flattened `redraw` batches, in wire order, split into named sections.

## 1. `:confirm()` -- the baseline choice prompt

Driven: `:call confirm("Save changes?", "&Yes\n&No")<CR>`, then an
unmatched key (`z`), then the accepted answer (`y`).

```
=== confirm: question + prompt line ===
['cmdline_show', [[0, 'call confirm("Save changes?", "&Yes\\n&No")', 0]], 42, ':', '', 0, 1, 0]
['mode_change', 'cmdline_normal', 4]
['mouse_off']
['flush']
['cmdline_hide', 1, False]
['msg_show', 'confirm', [[16, 'Save changes?', 10]], False, False, False, 1, 'typed_cmd']
['mode_change', 'normal', 0]
['mouse_on']
['flush']
['cmdline_show', [[0, '', 0]], 0, '', '[Y]es, (N)o: ', 0, 1, 10]
['mode_change', 'cmdline_normal', 4]
['mouse_off']
['flush']

=== confirm: unmatched key 'z' (expect re-arm, no new msg_show) ===
['cmdline_hide', 1, False]
['mode_change', 'normal', 0]
['mouse_on']
['flush']
['cmdline_show', [[0, '', 0]], 0, '', '[Y]es, (N)o: ', 0, 1, 10]
['mode_change', 'cmdline_normal', 4]
['mouse_off']
['flush']

=== confirm: answering 'y' ===
['cmdline_hide', 1, False]
['win_viewport', 2, ..., 0, 2, 0, 0, 1, 0]
['mode_change', 'normal', 0]
['mouse_on']
['flush']

post-resolution liveness check (nvim_eval v:count): [1, 2, None, 0]
```

**Confirms the as-built `MessageEntry::is_prompt` doc exactly:** the
question arrives once as `msg_show` kind `"confirm"`; the answer line is a
separate, repeated `cmdline_show`; an unmatched key re-arms via
`cmdline_hide` + `cmdline_show` alone, with **no second `msg_show`**; the
session stays alive and responsive after resolution.

## 2. `<Esc>` and bare `<CR>` on a choice prompt

```
=== confirm: <Esc> (does it resolve as cancel?) ===
['cmdline_hide', 1, True]
['win_viewport', ...]
['mode_change', 'normal', 0]
['mouse_on']
['flush']
post-Esc liveness check: [1, 2, None, 0]

=== confirm: bare <CR> (does it select the [Y] default?) ===
['cmdline_hide', 1, False]
['win_viewport', ...]
['mode_change', 'normal', 0]
['mouse_on']
['flush']
post-CR liveness check: [1, 2, None, 0]
```

Both resolve the dialog (not a re-arm) and leave the session alive.
`cmdline_hide`'s second argument is nvim's own `abort` flag: `True` for
`<Esc>`, `False` for every other case captured (a matched answer, the
bracketed default via `<CR>`, and a re-arm). `accepts()` therefore treats
`<CR>` and `<Esc>` as resolving keys on every choice prompt, not just the
literal accelerator letters.

## 3. The swapfile ATTENTION dialog

**Empirical finding, load-bearing:** on this pinned build, reopening a file
whose swapfile is owned by a **verifiably dead** process on the same host
fires **no `SwapExists` autocommand at all**, confirmed with a custom
marker-writing autocmd (fires when the owner is alive; does not fire when
the owner was `SIGKILL`'d moments earlier, same host, same hermetic
`XDG_STATE_HOME`). Nvim silently reclaims the swapfile; no dialog, no
`W325`, nothing on the wire. This is a genuinely different, more recent
core behavior than the "ATTENTION always fires on a stale swapfile"
assumption the original capture instructions were written against --
reality corrected that assumption here.

**The dialog *is* reachable when the owner is still alive** (or, by the
same code path, whenever nvim cannot immediately prove the owner dead), and
when reached, it captures as **the exact same wire shape as `:confirm()`**
-- `msg_show` kind `"confirm"` paired with a `cmdline_show` choice prompt --
which is why `PromptState` needs no separate ATTENTION-specific code path.
Captured with the default `nvim.swapfile` augroup removed so the dialog
free-form autoconfirms (`autocmd nvim.swapfile SwapExists set-vars`
otherwise resolves it silently before it reaches the wire):

```
=== ATTENTION dialog (owner alive, default autocmd removed) ===
['cmdline_show', [[0, 'edit swap_realdialog.txt', 0]], 107, ':', '', 0, 1, 0]
['mode_change', 'cmdline_normal', 4] ['mouse_off'] ['flush']
['cmdline_hide', 1, False]
['msg_show', 'emsg', [[25, 'E325: ATTENTION', 6]], False, True, False, 1, 'typed_cmd']
['msg_show', 'confirm', [[16, 'Found a swap file by the name "..."\n'
  '          owned by: root   dated: ...\n'
  '         file name: ".../swap_realdialog.txt"\n'
  '          modified: no\n'
  '         user name: root   host name: apps\n'
  '        process ID: 1348394 (STILL RUNNING)\n'
  'While opening file "..."\n'
  '(1) Another program may be editing the same file. ...\n'
  '(2) An edit session for this file crashed. ...\n'
  'Swap file "..." already exists!', 10]], False, False, False, 2, 'typed_cmd']
['mode_change', 'normal', 0] ['mouse_on'] ['flush']
['tabline_update', ...]
['cmdline_show', [[0, '', 0]], 0, '', '[O]pen Read-Only, (E)dit anyway, (R)ecover, (Q)uit, (A)bort: ', 0, 1, 10]
['mode_change', 'cmdline_normal', 4] ['mouse_off'] ['flush']
```

Two `msg_show` entries land in the same batch: an `emsg`-kind header line
("E325: ATTENTION") and a `confirm`-kind body carrying the full block of
explanatory text. `is_prompt()` only matches the `confirm`-kind one --
correct, since the `emsg` line is ordinary persistent toast text (an
error), not the prompt question itself, and the two coexist without
conflict. Answering `q` (Quit) resolves cleanly; session stays alive.

## 4. `inputlist()` -- the free-text prompt class

Driven: `:call inputlist(["Select an option:", "1. one", "2. two"])<CR>`,
then `1<CR>`.

```
=== inputlist(): list + number prompt ===
['cmdline_show', [[0, 'call inputlist([...])', 0]], 57, ':', '', 0, 1, 0]
['mode_change', 'cmdline_normal', 4] ['mouse_off'] ['flush']
['cmdline_hide', 1, False]
['msg_show', 'confirm', [[0, 'Select an option:\n1. one\n2. two', 0]], False, False, False, 1, 'typed_cmd']
['mode_change', 'normal', 0] ['mouse_on'] ['flush']
['cmdline_show', [[0, '', 0]], 0, '', 'Type number and <Enter> or click with the mouse (q or empty cancels): ', 0, 1, 0]
['mode_change', 'cmdline_normal', 4] ['mouse_off'] ['flush']

=== inputlist(): answering '1<CR>' ===
['cmdline_show', [[0, '1', 0]], 1, '', 'Type number and <Enter> or click with the mouse (q or empty cancels): ', 0, 1, 0]
['flush']
['cmdline_hide', 1, False]
['win_viewport', ...]
['mode_change', 'normal', 0] ['mouse_on'] ['flush']
```

Same `msg_show` kind (`"confirm"`) as `:confirm()`; the difference is
entirely in the paired `cmdline_show` prompt text. Two structural facts
distinguish this class and drive `PromptState`'s parser:

- The prompt text does not follow the `[X]abel, (Y)abel: ` bracket/paren
  convention at all (its first character is `T`, not `[` or `(`), so a
  parser built against capture 1's shape correctly refuses to classify it
  as a choice list.
- Nvim itself echoes what's typed back through `cmdline_show`'s `content`
  on every keystroke (`content: [[0, '1', 0]]` above) -- the authoritative
  source `PromptState` reads for its free-text `input`, rather than
  independently tracking typed characters and risking drift from what
  nvim's own cmdline buffer holds.

## 5. `return_prompt` no longer exists on this engine

The capture plan named `return_prompt` as a third prompt kind to capture. Driving the
classic triggers (`:!echo hi`, and `:for i in range(1,30) | echomsg i |
endfor`, both of which produce the legacy multi-line hit-enter prompt) with
`ext_messages` attached produced **no `return_prompt`-kind `msg_show` at
all**, and the session was never actually blocked waiting for
acknowledgement (a follow-up `nvim_eval` succeeded immediately, with no key
sent). This is not a capture-script bug: it is a documented, deliberate
removal, found in the pinned build's own changelog rather than guessed:

```
$ grep -n -B5 -A15 return_prompt \
    $(brew --prefix)/Cellar/neovim/0.12.4/share/nvim/runtime/doc/news.txt
EVENTS
• |ui-messages| no longer emits the `msg_show.return_prompt`, and
  `msg_history_clear` events. The `msg_clear` event was repurposed and is
  now emitted after the screen is cleared. These events arbitrarily
  assumed a message UI that mimics the legacy message grid. Benefit:
  reduced UI event traffic and more flexibility for UIs.
```

With `ext_messages` attached (which view uses, and `cmdheight=0` forces),
nvim trusts the UI to hold and scroll its own message history instead of
blocking for an on-grid "-- More --"/hit-enter acknowledgement, so there is
no return-prompt-class blocking dialog left to route on this engine at
all. `MessageEntry::is_prompt`'s existing `kind == "confirm"` check was
already correct as-built -- it never referenced `return_prompt` -- so this
finding requires no code change, only this record that the interface
comment's "confirm / return_prompt / inputlist-class" phrasing describes
one reachable wire kind (`"confirm"`) covering two behaviorally distinct
prompt shapes (choice and free-text), not three.

## Conclusions for the implementation

- `PromptState::from_entry` gates on `MessageEntry::is_prompt()` alone
  (`kind == "confirm"`); this is the only prompt-class kind reachable on
  the pinned engine, and it already covers `:confirm()`, the ATTENTION
  dialog, and `inputlist()`.
- Choice-vs-free-text is derived from the paired `cmdline_show` prompt
  text (`PromptState::learn_cmdline`), not from anything in the
  `msg_show` itself: a prompt string opening with `[` or `(` per accelerator
  segment is a choice prompt; anything else is free text.
- `accepts()` on a choice prompt: the accelerator letters (case-insensitive
  per `:help confirm()`), `<CR>` (bracketed default), `<Esc>` (cancel) --
  all three captured resolving the dialog rather than re-arming it.
- `accepts()` on a free-text prompt: a digit, `<BS>`, `<CR>`, `<Esc>`, and
  `q` -- exactly what the prompt's own text documents as accepted.
- No timeout: the engine is genuinely blocked in its own input loop on
  this path, not on an RPC request view is waiting on, and every capture
  above shows the session staying alive and responsive indefinitely until
  an accepted key resolves it.
