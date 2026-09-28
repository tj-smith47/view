-- WHY: remote-editing.sh's reading of the local colorscheme, run as
-- `nvim --headless -c 'luafile ...'` under the person's own config. A
-- config may set its scheme on VimEnter or on lazy.nvim's User VeryLazy,
-- and a headless nvim raises no VeryLazy, since lazy waits for a UI to
-- attach. So the reading waits for VimEnter, raises VeryLazy where lazy is
-- loaded and has not raised it, and reads once the callbacks those events
-- scheduled have run.
--
-- Four lines out: the scheme's name, the file that defines it, lazy's
-- plugin root, and whether Normal carries a background.
local function report()
  local name = vim.g.colors_name or ""
  local file = ""
  if name ~= "" then
    file = vim.api.nvim_get_runtime_file("colors/" .. name .. ".*", false)[1] or ""
  end
  local lazy_root = ""
  if package.loaded.lazy then
    local ok, config = pcall(require, "lazy.core.config")
    if ok and config.options then
      lazy_root = config.options.root or ""
    end
  end
  local normal = vim.api.nvim_get_hl(0, { name = "Normal", link = false })
  local background = normal.bg and "opaque" or "transparent"
  io.stdout:write(table.concat({ name, file, lazy_root, background }, "\n") .. "\n")
  vim.cmd("qa!")
end

vim.api.nvim_create_autocmd("VimEnter", {
  once = true,
  callback = function()
    if package.loaded.lazy and not vim.g.did_very_lazy then
      vim.api.nvim_exec_autocmds("User", { pattern = "VeryLazy", modeline = false })
    end
    vim.defer_fn(report, 200)
  end,
})
