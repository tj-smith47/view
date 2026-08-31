-- Rejected: the marker labels a block that runs unconditionally, which is
-- the shape a suppression takes once its gate is deleted.
local accommodate = vim.env.VIEW_COMPAT_ACCOMMODATIONS ~= "0"

-- view-compat-accommodation: a plugin's once-only notify dedup table
do
  local once = require("plugin.util")._once
  once["already-said"] = true
end
