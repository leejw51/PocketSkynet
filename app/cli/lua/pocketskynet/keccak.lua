-- keccak.lua — pure-Lua Keccak-256 (Lua 5.4+, native 64-bit integers).
--
-- Original Keccak with padding byte 0x01 — NOT NIST SHA3-256 (padding 0x06).
-- Ethereum addresses, EIP-191 digests, and EIP-55 checksums all use this
-- variant; PROTOCOL.md §1 calls the distinction out explicitly. Validated
-- byte-exactly against the protocol vectors in test.lua.

local M = {}

local RC = {
  0x0000000000000001, 0x0000000000008082, 0x800000000000808A,
  0x8000000080008000, 0x000000000000808B, 0x0000000080000001,
  0x8000000080008081, 0x8000000000008009, 0x000000000000008A,
  0x0000000000000088, 0x0000000080008009, 0x000000008000000A,
  0x000000008000808B, 0x800000000000008B, 0x8000000000008089,
  0x8000000000008003, 0x8000000000008002, 0x8000000000000080,
  0x000000000000800A, 0x800000008000000A, 0x8000000080008081,
  0x8000000000008080, 0x0000000080000001, 0x8000000080008008,
}

-- Rotation offsets, flat-indexed as state[x + 5*y + 1].
local ROT = {
  0, 1, 62, 28, 27,
  36, 44, 6, 55, 20,
  3, 10, 43, 25, 39,
  41, 45, 15, 21, 8,
  18, 2, 61, 56, 14,
}

local function rot64(a, n)
  if n == 0 then return a end
  return (a << n) | (a >> (64 - n))
end

local function keccak_f(s)
  local c = {}
  local b = {}
  for round = 1, 24 do
    -- theta
    for x = 0, 4 do
      c[x] = s[x + 1] ~ s[x + 6] ~ s[x + 11] ~ s[x + 16] ~ s[x + 21]
    end
    for x = 0, 4 do
      local d = c[(x + 4) % 5] ~ rot64(c[(x + 1) % 5], 1)
      for y = 0, 20, 5 do
        s[x + y + 1] = s[x + y + 1] ~ d
      end
    end
    -- rho + pi
    for x = 0, 4 do
      for y = 0, 4 do
        local from = x + 5 * y + 1
        b[y + 5 * ((2 * x + 3 * y) % 5) + 1] = rot64(s[from], ROT[from])
      end
    end
    -- chi
    for y = 0, 20, 5 do
      for x = 0, 4 do
        s[x + y + 1] = b[x + y + 1] ~ ((~b[(x + 1) % 5 + y + 1]) & b[(x + 2) % 5 + y + 1])
      end
    end
    -- iota
    s[1] = s[1] ~ RC[round]
  end
end

local RATE = 136 -- bytes, for a 256-bit output

--- Keccak-256 of a byte string; returns 32 raw bytes.
function M.keccak256(msg)
  local s = {}
  for i = 1, 25 do s[i] = 0 end

  -- multi-rate padding with Keccak's 0x01 domain byte
  local padlen = RATE - (#msg % RATE)
  local pad
  if padlen == 1 then
    pad = "\x81"
  else
    pad = "\x01" .. string.rep("\0", padlen - 2) .. "\x80"
  end
  msg = msg .. pad

  for off = 1, #msg, RATE do
    local o = off
    for i = 1, RATE // 8 do
      s[i] = s[i] ~ string.unpack("<I8", msg, o)
      o = o + 8
    end
    keccak_f(s)
  end

  return string.pack("<I8<I8<I8<I8", s[1], s[2], s[3], s[4])
end

return M
