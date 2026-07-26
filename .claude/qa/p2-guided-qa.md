# P2 guided acceptance QA — ~10 minutes

Run each step in a real terminal (not tmux for the first pass; tmux is a
separate leg). Each step lists the command/keys and the exact expected
result. Anything that deviates: note the step number and what you saw —
that's a finding, regardless of how minor it feels.

Build first: `task build` (or `cargo build -p view`), then use
`target/debug/view`.

| # | Do | Expect |
|---|---|---|
| 1 | `target/debug/view ~/qa-scratch.txt` in a fresh terminal | Themed shell frame appears instantly (statusline bar + "waiting for nvim" indicator), then the real buffer within a blink. No unthemed flash. |
| 2 | Immediately after launch (fast fingers): type `ihello` | Every keystroke lands once attach completes: buffer shows `hello`, none eaten, none doubled. |
| 3 | `<Esc>` then `:` | Cmdline renders as a native bottom-row overlay (not nvim's grid cmdline). Cursor sits after the `:`. |
| 4 | `:echo "toast check"` `<CR>` | Message appears as a native toast (top-right), not on a grid message row. No "Press ENTER" prompt. |
| 5 | `:tabnew` `<CR>` | Tabline appears on row 0 with two tabs; buffer content shifts down one row, nothing overdrawn. |
| 6 | `:tabclose` `<CR>` | Tabline disappears; content reclaims the row. |
| 7 | In insert mode type `he` then `<C-n>` | Native popupmenu anchored at the cursor with completion candidates; `<C-n>`/`<C-p>` moves the selection highlight. |
| 8 | `<Esc>`, then paste a two-line string (bracketed paste from your terminal clipboard) in insert mode | Both lines land exactly; a single `u` undoes the whole paste as ONE unit (paste is not replayed as keystrokes). |
| 9 | Mouse: click a different position in the buffer | Cursor moves there. With a tabline open, click accuracy is unchanged (row offset accounted). |
| 10 | Resize the terminal window while editing | Grid reflows to the new size promptly; statusline/tabline stay on their rows; no ghost cells. |
| 11 | `:call input("name: ")` `<CR>`, type `abc` `<CR>` | Prompt-mode cmdline shows `name: abc` as you type (nothing invisible, no eaten keys). |
| 12 | `:q!` `<CR>` | view exits; terminal is fully restored: prompt at left column, colors normal, no alt-screen residue, cursor visible. |
| 13 | `target/debug/view --tier basic ~/qa-scratch.txt`, then `:q!` | Starts and edits fine with degraded caps (stderr log line before the UI says tier=Basic + override). Exit clean. |
| 14 | `echo x | target/debug/view ~/qa-scratch.txt` then `:q!` (stdin not a tty) | Starts anyway (degrades to Basic caps, no crash), exits clean. |
| 15 | Colorscheme check: in your normal nvim, note your colorscheme; run view on the same file twice | Second run's FIRST frame (before nvim finishes loading) already shows last session's colors — the theme cache at work. |

Result: reply with "QA pass" or the list of step numbers + observations.
Findings feed the phase's known-bugs/fix process like any review finding.
