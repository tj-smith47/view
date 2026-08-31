-- Accepted: the marker gates a block, and the plugin private it reaches
-- lives inside that block.
local accommodate = vim.env.VIEW_COMPAT_ACCOMMODATIONS ~= "0"

-- view-compat-accommodation: a plugin's once-only notify dedup table
if accommodate then
  local once = require("plugin.util")._once
  once["already-said"] = true
end

-- view-compat-accommodation: a table value the switch selects between
local opts = accommodate and { thing = false } or {}
return opts
