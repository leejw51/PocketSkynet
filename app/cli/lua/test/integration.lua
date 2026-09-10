#!/usr/bin/env lua
-- integration.lua — drives the Lua client against a real pocketskynet
-- server (one process per transport variant, ephemeral ports, temp data
-- dirs, guaranteed teardown — see harness.lua).
--
--   lua app/cli/lua/test/integration.lua
--
-- Needs a built server binary (app/target/{release,debug}/pocketskynet, or
-- $POCKETSKYNET_BIN). The HTTP/3 group is skipped with a message when no
-- HTTP/3-capable curl exists on the machine.

local test_dir = arg[0]:match("^(.*)[/\\][^/\\]*$") or "."
package.path = test_dir .. "/../?.lua;" .. test_dir .. "/?.lua;" .. package.path

local T = require("runner")
local harness = require("harness")
local json = require("pocketskynet.json")
local sha2 = require("pocketskynet.sha2")
local eip191 = require("pocketskynet.eip191")
local transport = require("pocketskynet.transport")
local Client = require("pocketskynet.client")

-- Servers must die even when the suite aborts on the failure exit path.
T.at_exit(harness.stop_all)

local CLI = test_dir .. "/../pocketskynet.lua"

-- Hardhat account #0 and the 0x0123…ef vector key — two distinct wallets.
local KEY_A = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
local KEY_B = "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
local ADDR_A = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"

local function shquote(s)
  return "'" .. s:gsub("'", "'\\''") .. "'"
end

--- Run the CLI against a server; returns exit code, combined output.
local function run_cli(args)
  local parts = { "lua", shquote(CLI) }
  for _, a in ipairs(args) do parts[#parts + 1] = shquote(a) end
  local cmd = "env -u POCKETSKYNET_KEY -u POCKETSKYNET_TOKEN " ..
    "-u POCKETSKYNET_SERVER -u POCKETSKYNET_CURL " ..
    table.concat(parts, " ") .. " 2>&1"
  local p = assert(io.popen(cmd, "r"))
  local out = p:read("a") or ""
  local ok, _, code = p:close()
  if ok then code = 0 end
  return code, out
end

--- A raw (unwrapped) request helper for the negative-path tests.
local function raw(server, method, path, body_tbl, token)
  local t = transport.new({ server = server.base_url, insecure = true })
  local headers = {}
  if token then headers["Authorization"] = "Bearer " .. token end
  local status, body = t:request(method, path,
    body_tbl and json.encode(body_tbl) or nil, headers)
  assert(status, body)
  local decoded = nil
  if body and body ~= "" then
    local ok, v = pcall(json.decode, body)
    decoded = ok and v or nil
  end
  return status, decoded, body
end

---------------------------------------------------------------------------
-- Find an HTTP/3-capable curl (for the QUIC group).
---------------------------------------------------------------------------

local function curl_has_http3(path)
  local p = io.popen(shquote(path) .. " --version 2>/dev/null", "r")
  if not p then return false end
  local out = p:read("a") or ""
  p:close()
  return out:find("HTTP3", 1, true) ~= nil
end

local function find_http3_curl()
  local candidates = {
    os.getenv("POCKETSKYNET_CURL"),
    "curl",
    "/opt/homebrew/opt/curl/bin/curl",
    "/usr/local/opt/curl/bin/curl",
  }
  for _, c in ipairs(candidates) do
    if c and c ~= "" and curl_has_http3(c) then return c end
  end
  return nil
end

---------------------------------------------------------------------------
-- Main flow — wrapped so a failure outside any T.test (a failed server
-- boot, a crashed setup login) still tears every server down before exit.
---------------------------------------------------------------------------

local function main()

---------------------------------------------------------------------------
-- Group 1: plain HTTP/1.1 server
---------------------------------------------------------------------------

local server = harness.start({})

T.test("health answers ok with an uptime", function()
  local c = Client.new({ server = server.base_url })
  local h = c:health()
  T.eq(h.status, "ok")
  T.ok(math.type(h.uptime) == "integer" or type(h.uptime) == "number", "uptime is numeric")
end)

T.test("first login without a username is refused with guidance", function()
  local c = Client.new({ server = server.base_url, key = KEY_A })
  T.err_match(function() c:login() end, "Username is required for first%-time login")
end)

T.test("login happy path: challenge -> sign -> JWT", function()
  local c = Client.new({ server = server.base_url, key = KEY_A, username = "lua_alice" })
  local resp = c:login()
  T.ok(type(resp.token) == "string" and #resp.token > 20, "JWT present")
  T.eq(resp.user.walletAddress, ADDR_A, "lowercased wallet address")
  T.eq(resp.user.username, "lua_alice")
  T.ok(type(resp.encryptionSalt) == "string" and #resp.encryptionSalt == 64,
    "encryption salt is 64 hex chars")
  -- and the token actually works
  local rooms = c:rooms()
  T.ok(type(rooms) == "table", "authed request succeeds")
end)

T.test("a later login may omit the username and reuses the stored one", function()
  local c = Client.new({ server = server.base_url, key = KEY_A })
  local resp = c:login()
  T.eq(resp.user.username, "lua_alice")
end)

T.test("a signature from a different wallet is a 401", function()
  local status, challenge = raw(server, "POST", "/api/auth/challenge",
    { walletAddress = ADDR_A })
  T.eq(status, 200)
  local login_status, body = raw(server, "POST", "/api/auth/login", {
    walletAddress = ADDR_A,
    challengeId = challenge.challengeId,
    signature = eip191.sign(KEY_B, challenge.message), -- wrong key
    username = "mallory",
  })
  T.eq(login_status, 401)
  T.eq(body.message, "Invalid signature")
end)

T.test("a challenge cannot be reused (burned even by a failed login)", function()
  local status, challenge = raw(server, "POST", "/api/auth/challenge",
    { walletAddress = ADDR_A })
  T.eq(status, 200)
  local first = raw(server, "POST", "/api/auth/login", {
    walletAddress = ADDR_A,
    challengeId = challenge.challengeId,
    signature = eip191.sign(KEY_A, challenge.message),
  })
  T.eq(first, 200, "first use succeeds")
  local second, body = raw(server, "POST", "/api/auth/login", {
    walletAddress = ADDR_A,
    challengeId = challenge.challengeId,
    signature = eip191.sign(KEY_A, challenge.message),
  })
  T.eq(second, 400)
  T.eq(body.message, "Invalid or expired challenge")
end)

T.test("a garbage JWT is a 401 Invalid token", function()
  local status, body = raw(server, "GET", "/api/rooms", nil, "not-a-jwt")
  T.eq(status, 401)
  T.eq(body.message, "Invalid token")
end)

T.test("a missing JWT is a 401 No token provided", function()
  local status, body = raw(server, "GET", "/api/rooms", nil, nil)
  T.eq(status, 401)
  T.eq(body.message, "No token provided")
end)

-- one logged-in client for the room/message flow
local alice = Client.new({ server = server.base_url, key = KEY_A, username = "lua_alice" })
alice:login()
local room_id

T.test("creating a room returns a bare room the creator is in", function()
  local room = alice:create_room("Lua IT Room")
  room_id = room.id
  T.ok(room_id:match("^room_") ~= nil, "server-assigned room id")
  T.eq(room.name, "Lua IT Room")
  T.eq(room.currentKeyVersion, 1)
  T.eq(room.keyRotationPending, false)
end)

T.test("the room list is enriched and contains the new room", function()
  local rooms = alice:rooms()
  local found
  for _, r in ipairs(rooms) do
    if r.id == room_id then found = r end
  end
  T.ok(found, "created room appears in the list")
  T.eq(found.memberCount, 1)
  T.eq(found.hasEncryption, false)
  T.eq(found.members[1].user.walletAddress, ADDR_A)
end)

T.test("an invalid room name is a 400 Validation failed", function()
  local err = T.err_match(function()
    alice:create_room("<script>alert(1)</script>")
  end, "Validation failed")
  T.contains(err, "HTTP 400")
end)

T.test("creating a room requires authentication", function()
  local status, body = raw(server, "POST", "/api/rooms", { name = "nope" }, nil)
  T.eq(status, 401)
  T.eq(body.message, "No token provided")
end)

T.test("a sent message comes back with its sender and hash", function()
  local m = alice:send_message(room_id, "  hello from lua  ")
  T.ok(m.id:match("^msg_") ~= nil, "server-assigned message id")
  T.eq(m.content, "hello from lua", "server stores the trimmed content")
  T.eq(m.msgHash, sha2.to_hex(sha2.sha256("hello from lua")))
  T.eq(m.senderAddress, ADDR_A, "sender comes from the JWT")
  T.eq(m.sender.username, "lua_alice")
  T.eq(m.isEncrypted, false)
  T.ok(m.msgType == "add" or m.msgType == "message", "msgType")
end)

T.test("unicode content round-trips intact", function()
  local text = "한글 메시지 🍓🍊 and ascii"
  local sent = alice:send_message(room_id, text)
  T.eq(sent.content, text)
  local msgs = alice:messages(room_id)
  T.eq(msgs[#msgs].content, text, "listed verbatim")
  T.eq(msgs[#msgs].msgHash, sha2.to_hex(sha2.sha256(text)))
end)

T.test("messages list is chronologically ascending", function()
  alice:send_message(room_id, "third message")
  local msgs = alice:messages(room_id)
  T.ok(#msgs >= 3, "all messages present")
  for i = 2, #msgs do
    T.ok(msgs[i].msgSerial > msgs[i - 1].msgSerial, "serials strictly increase")
  end
  T.eq(msgs[1].content, "hello from lua")
end)

T.test("the limit query parameter caps the page", function()
  local msgs = alice:messages(room_id, 1)
  T.eq(#msgs, 1)
  T.eq(msgs[1].content, "third message", "the newest survives a limit of 1")
end)

T.test("a malformed msgHash is a 400 Validation failed", function()
  local status, body = raw(server, "POST",
    "/api/rooms/" .. room_id .. "/messages",
    { content = "x", msgHash = string.rep("G", 64) }, alice.token)
  T.eq(status, 400)
  T.eq(body.message, "Validation failed")
end)

T.test("a non-member can neither post nor read", function()
  local bob = Client.new({ server = server.base_url, key = KEY_B, username = "lua_bob" })
  bob:login()
  local err = T.err_match(function()
    bob:send_message(room_id, "intruding")
  end, "Access denied")
  T.contains(err, "HTTP 403")
  T.err_match(function() bob:messages(room_id) end, "Access denied")
end)

T.test("a nonexistent room answers 403, not 404", function()
  local err = T.err_match(function()
    alice:messages("room_does_not_exist_123")
  end, "Access denied")
  T.contains(err, "HTTP 403")
end)

---------------------------------------------------------------------------
-- Group 2: the CLI end to end (exit codes and output)
---------------------------------------------------------------------------

T.test("cli health exits 0 against a live server", function()
  local code, out = run_cli({ "--server", server.base_url, "health" })
  T.eq(code, 0)
  T.contains(out, "status: ok")
end)

T.test("cli login exits 0 and prints the token", function()
  local code, out = run_cli({
    "--server", server.base_url, "--key", KEY_A, "login",
  })
  T.eq(code, 0)
  T.contains(out, "address:  " .. ADDR_A)
  T.contains(out, "username: lua_alice")
  T.contains(out, "export POCKETSKYNET_TOKEN=")
end)

local cli_room_id

T.test("cli create-room exits 0 and prints the room id", function()
  local code, out = run_cli({
    "--server", server.base_url, "--key", KEY_A, "create-room", "CLI Room",
  })
  T.eq(code, 0)
  cli_room_id = out:match("created room (room_[%w_%.%-]+)")
  T.ok(cli_room_id, "room id printed: " .. out)
end)

T.test("cli send and messages round-trip, exit 0", function()
  local text = "cli says 안녕 🍊"
  local code = run_cli({
    "--server", server.base_url, "--key", KEY_A, "send", cli_room_id, text,
  })
  T.eq(code, 0)
  local list_code, out = run_cli({
    "--server", server.base_url, "--key", KEY_A, "messages", cli_room_id,
  })
  T.eq(list_code, 0)
  T.contains(out, "lua_alice: " .. text)
end)

T.test("cli rooms lists the created room, exit 0", function()
  local code, out = run_cli({
    "--server", server.base_url, "--key", KEY_A, "rooms",
  })
  T.eq(code, 0)
  T.contains(out, cli_room_id)
  T.contains(out, "CLI Room")
end)

T.test("cli send to a foreign room exits 1 with the server's error", function()
  local code, out = run_cli({
    "--server", server.base_url, "--key", KEY_B, "send", cli_room_id, "nope",
  })
  T.eq(code, 1)
  T.contains(out, "Access denied")
end)

T.test("cli reuses a token passed via --token", function()
  local _, login_out = run_cli({
    "--server", server.base_url, "--key", KEY_A, "login",
  })
  local token = login_out:match("export POCKETSKYNET_TOKEN=([%w%.%-_]+)")
  T.ok(token, "token extracted")
  local code, out = run_cli({
    "--server", server.base_url, "--token", token, "rooms",
  })
  T.eq(code, 0, "no key needed with a token")
  T.contains(out, cli_room_id)
end)

server.stop()

---------------------------------------------------------------------------
-- Group 3: HTTPS with a self-signed certificate (--insecure)
---------------------------------------------------------------------------

local tls_server = harness.start({ tls = true })

T.test("tls: health over https with --insecure", function()
  local c = Client.new({ server = tls_server.base_url, insecure = true })
  T.eq(c:health().status, "ok")
end)

T.test("tls: without --insecure the self-signed certificate is refused", function()
  local c = Client.new({ server = tls_server.base_url })
  T.err_match(function() c:health() end, "curl failed")
end)

T.test("tls: full login + room + message flow over https", function()
  local c = Client.new({
    server = tls_server.base_url, insecure = true,
    key = KEY_A, username = "lua_tls",
  })
  local resp = c:login()
  T.eq(resp.user.walletAddress, ADDR_A)
  local room = c:create_room("TLS Room")
  local m = c:send_message(room.id, "over https 🚀")
  T.eq(m.content, "over https 🚀")
  T.eq(#c:messages(room.id), 1)
end)

T.test("tls: cli health --insecure exits 0", function()
  local code, out = run_cli({
    "--server", tls_server.base_url, "--insecure", "health",
  })
  T.eq(code, 0)
  T.contains(out, "status: ok")
end)

tls_server.stop()

---------------------------------------------------------------------------
-- Group 4: HTTP/3 (QUIC) — needs an HTTP/3-capable curl
---------------------------------------------------------------------------

local h3_curl = find_http3_curl()
if not h3_curl then
  T.skip("http3: transport parity over QUIC",
    "no HTTP/3-capable curl found (checked PATH curl, " ..
    "/opt/homebrew/opt/curl/bin/curl, /usr/local/opt/curl/bin/curl; " ..
    "`brew install curl` provides one)")
else
  -- The conventional deployment: TCP (TLS) and UDP (QUIC) share the port
  -- number, so the same base URL serves both transports.
  local h3_server = harness.start({ tls = true, http3 = true })

  T.test("http3: the request really travels over QUIC (protocol = h3)", function()
    local c = Client.new({
      server = h3_server.base_url, http3 = true, insecure = true, curl = h3_curl,
    })
    local info = c:req("GET", "/api/server/info")
    T.eq(info.protocol, "h3",
      "/api/server/info reports the carrying protocol — must be h3")
  end)

  T.test("http3: login + rooms + messages over QUIC", function()
    local c = Client.new({
      server = h3_server.base_url, http3 = true, insecure = true, curl = h3_curl,
      key = KEY_A, username = "lua_quic",
    })
    local resp = c:login()
    T.eq(resp.user.walletAddress, ADDR_A)
    local room = c:create_room("QUIC Room")
    local m = c:send_message(room.id, "over quic 🛰")
    T.eq(m.content, "over quic 🛰")
    T.eq(#c:messages(room.id), 1)
  end)

  T.test("http3: cli --http3 health exits 0", function()
    local code, out = run_cli({
      "--server", h3_server.base_url, "--http3", "--insecure",
      "--curl", h3_curl, "health",
    })
    T.eq(code, 0)
    T.contains(out, "status: ok")
  end)

  h3_server.stop()
end

end -- main()

local ok, err = xpcall(main, function(e)
  return debug.traceback(tostring(e), 2)
end)

---------------------------------------------------------------------------
-- Teardown verification: no leaked servers (runs on every path)
---------------------------------------------------------------------------

harness.stop_all()

if not ok then
  io.write("FATAL (outside any test): ", tostring(err), "\n")
  T.failed = T.failed + 1
  T.failures[#T.failures + 1] = "suite setup/flow"
end

T.test("no spawned server process is still alive", function()
  T.ok(harness.none_alive(), "every spawned pid must be dead")
end)

T.finish("integration")
