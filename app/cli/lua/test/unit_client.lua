-- unit_client.lua — the API client against a stub transport: request-body
-- building (camelCase, optional fields omitted), the login flow's exact
-- wire shape, header handling, and error-envelope parsing. No server.

local json = require("pocketskynet.json")
local sha2 = require("pocketskynet.sha2")
local eip191 = require("pocketskynet.eip191")
local Client = require("pocketskynet.client")

local KEY = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
local ADDR = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
local CHALLENGE_MSG = "Welcome to FruitNation!\n\nClick to sign in and accept " ..
  "the FruitNation Terms of Service.\n\nThis request will not trigger a " ..
  "blockchain transaction or cost any gas fees.\n\nWallet address:\n" .. ADDR ..
  "\n\nNonce:\n" .. string.rep("ab", 32)

-- A transport double: records every call, replays canned responses.
local function stub_transport(responses)
  return {
    calls = {},
    responses = responses,
    request = function(self, method, path, body, headers)
      self.calls[#self.calls + 1] = {
        method = method, path = path, body = body, headers = headers or {},
      }
      local r = table.remove(self.responses, 1)
      if not r then return nil, "stub transport: no response queued for " .. path end
      return r[1], r[2]
    end,
  }
end

local function client_with(responses, opts)
  opts = opts or {}
  local c = Client.new({
    key = opts.key or KEY,
    token = opts.token,
    username = opts.username,
  })
  c.t = stub_transport(responses)
  return c
end

local function login_responses()
  return {
    { 200, json.encode({
        challengeId = "chal-1", message = CHALLENGE_MSG,
        expiresAt = "2026-01-01T00:00:00.000Z" }) },
    { 200, json.encode({
        token = "jwt-token-1",
        user = { walletAddress = ADDR, username = "alice" },
        encryptionSalt = string.rep("00", 32) }) },
  }
end

return function(T)
  T.test("login posts the lowercased wallet address to /api/auth/challenge", function()
    local c = client_with(login_responses())
    c:login()
    local call = c.t.calls[1]
    T.eq(call.method, "POST")
    T.eq(call.path, "/api/auth/challenge")
    local body = json.decode(call.body)
    T.eq(body.walletAddress, ADDR, "camelCase key, lowercase address")
    T.eq(call.headers["Authorization"], nil, "challenge is unauthenticated")
  end)

  T.test("login signs the challenge message verbatim", function()
    local c = client_with(login_responses())
    c:login()
    local body = json.decode(c.t.calls[2].body)
    T.eq(c.t.calls[2].path, "/api/auth/login")
    T.eq(body.challengeId, "chal-1")
    T.eq(body.walletAddress, ADDR)
    -- must be the deterministic EIP-191 signature over the exact message
    T.eq(body.signature, eip191.sign(KEY, CHALLENGE_MSG))
  end)

  T.test("login omits username when none is configured", function()
    local c = client_with(login_responses())
    c:login()
    local body = json.decode(c.t.calls[2].body)
    T.eq(body.username, nil, "username key must be absent, not null/empty")
    T.ok(not c.t.calls[2].body:find("username", 1, true),
      "the raw JSON must not mention username")
  end)

  T.test("login includes username when configured", function()
    local c = client_with(login_responses(), { username = "alice" })
    c:login()
    T.eq(json.decode(c.t.calls[2].body).username, "alice")
  end)

  T.test("login stores the JWT and reuses it for authed calls", function()
    local responses = login_responses()
    responses[#responses + 1] = { 200, "[]" }
    local c = client_with(responses)
    c:login()
    c:rooms()
    local call = c.t.calls[3]
    T.eq(call.method, "GET")
    T.eq(call.path, "/api/rooms")
    T.eq(call.headers["Authorization"], "Bearer jwt-token-1")
    T.eq(call.body, nil, "GET carries no body")
  end)

  T.test("a preset token skips the login round-trip entirely", function()
    local c = client_with({ { 200, "[]" } }, { token = "preset-jwt" })
    c:rooms()
    T.eq(#c.t.calls, 1, "no challenge/login calls")
    T.eq(c.t.calls[1].headers["Authorization"], "Bearer preset-jwt")
  end)

  T.test("send_message trims content and hashes exactly what it sends", function()
    local c = client_with({ { 200, json.encode({ id = "msg_1" }) } },
      { token = "tok" })
    c:send_message("room_x", "  hello there \n")
    local body = json.decode(c.t.calls[1].body)
    T.eq(c.t.calls[1].path, "/api/rooms/room_x/messages")
    T.eq(body.content, "hello there", "trimmed before sending")
    T.eq(body.msgHash, sha2.to_hex(sha2.sha256("hello there")),
      "msgHash = sha256 of the trimmed content")
    T.eq(body.isEncrypted, false)
  end)

  T.test("send_message hashes unicode content by bytes", function()
    local c = client_with({ { 200, json.encode({ id = "msg_1" }) } },
      { token = "tok" })
    local text = "한글 메시지 🍓🍊"
    c:send_message("room_x", text)
    local body = json.decode(c.t.calls[1].body)
    T.eq(body.content, text)
    -- pinned by the msgHash.plaintext protocol vector
    T.eq(body.msgHash, "90f15b87d2781befd4a1b6a91dea008417ad8b3f2e53d5cdaa82752db72009dd")
  end)

  T.test("send_message rejects empty and whitespace-only content locally", function()
    local c = client_with({}, { token = "tok" })
    T.err_match(function() c:send_message("room_x", "") end, "not be empty")
    T.err_match(function() c:send_message("room_x", "   \n\t ") end, "not be empty")
    T.eq(#c.t.calls, 0, "nothing must reach the wire")
  end)

  T.test("send_message rejects over-long content locally", function()
    local c = client_with({}, { token = "tok" })
    T.err_match(function()
      c:send_message("room_x", string.rep("a", 5001))
    end, "5000")
    T.eq(#c.t.calls, 0)
  end)

  T.test("create_room omits an empty description", function()
    local c = client_with({ { 200, json.encode({ id = "room_1" }) } },
      { token = "tok" })
    c:create_room("My room", "")
    local body = json.decode(c.t.calls[1].body)
    T.eq(body.name, "My room")
    T.eq(body.description, nil)
  end)

  T.test("create_room carries a non-empty description", function()
    local c = client_with({ { 200, json.encode({ id = "room_1" }) } },
      { token = "tok" })
    c:create_room("My room", "about things")
    T.eq(json.decode(c.t.calls[1].body).description, "about things")
  end)

  T.test("messages builds the limit query string", function()
    local c = client_with({ { 200, "[]" }, { 200, "[]" } }, { token = "tok" })
    c:messages("room_x")
    T.eq(c.t.calls[1].path, "/api/rooms/room_x/messages")
    c:messages("room_x", 5)
    T.eq(c.t.calls[2].path, "/api/rooms/room_x/messages?limit=5")
  end)

  T.test("the plain error envelope surfaces status and message", function()
    local c = client_with({ { 403, json.encode({ message = "Access denied" }) } },
      { token = "tok" })
    local err = T.err_match(function() c:rooms() end, "Access denied")
    T.contains(err, "HTTP 403")
    T.contains(err, "/api/rooms")
  end)

  T.test("the validation error envelope surfaces its message", function()
    local c = client_with({
      { 400, json.encode({
          message = "Validation failed",
          errors = { "roomId: Room ID contains invalid characters" } }) },
    }, { token = "tok" })
    local err = T.err_match(function() c:rooms() end, "Validation failed")
    T.contains(err, "HTTP 400")
  end)

  T.test("a 401 with Invalid token surfaces as an error", function()
    local c = client_with({ { 401, json.encode({ message = "Invalid token" }) } },
      { token = "expired" })
    T.err_match(function() c:rooms() end, "Invalid token")
  end)

  T.test("a non-JSON error body is still reported", function()
    local c = client_with({ { 502, "Bad Gateway (upstream)" } }, { token = "tok" })
    local err = T.err_match(function() c:rooms() end, "HTTP 502")
    T.contains(err, "Bad Gateway")
  end)

  T.test("a transport failure surfaces its message", function()
    local c = Client.new({ token = "tok" })
    c.t = { request = function() return nil, "curl failed (exit 7): refused" end }
    T.err_match(function() c:rooms() end, "curl failed")
  end)

  T.test("login raises on a malformed challenge response", function()
    local c = client_with({ { 200, json.encode({ nope = true }) } })
    T.err_match(function() c:login() end, "malformed challenge")
  end)

  T.test("login raises on a malformed login response", function()
    local c = client_with({
      { 200, json.encode({ challengeId = "c", message = "m" }) },
      { 200, json.encode({ user = {} }) }, -- no token
    })
    T.err_match(function() c:login() end, "malformed login")
  end)

  T.test("address derivation requires a key", function()
    local c = Client.new({})
    T.err_match(function() c:address() end, "no private key")
  end)
end
