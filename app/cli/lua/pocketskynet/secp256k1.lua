-- secp256k1.lua — pure-Lua secp256k1 ECDSA with RFC 6979 deterministic
-- nonces (Lua 5.4+, native 64-bit integers).
--
-- Big numbers are little-endian arrays of 16-bit limbs (16 limbs = 256
-- bits), so every limb product fits comfortably in a 64-bit Lua integer.
-- Both moduli have the shape m = 2^256 - c with a small-ish c, so modular
-- reduction is a fold (x = lo + hi*c) instead of general division. Point
-- arithmetic uses Jacobian coordinates; inverses via Fermat (a^(m-2)).
--
-- Correctness over speed: this is validated byte-exactly against
-- app/core/tests/vectors/protocol-v1.json (RFC 6979 determinism, low-S
-- normalization, v = 27/28) by test.lua.

local sha2 = require("pocketskynet.sha2")

local M = {}

---------------------------------------------------------------------------
-- 256-bit arithmetic on 16-bit limbs
---------------------------------------------------------------------------

local function trim(a)
  local n = #a
  while n > 1 and a[n] == 0 do
    a[n] = nil
    n = n - 1
  end
  return a
end

local function cmp(a, b) -- both trimmed
  local la, lb = #a, #b
  if la ~= lb then return la < lb and -1 or 1 end
  for i = la, 1, -1 do
    if a[i] ~= b[i] then return a[i] < b[i] and -1 or 1 end
  end
  return 0
end

local function is_zero(a)
  return #a == 1 and a[1] == 0
end

local function bn_add(a, b)
  local r, carry = {}, 0
  local n = math.max(#a, #b)
  for i = 1, n do
    local v = (a[i] or 0) + (b[i] or 0) + carry
    r[i] = v & 0xffff
    carry = v >> 16
  end
  if carry > 0 then r[n + 1] = carry end
  return r
end

local function bn_sub(a, b) -- requires a >= b
  local r, borrow = {}, 0
  for i = 1, #a do
    local v = a[i] - (b[i] or 0) - borrow
    if v < 0 then
      v = v + 0x10000
      borrow = 1
    else
      borrow = 0
    end
    r[i] = v
  end
  assert(borrow == 0, "bn_sub underflow")
  return trim(r)
end

local function bn_mul(a, b)
  local r = {}
  for i = 1, #a + #b do r[i] = 0 end
  for i = 1, #a do
    local ai = a[i]
    if ai ~= 0 then
      local carry = 0
      for j = 1, #b do
        local v = r[i + j - 1] + ai * b[j] + carry
        r[i + j - 1] = v & 0xffff
        carry = v >> 16
      end
      local k = i + #b
      while carry > 0 do
        local v = r[k] + carry
        r[k] = v & 0xffff
        carry = v >> 16
        k = k + 1
      end
    end
  end
  return trim(r)
end

-- Reduce x modulo m, where m = 2^256 - mc (mc precomputed).
local function bn_reduce(x, m, mc)
  x = trim(x)
  while #x > 16 do
    local lo, hi = {}, {}
    for i = 1, 16 do lo[i] = x[i] end
    for i = 17, #x do hi[i - 16] = x[i] end
    trim(lo)
    trim(hi)
    x = trim(bn_add(lo, bn_mul(hi, mc)))
  end
  while cmp(x, m) >= 0 do
    x = bn_sub(x, m)
  end
  return x
end

local function bn_from_bytes(s)
  local r = {}
  local n = #s
  for i = 1, (n + 1) // 2 do
    local lo = s:byte(n - 2 * i + 2) or 0
    local hi = s:byte(n - 2 * i + 1) or 0
    r[i] = hi * 256 + lo
  end
  return trim(r)
end

local function bn_to_bytes32(a)
  local out = {}
  for i = 16, 1, -1 do
    local v = a[i] or 0
    out[#out + 1] = string.char((v >> 8) & 0xff, v & 0xff)
  end
  return table.concat(out)
end

local function bn_from_hex(h)
  return bn_from_bytes(sha2.from_hex(h))
end

---------------------------------------------------------------------------
-- Curve constants
---------------------------------------------------------------------------

local P = bn_from_hex("fffffffffffffffffffffffffffffffffffffffffffffffffffffffefffffc2f")
local N = bn_from_hex("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141")
local HALF_N = bn_from_hex("7fffffffffffffffffffffffffffffff5d576e7357a4501ddfe92f46681b20a0")
local GX = bn_from_hex("79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798")
local GY = bn_from_hex("483ada7726a3c4655da4fbfc0e1108a8fd17b448a68554199c47d08ffb10d4b8")

local TWO256 = {}
for i = 1, 16 do TWO256[i] = 0 end
TWO256[17] = 1
local PC = bn_sub(TWO256, P) -- 2^32 + 977
local NC = bn_sub(TWO256, N)

local ONE = { 1 }
local ZERO = { 0 }

---------------------------------------------------------------------------
-- Modular helpers
---------------------------------------------------------------------------

local function mmul(a, b, m, mc)
  return bn_reduce(bn_mul(a, b), m, mc)
end

local function madd(a, b, m)
  local r = trim(bn_add(a, b))
  if cmp(r, m) >= 0 then r = bn_sub(r, m) end
  return r
end

local function msub(a, b, m)
  if cmp(a, b) >= 0 then return bn_sub(a, b) end
  return bn_sub(bn_add(a, m), b)
end

local function mpow(a, e, m, mc)
  local r = ONE
  for i = #e, 1, -1 do
    local limb = e[i]
    for bit = 15, 0, -1 do
      r = mmul(r, r, m, mc)
      if (limb >> bit) & 1 == 1 then
        r = mmul(r, a, m, mc)
      end
    end
  end
  return r
end

local function minv(a, m, mc)
  return mpow(a, bn_sub(m, { 2 }), m, mc)
end

---------------------------------------------------------------------------
-- Point arithmetic (Jacobian, a = 0)
---------------------------------------------------------------------------

local INFINITY = { x = ONE, y = ONE, z = ZERO }

local function pt_double(p)
  if is_zero(p.z) then return p end
  local ysq = mmul(p.y, p.y, P, PC)
  local s = mmul({ 4 }, mmul(p.x, ysq, P, PC), P, PC)
  local m = mmul({ 3 }, mmul(p.x, p.x, P, PC), P, PC)
  local x3 = msub(mmul(m, m, P, PC), madd(s, s, P), P)
  local y3 = msub(
    mmul(m, msub(s, x3, P), P, PC),
    mmul({ 8 }, mmul(ysq, ysq, P, PC), P, PC),
    P
  )
  local z3 = mmul({ 2 }, mmul(p.y, p.z, P, PC), P, PC)
  return { x = x3, y = y3, z = z3 }
end

-- Add an affine point q = {x, y} to a Jacobian point p.
local function pt_add_affine(p, q)
  if is_zero(p.z) then
    return { x = q.x, y = q.y, z = ONE }
  end
  local z1z1 = mmul(p.z, p.z, P, PC)
  local u2 = mmul(q.x, z1z1, P, PC)
  local s2 = mmul(q.y, mmul(p.z, z1z1, P, PC), P, PC)
  local h = msub(u2, p.x, P)
  local r = msub(s2, p.y, P)
  if is_zero(h) then
    if is_zero(r) then return pt_double(p) end
    return INFINITY
  end
  local hh = mmul(h, h, P, PC)
  local hhh = mmul(h, hh, P, PC)
  local v = mmul(p.x, hh, P, PC)
  local x3 = msub(msub(mmul(r, r, P, PC), hhh, P), madd(v, v, P), P)
  local y3 = msub(mmul(r, msub(v, x3, P), P, PC), mmul(p.y, hhh, P, PC), P)
  local z3 = mmul(p.z, h, P, PC)
  return { x = x3, y = y3, z = z3 }
end

local G_AFFINE = { x = GX, y = GY }

-- k * G, returned as affine {x, y} bignums. k must be in [1, n-1].
local function scalar_mul_g(k)
  local r = INFINITY
  for i = 255, 0, -1 do
    r = pt_double(r)
    local limb = k[(i >> 4) + 1] or 0
    if (limb >> (i & 15)) & 1 == 1 then
      r = pt_add_affine(r, G_AFFINE)
    end
  end
  assert(not is_zero(r.z), "scalar_mul_g: point at infinity")
  local zi = minv(r.z, P, PC)
  local zi2 = mmul(zi, zi, P, PC)
  return {
    x = mmul(r.x, zi2, P, PC),
    y = mmul(r.y, mmul(zi2, zi, P, PC), P, PC),
  }
end

---------------------------------------------------------------------------
-- RFC 6979 deterministic nonce generator (HMAC-SHA256 DRBG)
---------------------------------------------------------------------------

local hmac = sha2.hmac_sha256

-- Returns a closure producing successive nonce candidates k in [1, n-1].
local function rfc6979_generator(priv32, digest32)
  -- bits2octets(h1): int(h1) mod n, re-encoded as 32 bytes
  local h = bn_from_bytes(digest32)
  while cmp(h, N) >= 0 do h = bn_sub(h, N) end
  local h_oct = bn_to_bytes32(h)

  local v = string.rep("\1", 32)
  local k = string.rep("\0", 32)
  k = hmac(k, v .. "\0" .. priv32 .. h_oct)
  v = hmac(k, v)
  k = hmac(k, v .. "\1" .. priv32 .. h_oct)
  v = hmac(k, v)

  return function()
    while true do
      v = hmac(k, v)
      local candidate = bn_from_bytes(v)
      -- always advance the DRBG state so a rejected candidate (or a
      -- caller retry after r == 0 / s == 0) yields the next value
      k = hmac(k, v .. "\0")
      v = hmac(k, v)
      if not is_zero(candidate) and cmp(candidate, N) < 0 then
        return candidate
      end
    end
  end
end

---------------------------------------------------------------------------
-- Public API
---------------------------------------------------------------------------

local function check_private_key(priv32)
  assert(type(priv32) == "string" and #priv32 == 32, "private key must be 32 bytes")
  local d = bn_from_bytes(priv32)
  assert(not is_zero(d) and cmp(d, N) < 0, "private key out of range [1, n-1]")
  return d
end

--- Uncompressed public key (65 raw bytes, 0x04 || X || Y) for a 32-byte key.
function M.public_key(priv32)
  local d = check_private_key(priv32)
  local pub = scalar_mul_g(d)
  return "\4" .. bn_to_bytes32(pub.x) .. bn_to_bytes32(pub.y)
end

--- Recoverable ECDSA signature over a 32-byte digest.
--- Returns 65 raw bytes: r(32) || s(32) || v(1), with RFC 6979 nonces,
--- low-S normalization, and v = 27 + recovery_id.
function M.sign_recoverable(priv32, digest32)
  assert(type(digest32) == "string" and #digest32 == 32, "digest must be 32 bytes")
  local d = check_private_key(priv32)

  local e = bn_from_bytes(digest32)
  while cmp(e, N) >= 0 do e = bn_sub(e, N) end

  local next_k = rfc6979_generator(priv32, digest32)
  while true do
    local k = next_k()
    local pt = scalar_mul_g(k)

    local r = pt.x
    local recid = pt.y[1] & 1
    if cmp(r, N) >= 0 then
      r = bn_sub(r, N)
      recid = recid + 2
    end

    if not is_zero(r) then
      local kinv = minv(k, N, NC)
      local s = mmul(kinv, madd(e, mmul(r, d, N, NC), N), N, NC)
      if not is_zero(s) then
        if cmp(s, HALF_N) > 0 then
          s = bn_sub(N, s)
          recid = recid ~ 1
        end
        return bn_to_bytes32(r) .. bn_to_bytes32(s) .. string.char(27 + recid)
      end
    end
  end
end

-- Internal hooks for the test suite (not part of the public API).
M._internal = {
  bn_from_hex = bn_from_hex,
  bn_from_bytes = bn_from_bytes,
  bn_to_bytes32 = bn_to_bytes32,
  bn_add = bn_add,
  bn_sub = bn_sub,
  bn_mul = bn_mul,
  bn_reduce = bn_reduce,
  cmp = cmp,
  mmul = mmul,
  minv = minv,
  pt_double = pt_double,
  pt_add_affine = pt_add_affine,
  scalar_mul_g = scalar_mul_g,
  P = P, N = N, PC = PC, NC = NC, GX = GX, GY = GY,
}

return M
