-- A stand-in for a notifier that stacks its boxes down the top-right
-- corner. It holds `vim.notify`, and once the engine has entered it opens
-- three short boxes and, below them, a taller one complaining that
-- `vim.notify` was overwritten: the stack a notifier's health check raises
-- on every engine start. The complaint lands outside the message area
-- because the three boxes above it fill the corner first.
--
-- Once that complaint is gone, a second stage opens its boxes at full width
-- in one step: a complaint wider than half the screen at the same row, and
-- a progress box at the bottom right that updates and stays open.
vim.notify = function(msg, level, opts)
  return msg, level, opts
end

local COMPLAINT = {
  "STACKCOMPLAINT vim.notify has been",
  "overwritten by another plugin?",
  "",
  "file: view (the message area)",
  "",
  "",
  "",
  "",
}

local WIDE = {
  "STACKWIDE vim.notify has been overwritten by another plugin?",
  "",
  "file: view (the message area)",
  "",
  "",
}

-- the engine's pid tells one engine's markers from the last one's, since a
-- replaced engine leaves its screen standing until the new one draws
local PID = tostring(vim.fn.getpid())

local function mark(text)
  local lines = {}
  for _ = 1, 8 do
    table.insert(lines, "")
  end
  table.insert(lines, text .. " " .. PID)
  vim.api.nvim_buf_set_lines(0, 0, -1, false, lines)
end

local function box(row, lines, width)
  local buf = vim.api.nvim_create_buf(false, true)
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
  return vim.api.nvim_open_win(buf, false, {
    relative = "editor",
    anchor = "NE",
    row = row,
    col = vim.o.columns,
    width = width or 34,
    height = #lines,
    style = "minimal",
    border = "single",
    zindex = 50,
    noautocmd = true,
  })
end

-- Polls until `complaint` is closed or a toast's own timeout passes,
-- closing whatever of `wins` still stands at the timeout, and marks which.
local function watch(complaint, wins, taken, expired, after)
  local elapsed, timer = 0, vim.uv.new_timer()
  timer:start(16, 16, vim.schedule_wrap(function()
    elapsed = elapsed + 16
    if not vim.api.nvim_win_is_valid(complaint) then
      timer:stop()
      timer:close()
      mark(taken)
      if after then
        after()
      end
      return
    end
    if elapsed >= 4000 then
      timer:stop()
      timer:close()
      for _, win in ipairs(wins) do
        pcall(vim.api.nvim_win_close, win, true)
      end
      mark(expired)
    end
  end))
end

local function wide_stage()
  local progress = box(25, { "STACKPROGRESS 1/5 " .. PID }, 30)
  local wide = box(16, WIDE, 64)
  local step, ticker = 1, vim.uv.new_timer()
  ticker:start(50, 50, vim.schedule_wrap(function()
    step = step + 1
    local buf = vim.api.nvim_win_get_buf(progress)
    vim.api.nvim_buf_set_lines(buf, 0, -1, false, {
      "STACKPROGRESS " .. step .. "/5 " .. PID,
    })
    if step >= 5 then
      ticker:stop()
      ticker:close()
    end
  end))
  watch(wide, { wide }, "WIDETAKEN", "WIDEEXPIRED")
end

vim.api.nvim_create_autocmd("VimEnter", {
  once = true,
  callback = function()
    mark("STACKREADY")
    vim.defer_fn(function()
      local wins = {
        box(1, { "", "STACKTOAST one", "" }),
        box(6, { "", "STACKTOAST two", "" }),
        box(11, { "", "STACKTOAST three", "" }),
      }
      local complaint = box(16, COMPLAINT)
      table.insert(wins, complaint)
      watch(complaint, wins, "STACKTAKEN", "STACKEXPIRED", wide_stage)
    end, 300)
  end,
})
