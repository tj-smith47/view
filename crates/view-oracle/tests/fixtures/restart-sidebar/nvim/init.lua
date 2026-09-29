-- A config that reopens its file tree the way a plugin manager's startup
-- event does: after the attach has drawn the file alone. The delay stands
-- in for the plugins that load first, and is long enough that the file's
-- frame without the tree reaches the terminal on any host. The tree's line
-- carries the engine's pid, so the rows a restart holds read apart from
-- the ones the replacement draws.
vim.bo.buftype = "nofile"
vim.bo.filetype = "restartmain"
vim.api.nvim_buf_set_lines(0, 0, -1, false, { "RESTARTMAINFILE" })

vim.api.nvim_create_autocmd("UIEnter", {
  once = true,
  callback = function()
    vim.defer_fn(function()
      local main = vim.api.nvim_get_current_win()
      vim.cmd("topleft vnew")
      vim.bo.buftype = "nofile"
      vim.bo.filetype = "restarttree"
      vim.api.nvim_buf_set_lines(0, 0, -1, false,
        { "RESTARTSIDEBAR " .. vim.fn.getpid() })
      vim.cmd("vertical resize 24")
      vim.wo.winfixwidth = true
      vim.api.nvim_set_current_win(main)
    end, 400)
  end,
})
