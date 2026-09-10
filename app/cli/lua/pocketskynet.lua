#!/usr/bin/env lua
-- pocketskynet.lua — Lua CLI for the PocketSkynet server.
--
-- Commands:
--   health                    server liveness (no auth)
--   login                     challenge → EIP-191 sign → JWT
--   rooms                     list the rooms you are a member of
--   create-room <name>        create a room (you become admin + member)
--   send <roomId> <text>      send a plaintext message
--   messages <roomId>         list a room's messages
--
-- Flags:
--   --server <url>    base URL (default http://127.0.0.1:9081)
--   --http3           use HTTP/3 (QUIC) — needs an HTTP/3-capable curl
--   --insecure        accept self-signed dev certificates
--   --key <hex>       wallet private key (or POCKETSKYNET_KEY env var)
--   --token <jwt>     reuse a JWT instead of logging in (or POCKETSKYNET_TOKEN)
--   --username <name> username for first-time login
--   --curl <path>     curl binary to use (or POCKETSKYNET_CURL)
--
-- Requires Lua 5.4+ (uses native 64-bit integers; Lua 5.5 works too).

-- Make the bundled modules loadable no matter where this script runs from.
local script_dir = arg[0]:match("^(.*)[/\\][^/\\]*$") or "."
package.path = script_dir .. "/?.lua;" .. package.path

if not math.maxinteger or (1 << 62) <= 0 then
  io.stderr:write("pocketskynet.lua requires Lua 5.4 or newer (native 64-bit integers)\n")
  os.exit(1)
end

local Client = require("pocketskynet.client")
local json = require("pocketskynet.json")

local USAGE = [[
Usage: pocketskynet.lua [flags] <command> [args]

Commands:
  health                    check server liveness (no auth)
  login                     log in and print the JWT
  rooms                     list your rooms
  create-room <name>        create a room
  send <roomId> <text>      send a plaintext message
  messages <roomId>         list a room's messages

Flags:
  --server <url>            base URL (default http://127.0.0.1:9081)
  --http3                   use HTTP/3 (QUIC); requires an HTTP/3-capable curl
  --insecure                accept self-signed dev certificates
  --key <hex>               wallet private key (env: POCKETSKYNET_KEY)
  --token <jwt>             reuse a JWT (env: POCKETSKYNET_TOKEN)
  --username <name>         username for first-time login
  --curl <path>             curl binary (env: POCKETSKYNET_CURL)
]]

local function die(msg)
  io.stderr:write("error: ", msg, "\n")
  os.exit(1)
end

---------------------------------------------------------------------------
-- Argument parsing
---------------------------------------------------------------------------

local opts = {
  server = os.getenv("POCKETSKYNET_SERVER"),
  key = os.getenv("POCKETSKYNET_KEY"),
  token = os.getenv("POCKETSKYNET_TOKEN"),
}
local positional = {}

local flag_takes_value = {
  ["--server"] = "server",
  ["--key"] = "key",
  ["--token"] = "token",
  ["--username"] = "username",
  ["--curl"] = "curl",
}

local i = 1
while i <= #arg do
  local a = arg[i]
  if flag_takes_value[a] then
    i = i + 1
    if not arg[i] then die(a .. " requires a value") end
    opts[flag_takes_value[a]] = arg[i]
  elseif a == "--http3" then
    opts.http3 = true
  elseif a == "--insecure" then
    opts.insecure = true
  elseif a == "--help" or a == "-h" then
    io.write(USAGE)
    os.exit(0)
  elseif a:sub(1, 2) == "--" then
    die("unknown flag " .. a .. "\n\n" .. USAGE)
  else
    positional[#positional + 1] = a
  end
  i = i + 1
end

local command = table.remove(positional, 1)
if not command then
  io.write(USAGE)
  os.exit(1)
end

---------------------------------------------------------------------------
-- Output helpers
---------------------------------------------------------------------------

local function field(t, k, default)
  local v = t[k]
  if v == nil or v == json.null then return default end
  return v
end

local function print_message(m)
  local sender = field(m, "sender", {})
  local name = field(sender, "username", field(m, "senderAddress", "?"))
  local content = field(m, "content", "")
  if field(m, "isEncrypted", false) then
    content = "<encrypted message (E2EE not supported by this client)>"
  end
  local ts = field(m, "messageTimestamp", 0)
  print(string.format("[%s] %s: %s",
    os.date("!%Y-%m-%d %H:%M:%S", math.floor(ts / 1000)), name, content))
end

---------------------------------------------------------------------------
-- Commands
---------------------------------------------------------------------------

local function run()
  local client = Client.new(opts)

  if command == "health" then
    local h = client:health()
    print(string.format("status: %s (uptime %ss)",
      tostring(field(h, "status", "?")), tostring(field(h, "uptime", "?"))))

  elseif command == "login" then
    if not opts.key then die("login requires --key or POCKETSKYNET_KEY") end
    local resp = client:login()
    local user = field(resp, "user", {})
    print("address:  " .. field(user, "walletAddress", client:address()))
    print("username: " .. tostring(field(user, "username", "?")))
    print("token:    " .. resp.token)
    print()
    print("export POCKETSKYNET_TOKEN=" .. resp.token)

  elseif command == "rooms" then
    local rooms = client:rooms()
    if #rooms == 0 then
      print("no rooms")
      return
    end
    for _, r in ipairs(rooms) do
      print(string.format("%-60s  %-24s  members=%d%s",
        field(r, "id", "?"),
        tostring(field(r, "name", "")),
        field(r, "memberCount", 0),
        field(r, "hasEncryption", false) and "  [encrypted]" or ""))
    end

  elseif command == "create-room" then
    local name = positional[1]
    if not name then die("usage: create-room <name>") end
    local room = client:create_room(name)
    print("created room " .. field(room, "id", "?") .. " (" ..
      tostring(field(room, "name", "")) .. ")")

  elseif command == "send" then
    local room_id, text = positional[1], positional[2]
    if not room_id or not text then die("usage: send <roomId> <text>") end
    -- allow the message to be split across remaining args for convenience
    if #positional > 2 then
      text = table.concat(positional, " ", 2)
    end
    local m = client:send_message(room_id, text)
    print("sent " .. field(m, "id", "?"))

  elseif command == "messages" then
    local room_id = positional[1]
    if not room_id then die("usage: messages <roomId>") end
    local msgs = client:messages(room_id)
    if #msgs == 0 then
      print("no messages")
      return
    end
    for _, m in ipairs(msgs) do print_message(m) end

  else
    die("unknown command '" .. command .. "'\n\n" .. USAGE)
  end
end

local ok, err = pcall(run)
if not ok then
  die(tostring(err))
end
