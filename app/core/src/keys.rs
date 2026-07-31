//! Derivation and authentication of the **encryption** keypair.
//!
//! The E2EE keypair is deliberately *not* the wallet keypair. It is derived
//! from a wallet signature, which buys two things at once: the key is the same
//! on every device the user logs in from (no key transport, no QR-code pairing),
//! and it is identical whether they arrived via mnemonic, MetaMask, or Privy —
//! none of which will hand out the raw wallet key.
//!
//! The cost is that the derivation message is a capability. Anyone who can get
//! the user to sign it obtains their E2EE private key. That is why v2 mixes in a
//! per-account salt that only the authenticated owner can fetch, and why the
//! unsalted v1 message is decrypt-and-heal only (see
//! [`build_legacy_encryption_message`]).
//!
//! The second half of this module is the anti-MITM piece: a published
//! encryption public key means nothing on its own, because the server hands it
//! out. [`verify_key_binding`] is what turns "the server says this is Alice's
//! key" into "Alice's wallet signed for this key".

use k256::{PublicKey, SecretKey};
use sha3::{Digest, Keccak256};

use crate::crypto::{uncompressed_public_key_hex, CryptoError};
use crate::eip191;
use crate::ids::WalletAddress;
use crate::wallet::Wallet;

/// A derived end-to-end encryption keypair.
///
/// `public_hex` is cached because it is spliced verbatim into the key-binding
/// message: re-encoding it at each use would risk a casing or prefix difference
/// that invalidates the signature over it.
pub struct EncryptionKeypair {
    secret: SecretKey,
    public_hex: String,
}

impl core::fmt::Debug for EncryptionKeypair {
    /// Public half only — see [`crate::wallet::Wallet`]'s `Debug` for the
    /// reasoning.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EncryptionKeypair")
            .field("public_hex", &self.public_hex)
            .finish_non_exhaustive()
    }
}

impl EncryptionKeypair {
    /// The secret used for ECDH when wrapping and unwrapping room keys.
    pub fn secret_key(&self) -> &SecretKey {
        &self.secret
    }

    /// The public key as a parsed point.
    pub fn public_key(&self) -> PublicKey {
        self.secret.public_key()
    }

    /// The 130-character uncompressed hex published to the server as
    /// `publicKey`.
    pub fn public_key_hex(&self) -> &str {
        &self.public_hex
    }

    /// The private key as `0x`-prefixed hex, matching the reference client's
    /// storage format.
    pub fn private_key_hex(&self) -> String {
        format!("0x{}", hex::encode(self.secret.to_bytes()))
    }
}

// ---------------------------------------------------------------------------
// Derivation messages
// ---------------------------------------------------------------------------

/// Build the **v2 (salted)** encryption-key derivation message.
///
/// The salt is a per-account secret served only to the authenticated owner
/// (`encryptionSalt` on login, or `GET /api/auth/encryption-salt`). Including it
/// means a hostile dapp cannot reconstruct the message to phish a signature —
/// it does not know the salt.
///
/// `salt_hex` is spliced **verbatim**, never case-normalised: the message is
/// what gets signed, and the signature is what gets hashed into the private
/// key, so changing one character of casing produces a different identity.
pub fn build_salted_encryption_message(
    wallet_address: &WalletAddress,
    salt_hex: &str,
) -> Result<String, CryptoError> {
    if salt_hex.len() != 64 || !salt_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(CryptoError::InvalidSalt);
    }
    Ok(format!(
        "FruitNation Encryption Key Derivation v2\n\nAddress: {wallet_address}\nSalt: {salt_hex}\nPurpose: End-to-end encryption only"
    ))
}

/// Build the **v1 (unsalted)** derivation message. **LEGACY — read-only.**
///
/// This string is public and constant, so any dapp can ask a user to sign it
/// and thereby obtain their E2EE private key. Never derive a *new* key from it
/// and never publish a public key derived from it.
///
/// Its one permitted use is healing: when unwrapping a room key with the v2 key
/// fails, re-derive this key locally, unwrap with it, and immediately re-wrap
/// the recovered room key to the v2 public key at the same `keyVersion`. That
/// path is mnemonic-only — it needs a signature without a wallet popup — so for
/// MetaMask/Privy sessions, skip healing rather than prompting the user to sign
/// a phishable message.
pub fn build_legacy_encryption_message(wallet_address: &WalletAddress) -> String {
    format!(
        "FruitNation Encryption Key Derivation\n\nAddress: {wallet_address}\nPurpose: End-to-end encryption only"
    )
}

/// Turn an EIP-191 signature into an encryption keypair.
///
/// `encPriv = keccak256(signature_bytes)`. The trap: `ethers.keccak256(sig)`
/// takes a `0x…` hex *string* and hashes the **decoded 65 bytes**. Hashing the
/// ASCII of `"0xe98d…"` produces a completely different, completely valid-looking
/// key that no other client will agree with.
///
/// A hash landing on `0` or `≥ n` has probability ≈ 2⁻¹²⁸ but is handled rather
/// than unwrapped, because "impossible" errors are exactly the ones that panic
/// in production.
pub fn derive_encryption_keys_from_signature(
    signature_hex: &str,
) -> Result<EncryptionKeypair, CryptoError> {
    let stripped = signature_hex
        .strip_prefix("0x")
        .or_else(|| signature_hex.strip_prefix("0X"))
        .ok_or(CryptoError::InvalidSignature)?;
    if stripped.len() != 130 {
        return Err(CryptoError::InvalidSignature);
    }
    let bytes = hex::decode(stripped).map_err(|_| CryptoError::InvalidSignature)?;

    let digest: [u8; 32] = Keccak256::digest(&bytes).into();
    let secret = SecretKey::from_slice(&digest).map_err(|_| CryptoError::InvalidPrivateKey)?;
    let public_hex = uncompressed_public_key_hex(&secret.public_key());
    Ok(EncryptionKeypair { secret, public_hex })
}

/// Derive the current (v2, salted) encryption keypair for a mnemonic-backed
/// wallet.
///
/// For MetaMask/Privy sessions the signature comes from the provider instead;
/// feed it to [`derive_encryption_keys_from_signature`] directly.
pub fn derive_encryption_keys_v2(
    wallet: &Wallet,
    salt_hex: &str,
) -> Result<EncryptionKeypair, CryptoError> {
    let message = build_salted_encryption_message(wallet.address(), salt_hex)?;
    derive_encryption_keys_from_signature(&wallet.personal_sign(&message)?)
}

/// Derive the **legacy** unsalted encryption keypair. **Decrypt/heal only.**
///
/// Calling this is only appropriate inside the healing path described on
/// [`build_legacy_encryption_message`]. Do not publish the resulting public key
/// and do not wrap anything new to it.
pub fn derive_legacy_encryption_keys(wallet: &Wallet) -> Result<EncryptionKeypair, CryptoError> {
    let message = build_legacy_encryption_message(wallet.address());
    derive_encryption_keys_from_signature(&wallet.personal_sign(&message)?)
}

// ---------------------------------------------------------------------------
// Public-key binding (docs/CRYPTO.md §4)
// ---------------------------------------------------------------------------

/// Build the message that binds an encryption public key to a wallet address.
///
/// `enc_pub_hex` is the 130-character uncompressed hex with **no `0x` prefix**,
/// spliced in exactly as published. Both the server (`buildKeyBindingMessage` in
/// `shared/schema.ts`) and the reference client build this string byte
/// identically; any difference here shows up as a rejected key upload.
pub fn build_key_binding_message(wallet_address: &WalletAddress, enc_pub_hex: &str) -> String {
    format!(
        "FruitNation Public Key Binding\n\nAddress: {wallet_address}\nEncryption Public Key: {enc_pub_hex}"
    )
}

/// Sign the binding message with the **wallet** key (not the encryption key).
///
/// Signing with the encryption key would be circular: the whole point is that
/// the wallet — the thing the user's identity actually is — vouches for the
/// encryption key.
pub fn sign_key_binding(wallet: &Wallet, enc_pub_hex: &str) -> Result<String, CryptoError> {
    wallet.personal_sign(&build_key_binding_message(wallet.address(), enc_pub_hex))
}

/// Verify a published encryption public key against its binding signature, and
/// return the parsed key.
///
/// **This fails closed, and it is the only defence against a malicious or
/// compromised server substituting its own encryption key at invite/rotate
/// time.** Every one of these is an abort, never a warning:
///
/// * `public_key` absent → the user has never logged in; there is nothing to
///   wrap to.
/// * `public_key_sig` absent or empty → an unsigned key is unacceptable, even
///   though the schema permits it to exist for legacy rows. "Present but
///   unsigned" is exactly what an attacker would submit.
/// * the key is not a valid uncompressed curve point → reject at parse.
/// * the signature is malformed, non-canonical, or does not recover → reject.
/// * the recovered address ≠ `wallet_address` → reject.
///
/// `wallet_address` must be the address the caller **intends to share with**,
/// not one echoed back by the server in the same response as the key — trusting
/// the server for both halves of the comparison makes the check vacuous.
///
/// Returning the [`PublicKey`] rather than a `bool` is deliberate: it makes it
/// impossible to wrap to a key without having gone through this function.
pub fn verify_key_binding(
    wallet_address: &WalletAddress,
    public_key: Option<&str>,
    public_key_sig: Option<&str>,
) -> Result<PublicKey, CryptoError> {
    let public_key = public_key
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(CryptoError::KeyBindingFailed)?;
    let signature = public_key_sig
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(CryptoError::KeyBindingFailed)?;

    let parsed = crate::crypto::parse_uncompressed_public_key(public_key)
        .map_err(|_| CryptoError::KeyBindingFailed)?;

    // Rebuild the message locally from the address we intend to share with and
    // the exact key string we were handed.
    let message = build_key_binding_message(wallet_address, public_key);
    let recovered =
        eip191::recover_address(&message, signature).map_err(|_| CryptoError::KeyBindingFailed)?;

    // Both sides are `WalletAddress`, so this comparison is already
    // case-normalised by construction — no `to_lowercase()` to forget.
    if &recovered != wallet_address {
        return Err(CryptoError::KeyBindingFailed);
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WALLET_KEY: &str = "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SALT: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn wallet() -> Wallet {
        Wallet::from_private_key_hex(WALLET_KEY).unwrap()
    }

    #[test]
    fn salted_message_matches_the_worked_example() {
        let w = wallet();
        let msg = build_salted_encryption_message(w.address(), SALT).unwrap();
        assert_eq!(msg.len(), 200, "must be 200 UTF-8 bytes");
        assert_eq!(
            w.personal_sign(&msg).unwrap(),
            "0x4f4ecd00b2ae0f7de622f282c6c1b298a8f12b8d57f70d0452caed2d8f8d98b8415daeb38cbae47fc90bf46481cc0134eb621ffe557883f4d2f9cf23a4dd662c1b"
        );
    }

    #[test]
    fn salted_derivation_matches_the_worked_example() {
        let keys = derive_encryption_keys_v2(&wallet(), SALT).unwrap();
        assert_eq!(
            keys.private_key_hex(),
            "0xa6fd87b69e1c83ba6bdd5f5a502a41b707dac3993350372886b6217e8f06e6ea"
        );
        assert_eq!(
            keys.public_key_hex(),
            "045031e83ea2f138541de6908c38da03c6af49cd4f356e64799d63f9125a92a7b13094127a2ad3089544d16ed59a59e60dc869ec91f55466871df67e09bc4920e1"
        );
    }

    #[test]
    fn a_different_salt_gives_a_different_identity() {
        let a = derive_encryption_keys_v2(&wallet(), SALT).unwrap();
        let b = derive_encryption_keys_v2(&wallet(), &SALT.replace("00", "01")).unwrap();
        assert_ne!(a.public_key_hex(), b.public_key_hex());
    }

    #[test]
    fn malformed_salts_are_rejected() {
        let w = wallet();
        for bad in ["", "00", &"f".repeat(63), &"g".repeat(64)] {
            assert_eq!(
                build_salted_encryption_message(w.address(), bad).err(),
                Some(CryptoError::InvalidSalt),
                "should have rejected {bad:?}"
            );
        }
    }

    #[test]
    fn legacy_derivation_matches_both_canonical_vectors() {
        for (key, priv_hex, pub_hex) in [
            (
                "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
                "0xdde7337c32273ab3ca7154efc8c49b2873d797900ec7b047533ed7291f93f7a3",
                "04f35792987bfeeb9076b62b7e60c50fc81a87859b86388b10c9b651c5862a6cab08c4f425ad1ade24a688ce8666b0e5f2bea841e3388d64cad39a2f41846e7e9e",
            ),
            (
                WALLET_KEY,
                "0x53a19ea4568d269bc534f3ae57521fd2c43fa6a515f9c93389749749a8a050d3",
                "04bfd164ac84846bc26874a3ec72187690790d156663fa69e5abad4ce1e33a53d790f954f4d93184cf36db975cb738907f64996dd18b269bf1393e0705dc956ce5",
            ),
        ] {
            let w = Wallet::from_private_key_hex(key).unwrap();
            let keys = derive_legacy_encryption_keys(&w).unwrap();
            assert_eq!(keys.private_key_hex(), priv_hex);
            assert_eq!(keys.public_key_hex(), pub_hex);
        }
    }

    #[test]
    fn v1_and_v2_derivations_are_different_identities() {
        let w = wallet();
        assert_ne!(
            derive_legacy_encryption_keys(&w).unwrap().public_key_hex(),
            derive_encryption_keys_v2(&w, SALT)
                .unwrap()
                .public_key_hex()
        );
    }

    #[test]
    fn hashing_the_ascii_signature_would_produce_a_different_key() {
        // The §3.1 trap: keccak over the "0x…" text, not the bytes.
        let sig = "0x4f4ecd00b2ae0f7de622f282c6c1b298a8f12b8d57f70d0452caed2d8f8d98b8415daeb38cbae47fc90bf46481cc0134eb621ffe557883f4d2f9cf23a4dd662c1b";
        let wrong: [u8; 32] = Keccak256::digest(sig.as_bytes()).into();
        assert_ne!(
            hex::encode(wrong),
            "a6fd87b69e1c83ba6bdd5f5a502a41b707dac3993350372886b6217e8f06e6ea"
        );
    }

    #[test]
    fn signature_input_is_validated() {
        for bad in ["", "0x", "not hex", &"0".repeat(130)] {
            assert!(derive_encryption_keys_from_signature(bad).is_err());
        }
    }

    #[test]
    fn key_binding_matches_the_worked_example() {
        let w = wallet();
        let enc_pub = "045031e83ea2f138541de6908c38da03c6af49cd4f356e64799d63f9125a92a7b13094127a2ad3089544d16ed59a59e60dc869ec91f55466871df67e09bc4920e1";
        let message = build_key_binding_message(w.address(), enc_pub);
        assert_eq!(message.len(), 237, "must be 237 UTF-8 bytes");

        let sig = sign_key_binding(&w, enc_pub).unwrap();
        assert_eq!(
            sig,
            "0x119fd2e039b49088a5a6cc2222749a47da5e52b6013f53d17b87627f1fd7aed41c6f6682dc0b573ed490a5f2a0922ea14f782a0c7a1bce8807006065a86516621c"
        );
        assert_eq!(
            verify_key_binding(w.address(), Some(enc_pub), Some(&sig)).unwrap(),
            crate::crypto::parse_uncompressed_public_key(enc_pub).unwrap()
        );
    }

    #[test]
    fn key_binding_round_trips_for_a_derived_keypair() {
        let w = wallet();
        let keys = derive_encryption_keys_v2(&w, SALT).unwrap();
        let sig = sign_key_binding(&w, keys.public_key_hex()).unwrap();
        assert_eq!(
            verify_key_binding(w.address(), Some(keys.public_key_hex()), Some(&sig)).unwrap(),
            keys.public_key()
        );
    }

    #[test]
    fn key_binding_fails_closed_on_every_missing_or_bad_input() {
        let w = wallet();
        let keys = derive_encryption_keys_v2(&w, SALT).unwrap();
        let enc_pub = keys.public_key_hex().to_string();
        let sig = sign_key_binding(&w, &enc_pub).unwrap();

        let other = Wallet::from_private_key_hex(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        )
        .unwrap();

        // Missing key entirely.
        assert_eq!(
            verify_key_binding(w.address(), None, Some(&sig)).err(),
            Some(CryptoError::KeyBindingFailed)
        );
        // Present but blank.
        assert_eq!(
            verify_key_binding(w.address(), Some("   "), Some(&sig)).err(),
            Some(CryptoError::KeyBindingFailed)
        );
        // Key present, signature missing — the legacy-row case that must not be
        // treated as "good enough".
        assert_eq!(
            verify_key_binding(w.address(), Some(&enc_pub), None).err(),
            Some(CryptoError::KeyBindingFailed)
        );
        assert_eq!(
            verify_key_binding(w.address(), Some(&enc_pub), Some("")).err(),
            Some(CryptoError::KeyBindingFailed)
        );
        // Malformed signature.
        assert_eq!(
            verify_key_binding(w.address(), Some(&enc_pub), Some("0xdeadbeef")).err(),
            Some(CryptoError::KeyBindingFailed)
        );
        // Not a curve point.
        assert_eq!(
            verify_key_binding(
                w.address(),
                Some(&format!("04{}", "11".repeat(64))),
                Some(&sig)
            )
            .err(),
            Some(CryptoError::KeyBindingFailed)
        );
        // Signature valid, but for a different address: the substitution attack.
        assert_eq!(
            verify_key_binding(other.address(), Some(&enc_pub), Some(&sig)).err(),
            Some(CryptoError::KeyBindingFailed)
        );

        // A hostile server swapping in its own key: it can sign for its own
        // address, but not for the address we intend to share with.
        let attacker_keys = derive_encryption_keys_v2(&other, SALT).unwrap();
        let attacker_sig = sign_key_binding(&other, attacker_keys.public_key_hex()).unwrap();
        assert_eq!(
            verify_key_binding(
                w.address(),
                Some(attacker_keys.public_key_hex()),
                Some(&attacker_sig)
            )
            .err(),
            Some(CryptoError::KeyBindingFailed)
        );
    }

    #[test]
    fn binding_signature_covers_the_key_string_verbatim() {
        let w = wallet();
        let keys = derive_encryption_keys_v2(&w, SALT).unwrap();
        let sig = sign_key_binding(&w, keys.public_key_hex()).unwrap();

        // Uppercasing the key changes the message, so the signature no longer
        // recovers — which is exactly why the key must be spliced verbatim.
        let upper = keys.public_key_hex().to_uppercase();
        assert_eq!(
            verify_key_binding(w.address(), Some(&upper), Some(&sig)).err(),
            Some(CryptoError::KeyBindingFailed)
        );
    }
}
