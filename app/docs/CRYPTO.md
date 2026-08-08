# PocketSkynet Cryptography Specification

Byte-exact specification of every cryptographic operation in the FruitNation
protocol, written for a **Rust** implementation targeting both native and
`wasm32-unknown-unknown`.

**Authority order** (when documents disagree, the higher entry wins):

1. `server/client/src/lib/encryption.ts` — canonical implementation
2. `server/test/vectors/crypto-v2.json` — canonical test vectors
3. `server/server/routes.ts`, `server/shared/schema.ts`, `server/server/security.ts` — wire validation
4. `server/CRYPTO.md`, `server/PROTOCOL.md` — prose

> **Never** use `server/test/test-vectors.json`. It contains wrong expected
> values. The only canonical vector file is `server/test/vectors/crypto-v2.json`.

Every hex/base64 value in this document was recomputed from first principles
(Node `crypto`, no CryptoJS) and matches the canonical vectors exactly.

---

## 0. Notation and global conventions

| Symbol | Meaning |
| --- | --- |
| `hex(x)` | **lowercase** hex, no `0x` prefix unless stated |
| `b64(x)` | **standard** Base64, RFC 4648 §4 alphabet (`A–Z a–z 0–9 + /`), **with** `=` padding |
| `utf8(s)` | UTF-8 byte encoding of string `s` (no BOM, no NUL terminator) |
| `‖` | byte concatenation |

Global rules:

- **Symmetric keys** are 32 bytes, carried on the wire as 64 lowercase hex chars.
- **IVs** are 16 bytes, carried as 32 hex chars.
- **HMACs** are 32 bytes, carried as 64 lowercase hex chars.
- **secp256k1 public keys** are **uncompressed**: `04 ‖ X ‖ Y`, 65 bytes,
  carried as **130 hex chars with no `0x` prefix**. Compressed (33-byte) form is
  never used on the wire.
- **Ciphertext** is Base64 of the raw AES output. There is **no** OpenSSL
  `Salted__` header (CryptoJS only emits that when it derives a key from a
  passphrase; FruitNation always passes an explicit key).
- **Wallet addresses** are stored and compared **lowercased**. EIP-55
  checksummed form is display-only.
- **MAC inputs** are ASCII/UTF-8 string concatenations joined by `|`. `roomId`
  is server-restricted to `[A-Za-z0-9_.-]`, and hex/base64 never contain `|`,
  so the encoding is unambiguous.

### Wire-format validation the server enforces

These are hard constraints your Rust encoder must satisfy (`server/security.ts`):

| Field | Regex / bound |
| --- | --- |
| `msgHash` | `^[a-f0-9]{64}$` — **lowercase only** |
| message `iv` | `^[a-f0-9]{32}$` — **lowercase only** |
| message `hmac` | `^[a-f0-9]{64}$` — **lowercase only** |
| room-key `encryptionIV` | `^[a-fA-F0-9]{32}$` — mixed case **accepted** |
| room-key `hmac` | `^[a-fA-F0-9]{64}$` — mixed case **accepted** |
| `ephemeralPublicKey` | `^[a-fA-F0-9]+$`, ≤ 256 chars — mixed case **accepted** |
| `encryptedSymmetricKey` | 1..1024 chars, no charset restriction |
| `content` | 1..5000 chars, **server-side `.trim()` is applied** |
| `encVer` | integer 1..2 |
| `keyVersion` | integer 1..1_000_000 |
| `roomId` | 10..100 chars, `^[a-zA-Z0-9_.-]+$` |
| `publicKey` | `^[a-fA-F0-9]+$`, ≤ 130 chars |
| `publicKeySig` | `^0x[a-fA-F0-9]+$`, ≤ 200 chars |

> **Trap — MAC over received strings.** Because the server accepts mixed-case
> hex for `ephemeralPublicKey`, `encryptionIV` and room-key `hmac`, a verifier
> **MUST** recompute the MAC over the *exact strings it received*, byte for
> byte. Do **not** lowercase, uppercase, or otherwise normalize them before
> feeding them into the MAC input. A well-behaved encoder always emits
> lowercase; a verifier must not assume it.

> **Trap — server trims `content`.** `content` is `z.string().min(1).max(5000).transform(s => s.trim())`.
> Base64 ciphertext has no leading/trailing whitespace, so encrypted messages
> are unaffected. For **plaintext** messages you MUST trim the string *before*
> computing `msgHash`, or the stored content and the stored hash will disagree.
> The reference client does `message.trim()` before hashing.

---

## 1. Wallet: mnemonic → address

Reference: `server/client/src/services/wallet.ts` (`generateWalletFromMnemonic`).

### 1.1 BIP-39 mnemonic → seed

- Wordlist: **BIP-39 English**.
- Valid lengths: 12/15/18/21/24 words; checksum must validate.
- Passphrase: **empty string** (`""`). FruitNation never uses a BIP-39 passphrase.
- Seed = `PBKDF2-HMAC-SHA512(password = utf8(NFKD(mnemonic)), salt = utf8("mnemonic" ‖ passphrase), iterations = 2048, dkLen = 64)`.

The reference client calls `Mnemonic.fromPhrase(phrase.trim())` — **trim the
phrase** before validation/derivation.

### 1.2 BIP-32 derivation

- Master key: `I = HMAC-SHA512(key = utf8("Bitcoin seed"), msg = seed)`;
  `k_master = I[0..32]`, `chaincode = I[32..64]`.
- Path: **`m/44'/60'/0'/0/{index}`** (BIP-44, Ethereum coin type 60).
  `index` defaults to `0`. Hardened indices are `0x80000000 + i`.
- Result: 32-byte private key `d`, `1 ≤ d < n`.

### 1.3 secp256k1 keypair → Ethereum address

```
P            = d · G                      (secp256k1 base point)
uncompressed = 04 ‖ X ‖ Y                 (65 bytes)
addr_bytes   = keccak256(X ‖ Y)[12..32]   (drop the 0x04 prefix; take LAST 20 bytes)
address      = "0x" ‖ hex(addr_bytes)     (lowercase, canonical storage form)
```

**`keccak256` is original Keccak (padding `0x01`), NOT NIST SHA3-256
(padding `0x06`).** In Rust: `sha3::Keccak256`, never `sha3::Sha3_256`.

### 1.4 EIP-55 checksum (display only)

```
a = lowercase hex address WITHOUT "0x"     (40 chars)
h = hex(keccak256(utf8(a)))                (64 chars, lowercase)
for i in 0..40:
    out[i] = if a[i] is a letter and hexval(h[i]) >= 8 { a[i].to_ascii_uppercase() }
             else { a[i] }
address = "0x" + out
```

Worked example:

```
lowercase : 0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266
EIP-55    : 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266

lowercase : 0xfcad0b19bb29d4674531d6f115237e16afce377c
EIP-55    : 0xFCAd0B19bB29D4674531d6f115237E16AfCE377c
```

**The API always takes and returns lowercase addresses.** Send
`walletAddress.toLowerCase()` everywhere; compare case-insensitively.

---

## 2. EIP-191 `personal_sign`

Used for: the login challenge, the encryption-key derivation message, and the
public-key binding message.

### 2.1 Digest construction

```
prefix  = "\x19Ethereum Signed Message:\n" ‖ decimal_ascii(len_bytes(utf8(message)))
digest  = keccak256( utf8(prefix) ‖ utf8(message) )
```

- The `\x19` is a single byte `0x19`, **not** the two ASCII chars `\` `x19`.
- The length is the **UTF-8 byte length** of the message, not the character
  count. For a message with multi-byte characters these differ.
- The length is written in decimal ASCII with no padding.
- Prefix bytes for a 126-byte message:
  `19457468657265756d205369676e6564204d6573736167653a0a313236`
  (= `0x19` ‖ `"Ethereum Signed Message:\n"` ‖ `"126"`).

### 2.2 Signature serialization

ECDSA over secp256k1, **RFC 6979 deterministic nonce** (so the same wallet +
same message always yields the same 65 bytes — this is load-bearing: the E2EE
private key is derived from a signature, and multi-device access depends on it
being reproducible).

```
signature = r (32 bytes, big-endian)
          ‖ s (32 bytes, big-endian)
          ‖ v (1 byte)
```

- `s` **MUST be low-S normalized**: `s ≤ n/2`; if the raw `s > n/2`, use
  `n - s` and flip the recovery bit.
- `v = recovery_id + 27`, i.e. **`27` (0x1b) or `28` (0x1c)**. This is the
  Ethereum convention and what `ethers.verifyMessage` expects. Raw recovery ids
  `0`/`1` are **not** the wire format. (`ethers` tolerates 0/1 on input, but the
  server's zod schema and every canonical vector use 27/28 — always emit 27/28.)
- Wire encoding: `"0x"` + 130 lowercase hex chars (total string length 132).

### 2.3 Server-side verification (what `ethers.verifyMessage` does)

```
digest    = eip191_digest(message)
pubkey    = ecdsa_recover(digest, r, s, v - 27)
recovered = eip55( keccak256(pubkey.X ‖ pubkey.Y)[12..32] )
accept iff recovered.to_lowercase() == expected_address.to_lowercase()
```

### 2.4 Worked example (verified)

```
wallet private key : 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
wallet address     : 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266

message (126 UTF-8 bytes, \n = LF 0x0A):
"FruitNation Encryption Key Derivation\n\nAddress: 0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266\nPurpose: End-to-end encryption only"

prefix        : "\x19Ethereum Signed Message:\n126"
prefix hex    : 19457468657265756d205369676e6564204d6573736167653a0a313236
EIP-191 digest: 0x50da9eedfc742dd88128600e67d046124066add1cf9907a747837d823d9c7677

signature (65 bytes):
0xe98d833febe631493b5589273c82b3a66df264a47ca2c02754c0f9dffaeceaa601306b1edf3328bfe7f46b747def4fd6b4f4b400dcc76955a6fb33a47ac6ba531b

  r = 0xe98d833febe631493b5589273c82b3a66df264a47ca2c02754c0f9dffaeceaa6
  s = 0x01306b1edf3328bfe7f46b747def4fd6b4f4b400dcc76955a6fb33a47ac6ba53
  v = 0x1b = 27   (recovery_id = 0)
```

### 2.5 The login challenge message

Generated server-side (`server/routes.ts`, `POST /api/auth/challenge`), signed
verbatim by the client. Do **not** reconstruct it — sign exactly the string the
server returned:

```
Welcome to FruitNation!\n\nClick to sign in and accept the FruitNation Terms of Service.\n\nThis request will not trigger a blockchain transaction or cost any gas fees.\n\nWallet address:\n{walletAddressLowercase}\n\nNonce:\n{nonce64hex}
```

`nonce` = 32 random bytes hex. Challenges expire after 10 minutes and are
consumed atomically (single-use, burned even on a failed signature).

---

## 3. Encryption key derivation

The E2EE keypair is **separate from the wallet keypair**. It is derived from a
wallet signature so it is deterministic per account (multi-device works) and
identical whether the user logs in via mnemonic, MetaMask, or Privy.

### 3.1 v2 (salted) — CURRENT, use for all new keys

Reference: `AuthService.buildSaltedEncryptionMessage`, `deriveEncryptionKeysFromSignature`.

**Step 0.** Log in first. The salt is a per-account secret served *only* to the
authenticated owner: it comes back as `encryptionSalt` in the
`POST /api/auth/login` response, or from `GET /api/auth/encryption-salt`
(JWT required). It is **32 random bytes as 64 lowercase hex chars** and never
appears in public user objects.

**Step 1.** Build the message. Exact bytes, `\n` = LF (`0x0A`), **no trailing
newline**:

```
FruitNation Encryption Key Derivation v2\n
\n
Address: {walletAddressLowercase}\n
Salt: {saltHex}\n
Purpose: End-to-end encryption only
```

As a single Rust string literal:

```rust
let msg = format!(
    "FruitNation Encryption Key Derivation v2\n\nAddress: {}\nSalt: {}\nPurpose: End-to-end encryption only",
    wallet_address.to_lowercase(),
    salt_hex
);
```

**Step 2.** `sig = personal_sign(msg)` with the **wallet** key → 65 bytes.

**Step 3.** Derive:

```
encPriv = keccak256(sig_bytes)                  // 32 bytes; hash the 65 RAW BYTES,
                                                //  NOT the "0x..." ASCII string
encPub  = uncompressed_secp256k1(encPriv)       // "04" + X + Y, 130 lowercase hex, no 0x
```

> **Trap.** `ethers.keccak256(signature)` takes a `0x…` *hex string* and hashes
> the **decoded bytes**. In Rust: `Keccak256::digest(&sig_bytes_65)`. Hashing
> the ASCII of `"0xe98d…"` produces a completely different key.

`encPriv` is stored/passed as a `0x`-prefixed 66-char hex string in the
reference client; strip `0x` before use as scalar bytes. If `encPriv` is `0` or
`≥ n`, re-deriving is impossible — reject (probability ≈ 2⁻¹²⁸, but handle it
rather than panicking).

**Worked example (verified):**

```
wallet priv : 0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
address     : 0xfcad0b19bb29d4674531d6f115237e16afce377c
salt        : 00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff
message     : 200 UTF-8 bytes
signature   : 0x4f4ecd00b2ae0f7de622f282c6c1b298a8f12b8d57f70d0452caed2d8f8d98b8415daeb38cbae47fc90bf46481cc0134eb621ffe557883f4d2f9cf23a4dd662c1b
encPriv     : 0xa6fd87b69e1c83ba6bdd5f5a502a41b707dac3993350372886b6217e8f06e6ea
encPub      : 045031e83ea2f138541de6908c38da03c6af49cd4f356e64799d63f9125a92a7b13094127a2ad3089544d16ed59a59e60dc869ec91f55466871df67e09bc4920e1
```

### 3.2 v1 (unsalted) — **LEGACY, READ-ONLY**

Reference: `AuthService.buildEncryptionMessage`, `getLegacyEncryptionPrivateKey`.

```
FruitNation Encryption Key Derivation\n\nAddress: {walletAddressLowercase}\nPurpose: End-to-end encryption only
```

Identical downstream steps (`keccak256(sig)` → private key → uncompressed pub).

> **LEGACY / READ-ONLY.** This message is public and constant, so *any* dapp can
> ask a user to sign it and thereby obtain their E2EE private key. **Never
> derive a new key from it and never publish a public key derived from it.**
>
> Its only permitted use is the *healing* path: when unwrapping a room key with
> the v2-derived key fails, re-derive the legacy key locally, unwrap with it,
> and immediately **re-wrap the recovered room key to the v2 public key** at the
> same `keyVersion` (see `RoomKeyService.healLegacyRoomKey`). This path is
> mnemonic-only — it needs a signature without a wallet popup. For
> MetaMask/Privy sessions, skip healing.

Canonical legacy vectors (from `PROTOCOL.md`, independently reverified):

```
=== Legacy vector 1 ===
wallet priv : 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
address     : 0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266
signature   : 0xe98d833febe631493b5589273c82b3a66df264a47ca2c02754c0f9dffaeceaa601306b1edf3328bfe7f46b747def4fd6b4f4b400dcc76955a6fb33a47ac6ba531b
encPriv     : 0xdde7337c32273ab3ca7154efc8c49b2873d797900ec7b047533ed7291f93f7a3
encPub      : 04f35792987bfeeb9076b62b7e60c50fc81a87859b86388b10c9b651c5862a6cab08c4f425ad1ade24a688ce8666b0e5f2bea841e3388d64cad39a2f41846e7e9e

=== Legacy vector 2 ===
wallet priv : 0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
address     : 0xfcad0b19bb29d4674531d6f115237e16afce377c
signature   : 0x41d928b05cdef74a60021241437fb7697f814cd8d4401ce892182a0c2ef0adbe670f7276e6fad3e125111887d22d391b08ec542ad4069fafca16c9e58da57a401b
encPriv     : 0x53a19ea4568d269bc534f3ae57521fd2c43fa6a515f9c93389749749a8a050d3
encPub      : 04bfd164ac84846bc26874a3ec72187690790d156663fa69e5abad4ce1e33a53d790f954f4d93184cf36db975cb738907f64996dd18b269bf1393e0705dc956ce5
```

These two vectors also double as **EIP-191 signing vectors** and as
**mnemonic-independent address-derivation vectors**.

---

## 4. Public-key binding (anti-MITM on key distribution)

Reference: `shared/schema.ts::buildKeyBindingMessage` (server) and
`AuthService.buildKeyBindingMessage` (client) — byte-identical.

### 4.1 The binding message

```
FruitNation Public Key Binding\n\nAddress: {walletAddressLowercase}\nEncryption Public Key: {encPubHex}
```

`encPubHex` is the 130-char uncompressed hex **with no `0x` prefix**, spliced in
verbatim (case as published — the reference client always produces lowercase).

```rust
let binding = format!(
    "FruitNation Public Key Binding\n\nAddress: {}\nEncryption Public Key: {}",
    wallet_address.to_lowercase(),
    enc_pub_hex
);
```

### 4.2 Publishing

```
publicKeySig = personal_sign(binding)          // signed by the WALLET key, not encPriv
PUT /api/auth/encryption-key { publicKey: encPubHex, publicKeySig }
```

The server re-derives the binding message from the JWT's wallet address and the
submitted `publicKey`, recovers the signer, and rejects on mismatch. The same
check runs on the optional `publicKey`/`publicKeySig` fields of
`POST /api/auth/login` (legacy path).

### 4.3 Mandatory verification before wrapping

Reference: `RoomKeyService.fetchVerifiedPublicKey`.

Before wrapping a room key to **anyone else's** public key, a client **MUST**:

1. `POST /api/users/public-keys` (JWT required, ≤50 addresses per call) with the
   address(es); get `[{ walletAddress, publicKey, publicKeySig }]`.
2. If `publicKey` is missing → **abort** ("user must log in first").
3. If `publicKeySig` is missing/null → **abort**. An unsigned key is not
   acceptable, even though the schema allows it to exist for legacy rows.
4. Rebuild `binding = buildKeyBindingMessage(userAddress, publicKey)` locally
   using the **address you intend to share with** (not one echoed by the server)
   and the **exact `publicKey` string returned**.
5. Recover the signer from `publicKeySig` over the EIP-191 digest of `binding`.
   Malformed signature → **abort**.
6. `recovered.to_lowercase() != userAddress.to_lowercase()` → **abort**.
7. Only then wrap.

This is the sole defense against a malicious/compromised server substituting its
own encryption public key at invite/rotate time. Failing open here silently
destroys the E2EE guarantee — **abort, never warn-and-continue**. Note the
reference `rotateRoomKey` runs this check for *every* member on *every*
rotation; do the same.

**Worked example (verified):**

```
address     : 0xfcad0b19bb29d4674531d6f115237e16afce377c
encPub      : 045031e83ea2f138541de6908c38da03c6af49cd4f356e64799d63f9125a92a7b13094127a2ad3089544d16ed59a59e60dc869ec91f55466871df67e09bc4920e1
binding msg : 237 UTF-8 bytes
publicKeySig: 0x119fd2e039b49088a5a6cc2222749a47da5e52b6013f53d17b87627f1fd7aed41c6f6682dc0b573ed490a5f2a0922ea14f782a0c7a1bce8807006065a86516621c
recovers to : 0xFCAd0B19bB29D4674531d6f115237E16AfCE377c  ✓
```

---

## 5. Subkey derivation (label-KDF)

```
subkey(K, label) = HMAC-SHA256(key = K_raw_32_bytes, message = ascii(label))
```

- **Key** = the 32 **raw** bytes (hex-decode the key string first).
- **Message** = the ASCII label, no separators, no length prefix, no trailing NUL.
- Output = full 32-byte HMAC tag, used **directly** as the AES-256 key or the
  HMAC-SHA256 key. No truncation, no second round, no HKDF-Expand.

> **Trap — argument order.** The TypeScript is `CryptoJS.HmacSHA256(label, key)`,
> whose signature is `(message, key)`. The label is the **message**; the key
> material is the **key**. Swapping them silently produces plausible-looking
> garbage.

The four labels — **exact ASCII, case-sensitive, no trailing whitespace**:

| Purpose | Label |
| --- | --- |
| Message AES-256 key | `FruitNation/v2/message/enc` |
| Message HMAC-SHA256 key | `FruitNation/v2/message/mac` |
| Room-key-wrap AES-256 key | `FruitNation/v2/roomkey/enc` |
| Room-key-wrap HMAC-SHA256 key | `FruitNation/v2/roomkey/mac` |

```rust
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub const MSG_ENC_LABEL: &str = "FruitNation/v2/message/enc";
pub const MSG_MAC_LABEL: &str = "FruitNation/v2/message/mac";
pub const WRAP_ENC_LABEL: &str = "FruitNation/v2/roomkey/enc";
pub const WRAP_MAC_LABEL: &str = "FruitNation/v2/roomkey/mac";

fn derive_subkey(key: &[u8; 32], label: &str) -> [u8; 32] {
    let mut mac = <Hmac<Sha256>>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(label.as_bytes());
    mac.finalize().into_bytes().into()
}
```

The `subkeys` array of `crypto-v2.json` covers both keys × all four labels.

---

## 6. Message encryption v2 (`encVer = 2`)

Reference: `encryptMessageV2` / `decryptMessageV2`.

### 6.1 Encrypt

```
encKey = subkey(roomSymmetricKey, "FruitNation/v2/message/enc")
macKey = subkey(roomSymmetricKey, "FruitNation/v2/message/mac")
iv     = 16 CSPRNG bytes                                  (fresh per message)
ct     = AES-256-CBC-PKCS7(key = encKey, iv = iv, pt = utf8(plaintext))
ctB64  = b64(ct)                                          (standard alphabet, padded)
ivHex  = hex(iv)                                          (lowercase, 32 chars)
macIn  = "FNv2|message|" ‖ roomId ‖ "|" ‖ ivHex ‖ "|" ‖ ctB64
hmac   = hex( HMAC-SHA256(key = macKey, msg = utf8(macIn)) )
```

MAC input layout, exactly:

```
FNv2|message|{roomId}|{ivHex}|{ciphertextBase64}
```

- Literal tag `FNv2` — uppercase F, N, lowercase v, digit 2.
- Exactly four `|` separators, no spaces anywhere.
- `ivHex` is lowercase; `ctB64` is standard Base64 **including** `=` padding
  (the padding chars are part of the MAC input).
- Nothing is length-prefixed. The `|` separator is the only framing, which is
  safe because `roomId ∈ [A-Za-z0-9_.-]` and hex/base64 exclude `|`.

**Encrypt-then-MAC**: the MAC is computed over the *ciphertext*, the *IV* and
the *roomId*, never over the plaintext.

Wire fields:

```json
{
  "content": "{ctB64}",
  "iv": "{ivHex}",
  "hmac": "{hmacHex}",
  "isEncrypted": true,
  "encVer": 2,
  "keyVersion": <epoch>,
  "msgHash": "{sha256_hex_of_ctB64}"
}
```

### 6.2 Decrypt

Strict ordering — **verify before you decrypt**:

1. Recompute `macIn` using `roomId` of the room the message belongs to, the
   received `iv` string **verbatim**, and the received `content` string
   **verbatim**.
2. `computed = HMAC-SHA256(macKey, macIn)`.
3. Compare `computed` against the received `hmac` in **constant time**. The
   reference compares the two 64-char hex strings char-by-char with an
   accumulating XOR, and returns `false` immediately if the lengths differ.
   Length leakage is acceptable; content leakage is not. In Rust, prefer
   `Mac::verify_slice(&expected_bytes)` (already constant-time) over comparing
   hex strings.
4. On mismatch → **abort**. Do not attempt AES. Do not report *why*.
5. Only then: `pt = AES-256-CBC-PKCS7-decrypt(encKey, iv, base64_decode(content))`.
6. Decode `pt` as UTF-8. Empty result → treat as failure (the reference throws
   on an empty decrypt, since CryptoJS returns `""` for a bad UTF-8 decode).

PKCS#7 unpadding errors must be reported identically to MAC failures — but note
that with encrypt-then-MAC verified first, a padding oracle is unreachable.

### 6.3 Constant-time comparison

```rust
use hmac::{Hmac, Mac};
use sha2::Sha256;

fn verify_mac(mac_key: &[u8; 32], mac_input: &str, expected_hex: &str) -> bool {
    let Ok(expected) = hex::decode(expected_hex) else { return false };
    if expected.len() != 32 { return false; }
    let mut mac = <Hmac<Sha256>>::new_from_slice(mac_key).unwrap();
    mac.update(mac_input.as_bytes());
    mac.verify_slice(&expected).is_ok()   // constant-time
}
```

---

## 7. Room-key wrapping v2 (`encVer = 2`)

Reference: `encryptRoomKeyForUserV2` / `decryptRoomKeyV2`.

### 7.1 Wrap

```
(ephPriv, ephPub) = fresh secp256k1 keypair            (per wrap, never reused)
Q                 = ephPriv · P_recipient              (EC point multiply)
sharedX           = hex( Q.x as 32 bytes big-endian )  (64 lowercase hex, zero-left-padded)
encKey            = subkey(sharedX, "FruitNation/v2/roomkey/enc")
macKey            = subkey(sharedX, "FruitNation/v2/roomkey/mac")
iv                = 16 CSPRNG bytes
ct                = AES-256-CBC-PKCS7(encKey, iv, utf8( hex(roomKey) ))
ctB64             = b64(ct)
ephPubHex         = hex(04 ‖ X_eph ‖ Y_eph)            (130 lowercase hex)
macIn             = "FNv2|roomkey|" ‖ roomId ‖ "|" ‖ ephPubHex ‖ "|" ‖ hex(iv) ‖ "|" ‖ ctB64
hmac              = hex( HMAC-SHA256(macKey, utf8(macIn)) )
```

MAC input layout, exactly:

```
FNv2|roomkey|{roomId}|{ephemeralPublicKeyHex}|{ivHex}|{ciphertextBase64}
```

Five `|` separators. The ephemeral public key **is** authenticated (this is one
of the v1 fixes).

### 7.2 ECDH — precise definition

**The shared secret is the X coordinate of the ECDH point, and nothing else.**

- Not the compressed point. Not the full `04‖X‖Y` encoding. Not `X‖Y`.
- **No hashing of the shared secret before the label-KDF** — `sharedX` (as a
  64-char lowercase hex string) is hex-decoded to 32 raw bytes and fed straight
  into `HMAC-SHA256` as the *key*. The label-KDF *is* the KDF; there is no
  additional SHA-256/HKDF step.
- The X coordinate is a big-endian 32-byte integer, **left-padded with zero
  bytes**. The TypeScript does `bn.toString(16).padStart(64, "0")` precisely
  because elliptic's BN drops leading zeros. In Rust this is free: `k256`'s
  `diffie_hellman` returns `SharedSecret` whose `raw_secret_bytes()` is already
  the fixed-width 32-byte big-endian X coordinate.

```rust
use k256::{PublicKey, SecretKey, ecdh::diffie_hellman};

let shared = diffie_hellman(eph_secret.to_nonzero_scalar(), recipient_pub.as_affine());
let shared_x: [u8; 32] = (*shared.raw_secret_bytes()).into();   // == hex-decoded sharedX
```

### 7.3 The wrapped plaintext

> **Trap.** The plaintext being encrypted is the **64-character lowercase hex
> ASCII string** of the room key — not its 32 raw bytes. So the AES input is 64
> bytes, which PKCS#7-pads to 80 bytes → 80 bytes of ciphertext → 108 Base64
> chars. If your wrap ciphertext is 44 Base64 chars you encrypted the raw bytes
> and every other client will fail to unwrap it.

### 7.4 Unwrap

1. Parse `ephemeralPublicKey` as an uncompressed secp256k1 point; **validate it
   is on the curve and not the identity**. Reject otherwise. (`k256`'s
   `PublicKey::from_sec1_bytes` performs this check.)
2. `sharedX = X(userEncPriv · ephPub)`.
3. `macKey = subkey(sharedX, WRAP_MAC_LABEL)`.
4. Rebuild `macIn` from the **roomId you fetched the wrap for** and the
   `ephemeralPublicKey` / `encryptionIV` / `encryptedSymmetricKey` strings
   **exactly as received** (see the mixed-case trap in §0).
5. Constant-time compare against `hmac`. Mismatch → **abort**, no AES.
6. `encKey = subkey(sharedX, WRAP_ENC_LABEL)`; AES-256-CBC-PKCS7 decrypt.
7. Decode UTF-8 and require the result to match `^[0-9a-f]{64}$` **case-
   insensitively** (the reference regex is `/^[0-9a-f]{64}$/i`). Reject
   otherwise.
8. Hex-decode to the 32-byte room key.

Wire fields:

```json
{
  "encryptedSymmetricKey": "{ctB64}",
  "ephemeralPublicKey": "{ephPubHex}",
  "encryptionIV": "{ivHex}",
  "hmac": "{hmacHex}",
  "encVer": 2,
  "keyVersion": <epoch>
}
```

### 7.5 Room key generation

`generateRoomKey()` = **32 bytes from a CSPRNG**, carried as 64 lowercase hex
chars. No structure, no derivation, no versioning inside the key.

---

## 8. `encVer = 1` (LEGACY) — decrypt-only

Never write `encVer: 1`. Implement these paths **only** to read old rows.
Dispatch rule from `decryptMessageByVersion` / `decryptRoomKeyByVersion`:

```
if (encVer ?? 1) >= 2 { v2 } else { v1 }
```

A missing or `null` `encVer` means **1**.

### 8.1 v1 messages

```
key   = the room symmetric key (hex string, 64 chars)
ct    = AES-256-CBC-PKCS7(key = hex_decode(key) /* 32 raw bytes */, iv, utf8(pt))
ctB64 = b64(ct)
hmac  = hex( HMAC-SHA256(key = utf8(keyHexString) /* 64 ASCII BYTES */, msg = utf8(ctB64)) )
```

> **The single biggest v1 trap.** The AES key is the **32 raw bytes**, but the
> HMAC key is the **64 ASCII bytes of the lowercase hex string**. CryptoJS's
> `HmacSHA256(msg, stringKey)` UTF-8-encodes a `String` key; the code passes the
> hex *string*, while the AES call passes `CryptoJS.enc.Hex.parse(key)`. These
> are different keys and the discrepancy is intentional-by-accident.
>
> Verified with the canonical key `9f86d081…0a08`, IV `000102…0e0f`,
> plaintext `"attack at dawn"`:
>
> ```
> ct (v1)                        = L/mApzuGAZvpOIqxuMaATg==
> HMAC with ASCII-hex key (correct) = 98ffe503961ec8f1ae0f412db362936c79e0cae42e01bd1ac06c171ac4e0ec95
> HMAC with raw 32-byte key (wrong) = e3109563ee713e66836137cc097fbcabf24457e8ae6c73ca1c1b44055d171675
> ```

v1 authenticates **only the ciphertext** — no version tag, no roomId, no IV.
Consequences you must not replicate: the IV can be bit-flipped to tamper with
the first plaintext block, and ciphertexts replay across rooms.

v1 decrypt order in the reference is still MAC-then-decrypt, but the comparison
in the original v1 helper is the shared `constantTimeEqualHex`.

### 8.2 v1 room keys

```
sharedX = X(ephPriv · P_recipient), 64 lowercase hex chars, zero-left-padded
aesKey  = hex_decode(sharedX)                         // 32 RAW bytes, used DIRECTLY as AES key
ct      = AES-256-CBC-PKCS7(aesKey, iv, utf8( hex(roomKey) ))   // still the 64-char hex string
ctB64   = b64(ct)
hmac    = hex( HMAC-SHA256(key = utf8(sharedXHexString) /* 64 ASCII BYTES */, msg = utf8(ctB64)) )
```

Same ASCII-vs-raw key split as v1 messages. Note v1 uses the **raw ECDH
x-coordinate directly as the AES-256 key** — no KDF at all — and does not
authenticate the ephemeral public key or the IV.

Unwrap validation is the same `^[0-9a-f]{64}$/i` check on the decrypted string.

---

## 9. Key epochs (rotation)

Reference: `RoomKeyService`, `POST /api/rooms/:roomId/rotate-key`,
`server/routes.ts` message-post guards.

### 9.1 Model

- Each room carries `currentKeyVersion` (integer ≥ 1; epoch 1 is created with
  the room).
- Each `room_keys` row is `(roomId, userAddress, keyVersion)` → a wrap of that
  epoch's symmetric key for that member.
- Each message carries `keyVersion`. **Missing/null `keyVersion` means 1.**

### 9.2 Selecting a key

- **Encrypting**: always use the key for the *highest* epoch you hold
  (`bundle.latest`), and stamp the message with that `keyVersion`.
- **Decrypting**: use the key for `message.keyVersion ?? 1`. A client keeps
  **all** epochs it ever received so history stays readable across rotations.
- Fetch all epochs via `GET /api/rooms/:roomId/keys/versions` (returns an array
  of wraps). Fall back to the single-key endpoint `GET /api/rooms/:roomId/keys`
  for old servers, treating a missing `keyVersion` as 1. Wraps are stored with
  `POST /api/rooms/:roomId/keys`.
- Unwrap failures on an individual epoch must be **tolerated** (log + skip, try
  the legacy-heal path) so one corrupt row cannot black out all of history.

### 9.3 Rotation procedure

Triggered when a member is removed or leaves; the server then sets
`keyRotationPending` on the room.

1. Server refuses new **encrypted** posts with `409 { code: "KEY_ROTATION_REQUIRED" }`
   while a rotation is pending — fail-closed, so nothing is written under the
   compromised epoch.
2. **Any current member** (not just an admin — Signal-style, the sender drives
   the re-key) performs:
   - `GET /api/rooms/:roomId/members`
   - generate a fresh 32-byte key
   - `newVersion = currentKeyVersion + 1`
   - for **every** current member: `fetchVerifiedPublicKey` (§4.3, mandatory)
     then `encryptRoomKeyForUserV2(newKey, theirPub, roomId)`
   - `POST /api/rooms/:roomId/rotate-key { newVersion, keys: [...] }` — atomic.
3. Server enforces: caller is a member; `newVersion ∈ [2, 1_000_000]`; **every**
   current member is covered; **no** wrap targets a non-member; `1 ≤ len(keys) ≤ 200`.
4. Cache the new epoch locally.

Client-side recovery from a race:

- `409 STALE_KEY_VERSION` → your cached `latest` is behind. Clear the cache,
  refetch key versions, re-encrypt under the new epoch, retry **once**.
- `409 KEY_ROTATION_REQUIRED` → perform the rotation yourself (step 2), then
  re-encrypt and retry.

The reference client caches decrypted bundles for **1 hour**, keyed by
`{walletAddress}_{roomId}`, which is why these two 409s are normal traffic, not
exceptional.

### 9.4 Forward secrecy — what you actually get

**Provided:**

- A removed member never receives the epoch-`N+1` wrap, so their cached epoch-`N`
  key cannot read anything sent afterward.
- The server enforces fail-closed: no encrypted message can be written under an
  epoch whose membership is stale.

**Not provided — state these plainly to users:**

- **No backward secrecy.** A new member (or the server, if it retained old
  wraps) can read all history from every epoch they hold a wrap for. Rotation
  protects *future* messages only.
- **No per-message ratchet.** All messages in an epoch share one key. Compromise
  of an epoch key exposes the whole epoch.
- **No forward secrecy against long-term key compromise.** The E2EE private key
  is deterministically re-derivable from a wallet signature. Compromise of the
  wallet key (or of a phished derivation signature) retroactively decrypts every
  wrap that key ever received, and hence every epoch.
- **No in-room replay/reorder protection.** Nothing binds a message to a
  sequence number or to the previous message.
- **Metadata is fully visible** to the server: sender, recipients, timestamps,
  membership, message sizes.
- **Rotation integrity is unverifiable.** The server never holds the key, so it
  cannot check that a rotation wrapped the *same, valid* key for every member.
  Any current member can publish inconsistent/garbage wraps and lock the room's
  next epoch (a griefing DoS). It is confidentiality-preserving — the attacker
  learns nothing new — and inherent to E2EE group re-keying without a verified
  transcript.

---

## 10. `msgHash`

`msgHash` is `hex(SHA-256(utf8(S)))` — **lowercase**, 64 chars — where `S`
differs per event type. The server regex is `^[a-f0-9]{64}$`; uppercase is
rejected. SHA-256 here is plain SHA-256, **not** Keccak.

| Event | Who computes | `S` = SHA-256 input |
| --- | --- | --- |
| `message`, **encrypted** | client | the **Base64 ciphertext string** (`content` as sent) |
| `message`, **plaintext** | client | the **trimmed plaintext** (`content` as sent) |
| `edit` (`PATCH /api/messages/:id`) | client | the **new** `content` as sent — ciphertext if the room has a key, else trimmed plaintext |
| `delete` | server | not hashed — server force-sets `msgHash = ""`, `content = ""`, `iv = null`, `hmac = null` |
| `emoticon_add` / `emoticon_remove` | **server** | `"{messageId}:{emoticonCode}:{add|remove}:{senderWalletAddress}:{timestampMs}"` |

### 10.1 Encrypted messages — hash the ciphertext, never the plaintext

```
msgHash = hex( SHA-256( utf8( ciphertextBase64 ) ) )
```

This is a security requirement, not a convention. `msgHash` is stored
server-side and may be published on-chain; a hash over the plaintext would let
anyone confirm guessed message contents by dictionary attack, defeating the
encryption for short messages. The base64 string is hashed **as ASCII text**,
including its `=` padding — not the decoded ciphertext bytes.

Reference: `prepareMessage` in `MessageInput.tsx`
(`msgHash: await computeSHA256(encrypted.encrypted)`), and `computeSHA256` in
`client/src/utils/crypto.ts` (`TextEncoder` → `crypto.subtle.digest("SHA-256")`
→ lowercase hex).

### 10.2 Plaintext messages

```
msgHash = hex( SHA-256( utf8( content.trim() ) ) )
```

Trim first — the server trims `content` before storing (§0).

### 10.3 Edits

Same rule as a new message: hash whatever you put in `content`. An edit is a
**new event** and is re-encrypted under the **current** epoch (with a fresh IV),
so its `msgHash` is over the *new* ciphertext, not the original's.

### 10.4 Emoticon events

Computed **server-side**; the client neither sends nor can independently
reproduce it (it depends on the server's `Date.now()`):

```
eventData = "{messageId}:{emoticonCode}:{add|remove}:{senderWalletAddress}:{timestampMs}"
msgHash   = hex( SHA-256( utf8(eventData) ) )
```

- `{add|remove}` is the literal string `add` or `remove`.
- `senderWalletAddress` comes from the JWT and is lowercase.
- `timestampMs` is `Date.now()` — milliseconds since epoch, decimal, no padding.
- Colons are the only separators; the emoticon code is inserted verbatim (it is
  Unicode, 1..64 chars, trimmed).

A Rust client should treat this as read-only and simply store the value the
server returns.

### 10.5 On-chain publishing

`POST /api/messages/:id/publish-txhash` accepts a tx hash. When `FN_RPC_URL` is
configured the server verifies on-chain that the transaction exists, that
`tx.to == fruitnationWallet`, and that `tx.data.toLowerCase()` **contains** the
message's `msgHash` (lowercased, `0x` stripped). Put the bare 64 hex chars into
the calldata.

---

## 11. Rust crate mapping

All recommended crates are **pure Rust** (no OpenSSL, no C toolchain) and build
for `wasm32-unknown-unknown`.

| Primitive | Crate | Notes |
| --- | --- | --- |
| secp256k1 (ECDSA, ECDH, point ops) | **`k256`** (`0.13`) | RustCrypto, pure Rust, wasm-clean. Features: `ecdsa`, `ecdh`, `arithmetic`, `sha256`. Provides RFC 6979 deterministic signing and recovery (`ecdsa::RecoveryId`, `VerifyingKey::recover_from_prehash`). **Prefer over `secp256k1`** (libsecp256k1 C bindings — builds for wasm only via emscripten-ish hacks and bloats the bundle). |
| keccak256 | **`sha3`** (`0.10`) → `sha3::Keccak256` | **Not `Sha3_256`.** `tiny-keccak` (`keccak` feature) is a smaller alternative; both are pure Rust. |
| SHA-256 | **`sha2`** (`0.10`) | Enable `asm` only on native; leave it off for wasm. |
| SHA-512 (BIP-32/39 internals) | **`sha2`** | Same crate. |
| HMAC | **`hmac`** (`0.12`) | `Hmac<Sha256>`, `Hmac<Sha512>`. Use `Mac::verify_slice` for constant-time comparison. |
| PBKDF2 (BIP-39 seed) | **`pbkdf2`** (`0.12`) | `pbkdf2_hmac::<Sha512>`; or let the BIP-39 crate do it. |
| AES-256-CBC + PKCS#7 | **`aes`** (`0.8`) + **`cbc`** (`0.1`) | `cbc::Encryptor<aes::Aes256>` / `Decryptor`, with `block_padding::Pkcs7`. Bitsliced/constant-time by default on wasm; hardware AES on x86-64/aarch64. |
| Base64 | **`base64`** (`0.22`) | `base64::engine::general_purpose::STANDARD` — standard alphabet **with** padding. Do **not** use `STANDARD_NO_PAD` or `URL_SAFE`. |
| Hex | **`hex`** (`0.4`) or **`const-hex`** | `hex::encode` emits lowercase (correct default). `const-hex` is faster. |
| BIP-39 | **`bip39`** (`2.x`) | Pure Rust, `no_std`-capable. Use `Mnemonic::to_seed("")` for the empty passphrase. Alternative: `coins-bip39` (what ethers-rs uses). |
| BIP-32 | **`bip32`** (`0.5`) with `default-features = false, features = ["secp256k1-ffi"→ OFF, "alloc"]` | Configure it to use `k256`, **not** the `secp256k1` C crate. Alternative: `coins-bip32`. Or implement BIP-32 CKD directly — it is ~40 lines over `hmac`+`sha2`+`k256`. |
| EIP-55 / address utils | hand-rolled (~15 lines) or **`alloy-primitives`** (`Address::to_checksum`) | `alloy-primitives` is wasm-clean; `ethers-core` also works but is heavier and deprecated. |
| CSPRNG | **`getrandom`** (`0.2`, matching `core/Cargo.toml`) | See the wasm note below, and `core/src/random.rs` for the full argument: one module, no fallback, an error on refusal. Chosen over `rand`'s `OsRng` because `OsRng` reports a refusal through `RngCore::fill_bytes`, which cannot fail and so panics — a property of that trait's contract, not of any `rand`/`getrandom` version, so it holds across a future bump. Everything unguessable on either target draws from there. |
| Constant-time compare | **`subtle`** (`2.x`) | Only needed if you compare something other than a MAC; `hmac`'s `verify_slice` already covers MACs. |
| Zeroizing key material | **`zeroize`** (`1.x`) with `derive` | Wrap room keys, subkeys, `sharedX`, private keys. `k256::SecretKey` already zeroizes. |
| HKDF | **not needed** | FruitNation's KDF is a bare `HMAC-SHA256(key, label)`. Do **not** substitute HKDF — it would change every output. Listed only to say: don't. |

### 11.1 `wasm32-unknown-unknown` specifics

- **`getrandom`.** On `wasm32-unknown-unknown` there is no OS RNG; you must opt
  into the JS `crypto.getRandomValues` backend.
  - `getrandom` **0.2**: `getrandom = { version = "0.2", features = ["js"] }`
  - `getrandom` **0.3**: feature is `wasm_js`, *and* you must set
    `RUSTFLAGS='--cfg getrandom_backend="wasm_js"'` (e.g. in
    `.cargo/config.toml` under `[target.wasm32-unknown-unknown]`).
  - Transitive dependencies may pull in *both* 0.2 and 0.3 — enable the feature
    for each version present, or the build fails at link time with
    "the wasm32-unknown-unknown targets are not supported by default".
- **Avoid**: `openssl`, `ring` (partial wasm support, and it links C),
  `secp256k1` (libsecp256k1 C), `rust-crypto`, anything with a `build.rs` that
  invokes `cc`.
- **Timing.** Constant-time guarantees are weaker in a browser (JIT, no control
  over the engine). Use the constant-time APIs anyway; they still remove the
  obvious data-dependent branches.
- **`aes`** compiles to constant-time bitsliced code on wasm (no AES-NI). Do not
  enable `aes_force_soft`-adjacent flags expecting speed on native — just leave
  defaults.
- **Key storage** in the browser: the reference web client keeps the E2EE
  private key and decrypted room keys in `localStorage`. That is XSS-readable.
  For PocketSkynet prefer non-extractable WebCrypto handles or an in-memory-only
  session key with re-derivation on reload, and `zeroize` everything you hold in
  Rust memory.

### 11.2 Suggested `Cargo.toml` sketch

```toml
[dependencies]
k256   = { version = "0.13", default-features = false, features = ["ecdsa", "ecdh", "arithmetic", "alloc"] }
sha3   = { version = "0.10", default-features = false }
sha2   = { version = "0.10", default-features = false }
hmac   = { version = "0.12", default-features = false }
aes    = { version = "0.8",  default-features = false }
cbc    = { version = "0.1",  default-features = false, features = ["alloc", "block-padding"] }
base64 = { version = "0.22", default-features = false, features = ["alloc"] }
hex    = { version = "0.4",  default-features = false, features = ["alloc"] }
bip39  = { version = "2",    default-features = false, features = ["std"] }
zeroize = { version = "1", features = ["derive"] }
subtle = "2"

[target.'cfg(target_arch = "wasm32")'.dependencies]
getrandom = { version = "0.2", features = ["js"] }
```

---

## 12. Test vectors

The **canonical** file is `server/test/vectors/crypto-v2.json`
(`formatVersion: 2`). PocketSkynet's Rust test suite **must** load that file
from the FruitNation repo (or a checked-in copy synced from it) and assert
byte-equality on every entry. Do not retype the values — parse the file, so a
regeneration upstream is caught by a failing test rather than silently diverging.

Regenerate upstream with `npx vite-node scripts/generate-crypto-vectors.ts`
(only when the format intentionally changes, bumping `formatVersion`).

### 12.1 Loader

```rust
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Vectors {
    pub format_version: u32,
    pub labels: Labels,
    pub subkeys: Vec<SubkeyVec>,
    pub messages: Vec<MessageVec>,
    pub room_key_wraps: Vec<WrapVec>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Labels {
    pub message_enc: String,
    pub message_mac: String,
    pub room_key_enc: String,
    pub room_key_mac: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubkeyVec {
    pub key_hex: String,
    pub label: String,
    pub subkey_hex: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageVec {
    pub name: String,
    pub symmetric_key_hex: String,
    pub room_id: String,
    pub plaintext_utf8: String,
    pub iv_hex: String,
    pub ciphertext_base64: String,
    pub hmac_hex: String,
    pub msg_hash_hex: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WrapVec {
    pub name: String,
    pub room_symmetric_key_hex: String,
    pub room_id: String,
    pub recipient_private_key_hex: String,
    pub recipient_public_key_hex: String,
    pub ephemeral_private_key_hex: String,
    pub ephemeral_public_key_hex: String,
    pub iv_hex: String,
    pub encrypted_symmetric_key_base64: String,
    pub hmac_hex: String,
}

pub fn load() -> Vectors {
    // Keep the path configurable; CI should point at the FruitNation checkout.
    let raw = std::fs::read_to_string(
        std::env::var("FN_CRYPTO_VECTORS")
            .unwrap_or_else(|_| "../server/test/vectors/crypto-v2.json".into()),
    )
    .expect("crypto-v2.json");
    serde_json::from_str(&raw).expect("valid vectors")
}
```

### 12.2 Required assertions

```
for v in subkeys:
    assert hex(derive_subkey(hex_decode(v.key_hex), v.label)) == v.subkey_hex

for v in messages:
    (ct, iv, mac) = encrypt_message_v2(v.plaintext_utf8, v.symmetric_key_hex,
                                       v.room_id, iv = hex_decode(v.iv_hex))
    assert ct  == v.ciphertext_base64
    assert iv  == v.iv_hex
    assert mac == v.hmac_hex
    assert hex(sha256(v.ciphertext_base64.as_bytes())) == v.msg_hash_hex
    assert decrypt_message_v2(v.ciphertext_base64, v.symmetric_key_hex,
                              v.iv_hex, v.hmac_hex, v.room_id) == v.plaintext_utf8

for v in roomKeyWraps:
    assert uncompressed_pub(v.recipient_private_key_hex) == v.recipient_public_key_hex
    assert uncompressed_pub(v.ephemeral_private_key_hex) == v.ephemeral_public_key_hex
    w = wrap_v2(v.room_symmetric_key_hex, v.recipient_public_key_hex, v.room_id,
                eph_priv = v.ephemeral_private_key_hex, iv = v.iv_hex)
    assert w.ciphertext == v.encrypted_symmetric_key_base64
    assert w.eph_pub    == v.ephemeral_public_key_hex
    assert w.hmac       == v.hmac_hex
    assert unwrap_v2(w, v.recipient_private_key_hex, v.room_id) == v.room_symmetric_key_hex
```

### 12.3 Negative tests (the reference porting checklist requires these)

Each MUST return an authentication error **before** any AES operation:

- flip one bit of the ciphertext
- flip one bit of the IV
- flip one bit of the HMAC
- truncate/extend the HMAC (length mismatch → reject)
- use a different `roomId` in the MAC context (cross-room replay)
- **wraps only:** substitute a different (valid) ephemeral public key
- **wraps only:** an `ephemeralPublicKey` that is not a valid curve point → reject at parse
- unwrap producing a string that fails `^[0-9a-f]{64}$/i` → reject

### 12.4 Vector values (for eyeballing; the JSON is authoritative)

Shared constants:

```
K1     = 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08
K2     = 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f
roomId = room-vector-0001
```

Subkeys:

| key | label | subkey |
| --- | --- | --- |
| K1 | `FruitNation/v2/message/enc` | `1391162eeeeb69860c140af5cd201691ff07bfb4822f01c6d59e954846ecdcc9` |
| K1 | `FruitNation/v2/message/mac` | `3f49d718c2c07fca9155deeb689715f55af143a95b3b5f18acfb3a20d9594088` |
| K1 | `FruitNation/v2/roomkey/enc` | `78cb5ef8ddb7232c7f6d0287b9608ca38a446fb33a7819236527b24946023644` |
| K1 | `FruitNation/v2/roomkey/mac` | `601bc6fb7ef7c5779c154ead210251cb5c38aedbda04f5d5e75ed77c5d4ecc1b` |
| K2 | `FruitNation/v2/message/enc` | `0615fce531895ef6a62e4dbb8352bca0b992927db98c013a2710c7608f1d9ecb` |
| K2 | `FruitNation/v2/message/mac` | `95ea78f04d63373b907cd7e6dc2bf56b100fd7d64b626c2747ddb1b8149a38fc` |
| K2 | `FruitNation/v2/roomkey/enc` | `7c64755ea076c1706e55f8586c64b5140f2067aacb67d507c2cd1cea0197595d` |
| K2 | `FruitNation/v2/roomkey/mac` | `1f8a7be6d1f1c8206eda29d3955e31220a8409bd252544a321ca418ef62b3c8a` |

Messages (all `roomId = room-vector-0001`):

```
--- ascii (key K1) ---
plaintext : attack at dawn
iv        : 000102030405060708090a0b0c0d0e0f
ciphertext: 3nP4XMnquk7mpaDFxNxnZA==
hmac      : 403a7ba221a2769b6503b298de98ae0e82010a0fadb07a8aeb234aa7322bf4a7
msgHash   : 62a40b0ecaa574014eaf4c19c518bb596ed502b4c5e55c01e3e647e7b3f9e3a9

--- quotes-and-percent (key K1) ---
plaintext : I'm 50% sure — "ok"
            (ASCII apostrophe U+0027, ASCII quotes U+0022, EM DASH U+2014 = 3 UTF-8 bytes;
             21 UTF-8 bytes total, so it PKCS#7-pads across 2 AES blocks)
iv        : 0f0e0d0c0b0a09080706050403020100
ciphertext: jLykKspwGTDA6abyS7HrIsSbcL6kRO4RixQVgE+VlJk=
hmac      : 21fcc30a571e39540f7d3277fb7bcbe9e1ba2cbc3ce3fec01825aa60ded7ce5e
msgHash   : cfb37d96881cc09c28e71882fd1db8597cb842997edd0e12ca0561ddef2b5994

--- unicode-emoji (key K1) ---
plaintext : 한글 메시지 🍓🍊
iv        : 101112131415161718191a1b1c1d1e1f
ciphertext: AeHMd1L87BW8NOlkHslfgN7D7U3yQPnhvrm9X20aeh8=
hmac      : e8b2ec13ca3124ef7736d5096b604c7b665292a89297c3cde8ab403dc6e6efe0
msgHash   : 5b75ba267ea223ed430e57b0aea603fbb1439c8b6123614ed4990088b4aea7ad

--- single-char (key K1) ---
plaintext : a
iv        : 202122232425262728292a2b2c2d2e2f
ciphertext: JndrHohFMVfj9jxv6h0ZkA==
hmac      : f2c627df7c4d921efe20d95802035adc17432216e249e64a2c0646df077db250
msgHash   : aee6a4fcab6794dd7bcbd7293a3fd7c54aec6c919f57f16a18250aee86e12308

--- multi-block (key K1) ---
plaintext : "0123456789abcdef" repeated 8 times (128 bytes)
iv        : 303132333435363738393a3b3c3d3e3f
ciphertext: ktKcOahOdVuWe4rkP/lv//pxnglyIYDuZSteFUhcraufCsOEl9IeDZowi3T4u4vRv/CZ2RP3snVnBsbl7bF5v6E/Nfkizv7W8QfXLXoHFg6g6mgGmpz8sw0u7YSHE/q+U/ySnoCY06ekfQSaB2wB9YQFu3vbLuSNAd3HA8VDZ019ndTk/PHz7KimPi+Mgi/z
hmac      : 85a43ec3fcb24cc248649696c2464492dcfd1795f3072968859b98b88aa85b24
msgHash   : c8d463c0b4e2a9d6e30dc994891861449b341793aab154e5c3c6c2f29d89c368

--- second-key (key K2) ---
plaintext : same format, different key
iv        : 404142434445464748494a4b4c4d4e4f
ciphertext: 8vxniDJFJnsBlZK8WSJtSwRARGBcTclQ/e4WEkeCtnM=
hmac      : 006687a964033b86bb4ae390e71a85623171385362a59925f0379ff88087e60b
msgHash   : ea0ae8d472d8dd4d88d0cefabeccaa4f439de2e75be0e9a987eab81b0dd7d257
```

Room-key wrap `wrap-1`:

```
roomSymmetricKey : 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08   (K1)
roomId           : room-vector-0001
recipientPriv    : 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
recipientPub     : 044646ae5047316b4230d0086c8acec687f00b1cd9d1dc634f6cb358ac0a9a8ffffe77b4dd0a4bfb95851f3b7355c781dd60f8418fc8a65d14907aff47c903a559
ephemeralPriv    : 2222222222222222222222222222222222222222222222222222222222222222
ephemeralPub     : 04466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f276728176c3c6431f8eeda4538dc37c865e2784f3a9e77d044f33e407797e1278a
sharedX          : 862f2e40830f671dbe6c39599174d13c127fe11ee95738764a9a3f22d99dcc14   [derived, not in JSON]
wrapEncKey       : subkey(sharedX, "FruitNation/v2/roomkey/enc")
wrapMacKey       : subkey(sharedX, "FruitNation/v2/roomkey/mac")
iv               : 505152535455565758595a5b5c5d5e5f
ciphertext       : TS/2g90wOFyPTYukrttp7IEboZROacO4J1Dbz5x4uGxpJ8VRN/2vBQVnNrbxj0ToDDTcACGiGnwmG8WEmahxpGwWu5AWIBBEyO0TD0t+woI=
hmac             : 02b3df9450f08183c29408e8042d4b612e85ec7cbb5dad7c2a79c347c8a66ea1

MAC input string:
FNv2|roomkey|room-vector-0001|04466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f276728176c3c6431f8eeda4538dc37c865e2784f3a9e77d044f33e407797e1278a|505152535455565758595a5b5c5d5e5f|TS/2g90wOFyPTYukrttp7IEboZROacO4J1Dbz5x4uGxpJ8VRN/2vBQVnNrbxj0ToDDTcACGiGnwmG8WEmahxpGwWu5AWIBBEyO0TD0t+woI=
```

`sharedX` is an intermediate — it is **not** in the JSON, but it is a valuable
unit-test checkpoint: if your ECDH produces
`862f2e40830f671dbe6c39599174d13c127fe11ee95738764a9a3f22d99dcc14`, your curve
math and x-coordinate encoding are correct.

Note the wrap ciphertext is **108 Base64 chars** (80 raw bytes = 64-byte hex
plaintext + 16 bytes PKCS#7 padding). See §7.3.

### 12.5 Vectors not in the JSON

`crypto-v2.json` covers only the label-KDF, message crypto and wrap crypto. The
following are specified in this document with worked, independently verified
values and should be turned into Rust tests too:

- §1.4 EIP-55 checksum (2 vectors)
- §2.4 EIP-191 digest + signature (1 vector, with `r`/`s`/`v` split)
- §3.1 salted derivation (1 vector)
- §3.2 legacy derivation (2 vectors — also address-derivation vectors)
- §4.3 key binding sign + recover (1 vector)
- §8.1 v1 message ciphertext + the ASCII-hex-HMAC-key gotcha (1 vector)

---

## 13. OPEN QUESTIONS

Items where the TypeScript does not fully pin down behavior. Each carries a
best-evidence answer — implement the stated answer, and revisit if upstream
clarifies.

**OQ-1 — `keyVersion` on individual wrap rows sent to `/rotate-key`.**
The client-side `rotateRoomKey` builds each wrap object *without* a `keyVersion`
field; the server overwrites it with the request-level `body.newVersion` for
every row. The per-row field is accepted by the zod schema but ignored.
**Answer:** omit `keyVersion` from individual wrap objects in a rotate request;
send it at the request level as `newVersion`. For the single-key
`POST /api/rooms/:roomId/key` endpoint, `keyVersion` **is** per-row and defaults
to 1 if omitted — always send it explicitly.

**OQ-2 — Emoticon `msgHash` address casing.**
`eventData` interpolates `authReq.user.walletAddress`, which comes from the JWT
payload, which was signed from `user.walletAddress` after `upsertUser`
lowercased it. So it is lowercase in practice, but nothing in the type system
enforces it.
**Answer:** treat emoticon `msgHash` as an opaque server-produced value. Never
recompute or validate it client-side.

**OQ-3 — `encVer` on message rows written before the field existed.**
The DB column has a default and old rows may be `null`.
**Answer:** `encVer ?? 1` and `keyVersion ?? 1` — matches
`decryptMessageByVersion` / `decryptRoomKeyByVersion` and the server's
`validatedMessage.encVer ?? 1`.

**OQ-4 — Low-S enforcement on inbound `publicKeySig`.**
`ethers.verifyMessage` rejects high-S signatures (ethers v6 enforces
canonicality). The server relies on that; the zod schema only checks the hex
shape and a ≤200-char bound.
**Answer:** when *producing* signatures, always low-S normalize (`k256`'s
`SigningKey::sign_prehash_recoverable` does this). When *verifying*, reject
high-S to match ethers, so a signature your client accepts is one the server
would also accept.

**OQ-5 — Empty-plaintext messages.**
`messageContent` is `min(1)` post-trim, and the v2 decryptor throws when the
UTF-8 decode yields `""` (CryptoJS returns `""` both for "genuinely empty" and
"invalid UTF-8"). A correctly MAC-verified ciphertext of the empty string would
therefore be reported as a decryption failure.
**Answer:** never encrypt an empty (or whitespace-only) plaintext; the server
rejects it anyway. In Rust, distinguish "decrypted to empty" from "invalid
UTF-8" internally, but still refuse to *send* empty content.

**OQ-6 — Salt rotation.**
`getOrCreateEncryptionSalt` never rotates an existing salt (`onConflictDoNothing`,
then re-read the winner). There is no documented salt-rotation flow.
**Answer:** treat the salt as immutable per account. If it ever changed, every
existing room-key wrap for that account would become unreadable and would need
the same heal-and-rewrap treatment as the legacy derivation.
