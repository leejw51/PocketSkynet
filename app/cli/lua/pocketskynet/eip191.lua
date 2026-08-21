-- eip191.lua — EIP-191 personal_sign and Ethereum address utilities.
--
-- digest = keccak256(0x19 || "Ethereum Signed Message:\n"
--                         || decimal(utf8_byte_len) || message)
-- The length is the UTF-8 BYTE length, not the character count (pinned by
-- the "unicode-length-is-bytes" protocol vector). Signatures are RFC 6979
-- deterministic, low-S, v = 27/28, wire form "0x" + 130 lowercase hex.

local keccak = require("pocketskynet.keccak")
local secp = require("pocketskynet.secp256k1")
local sha2 = require("pocketskynet.sha2")

local M = {}

local function parse_private_key(key)
  local hex = key:gsub("^0x", "")
  assert(#hex == 64 and not hex:match("[^0-9a-fA-F]"),
    "private key must be 64 hex chars (optionally 0x-prefixed)")
  return sha2.from_hex(hex)
end

--- EIP-191 digest of a message string; returns 32 raw bytes.
function M.digest(message)
  return keccak.keccak256(
    "\25Ethereum Signed Message:\n" .. tostring(#message) .. message
  )
end

--- personal_sign: returns the wire signature "0x" + 130 lowercase hex.
function M.sign(private_key_hex, message)
  local priv = parse_private_key(private_key_hex)
  local sig = secp.sign_recoverable(priv, M.digest(message))
  return "0x" .. sha2.to_hex(sig)
end

--- Uncompressed public key for a private key, as 130 lowercase hex (no 0x).
function M.public_key_hex(private_key_hex)
  local priv = parse_private_key(private_key_hex)
  return sha2.to_hex(secp.public_key(priv))
end

--- Lowercase 0x-address for a private key.
--- address = "0x" + hex(keccak256(X || Y)[12..32]) — the 0x04 SEC1 prefix
--- byte is dropped before hashing (the classic porting bug).
function M.address(private_key_hex)
  local priv = parse_private_key(private_key_hex)
  local pub = secp.public_key(priv)
  local hash = keccak.keccak256(pub:sub(2)) -- X || Y, without the 0x04 byte
  return "0x" .. sha2.to_hex(hash:sub(13))
end

--- EIP-55 checksummed form of a lowercase 0x-address (display only).
function M.eip55(address)
  local lower = address:lower():gsub("^0x", "")
  assert(#lower == 40 and not lower:match("[^0-9a-f]"), "invalid address")
  local hash = sha2.to_hex(keccak.keccak256(lower))
  local out = {}
  for i = 1, 40 do
    local c = lower:sub(i, i)
    if c:match("[a-f]") and tonumber(hash:sub(i, i), 16) >= 8 then
      c = c:upper()
    end
    out[i] = c
  end
  return "0x" .. table.concat(out)
end

return M
