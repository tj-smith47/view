-- The compat harness's LazyVim-style fixture: a real lazy.nvim-managed
-- plugin stack, pinned by the committed lazy-lock.json next to this file.
-- Slice of the design spec's own compat matrix chosen to cover both named
-- classes without pulling in the full plugin matrix (that is future work's
-- job, filling the matrix this harness drives): lualine, nvim-notify, and
-- dressing are ui-owning; telescope, nvim-treesitter, and nvim-cmp are
-- semantic.
--
-- Plugin installs land under `stdpath("data") .. "/lazy"`, which the
-- driver (crates/view-harness/src/bin/oracle.rs's compat_command) points
-- at a cache directory keyed by this file's own lazy-lock.json hash via
-- XDG_DATA_HOME, not anything this file sets itself: lazy.nvim's own
-- "already installed? skip; else clone" check on that path is the entire
-- "network on first run only" property, no bespoke caching logic needed.
vim.fn.serverstart(vim.env.VIEW_COMPAT_SOCK)

local uv = vim.uv or vim.loop
local lazypath = vim.fn.stdpath("data") .. "/lazy/lazy.nvim"
if not uv.fs_stat(lazypath) then
  vim.api.nvim_echo({ { "compat: cloning lazy.nvim\n", "DiagnosticInfo" } }, true, {})
  local ok, out = pcall(vim.fn.system, {
    "git",
    "clone",
    "--filter=blob:none",
    "https://github.com/folke/lazy.nvim.git",
    lazypath,
  })
  if not ok or vim.v.shell_error ~= 0 then
    vim.api.nvim_echo({
      { "compat: failed to clone lazy.nvim\n", "ErrorMsg" },
      { vim.trim(out or ""), "WarningMsg" },
    }, true, {})
  end
end
vim.opt.rtp:prepend(lazypath)

-- A cold install pops lazy.nvim's own floating status window and leaves it
-- open (and focused) for a human to review and dismiss. This harness has
-- no human at the keyboard: an open, non-modifiable Lazy window steals
-- every subsequent scripted keystroke instead of reaching the real buffer
-- (observed live: "E21: Cannot make changes, 'modifiable' is off" on the
-- first scenario step after a genuinely cold install). lazy.nvim fires
-- LazyDone synchronously at the tail of its own setup() call, after any
-- `wait = true` install already finished (lazy.nvim's core/loader.lua),
-- so this autocmd must be registered before calling setup() below -- after
-- it returns, the event has already fired and a listener added afterward
-- never sees it (verified against the vendored lazy.nvim source; this was
-- the actual bug in an earlier version of this fixture that registered the
-- autocmd after setup() and never saw the window close).
vim.api.nvim_create_autocmd("User", {
  pattern = "LazyDone",
  callback = function()
    for _, win in ipairs(vim.api.nvim_list_wins()) do
      local buf = vim.api.nvim_win_get_buf(win)
      if vim.bo[buf].filetype == "lazy" then
        vim.api.nvim_win_close(win, true)
      end
    end
  end,
})

require("lazy").setup({
  -- No custom `name =` aliases: lazy.nvim's default name (the repo name,
  -- e.g. "lualine.nvim") is what a `dependencies` reference must match too;
  -- an alias on one spec entry but not its dependency reference silently
  -- produces two separate clones of the same plugin under two different
  -- lazy-managed directories.
  spec = {
    { "nvim-lualine/lualine.nvim", opts = {} },
    { "rcarriga/nvim-notify", opts = {} },
    { "stevearc/dressing.nvim", opts = {} },
    { "nvim-lua/plenary.nvim" },
    {
      "nvim-telescope/telescope.nvim",
      dependencies = { "nvim-lua/plenary.nvim" },
      opts = {},
    },
    { "nvim-treesitter/nvim-treesitter", branch = "master" },
    { "hrsh7th/nvim-cmp" },
  },
  lockfile = vim.fn.stdpath("config") .. "/lazy-lock.json",
  install = { missing = true },
  checker = { enabled = false },
  change_detection = { enabled = false },
  ui = { border = "none" },
})
