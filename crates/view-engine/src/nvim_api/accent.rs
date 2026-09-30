//! The accent role's probe, which also reads the notice levels' colours
//! and the syntax colours a tree row's git glyphs take.
//!
//! nvim's `hl_group_set` broadcast covers the builtin UI groups alone.
//! `Function`, `Statement`, `Constant`, `PreProc` and `Comment` are syntax
//! groups and the `Diagnostic*` groups are none of the UI ones, so these
//! colours are read by asking.

/// Reads every foreground in one round trip, following every link to the
/// group that actually carries the colour.
///
/// A missing key is a colorscheme that sets no foreground for that group,
/// which the reply decodes as `None` and the theme answers by falling
/// through to the next colour in its order.
pub(super) const ACCENT_PROBE_CHUNK: &str = "\
local function fg(name)\n\
  local ok, hl = pcall(vim.api.nvim_get_hl, 0, { name = name, link = false })\n\
  if not ok then return nil end\n\
  return hl and hl.fg or nil\n\
end\n\
return {\n\
  ['function'] = fg('Function'),\n\
  statement = fg('Statement'),\n\
  error = fg('DiagnosticError'),\n\
  warn = fg('DiagnosticWarn'),\n\
  info = fg('DiagnosticInfo'),\n\
  hint = fg('DiagnosticHint'),\n\
  constant = fg('Constant'),\n\
  preproc = fg('PreProc'),\n\
  comment = fg('Comment'),\n\
}\n";
