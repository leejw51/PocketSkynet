# PocketSkynet Protocol Specification (for porting)

Everything a port of PocketSkynet's client protocol needs — Swift, Python,
JavaScript, Kotlin, or anything else — in one document, with machine-readable
test vectors for every operation.

**Reference implementation:** the Rust crate [`app/core/`](app/core/). It is
pure protocol (no I/O, no clock, no database) and every rule below is enforced
by its test suite.

**Test vectors** (both generated/validated by the Rust suite — never edit by hand):

| File | Covers |
| --- | --- |
| [`app/core/tests/vectors/protocol-v1.json`](app/core/tests/vectors/protocol-v1.json) | wallet, EIP-191, key derivation, key binding, **full E2EE + key-rotation scenario** (`e2eeRotationScenario`), legacy v1 E2EE, msgHash, usernames, amounts, gas, ABI, RLP, EIP-155 transactions, realtime events |
| [`app/core/tests/vectors/crypto-v2.json`](app/core/tests/vectors/crypto-v2.json) | E2EE v2 primitives: subkeys, message encryption, room-key wraps (canonical, synced from FruitNation) |

A port is done when it reproduces **every** value in both files byte-for-byte
and rejects every tampering case in §12. Regenerate `protocol-v1.json` after an
intentional protocol change with:

```sh
UPDATE_PROTOCOL_VECTORS=1 cargo test -p pocketskynet-core --test protocol_vectors
```

Deeper prose lives in [`app/docs/CRYPTO.md`](app/docs/CRYPTO.md) (byte-level
crypto spec), [`app/docs/API.md`](app/docs/API.md) (REST endpoints), and
[`app/docs/REALTIME.md`](app/docs/REALTIME.md) (WebSocket/SSE). When documents
disagree, the Rust implementation + vector files win.

---

## 1. Global conventions

- `hex(x)` — **lowercase** hex, no `0x` prefix unless stated.
- `b64(x)` — **standard** Base64 (RFC 4648 §4 alphabet, `+` `/`), **with** `=`
  padding. Never URL-safe, never unpadded — the padding characters are part of
  MAC inputs and hash inputs.
- `utf8(s)` — UTF-8 bytes, no BOM, no NUL terminator.
- **Symmetric keys**: 32 bytes, on the wire as 64 lowercase hex chars.
- **IVs**: 16 bytes → 32 hex chars. **HMACs**: 32 bytes → 64 hex chars.
- **secp256k1 public keys**: uncompressed `04 ‖ X ‖ Y`, 65 bytes → **130 hex
  chars, no `0x`**. The compressed form is never used on the wire.
- **Wallet addresses**: stored and compared **lowercase** `0x` + 40 hex.
  EIP-55 checksummed form is display-only.
- **keccak256** means original Keccak (padding byte `0x01`), **not** NIST
  SHA3-256 (padding `0x06`). `msgHash` uses plain SHA-256, not Keccak.
- **MAC inputs are the received strings, verbatim.** The server accepts
  mixed-case hex for some room-key fields; never case-normalize a field before
  feeding it to a MAC.
- Constant-time comparison for all MACs. A length mismatch may short-circuit;
  content comparison must not.

Server-side wire validation your encoder must satisfy:

| Field | Rule |
| --- | --- |
| `msgHash`, message `iv`/`hmac` | lowercase hex only (`^[a-f0-9]{…}$`) |
| room-key `encryptionIV`/`hmac`/`ephemeralPublicKey` | mixed case accepted |
| `content` | 1–5000 chars, server applies `.trim()` |
| `encVer` | 1..2 · `keyVersion` | 1..1_000_000 |
| `roomId` | 10–100 chars, `[a-zA-Z0-9_.-]` |
| `messageId` | 10–100 chars, `[a-zA-Z0-9_-]` (no dot — asymmetry is deliberate) |

---

## 2. Identifiers

- `WalletAddress` — normalize to lowercase at every construction point; compare
  case-insensitively by construction. Abbreviated display: `0x742d…6b22`
  (first 6 chars ‖ `…` ‖ last 4).
- **EIP-55 checksum** (display only): `h = hex(keccak256(utf8(lower40)))`;
  uppercase hex letter *i* iff nibble *i* of `h` ≥ 8.
  Vectors: `wallet.eip55`.
- `RoomId` / `MessageId` — opaque server-assigned strings, validated per the
  table above. Reject traversal characters; a port must not be stricter than
  the wire format (room ids may contain `.`).
- **Webhook senders** — a webhook post's `senderAddress` is a derived,
  wallet-shaped address: `"0x00000000"` ‖ first 32 hex chars of
  `hex(SHA-256(utf8(webhookId)))`. It parses as an ordinary `WalletAddress`
  (a port must not special-case it on the wire), no key for it exists, and
  the reserved 4-zero-byte prefix is the display signal for "this sender is
  a webhook, not a member".

---

## 3. Wallet: mnemonic → address

Vectors: `wallet.bip39Seeds`, `wallet.accounts`, `wallet.privateKeyImports`.

1. **BIP-39** English wordlist; 12/15/18/21/24 words; checksum must validate;
   **trim the phrase** before parsing. Passphrase is always `""`.
   `seed = PBKDF2-HMAC-SHA512(NFKD(mnemonic), "mnemonic", 2048, 64)`.
2. **BIP-32/BIP-44** path **`m/44'/60'/0'/0/{index}`** (Ethereum, same as
   MetaMask). Master: `HMAC-SHA512("Bitcoin seed", seed)`. Non-hardened
   children hash the **compressed** parent public key. If a child scalar is
   invalid (~2⁻¹²⁷), surface an error — do not silently skip to the next index.
3. **Address**: `P = d·G`; `address = "0x" + hex(keccak256(X ‖ Y)[12..32])`
   — hash the 64-byte X‖Y, **dropping** the leading `0x04` SEC1 byte.
   Hashing all 65 bytes is the classic bug: it yields a valid-looking address
   belonging to nobody.
4. Private keys: reject `0` and ≥ n on import. `0x` prefix optional on input,
   emitted on output.

---

## 4. EIP-191 `personal_sign`

Used by: login challenge, encryption-key derivation, public-key binding.
Vectors: `eip191[]` (message, UTF-8 length, digest, signature).

```
digest = keccak256( 0x19 ‖ "Ethereum Signed Message:\n" ‖ decimal(byte_len) ‖ utf8(msg) )
```

- The length is the **UTF-8 byte length**, not the character count (see the
  `unicode-length-is-bytes` vector: `"🍓 strawberry"` is 15 bytes, 12 chars).
- ECDSA over secp256k1 with **RFC 6979 deterministic nonces**. Determinism is
  load-bearing: the E2EE private key is derived from a signature, so the same
  (key, message) must always produce the same 65 bytes on every device.
- Serialization: `r(32) ‖ s(32) ‖ v(1)`, `s` **low-S normalized**,
  `v = recovery_id + 27` (emit 27/28; tolerate 0/1 on input). Wire form:
  `"0x"` + 130 lowercase hex.
- Verification = recover public key from digest, derive address, compare
  lowercase. **Reject high-S signatures** (they are malleable and ethers v6
  rejects them server-side).

**Login flow**: `POST /api/auth/challenge` returns a challenge string
(template in `templates.loginChallenge`; nonce = 32 random bytes hex, expires
in 10 min, single-use). Sign it **verbatim** — never reconstruct it — and
`POST /api/auth/login { walletAddress, signature }` for a JWT.

---

## 5. E2EE key derivation

The encryption keypair is separate from the wallet keypair, derived from a
wallet signature so it is deterministic per account and identical across
mnemonic/MetaMask/Privy logins.
Vectors: `encryptionKeyDerivation.v2`, `encryptionKeyDerivation.v1Legacy`.

**v2 (salted) — current.** The salt is a per-account secret (64 hex chars)
served only to the authenticated owner (`encryptionSalt` on login, or
`GET /api/auth/encryption-salt`). Build (`templates.encryptionKeyV2`, LF
newlines, no trailing newline):

```
FruitNation Encryption Key Derivation v2\n\nAddress: {addressLower}\nSalt: {saltHex}\nPurpose: End-to-end encryption only
```

Then:

```
sig     = personal_sign(walletKey, message)          // 65 bytes
encPriv = keccak256(sig_bytes)                       // hash the 65 RAW bytes, not the "0x…" ASCII
encPub  = uncompressed(encPriv · G)                  // 130 lowercase hex, no 0x
```

Reject `encPriv` = 0 or ≥ n (≈2⁻¹²⁸) instead of panicking.

**v1 (unsalted) — LEGACY, decrypt/heal only.** Same pipeline over
`templates.encryptionKeyV1Legacy`. The message is public and constant, so any
dapp can phish the signature: **never derive a new key from it, never publish
a key derived from it.** Its only use is healing — if a v2 unwrap fails,
derive the legacy key locally (mnemonic sessions only), unwrap, and
immediately re-wrap to the v2 key at the same `keyVersion`.

---

## 6. Public-key binding (anti-MITM)

Vectors: `keyBindings[]`. Message (`templates.keyBinding`):

```
FruitNation Public Key Binding\n\nAddress: {addressLower}\nEncryption Public Key: {encPubHex}
```

Signed by the **wallet** key; published via
`PUT /api/auth/encryption-key { publicKey, publicKeySig }`.

Before wrapping a room key to anyone, a client **MUST** fetch their
`{ publicKey, publicKeySig }`, rebuild the binding message locally from the
address it *intends to share with* (never one echoed by the server) and the
exact `publicKey` string received, recover the signer, and abort on any
failure: missing key, missing/empty signature, off-curve point, malformed or
non-canonical signature, or recovered ≠ expected. **Abort, never
warn-and-continue** — this check is the only defense against a malicious
server substituting its own key.

---

## 7. Subkey derivation (label-KDF)

Vectors: `crypto-v2.json → subkeys`; labels in `templates.subkeyLabels`.

```
subkey(K, label) = HMAC-SHA256(key = K_raw_32_bytes, message = ascii(label))
```

Full 32-byte tag used directly — no truncation, no HKDF (substituting HKDF
changes every ciphertext ever produced). **Trap:** the reference TypeScript is
`CryptoJS.HmacSHA256(label, key)` whose signature is `(message, key)` — the
label is the *message*. Swapping the arguments still yields 32 plausible
bytes; only the vectors catch it. Labels (exact ASCII, case-sensitive):

```
FruitNation/v2/message/enc    FruitNation/v2/message/mac
FruitNation/v2/roomkey/enc    FruitNation/v2/roomkey/mac
```

---

## 8. Message encryption v2 (`encVer = 2`)

Vectors: `crypto-v2.json → messages`.

**Encrypt** (refuse empty/whitespace-only plaintext):

```
encKey = subkey(roomKey, ".../message/enc")
macKey = subkey(roomKey, ".../message/mac")
iv     = 16 fresh CSPRNG bytes (per message, always)
ct     = AES-256-CBC-PKCS7(encKey, iv, utf8(plaintext))
macIn  = "FNv2|message|{roomId}|{ivHex}|{b64(ct)}"      // padding included
hmac   = hex(HMAC-SHA256(macKey, utf8(macIn)))
```

Wire: `{ content: b64(ct), iv: ivHex, hmac, isEncrypted: true, encVer: 2,
keyVersion, msgHash }`.

**Decrypt — verify-then-decrypt, strictly:** rebuild `macIn` from the room the
message belongs to and the received `iv`/`content` strings verbatim; compare
the MAC in constant time; on mismatch **abort without touching AES** and
report a single opaque "decryption failed" for every failure mode (bad MAC,
bad base64, bad padding, bad UTF-8, empty plaintext) — distinguishing them
hands out an oracle.

---

## 9. Room-key wrapping v2

Vectors: `crypto-v2.json → roomKeyWraps`; ECDH checkpoint in
`protocol-v1.json → ecdh`.

```
(ephPriv, ephPub) = fresh keypair per wrap (never reused)
sharedX = X-coordinate of (ephPriv · recipientPub), 32 bytes big-endian,
          zero-left-padded — NOT hashed, NOT the full point
encKey  = subkey(sharedX, ".../roomkey/enc")
macKey  = subkey(sharedX, ".../roomkey/mac")
ct      = AES-256-CBC-PKCS7(encKey, iv, utf8( hex(roomKey) ))   // the 64-char HEX STRING, not raw bytes
macIn   = "FNv2|roomkey|{roomId}|{ephPubHex}|{ivHex}|{b64(ct)}"
```

- **Plaintext is the hex string**: 64 bytes → 80 after padding → **108 base64
  chars**. A 44-char wrap means someone encrypted the raw bytes; no other
  client can read it.
- The ephemeral public key **is** authenticated (a v1 fix).
- Room keys are 32 CSPRNG bytes; no structure, epoch lives in `keyVersion`.

**Unwrap:** parse `ephemeralPublicKey` as an uncompressed point, rejecting
off-curve and identity; ECDH; verify MAC over the received strings verbatim
and the roomId you *fetched the wrap for*; decrypt; require the plaintext to
match `^[0-9a-f]{64}$` case-insensitively; hex-decode. All failures are the
one opaque error.

---

## 10. Legacy v1 (`encVer` = 1 or missing) — decrypt-only

Vectors: `legacyV1.messages`, `legacyV1.roomKeyWraps`. Never write v1.
Dispatch: `(encVer ?? 1) >= 2 → v2, else v1`.

- **v1 messages**: AES key = the **32 raw key bytes**, but the HMAC key = the
  **64 ASCII bytes of the lowercase hex string** (a CryptoJS accident that is
  now protocol — the `rejectedHmacRawKeyHex` field pins the wrong-key value a
  port must *not* produce). MAC input = the base64 ciphertext alone: no
  version tag, no roomId, no IV.
- **v1 wraps**: raw ECDH `sharedX` used **directly** as the AES key (no KDF);
  HMAC key = ASCII hex of `sharedX`; ephemeral key and IV unauthenticated.
  Same `^[0-9a-f]{64}$/i` plaintext check.

---

## 11. Key epochs and rotation

Vectors: **`e2eeRotationScenario`** in `protocol-v1.json` — the complete
lifecycle, end to end, with every intermediate value:

- **Members**: alice (Hardhat #0), bob (`0x0123…ef`), carol (Hardhat #1),
  each with their own salt, derived v2 encryption keypair, and key-binding
  signature.
- **Epoch 1** (`keyVersion: 1`, room key = K1): wraps to all three members
  (distinct pattern ephemerals + IVs) and two messages (ASCII + Unicode)
  with `content`/`iv`/`hmac`/`msgHash` as they would go on the wire.
- **Epoch 2** (`keyVersion: 2`, room key = K2): carol removed; wraps for
  alice and bob **only**, plus one message under the new epoch.
- **Healing**: the epoch-1 key stranded in a v1 wrap to alice's *legacy*
  (unsalted) keypair, and the healed v2 re-wrap at the same `keyVersion`.
- **`expectations`**: the negative tests a port must mirror — carol's key
  fails on every epoch-2 wrap, wraps open for exactly one recipient, and
  messages never decrypt across epochs in either direction. The Rust
  generator asserts each of these while producing the file.

Rules:

- Rooms carry `currentKeyVersion` (≥1); messages carry `keyVersion`
  (missing = 1). Encrypt under the **highest** epoch you hold and stamp its
  `keyVersion`; decrypt with the message's epoch; keep every epoch ever
  received so history stays readable. Tolerate per-epoch unwrap failures
  (skip + heal), never black out all history for one bad row.
- Rotation (member removed/left → server sets `keyRotationPending` and
  rejects encrypted posts with `409 KEY_ROTATION_REQUIRED`): any member
  generates a fresh key, `newVersion = current + 1`, runs the §6 binding check
  for **every** member, wraps to each, and posts atomically to
  `/api/rooms/:id/rotate-key`. A member *joining* an encrypted room through an
  invite link (`/api/invites/:token/redeem`, API.md §6.7a) sets the same flag
  from the other side — the joiner holds no wrap for the current epoch, and
  the full-coverage rule above is what gets them one at the next rotation.
- `409 STALE_KEY_VERSION` → clear cache, refetch epochs, re-encrypt, retry
  once. `409 KEY_ROTATION_REQUIRED` → perform the rotation, then retry.
- Honesty notes for UI: no backward secrecy, no per-message ratchet, metadata
  visible to the server, rotation integrity unverifiable by the server.

---

## 12. Negative tests (required)

A port must reject — identically and opaquely — for both messages and wraps:
flipped ciphertext, flipped IV, flipped/truncated/extended HMAC, cross-room
replay (same fields, different roomId), wrong key, ephemeral-key substitution
(valid but different point), off-curve ephemeral key, and an authentic wrap
whose plaintext is not 64 hex chars. The Rust suite
(`core/tests/vectors.rs::canonical_vectors_reject_tampering`) is the model.

---

## 13. `msgHash`

`hex(SHA-256(utf8(S)))`, lowercase — plain SHA-256, never Keccak.
Vectors: `msgHash.*`.

| Event | `S` |
| --- | --- |
| encrypted message / edit | the **base64 ciphertext string** as sent (padding included) — never the plaintext, which would enable dictionary confirmation of short messages |
| plaintext message / edit | the **trimmed** content (server trims before storing) |
| delete | none — server force-sets `msgHash = ""` |
| emoticon add/remove | `"{messageId}:{code}:{add\|remove}:{senderLower}:{timestampMs}"` — server-computed (uses server time); clients store what they receive |

On-chain publishing: the server verifies the tx calldata **contains** the bare
64 hex chars of `msgHash`.

---

## 14. Deterministic usernames and room names

Vectors: `usernames[]`, `roomNames[]`. Word lists: 152 adjectives × 156 nouns,
transcribed in [`app/core/src/username.rs`](app/core/src/username.rs) —
**order is protocol**; inserting a word renames accounts.

```
hash   = keccak256(utf8(lowercase(address)))      // "0x" included
adj    = be_u16(hash[0..2]) % 152
noun   = be_u16(hash[2..4]) % 156
suffix = be_u16(hash[4..6]) % 10000               // zero-padded to 4 digits
username = ADJECTIVES[adj] + NOUNS[noun] + suffix     // "OmegaMustang0198"
roomName = ADJ + " " + NOUN + " " + suffix            // same picks over arbitrary entropy
```

Hash the **address** (lowercased first), never the credential.

---

## 15. Amounts, quantities, gas

Vectors: `amounts.*`, `intrinsicGas[]`.

- Human decimal ↔ base units by **pure string/integer arithmetic** — never
  floats (`0.1` CRO is not representable in f64 wei). Reject more fractional
  digits than the token's decimals (no silent truncation). Formatting: no
  scientific notation, trim trailing zeros, drop the point for whole numbers.
- JSON-RPC quantities: minimal `0x` hex (`0x0`, `0x1b4`).
- ABI `uint256` returns: `"0x"` → 0 (missing contract); wrong width or a value
  above the port's integer range → error, never truncate.
- **Cronos intrinsic gas** (differs from Ethereum's 16/4!):
  `21000 + ceil(1.2 × (40·nonzero_bytes + 10·zero_bytes))`, integer form
  `21000 + ceil(6·g/5)`.

Networks: Cronos mainnet chain 25 (CRO, USDC at
`0xc21223249ca28397b4b6541dffaecc539bff0c59`), testnet 338 (TCRO). Full
registry in [`app/core/src/chain.rs`](app/core/src/chain.rs) /
`GET /api/networks`.

---

## 16. EVM transactions (legacy type-0, EIP-155)

Vectors: `transactions[]`, `rlp[]`.

```
sighash = keccak256(rlp([nonce, gasPrice, gasLimit, to, value, data, chainId, 0, 0]))
sign    → RFC 6979; v = chainId·2 + 35 + recoveryId
raw     = rlp([nonce, gasPrice, gasLimit, to, value, data, v, r, s])
txHash  = keccak256(raw)
```

RLP notes: integers are minimal big-endian (zero = empty string, `0x80`);
single byte < `0x80` is its own encoding; `r`/`s` are stripped of leading
zeros; contract creation encodes `to` as the empty string. Only legacy
transactions are used — Cronos ignores EIP-1559 fields. The chain-1 vector is
the EIP-155 spec example; every correct implementation reproduces it exactly.

---

## 17. ABI encoding (Bank: ERC-20, Greeter, VVS swaps)

Vectors: `abi.selectors`, `abi.calls`, `slippage.*`.

- Selector = first 4 bytes of `keccak256(canonicalSignature)` — no spaces, no
  parameter names.
- Static args (uint, address) occupy one 32-byte word in the head; dynamic
  args (string, address[]) put a byte-offset in the head and
  `length-word ‖ padded-data` in the tail. String lengths count **bytes**
  (see the Korean `setGreeting` vector). Deploy data = creation bytecode
  (`app/core/src/contracts/*.hex`) ‖ constructor args.
- VVS router `0x145863eb42cf62847a6ca784e6416c1682b1b2ae` and WCRO
  `0x5c7f8a570d578ed84e63fdfa7b1ee72deae1ae23` exist on **chain 25 only**.
- Slippage: `amountOutMin = amount × (10000 − bps) / 10000`, integer,
  saturating; percent input clamps to [0.1, 50], defaults 0.5% on garbage.

---

## 18. Realtime events

Vectors: `realtimeEvents.*` — the exact JSON for every server event, client
message, and fan-out target. One shape across WebSocket frames, SSE `data:`
payloads, and JSONL logs; the SSE `event:` name always equals the JSON `type`
tag. Events are **wake-up signals, not content** (`new_message` carries
`roomId` + `msgSerial`, never a body — fetch `/api/rooms/:id/sync?since=`).
`typing`, `shout`, and `pong` are never replayed on resume;
`resync_required` means do a full sync. Client → server is only
`{"type":"ping"}` and `{"type":"typing","roomId":…}`.

---

## 19. Library mapping per language

Primitives needed: secp256k1 (ECDSA + recovery + ECDH), Keccak-256, SHA-256,
HMAC-SHA256, PBKDF2-HMAC-SHA512 (BIP-39), HMAC-SHA512 (BIP-32),
AES-256-CBC + PKCS#7, standard Base64, hex, CSPRNG.

| Need | Swift | Python | JavaScript/TypeScript |
| --- | --- | --- | --- |
| secp256k1 sign/recover/ECDH | [swift-secp256k1](https://github.com/21-DOT-DEV/swift-secp256k1) | `coincurve` (libsecp256k1) | `@noble/secp256k1` or `@noble/curves` |
| Keccak-256 | CryptoSwift `SHA3(variant: .keccak256)` | `pycryptodome` `keccak` (not `hashlib.sha3_256`!) | `@noble/hashes/sha3` `keccak_256` |
| SHA-256 / HMAC / PBKDF2 | CryptoKit (`SHA256`, `HMAC`, or CommonCrypto for PBKDF2) | `hashlib` / `hmac` | `@noble/hashes`, or WebCrypto |
| AES-256-CBC + PKCS#7 | CryptoSwift `AES(..., blockMode: CBC, padding: .pkcs7)` | `pycryptodome` `AES.MODE_CBC` + pad/unpad | `@noble/ciphers` `cbc`, or WebCrypto `AES-CBC` |
| BIP-39/32 | wrap the primitives above (or WalletKit) | `bip_utils` or hand-roll (§3 is ~40 lines) | `@scure/bip39` + `@scure/bip32` |
| CSPRNG | `SecRandomCopyBytes` | `secrets` / `os.urandom` | `crypto.getRandomValues` |

Universal traps, one line each — every one is pinned by a vector:

1. Keccak-256 ≠ SHA3-256 (padding byte differs).
2. Address = keccak of X‖Y **without** the `0x04` prefix; take the *last* 20 bytes.
3. EIP-191 length is UTF-8 **bytes**, not characters.
4. Signatures: RFC 6979, low-S, `v ∈ {27, 28}`.
5. `encPriv = keccak(65 raw signature bytes)`, not the ASCII `"0x…"` string.
6. Label-KDF argument order: the label is the HMAC *message*, the key material is the *key*.
7. ECDH shared secret = the X coordinate alone, zero-left-padded to 32 bytes, **unhashed**.
8. Room-key wrap plaintext = the 64-char hex *string* (108-char base64 wrap), not raw bytes.
9. v1 HMAC keys are the ASCII hex of the key, while v1 AES keys are the raw bytes.
10. Base64 is standard-alphabet **with padding**; the `=` signs are MAC'd and hashed.
11. MAC the received strings verbatim; never case-normalize first.
12. Verify-then-decrypt; one opaque error for every decryption failure.
13. `msgHash` of an encrypted message hashes the base64 *ciphertext*, never the plaintext.
14. Trim plaintext before hashing/sending (the server trims `content`).
15. Cronos intrinsic gas is 40/10 per data byte with a ×1.2 margin, not Ethereum's 16/4.
16. Amounts are integer math end-to-end; JSON carries big values as strings (JS number ≈ 2⁵³).

### Suggested port order

Each step is verifiable against vectors before the next: hex/base64/keccak →
addresses + EIP-55 (§2–3) → EIP-191 (§4) → key derivation + binding (§5–6) →
label-KDF (§7) → message v2 (§8) → wraps v2 (§9) → legacy v1 (§10) → msgHash
(§13) → usernames (§14) → amounts/gas (§15) → RLP + EIP-155 (§16) → ABI (§17)
→ events (§18) → negative tests (§12) → and finish with the
**`e2eeRotationScenario`** (§11), which exercises everything from §4–§10
together and is the best single indicator that the port is actually done.
