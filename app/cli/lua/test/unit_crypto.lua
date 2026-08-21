-- unit_crypto.lua — hash and signature units, byte-exact against the
-- canonical protocol vectors (app/core/tests/vectors/protocol-v1.json)
-- plus published FIPS/RFC vectors for the primitives.

local json = require("pocketskynet.json")
local sha2 = require("pocketskynet.sha2")
local keccak = require("pocketskynet.keccak")
local eip191 = require("pocketskynet.eip191")
local secp = require("pocketskynet.secp256k1")

local test_dir = debug.getinfo(1, "S").source:match("^@(.*)[/\\][^/\\]*$") or "."
local VECTORS_PATH = test_dir .. "/../../../core/tests/vectors/protocol-v1.json"

return function(T)
  -------------------------------------------------------------------------
  -- SHA-256: FIPS 180-4 vectors + padding-boundary lengths
  -------------------------------------------------------------------------

  local sha_vectors = {
    { "", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855" },
    { "abc", "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad" },
    { "abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
      "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1" },
    -- padding boundaries: 55 fits with length in one block, 56 forces a
    -- second block, 64 is exactly one block, 65 starts a second
    { string.rep("a", 55), "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318" },
    { string.rep("a", 56), "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a" },
    { string.rep("a", 63), "7d3e74a05d7db15bce4ad9ec0658ea98e3f06eeecf16b4c6fff2da457ddc2f34" },
    { string.rep("a", 64), "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb" },
    { string.rep("a", 65), "635361c48bb9eab14198e76ea8ab7f1a41685d6ad62aa9146d301d4f17eb0ae0" },
    { string.rep("a", 119), "31eba51c313a5c08226adf18d4a359cfdfd8d2e816b13f4af952f7ea6584dcfb" },
    { string.rep("a", 120), "2f3d335432c70b580af0e8e1b3674a7c020d683aa5f73aaaedfdc55af904c21c" },
  }
  for i, v in ipairs(sha_vectors) do
    T.test(string.format("sha256 vector #%d (len %d)", i, #v[1]), function()
      T.eq(sha2.to_hex(sha2.sha256(v[1])), v[2])
    end)
  end

  -------------------------------------------------------------------------
  -- HMAC-SHA256: RFC 4231, including the >64-byte-key cases
  -------------------------------------------------------------------------

  T.test("hmac-sha256 rfc4231 #1", function()
    T.eq(sha2.to_hex(sha2.hmac_sha256(string.rep("\x0b", 20), "Hi There")),
      "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7")
  end)
  T.test("hmac-sha256 rfc4231 #2 (short key)", function()
    T.eq(sha2.to_hex(sha2.hmac_sha256("Jefe", "what do ya want for nothing?")),
      "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843")
  end)
  T.test("hmac-sha256 rfc4231 #3 (20-byte 0xaa key)", function()
    T.eq(sha2.to_hex(sha2.hmac_sha256(string.rep("\xaa", 20), string.rep("\xdd", 50))),
      "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe")
  end)
  T.test("hmac-sha256 rfc4231 #6 (131-byte key, hashed down)", function()
    T.eq(sha2.to_hex(sha2.hmac_sha256(string.rep("\xaa", 131),
      "Test Using Larger Than Block-Size Key - Hash Key First")),
      "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54")
  end)
  T.test("hmac-sha256 rfc4231 #7 (131-byte key, long message)", function()
    T.eq(sha2.to_hex(sha2.hmac_sha256(string.rep("\xaa", 131),
      "This is a test using a larger than block-size key and a larger than " ..
      "block-size data. The key needs to be hashed before being used by the " ..
      "HMAC algorithm.")),
      "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2")
  end)

  -------------------------------------------------------------------------
  -- Keccak-256 (original 0x01 padding, not SHA3-256)
  -------------------------------------------------------------------------

  T.test("keccak256 empty (the famous Ethereum constant)", function()
    T.eq(sha2.to_hex(keccak.keccak256("")),
      "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470")
  end)
  T.test("keccak256 abc", function()
    T.eq(sha2.to_hex(keccak.keccak256("abc")),
      "4e03657aea45a94fc7d47ba826c8d667c0d1e6e33a64a036ec44f58fa12d6c45")
  end)
  T.test("keccak256 differs from sha3-256 (padding byte)", function()
    -- SHA3-256("") = a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a
    T.ok(sha2.to_hex(keccak.keccak256("")) ~=
      "a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a",
      "keccak must not equal SHA3-256")
  end)
  T.test("keccak256 exactly one rate block (136 bytes)", function()
    -- cross-checked against an independent Python implementation of the spec
    T.eq(sha2.to_hex(keccak.keccak256(string.rep("a", 136))),
      "a6c4d403279fe3e0af03729caada8374b5ca54d8065329a3ebcaeb4b60aa386e")
  end)
  T.test("keccak256 single-byte 0x81 padding case (135 bytes)", function()
    -- 135-byte input leaves exactly one padding byte in the block;
    -- cross-checked against an independent Python implementation
    T.eq(sha2.to_hex(keccak.keccak256(string.rep("a", 135))),
      "34367dc248bbd832f4e3e69dfaac2f92638bd0bbd18f2912ba4ef454919cf446")
  end)

  -------------------------------------------------------------------------
  -- Protocol vectors
  -------------------------------------------------------------------------

  local f = assert(io.open(VECTORS_PATH, "rb"),
    "cannot open protocol vectors at " .. VECTORS_PATH)
  local vectors = json.decode(f:read("a"))
  f:close()

  -- eip191[]: byte length, digest, signature, and key → address — all exact
  for _, v in ipairs(vectors.eip191) do
    T.test("eip191/" .. v.name, function()
      T.eq(#v.message, math.floor(v.messageUtf8Len), "UTF-8 byte length")
      T.eq(sha2.to_hex(eip191.digest(v.message)), v.digestHex, "digest")
      T.eq(eip191.sign(v.privateKeyHex, v.message), v.signatureHex, "signature")
      T.eq(eip191.address(v.privateKeyHex), v.address, "address")
    end)
  end

  T.test("eip191 length prefix counts bytes, not characters", function()
    -- "🍓 strawberry" is 12 characters but 15 UTF-8 bytes; the vector digest
    -- above already pins this — here the wrong (character-count) digest is
    -- shown to differ, so a codepoint-count bug cannot pass both.
    local msg = "\240\159\141\147 strawberry"
    local right = keccak.keccak256("\25Ethereum Signed Message:\n15" .. msg)
    local wrong = keccak.keccak256("\25Ethereum Signed Message:\n12" .. msg)
    T.eq(sha2.to_hex(eip191.digest(msg)), sha2.to_hex(right))
    T.ok(sha2.to_hex(right) ~= sha2.to_hex(wrong), "byte vs char digests must differ")
  end)

  -- wallet.privateKeyImports: key → pubkey → address → EIP-55
  for i, v in ipairs(vectors.wallet.privateKeyImports) do
    T.test("wallet/privateKeyImport#" .. i, function()
      T.eq(eip191.public_key_hex(v.privateKeyHex), v.publicKeyUncompressedHex, "public key")
      T.eq(eip191.address(v.privateKeyHex), v.address, "address")
      T.eq(eip191.eip55(v.address), v.addressChecksummed, "EIP-55")
    end)
  end

  -- wallet.accounts: derived keys reused as extra key → address checks
  -- (BIP-32/39 derivation itself is out of the client's scope)
  for i, v in ipairs(vectors.wallet.accounts or {}) do
    T.test("wallet/account#" .. i .. " key->address", function()
      T.eq(eip191.address(v.privateKeyHex), v.address)
      T.eq(eip191.eip55(v.address), v.addressChecksummed, "EIP-55")
    end)
  end

  -- wallet.eip55 display checksums
  for i, v in ipairs(vectors.wallet.eip55) do
    T.test("wallet/eip55#" .. i, function()
      T.eq(eip191.eip55(v.lower), v.checksummed)
    end)
  end

  -- msgHash: plain SHA-256, never keccak (PROTOCOL.md §13)
  for i, v in ipairs(vectors.msgHash.plaintext) do
    T.test("msgHash/plaintext#" .. i, function()
      local content = v.content:match("^%s*(.-)%s*$") -- server trims first
      if v.trimmedTo then T.eq(content, v.trimmedTo, "trim") end
      T.eq(sha2.to_hex(sha2.sha256(content)), v.msgHashHex)
    end)
  end
  for i, v in ipairs(vectors.msgHash.encrypted) do
    T.test("msgHash/encrypted#" .. i .. " hashes the base64 ciphertext", function()
      T.eq(sha2.to_hex(sha2.sha256(v.ciphertextBase64)), v.msgHashHex)
    end)
  end
  for i, v in ipairs(vectors.msgHash.emoticon) do
    T.test("msgHash/emoticon#" .. i, function()
      local event = string.format("%s:%s:%s:%s:%d",
        v.messageId, v.emoticonCode, v.action, v.senderAddress,
        math.floor(v.timestampMs))
      T.eq(event, v.eventData, "event string")
      T.eq(sha2.to_hex(sha2.sha256(event)), v.msgHashHex)
    end)
  end
  T.test("msgHash/delete is the empty string, not a hash", function()
    T.eq(vectors.msgHash.delete.msgHash, "")
  end)

  -------------------------------------------------------------------------
  -- secp256k1 edges
  -------------------------------------------------------------------------

  local N_HEX = "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141"
  local HALF_N = "7fffffffffffffffffffffffffffffff5d576e7357a4501ddfe92f46681b20a0"

  T.test("secp256k1 rejects private key 0", function()
    T.err_match(function()
      secp.public_key(string.rep("\0", 32))
    end, "out of range")
    T.err_match(function()
      secp.sign_recoverable(string.rep("\0", 32), sha2.sha256("x"))
    end, "out of range")
  end)

  T.test("secp256k1 rejects private key = n and > n", function()
    T.err_match(function()
      secp.public_key(sha2.from_hex(N_HEX))
    end, "out of range")
    T.err_match(function()
      secp.public_key(string.rep("\xff", 32))
    end, "out of range")
  end)

  T.test("secp256k1 accepts private key = n-1", function()
    -- n-1 is the largest valid key; its public key is -G, sharing G's x
    local n_minus_1 = sha2.from_hex(
      "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364140")
    local pub = sha2.to_hex(secp.public_key(n_minus_1))
    T.eq(pub:sub(3, 66),
      "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
      "x coordinate must equal G.x")
    local sig = secp.sign_recoverable(n_minus_1, sha2.sha256("hello"))
    T.eq(#sig, 65)
  end)

  T.test("signatures are deterministic (RFC 6979)", function()
    local key = sha2.from_hex(
      "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80")
    local digest = sha2.sha256("determinism check")
    T.eq(sha2.to_hex(secp.sign_recoverable(key, digest)),
      sha2.to_hex(secp.sign_recoverable(key, digest)))
  end)

  T.test("every produced signature is low-S with v in {27, 28}", function()
    local key = sha2.from_hex(
      "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80")
    for i = 1, 12 do
      local sig = secp.sign_recoverable(key, sha2.sha256("message " .. i))
      T.eq(#sig, 65, "signature length")
      local s_hex = sha2.to_hex(sig:sub(33, 64))
      T.ok(s_hex <= HALF_N, "s must be low (got " .. s_hex .. ")")
      local v = sig:byte(65)
      T.ok(v == 27 or v == 28, "v must be 27 or 28 (got " .. v .. ")")
    end
  end)

  T.test("eip191.sign wire form is 0x + 130 lowercase hex", function()
    local sig = eip191.sign(
      "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80", "hi")
    T.eq(#sig, 132, "length: 0x + 130 hex chars")
    T.eq(sig:sub(1, 2), "0x", "0x prefix")
    T.ok(not sig:sub(3):match("[^0-9a-f]"), "lowercase hex only")
  end)

  T.test("malformed private keys are rejected before signing", function()
    T.err_match(function() eip191.sign("0xzz", "m") end, "64 hex")
    T.err_match(function() eip191.sign("1234", "m") end, "64 hex")
    T.err_match(function()
      eip191.sign("0x" .. string.rep("g", 64), "m")
    end, "64 hex")
  end)

  T.test("eip55 rejects a malformed address", function()
    T.err_match(function() eip191.eip55("0x1234") end, "invalid address")
  end)
end
