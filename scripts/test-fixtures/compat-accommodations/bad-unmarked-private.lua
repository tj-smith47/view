-- Rejected: a reach into a plugin private with no marker declaring it as an
-- accommodation at all.
local once = require("plugin.util")._once
once["already-said"] = true
