#!/usr/bin/env lua
-- unit.lua — run every unit suite (no server required):
--   lua app/cli/lua/test/unit.lua
-- Optionally name suites to run a subset:
--   lua app/cli/lua/test/unit.lua crypto json

local test_dir = arg[0]:match("^(.*)[/\\][^/\\]*$") or "."
package.path = test_dir .. "/../?.lua;" .. test_dir .. "/?.lua;" .. package.path

local T = require("runner")

local SUITES = { "crypto", "json", "client", "transport", "cli" }

local wanted = {}
for _, name in ipairs(arg) do wanted[name] = true end

for _, name in ipairs(SUITES) do
  if next(wanted) == nil or wanted[name] then
    require("unit_" .. name)(T)
  end
end

T.finish("unit")
