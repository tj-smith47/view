-- Whether a float announces its own closing, in the three contexts the
-- close can be called from.
--
-- Run against the pinned engine with no plugins and no config at all:
--
--   nvim --headless --clean \
--     -c 'luafile scripts/acceptance/winclosed-autocmd-nesting.lua' -c 'qa!'
--
-- The question it settles: nvim-cmp's menu windows disappear without a
-- WinClosed, while nvim-notify's, noice's and telescope's do fire one, and
-- all four open with noautocmd = true. If the engine suppressed WinClosed
-- on a noautocmd window the absence would be an engine rule and every
-- plugin would be silent; it does not, so the difference has to be in
-- where the close is called from.

local api = vim.api
local log = {}

api.nvim_create_autocmd({ "WinNew", "WinClosed" }, {
  callback = function(args)
    log[#log + 1] = args.event .. ":" .. tostring(args.match)
  end,
})

local buf = api.nvim_create_buf(false, true)

local function float(row)
  return api.nvim_open_win(buf, false, {
    relative = "editor",
    row = row,
    col = 1,
    width = 5,
    height = 1,
    noautocmd = true,
  })
end

-- 0: the opening control. Every float below is opened with
-- noautocmd = true, as all four captured plugins open theirs; this one is
-- not, so the WinNew it raises is what proves the others' silence is the
-- flag and not a missing listener.
local announced = api.nvim_open_win(buf, false, {
  relative = "editor",
  row = 7,
  col = 1,
  width = 5,
  height = 1,
})

-- 1: closed from the top level, the control.
local top = float(1)
api.nvim_win_hide(top)

-- 2: closed from inside an autocmd callback registered without `nested`,
-- which is how nvim-cmp registers every event it listens on.
local plain = float(3)
local group = api.nvim_create_augroup("winclosed_nesting", { clear = true })
api.nvim_create_autocmd("User", {
  group = group,
  pattern = "PlainClose",
  callback = function()
    api.nvim_win_hide(plain)
  end,
})
log[#log + 1] = "|plain|"
api.nvim_exec_autocmds("User", { pattern = "PlainClose" })

-- 3: the same call from a callback registered with `nested = true`.
local nested = float(5)
api.nvim_create_autocmd("User", {
  group = group,
  pattern = "NestedClose",
  nested = true,
  callback = function()
    api.nvim_win_hide(nested)
  end,
})
log[#log + 1] = "|nested|"
api.nvim_exec_autocmds("User", { pattern = "NestedClose" })

print(string.format(
  "announced=%d top=%d plain=%d nested=%d",
  announced,
  top,
  plain,
  nested
))
print("events: " .. table.concat(log, " "))
