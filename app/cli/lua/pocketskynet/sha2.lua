-- sha2.lua — pure-Lua SHA-256 and HMAC-SHA256 (Lua 5.4+, native integers).
--
-- SHA-256 is needed for msgHash (§13 of PROTOCOL.md) and as the hash inside
-- the RFC 6979 deterministic-nonce generator; HMAC-SHA256 is the DRBG
-- primitive itself. Validated against FIPS 180-4 / RFC 4231 vectors in
-- test.lua.

local M = {}

local K = {
  0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1,
  0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
  0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
  0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
  0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147,
  0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
  0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
  0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
  0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
  0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
  0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
}

local MASK = 0xffffffff

local function rrot(x, n)
  return ((x >> n) | (x << (32 - n))) & MASK
end

--- SHA-256 of a byte string; returns 32 raw bytes.
function M.sha256(msg)
  local len = #msg
  -- padding: 0x80, zeros, 8-byte big-endian bit length
  local pad = 56 - (len + 1) % 64
  if pad < 0 then pad = pad + 64 end
  msg = msg .. "\128" .. string.rep("\0", pad) .. string.pack(">I8", len * 8)

  local h0, h1, h2, h3 = 0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a
  local h4, h5, h6, h7 = 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19
  local w = {}

  for off = 1, #msg, 64 do
    local o = off
    for i = 1, 16 do
      w[i] = string.unpack(">I4", msg, o)
      o = o + 4
    end
    for i = 17, 64 do
      local x, y = w[i - 15], w[i - 2]
      local s0 = rrot(x, 7) ~ rrot(x, 18) ~ (x >> 3)
      local s1 = rrot(y, 17) ~ rrot(y, 19) ~ (y >> 10)
      w[i] = (w[i - 16] + s0 + w[i - 7] + s1) & MASK
    end

    local a, b, c, d, e, f, g, h = h0, h1, h2, h3, h4, h5, h6, h7
    for i = 1, 64 do
      local s1 = rrot(e, 6) ~ rrot(e, 11) ~ rrot(e, 25)
      local ch = (e & f) ~ ((~e & MASK) & g)
      local t1 = (h + s1 + ch + K[i] + w[i]) & MASK
      local s0 = rrot(a, 2) ~ rrot(a, 13) ~ rrot(a, 22)
      local maj = (a & b) ~ (a & c) ~ (b & c)
      local t2 = (s0 + maj) & MASK
      h, g, f, e = g, f, e, (d + t1) & MASK
      d, c, b, a = c, b, a, (t1 + t2) & MASK
    end

    h0 = (h0 + a) & MASK
    h1 = (h1 + b) & MASK
    h2 = (h2 + c) & MASK
    h3 = (h3 + d) & MASK
    h4 = (h4 + e) & MASK
    h5 = (h5 + f) & MASK
    h6 = (h6 + g) & MASK
    h7 = (h7 + h) & MASK
  end

  return string.pack(">I4>I4>I4>I4>I4>I4>I4>I4", h0, h1, h2, h3, h4, h5, h6, h7)
end

--- HMAC-SHA256(key, msg); both raw byte strings; returns 32 raw bytes.
function M.hmac_sha256(key, msg)
  if #key > 64 then key = M.sha256(key) end
  key = key .. string.rep("\0", 64 - #key)
  local ipad, opad = {}, {}
  for i = 1, 64 do
    local b = key:byte(i)
    ipad[i] = string.char(b ~ 0x36)
    opad[i] = string.char(b ~ 0x5c)
  end
  return M.sha256(table.concat(opad) .. M.sha256(table.concat(ipad) .. msg))
end

--- Lowercase hex helpers used throughout the client.
function M.to_hex(s)
  return (s:gsub(".", function(c) return string.format("%02x", c:byte()) end))
end

function M.from_hex(h)
  assert(#h % 2 == 0 and not h:match("[^0-9a-fA-F]"), "invalid hex string")
  return (h:gsub("%x%x", function(cc) return string.char(tonumber(cc, 16)) end))
end

return M
