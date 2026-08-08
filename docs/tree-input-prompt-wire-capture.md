# Wire capture: file-tree create/rename/delete prompts

Captured live against the pinned engine per "capture, never recall." Source
of truth for `TREE_INPUT_PROMPT_CHUNK` (shared by
`EngineHandle::tree_create_prompt` and `EngineHandle::tree_rename_prompt`)
and `TREE_DELETE_CONFIRM_CHUNK` (`EngineHandle::tree_delete_confirm`) --
whether a blocked `vim.fn.input()`/`vim.fn.confirm()`, primed with a
`kind = "confirm"` `nvim_echo`, arrives on the redraw wire as the same
`msg_show`/`cmdline_show` pair `PromptState`'s existing `Answer::Choices`
parsing already handles, or as something this crate has never actually
observed and would silently mis-route.

## Engine identity

```
$ nvim --version | head -3
NVIM v0.12.4
Build type: Release
LuaJIT 2.1.1785192264
```

Matches `.engine-pin` (`v0.12.4`) exactly.

## Capture method

`EngineHandle::request_probe` (the generic async-probe seam this crate
already uses for highlight-attribute capture) issues each chunk under
test over a real `ui_attach`ed connection, and the redraw events it
produces are logged as they arrive, then again after answering with real
`EngineHandle::input` keystrokes. `request_probe` decodes its reply
through the fg/bg-shaped `decode_hl_probe_reply` (built for a different
probe), so the logged `HlProbeReply` lines below carry no signal -- they
exist only to mark "the blocking call returned"; the *returned value* is
intentionally covered elsewhere, see Conclusions.

## 1. Create prompt (`TREE_INPUT_PROMPT_CHUNK`, empty `default`)

Chunk driven with `("New file: ", "")`, `<CR>` sent, "hello" typed first:

```
EVENT: MsgShow { kind: "confirm", content: [(16, "New file: ")], replace_last: false }
EVENT: CmdlineShow { content: [(0, "")], pos: 0, firstc: "", prompt: "New file: ", indent: 0, level: 1 }
EVENT2: CmdlineShow { content: [(0, "hello")], pos: 5, firstc: "", prompt: "New file: ", indent: 0, level: 1 }
EVENT2: CmdlineHide
```

## 2. Rename prompt (`TREE_INPUT_PROMPT_CHUNK`, prefilled `default`)

Chunk driven with `("Rename: ", "old.txt")`:

```
RENAME_EVENT: MsgShow { kind: "confirm", content: [(16, "Rename: ")], replace_last: false }
RENAME_EVENT: CmdlineShow { content: [(0, "old.txt")], pos: 7, firstc: "", prompt: "Rename: ", indent: 0, level: 1 }
RENAME_EVENT2: CmdlineHide
```

The `default` argument arrives pre-populated in `CmdlineShow`'s own
`content`/`pos` (cursor already past `old.txt`, ready to edit or accept as
typed) -- nvim does the prefill, not this crate.

## 3. Delete confirm (`TREE_DELETE_CONFIRM_CHUNK`)

Chunk driven with the prompt text `"Delete foo.txt?"` against a path with no
loaded buffer (the bufloaded guard's fall-through case), answered `"y"`:

```
DEL_EVENT: MsgShow { kind: "confirm", content: [(16, "Delete foo.txt?")], replace_last: false }
DEL_EVENT: CmdlineShow { content: [(0, "")], pos: 0, firstc: "", prompt: "[Y]es, (N)o: ", indent: 0, level: 1 }
DEL_EVENT2: CmdlineHide
```

`vim.fn.confirm(prompt, "&Yes\n&No")` reuses nvim's own accelerator-prompt
rendering (`[Y]es, (N)o: `) rather than anything this crate constructs --
the tree's delete confirm is indistinguishable on the wire from any other
`confirm()`-class prompt already in the codebase.

This capture predates `TREE_DELETE_CONFIRM_CHUNK`'s `bufloaded` guard and
still reflects the wire shape exactly: the guard only changes what happens
*before* `vim.fn.confirm` runs, never what crosses the wire once it does.
When the guard finds a loaded buffer for the target path, the chunk returns
`{ buffer_open = true }` and never calls `vim.fn.confirm` at all -- there is
no `MsgShow`/`CmdlineShow` pair to capture for that path, because nothing
is put on the wire; the absence of any prompt traffic *is* the live proof,
exercised in `crates/view-engine/tests/tree_file_ops_live.rs`'s
`pressing_d_on_a_file_with_a_loaded_modified_buffer_refuses_the_delete_and_records_a_notice`.

## Conclusions for the implementation

- All three chunks arrive as exactly the `kind = "confirm"` `MsgShow` +
  `CmdlineShow` pair `PromptState`'s `is_prompt` (`kind == "confirm"`
  check) and `Answer::Choices` parsing were already built to handle --
  no new redraw-event branch is needed for any of the three tree prompts.
- The *returned value* each blocked call replies with once answered
  (`vim.fn.input`'s typed string; `vim.fn.confirm`'s 1-based choice index,
  or `TREE_DELETE_CONFIRM_CHUNK`'s own `{ buffer_open = true }` short
  circuit) is not observable through `request_probe`'s fg/bg-shaped decode
  path used above, and is instead covered where it matters: live, through
  the real `tree_create_prompt`/`tree_rename_prompt`/`tree_delete_confirm`
  methods and their real `decode_prompt_reply`/`decode_delete_confirm_reply`
  decoders, driven end-to-end from a real keypress through `update()` in
  `crates/view-engine/tests/tree_file_ops_live.rs`.
- `decode_delete_confirm_reply` reads `TREE_DELETE_CONFIRM_CHUNK`'s table
  reply: `buffer_open = true` decodes to `DeleteConfirmOutcome::BufferOpen`
  before `choice` is even inspected, and otherwise `choice == 1` decodes to
  `DeleteConfirmOutcome::Confirmed`, matching `:help confirm()`'s documented
  contract directly (`1` = first button, `&Yes`); every other shape,
  including one this crate has not actually seen from the pinned engine,
  degrades to `DeleteConfirmOutcome::Declined`.
