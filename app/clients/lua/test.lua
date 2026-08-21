#!/usr/bin/env lua
-- test.lua — validates the Lua client's crypto byte-exactly against the
-- canonical protocol vectors (app/core/tests/vectors/protocol-v1.json).
--
-- Run from anywhere:
--   lua app/clients/lua/test.lua
--
-- Covers: SHA-256 / HMAC-SHA256 (FIPS / RFC 4231 vectors), Keccak-256,
-- EIP-191 digests, RFC 6979 deterministic low-S signatures (v = 27/28),
-- private-key → public-key → address derivation, and EIP-55 checksums.

local script_dir = arg[0]:match("^(.*)[/\\][^/\\]*$") or "."
package.path = script_dir .. "/?.lua;" .. package.path

local json = require("pocketskynet.json")
local sha2 = require("pocketskynet.sha2")
local keccak = require("pocketskynet.keccak")
local eip191 = require("pocketskynet.eip191")

local VECTORS_PATH = script_dir .. "/../../core/tests/vectors/protocol-v1.json"

local passed, failed = 0, 0

local function check(name, got, want)
  if got == want then
    passed = passed + 1
  else
    failed = failed + 1
    io.write("FAIL ", name, "\n  want: ", tostring(want), "\n  got:  ", tostring(got), "\n")
  end
end

---------------------------------------------------------------------------
-- Primitive self-tests (published test vectors)
---------------------------------------------------------------------------

check("sha256(empty)", sha2.to_hex(sha2.sha256("")),
  "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
check("sha256(abc)", sha2.to_hex(sha2.sha256("abc")),
  "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
check("sha256(two-block)", sha2.to_hex(sha2.sha256(
  "abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
  "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1")

-- RFC 4231 test case 1 and 2
check("hmac-sha256(rfc4231#1)",
  sha2.to_hex(sha2.hmac_sha256(string.rep("\x0b", 20), "Hi There")),
  "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7")
check("hmac-sha256(rfc4231#2)",
  sha2.to_hex(sha2.hmac_sha256("Jefe", "what do ya want for nothing?")),
  "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843")

-- Keccak-256 (original padding 0x01, NOT SHA3-256)
check("keccak256(empty)", sha2.to_hex(keccak.keccak256("")),
  "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470")
check("keccak256(abc)", sha2.to_hex(keccak.keccak256("abc")),
  "4e03657aea45a94fc7d47ba826c8d667c0d1e6e33a64a036ec44f58fa12d6c45")
-- rate-boundary case: exactly one full 136-byte block of input (multi-block
-- absorption is also pinned by the >136-byte login-challenge EIP-191 vector)
check("keccak256(136*a)", sha2.to_hex(keccak.keccak256(string.rep("a", 136))),
  "a6c4d403279fe3e0af03729caada8374b5ca54d8065329a3ebcaeb4b60aa386e")

---------------------------------------------------------------------------
-- Protocol vectors
---------------------------------------------------------------------------

local f = assert(io.open(VECTORS_PATH, "rb"),
  "cannot open protocol vectors at " .. VECTORS_PATH)
local vectors = json.decode(f:read("a"))
f:close()

-- eip191[]: digest, signature, and key → address, byte-exact
for _, v in ipairs(vectors.eip191) do
  local name = "eip191/" .. v.name
  check(name .. "/utf8-len", #v.message, math.floor(v.messageUtf8Len))
  check(name .. "/digest", sha2.to_hex(eip191.digest(v.message)), v.digestHex)
  check(name .. "/signature", eip191.sign(v.privateKeyHex, v.message), v.signatureHex)
  check(name .. "/address", eip191.address(v.privateKeyHex), v.address)
end

-- wallet.privateKeyImports: key → uncompressed pubkey → address → EIP-55
for i, v in ipairs(vectors.wallet.privateKeyImports) do
  local name = "wallet/privateKeyImport#" .. i
  check(name .. "/publicKey", eip191.public_key_hex(v.privateKeyHex),
    v.publicKeyUncompressedHex)
  check(name .. "/address", eip191.address(v.privateKeyHex), v.address)
  check(name .. "/eip55", eip191.eip55(v.address), v.addressChecksummed)
end

-- wallet.accounts carry derived private keys too — reuse them as extra
-- key → address checks (BIP-32/39 derivation itself is out of scope here).
for i, v in ipairs(vectors.wallet.accounts or {}) do
  if v.privateKeyHex and v.address then
    check("wallet/account#" .. i .. "/address",
      eip191.address(v.privateKeyHex), v.address)
  end
end

-- wallet.eip55: checksum display form
for i, v in ipairs(vectors.wallet.eip55) do
  check("wallet/eip55#" .. i, eip191.eip55(v.lower), v.checksummed)
end

---------------------------------------------------------------------------
-- Report
---------------------------------------------------------------------------

print(string.format("%d passed, %d failed", passed, failed))
if failed > 0 then os.exit(1) end
