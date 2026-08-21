-- client.lua — PocketSkynet API client (login, rooms, messages, health).
--
-- Wire protocol per app/docs/API.md and PROTOCOL.md §4:
--   POST /api/auth/challenge { walletAddress }  → { challengeId, message }
--   sign the challenge message VERBATIM with EIP-191 personal_sign
--   POST /api/auth/login { walletAddress, challengeId, signature[, username] }
--     → { token, user, … }; then Authorization: Bearer <token>.
-- Every login burns its challenge, so a retry always refetches one.

local json = require("pocketskynet.json")
local transport = require("pocketskynet.transport")
local eip191 = require("pocketskynet.eip191")
local sha2 = require("pocketskynet.sha2")

local Client = {}
Client.__index = Client

--- opts: server, http3, insecure, curl (transport options);
---       key (private key hex), token (pre-existing JWT), username.
function Client.new(opts)
  opts = opts or {}
  return setmetatable({
    t = transport.new(opts),
    key = opts.key,
    token = opts.token,
    username = opts.username,
  }, Client)
end

-- Perform a request; decode JSON; raise a readable error on HTTP >= 400.
function Client:req(method, path, body_tbl, authed)
  local headers = {}
  if authed then
    headers["Authorization"] = "Bearer " .. self:ensure_token()
  end
  local body = body_tbl and json.encode(body_tbl) or nil
  local status, resp = self.t:request(method, path, body, headers)
  if not status then
    error(resp, 0)
  end
  local decoded
  if resp and resp ~= "" then
    local ok, v = pcall(json.decode, resp)
    decoded = ok and v or nil
  end
  if status >= 400 then
    local msg = decoded and type(decoded) == "table" and decoded.message or resp
    error(string.format("HTTP %d %s %s: %s", status, method, path, tostring(msg)), 0)
  end
  return decoded, status
end

---------------------------------------------------------------------------
-- Auth
---------------------------------------------------------------------------

function Client:address()
  assert(self.key, "no private key configured (use --key or POCKETSKYNET_KEY)")
  return eip191.address(self.key)
end

--- Challenge → sign verbatim → login. Returns the full login response and
--- stores the JWT for subsequent requests.
function Client:login()
  local wallet = self:address()
  local challenge = self:req("POST", "/api/auth/challenge", { walletAddress = wallet })
  assert(type(challenge) == "table" and type(challenge.message) == "string",
    "malformed challenge response")

  local body = {
    walletAddress = wallet,
    challengeId = challenge.challengeId,
    signature = eip191.sign(self.key, challenge.message),
  }
  if self.username and self.username ~= "" then
    body.username = self.username
  end

  local resp = self:req("POST", "/api/auth/login", body)
  assert(type(resp) == "table" and type(resp.token) == "string",
    "malformed login response")
  self.token = resp.token
  return resp
end

function Client:ensure_token()
  if not self.token then
    self:login()
  end
  return self.token
end

---------------------------------------------------------------------------
-- API surface used by the CLI
---------------------------------------------------------------------------

function Client:health()
  return self:req("GET", "/api/health")
end

function Client:rooms()
  -- An empty/nil body (e.g. a 200 with no JSON) becomes an empty list, so
  -- callers can always `ipairs`/`#` the result without a nil-length error.
  return self:req("GET", "/api/rooms", nil, true) or {}
end

function Client:create_room(name, description)
  local body = { name = name }
  if description and description ~= "" then body.description = description end
  return self:req("POST", "/api/rooms", body, true)
end

--- Send a plaintext message (E2EE is out of scope for this client).
--- The server trims content before storing, and msgHash for a plaintext
--- message is SHA-256 of the trimmed content (PROTOCOL.md §13) — so trim
--- first and hash exactly what is sent.
function Client:send_message(room_id, text)
  local content = text:match("^%s*(.-)%s*$")
  assert(#content > 0, "message content must not be empty")
  -- The server's 1–5000 limit counts characters, not bytes, so measure in
  -- UTF-8 codepoints (falling back to bytes only if the text is not valid
  -- UTF-8). Counting bytes would reject a multibyte message the server
  -- would happily accept.
  local char_len = utf8.len(content) or #content
  assert(char_len <= 5000, "message content must be at most 5000 characters")
  return self:req("POST", "/api/rooms/" .. room_id .. "/messages", {
    content = content,
    msgHash = sha2.to_hex(sha2.sha256(content)),
    isEncrypted = false,
  }, true)
end

function Client:messages(room_id, limit)
  local path = "/api/rooms/" .. room_id .. "/messages"
  if limit then path = path .. "?limit=" .. tostring(limit) end
  -- Coerce an empty/nil body to an empty list — see Client:rooms.
  return self:req("GET", path, nil, true) or {}
end

return Client
