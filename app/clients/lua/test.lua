#!/usr/bin/env lua
-- test.lua — entry point for the client's unit suites (no server needed):
--
--   lua app/clients/lua/test.lua
--
-- The suites live in test/unit_*.lua (crypto vectors, JSON codec, client
-- wire shapes, transport quoting, CLI exit codes); this file just runs
-- test/unit.lua. The server-backed suite is test/integration.lua.

local script_dir = arg[0]:match("^(.*)[/\\][^/\\]*$") or "."
arg[0] = script_dir .. "/test/unit.lua"
return assert(loadfile(arg[0]))(table.unpack(arg))
