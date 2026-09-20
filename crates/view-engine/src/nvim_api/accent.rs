//! The accent role's probe.
//!
//! `Function` and `Statement` are syntax groups, and nvim's `hl_group_set`
//! broadcast covers the UI elements only, so the two colours the accent
//! resolves from can be read no other way than by asking.

/// Reads both foregrounds in one round trip, following every link to the
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
return { ['function'] = fg('Function'), statement = fg('Statement') }\n";
