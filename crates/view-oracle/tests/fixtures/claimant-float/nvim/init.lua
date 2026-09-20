-- A stand-in for the plugin class view supersedes, reduced to the two
-- things that make one: a message channel this config holds instead of the
-- engine, and a window over the message area opened after VimEnter and
-- reconfigured on a timer -- which is how a notification plugin opens and
-- slides its own boxes (`noautocmd = true`, then `nvim_win_set_config` per
-- animation frame, so no autocmd fires for the window's arrival or for any
-- step of it).
vim.notify = function(msg, level, opts)
  return msg, level, opts
end

local FLOAT_TEXT = "CLAIMANTFLOATTEXT this renderer cannot work when the "
  .. "GUI has ext_messages enabled"

-- Both markers sit well down the buffer, and the float sits below the top
-- rows: view's own claimant notice is a box across rows 0-4, and anything
-- either editor draws under it would be overpainted rather than written --
-- which would make a float that did reach the frame unreadable in the
-- stream this pin asserts against.
local function mark(text)
  local lines = {}
  for _ = 1, 8 do
    table.insert(lines, "")
  end
  table.insert(lines, text)
  vim.api.nvim_buf_set_lines(0, 0, -1, false, lines)
end

vim.api.nvim_create_autocmd("VimEnter", {
  once = true,
  callback = function()
    mark("CLAIMANTFLOATREADY")
    vim.defer_fn(function()
      local buf = vim.api.nvim_create_buf(false, true)
      vim.api.nvim_buf_set_lines(buf, 0, -1, false, { FLOAT_TEXT })
      local cols = vim.o.columns
      local win = vim.api.nvim_open_win(buf, false, {
        relative = "editor",
        anchor = "NE",
        row = 6,
        col = cols,
        width = 44,
        height = 1,
        style = "minimal",
        zindex = 50,
        noautocmd = true,
      })
      -- the slide: one `nvim_win_set_config` every 16 ms, moving the box's
      -- left edge the way the captured session's 3x3 -> 62x3 boxes move
      -- theirs. It runs long enough to outlive the take, so the step that
      -- finds the window gone is what tells the terminal it went.
      --
      -- Every frame is wide enough for the whole marker, including the
      -- first: a box whose opening frames are too narrow to hold it would
      -- leave nothing to find in the stream, and a pin that cannot fail is
      -- worse than no pin.
      local step, timer = 0, vim.uv.new_timer()
      timer:start(16, 16, vim.schedule_wrap(function()
        if not vim.api.nvim_win_is_valid(win) then
          timer:stop()
          timer:close()
          mark("CLAIMANTFLOATTAKEN")
          return
        end
        step = step + 1
        if step > 300 then
          timer:stop()
          timer:close()
          return
        end
        pcall(vim.api.nvim_win_set_config, win, {
          relative = "editor",
          anchor = "NE",
          row = 6,
          col = cols,
          width = 44 - step % 2,
          height = 1,
        })
      end))
    end, 300)
  end,
})
