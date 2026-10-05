# view cheat sheet

Keys are shown as you type them. `<Space>` is your leader key, so `<Space>ff` is
"press leader, then f, then f". If your config sets a different `mapleader`, use
that in its place.

Each leader key is also a `:View` command, shown beside it. `:View` followed by
Tab lists the commands. `:map <Space>` lists the keys view registered.
`<Space>fk` opens a log of each mapping you fire. `view --print-caps`
prints what view found out about your terminal and every setting it resolved,
then exits.

| Command | What it does |
| --- | --- |
| `:View` | Puts `:View ` back on the command line, so Tab lists what to type next. |
| `:View <feature>` | Does what that feature's first key does. `:View tree` opens the tree, and `:View keys` opens the key log. |

## Starting view

| Flag | What it does |
| --- | --- |
| `--remote [USER@]HOST[:PATH]` | Runs the editor on another machine over ssh and opens PATH there. |
| `--ssh-port PORT` | Uses this ssh port for `--remote`. |
| `--ssh-opt KEY=VALUE` | Passes an ssh option through for `--remote`. You can repeat it. |
| `--nvim-bin PATH` | Uses this editor binary. With `--remote` it names the one on the other machine. |
| `--tier full`, `standard` or `basic` | Tells view what your terminal can do. View skips asking the terminal. |
| `--theme NAME` | Starts with this colorscheme. |
| `--panes tiles`, `nvim` or `auto` | Picks the window look for this session. |
| `--single-grid` | Lets the editor draw the whole window layout itself. Use it to compare a layout view draws differently. |
| `--print-caps` | Prints your terminal's features and every setting view resolved, then exits. |
| `--clean` | Starts with no `view.toml` and no `init.lua`. Every feature uses its default. |
| `--appname NAME` | Reads your editor config from `~/.config/NAME`. |
| `--config PATH` | Reads this `view.toml`. |
| `--version` | Prints the version. |

Every other argument goes to the editor as you typed it, so `+42`, `-R` and file
names work. View's own flags go first.

## Finding files and commands

| Key or command | What it does |
| --- | --- |
| `<Space>ff` (`:View picker files`) | Opens the file finder. |
| `<Space>fb` (`:View picker buffers`) | Opens the finder on your open buffers. |
| `<Space>fg` (`:View picker grep`) | Opens a live search through the text of your files. |
| `<Space><Space>` (`:View palette open`) | Opens the command line. |
| `<Space>e` (`:View tree toggle`) | Opens the file tree, jumps back into it, or closes it when you are already in it. |

While the finder is open:

| Key | What it does |
| --- | --- |
| Any character | Adds it to what you are searching for. |
| `<BS>` | Deletes the last character of the search. |
| `<Down>`, `<C-n>` or `<C-j>` | Moves to the next result. |
| `<Up>`, `<C-p>` or `<C-k>` | Moves to the previous result. |
| `<CR>` | Opens the selected result in the window you came from and closes the finder. |
| `<C-v>` | Opens it in a split beside that window. |
| `<C-x>` | Opens it in a split below that window. |
| `<C-t>` | Opens it in a new tab. |
| `<Esc>` | Closes the finder. |

A search match opens on its line. A buffer result switches to that buffer. A
title ending in `200+` means more results matched than the list holds, so narrow
the search.

While you are in the file tree:

| Key | What it does |
| --- | --- |
| `<Down>` | Moves to the next row. |
| `<Up>` | Moves to the previous row. |
| `<CR>` | Opens the file, or expands or collapses the folder. A floating tree closes as the file opens. |
| `a` | Asks for a name and creates a file. |
| `r` | Asks for a new name for the selected file. Folders are left alone. |
| `d` | Asks before it deletes the selected file. Folders are left alone. |
| `<Esc>` | Closes a floating tree. A tree in its own window stays, and you go back to the window you came from. |

## Panes and tiles

| Key or command | What it does |
| --- | --- |
| `<Space>wn` (`:View window new`) | Splits the current tile along its longer side. |
| `<Space>wz` (`:View window zoom`) | Makes the current tile fill the screen. Press it again to even the tiles out. |
| `<Space>wf` (`:View window fit`) | Sizes the tile to its longest line. |
| `<Space>ws` (`:View window flip`) | With exactly two tiles, switches them between side by side and stacked. |
| `<Space>uf` (`:View window float`) | Moves the tree, agent panel or message history you are in between a window of its own and floating over your buffers. |
| `<C-w>m` (`:View window resize_mode`) | Starts resize mode on the window or sidebar you are in. |
| `<Space>ug` (`:View ui gaps`) | Turns the gaps between tiles on or off. Works in the tiles look. |
| `<Space>uw` (`:View ui cycle_surfaces`) | Moves the tree, agent panel, palette and message history together through config, windowed and overlay placement. |
| `:View ui panes tiles` | Gives every window its own frame. |
| `:View ui panes nvim` | Draws windows the way Neovim does. |
| `:View ui panes auto` | Uses the Neovim look under a tiling window manager and tiles everywhere else. |
| `:View ui panes` | Tells you which look is on, what decided it, and who draws the top row. |
| `<Space>w1` (`:View window to_tabpage_1`) | Moves the current window to tab page 1. |
| `<Space>w2` (`:View window to_tabpage_2`) | Moves the current window to tab page 2. |
| `<Space>w3` (`:View window to_tabpage_3`) | Moves the current window to tab page 3. |
| `<Space>w4` (`:View window to_tabpage_4`) | Moves the current window to tab page 4. |
| `<Space>w5` (`:View window to_tabpage_5`) | Moves the current window to tab page 5. |
| `<Space>w6` (`:View window to_tabpage_6`) | Moves the current window to tab page 6. |
| `<Space>w7` (`:View window to_tabpage_7`) | Moves the current window to tab page 7. |
| `<Space>w8` (`:View window to_tabpage_8`) | Moves the current window to tab page 8. |
| `<Space>w9` (`:View window to_tabpage_9`) | Moves the current window to tab page 9. |

While a sidebar (the tree, the agent panel, the message history in its own
window) has the keyboard:

| Key | What it does |
| --- | --- |
| `<S-Right>` or `<C-w>>` | Makes the sidebar one notch wider (5% of the screen). |
| `<S-Left>` or `<C-w><` | Makes the sidebar one notch narrower. |
| `<C-w>m` | Starts resize mode on the sidebar. |

In resize mode the status line shows `RESIZE`. A count before the key repeats
it, so `3l` is three steps wider. A step is 5% of the screen.

| Key | What it does |
| --- | --- |
| `h` or `<Left>` | Narrower. |
| `l` or `<Right>` | Wider. |
| `j` or `<Down>` | Shorter. |
| `k` or `<Up>` | Taller. |
| `=` | Makes every window the same size. |
| `<Esc>`, `<CR>` or `q` | Leaves resize mode. |
| Any other key | Leaves resize mode and then does what it always does. |

### Desktop chords

These work on top of the keys above. They are on by default over ssh and on a
Linux console with no desktop running. They are off by default on a graphical
desktop, on macOS and on Windows. `[keys] profile = "desktop"` turns them on and
`"editor"` turns them off. `:View keys profile` tells you which is on. Press the
Super form where your terminal speaks the kitty keyboard protocol, and the Alt
form everywhere else.

| Key | What it does |
| --- | --- |
| `<D-Left>` or `<M-Left>` | Moves to the window on the left. |
| `<D-Right>` or `<M-Right>` | Moves to the window on the right. |
| `<D-Up>` or `<M-Up>` | Moves to the window above. |
| `<D-Down>` or `<M-Down>` | Moves to the window below. |
| `<S-D-Left>` or `<S-M-Left>` | Moves the window to the far left. |
| `<S-D-Right>` or `<S-M-Right>` | Moves the window to the far right. |
| `<S-D-Up>` or `<S-M-Up>` | Moves the window to the top. |
| `<S-D-Down>` or `<S-M-Down>` | Moves the window to the bottom. |
| `<D-w>` or `<M-w>` | Closes the window. |
| `<D-q>` or `<M-q>` | Closes the window. |
| `<D-CR>` or `<M-CR>` | Splits the current tile along its longer side. |
| `<D-f>` or `<M-f>` | Makes the tile fill the screen, and evens the tiles out on the next press. |
| `<M-D-f>` or `<C-M-f>` | Makes the window as wide as it can be. |
| `<D-Home>` or `<M-Home>` | Sizes the tile to its longest line. |
| `<D-j>` or `<M-j>` | With exactly two tiles, switches them between side by side and stacked. |
| `<D-t>` or `<M-t>` | Moves the panel you are in between a window of its own and floating. |
| `<D-Space>` or `<M-Space>` | Opens the command line. |
| `<M-D-Space>` or `<C-M-Space>` | Opens the file finder. |
| `<S-D-f>` or `<M-F>` | Opens the file tree, jumps back into it, or closes it. |
| `<D-1>` or `<M-1>` | Goes to tab page 1. |
| `<D-2>` or `<M-2>` | Goes to tab page 2. |
| `<D-3>` or `<M-3>` | Goes to tab page 3. |
| `<D-4>` or `<M-4>` | Goes to tab page 4. |
| `<D-5>` or `<M-5>` | Goes to tab page 5. |
| `<D-6>` or `<M-6>` | Goes to tab page 6. |
| `<D-7>` or `<M-7>` | Goes to tab page 7. |
| `<D-8>` or `<M-8>` | Goes to tab page 8. |
| `<D-9>` or `<M-9>` | Goes to tab page 9. |
| `<S-D-1>` or `<M-!>` | Moves the window to tab page 1. |
| `<S-D-2>` or `<M-@>` | Moves the window to tab page 2. |
| `<S-D-3>` or `<M-#>` | Moves the window to tab page 3. |
| `<S-D-4>` or `<M-$>` | Moves the window to tab page 4. |
| `<S-D-5>` or `<M-%>` | Moves the window to tab page 5. |
| `<S-D-6>` or `<M-^>` | Moves the window to tab page 6. |
| `<S-D-7>` or `<M-&>` | Moves the window to tab page 7. |
| `<S-D-8>` or `<M-*>` | Moves the window to tab page 8. |
| `<S-D-9>` or `<M-(>` | Moves the window to tab page 9. |
| `<D-Tab>` or `<M-Tab>` | Goes to the next tab page. |
| `<S-D-Tab>` or `<S-M-Tab>` | Goes to the previous tab page. |
| `<D-->` or `<M-->` | Makes the window narrower. |
| `<D-=>` or `<M-=>` | Makes the window wider. |
| `<S-D-->` or `<M-_>` | Makes the window shorter. |
| `<S-D-=>` or `<M-+>` | Makes the window taller. |
| `<S-D-BS>` or `<C-M-g>` | Turns the gaps between tiles on or off. |
| `<S-M-D-,>` or `<C-M-n>` | Opens or closes the message history. |
| `<D-,>` or `<M-,>` | Takes down the newest notification. |
| `<C-S-D-a>` or `<C-M-a>` | Opens the agent panel, jumps back into it, or closes it. |

## The agent panel

`[ai] enabled = false` turns the panel and its keys off. The first time you open
the panel in a project, view asks whether you trust that project.

| Key or command | What it does |
| --- | --- |
| `<Space>ai` (`:View ai toggle`) | Opens the panel with the cursor in the prompt. Closes it when you are in it. Takes you back into it when it is open and you are not. |
| `:View ai open` | Opens the panel and puts you in it. |
| `:View ai focus` | Puts you in the panel, opening it first if needed. |
| `:View ai close` | Closes the panel and leaves the agent session running. |
| `:View ai dismiss` | Clears the crash banner. |

While you are in the panel:

| Key | What it does |
| --- | --- |
| Any character | Types it into the prompt. |
| `<BS>` | Deletes the last character of the prompt. |
| `<CR>` | Sends the prompt. Nothing is sent while the agent is still answering. |
| `<M-CR>` | Starts a new line in the prompt. |
| `<S-CR>` | Starts a new line in the prompt (needs the kitty keyboard protocol). |
| `<C-c>` | Stops the agent's answer. |
| `<PageUp>` or `<PageDown>` | Scrolls the transcript a full window. |
| `<C-u>` or `<C-d>` | Scrolls the transcript half a window. `<C-d>` also clears the crash banner while one is showing. |
| `<Esc>` | Steps out of the panel and leaves it on screen. |

While the agent is asking permission:

| Key | What it does |
| --- | --- |
| `1` to `9` | Picks the option on that row. |
| `<Esc>` | Cancels the request. |
| `<C-c>` | Stops the agent's answer. |

While you review an agent's edit (these work only in the reviewed file):

| Key or command | What it does |
| --- | --- |
| `<Space>ha` (`:View review accept`) | Accepts the hunk under the cursor. |
| `<Space>hA` (`:View review accept_all`) | Accepts every hunk still fresh, as one write. |
| `<Space>hx` (`:View review reject`) | Rejects the hunk under the cursor. |
| `:View review reject_all` | Rejects the whole proposal. It has no key. |
| `<Space>hR` (`:View review rediff`) | Re-anchors a hunk your own edit moved. |
| `<Space>hq` (`:View review leave`) | Leaves the review and decides nothing further. |
| `]c` (`:View review next`) | Jumps to the next hunk awaiting a decision. |
| `[c` (`:View review prev`) | Jumps to the previous hunk awaiting a decision. |

## Messages

| Key or command | What it does |
| --- | --- |
| `<Space>fm` (`:View notifications history`) | Opens or closes the list of everything view has said this session. |
| `<Space>fp` (`:View notifications pause`) | Freezes the notification stack, and unfreezes it. |
| `<Space>fd` (`:View notifications dismiss`) | Takes down the newest notification. |
| `<Esc>` | Takes down a standing error or warning from the editor. The key still does what it does in Neovim. |

While the message history is open:

| Key | What it does |
| --- | --- |
| `j` | Selects the next entry. |
| `k` | Selects the previous entry. |
| `<C-d>` | Selects half a screen further down. |
| `<C-u>` | Selects half a screen further up. |
| `gg` | Selects the newest entry. |
| `G` | Selects the oldest entry. |
| `y` | Copies the selected entry to your clipboard. |
| `d` | Takes down the standing notice the entry belongs to. Messages from the editor have none. |
| `<Esc>` | Closes the history. In a window of its own, it takes you back to the window you came from. |

## When the editor stops answering

A banner names how long the editor has been silent. These keys answer it.

| Key | What it does |
| --- | --- |
| `<C-c>` | Interrupts the editor and keeps your session. |
| `<F5>` | Restarts the editor and reopens your files with their unsaved text. |
| `<Esc>` | Puts the box away and leaves the editor to finish. |
| `<C-q>` | Quits view. It is offered once the editor has stopped running. |

## The key helper

| Key or command | What it does |
| --- | --- |
| `<Space>fk` (`:View keys log`) | Opens or closes a log of each mapping you fire: the key, whose mapping it is, and what it took over. |
| `:View keys focus` | Opens the log if needed and hands it your keys so you can scroll it. |
| `:View keys profile` | Tells you which key profile is on and why. Add `desktop`, `editor` or `auto` to switch it for this session. |

While the log has your keys:

| Key | What it does |
| --- | --- |
| `j` | Selects the next older mapping. |
| `k` | Selects the next newer mapping. |
| `<C-d>` | Selects half a screen further down. |
| `<C-u>` | Selects half a screen further up. |
| `gg` | Selects the newest mapping. |
| `G` | Selects the oldest mapping. |
| `y` | Copies the selected row to your clipboard. |
| `<Esc>` | Closes the log. |

## The session recorder

Needs `[dvr] enabled = true` in `view.toml`.

| Key or command | What it does |
| --- | --- |
| `<Space>fv` (`:View dvr scrub`) | Freezes the screen on the last frame and opens the scrub bar. |
| `q` or `<Esc>` (`:View dvr close`) | Closes the scrub and returns to the live screen. |
| `:View dvr export` | Writes the recording to `view-dvr-<seconds>.vdvr` in the directory view started in, on the machine view runs on, including under `--remote`. Add a path to write there. The clip holds the screen as it was shown and nothing you typed. |
| `:View dvr play PATH` | Opens a saved clip in the scrub. `b` and `e` close the clip. `q` returns to your session. |

While you are scrubbing:

| Key | What it does |
| --- | --- |
| `h` | One frame back. |
| `l` | One frame forward. |
| `H` | One second back. |
| `L` | One second forward. |
| `g` | Jumps to the oldest frame kept. |
| `G` | Jumps to the newest frame. |
| `q` | Returns to the live screen. |
| `<Esc>` | Returns to the live screen. |
| `b` | Replaces the editor with a fresh one brought to this frame, after you confirm. |
| `e` | Writes the recording to a clip file, then returns to the live screen. |

When view asks whether to branch:

| Key | What it does |
| --- | --- |
| `y` or `<CR>` | Branches from the frame. |
| `n` or `<Esc>` | Returns to the live screen. |

## Clipboard

view adds no clipboard keys of its own. `y` copies the selected row inside the
message history and the key log, as listed above.

## Everything else

view adds no other keys or `:View` commands. Keys view does not own go to Neovim
under Neovim's own names, so your mappings fire as they do in Neovim.
