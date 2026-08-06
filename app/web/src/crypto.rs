//! Client-side E2EE: a thin, opinionated layer over `pocketskynet_core`.
//!
//! Nothing here reimplements a primitive. What it *does* add is the policy the
//! primitives cannot enforce on their own:
//!
//! * **Key material never leaves this process.** The mnemonic, the wallet
//!   private key and the derived encryption private key are held in memory
//!   only. They are never persisted, never logged, never put in a request body.
//!   See [`crate::session`] for the persistence policy and why.
//! * **Wrapping fails closed.** [`wrap_room_key_for`] refuses to produce a wrap
//!   for a recipient whose published key does not verify against its binding
//!   signature. That check is the sole defence against a compromised server
//!   substituting its own key at invite time, so it aborts — it never warns and
//!   continues (CRYPTO.md §4.3).
//! * **Epochs are first class.** A member accumulates one wrap per epoch;
//!   dropping the old ones would black out history across every rotation, so
//!   [`RoomKeyBundle`] keeps them all and decryption selects by the *message's*
//!   `keyVersion` while encryption always uses the highest held.

use std::collections::BTreeMap;

use pocketskynet_core::crypto::{
    decrypt_message_by_version, encrypt_message_v2, generate_room_key, uncompressed_public_key_hex,
    unwrap_room_key_by_version, wrap_room_key_v2, WrappedRoomKey,
};
use pocketskynet_core::keys::{
    derive_encryption_keys_from_signature, derive_encryption_keys_v2,
    derive_legacy_encryption_keys, sign_key_binding, verify_key_binding, EncryptionKeypair,
};
use pocketskynet_core::{hash, CryptoError, RoomId, Wallet, WalletAddress};

use crate::api::messages::MessageBody;
use crate::api::{Message, PublicKeyEntry, RoomKey, RoomKeyWrap};

/// Who holds the wallet key for this session.
///
/// The distinction is load-bearing rather than cosmetic. A mnemonic or pasted
/// private key means *this device* can sign, synchronously, as often as it
/// likes — which is what makes local transaction signing and the legacy
/// key-healing path possible. A browser wallet (MetaMask, or a Privy embedded
/// wallet reached over EIP-1193) means the key is somewhere else and every
/// signature is an async round trip through a prompt the person must approve.
///
/// Encoding that in the type is what stops a synchronous `sign_transaction`
/// from silently becoming impossible at runtime: an external session cannot
/// offer one, and says so.
enum Signer {
    /// A key this device holds.
    Local(Wallet),
    /// An external EIP-1193 wallet. Only the address is known here; signing
    /// happens in `eip1193` before a `SessionKeys` is ever built.
    External(WalletAddress),
}

/// Everything derived from one unlocked wallet.
///
/// Deliberately **not** `Clone` and **not** `Debug`: a `Clone` invites a second
/// copy to outlive a sign-out, and a derived `Debug` is how private keys end up
/// in a console log.
pub struct SessionKeys {
    signer: Signer,
    encryption: EncryptionKeypair,
    /// The wallet's signature over the key-binding message, published to the
    /// server so other members can verify this identity before wrapping to it.
    binding_sig: String,
    /// The unsalted legacy keypair, derived on demand for the healing path
    /// only. Kept behind an `Option` because deriving it costs a signature and
    /// most sessions never need it.
    legacy: Option<EncryptionKeypair>,
}

impl SessionKeys {
    /// Derive the v2 (salted) encryption identity for an unlocked wallet.
    ///
    /// The salt comes from the login response or `GET /api/auth/encryption-salt`
    /// and is itself a secret — a public salt would let any page reconstruct
    /// the derivation message and phish the signature that *is* the private key.
    pub fn derive(wallet: Wallet, salt_hex: &str) -> Result<Self, CryptoError> {
        let encryption = derive_encryption_keys_v2(&wallet, salt_hex)?;
        let binding_sig = sign_key_binding(&wallet, encryption.public_key_hex())?;
        Ok(Self {
            signer: Signer::Local(wallet),
            encryption,
            binding_sig,
            legacy: None,
        })
    }

    /// Build a session from signatures a browser wallet already produced.
    ///
    /// The two signatures must be over exactly the messages
    /// `core::keys::build_salted_encryption_message` and
    /// `build_key_binding_message` produce — those strings are byte-identical to
    /// the reference client's, which is what makes a MetaMask identity the same
    /// one in both clients.
    ///
    /// This takes signatures rather than a signing closure because signing here
    /// is async and fallible in a way `derive` is not: the prompts happen in
    /// `actions`, and by the time this is called the hard part is done.
    pub fn from_external(
        address: WalletAddress,
        derivation_sig: &str,
        binding_sig: String,
    ) -> Result<Self, CryptoError> {
        let encryption = derive_encryption_keys_from_signature(derivation_sig)?;
        Ok(Self {
            signer: Signer::External(address),
            encryption,
            binding_sig,
            // Never populated for an external session: the legacy message is
            // public and constant, so prompting a browser wallet to sign it
            // would be teaching the person to approve exactly the phishing
            // request this app warns about. Healing is skipped instead.
            legacy: None,
        })
    }

    /// Whether this session can sign transactions on this device.
    ///
    /// The wallet and bank features ask before offering a send, so an external
    /// session gets a clear explanation instead of a failed signature.
    pub fn can_sign_locally(&self) -> bool {
        matches!(self.signer, Signer::Local(_))
    }

    pub fn address(&self) -> &WalletAddress {
        match &self.signer {
            Signer::Local(w) => w.address(),
            Signer::External(a) => a,
        }
    }

    /// The uncompressed encryption public key, 130 hex chars, no `0x`. Safe to
    /// publish — this is the half that is *meant* to be shared.
    pub fn public_key_hex(&self) -> &str {
        self.encryption.public_key_hex()
    }

    /// The wallet's signature over the binding message. Also safe to publish.
    pub fn binding_sig(&self) -> &str {
        &self.binding_sig
    }

    /// Sign an EVM transaction with the wallet key.
    ///
    /// This is deliberately the only door between the wallet's secret and the
    /// blockchain layer: the send dialog hands over an unsigned
    /// [`LegacyTransaction`] and receives raw bytes, so the secret key itself
    /// never crosses into component code where a stray clone could outlive
    /// the session.
    pub fn sign_transaction(
        &self,
        tx: &pocketskynet_core::chain::LegacyTransaction,
    ) -> Result<pocketskynet_core::chain::SignedTransaction, pocketskynet_core::chain::ChainError>
    {
        let Signer::Local(wallet) = &self.signer else {
            // An external wallet signs transactions through its own provider,
            // which is an async path this synchronous door cannot reach. The
            // caller checks `can_sign_locally` first; this is the backstop.
            return Err(pocketskynet_core::chain::ChainError::NoSigningKey);
        };
        tx.sign(wallet.secret_key())
    }

    /// Derive (once) and return the legacy unsalted keypair.
    ///
    /// **Read-only, healing path only.** The legacy derivation message is
    /// public and constant, so a key derived from it must never be published
    /// and nothing new must ever be wrapped to it. It exists solely to recover
    /// room keys wrapped before the salted derivation existed.
    fn legacy(&mut self) -> Result<&EncryptionKeypair, CryptoError> {
        let Signer::Local(wallet) = &self.signer else {
            // Healing is mnemonic-only by design (core/src/keys.rs): the legacy
            // message is public and constant, so prompting a browser wallet to
            // sign it would be teaching people to approve the exact phishing
            // request this app warns about.
            return Err(CryptoError::NoLocalKey);
        };
        if self.legacy.is_none() {
            self.legacy = Some(derive_legacy_encryption_keys(wallet)?);
        }
        Ok(self.legacy.as_ref().expect("just derived"))
    }
}

/// Every epoch key this client holds for one room.
#[derive(Default)]
pub struct RoomKeyBundle {
    epochs: BTreeMap<i64, [u8; 32]>,
    /// Epochs whose wrap was present but could not be unwrapped. Tracked so the
    /// UI can distinguish "you joined after this epoch" (absent) from "this row
    /// is corrupt" (present but broken) without retrying forever.
    failed: Vec<i64>,
}

impl RoomKeyBundle {
    pub fn is_empty(&self) -> bool {
        self.epochs.is_empty()
    }

    /// The highest epoch held. Encryption always uses this one.
    pub fn latest(&self) -> Option<(i64, &[u8; 32])> {
        self.epochs.last_key_value().map(|(v, k)| (*v, k))
    }

    /// The key for a specific epoch. Decryption always uses the *message's*
    /// epoch, never the latest — that is what keeps history readable.
    pub fn get(&self, version: i64) -> Option<&[u8; 32]> {
        self.epochs.get(&version)
    }

    pub fn failed_epochs(&self) -> &[i64] {
        &self.failed
    }

    /// A value that changes exactly when the set of held epochs changes.
    ///
    /// This is the invalidation key for anything caching *results of*
    /// decryption. It deliberately fingerprints the bundle, not the room
    /// record: after a rotation happened while this device was away, the room
    /// already names the new epoch before the key for it arrives, so a cache
    /// keyed on `current_key_version` would never notice the bundle catching
    /// up — and rows would stay sealed until something unrelated cleared it.
    /// Epochs are only ever added, so (count, newest) cannot alias.
    pub fn coverage(&self) -> (usize, Option<i64>) {
        (
            self.epochs.len(),
            self.epochs.last_key_value().map(|(v, _)| *v),
        )
    }

    pub fn insert(&mut self, version: i64, key: [u8; 32]) {
        self.epochs.insert(version, key);
        self.failed.retain(|v| *v != version);
    }
}

/// Unwrap every epoch the server handed us.
///
/// One corrupt row must not black out the whole room, so failures are collected
/// rather than propagated (CRYPTO.md §9.2). When the v2 key fails, the legacy
/// key is tried — that is the "healing" path; the caller is then expected to
/// re-wrap the recovered key to the v2 public key at the same epoch.
///
/// Returns the bundle plus the epochs that were healed, so the caller can
/// schedule those re-wraps.
pub fn unwrap_bundle(
    session: &mut SessionKeys,
    room_id: &RoomId,
    wraps: &[RoomKey],
) -> (RoomKeyBundle, Vec<i64>) {
    let mut bundle = RoomKeyBundle::default();
    let mut healed = Vec::new();

    for w in wraps {
        let wire = WrappedRoomKey {
            // Verbatim strings: the MAC covers exactly what arrived, so
            // normalising the casing here would turn a valid wrap into a MAC
            // failure (CRYPTO.md §0).
            encrypted_symmetric_key: w.encrypted_symmetric_key.clone(),
            ephemeral_public_key: w.ephemeral_public_key.clone(),
            encryption_iv: w.encryption_iv.clone(),
            hmac: w.hmac.clone(),
        };
        let enc_ver = Some(w.enc_ver.max(1) as u32);

        match unwrap_room_key_by_version(
            enc_ver,
            &wire,
            session.encryption.secret_key(),
            room_id.as_str(),
        ) {
            Ok(key) => bundle.insert(w.key_version, key),
            Err(_) => {
                // Healing: an old wrap may be addressed to the pre-salt key.
                let recovered = session.legacy().ok().and_then(|legacy| {
                    unwrap_room_key_by_version(
                        enc_ver,
                        &wire,
                        legacy.secret_key(),
                        room_id.as_str(),
                    )
                    .ok()
                });
                match recovered {
                    Some(key) => {
                        bundle.insert(w.key_version, key);
                        healed.push(w.key_version);
                    }
                    None => bundle.failed.push(w.key_version),
                }
            }
        }
    }
    (bundle, healed)
}

/// What happened when we tried to render a message body.
///
/// The three failure variants are distinct on purpose (DESIGN.md §7.3): "no key
/// for this epoch" means *you joined later*, "missing metadata" means *the row
/// is malformed*, and "decryption failed" means *the MAC did not verify*. They
/// have different causes and different remedies, and collapsing them into one
/// "🔒 encrypted" string tells the user nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decrypted {
    /// The message was never encrypted.
    Plaintext(String),
    /// Decrypted successfully.
    Text(String),
    /// No wrap held for this message's epoch.
    NoKeyForEpoch(i64),
    /// Flagged encrypted but `iv` or `hmac` is absent.
    MissingMetadata,
    /// MAC verification or unpadding failed.
    Failed,
}

impl Decrypted {
    /// The text to render, or `None` if this is a sealed placeholder.
    pub fn text(&self) -> Option<&str> {
        match self {
            Decrypted::Plaintext(s) | Decrypted::Text(s) => Some(s),
            _ => None,
        }
    }

    /// The placeholder copy for a sealed bubble.
    pub fn placeholder(&self) -> Option<String> {
        match self {
            Decrypted::NoKeyForEpoch(v) => Some(format!("🔒 Encrypted — no key for epoch {v}")),
            Decrypted::MissingMetadata => Some("🔒 Missing metadata".to_owned()),
            Decrypted::Failed => Some("🔒 Decryption failed".to_owned()),
            _ => None,
        }
    }
}

/// Render one message body, decrypting if needed.
///
/// Never returns an error: an undecryptable message is a *state*, not a
/// failure, and one bad row must not abort the batch it arrived in.
pub fn decrypt_message(bundle: &RoomKeyBundle, room_id: &RoomId, msg: &Message) -> Decrypted {
    if !msg.is_encrypted {
        return Decrypted::Plaintext(msg.content.clone());
    }
    if !msg.has_crypto_metadata() {
        return Decrypted::MissingMetadata;
    }
    let version = msg.key_version();
    let Some(key) = bundle.get(version) else {
        return Decrypted::NoKeyForEpoch(version);
    };
    let (Some(iv), Some(mac)) = (msg.iv.as_deref(), msg.hmac.as_deref()) else {
        return Decrypted::MissingMetadata;
    };

    match decrypt_message_by_version(
        Some(msg.enc_ver().max(1) as u32),
        &msg.content,
        iv,
        mac,
        key,
        room_id.as_str(),
    ) {
        Ok(text) => Decrypted::Text(text),
        Err(_) => Decrypted::Failed,
    }
}

/// Build the request body for a plaintext message.
///
/// The content is trimmed **before** hashing because the server trims it before
/// storing; hashing the untrimmed string would store a `msgHash` that does not
/// match the stored `content` (CRYPTO.md §0).
pub fn plaintext_body(content: &str) -> MessageBody {
    let trimmed = content.trim().to_owned();
    let msg_hash = hash::msg_hash_plaintext(&trimmed);
    MessageBody {
        content: trimmed,
        msg_hash,
        is_encrypted: false,
        iv: None,
        hmac: None,
        enc_ver: 1,
        key_version: 1,
        // Threading and mentions are the caller's business — the composer
        // knows which thread it is in and who the autocomplete resolved, and
        // neither is derivable from the ciphertext this module produces. See
        // `MessageBody::in_thread` / `naming`.
        parent_message_id: None,
        mentions: Vec::new(),
    }
}

/// Build the request body for an encrypted message, sealed under a specific
/// epoch.
///
/// `msgHash` is over the **ciphertext**, never the plaintext: the hash is
/// stored server-side and may be published on-chain, and a hash over the
/// plaintext would let anyone confirm a guessed short message by dictionary
/// attack — defeating the encryption for exactly the messages that matter most.
pub fn encrypted_body(
    room_key: &[u8; 32],
    key_version: i64,
    room_id: &RoomId,
    plaintext: &str,
) -> Result<MessageBody, CryptoError> {
    let enc = encrypt_message_v2(plaintext.trim(), room_key, room_id.as_str())?;
    let msg_hash = hash::msg_hash_encrypted(&enc.content);
    Ok(MessageBody {
        content: enc.content,
        msg_hash,
        is_encrypted: true,
        iv: Some(enc.iv),
        hmac: Some(enc.hmac),
        enc_ver: 2,
        key_version,
        parent_message_id: None,
        mentions: Vec::new(),
    })
}

/// Why a wrap could not be produced for a recipient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrapRefusal {
    pub address: WalletAddress,
    pub reason: WrapRefusalReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapRefusalReason {
    /// The server returned no entry for this address: they have never logged
    /// in, so there is no key to wrap to.
    NoPublishedKey,
    /// A key exists but carries no binding signature. Unsigned is *exactly*
    /// what a substituting server would send, so this is a refusal, not a
    /// downgrade.
    Unverifiable,
    /// The signature exists but does not recover to this address.
    BindingMismatch,
    /// Encryption itself failed (a curve or RNG error).
    WrapFailed,
}

impl WrapRefusalReason {
    pub fn message(self) -> &'static str {
        match self {
            WrapRefusalReason::NoPublishedKey => "hasn't signed in yet, so has no encryption key",
            WrapRefusalReason::Unverifiable => "has an unsigned encryption key",
            WrapRefusalReason::BindingMismatch => "has a key that doesn't match their wallet",
            WrapRefusalReason::WrapFailed => "could not be sent a key",
        }
    }
}

/// Wrap a room key to a set of recipients, verifying every binding first.
///
/// The recipient list is the **caller's** list of addresses, and each one is
/// looked up in `entries` — not the other way round. Iterating the server's
/// response instead would let a server drop a member from the result and have
/// the rotation silently exclude them, which is both a lock-out and an
/// undetected membership change.
///
/// Any refusal is returned rather than skipped: a rotation must cover every
/// member or not happen at all, so the caller has to see the refusals to decide.
pub fn wrap_room_key_for(
    room_key: &[u8; 32],
    room_id: &RoomId,
    recipients: &[WalletAddress],
    entries: &[PublicKeyEntry],
    key_version: Option<i64>,
) -> (Vec<RoomKeyWrap>, Vec<WrapRefusal>) {
    let mut wraps = Vec::with_capacity(recipients.len());
    let mut refusals = Vec::new();

    for address in recipients {
        let entry = entries.iter().find(|e| &e.wallet_address == address);
        let Some(entry) = entry else {
            refusals.push(WrapRefusal {
                address: address.clone(),
                reason: WrapRefusalReason::NoPublishedKey,
            });
            continue;
        };
        if entry
            .public_key_sig
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
        {
            refusals.push(WrapRefusal {
                address: address.clone(),
                reason: WrapRefusalReason::Unverifiable,
            });
            continue;
        }
        // Rebuild the binding message from the address we *intend to share
        // with*, never the one echoed back beside the key.
        let public = match verify_key_binding(
            address,
            Some(&entry.public_key),
            entry.public_key_sig.as_deref(),
        ) {
            Ok(p) => p,
            Err(_) => {
                refusals.push(WrapRefusal {
                    address: address.clone(),
                    reason: WrapRefusalReason::BindingMismatch,
                });
                continue;
            }
        };

        match wrap_room_key_v2(room_key, &public, room_id.as_str()) {
            Ok(w) => wraps.push(RoomKeyWrap {
                user_address: address.clone(),
                encrypted_symmetric_key: w.encrypted_symmetric_key,
                ephemeral_public_key: w.ephemeral_public_key,
                encryption_iv: w.encryption_iv,
                hmac: w.hmac,
                enc_ver: 2,
                key_version,
            }),
            Err(_) => refusals.push(WrapRefusal {
                address: address.clone(),
                reason: WrapRefusalReason::WrapFailed,
            }),
        }
    }
    (wraps, refusals)
}

/// Wrap a room key to **yourself**, which needs no binding check: you are the
/// authority on your own key, and it never left this process.
pub fn wrap_room_key_for_self(
    room_key: &[u8; 32],
    room_id: &RoomId,
    session: &SessionKeys,
    key_version: i64,
) -> Result<RoomKeyWrap, CryptoError> {
    let public = session.encryption.public_key();
    // Sanity: the hex we publish must encode the key we are wrapping to.
    debug_assert_eq!(
        uncompressed_public_key_hex(&public),
        session.public_key_hex()
    );
    let w = wrap_room_key_v2(room_key, &public, room_id.as_str())?;
    Ok(RoomKeyWrap {
        user_address: session.address().clone(),
        encrypted_symmetric_key: w.encrypted_symmetric_key,
        ephemeral_public_key: w.ephemeral_public_key,
        encryption_iv: w.encryption_iv,
        hmac: w.hmac,
        enc_ver: 2,
        key_version: Some(key_version),
    })
}

/// A fresh 32-byte room key from the browser CSPRNG.
pub fn new_room_key() -> Result<[u8; 32], CryptoError> {
    generate_room_key()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pocketskynet_core::MessageId;

    /// A browser-wallet session, built the way `actions::sign_in_with_wallet`
    /// builds one: from signatures, with no wallet key anywhere.
    fn external_session() -> SessionKeys {
        // Any 65 bytes work — the derivation hashes them, and the chance of
        // landing outside the curve order is ~2^-128.
        let derivation_sig = format!("0x{}", "7c".repeat(65));
        let binding_sig = format!("0x{}", "3d".repeat(65));
        SessionKeys::from_external(addr(9), &derivation_sig, binding_sig).unwrap()
    }

    #[test]
    fn an_external_session_has_an_identity_but_no_local_key() {
        let s = external_session();
        // It is a full E2EE identity: it can be published and wrapped to.
        assert_eq!(s.public_key_hex().len(), 130);
        assert!(s.public_key_hex().starts_with("04"));
        assert_eq!(s.address(), &addr(9));
        // But the wallet key is in the extension, not here.
        assert!(!s.can_sign_locally());
    }

    #[test]
    fn the_same_wallet_signature_always_derives_the_same_identity() {
        // This is the interoperability guarantee: the reference client derives
        // `keccak256(signature)` too, so the same MetaMask signature must give
        // the same encryption key in both clients. If this ever stops being a
        // pure function of the signature, the two clients disagree about who
        // someone is and rooms silently stop decrypting across them.
        let a = external_session();
        let b = external_session();
        assert_eq!(a.public_key_hex(), b.public_key_hex());
    }

    #[test]
    fn an_external_session_refuses_to_sign_a_transaction_rather_than_failing_oddly() {
        use pocketskynet_core::chain::{ChainError, LegacyTransaction};
        let s = external_session();
        let tx = LegacyTransaction {
            nonce: 0,
            gas_price: 1,
            gas_limit: 21_000,
            to: Some(addr(1)),
            value: 0,
            data: vec![],
            chain_id: 25,
        };
        // A named refusal, not a signature over nothing and not a panic. The
        // send dialogs check `can_sign_locally` first; this is the backstop that
        // makes forgetting to check safe.
        assert!(matches!(
            s.sign_transaction(&tx),
            Err(ChainError::NoSigningKey)
        ));
    }

    #[test]
    fn an_external_session_declines_legacy_healing_for_the_stated_reason() {
        let mut s = external_session();
        // Not `InvalidSignature`: nothing is wrong with any signature. Healing
        // needs a signature over a public, constant, phishable message, and a
        // browser wallet must never be prompted for that.
        assert!(matches!(s.legacy(), Err(CryptoError::NoLocalKey)));
    }

    fn room() -> RoomId {
        RoomId::new("room-vector-0001").unwrap()
    }

    fn addr(n: u8) -> WalletAddress {
        WalletAddress::new(&format!("0x{:040x}", n as u32)).unwrap()
    }

    fn message(is_encrypted: bool, key_version: i64, iv: Option<&str>) -> Message {
        Message {
            id: MessageId::new("msg_1_aaaaa").unwrap(),
            room_id: room(),
            sender_address: addr(1),
            content: "body".into(),
            msg_hash: String::new(),
            message_timestamp: 0,
            msg_type: "add".into(),
            msg_serial: 1,
            is_deleted: false,
            edited_at: None,
            created_at: None,
            is_encrypted,
            iv: iv.map(str::to_owned),
            hmac: iv.map(|_| "f".repeat(64)),
            enc_ver: Some(2),
            key_version: Some(key_version),
            tx_hash: None,
            target_message_id: None,
            emoticon_code: None,
            parent_message_id: None,
            reply_count: None,
            last_reply_at: None,
            sender: None,
        }
    }

    #[test]
    fn coverage_changes_whenever_an_epoch_is_added_in_either_direction() {
        // The chat view's plaintext cache invalidates on this value, so it
        // must move for *any* addition — most importantly a backfilled older
        // epoch, where the newest version alone would not budge and rows of
        // that epoch would stay cached as sealed forever.
        let mut b = RoomKeyBundle::default();
        let empty = b.coverage();
        b.insert(3, [3u8; 32]);
        let after_first = b.coverage();
        assert_ne!(empty, after_first);
        // Backfill an OLDER epoch: newest stays 3, count is what moves.
        b.insert(1, [1u8; 32]);
        let after_backfill = b.coverage();
        assert_ne!(after_first, after_backfill);
        // A newer epoch moves it too, and re-inserting an epoch does not.
        b.insert(4, [4u8; 32]);
        let after_newer = b.coverage();
        assert_ne!(after_backfill, after_newer);
        b.insert(4, [9u8; 32]);
        assert_eq!(after_newer, b.coverage());
    }

    #[test]
    fn bundle_selects_the_highest_epoch_for_sending() {
        let mut b = RoomKeyBundle::default();
        assert!(b.latest().is_none());
        b.insert(1, [1u8; 32]);
        b.insert(3, [3u8; 32]);
        b.insert(2, [2u8; 32]);
        // BTreeMap ordering, not insertion order — 3 is the latest epoch.
        assert_eq!(b.latest().unwrap().0, 3);
        assert_eq!(b.latest().unwrap().1, &[3u8; 32]);
        assert_eq!(b.get(1), Some(&[1u8; 32]));
        assert!(b.get(4).is_none());
    }

    #[test]
    fn a_plaintext_message_is_never_routed_through_decryption() {
        let bundle = RoomKeyBundle::default();
        let m = message(false, 1, None);
        assert_eq!(
            decrypt_message(&bundle, &room(), &m),
            Decrypted::Plaintext("body".into())
        );
    }

    #[test]
    fn the_three_sealed_states_are_distinguishable() {
        let mut bundle = RoomKeyBundle::default();

        // Encrypted, no iv/hmac at all → malformed row, not a key problem.
        let m = message(true, 1, None);
        assert_eq!(
            decrypt_message(&bundle, &room(), &m),
            Decrypted::MissingMetadata
        );

        // Encrypted with metadata but we hold no wrap for epoch 7.
        let m = message(true, 7, Some(&"0".repeat(32)));
        assert_eq!(
            decrypt_message(&bundle, &room(), &m),
            Decrypted::NoKeyForEpoch(7)
        );

        // We hold the epoch but the MAC will not verify.
        bundle.insert(7, [0u8; 32]);
        assert_eq!(decrypt_message(&bundle, &room(), &m), Decrypted::Failed);

        // Each renders a different string — never collapse them.
        let placeholders = [
            Decrypted::NoKeyForEpoch(7).placeholder(),
            Decrypted::MissingMetadata.placeholder(),
            Decrypted::Failed.placeholder(),
        ];
        let unique: std::collections::HashSet<_> = placeholders.iter().collect();
        assert_eq!(unique.len(), 3);
        assert!(Decrypted::Text("hi".into()).placeholder().is_none());
    }

    #[test]
    fn plaintext_body_trims_before_hashing_so_hash_and_content_agree() {
        let b = plaintext_body("  hello  ");
        assert_eq!(b.content, "hello");
        assert_eq!(b.msg_hash, hash::msg_hash_plaintext("hello"));
        assert!(!b.is_encrypted);
        assert_eq!(b.enc_ver, 1);
        assert_eq!(b.key_version, 1);
        assert!(b.iv.is_none() && b.hmac.is_none());
    }

    #[test]
    fn encrypted_body_hashes_the_ciphertext_not_the_plaintext() {
        let key = [7u8; 32];
        let b = encrypted_body(&key, 3, &room(), "attack at dawn").unwrap();
        assert!(b.is_encrypted);
        assert_eq!(b.enc_ver, 2);
        assert_eq!(b.key_version, 3);
        assert_eq!(b.msg_hash, hash::msg_hash_encrypted(&b.content));
        // The hash of the plaintext must NOT appear anywhere.
        assert_ne!(b.msg_hash, hash::msg_hash_plaintext("attack at dawn"));
        assert_eq!(b.iv.as_ref().unwrap().len(), 32);
        assert_eq!(b.hmac.as_ref().unwrap().len(), 64);
    }

    #[test]
    fn an_encrypted_body_round_trips_through_decryption() {
        let key = [9u8; 32];
        let body = encrypted_body(&key, 2, &room(), "한글 메시지 🍓").unwrap();
        let mut bundle = RoomKeyBundle::default();
        bundle.insert(2, key);

        let mut m = message(true, 2, None);
        m.content = body.content.clone();
        m.iv = body.iv.clone();
        m.hmac = body.hmac.clone();

        assert_eq!(
            decrypt_message(&bundle, &room(), &m),
            Decrypted::Text("한글 메시지 🍓".into())
        );
    }

    #[test]
    fn decryption_is_bound_to_the_room_so_a_replayed_ciphertext_fails() {
        let key = [9u8; 32];
        let body = encrypted_body(&key, 1, &room(), "secret").unwrap();
        let mut bundle = RoomKeyBundle::default();
        bundle.insert(1, key);

        let other = RoomId::new("room-vector-0002").unwrap();
        let mut m = message(true, 1, None);
        m.content = body.content;
        m.iv = body.iv;
        m.hmac = body.hmac;

        assert_eq!(decrypt_message(&bundle, &other, &m), Decrypted::Failed);
    }

    #[test]
    fn wrapping_refuses_every_way_a_key_can_fail_to_verify() {
        let key = [1u8; 32];
        let recipients = vec![addr(1), addr(2), addr(3)];
        let entries = vec![
            // addr(1) is simply absent from the response.
            PublicKeyEntry {
                wallet_address: addr(2),
                public_key: "04".to_string() + &"ab".repeat(64),
                public_key_sig: None, // unsigned
            },
            PublicKeyEntry {
                wallet_address: addr(3),
                public_key: "04".to_string() + &"ab".repeat(64),
                public_key_sig: Some("0x".to_string() + &"11".repeat(65)), // bogus
            },
        ];

        let (wraps, refusals) = wrap_room_key_for(&key, &room(), &recipients, &entries, Some(1));
        assert!(
            wraps.is_empty(),
            "nothing may be wrapped to an unverified key"
        );
        assert_eq!(refusals.len(), 3);
        assert_eq!(refusals[0].reason, WrapRefusalReason::NoPublishedKey);
        assert_eq!(refusals[1].reason, WrapRefusalReason::Unverifiable);
        assert_eq!(refusals[2].reason, WrapRefusalReason::BindingMismatch);
    }

    #[test]
    fn wrapping_iterates_the_callers_roster_not_the_servers_response() {
        // A server that echoes an extra recipient must not get one wrapped: the
        // roster is the caller's, and entries are only ever looked *up*.
        let key = [1u8; 32];
        let entries = vec![PublicKeyEntry {
            wallet_address: addr(9),
            public_key: "04".to_string() + &"ab".repeat(64),
            public_key_sig: Some("0x".to_string() + &"11".repeat(65)),
        }];
        let (wraps, refusals) = wrap_room_key_for(&key, &room(), &[], &entries, Some(1));
        assert!(wraps.is_empty());
        assert!(refusals.is_empty());
    }

    #[test]
    fn a_verified_recipient_gets_a_wrap_that_they_can_unwrap() {
        // Full round trip: derive a real identity, publish it, wrap, unwrap.
        let wallet = Wallet::from_private_key_hex(
            "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        let salt = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let mut session = SessionKeys::derive(wallet, salt).unwrap();

        let entry = PublicKeyEntry {
            wallet_address: session.address().clone(),
            public_key: session.public_key_hex().to_owned(),
            public_key_sig: Some(session.binding_sig().to_owned()),
        };
        let room_key = [0x5au8; 32];
        let (wraps, refusals) = wrap_room_key_for(
            &room_key,
            &room(),
            std::slice::from_ref(session.address()),
            std::slice::from_ref(&entry),
            Some(4),
        );
        assert!(refusals.is_empty(), "a correctly bound key must verify");
        assert_eq!(wraps.len(), 1);
        assert_eq!(wraps[0].key_version, Some(4));
        assert_eq!(wraps[0].enc_ver, 2);

        // And the recipient — us — can read it back.
        let stored = RoomKey {
            id: 0,
            room_id: room(),
            user_address: session.address().clone(),
            encrypted_symmetric_key: wraps[0].encrypted_symmetric_key.clone(),
            ephemeral_public_key: wraps[0].ephemeral_public_key.clone(),
            encryption_iv: wraps[0].encryption_iv.clone(),
            hmac: wraps[0].hmac.clone(),
            enc_ver: 2,
            key_version: 4,
            created_at: None,
        };
        let (bundle, healed) = unwrap_bundle(&mut session, &room(), &[stored]);
        assert!(healed.is_empty());
        assert_eq!(bundle.get(4), Some(&room_key));
        assert_eq!(bundle.latest().unwrap().0, 4);
    }

    #[test]
    fn a_corrupt_wrap_is_recorded_but_does_not_lose_the_good_epochs() {
        let wallet = Wallet::from_private_key_hex(
            "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        let mut session = SessionKeys::derive(wallet, &"ab".repeat(32)).unwrap();

        let good = wrap_room_key_for_self(&[3u8; 32], &room(), &session, 2).unwrap();
        let to_row = |w: &RoomKeyWrap, version: i64| RoomKey {
            id: 0,
            room_id: room(),
            user_address: w.user_address.clone(),
            encrypted_symmetric_key: w.encrypted_symmetric_key.clone(),
            ephemeral_public_key: w.ephemeral_public_key.clone(),
            encryption_iv: w.encryption_iv.clone(),
            hmac: w.hmac.clone(),
            enc_ver: 2,
            key_version: version,
            created_at: None,
        };
        let mut broken = to_row(&good, 1);
        broken.hmac = "0".repeat(64);

        let (bundle, _) = unwrap_bundle(&mut session, &room(), &[broken, to_row(&good, 2)]);
        assert_eq!(bundle.get(2), Some(&[3u8; 32]));
        assert!(bundle.get(1).is_none());
        assert_eq!(bundle.failed_epochs(), &[1]);
        // Crucially, the room is still usable.
        assert!(!bundle.is_empty());
    }

    #[test]
    fn refusal_reasons_all_have_user_facing_copy() {
        for r in [
            WrapRefusalReason::NoPublishedKey,
            WrapRefusalReason::Unverifiable,
            WrapRefusalReason::BindingMismatch,
            WrapRefusalReason::WrapFailed,
        ] {
            assert!(!r.message().is_empty());
        }
    }
}
