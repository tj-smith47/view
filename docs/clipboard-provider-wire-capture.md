# Wire capture: `g:clipboard` provider contract

Captured live against the pinned engine per "capture, never recall." Source
of truth for the clipboard provider implementation.

## Engine identity

```
$ nvim --version | head -3
NVIM v0.12.4
Build type: Release
LuaJIT 2.1.1785192264
```

```
$ nvim --api-info | python3 -c "import msgpack,sys; print(msgpack.unpackb(sys.stdin.buffer.read(), raw=False)['version'])"
{'major': 0, 'minor': 12, 'patch': 4, 'prerelease': False, 'api_level': 14, 'api_compatible': 0, 'api_prerelease': False, 'build': None}
```

Matches `.engine-pin` (`v0.12.4`) exactly.

## `:help g:clipboard` (verbatim, from `provider.txt`)

Captured via `nvim --headless -c "help g:clipboard" -c "write! <out>" -c "qa!"`.

```
Clipboard integration			      *provider-clipboard* *clipboard*

Nvim has no direct connection to the system clipboard. Instead it depends on
a |provider| which transparently uses shell commands to communicate with the
system clipboard or any other clipboard "backend".

To ALWAYS use the clipboard for ALL operations (instead of interacting with
the "+" and/or "*" registers explicitly): >vim
    set clipboard+=unnamedplus

See 'clipboard' for details and options.

							      *clipboard-tool*
The presence of a working clipboard tool implicitly enables the "+" and "*"
registers. Nvim supports these clipboard tools, in order of priority:

- |g:clipboard| : User override (if set to a dict or any string "name" below;
  e.g. `g:clipboard="tmux"` forces tmux clipboard and skips auto-detection).
- "pbcopy"    : pbcopy, pbpaste (macOS)
- "wl-copy"   : wl-copy, wl-paste (if $WAYLAND_DISPLAY is set)
- "wayclip"   : waycopy, waypaste (if $WAYLAND_DISPLAY is set)
- "xsel"      : xsel (if $DISPLAY is set)
- "xclip"     : xclip (if $DISPLAY is set)
- "lemonade"  : lemonade (for SSH) https://github.com/pocke/lemonade
- "doitclient": doitclient (for SSH) https://www.chiark.greenend.org.uk/~sgtatham/doit/
- "win32yank" : *win32yank* (Windows)
- "putclip"   : putclip, getclip (Windows) https://cygwin.com/packages/summary/cygutils.html
- "clip"      : clip, powershell (Windows) https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/clip
- "termux"    : termux (via termux-clipboard-set, termux-clipboard-get)
- "tmux"      : tmux (if $TMUX is set)
- "osc52"     : |clipboard-osc52| (if supported by your terminal)

								 *g:clipboard*
To configure a custom clipboard tool, set `g:clipboard` to a string name (from
the above |clipboard-tool| list), or dict (to explicitly specify the shell
commands or |lambda| functions) before clipboard providers are initialized.

Note: Clipboard providers are initialized by calling `has('clipboard')`, which
is best avoided before setting `g:clipboard`. See "G:CLIPBOARD SETTINGS ARE NOT
USED" in |faq-runtime| for workarounds.

If "cache_enabled" is |TRUE| then when a selection is copied Nvim will cache
the selection until the copy command process dies. When pasting, if the copy
process has not died the cached selection is applied.

The "copy" function stores a list of lines and the register type. The "paste"
function returns the clipboard as a `[lines, regtype]` list, where `lines` is
a list of lines and `regtype` is a register type conforming to |setreg()|.
```

The OSC52 section (same page) documents the bundled reference provider:

```
							    *clipboard-osc52*
Nvim bundles a clipboard provider that allows copying to the system clipboard
using OSC 52, an "Operating System Command" control-sequence that causes the
terminal emulator to write to or read from the system clipboard.

When Nvim is running in the |TUI|, it automatically detects host terminal
support for OSC 52. If successful, then Nvim will use OSC 52 for copying and
pasting if no other |clipboard-tool| is found and when 'clipboard' is unset.
NOTE: Using a terminal multiplexer (e.g. tmux) may inhibit automatic OSC 52
support detection.
```

## Reference implementation cross-check: `lua/vim/ui/clipboard/osc52.lua` (bundled)

Read directly from the pinned install
(`$(brew --prefix)/Cellar/neovim/0.12.4/share/nvim/runtime/lua/vim/ui/clipboard/osc52.lua`).
`M.paste(reg)` returns a *bare* list of lines (`vim.split(contents, '\n')`),
**not** a `[lines, regtype]` pair: narrower than the documented contract.

## Reference implementation cross-check: `autoload/provider/clipboard.vim` (bundled)

`s:clipboard.get(a:reg)` invokes the `paste` Funcref and passes its return
value straight through as `clipboard_data` with no repackaging, so both
shapes reach the C-level register setter unmodified, whichever the paste
Funcref chooses to return.

## Empirical resolution (headless pinned nvim, `nvim --headless --clean -l <script>.lua`)

The doc text and the bundled OSC52 example disagree on the paste return
shape. Resolved empirically against the pinned binary rather than guessed:

**1. Bare list of lines (no regtype) is accepted:**

```lua
vim.g.clipboard = {
  name = 'test-plain',
  copy = { ['+'] = function(lines, regtype) end, ['*'] = function(lines, regtype) end },
  paste = { ['+'] = function() return {'hello', 'world'} end, ['*'] = function() return {'hello', 'world'} end },
  cache_enabled = 0,
}
```
```
plain-list getreg ok=true res="hello\nworld"
regtype=v
```
A bare list is accepted; register type defaults to charwise (`v`).

**2. `[lines, regtype]` pair form is also accepted and respects the regtype:**

```lua
paste = { ['+'] = function() return {{'a', 'b'}, 'V'} end },
```
```
pair-form getreg ok=true res="a\nb\n"
regtype=V
```

**3. Both `'+'` and `'*'` keys are required in `copy`/`paste`: omitting one
breaks that register:**

```lua
vim.g.clipboard = {
  name = 'test-pair-and-plus-only',
  copy = { ['+'] = function(lines, regtype) end },   -- '*' omitted
  paste = { ['+'] = function() return {{'a', 'b'}, 'V'} end },
  cache_enabled = 0,
}
```
```
star-without-key ok=false res="Vim:clipboard: provider returned invalid data"
```

## Conclusions for the implementation

- `g:clipboard.paste['+']`/`['*']` may return either a bare list of lines
  (regtype defaults to charwise `v`) or a `[lines, regtype]` pair. view's
  injected paste closure uses the `[lines, regtype]` pair form so
  linewise/charwise fidelity round-trips through `"+yy`/`"+p`.
- `copy`/`paste` dicts must define **both** `'+'` and `'*'` keys or the
  omitted register errors on every access. view wires both registers to the
  same clipboard backend (arboard has one system clipboard; there is no
  macOS/Windows equivalent of the X11 primary selection, and wiring both to
  the same store matches the bundled OSC52 provider's own behavior).
- `cache_enabled = 0` is required. Option B (read-at-paste-time, never
  cached) is the chosen design; caching would reintroduce the stale-read bug
  Option A was rejected for.
