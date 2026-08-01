-- The compat harness's LazyVim-style fixture: a real lazy.nvim-managed
-- plugin stack, pinned by the committed lazy-lock.json next to this file.
-- Carries the compat matrix's named plugin floor across all three
-- config-reconciliation classes in one shared stack rather than one
-- fixture per plugin, so every scenario also runs against the
-- cross-plugin interactions (noice rerouting messages through
-- nvim-notify, for instance) a real plugin-heavy config has.
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
    {
      "folke/noice.nvim",
      dependencies = { "MunifTanjim/nui.nvim" },
      -- view attaches to nvim with ext_cmdline/ext_messages/ext_popupmenu
      -- externalized. Disabling those three noice components is noice's own
      -- supported configuration for such a GUI, and its message router (the
      -- vim.notify path, patched in by noice's deferred M.enable()) does
      -- honor it -- that's what the noice scenario's "routed" assertion
      -- proves. But noice.health.check() (lua/noice/health.lua) runs
      -- synchronously from noice's setup(), before that setup call has
      -- parsed these very opts into noice's Config.options (still `{}` at
      -- that point -- read from noice's own source, lua/noice/config/init.lua
      -- populates it only inside the *deferred* load() this setup call
      -- schedules). health.check() unconditionally raises one ERROR
      -- notification per ext_cmdline/ext_popupmenu/ext_messages any attached
      -- UI has enabled, with no regard for the disabled-components config
      -- above -- confirmed live (compat repro) and upstream
      -- (github.com/folke/noice.nvim#1137, closed stale, unfixed, present on
      -- current main as of this fixture's pin). No noice option gates this
      -- check, so the opts table alone can never silence it; stubbing
      -- health.check for this fixture's own noice load is the only way to
      -- keep the compat evidence free of it. Left in place for the whole
      -- session (not restored after setup): health.check() also reruns on a
      -- 1s interval for as long as noice is enabled
      -- (Health.checker = Util.interval(1000, ...)), so restoring the real
      -- implementation right after setup() would only delay the same
      -- spurious notifications by about a second, not remove them.
      config = function(_, opts)
        local health = require("noice.health")
        health.check = function()
          return true
        end
        require("noice").setup(opts)
      end,
      opts = {
        cmdline = { enabled = false },
        messages = { enabled = false },
        popupmenu = { enabled = false },
      },
    },
    -- Both tree plugins default to hijacking netrw via
    -- `silent! autocmd! FileExplorer *`, which runs before netrw's own
    -- plugin phase has created that augroup: `silent!` hides the E216 but
    -- still writes it into v:errmsg, where the harness's zero-error
    -- epilogue reads it. Hijacking netrw is irrelevant to what these rows
    -- assert (sidebar rendering), so both hijacks stay off.
    { "nvim-tree/nvim-tree.lua", opts = { hijack_netrw = false } },
    {
      "nvim-neo-tree/neo-tree.nvim",
      dependencies = { "nvim-lua/plenary.nvim", "MunifTanjim/nui.nvim" },
      opts = { filesystem = { hijack_netrw_behavior = "disabled" } },
    },
    { "j-hui/fidget.nvim", opts = {} },
    {
      "folke/which-key.nvim",
      opts = {},
      config = function(_, opts)
        local wk = require("which-key")
        wk.setup(opts)
        -- Registered through which-key itself so the mapping (and its
        -- popup description, the scenario's grid marker) exists exactly
        -- when the plugin does. Lives under the default backslash leader.
        wk.add({
          { "<leader>m", "<cmd>echo 'whichkey-target'<CR>", desc = "whichkey-compat-marker" },
        })
      end,
    },
    { "nvim-lua/plenary.nvim" },
    {
      "nvim-telescope/telescope.nvim",
      dependencies = { "nvim-lua/plenary.nvim" },
      opts = {},
    },
    {
      "nvim-treesitter/nvim-treesitter",
      branch = "master",
      config = function()
        -- ensure_installed pins the parser set so scenario assertions
        -- are deterministic; the one-time parser compile lands in the
        -- shared plugin cache next to the plugin itself.
        require("nvim-treesitter.configs").setup({
          ensure_installed = { "lua" },
          sync_install = false,
          highlight = { enable = true },
        })
      end,
    },
    {
      "hrsh7th/nvim-cmp",
      dependencies = { "hrsh7th/cmp-buffer" },
      config = function()
        -- The buffer source is the one completion source whose candidate
        -- set a scenario fully controls: whatever words the scenario
        -- itself typed into the buffer.
        local cmp = require("cmp")
        cmp.setup({ sources = { { name = "buffer" } } })
      end,
    },
    {
      "echasnovski/mini.nvim",
      config = function()
        -- One module with a deterministic, buffer-local observable
        -- effect stands in for the library: typing an opening paren in
        -- insert mode yields a balanced pair.
        require("mini.pairs").setup()
      end,
    },
  },
  lockfile = vim.fn.stdpath("config") .. "/lazy-lock.json",
  install = { missing = true },
  checker = { enabled = false },
  change_detection = { enabled = false },
  ui = { border = "none" },
})
