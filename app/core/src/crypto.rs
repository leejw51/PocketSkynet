//! The FruitNation v2 end-to-end encryption primitives.
//!
//! Two things are encrypted in this protocol and they use different key
//! material but the *same* shape:
//!
//! * **messages** — AES-256-CBC under a subkey of the room's symmetric key;
//! * **room-key wraps** — AES-256-CBC under a subkey of an ECDH shared secret,
//!   so a room key can be handed to a member without the server reading it.
//!
//! Both are **encrypt-then-MAC**: the HMAC covers the ciphertext, the IV, the
//! room id and a version tag, and it is checked *before* any AES operation
//! runs. That ordering is what makes a padding oracle unreachable, and it is
//! why every decryption entry point in this module verifies first and decrypts
//! second.
//!
//! Byte-for-byte reference: `docs/CRYPTO.md` §5–§8, itself derived from
//! `server/client/src/lib/encryption.ts` and validated against
//! `server/test/vectors/crypto-v2.json`.

use aes::Aes256;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use elliptic_curve::sec1::ToEncodedPoint;
use hmac::{Hmac, Mac};
use k256::{ecdh::diffie_hellman, PublicKey, SecretKey};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::random;

type Aes256CbcEnc = cbc::Encryptor<Aes256>;
type Aes256CbcDec = cbc::Decryptor<Aes256>;
type HmacSha256 = Hmac<Sha256>;

/// Everything that can go wrong in the crypto layer.
///
/// The interesting design constraint is [`CryptoError::DecryptionFailed`]: it
/// is deliberately **one** variant covering "the MAC did not verify", "the
/// base64 was malformed", "the padding was wrong", "the plaintext was not
/// UTF-8" and "the unwrapped room key was not 64 hex characters". Every one of
/// those inputs is attacker-controlled, so distinguishing them in the return
/// type would hand an attacker a decryption oracle. Anything a *caller* got
/// wrong (a bad local private key, an unsupported version) keeps its own
/// variant, because that is a programming error, not an oracle.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CryptoError {
    /// Authentication failed, or the authenticated plaintext was malformed.
    ///
    /// Never add detail to this variant. Callers must not be able to tell
    /// "wrong MAC" from "bad padding" from "not UTF-8".
    #[error("decryption failed")]
    DecryptionFailed,

    /// A private key was not a valid secp256k1 scalar (zero, or ≥ n).
    #[error("invalid private key")]
    InvalidPrivateKey,

    /// A public key was not a valid, non-identity secp256k1 point.
    #[error("invalid public key")]
    InvalidPublicKey,

    /// Hex or base64 that the *caller* supplied could not be decoded, or had
    /// the wrong length. Never returned from a decryption path.
    #[error("malformed encoding")]
    InvalidEncoding,

    /// A signature was not 65 bytes, had an unusable `v`, was non-canonical
    /// (high-S), or did not recover to a point.
    #[error("invalid signature")]
    InvalidSignature,

    /// Signing failed. In practice unreachable with a valid key.
    #[error("signing failed")]
    SigningFailed,

    /// A published encryption public key is missing, unsigned, or its binding
    /// signature does not recover to the claimed wallet address.
    #[error("public key binding verification failed")]
    KeyBindingFailed,

    /// The operation needs a wallet key on this device, and this session signed
    /// in with an external wallet that holds its own.
    ///
    /// Distinct from [`Self::InvalidSignature`] on purpose: nothing was wrong
    /// with any signature, and reporting one that way sends whoever reads the
    /// log looking for corruption instead of for a browser wallet.
    #[error("no local wallet key")]
    NoLocalKey,

    /// The BIP-39 phrase had a bad word, a bad length, or a bad checksum.
    #[error("invalid mnemonic")]
    InvalidMnemonic,

    /// BIP-32 hit the ~2⁻¹²⁷ case where a child index yields an invalid key.
    /// BIP-32 says to skip to the next index; we surface it instead of
    /// silently changing the derivation path out from under the caller.
    #[error("BIP-32 child key derivation failed for this index")]
    KeyDerivation,

    /// `encVer` was neither 1 nor 2.
    #[error("unsupported encryption version {0}")]
    UnsupportedVersion(u32),

    /// Refusing to encrypt an empty or whitespace-only message: the server
    /// rejects it post-trim and the reference decryptor reports an empty
    /// plaintext as a failure, so it could never round-trip.
    #[error("refusing to encrypt empty content")]
    EmptyPlaintext,

    /// The per-account encryption salt was not 64 hex characters.
    #[error("invalid encryption salt")]
    InvalidSalt,

    /// The OS/browser CSPRNG refused to produce bytes. Fail loudly rather than
    /// fall back to anything weaker.
    #[error("secure random number generation failed")]
    Randomness,
}

// ---------------------------------------------------------------------------
// Subkey derivation (docs/CRYPTO.md §5)
// ---------------------------------------------------------------------------

/// AES-256 key label for message encryption.
pub const MSG_ENC_LABEL: &str = "FruitNation/v2/message/enc";
/// HMAC-SHA256 key label for message authentication.
pub const MSG_MAC_LABEL: &str = "FruitNation/v2/message/mac";
/// AES-256 key label for room-key wrapping.
pub const WRAP_ENC_LABEL: &str = "FruitNation/v2/roomkey/enc";
/// HMAC-SHA256 key label for room-key-wrap authentication.
pub const WRAP_MAC_LABEL: &str = "FruitNation/v2/roomkey/mac";

/// The label-KDF: `HMAC-SHA256(key = 32 raw key bytes, message = ASCII label)`.
///
/// The full 32-byte tag is used directly — no truncation, no HKDF-Expand, no
/// second round. Substituting HKDF here (the "obviously more correct" choice)
/// would change every ciphertext this protocol has ever produced.
///
/// The argument order is the trap. The TypeScript reads
/// `CryptoJS.HmacSHA256(label, key)`, whose signature is `(message, key)` — so
/// the *label* is the message and the *key material* is the key. Swapping them
/// still produces 32 plausible-looking bytes, which is exactly why it has to be
/// pinned by the vectors.
pub fn derive_subkey(key: &[u8; 32], label: &str) -> [u8; 32] {
    let mut mac = <HmacSha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(label.as_bytes());
    mac.finalize().into_bytes().into()
}

/// Compute `HMAC-SHA256(key, utf8(input))` and return it as lowercase hex.
pub(crate) fn mac_hex(key: &[u8; 32], input: &str) -> String {
    let mut mac = <HmacSha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(input.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Constant-time check of a received hex MAC against a freshly computed one.
///
/// The received string is decoded, never compared as text: comparing hex would
/// make the result depend on the *casing* the peer chose, and the whole point
/// of §0's mixed-case trap is that we never normalise attacker-supplied strings
/// in a way that changes the security decision. A length mismatch short-circuits
/// (length leakage is acceptable; content leakage is not), and the 32-byte
/// comparison itself goes through [`subtle`] so it does not branch on data.
pub(crate) fn verify_mac(key: &[u8; 32], input: &str, expected_hex: &str) -> bool {
    let Ok(expected) = hex::decode(expected_hex) else {
        return false;
    };
    if expected.len() != 32 {
        return false;
    }
    let mut mac = <HmacSha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(input.as_bytes());
    let computed = mac.finalize().into_bytes();
    computed.as_slice().ct_eq(expected.as_slice()).into()
}

// ---------------------------------------------------------------------------
// AES-256-CBC helpers
// ---------------------------------------------------------------------------

pub(crate) fn aes_cbc_encrypt(key: &[u8; 32], iv: &[u8; 16], plaintext: &[u8]) -> Vec<u8> {
    Aes256CbcEnc::new(key.into(), iv.into()).encrypt_padded_vec_mut::<Pkcs7>(plaintext)
}

/// Decrypt, mapping *every* failure to [`CryptoError::DecryptionFailed`].
///
/// Callers must have verified the MAC first, so a padding error here means a
/// key mismatch rather than an attacker probing — but the error stays opaque
/// regardless, so no future refactor can turn it into an oracle.
pub(crate) fn aes_cbc_decrypt(
    key: &[u8; 32],
    iv: &[u8; 16],
    ciphertext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    if ciphertext.is_empty() || ciphertext.len() % 16 != 0 {
        return Err(CryptoError::DecryptionFailed);
    }
    Aes256CbcDec::new(key.into(), iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(ciphertext)
        .map_err(|_| CryptoError::DecryptionFailed)
}

/// A fresh 32-byte room symmetric key.
///
/// No structure, no derivation, no version marker inside the key — the epoch
/// lives beside it in `keyVersion`, never in the key material.
///
/// Straight from [`crate::random`], with no post-processing whatsoever. Hashing
/// or stretching the draw would look like diligence and would add nothing: the
/// output of the OS CSPRNG is already the strongest thing available, and every
/// extra step is one more place for a mistake to hide.
pub fn generate_room_key() -> Result<[u8; 32], CryptoError> {
    random::bytes::<32>()
}

pub(crate) fn decode_iv(iv_hex: &str) -> Option<[u8; 16]> {
    let mut iv = [0u8; 16];
    hex::decode_to_slice(iv_hex, &mut iv).ok()?;
    Some(iv)
}

// ---------------------------------------------------------------------------
// Message encryption v2 (docs/CRYPTO.md §6)
// ---------------------------------------------------------------------------

/// The wire fields produced by [`encrypt_message_v2`].
///
/// Field names match the JSON the server expects, so this struct can be
/// splatted straight into a message-create request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedMessage {
    /// Base64 (standard alphabet, padded) of the raw AES output.
    pub content: String,
    /// 32 lowercase hex characters.
    pub iv: String,
    /// 64 lowercase hex characters.
    pub hmac: String,
}

/// The exact MAC input for a v2 message: `FNv2|message|{roomId}|{ivHex}|{ctB64}`.
///
/// The `|` separator is the only framing — nothing is length-prefixed — which
/// is unambiguous only because `roomId` is server-restricted to
/// `[A-Za-z0-9_.-]` and hex/base64 cannot contain `|`. If the server ever
/// widened the roomId charset, this encoding would need a length prefix.
fn message_mac_input(room_id: &str, iv_hex: &str, ciphertext_b64: &str) -> String {
    format!("FNv2|message|{room_id}|{iv_hex}|{ciphertext_b64}")
}

/// Encrypt a message under the room's symmetric key with a fresh random IV.
///
/// A new IV per message is mandatory: CBC with a repeated IV leaks whether two
/// messages share a prefix.
pub fn encrypt_message_v2(
    plaintext: &str,
    room_key: &[u8; 32],
    room_id: &str,
) -> Result<EncryptedMessage, CryptoError> {
    encrypt_message_v2_with_iv(plaintext, room_key, room_id, &random::bytes::<16>()?)
}

/// Encrypt with a caller-supplied IV.
///
/// **Only for reproducing test vectors.** Production code must call
/// [`encrypt_message_v2`], which draws the IV from the CSPRNG. This is public
/// solely so the integration test in `tests/vectors.rs` can assert
/// byte-equality against the canonical file.
pub fn encrypt_message_v2_with_iv(
    plaintext: &str,
    room_key: &[u8; 32],
    room_id: &str,
    iv: &[u8; 16],
) -> Result<EncryptedMessage, CryptoError> {
    if plaintext.trim().is_empty() {
        return Err(CryptoError::EmptyPlaintext);
    }

    let enc_key = derive_subkey(room_key, MSG_ENC_LABEL);
    let mac_key = derive_subkey(room_key, MSG_MAC_LABEL);

    let ciphertext = aes_cbc_encrypt(&enc_key, iv, plaintext.as_bytes());
    // Standard alphabet *with* padding: the `=` characters are part of the MAC
    // input, so switching to a no-pad engine would silently break every peer.
    let content = BASE64.encode(&ciphertext);
    let iv_hex = hex::encode(iv);
    let hmac = mac_hex(&mac_key, &message_mac_input(room_id, &iv_hex, &content));

    Ok(EncryptedMessage {
        content,
        iv: iv_hex,
        hmac,
    })
}

/// Verify and decrypt a v2 message.
///
/// `iv_hex`, `hmac_hex` and `ciphertext_b64` must be the strings **exactly as
/// received**. Do not lowercase them first: the server accepts mixed-case hex
/// in some fields, so the sender's casing is part of what was authenticated,
/// and "helpfully" normalising it turns a valid message into a MAC failure (or,
/// worse, invites someone to normalise on the *sending* side too and lose the
/// binding).
pub fn decrypt_message_v2(
    ciphertext_b64: &str,
    iv_hex: &str,
    hmac_hex: &str,
    room_key: &[u8; 32],
    room_id: &str,
) -> Result<String, CryptoError> {
    let mac_key = derive_subkey(room_key, MSG_MAC_LABEL);
    let mac_input = message_mac_input(room_id, iv_hex, ciphertext_b64);

    // Step 1: authenticate. Nothing below this line runs on unauthenticated
    // data, which is what keeps the CBC padding check from becoming an oracle.
    if !verify_mac(&mac_key, &mac_input, hmac_hex) {
        return Err(CryptoError::DecryptionFailed);
    }

    let iv = decode_iv(iv_hex).ok_or(CryptoError::DecryptionFailed)?;
    let ciphertext = BASE64
        .decode(ciphertext_b64)
        .map_err(|_| CryptoError::DecryptionFailed)?;

    let enc_key = derive_subkey(room_key, MSG_ENC_LABEL);
    let plaintext = aes_cbc_decrypt(&enc_key, &iv, &ciphertext)?;

    let text = String::from_utf8(plaintext).map_err(|_| CryptoError::DecryptionFailed)?;
    if text.is_empty() {
        // Matches the reference decryptor, which throws on an empty result.
        // Unreachable in practice: we refuse to encrypt empty content, and the
        // server enforces `min(1)` post-trim.
        return Err(CryptoError::DecryptionFailed);
    }
    Ok(text)
}

// ---------------------------------------------------------------------------
// Room-key wrapping v2 (docs/CRYPTO.md §7)
// ---------------------------------------------------------------------------

/// A room key wrapped to one member's encryption public key.
///
/// Field names match the wire JSON. Keep the strings verbatim once received:
/// [`unwrap_room_key_v2`] re-MACs over them exactly as they arrived.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WrappedRoomKey {
    /// Base64 of the AES output over the *hex string* of the room key.
    pub encrypted_symmetric_key: String,
    /// 130 hex characters, uncompressed `04 ‖ X ‖ Y`, no `0x` prefix.
    pub ephemeral_public_key: String,
    /// 32 hex characters.
    pub encryption_iv: String,
    /// 64 hex characters.
    pub hmac: String,
}

/// The exact MAC input for a v2 wrap:
/// `FNv2|roomkey|{roomId}|{ephPubHex}|{ivHex}|{ctB64}`.
///
/// Unlike v1, the ephemeral public key **is** authenticated here — otherwise a
/// server could swap in its own ephemeral key and the recipient would have no
/// way to notice.
fn room_key_mac_input(
    room_id: &str,
    ephemeral_public_key_hex: &str,
    iv_hex: &str,
    ciphertext_b64: &str,
) -> String {
    format!("FNv2|roomkey|{room_id}|{ephemeral_public_key_hex}|{iv_hex}|{ciphertext_b64}")
}

/// ECDH, returning the raw 32-byte big-endian X coordinate of the shared point.
///
/// **The shared secret is the X coordinate and nothing else.** Not the
/// compressed point, not `04‖X‖Y`, not `X‖Y`, and — critically — it is **not
/// hashed** before the label-KDF. `HMAC-SHA256(sharedX, label)` *is* the KDF.
/// Adding the "obvious" SHA-256 or HKDF step here would change every wrap.
///
/// The 32-byte width matters too: the TypeScript has to `padStart(64, "0")`
/// because its bignum drops leading zeros. `k256` returns a fixed-width buffer,
/// so the zero-padding is free — but only if you take these bytes and not a
/// re-encoded integer.
fn ecdh_shared_x(secret: &SecretKey, public: &PublicKey) -> [u8; 32] {
    let shared = diffie_hellman(secret.to_nonzero_scalar(), public.as_affine());
    (*shared.raw_secret_bytes()).into()
}

/// Wrap a room key for one recipient, with a fresh ephemeral key and IV.
///
/// The ephemeral keypair is never reused: reuse would make two wraps to the
/// same recipient share a key stream position and, more importantly, would turn
/// a single ephemeral-key compromise into a compromise of every wrap made with
/// it.
///
/// The caller must have verified the recipient's public key binding first (see
/// [`crate::keys::verify_key_binding`]) — wrapping to an unverified key is the
/// one mistake that silently voids the entire E2EE guarantee.
pub fn wrap_room_key_v2(
    room_key: &[u8; 32],
    recipient_public_key: &PublicKey,
    room_id: &str,
) -> Result<WrappedRoomKey, CryptoError> {
    let ephemeral = random::secret_key()?;
    let iv = random::bytes::<16>()?;
    Ok(wrap_room_key_v2_with(
        room_key,
        recipient_public_key,
        room_id,
        &ephemeral,
        &iv,
    ))
}

/// Wrap with a caller-supplied ephemeral key and IV.
///
/// **Only for reproducing test vectors.** Reusing an ephemeral key in
/// production defeats the point of using one; call [`wrap_room_key_v2`].
pub fn wrap_room_key_v2_with(
    room_key: &[u8; 32],
    recipient_public_key: &PublicKey,
    room_id: &str,
    ephemeral_secret: &SecretKey,
    iv: &[u8; 16],
) -> WrappedRoomKey {
    let shared_x = ecdh_shared_x(ephemeral_secret, recipient_public_key);
    let enc_key = derive_subkey(&shared_x, WRAP_ENC_LABEL);
    let mac_key = derive_subkey(&shared_x, WRAP_MAC_LABEL);

    // The plaintext is the 64-character lowercase hex ASCII *string* of the
    // room key, not its 32 raw bytes. 64 bytes → 80 after PKCS#7 → 108 base64
    // characters. A 44-character wrap means someone encrypted the raw bytes and
    // no other client will be able to read it.
    let plaintext = hex::encode(room_key);
    let ciphertext = aes_cbc_encrypt(&enc_key, iv, plaintext.as_bytes());
    let encrypted_symmetric_key = BASE64.encode(&ciphertext);

    let ephemeral_public_key = uncompressed_public_key_hex(&ephemeral_secret.public_key());
    let encryption_iv = hex::encode(iv);
    let hmac = mac_hex(
        &mac_key,
        &room_key_mac_input(
            room_id,
            &ephemeral_public_key,
            &encryption_iv,
            &encrypted_symmetric_key,
        ),
    );

    WrappedRoomKey {
        encrypted_symmetric_key,
        ephemeral_public_key,
        encryption_iv,
        hmac,
    }
}

/// Verify and unwrap a v2 room key with the recipient's E2EE private key.
///
/// `room_id` must be the room the wrap was *fetched for*, not one echoed back
/// by the server — binding to a server-supplied room id would defeat the
/// cross-room replay protection the MAC exists to provide.
pub fn unwrap_room_key_v2(
    wrap: &WrappedRoomKey,
    recipient_secret: &SecretKey,
    room_id: &str,
) -> Result<[u8; 32], CryptoError> {
    // `from_sec1_bytes` rejects off-curve points and the identity, so an
    // attacker cannot force a degenerate shared secret. The failure is folded
    // into `DecryptionFailed` because the ephemeral key is attacker-controlled.
    let ephemeral_public = parse_uncompressed_public_key(&wrap.ephemeral_public_key)
        .map_err(|_| CryptoError::DecryptionFailed)?;

    let shared_x = ecdh_shared_x(recipient_secret, &ephemeral_public);
    let mac_key = derive_subkey(&shared_x, WRAP_MAC_LABEL);

    let mac_input = room_key_mac_input(
        room_id,
        &wrap.ephemeral_public_key,
        &wrap.encryption_iv,
        &wrap.encrypted_symmetric_key,
    );
    if !verify_mac(&mac_key, &mac_input, &wrap.hmac) {
        return Err(CryptoError::DecryptionFailed);
    }

    let iv = decode_iv(&wrap.encryption_iv).ok_or(CryptoError::DecryptionFailed)?;
    let ciphertext = BASE64
        .decode(&wrap.encrypted_symmetric_key)
        .map_err(|_| CryptoError::DecryptionFailed)?;

    let enc_key = derive_subkey(&shared_x, WRAP_ENC_LABEL);
    let plaintext = aes_cbc_decrypt(&enc_key, &iv, &ciphertext)?;
    parse_room_key_hex(&plaintext)
}

/// Validate `^[0-9a-f]{64}$` case-insensitively and hex-decode.
///
/// The reference regex is `/^[0-9a-f]{64}$/i`, so uppercase is accepted on the
/// way in even though every encoder emits lowercase. This check is what stops a
/// wrap that decrypted to garbage-but-well-padded bytes from being installed as
/// a room key.
fn parse_room_key_hex(plaintext: &[u8]) -> Result<[u8; 32], CryptoError> {
    if plaintext.len() != 64 || !plaintext.iter().all(u8::is_ascii_hexdigit) {
        return Err(CryptoError::DecryptionFailed);
    }
    let mut key = [0u8; 32];
    hex::decode_to_slice(plaintext, &mut key).map_err(|_| CryptoError::DecryptionFailed)?;
    Ok(key)
}

// ---------------------------------------------------------------------------
// Public-key encoding helpers
// ---------------------------------------------------------------------------

/// Encode a public key the way this protocol carries it: uncompressed
/// `04 ‖ X ‖ Y`, 130 lowercase hex characters, **no** `0x` prefix.
///
/// The compressed (33-byte) form is never used on the wire, and mixing the two
/// would change the key-binding message and invalidate the signature over it.
pub fn uncompressed_public_key_hex(public_key: &PublicKey) -> String {
    hex::encode(public_key.to_encoded_point(false).as_bytes())
}

/// Parse a 130-hex-character uncompressed public key.
///
/// Only the uncompressed form is accepted: allowing compressed input would mean
/// two different strings map to the same key, and the key-binding signature is
/// over the *string*.
pub fn parse_uncompressed_public_key(hex_str: &str) -> Result<PublicKey, CryptoError> {
    if hex_str.len() != 130 {
        return Err(CryptoError::InvalidPublicKey);
    }
    let bytes = hex::decode(hex_str).map_err(|_| CryptoError::InvalidPublicKey)?;
    if bytes[0] != 0x04 {
        return Err(CryptoError::InvalidPublicKey);
    }
    PublicKey::from_sec1_bytes(&bytes).map_err(|_| CryptoError::InvalidPublicKey)
}

// ---------------------------------------------------------------------------
// LEGACY encVer = 1 (docs/CRYPTO.md §8) — DECRYPT ONLY
// ---------------------------------------------------------------------------

/// **Legacy, decrypt-only.** Read a pre-v2 message.
///
/// Never write `encVer: 1`. v1 authenticates *only the ciphertext*: no version
/// tag, no room id, no IV. That means the IV can be bit-flipped to tamper with
/// the first plaintext block and ciphertexts replay freely across rooms. These
/// paths exist to read history, nothing more — there is no v1 encryptor in this
/// crate on purpose.
///
/// The one thing that must be reproduced exactly: the AES key is the **32 raw
/// bytes** of the room key while the HMAC key is the **64 ASCII bytes of its
/// lowercase hex string**. CryptoJS UTF-8-encodes a `String` key, and the
/// reference passed the hex string to the HMAC while passing parsed bytes to
/// AES. It was a mistake, it is now the protocol.
pub fn decrypt_message_v1(
    ciphertext_b64: &str,
    iv_hex: &str,
    hmac_hex: &str,
    room_key: &[u8; 32],
) -> Result<String, CryptoError> {
    let key_ascii = hex::encode(room_key);
    let mut mac =
        <HmacSha256>::new_from_slice(key_ascii.as_bytes()).expect("HMAC accepts any key length");
    mac.update(ciphertext_b64.as_bytes());
    let computed = mac.finalize().into_bytes();

    let Ok(expected) = hex::decode(hmac_hex) else {
        return Err(CryptoError::DecryptionFailed);
    };
    if expected.len() != 32 || !bool::from(computed.as_slice().ct_eq(expected.as_slice())) {
        return Err(CryptoError::DecryptionFailed);
    }

    let iv = decode_iv(iv_hex).ok_or(CryptoError::DecryptionFailed)?;
    let ciphertext = BASE64
        .decode(ciphertext_b64)
        .map_err(|_| CryptoError::DecryptionFailed)?;
    let plaintext = aes_cbc_decrypt(room_key, &iv, &ciphertext)?;

    let text = String::from_utf8(plaintext).map_err(|_| CryptoError::DecryptionFailed)?;
    if text.is_empty() {
        return Err(CryptoError::DecryptionFailed);
    }
    Ok(text)
}

/// **Legacy, decrypt-only.** Unwrap a pre-v2 room key.
///
/// v1 uses the raw ECDH x-coordinate **directly as the AES-256 key** — no KDF
/// at all — and authenticates neither the ephemeral public key nor the IV nor
/// the room id. The HMAC key is again the 64 ASCII bytes of the shared-secret
/// hex string, mirroring the v1 message quirk.
///
/// A successful unwrap here should be followed immediately by re-wrapping the
/// recovered key to the v2 public key at the same `keyVersion` (the "healing"
/// path), so the v1 row eventually disappears.
pub fn unwrap_room_key_v1(
    wrap: &WrappedRoomKey,
    recipient_secret: &SecretKey,
) -> Result<[u8; 32], CryptoError> {
    let ephemeral_public = parse_uncompressed_public_key(&wrap.ephemeral_public_key)
        .map_err(|_| CryptoError::DecryptionFailed)?;
    let shared_x = ecdh_shared_x(recipient_secret, &ephemeral_public);
    let shared_x_ascii = hex::encode(shared_x);

    let mut mac = <HmacSha256>::new_from_slice(shared_x_ascii.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(wrap.encrypted_symmetric_key.as_bytes());
    let computed = mac.finalize().into_bytes();

    let Ok(expected) = hex::decode(&wrap.hmac) else {
        return Err(CryptoError::DecryptionFailed);
    };
    if expected.len() != 32 || !bool::from(computed.as_slice().ct_eq(expected.as_slice())) {
        return Err(CryptoError::DecryptionFailed);
    }

    let iv = decode_iv(&wrap.encryption_iv).ok_or(CryptoError::DecryptionFailed)?;
    let ciphertext = BASE64
        .decode(&wrap.encrypted_symmetric_key)
        .map_err(|_| CryptoError::DecryptionFailed)?;
    // No KDF: sharedX itself is the AES key.
    let plaintext = aes_cbc_decrypt(&shared_x, &iv, &ciphertext)?;
    parse_room_key_hex(&plaintext)
}

/// Normalise a possibly-absent `encVer` field.
///
/// A missing or `null` `encVer` means **1** — rows predate the column. Same
/// rule as the reference `decryptMessageByVersion`.
pub fn effective_enc_ver(enc_ver: Option<u32>) -> u32 {
    enc_ver.unwrap_or(1)
}

/// Decrypt a message using whichever version it was written under.
pub fn decrypt_message_by_version(
    enc_ver: Option<u32>,
    ciphertext_b64: &str,
    iv_hex: &str,
    hmac_hex: &str,
    room_key: &[u8; 32],
    room_id: &str,
) -> Result<String, CryptoError> {
    match effective_enc_ver(enc_ver) {
        v if v >= 2 => decrypt_message_v2(ciphertext_b64, iv_hex, hmac_hex, room_key, room_id),
        1 => decrypt_message_v1(ciphertext_b64, iv_hex, hmac_hex, room_key),
        other => Err(CryptoError::UnsupportedVersion(other)),
    }
}

/// Unwrap a room key using whichever version it was written under.
pub fn unwrap_room_key_by_version(
    enc_ver: Option<u32>,
    wrap: &WrappedRoomKey,
    recipient_secret: &SecretKey,
    room_id: &str,
) -> Result<[u8; 32], CryptoError> {
    match effective_enc_ver(enc_ver) {
        v if v >= 2 => unwrap_room_key_v2(wrap, recipient_secret, room_id),
        1 => unwrap_room_key_v1(wrap, recipient_secret),
        other => Err(CryptoError::UnsupportedVersion(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const K1: [u8; 32] = [
        0x9f, 0x86, 0xd0, 0x81, 0x88, 0x4c, 0x7d, 0x65, 0x9a, 0x2f, 0xea, 0xa0, 0xc5, 0x5a, 0xd0,
        0x15, 0xa3, 0xbf, 0x4f, 0x1b, 0x2b, 0x0b, 0x82, 0x2c, 0xd1, 0x5d, 0x6c, 0x15, 0xb0, 0xf0,
        0x0a, 0x08,
    ];
    const ROOM: &str = "room-vector-0001";

    fn iv() -> [u8; 16] {
        [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ]
    }

    fn secret(hex_str: &str) -> SecretKey {
        SecretKey::from_slice(&hex::decode(hex_str).unwrap()).unwrap()
    }

    #[test]
    fn subkeys_match_the_canonical_values() {
        assert_eq!(
            hex::encode(derive_subkey(&K1, MSG_ENC_LABEL)),
            "1391162eeeeb69860c140af5cd201691ff07bfb4822f01c6d59e954846ecdcc9"
        );
        assert_eq!(
            hex::encode(derive_subkey(&K1, MSG_MAC_LABEL)),
            "3f49d718c2c07fca9155deeb689715f55af143a95b3b5f18acfb3a20d9594088"
        );
        assert_eq!(
            hex::encode(derive_subkey(&K1, WRAP_ENC_LABEL)),
            "78cb5ef8ddb7232c7f6d0287b9608ca38a446fb33a7819236527b24946023644"
        );
        assert_eq!(
            hex::encode(derive_subkey(&K1, WRAP_MAC_LABEL)),
            "601bc6fb7ef7c5779c154ead210251cb5c38aedbda04f5d5e75ed77c5d4ecc1b"
        );
    }

    #[test]
    fn swapping_the_hmac_argument_order_would_be_caught() {
        // The trap from §5: HmacSHA256(label, key) is (message, key).
        let mut wrong = <HmacSha256>::new_from_slice(MSG_ENC_LABEL.as_bytes()).unwrap();
        wrong.update(&K1);
        assert_ne!(
            hex::encode(wrong.finalize().into_bytes()),
            "1391162eeeeb69860c140af5cd201691ff07bfb4822f01c6d59e954846ecdcc9"
        );
    }

    #[test]
    fn message_round_trips_with_a_random_iv() {
        let enc = encrypt_message_v2("attack at dawn", &K1, ROOM).unwrap();
        let got = decrypt_message_v2(&enc.content, &enc.iv, &enc.hmac, &K1, ROOM).unwrap();
        assert_eq!(got, "attack at dawn");
    }

    #[test]
    fn message_vector_is_reproduced_bit_for_bit() {
        let enc = encrypt_message_v2_with_iv("attack at dawn", &K1, ROOM, &iv()).unwrap();
        assert_eq!(enc.content, "3nP4XMnquk7mpaDFxNxnZA==");
        assert_eq!(enc.iv, "000102030405060708090a0b0c0d0e0f");
        assert_eq!(
            enc.hmac,
            "403a7ba221a2769b6503b298de98ae0e82010a0fadb07a8aeb234aa7322bf4a7"
        );
    }

    #[test]
    fn every_message_iv_is_fresh() {
        let a = encrypt_message_v2("hello", &K1, ROOM).unwrap();
        let b = encrypt_message_v2("hello", &K1, ROOM).unwrap();
        assert_ne!(a.iv, b.iv);
        assert_ne!(a.content, b.content);
    }

    #[test]
    fn refuses_to_encrypt_blank_content() {
        assert_eq!(
            encrypt_message_v2("   \n\t ", &K1, ROOM),
            Err(CryptoError::EmptyPlaintext)
        );
    }

    fn flip_first_base64_char(s: &str) -> String {
        let mut bytes = s.as_bytes().to_vec();
        bytes[0] = if bytes[0] == b'A' { b'B' } else { b'A' };
        String::from_utf8(bytes).unwrap()
    }

    fn flip_first_hex_char(s: &str) -> String {
        let mut bytes = s.as_bytes().to_vec();
        bytes[0] = if bytes[0] == b'0' { b'1' } else { b'0' };
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn message_tampering_is_always_rejected_identically() {
        let enc = encrypt_message_v2_with_iv("attack at dawn", &K1, ROOM, &iv()).unwrap();

        let cases: Vec<(&str, String, String, String, &str)> = vec![
            (
                "ciphertext",
                flip_first_base64_char(&enc.content),
                enc.iv.clone(),
                enc.hmac.clone(),
                ROOM,
            ),
            (
                "iv",
                enc.content.clone(),
                flip_first_hex_char(&enc.iv),
                enc.hmac.clone(),
                ROOM,
            ),
            (
                "hmac",
                enc.content.clone(),
                enc.iv.clone(),
                flip_first_hex_char(&enc.hmac),
                ROOM,
            ),
            (
                "truncated hmac",
                enc.content.clone(),
                enc.iv.clone(),
                enc.hmac[..62].to_string(),
                ROOM,
            ),
            (
                "extended hmac",
                enc.content.clone(),
                enc.iv.clone(),
                format!("{}00", enc.hmac),
                ROOM,
            ),
            (
                "cross-room replay",
                enc.content.clone(),
                enc.iv.clone(),
                enc.hmac.clone(),
                "room-vector-0002",
            ),
        ];

        for (what, content, iv_hex, hmac_hex, room) in cases {
            assert_eq!(
                decrypt_message_v2(&content, &iv_hex, &hmac_hex, &K1, room),
                Err(CryptoError::DecryptionFailed),
                "tampering with {what} must fail"
            );
        }
    }

    #[test]
    fn message_rejects_the_wrong_key() {
        let enc = encrypt_message_v2_with_iv("attack at dawn", &K1, ROOM, &iv()).unwrap();
        let mut other = K1;
        other[31] ^= 1;
        assert_eq!(
            decrypt_message_v2(&enc.content, &enc.iv, &enc.hmac, &other, ROOM),
            Err(CryptoError::DecryptionFailed)
        );
    }

    #[test]
    fn ecdh_shared_x_matches_the_documented_checkpoint() {
        let recipient = secret("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
        let ephemeral = secret("2222222222222222222222222222222222222222222222222222222222222222");
        let x = ecdh_shared_x(&ephemeral, &recipient.public_key());
        assert_eq!(
            hex::encode(x),
            "862f2e40830f671dbe6c39599174d13c127fe11ee95738764a9a3f22d99dcc14"
        );
        // ECDH is symmetric; both parties must land on the same X.
        assert_eq!(x, ecdh_shared_x(&recipient, &ephemeral.public_key()));
    }

    #[test]
    fn wrap_vector_is_reproduced_bit_for_bit() {
        let recipient = secret("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
        let ephemeral = secret("2222222222222222222222222222222222222222222222222222222222222222");
        let wrap_iv: [u8; 16] = hex::decode("505152535455565758595a5b5c5d5e5f")
            .unwrap()
            .try_into()
            .unwrap();

        let wrap = wrap_room_key_v2_with(&K1, &recipient.public_key(), ROOM, &ephemeral, &wrap_iv);
        assert_eq!(
            wrap.encrypted_symmetric_key,
            "TS/2g90wOFyPTYukrttp7IEboZROacO4J1Dbz5x4uGxpJ8VRN/2vBQVnNrbxj0ToDDTcACGiGnwmG8WEmahxpGwWu5AWIBBEyO0TD0t+woI="
        );
        assert_eq!(
            wrap.hmac,
            "02b3df9450f08183c29408e8042d4b612e85ec7cbb5dad7c2a79c347c8a66ea1"
        );
        // 64-byte hex plaintext + PKCS#7 → 80 raw bytes → 108 base64 chars.
        assert_eq!(wrap.encrypted_symmetric_key.len(), 108);
        assert_eq!(unwrap_room_key_v2(&wrap, &recipient, ROOM).unwrap(), K1);
    }

    #[test]
    fn wrap_round_trips_with_fresh_randomness() {
        let recipient = random::secret_key().unwrap();
        let key = generate_room_key().unwrap();
        let wrap = wrap_room_key_v2(&key, &recipient.public_key(), ROOM).unwrap();
        assert_eq!(unwrap_room_key_v2(&wrap, &recipient, ROOM).unwrap(), key);
    }

    /// That `generate_room_key` is wired to the CSPRNG and draws afresh on each
    /// call — not that the CSPRNG is any good, which `random::tests` owns with
    /// its sixteen-draw sweep. This layer only needs to prove the wrapper does
    /// not cache, hand back a constant, or return zeros; two distinct non-zero
    /// keys is the whole of that, without re-running the statistical loop a
    /// module down.
    #[test]
    fn room_keys_are_fresh_and_never_zero() {
        let a = generate_room_key().unwrap();
        let b = generate_room_key().unwrap();
        assert_ne!(a, [0u8; 32], "a zero room key is not a room key");
        assert_ne!(a, b, "generate_room_key handed back the same key twice");
    }

    /// Every value drawn from the CSPRNG in this module — the room key, the
    /// per-message IV, the ephemeral ECDH secret and the wrap IV — refuses
    /// rather than degrades. Encryption with a fixed IV leaks whether two
    /// messages share a prefix and a fixed ephemeral key collapses forward
    /// secrecy across wraps, so "return something and log it" is not an option
    /// for any of them.
    #[test]
    fn nothing_here_encrypts_with_degraded_randomness() {
        let recipient = random::secret_key().unwrap();
        let recipient_public = recipient.public_key();

        let _guard = crate::random::FailureGuard::new();

        assert_eq!(generate_room_key().err(), Some(CryptoError::Randomness));
        assert_eq!(
            encrypt_message_v2("hello", &K1, ROOM).err(),
            Some(CryptoError::Randomness)
        );
        assert_eq!(
            wrap_room_key_v2(&K1, &recipient_public, ROOM).err(),
            Some(CryptoError::Randomness)
        );
    }

    #[test]
    fn wrap_tampering_is_always_rejected_identically() {
        let recipient = secret("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
        let ephemeral = secret("2222222222222222222222222222222222222222222222222222222222222222");
        let wrap_iv: [u8; 16] = [0x50; 16];
        let good = wrap_room_key_v2_with(&K1, &recipient.public_key(), ROOM, &ephemeral, &wrap_iv);

        let mut cases: Vec<(&str, WrappedRoomKey, &str)> = Vec::new();

        let mut w = good.clone();
        w.encrypted_symmetric_key = flip_first_base64_char(&w.encrypted_symmetric_key);
        cases.push(("ciphertext", w, ROOM));

        let mut w = good.clone();
        w.encryption_iv = flip_first_hex_char(&w.encryption_iv);
        cases.push(("iv", w, ROOM));

        let mut w = good.clone();
        w.hmac = flip_first_hex_char(&w.hmac);
        cases.push(("hmac", w, ROOM));

        let mut w = good.clone();
        w.hmac.truncate(62);
        cases.push(("truncated hmac", w, ROOM));

        // A different but perfectly valid ephemeral key: the MAC covers it, so
        // substitution is caught rather than producing a silent garbage key.
        let mut w = good.clone();
        let other_eph = secret("3333333333333333333333333333333333333333333333333333333333333333");
        w.ephemeral_public_key = uncompressed_public_key_hex(&other_eph.public_key());
        cases.push(("ephemeral key substitution", w, ROOM));

        // Not a curve point at all — rejected at parse, before any ECDH.
        let mut w = good.clone();
        w.ephemeral_public_key = format!("04{}", "11".repeat(64));
        cases.push(("off-curve ephemeral key", w, ROOM));

        cases.push(("cross-room replay", good.clone(), "room-vector-0002"));

        for (what, wrap, room) in cases {
            assert_eq!(
                unwrap_room_key_v2(&wrap, &recipient, room),
                Err(CryptoError::DecryptionFailed),
                "tampering with {what} must fail"
            );
        }
    }

    #[test]
    fn wrap_rejects_the_wrong_recipient_key() {
        let recipient = random::secret_key().unwrap();
        let impostor = random::secret_key().unwrap();
        let wrap = wrap_room_key_v2(&K1, &recipient.public_key(), ROOM).unwrap();
        assert_eq!(
            unwrap_room_key_v2(&wrap, &impostor, ROOM),
            Err(CryptoError::DecryptionFailed)
        );
    }

    #[test]
    fn unwrap_rejects_a_plaintext_that_is_not_64_hex_chars() {
        // Hand-build a wrap whose plaintext is well-padded but not a room key.
        let recipient = secret("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
        let ephemeral = secret("2222222222222222222222222222222222222222222222222222222222222222");
        let wrap_iv = [0x50u8; 16];
        let shared_x = ecdh_shared_x(&ephemeral, &recipient.public_key());
        let enc_key = derive_subkey(&shared_x, WRAP_ENC_LABEL);
        let mac_key = derive_subkey(&shared_x, WRAP_MAC_LABEL);

        let ct = aes_cbc_encrypt(&enc_key, &wrap_iv, b"not a room key at all, but 30 by");
        let encrypted_symmetric_key = BASE64.encode(&ct);
        let ephemeral_public_key = uncompressed_public_key_hex(&ephemeral.public_key());
        let encryption_iv = hex::encode(wrap_iv);
        let hmac = mac_hex(
            &mac_key,
            &room_key_mac_input(
                ROOM,
                &ephemeral_public_key,
                &encryption_iv,
                &encrypted_symmetric_key,
            ),
        );
        let wrap = WrappedRoomKey {
            encrypted_symmetric_key,
            ephemeral_public_key,
            encryption_iv,
            hmac,
        };

        // The MAC verifies — this is an authentic message — and it is still
        // rejected, because "authentic" is not "well-formed".
        assert_eq!(
            unwrap_room_key_v2(&wrap, &recipient, ROOM),
            Err(CryptoError::DecryptionFailed)
        );
    }

    #[test]
    fn legacy_v1_message_uses_the_ascii_hex_hmac_key() {
        // §8.1's worked example.
        let iv_hex = "000102030405060708090a0b0c0d0e0f";
        let ct = "L/mApzuGAZvpOIqxuMaATg==";
        let good = "98ffe503961ec8f1ae0f412db362936c79e0cae42e01bd1ac06c171ac4e0ec95";
        let raw_key_mac = "e3109563ee713e66836137cc097fbcabf24457e8ae6c73ca1c1b44055d171675";

        assert_eq!(
            decrypt_message_v1(ct, iv_hex, good, &K1).unwrap(),
            "attack at dawn"
        );
        // The wrong (raw 32-byte) HMAC key must not be accepted.
        assert_eq!(
            decrypt_message_v1(ct, iv_hex, raw_key_mac, &K1),
            Err(CryptoError::DecryptionFailed)
        );
    }

    #[test]
    fn version_dispatch_treats_missing_enc_ver_as_v1() {
        assert_eq!(effective_enc_ver(None), 1);
        assert_eq!(effective_enc_ver(Some(2)), 2);

        let iv_hex = "000102030405060708090a0b0c0d0e0f";
        let ct = "L/mApzuGAZvpOIqxuMaATg==";
        let mac = "98ffe503961ec8f1ae0f412db362936c79e0cae42e01bd1ac06c171ac4e0ec95";
        assert_eq!(
            decrypt_message_by_version(None, ct, iv_hex, mac, &K1, ROOM).unwrap(),
            "attack at dawn"
        );

        let v2 = encrypt_message_v2_with_iv("attack at dawn", &K1, ROOM, &iv()).unwrap();
        assert_eq!(
            decrypt_message_by_version(Some(2), &v2.content, &v2.iv, &v2.hmac, &K1, ROOM).unwrap(),
            "attack at dawn"
        );
        assert_eq!(
            decrypt_message_by_version(Some(0), &v2.content, &v2.iv, &v2.hmac, &K1, ROOM),
            Err(CryptoError::UnsupportedVersion(0))
        );
    }

    #[test]
    fn public_key_hex_is_uncompressed_and_round_trips() {
        let sk = secret("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
        let encoded = uncompressed_public_key_hex(&sk.public_key());
        assert_eq!(encoded.len(), 130);
        assert!(encoded.starts_with("04"));
        assert_eq!(
            parse_uncompressed_public_key(&encoded).unwrap(),
            sk.public_key()
        );

        // Compressed input is refused even though it is a valid SEC1 encoding.
        let compressed = hex::encode(sk.public_key().to_encoded_point(true).as_bytes());
        assert_eq!(
            parse_uncompressed_public_key(&compressed),
            Err(CryptoError::InvalidPublicKey)
        );
    }
}
