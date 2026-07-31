//! The single point where the suite touches `pocketskynet-core`.
//!
//! Every core import in the whole test suite lives in this file, so a rename in
//! `core/` costs one edit rather than nine. The wrappers deliberately expose
//! wire-shaped types (hex strings, base64) rather than curve types: the tests
//! are asserting on what goes over HTTP.
//!
//! The messages that get *signed* — the derivation message, the binding
//! message, the login challenge — are rebuilt here from `docs/CRYPTO.md` rather
//! than imported from core. A test that reuses the implementation's own
//! constant for the thing it is verifying proves nothing.

use std::sync::Arc;

use pocketskynet_core::keys::EncryptionKeypair;
use pocketskynet_core::wallet::Wallet;
use pocketskynet_core::{crypto as core_crypto, eip191, hash, keys, WalletAddress};

// --- wallet ---------------------------------------------------------------

/// A wallet key pair plus its lowercase address.
///
/// `Wallet` holds a zeroizing `SecretKey` and is not `Clone`; an `Arc` lets a
/// test log the same identity in twice without re-deriving it.
#[derive(Clone)]
pub struct Signer {
    inner: Arc<Wallet>,
    address: String,
}

impl Signer {
    /// A fresh random wallet. Generating identities beats a fixture list, which
    /// would eventually collide between parallel tests.
    pub fn random() -> Self {
        Self::wrap(Wallet::random().expect("generate a wallet"))
    }

    /// Deterministic wallet from a 32-byte private key, for spec vectors.
    pub fn from_private_key(bytes: &[u8; 32]) -> Self {
        Self::wrap(Wallet::from_private_key_bytes(bytes).expect("valid secp256k1 scalar"))
    }

    fn wrap(wallet: Wallet) -> Self {
        let address = wallet.address().as_str().to_string();
        Signer {
            inner: Arc::new(wallet),
            address,
        }
    }

    /// Lowercase `0x…` — the only form the API accepts or returns.
    pub fn address(&self) -> &str {
        &self.address
    }

    pub fn wallet_address(&self) -> &WalletAddress {
        self.inner.address()
    }

    /// EIP-191 `personal_sign`: `0x` + 130 lowercase hex (CRYPTO §2.2).
    pub fn sign(&self, message: &str) -> String {
        self.inner.personal_sign(message).expect("personal_sign")
    }
}

/// Recover the lowercase signer address, or `None` for a malformed signature.
pub fn recover_signer(message: &str, signature: &str) -> Option<String> {
    eip191::recover_address(message, signature)
        .ok()
        .map(|a| a.as_str().to_string())
}

// --- messages that get signed ---------------------------------------------

/// CRYPTO §3.1 — the v2 (salted) encryption-key derivation message.
pub fn derivation_message_v2(address: &str, salt_hex: &str) -> String {
    format!(
        "FruitNation Encryption Key Derivation v2\n\nAddress: {}\nSalt: {}\nPurpose: End-to-end encryption only",
        address.to_lowercase(),
        salt_hex
    )
}

/// CRYPTO §4.1 — the public-key binding message.
pub fn binding_message(address: &str, public_key_hex: &str) -> String {
    format!(
        "FruitNation Public Key Binding\n\nAddress: {}\nEncryption Public Key: {}",
        address.to_lowercase(),
        public_key_hex
    )
}

/// API §6.2.1 — the exact login challenge the server must produce.
pub fn expected_challenge_message(address: &str, nonce_hex: &str) -> String {
    format!(
        "Welcome to FruitNation!\n\nClick to sign in and accept the FruitNation Terms of Service.\n\nThis request will not trigger a blockchain transaction or cost any gas fees.\n\nWallet address:\n{}\n\nNonce:\n{}",
        address.to_lowercase(),
        nonce_hex
    )
}

// --- E2EE identity --------------------------------------------------------

/// The E2EE key pair, which is *not* the wallet key pair (CRYPTO §3).
#[derive(Clone)]
pub struct Identity {
    keypair: Arc<EncryptionKeypair>,
    /// Uncompressed `04…`, 130 lowercase hex, no `0x` — the wire form.
    pub public_key: String,
}

impl Identity {
    /// The E2EE private key as `0x`-prefixed hex, for equality assertions.
    /// The key itself is never sent anywhere — only the public half is.
    pub fn private_key_hex(&self) -> String {
        self.keypair.private_key_hex()
    }
}

/// Derive the v2 identity: sign the salted derivation message with the wallet
/// key, then `encPriv = keccak256(sig_bytes)` (CRYPTO §3.1).
pub fn derive_encryption_identity(signer: &Signer, salt_hex: &str) -> Identity {
    // Built from the spec text above, not from core's builder, so a divergence
    // between the two would surface as a failing binding rather than pass.
    let signature = signer.sign(&derivation_message_v2(signer.address(), salt_hex));
    let keypair =
        keys::derive_encryption_keys_from_signature(&signature).expect("derive encryption keys");
    Identity {
        public_key: keypair.public_key_hex().to_string(),
        keypair: Arc::new(keypair),
    }
}

/// The `publicKeySig` a client publishes alongside its `publicKey` (§4.2).
pub fn key_binding_signature(signer: &Signer, public_key_hex: &str) -> String {
    signer.sign(&binding_message(signer.address(), public_key_hex))
}

/// The §4.3 check every client MUST run before wrapping a room key.
pub fn verify_key_binding(address: &str, public_key_hex: &str, signature: &str) -> bool {
    match recover_signer(&binding_message(address, public_key_hex), signature) {
        Some(recovered) => recovered.eq_ignore_ascii_case(address),
        None => false,
    }
}

// --- message encryption (encVer 2) ----------------------------------------

/// The wire fields of an encrypted message body.
pub struct EncryptedMessage {
    pub content: String,
    pub iv: String,
    pub hmac: String,
    pub msg_hash: String,
}

/// AES-256-CBC + encrypt-then-MAC over `FNv2|message|{roomId}|{iv}|{ct}` (§6.1).
pub fn encrypt_message(room_key_hex: &str, room_id: &str, plaintext: &str) -> EncryptedMessage {
    let sealed = core_crypto::encrypt_message_v2(plaintext, &key_bytes(room_key_hex), room_id)
        .expect("encrypt_message_v2");
    let msg_hash = sha256_hex(sealed.content.as_bytes());
    EncryptedMessage {
        content: sealed.content,
        iv: sealed.iv,
        hmac: sealed.hmac,
        msg_hash,
    }
}

/// Verify-then-decrypt (§6.2). `None` on a MAC failure — never a panic, since
/// several tests deliberately feed a key that must not work.
pub fn decrypt_message(
    room_key_hex: &str,
    room_id: &str,
    content: &str,
    iv_hex: &str,
    hmac_hex: &str,
) -> Option<String> {
    core_crypto::decrypt_message_v2(content, iv_hex, hmac_hex, &key_bytes(room_key_hex), room_id)
        .ok()
}

// --- room-key wrapping (encVer 2) -----------------------------------------

/// The wire fields of a `POST /api/rooms/:roomId/keys` body.
pub struct WrappedKey {
    pub encrypted_symmetric_key: String,
    pub ephemeral_public_key: String,
    pub encryption_iv: String,
    pub hmac: String,
}

/// 32 CSPRNG bytes as 64 lowercase hex (§7.5).
pub fn generate_room_key() -> String {
    hex::encode(core_crypto::generate_room_key().expect("generate a room key"))
}

/// ECDH to the recipient's E2EE public key, then AES-CBC + HMAC (§7.1).
pub fn wrap_room_key(
    room_key_hex: &str,
    recipient_public_key_hex: &str,
    room_id: &str,
) -> WrappedKey {
    let recipient = core_crypto::parse_uncompressed_public_key(recipient_public_key_hex)
        .expect("a 130-hex uncompressed point");
    let wrapped = core_crypto::wrap_room_key_v2(&key_bytes(room_key_hex), &recipient, room_id)
        .expect("wrap_room_key_v2");
    WrappedKey {
        encrypted_symmetric_key: wrapped.encrypted_symmetric_key,
        ephemeral_public_key: wrapped.ephemeral_public_key,
        encryption_iv: wrapped.encryption_iv,
        hmac: wrapped.hmac,
    }
}

/// §7.4 — returns the 64-hex room key, or `None` when the wrap is not for us.
pub fn unwrap_room_key(
    identity: &Identity,
    room_id: &str,
    encrypted_symmetric_key: &str,
    ephemeral_public_key: &str,
    encryption_iv: &str,
    hmac_hex: &str,
) -> Option<String> {
    let wrap = core_crypto::WrappedRoomKey {
        encrypted_symmetric_key: encrypted_symmetric_key.to_string(),
        ephemeral_public_key: ephemeral_public_key.to_string(),
        encryption_iv: encryption_iv.to_string(),
        hmac: hmac_hex.to_string(),
    };
    core_crypto::unwrap_room_key_v2(&wrap, identity.keypair.secret_key(), room_id)
        .ok()
        .map(hex::encode)
}

// --- hashing --------------------------------------------------------------

/// `msgHash` for a plaintext or ciphertext body (CRYPTO §10). Lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hash::sha256_hex(bytes)
}

/// The server-side emoticon-event hash preimage (CRYPTO §10.4), rebuilt from
/// the spec so the assertion is independent of the server's implementation.
pub fn emoticon_hash(
    message_id: &str,
    code: &str,
    action: &str,
    sender: &str,
    timestamp_ms: i64,
) -> String {
    sha256_hex(format!("{message_id}:{code}:{action}:{sender}:{timestamp_ms}").as_bytes())
}

fn key_bytes(room_key_hex: &str) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    hex::decode_to_slice(room_key_hex, &mut bytes).expect("a 64-character hex room key");
    bytes
}
