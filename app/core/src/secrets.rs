//! The Skynet Password store: a key/value secret box only its owner can open.
//!
//! # What this is
//!
//! An entry is a **key** and a **value** — "the router's admin page" and the
//! password for it. Both halves are sealed here, client-side, and the server
//! receives two opaque blobs and a row id. There is no server-side decryption
//! path, not even a privileged one; the material that opens the box is derived
//! from a wallet signature and never leaves the browser process.
//!
//! # Why the key is encrypted too
//!
//! The obvious shape is to store the key in plaintext so the server can sort,
//! search and de-duplicate on it. That would be a mistake, and not a subtle
//! one: the key *names the secret*. "chase.com", "the safe in the bedroom",
//! "grandma's care-home portal" — a list of those, per wallet address, is a
//! target profile even if every password stays sealed. So both fields go
//! through the same seal, the server sorts on `updated_at` instead, and the
//! client does its own filtering over decrypted text. The cost is real: no
//! server-side search, and a full list fetch to filter. It is worth it.
//!
//! # Where the key material comes from
//!
//! Nothing new. The vault key is a label-KDF subkey of the **E2EE private
//! key** the session already holds (`docs/CRYPTO.md` §3.1, §5):
//!
//! ```text
//! vaultKey = HMAC-SHA256(key = encPriv (32 raw bytes), msg = "PocketSkynet/v1/password/vault")
//! encKey   = HMAC-SHA256(key = vaultKey,               msg = "PocketSkynet/v1/password/enc")
//! macKey   = HMAC-SHA256(key = vaultKey,               msg = "PocketSkynet/v1/password/mac")
//! ```
//!
//! That is the same primitive, the same argument order and the same "the full
//! 32-byte tag is the key" rule as [`crate::crypto::derive_subkey`], which is
//! deliberate — this module introduces no cryptosystem, only three new labels.
//! `encPriv` itself is `keccak256(wallet signature over the salted derivation
//! message)`, so the vault key is:
//!
//! * **deterministic** — the same wallet unlocks the same entries on a second
//!   device, with no export/import step and nothing to sync;
//! * **not phishable by a page** — the derivation message is salted with a
//!   per-account secret the server only hands to its owner;
//! * **not a second password**. A separate vault passphrase was considered and
//!   rejected: it would be one more thing to lose, it would have to be
//!   stretched (Argon2/scrypt — neither is in this tree, and both are a real
//!   dependency on wasm), and losing it would destroy the entries while the
//!   wallet that everything else in this app depends on sat there working. The
//!   existing material is strictly stronger than a memorised string, and the
//!   threat it does *not* cover — an attacker who already holds the wallet — is
//!   one where a passphrase typed on the same compromised device buys nothing.
//!
//! # Encrypt-then-MAC, per field
//!
//! Each field is AES-256-CBC with a fresh random IV, and the MAC is computed
//! over the ciphertext, the IV, the entry id and the field name:
//!
//! ```text
//! PSv1|secret|{entryId}|{key|value}|{ivHex}|{ciphertextBase64}
//! ```
//!
//! Both bindings earn their place. Without the **field name**, a server could
//! swap an entry's key ciphertext into its value slot and the client would
//! happily display a password as a label. Without the **entry id**, it could
//! move a value from one entry to another and mislabel which site a password
//! belongs to. Neither attack reveals a plaintext — but "the server cannot
//! read it" is a weaker promise than "the server cannot rearrange it", and the
//! second one is nearly free.
//!
//! The entry id is minted by the *client* ([`new_entry_id`]) for exactly this
//! reason: a server-assigned id could not be part of the MAC input, because the
//! ciphertext has to be sealed before the row exists.
//!
//! # What this protects against, and what it does not
//!
//! **Protects against:** a curious or compromised *server*, a stolen database
//! file, a backup on someone else's disk, and any operator of the deployment.
//! None of them holds `encPriv`, and the rows are indistinguishable from noise
//! without it. Tampering — with a ciphertext, an IV, a field slot, or an entry
//! id — fails the MAC and is reported as a failure, never as a plausible
//! plaintext.
//!
//! **Does not protect against:**
//!
//! * **Anyone who can run script on this origin.** An XSS on the page holds the
//!   live session and can ask this module to decrypt everything. Nothing
//!   client-side survives that, and this module does not pretend otherwise.
//! * **Anyone holding the wallet credential.** The vault key is derived, not
//!   random, so a stolen recovery phrase retroactively opens every entry that
//!   wallet ever sealed. See [`crate::keys`] and `docs/CRYPTO.md` §9.4 — this
//!   is the same trade the room keys already make.
//! * **Traffic analysis.** The server sees how many entries you have, when each
//!   was created and last changed, and roughly how long each field is (CBC
//!   pads to a 16-byte boundary; it does not hide length beyond that). If the
//!   number and rhythm of your secrets is itself sensitive, this is visible.
//! * **Rollback.** The MAC binds a ciphertext to its entry and field, not to a
//!   version. A server that keeps an old row can serve the previous password
//!   back after an edit, and the client cannot tell. Detecting that needs a
//!   signed, monotonic transcript the server cannot forge, which is a larger
//!   design than this one.
//! * **Memory.** Decrypted values live in ordinary `String`s. `zeroize` is not
//!   a dependency of this crate, and on wasm — where the heap is a JS-visible
//!   `ArrayBuffer` and the GC moves nothing on request — guaranteed wiping is
//!   closer to theatre than to a guarantee. So the guarantee this layer *does*
//!   make is **lifetime minimization**, not zeroization, and it is the caller's
//!   to keep: a decrypted *value* is never cached, never held in a list, and
//!   never written to any storage backend. [`open_field`] returns a plaintext
//!   and the caller is expected to use it and drop it within the one action
//!   that asked for it — a reveal, a copy, an edit. The `web` client
//!   (`web/src/secrets.rs`, `components/passwords.rs`) is built to that rule:
//!   the list decrypts only the *label* of each entry, and a value is opened
//!   solely at the instant it is revealed or copied. The unavoidable residue —
//!   a value briefly in the heap while it is on screen, and the plaintext in an
//!   input field while it is being typed or edited — is exactly that, residue;
//!   everything else is minimized rather than trusted to a wipe.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::crypto::{
    aes_cbc_decrypt, aes_cbc_encrypt, decode_iv, derive_subkey, mac_hex, verify_mac, CryptoError,
};
use crate::random;

/// The label that turns the E2EE private key into this vault's root key.
pub const VAULT_LABEL: &str = "PocketSkynet/v1/password/vault";
/// The AES-256 subkey label.
pub const SECRET_ENC_LABEL: &str = "PocketSkynet/v1/password/enc";
/// The HMAC-SHA256 subkey label.
pub const SECRET_MAC_LABEL: &str = "PocketSkynet/v1/password/mac";

/// The wire version stamped on every sealed field. One scheme so far; the
/// column exists so a second one can be introduced without guessing.
pub const SECRET_ENC_VER: i64 = 1;

/// Longest plaintext accepted in either field.
///
/// A password manager is not a notes app: 4 KB is generous for a passphrase, a
/// recovery code or a connection string, and bounding it here means the server
/// can bound the ciphertext it stores without having to reason about padding.
pub const MAX_SECRET_BYTES: usize = 4096;

/// Which half of an entry a ciphertext belongs to.
///
/// An enum rather than a `&str` parameter because the string is *inside the
/// MAC*: a caller that passed `"Key"` instead of `"key"` would produce entries
/// that seal fine and never open again, with no error to point at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// What the secret is *for* — a site, an account, a door.
    Key,
    /// The secret itself.
    Value,
}

impl Field {
    /// The exact ASCII that goes into the MAC input. Case-sensitive; never
    /// normalise it, and never localise it.
    pub fn as_str(self) -> &'static str {
        match self {
            Field::Key => "key",
            Field::Value => "value",
        }
    }
}

/// One sealed field, in the shape the wire and the database both use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SealedField {
    /// Base64 (standard alphabet, padded) of the raw AES output.
    pub ciphertext: String,
    /// 32 lowercase hex characters.
    pub iv: String,
    /// 64 lowercase hex characters.
    pub hmac: String,
}

/// The root key for one account's password store, with its two working
/// subkeys.
///
/// A newtype rather than a bare `[u8; 32]` so it cannot be handed to
/// [`crate::crypto::encrypt_message_v2`] as a room key by accident — the two
/// are the same shape and mean entirely different things.
///
/// The AES and HMAC subkeys are derived **once**, in [`Self::derive`], and held
/// here rather than recomputed on every [`seal_field`] / [`open_field`] call.
/// Opening a 500-entry store means 1000 field decrypts, and re-running the two
/// label-KDF HMACs per field turned that into 3000 avoidable HMAC passes on the
/// wasm main thread — the caller memoises the *result* (see `web/src/secrets.rs`
/// and `components/passwords.rs`), and this makes the work behind one such pass
/// as small as it can be.
#[derive(Clone)]
pub struct VaultKey {
    enc_key: [u8; 32],
    mac_key: [u8; 32],
}

impl std::fmt::Debug for VaultKey {
    /// Never print the bytes. A derived `Debug` on key material is how secrets
    /// reach a console log, and this one is the whole store.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VaultKey(<redacted>)")
    }
}

impl VaultKey {
    /// Derive the vault key from the session's E2EE private key.
    ///
    /// Takes the raw 32 scalar bytes, which is what `SecretKey::to_bytes()`
    /// gives. Hashing the `0x…` hex *string* instead would produce a different
    /// key that works perfectly until somebody fixes it — see the trap in
    /// `docs/CRYPTO.md` §3.1.
    pub fn derive(encryption_private_key: &[u8; 32]) -> Self {
        let root = derive_subkey(encryption_private_key, VAULT_LABEL);
        Self {
            enc_key: derive_subkey(&root, SECRET_ENC_LABEL),
            mac_key: derive_subkey(&root, SECRET_MAC_LABEL),
        }
    }

    fn enc_key(&self) -> &[u8; 32] {
        &self.enc_key
    }

    fn mac_key(&self) -> &[u8; 32] {
        &self.mac_key
    }
}

/// The exact MAC input: `PSv1|secret|{entryId}|{field}|{ivHex}|{ctB64}`.
///
/// `|` is the only framing and nothing is length-prefixed, which is
/// unambiguous because [`is_valid_entry_id`] restricts the id to
/// `[A-Za-z0-9_-]`, the field is one of two fixed ASCII words, and hex and
/// base64 cannot contain `|`. Widening the id charset would require a length
/// prefix here.
fn secret_mac_input(entry_id: &str, field: Field, iv_hex: &str, ciphertext_b64: &str) -> String {
    format!(
        "PSv1|secret|{entry_id}|{}|{iv_hex}|{ciphertext_b64}",
        field.as_str()
    )
}

/// Seal one field of one entry.
///
/// The IV is fresh on every call, including on an edit: reusing it would leak
/// whether the new value shares a prefix with the old one, which for a password
/// that was "rotated" by appending a digit is very nearly the whole secret.
pub fn seal_field(
    vault: &VaultKey,
    entry_id: &str,
    field: Field,
    plaintext: &str,
) -> Result<SealedField, CryptoError> {
    seal_field_with_iv(vault, entry_id, field, plaintext, &random::bytes::<16>()?)
}

/// Seal with a caller-supplied IV.
///
/// **Only for tests that need a fixed ciphertext.** Production code calls
/// [`seal_field`], which draws the IV from the CSPRNG.
pub fn seal_field_with_iv(
    vault: &VaultKey,
    entry_id: &str,
    field: Field,
    plaintext: &str,
    iv: &[u8; 16],
) -> Result<SealedField, CryptoError> {
    if !is_valid_entry_id(entry_id) {
        return Err(CryptoError::InvalidEncoding);
    }
    // A field is allowed to be empty — a note with no password yet is a
    // legitimate row — but it is not allowed to be enormous, because the
    // server bounds what it will store and a refusal here is clearer than a
    // 400 after the round trip.
    if plaintext.len() > MAX_SECRET_BYTES {
        return Err(CryptoError::InvalidEncoding);
    }

    let ciphertext = aes_cbc_encrypt(vault.enc_key(), iv, plaintext.as_bytes());
    // Standard alphabet *with* padding: the `=` characters are part of the MAC
    // input, exactly as in the message scheme.
    let ciphertext = BASE64.encode(&ciphertext);
    let iv_hex = hex::encode(iv);
    let hmac = mac_hex(
        vault.mac_key(),
        &secret_mac_input(entry_id, field, &iv_hex, &ciphertext),
    );

    Ok(SealedField {
        ciphertext,
        iv: iv_hex,
        hmac,
    })
}

/// Verify and open one field.
///
/// `entry_id` must be the id the row was *fetched under* and `field` the slot
/// it was read from — that is what makes the two rearrangement attacks in the
/// module docs detectable. Every failure is
/// [`CryptoError::DecryptionFailed`]: which of "the MAC did not verify", "the
/// padding was wrong" and "the plaintext was not UTF-8" happened is exactly
/// what an attacker with a decryption oracle would like to know.
pub fn open_field(
    vault: &VaultKey,
    entry_id: &str,
    field: Field,
    sealed: &SealedField,
) -> Result<String, CryptoError> {
    let mac_input = secret_mac_input(entry_id, field, &sealed.iv, &sealed.ciphertext);

    // Authenticate first. Nothing below this line runs on unauthenticated
    // data, which is what keeps the CBC padding check from becoming an oracle.
    if !verify_mac(vault.mac_key(), &mac_input, &sealed.hmac) {
        return Err(CryptoError::DecryptionFailed);
    }

    let iv = decode_iv(&sealed.iv).ok_or(CryptoError::DecryptionFailed)?;
    let ciphertext = BASE64
        .decode(&sealed.ciphertext)
        .map_err(|_| CryptoError::DecryptionFailed)?;
    let plaintext = aes_cbc_decrypt(vault.enc_key(), &iv, &ciphertext)?;
    String::from_utf8(plaintext).map_err(|_| CryptoError::DecryptionFailed)
}

/// Whether a string could be an entry id this client minted.
///
/// The charset is the message-id charset (`[A-Za-z0-9_-]`, `docs/API.md` §3.1)
/// and for the same reasons: an id is interpolated into a URL path, a log line
/// and — here — a MAC input, and none of those wants an escaping rule.
pub fn is_valid_entry_id(id: &str) -> bool {
    (ENTRY_ID_MIN_LEN..=ENTRY_ID_MAX_LEN).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Bounds on an entry id, matching the opaque-id newtypes in [`crate::ids`].
pub const ENTRY_ID_MIN_LEN: usize = 10;
/// See [`ENTRY_ID_MIN_LEN`].
pub const ENTRY_ID_MAX_LEN: usize = 100;

/// Mint a fresh entry id: `sec_` + 32 hex characters of CSPRNG output.
///
/// Random rather than `sec_{millis}_{rand}` — the shape the rooms and messages
/// use — because those ids are assigned by a server that already knows when a
/// row was made. This one is chosen by the client and travels *with* the
/// ciphertext, so putting a timestamp in it would publish a creation time in a
/// second, unremovable place; the server records `created_at` on its own and
/// can be told to forget it, but an id is forever.
pub fn new_entry_id() -> Result<String, CryptoError> {
    Ok(format!("sec_{}", hex::encode(random::bytes::<16>()?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for a derived E2EE private key. Any 32 bytes work: the vault
    /// key is a hash of them, not a curve point.
    const ENC_PRIV: [u8; 32] = [
        0xa6, 0xfd, 0x87, 0xb6, 0x9e, 0x1c, 0x83, 0xba, 0x6b, 0xdd, 0x5f, 0x5a, 0x50, 0x2a, 0x41,
        0xb7, 0x07, 0xda, 0xc3, 0x99, 0x33, 0x50, 0x37, 0x28, 0x86, 0xb6, 0x21, 0x7e, 0x8f, 0x06,
        0xe6, 0xea,
    ];
    const ENTRY: &str = "sec_00112233445566778899aabbccddeeff";

    fn vault() -> VaultKey {
        VaultKey::derive(&ENC_PRIV)
    }

    fn other_vault() -> VaultKey {
        let mut bytes = ENC_PRIV;
        bytes[31] ^= 1;
        VaultKey::derive(&bytes)
    }

    fn iv() -> [u8; 16] {
        [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ]
    }

    #[test]
    fn a_sealed_field_round_trips() {
        for (field, text) in [
            (Field::Key, "chase.com"),
            (Field::Value, "correct horse battery staple"),
            (Field::Value, "한글 비밀번호 🔐"),
            (Field::Value, ""),
        ] {
            let sealed = seal_field(&vault(), ENTRY, field, text).unwrap();
            assert_eq!(open_field(&vault(), ENTRY, field, &sealed).unwrap(), text);
        }
    }

    #[test]
    fn the_wire_shape_is_base64_and_lowercase_hex() {
        let sealed = seal_field(&vault(), ENTRY, Field::Value, "hunter2").unwrap();
        assert_eq!(sealed.iv.len(), 32);
        assert!(sealed.iv.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(sealed.iv, sealed.iv.to_lowercase());
        assert_eq!(sealed.hmac.len(), 64);
        assert_eq!(sealed.hmac, sealed.hmac.to_lowercase());
        assert!(BASE64.decode(&sealed.ciphertext).is_ok());
    }

    #[test]
    fn nothing_in_the_sealed_form_resembles_the_plaintext() {
        // The property the whole feature rests on: what the server stores must
        // not contain, or be derivable from, what the user typed.
        let secret = "correct horse battery staple";
        let sealed = seal_field(&vault(), ENTRY, Field::Value, secret).unwrap();
        let blob = format!("{}{}{}", sealed.ciphertext, sealed.iv, sealed.hmac);
        assert!(!blob.contains(secret));
        assert!(!blob.contains("horse"));
        // And not a hex or base64 encoding of it either.
        assert!(!blob.contains(&hex::encode(secret)));
        assert!(!blob.contains(&BASE64.encode(secret)));
    }

    #[test]
    fn every_seal_draws_a_fresh_iv() {
        // Two seals of the same plaintext must not be recognisable as such —
        // otherwise the server learns which of your passwords are duplicates.
        let a = seal_field(&vault(), ENTRY, Field::Value, "hunter2").unwrap();
        let b = seal_field(&vault(), ENTRY, Field::Value, "hunter2").unwrap();
        assert_ne!(a.iv, b.iv);
        assert_ne!(a.ciphertext, b.ciphertext);
        assert_ne!(a.hmac, b.hmac);
    }

    #[test]
    fn a_different_vault_key_cannot_open_it() {
        let sealed = seal_field(&vault(), ENTRY, Field::Value, "hunter2").unwrap();
        assert_eq!(
            open_field(&other_vault(), ENTRY, Field::Value, &sealed),
            Err(CryptoError::DecryptionFailed)
        );
    }

    #[test]
    fn the_vault_key_is_not_the_encryption_private_key() {
        // If the derivation were ever "simplified" to use encPriv directly,
        // a compromise of one wrapped room key's ECDH partner would become a
        // compromise of the password store. The label is what separates them.
        let vault = vault();
        assert_ne!(vault.enc_key(), &ENC_PRIV);
        assert_ne!(vault.enc_key(), vault.mac_key());
        // The vault root is hashed twice more before use, so neither working
        // subkey is the private key that seeded it.
        assert_ne!(vault.mac_key(), &ENC_PRIV);
    }

    #[test]
    fn the_labels_are_not_the_room_key_labels() {
        // Domain separation from the messaging scheme, asserted rather than
        // assumed: the same 32 bytes must never be an AES key in both.
        use crate::crypto::{MSG_ENC_LABEL, MSG_MAC_LABEL, WRAP_ENC_LABEL, WRAP_MAC_LABEL};
        for label in [MSG_ENC_LABEL, MSG_MAC_LABEL, WRAP_ENC_LABEL, WRAP_MAC_LABEL] {
            assert_ne!(label, VAULT_LABEL);
            assert_ne!(label, SECRET_ENC_LABEL);
            assert_ne!(label, SECRET_MAC_LABEL);
        }
    }

    #[test]
    fn a_ciphertext_cannot_be_moved_between_the_two_fields() {
        // The server holds both halves of a row. Without the field name in the
        // MAC it could swap them, and the UI would print the password as the
        // label of the entry — on screen, in plain sight.
        let sealed = seal_field(&vault(), ENTRY, Field::Key, "chase.com").unwrap();
        assert_eq!(
            open_field(&vault(), ENTRY, Field::Value, &sealed),
            Err(CryptoError::DecryptionFailed)
        );
    }

    #[test]
    fn a_ciphertext_cannot_be_moved_between_entries() {
        let other = "sec_ffeeddccbbaa99887766554433221100";
        let sealed = seal_field(&vault(), ENTRY, Field::Value, "hunter2").unwrap();
        assert_eq!(
            open_field(&vault(), other, Field::Value, &sealed),
            Err(CryptoError::DecryptionFailed)
        );
    }

    #[test]
    fn every_kind_of_tampering_is_refused_identically() {
        let good = seal_field_with_iv(&vault(), ENTRY, Field::Value, "hunter2", &iv()).unwrap();

        let flip_first = |s: &str, alt: u8, fallback: u8| {
            let mut bytes = s.as_bytes().to_vec();
            bytes[0] = if bytes[0] == alt { fallback } else { alt };
            String::from_utf8(bytes).unwrap()
        };

        let mut cases: Vec<(&str, SealedField)> = Vec::new();

        let mut f = good.clone();
        f.ciphertext = flip_first(&f.ciphertext, b'A', b'B');
        cases.push(("ciphertext", f));

        let mut f = good.clone();
        f.iv = flip_first(&f.iv, b'0', b'1');
        cases.push(("iv", f));

        let mut f = good.clone();
        f.hmac = flip_first(&f.hmac, b'0', b'1');
        cases.push(("hmac", f));

        let mut f = good.clone();
        f.hmac.truncate(62);
        cases.push(("truncated hmac", f));

        let mut f = good.clone();
        f.hmac.push_str("00");
        cases.push(("extended hmac", f));

        let mut f = good.clone();
        f.ciphertext.push('=');
        cases.push(("padded ciphertext", f));

        for (what, sealed) in cases {
            assert_eq!(
                open_field(&vault(), ENTRY, Field::Value, &sealed),
                Err(CryptoError::DecryptionFailed),
                "tampering with {what} must fail"
            );
        }
    }

    #[test]
    fn sealing_refuses_an_id_it_could_not_have_minted() {
        // An id carrying a `|` would make the MAC framing ambiguous, and one
        // carrying a `/` would escape a URL path. Both are refused at the
        // sealing boundary rather than trusted to a later check.
        for bad in [
            "short",
            "sec_with|pipe|inside",
            "sec_with/slash",
            &"a".repeat(101),
        ] {
            assert_eq!(
                seal_field(&vault(), bad, Field::Value, "x"),
                Err(CryptoError::InvalidEncoding),
                "should have refused {bad:?}"
            );
        }
        assert!(seal_field(&vault(), ENTRY, Field::Value, "x").is_ok());
    }

    #[test]
    fn an_oversized_secret_is_refused_before_the_round_trip() {
        let huge = "x".repeat(MAX_SECRET_BYTES + 1);
        assert_eq!(
            seal_field(&vault(), ENTRY, Field::Value, &huge),
            Err(CryptoError::InvalidEncoding)
        );
        assert!(seal_field(&vault(), ENTRY, Field::Value, &"x".repeat(MAX_SECRET_BYTES)).is_ok());
    }

    #[test]
    fn minted_ids_are_valid_and_do_not_repeat() {
        let a = new_entry_id().unwrap();
        let b = new_entry_id().unwrap();
        assert_ne!(a, b);
        assert!(a.starts_with("sec_"));
        assert_eq!(a.len(), 4 + 32);
        assert!(is_valid_entry_id(&a));
        // No timestamp: two ids minted back to back share no prefix beyond the
        // literal tag.
        assert_ne!(a[4..8], b[4..8]);
    }

    #[test]
    fn the_field_names_are_the_exact_strings_the_mac_covers() {
        // These two strings are protocol. Renaming either — even to something
        // tidier — makes every existing entry undecryptable.
        assert_eq!(Field::Key.as_str(), "key");
        assert_eq!(Field::Value.as_str(), "value");
    }

    #[test]
    fn the_mac_input_layout_is_exactly_the_documented_one() {
        assert_eq!(
            secret_mac_input(ENTRY, Field::Value, "0011", "Y3Q="),
            "PSv1|secret|sec_00112233445566778899aabbccddeeff|value|0011|Y3Q="
        );
        assert_eq!(
            secret_mac_input(ENTRY, Field::Value, "a", "b")
                .matches('|')
                .count(),
            5
        );
    }
}
