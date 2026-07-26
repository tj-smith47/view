<div align="center">
  <img src="assets/view-logo.svg" alt="view" width="120" />
  <h1>view</h1>
  <p><b>A terminal-first modal editor with a modern, coherent UI, powered by an embedded, pinned Neovim.</b></p>
  <p>
    <img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-8be9fd?style=flat-square" />
    <img alt="Status: pre-alpha" src="https://img.shields.io/badge/status-pre--alpha-ffb86c?style=flat-square" />
  </p>
</div>

Your `init.lua`, plugins, LSP servers, and treesitter setup run unmodified,
because a real Neovim runs them.

**Status: pre-alpha. Not yet usable.**

## Why

- **Painless migration**: real Neovim is the engine; compatibility is total
  by construction, not reimplementation.
- **Fast where you feel it**: native Rust rendering, pickers, and UI that
  never jank on plugin Lua; measured against Neovim, budgets enforced in CI.
- **Modern out of the box**: one design system for messages, popups,
  command line, statusline, and notifications. No plugin patchwork required
  (yours still works).

## Requirements

- A terminal. Best experience on kitty, ghostty, or WezTerm; degrades
  gracefully elsewhere.

## License

MIT or Apache-2.0, at your option.
