# msg_show kind lifetimes, observed against the pinned engine

Engine: nvim v0.12.4 (`.engine-pin`), driven over stdio msgpack-RPC with
`ext_messages` + `ext_linegrid` + `ext_cmdline` attached. Every line below is
an observation from that session, not a reading of the docs.

## The kind table view classifies against

`api-ui-events.txt` in the pinned runtime
(`.../neovim/0.12.4/share/nvim/runtime/doc/api-ui-events.txt`, `*ui-messages*`)
lists 22 kinds. The error/warning ones are `emsg`, `echoerr`, `lua_error`,
`rpc_error`, `wmsg` — and `shell_err`, which view was missing.

## shell_err is real and carries the failing command's only explanation

`:!echo out; echo err 1>&2` under `ext_messages` produced, in order:

```
('shell_cmd', ':!echo out; echo err 1>&2\r\n')
('shell_out', 'out\n')
('shell_err', 'err\n')
```

stderr arrives as its own `msg_show` with kind `shell_err`. Classified
transient, it was dismissed by the next keypress like an info line.

## confirm is neither persistent nor transient

`call confirm('Save changes?', "&Yes\n&No")`, with flush boundaries shown:

```
AFTER PROMPT:
  msg_show     [['confirm', [[16, 'Save changes?', 10]], False, False, False, 1, '']]
  flush
  cmdline_show [[[[0, '', 0]], 0, '', '[Y]es, (N)o: ', 0, 1, 10]]
  mode_change  cmdline_normal
  flush

AFTER INVALID KEY 'q':
  cmdline_hide
  mode_change  normal
  flush
  cmdline_show [[[[0, '', 0]], 0, '', '[Y]es, (N)o: ', 0, 1, 10]]
  mode_change  cmdline_normal
  flush

AFTER 'y':
  cmdline_hide
  mode_change  normal
  flush
```

Three facts follow, and they rule out both of the lifetimes view already had:

1. The question is emitted **once**. Re-arming the prompt after a refused key
   re-emits `cmdline_show` alone. A question dismissed on that keypress leaves
   an answer line with nothing to answer.
2. No `msg_clear` ever arrives — not after the refused key, not after the
   answer, not after a later `:echo`. A question kept until explicitly cleared
   occludes the buffer for the rest of the session.
3. `cmdline_hide` is **not** the end-of-prompt signal. The refused key emits
   `cmdline_hide` + `flush` before the re-armed `cmdline_show`, so a rule that
   drops the question when the cmdline closes — whether checked at the event or
   at the following flush — drops it on the first refused key. This was the
   first hypothesis and this trace is what disproved it.

The rule that survives all three: the question is dismissable by user activity
like any transient entry, but only once the cmdline has closed. While the
prompt is open no keypress takes it away; after it closes the next keypress
does.

## Reproducing

`~/.claude/tmp/kinds.py` (shell kinds) and `~/.claude/tmp/kinds5.py` (the
confirm trace, with `flush` left in the non-skipped set) drive the engine
directly and print the event log.
