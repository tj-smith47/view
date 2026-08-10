# P4 guided acceptance QA — ~25 minutes

P4 ships the first user-visible native editor surface: picker, tree,
statusline, notifications and the completion palette, each supersedes-aware
of the plugin it replaces, each opt-out-able, each announced once on first
use. This pass exercises that surface directly, against a real config, not
just the harness that gates it.

Everything below runs from the repo root against `target/release/view`
unless noted. Each step lists the command/keys and the exact expected
result. Anything that deviates: note the step number and what you saw —
that is a finding, regardless of how minor it feels.

## A. The gates this phase adds

| # | Do | Expect |
|---|---|---|
| 1 | `task ci` | Green, as P3. |
| 2 | `task oracle` | Green, including the native-overlay golden entries (picker/tree/statusline/palette × basic/standard/full tiers) — 18 goldens total. |
| 3 | `task compat` | Every §13.3-named plugin scenario (lualine, noice, nvim-notify, nvim-tree/neo-tree, dressing, fidget, telescope, nvim-treesitter, nvim-cmp, mini.nvim, which-key) reports its state (native-only / superseded / deferred / present) and a verdict. |
| 4 | `VIEW_DAILY_CONFIG=~/.config/nvim task compat` | The extra fixture-less scenario runs against the maintainer's real config and passes — this is the "no fixture can hide a real-config regression" leg. |
| 5 | `task perf-audit` | Full matrix gated with native features ENABLED; the picker scenario (100k-entry match corpus, 1M-file scan) completes and gates `match_paint_p99_ms` / `first_page_p99_ms`. |

## B. Picker — supersedes telescope

| # | Do | Expect |
|---|---|---|
| 6 | Launch `view` against this repo, `<leader>ff` | A native picker overlay opens (not telescope, even if installed) listing files; typing narrows the list live. |
| 7 | `<leader>fg`, type a query that matches text in `Cargo.toml` | Grep-source picker opens; results stream in as the walk progresses rather than appearing all at once. |
| 8 | `<leader>fb` | Buffer-source picker lists open buffers only. |
| 9 | `:View picker files` (typed, not mapped) | Same picker as step 6 — the command works with the key unmapped. |
| 10 | Select an entry with `<CR>` | Picker closes, the chosen file opens, cursor lands at (1,1) or the buffer's last position — no picker chrome left behind. |
| 11 | Open the picker, press `<Esc>` | Closes cleanly; a subsequent `:reg`, `:marks`, cursor position and mode are exactly what they were before the picker opened (this is the non-interference contract, T15 step 4, observed by hand). |

## C. Tree — supersedes neo-tree/netrw

| # | Do | Expect |
|---|---|---|
| 12 | `<leader>e` | A file tree opens in a side column, rooted at cwd. |
| 13 | Navigate with `<Down>`/`<Up>`, expand a directory | Tree redraws in place; no flicker of the main buffer. While the tree has focus, keys route to view's own tree state machine rather than nvim's keymap table, so only `<Down>`/`<Up>`/`<CR>`/`<Esc>`/`a`/`r`/`d` do anything — plain `j`/`k` are silent no-ops here, unlike inside an nvim buffer. |
| 14 | `<Esc>` | Tree closes; the main window regains full width and the buffer/cursor underneath is untouched. `<leader>e` does NOT close it from within — that keystroke only reaches nvim's leader-mapped toggle when focus is on the main buffer, so pressing it while the tree has focus is silently swallowed the same as any other unrecognized key. |
| 15 | Close the tree first (`<Esc>`), then `:View tree toggle` from the main buffer | Opens the tree — the always-reachable command works standalone from the buffer side. It is an open path, not a close path: typing a `:`-command while the tree already has focus does not reach the cmdline (the same focus-routing as step 13), so do not attempt this step while the tree is already open. |

## D. Statusline — supersedes lualine

| # | Do | Expect |
|---|---|---|
| 16 | Open any file | A native statusline renders at the bottom: mode, filename, position, at minimum — no `<leader>` step needed, since `entry_keys = false` for this feature (it has no open/close bracket). |
| 17 | Switch modes (`i`, `<Esc>`, `v`) | The mode indicator updates live with each transition. |

## E. Notifications — supersedes nvim-notify/noice messages

| # | Do | Expect |
|---|---|---|
| 18 | `:echo "hi"` | A transient toast renders and expires on its own after the idle timeout — not a blocking `:messages` prompt. |
| 18a | `:bogus` (an error) | A toast renders but does NOT expire on its own — nvim's error/warning kinds (`emsg`/`echoerr`/`wmsg`/`lua_error`/`rpc_error`/`shell_err`) route Sticky (`toast.rs::route`) and stay until nvim itself clears or replaces them, matching bare nvim's own error persistence; a keypress does not dismiss it either (`a_keypress_never_dismisses_a_persistent_toast`). |
| 19 | `<leader>fm` | A message-history overlay opens, listing prior messages including the one from step 18. |
| 20 | `:View notifications history` | Same as step 19. |

## F. Palette — supersedes noice cmdline

| # | Do | Expect |
|---|---|---|
| 21 | Enter insert mode, type a few characters of a known identifier, `<C-n>` | A native completion palette renders inline (not a plugin popup) with candidates. |
| 22 | `:` (cmdline) | A native cmdline surface renders — this is the noice-cmdline supersession; typing and `<CR>`/`<Esc>` behave exactly as bare nvim's cmdline. |

## G. First-run toast and off-switches

| # | Do | Expect |
|---|---|---|
| 23 | Remove `~/.local/state/view/native-first-run.toml` (or the configured state dir), launch `view` fresh, trigger picker/tree/statusline/notifications once each | Each feature's takeover is announced exactly once, as prose naming what it took and the exact `off_switch` line to reverse it (e.g. `native.picker = false`). |
| 24 | Relaunch `view` | No re-announcement — the record persisted. |
| 25 | Add `native.picker = false` to `view.toml`, relaunch, `<leader>ff` | The key is NOT taken; whatever your own config mapped `<leader>ff` to (or nothing) is what fires; `:View picker files` still opens the picker (command stays reachable per `docs/keymaps.md`). |
| 26 | Revert `view.toml` | Picker key returns. |

## H. Reading the evidence surface

| # | Do | Expect |
|---|---|---|
| 27 | `cargo run -p view-harness --bin oracle -- page` | `docs/compat.md` regenerates; every §13.3 plugin row carries its three-state (or present-only) classification. |
| 28 | Open `docs/keymaps.md` | Matches step 6-20's live key behavior exactly — the doc is generated from `default_maps()` and a test fails if it drifts. |

Result: reply with "QA pass" or the list of step numbers + observations.
Findings feed the phase's known-bugs/fix process like any review finding.
