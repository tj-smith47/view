-- Capture chunk for the floats plugins draw over surfaces view owns.
--
-- Loaded with `dofile()` through the compat harness probe channel into a
-- live session (view attached, heavy fixture), it arms an autocmd recorder
-- and publishes the snapshot entry points the scenario beside it calls
-- between keystrokes. Every value written comes from the running session
-- (nvim_win_get_config, the buffer, the extmark namespaces), never from a
-- plugin's source.
--
-- The record goes to a file rather than to the probe reply: a reply is one
-- trimmed line and this record is many, so the entry points return only a
-- short status a scenario step can assert on.
--
-- That file lands under the repo's own target/ directory, found by walking
-- up from the session's cwd. A compat session's environment is swept down
-- to an allowlist before it is spawned (view-oracle's make_hermetic), so a
-- variable naming the path never survives the spawn, and every other
-- directory the session can see is a per-run scratch world that is removed
-- when the run ends. $VIEW_FLOAT_CAPTURE_OUT still wins where it does
-- survive, which is any caller that is not a hermetic pty spawn.
--
-- Timings are read from vim.uv.hrtime() inside the session, so the
-- keystroke-to-reconfiguration interval never carries the probe
-- subprocess's own round trip.

local uv = vim.uv or vim.loop
local api = vim.api

local function repo_root()
  local found = vim.fs.find("scripts", {
    upward = true,
    type = "directory",
    path = vim.fn.getcwd(),
    limit = 1,
  })[1]
  return found and vim.fs.dirname(found) or nil
end

local out_path = vim.env.VIEW_FLOAT_CAPTURE_OUT
if not out_path then
  local root = repo_root() or vim.fn.stdpath("cache")
  vim.fn.mkdir(root .. "/target", "p")
  out_path = root .. "/target/surface-float-capture.txt"
end

local M = {}
_G.VFC = M

-- Reopened per line rather than held: a session killed mid-capture (a
-- scenario timeout, a plugin wedge) still leaves every line written so far
-- on disk instead of an empty buffered file.
local function w(text)
  local f = io.open(out_path, "a")
  if not f then
    return
  end
  f:write(text .. "\n")
  f:close()
end

local function ms(ns)
  return string.format("%.3f", ns / 1e6)
end

local armed_at = uv.hrtime()
local events = {}
local drained = 0
local hide_state = nil

local function float_wins()
  local list = {}
  for _, win in ipairs(api.nvim_list_wins()) do
    local ok, cfg = pcall(api.nvim_win_get_config, win)
    if ok and cfg.relative and cfg.relative ~= "" then
      list[#list + 1] = win
    end
  end
  table.sort(list)
  return list
end

local function float_ids()
  local ids = {}
  for _, win in ipairs(float_wins()) do
    ids[#ids + 1] = tostring(win)
  end
  return "{" .. table.concat(ids, ",") .. "}"
end

-- Each record carries the float set standing at the moment the event
-- fired, which is what makes the ordering question answerable: an event
-- seen while the set is empty happened before the window existed, one
-- seen with the id present happened after it was configured.
local function record(name, win)
  events[#events + 1] = {
    t = uv.hrtime(),
    name = name,
    win = win,
    floats = float_ids(),
  }
end

local function drain(indent)
  local lines = {}
  while drained < #events do
    drained = drained + 1
    local e = events[drained]
    lines[#lines + 1] = string.format(
      "%s%s win=%s floats=%s t=%sms",
      indent,
      e.name,
      tostring(e.win),
      e.floats,
      ms(e.t - armed_at)
    )
  end
  if #lines == 0 then
    return indent .. "(no events)"
  end
  return table.concat(lines, "\n")
end

local function extmarks(buf)
  local lines = {}
  for name, ns in pairs(api.nvim_get_namespaces()) do
    local ok, marks = pcall(api.nvim_buf_get_extmarks, buf, ns, 0, -1, {
      details = true,
    })
    if ok and #marks > 0 then
      lines[#lines + 1] = string.format(
        "    ns %q (id %d): %d marks",
        name,
        ns,
        #marks
      )
      for i = 1, math.min(#marks, 4) do
        lines[#lines + 1] = "      " .. vim.inspect(marks[i], {
          newline = " ",
          indent = "",
        })
      end
    end
  end
  if #lines == 0 then
    return "    (no extmarks in any namespace)"
  end
  return table.concat(lines, "\n")
end

local function dump_win(win)
  local buf = api.nvim_win_get_buf(win)
  local cfg = api.nvim_win_get_config(win)
  local lines = api.nvim_buf_get_lines(buf, 0, -1, false)
  local cursor = api.nvim_win_get_cursor(win)
  w(string.format("  win %d (buf %d)", win, buf))
  w("    config: " .. vim.inspect(cfg, { newline = " ", indent = "" }))
  w(string.format(
    "    filetype=%q buftype=%q name=%q cursorline=%s winblend=%s",
    vim.bo[buf].filetype,
    vim.bo[buf].buftype,
    api.nvim_buf_get_name(buf),
    tostring(vim.wo[win].cursorline),
    tostring(vim.wo[win].winblend)
  ))
  w("    lines: " .. vim.inspect(lines, { newline = " ", indent = "" }))
  w(string.format("    cursor: {%d, %d}", cursor[1], cursor[2]))
  w(extmarks(buf))
end

---Full record of every float standing right now, plus the events since the
---previous entry point call.
---@param label string
---@return string status
function M.snap(label)
  local wins = float_wins()
  w("")
  w(string.format("== %s  (t=%sms)", label, ms(uv.hrtime() - armed_at)))
  w("  mode=" .. api.nvim_get_mode().mode .. " cmdline=" .. vim.fn.getcmdline())
  w("  events since last entry:")
  w(drain("    "))
  if #wins == 0 then
    w("  (no floating windows)")
    return "ok"
  end
  for _, win in ipairs(wins) do
    local ok, err = pcall(dump_win, win)
    if not ok then
      w("  win " .. win .. " dump failed: " .. tostring(err))
    end
  end
  return "ok"
end

---Whether any float stands right now, as a positive-sentinel answer a
---scenario can wait toward.
---@return string status
function M.any()
  return #float_wins() > 0 and "float" or "clear"
end

---Blocks inside the session until every float has gone (a notification
---timing out, a picker closing), so the next subject is captured on its
---own rather than stacked under the previous one's window. vim.wait runs
---the event loop, which is what lets a plugin's own fade timers finish.
---@return string status
function M.wait_clear()
  vim.wait(7000, function()
    return #float_wins() == 0
  end, 50)
  return "ok"
end

---One keystroke's worth of churn: the float ids standing, their geometry,
---and the events the key produced. Deliberately shorter than snap() so the
---per-key record reads as a sequence.
---@param label string
---@return string status
function M.tick(label)
  local wins = float_wins()
  w("")
  w(string.format("-- %s  (t=%sms) floats=%s", label, ms(uv.hrtime() - armed_at), float_ids()))
  w("  cmdline=" .. vim.fn.getcmdline())
  for _, win in ipairs(wins) do
    local cfg = api.nvim_win_get_config(win)
    local buf = api.nvim_win_get_buf(win)
    w(string.format(
      "  win %d buf %d row=%s col=%s width=%s height=%s zindex=%s hide=%s lines=%d cursor=%d",
      win,
      buf,
      tostring(cfg.row),
      tostring(cfg.col),
      tostring(cfg.width),
      tostring(cfg.height),
      tostring(cfg.zindex),
      tostring(cfg.hide),
      api.nvim_buf_line_count(buf),
      api.nvim_win_get_cursor(win)[1]
    ))
  end
  w("  events:")
  w(drain("    "))
  return "ok"
end

---Hides the lowest-numbered float the way the absorption would, then arms a
---1 ms sampler that watches for the plugin reconfiguring it back into view.
---The sampler runs on a libuv timer inside the session, so what it times is
---the plugin's own reaction and not a probe round trip. Sampling through
---vim.schedule keeps every API call out of a fast-event context.
---@return string status
function M.hide()
  local wins = float_wins()
  if #wins == 0 then
    w("")
    w("-- hide skipped: no float standing")
    return "ok"
  end
  local win = wins[1]
  local ok, err = pcall(api.nvim_win_set_config, win, { hide = true })
  if not ok then
    w("  hide failed: " .. tostring(err))
    return "ok"
  end
  local known = {}
  for _, other in ipairs(wins) do
    known[other] = true
  end
  local before = api.nvim_win_get_config(win)
  local timer = uv.new_timer()
  hide_state = {
    win = win,
    at = uv.hrtime(),
    samples = 0,
    result = nil,
    known = known,
    geometry = string.format(
      "%s,%s,%s,%s",
      tostring(before.row),
      tostring(before.col),
      tostring(before.width),
      tostring(before.height)
    ),
    timer = timer,
  }
  local st = hide_state
  -- Sampling continues after the first outcome: a plugin that closes the
  -- hidden window and opens a replacement is the failure mode absorption
  -- has to survive, and stopping at the close would record only half of it.
  timer:start(0, 1, function()
    vim.schedule(function()
      st.samples = st.samples + 1
      if not api.nvim_win_is_valid(st.win) then
        st.result = st.result or "closed"
        st.at_result = st.at_result or uv.hrtime()
      else
        local cfg = api.nvim_win_get_config(st.win)
        if not cfg.hide and not st.result then
          st.result = "re-shown"
          st.at_result = uv.hrtime()
        end
        -- The plugin's own next nvim_win_set_config on the same window,
        -- observed as the geometry moving. Whether `hide` survived it is
        -- the whole question the absorption turns on.
        local now = string.format(
          "%s,%s,%s,%s",
          tostring(cfg.row),
          tostring(cfg.col),
          tostring(cfg.width),
          tostring(cfg.height)
        )
        if now ~= st.geometry and not st.at_reconfig then
          st.at_reconfig = uv.hrtime()
          st.reconfig_geometry = now
          st.reconfig_hide = cfg.hide
        end
      end
      if not st.replacement then
        for _, candidate in ipairs(float_wins()) do
          if not st.known[candidate] then
            st.replacement = candidate
            st.at_replacement = uv.hrtime()
            break
          end
        end
      end
    end)
  end)
  w("")
  w(string.format(
    "-- hide win %d (t=%sms) config now: %s",
    win,
    ms(st.at - armed_at),
    vim.inspect(api.nvim_win_get_config(win), { newline = " ", indent = "" })
  ))
  return "ok"
end

---What the sampler saw, measured from the last cmdline change after the
---hide (the keystroke's own arrival, timestamped by the recorder) to the
---moment the window was observed unhidden or gone.
---@return string status
function M.reshow()
  if not hide_state then
    w("")
    w("-- reshow skipped: nothing was hidden")
    return "ok"
  end
  local st = hide_state
  local key_at = nil
  for _, e in ipairs(events) do
    if e.t > st.at and e.name == "CmdlineChanged" then
      key_at = e.t
    end
  end
  local result = st.result or "still-hidden"
  local from_hide = st.at_result and ms(st.at_result - st.at) or "n/a"
  local from_key = (st.at_result and key_at) and ms(st.at_result - key_at) or "n/a"
  w("")
  w(string.format(
    "-- reshow win=%d result=%s samples=%d keystroke->reconfigure=%sms hide->reconfigure=%sms",
    st.win,
    result,
    st.samples,
    from_key,
    from_hide
  ))
  if st.at_reconfig then
    w(string.format(
      "  same-window reconfigure: %s -> %s hide=%s keystroke->reconfigure=%sms hide->reconfigure=%sms",
      st.geometry,
      st.reconfig_geometry,
      tostring(st.reconfig_hide),
      key_at and ms(st.at_reconfig - key_at) or "n/a",
      ms(st.at_reconfig - st.at)
    ))
  else
    w("  same-window reconfigure: none observed (geometry " .. st.geometry .. ")")
  end
  if st.replacement then
    w(string.format(
      "  replacement win=%d keystroke->replacement=%sms hide->replacement=%sms config: %s",
      st.replacement,
      key_at and ms(st.at_replacement - key_at) or "n/a",
      ms(st.at_replacement - st.at),
      vim.inspect(api.nvim_win_get_config(st.replacement), {
        newline = " ",
        indent = "",
      })
    ))
  else
    w("  replacement: none observed")
  end
  if api.nvim_win_is_valid(st.win) then
    w("  config: " .. vim.inspect(api.nvim_win_get_config(st.win), {
      newline = " ",
      indent = "",
    }))
  end
  w("  floats now: " .. float_ids())
  w("  events:")
  w(drain("    "))
  st.timer:stop()
  if not st.timer:is_closing() then
    st.timer:close()
  end
  hide_state = nil
  return "ok"
end

---Header: the identity every value below is captured against.
---@return string status
function M.header()
  local v = vim.version()
  w("")
  w("################ capture " .. os.date("%Y-%m-%dT%H:%M:%S") .. " ################")
  w(string.format("nvim %d.%d.%d api_level=%s", v.major, v.minor, v.patch, tostring(v.api_level)))
  w("lines=" .. vim.o.lines .. " columns=" .. vim.o.columns .. " cmdheight=" .. vim.o.cmdheight)
  local ui = api.nvim_list_uis()[1] or {}
  w(string.format(
    "ui ext_cmdline=%s ext_popupmenu=%s ext_messages=%s ext_multigrid=%s",
    tostring(ui.ext_cmdline),
    tostring(ui.ext_popupmenu),
    tostring(ui.ext_messages),
    tostring(ui.ext_multigrid)
  ))
  w("out=" .. out_path .. " cwd=" .. vim.fn.getcwd())
  local lock = vim.fn.stdpath("config") .. "/lazy-lock.json"
  local ok, text = pcall(vim.fn.readfile, lock)
  if ok then
    w("lazy-lock.json:")
    for _, line in ipairs(text) do
      w("  " .. line)
    end
  end
  return "ok"
end

local group = api.nvim_create_augroup("view_float_capture", { clear = true })
for _, ev in ipairs({
  "WinNew",
  "WinScrolled",
  "WinResized",
  "WinClosed",
  "CursorMovedI",
  "TextChangedI",
  "CmdlineEnter",
  "CmdlineChanged",
  "CmdlineLeave",
}) do
  api.nvim_create_autocmd(ev, {
    group = group,
    callback = function(args)
      -- WinClosed reports the closing window in `match`; every other event
      -- here reports whatever window is current when it fires.
      local win = tonumber(args.match) or api.nvim_get_current_win()
      record(ev, win)
    end,
  })
end

-- The heavy fixture pins nvim-cmp with cmp-buffer as its only source and
-- configures insert mode alone, so the cmdline float has to be asked for
-- here. Same plugin, same view layer, same window machinery: only the
-- source list differs from a config that installs cmp-cmdline.
local has_cmp, cmp = pcall(require, "cmp")
if has_cmp then
  local cmdline_opts = {
    mapping = cmp.mapping.preset.cmdline(),
    sources = { { name = "buffer" } },
  }
  cmp.setup.cmdline(":", cmdline_opts)
  cmp.setup.cmdline("/", cmdline_opts)
end

return has_cmp and "armed" or "armed-no-cmp"
