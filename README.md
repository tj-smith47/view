# view

A terminal-first modal editor with a modern, coherent UI — powered by an
embedded, pinned Neovim. Your `init.lua`, plugins, LSP servers, and treesitter
setup run unmodified, because a real Neovim runs them.

**Status: pre-alpha. Not yet usable.**

## Why

- **Painless migration** — real Neovim is the engine; compatibility is total
  by construction, not reimplementation.
- **Fast where you feel it** — native Rust rendering, pickers, and UI that
  never jank on plugin Lua; measured against Neovim, budgets enforced in CI.
- **Modern out of the box** — one design system for messages, popups,
  command line, statusline, and notifications. No plugin patchwork required
  (yours still works).

## Requirements

- A terminal. Best experience on kitty, ghostty, or WezTerm; degrades
  gracefully elsewhere.

## License

MIT or Apache-2.0, at your option.
