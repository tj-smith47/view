-- A fixture whose whole purpose is that this table is already set before
-- view's own take-over ever runs: both '+' and '*' keys are populated
-- (the wire-capture doc's "both keys required" finding), and
-- cache_enabled matches what view's own provider sets, so the only
-- observable difference between this provider and view's own is the
-- 'name' field the clipboard-precedence scenario probes.
vim.g.clipboard = {
  name = 'user-provider',
  copy = {
    ['+'] = function(_lines, _regtype) end,
    ['*'] = function(_lines, _regtype) end,
  },
  paste = {
    ['+'] = function() return {} end,
    ['*'] = function() return {} end,
  },
  cache_enabled = 0,
}

vim.fn.serverstart(vim.env.VIEW_COMPAT_SOCK)
