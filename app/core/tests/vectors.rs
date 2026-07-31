//! Conformance against the **canonical** FruitNation crypto vectors.
//!
//! The vectors are read from the FruitNation checkout at runtime — deliberately
//! not copied into this repo. A copy is a snapshot that silently goes stale;
//! reading the real file means an upstream regeneration surfaces here as a
//! failing test instead of as a client that cannot decrypt anyone's messages.
//!
//! `server/test/test-vectors.json` contains wrong expected values for some
//! entries and must never be used. `server/test/vectors/crypto-v2.json` is the
//! only canonical file.
//!
//! If the file is missing the suite **fails loudly**. A skipped crypto
//! conformance test is worse than no test at all: it reports green while
//! asserting nothing.

use std::path::PathBuf;

use pocketskynet_core::crypto::{
    decrypt_message_v2, derive_subkey, encrypt_message_v2_with_iv, uncompressed_public_key_hex,
    unwrap_room_key_v2, wrap_room_key_v2_with, WrappedRoomKey,
};
use pocketskynet_core::hash::msg_hash_encrypted;
use pocketskynet_core::k256::SecretKey;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Vectors {
    format_version: u32,
    labels: Labels,
    mac_input_layouts: MacInputLayouts,
    subkeys: Vec<SubkeyVector>,
    messages: Vec<MessageVector>,
    room_key_wraps: Vec<WrapVector>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Labels {
    message_enc: String,
    message_mac: String,
    room_key_enc: String,
    room_key_mac: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MacInputLayouts {
    message: String,
    room_key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubkeyVector {
    key_hex: String,
    label: String,
    subkey_hex: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessageVector {
    name: String,
    symmetric_key_hex: String,
    room_id: String,
    plaintext_utf8: String,
    iv_hex: String,
    ciphertext_base64: String,
    hmac_hex: String,
    msg_hash_hex: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WrapVector {
    name: String,
    room_symmetric_key_hex: String,
    room_id: String,
    recipient_private_key_hex: String,
    recipient_public_key_hex: String,
    ephemeral_private_key_hex: String,
    ephemeral_public_key_hex: String,
    iv_hex: String,
    encrypted_symmetric_key_base64: String,
    hmac_hex: String,
}

/// Locate the canonical vector file.
///
/// Defaults to the FruitNation checkout that sits alongside this repo, relative
/// to `CARGO_MANIFEST_DIR` so it works from any working directory and under any
/// CI layout. `FN_CRYPTO_VECTORS` overrides it for checkouts arranged
/// differently.
fn vectors_path() -> PathBuf {
    if let Ok(explicit) = std::env::var("FN_CRYPTO_VECTORS") {
        return PathBuf::from(explicit);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../server/test/vectors/crypto-v2.json")
}

fn load() -> Vectors {
    let path = vectors_path();
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "canonical crypto vectors not found at {} ({e}).\n\
             This test must never be skipped. Check out the FruitNation server repo \
             next to PocketSkynet, or set FN_CRYPTO_VECTORS to the path of \
             server/test/vectors/crypto-v2.json.",
            path.display()
        )
    });
    serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("{} is not a valid vector file: {e}", path.display()))
}

fn key32(hex_str: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    hex::decode_to_slice(hex_str, &mut out).expect("32-byte hex");
    out
}

fn iv16(hex_str: &str) -> [u8; 16] {
    let mut out = [0u8; 16];
    hex::decode_to_slice(hex_str, &mut out).expect("16-byte hex");
    out
}

fn secret(hex_str: &str) -> SecretKey {
    SecretKey::from_slice(&hex::decode(hex_str).expect("hex")).expect("valid scalar")
}

/// Guard against the format changing underneath us without anyone noticing.
#[test]
fn vector_file_has_the_expected_shape() {
    let v = load();
    assert_eq!(
        v.format_version, 2,
        "vector formatVersion changed; re-read docs/CRYPTO.md before touching the implementation"
    );
    assert!(!v.subkeys.is_empty());
    assert!(!v.messages.is_empty());
    assert!(!v.room_key_wraps.is_empty());
}

/// The labels are part of the protocol, so the constants must match the file
/// rather than merely being consistent with themselves.
#[test]
fn labels_match_the_compiled_in_constants() {
    let v = load();
    assert_eq!(
        v.labels.message_enc,
        pocketskynet_core::crypto::MSG_ENC_LABEL
    );
    assert_eq!(
        v.labels.message_mac,
        pocketskynet_core::crypto::MSG_MAC_LABEL
    );
    assert_eq!(
        v.labels.room_key_enc,
        pocketskynet_core::crypto::WRAP_ENC_LABEL
    );
    assert_eq!(
        v.labels.room_key_mac,
        pocketskynet_core::crypto::WRAP_MAC_LABEL
    );
}

/// The MAC layouts are documented in the file; assert them so a silent change
/// upstream (an added field, a reordering) cannot slip past.
#[test]
fn mac_input_layouts_are_the_ones_we_implement() {
    let v = load();
    assert_eq!(
        v.mac_input_layouts.message,
        "FNv2|message|{roomId}|{ivHex}|{ciphertextBase64}"
    );
    assert_eq!(
        v.mac_input_layouts.room_key,
        "FNv2|roomkey|{roomId}|{ephemeralPublicKeyHex}|{ivHex}|{ciphertextBase64}"
    );
}

#[test]
fn subkey_vectors() {
    let v = load();
    for vector in &v.subkeys {
        assert_eq!(
            hex::encode(derive_subkey(&key32(&vector.key_hex), &vector.label)),
            vector.subkey_hex,
            "subkey({}, {})",
            vector.key_hex,
            vector.label
        );
    }
}

#[test]
fn message_vectors() {
    let v = load();
    for vector in &v.messages {
        let key = key32(&vector.symmetric_key_hex);
        let iv = iv16(&vector.iv_hex);

        let encrypted =
            encrypt_message_v2_with_iv(&vector.plaintext_utf8, &key, &vector.room_id, &iv)
                .unwrap_or_else(|e| panic!("[{}] encrypt failed: {e}", vector.name));

        assert_eq!(
            encrypted.content, vector.ciphertext_base64,
            "[{}] ciphertext",
            vector.name
        );
        assert_eq!(encrypted.iv, vector.iv_hex, "[{}] iv", vector.name);
        assert_eq!(encrypted.hmac, vector.hmac_hex, "[{}] hmac", vector.name);
        assert_eq!(
            msg_hash_encrypted(&vector.ciphertext_base64),
            vector.msg_hash_hex,
            "[{}] msgHash",
            vector.name
        );

        let decrypted = decrypt_message_v2(
            &vector.ciphertext_base64,
            &vector.iv_hex,
            &vector.hmac_hex,
            &key,
            &vector.room_id,
        )
        .unwrap_or_else(|e| panic!("[{}] decrypt failed: {e}", vector.name));
        assert_eq!(
            decrypted, vector.plaintext_utf8,
            "[{}] plaintext",
            vector.name
        );
    }
}

#[test]
fn room_key_wrap_vectors() {
    let v = load();
    for vector in &v.room_key_wraps {
        let recipient = secret(&vector.recipient_private_key_hex);
        let ephemeral = secret(&vector.ephemeral_private_key_hex);

        assert_eq!(
            uncompressed_public_key_hex(&recipient.public_key()),
            vector.recipient_public_key_hex,
            "[{}] recipient public key",
            vector.name
        );
        assert_eq!(
            uncompressed_public_key_hex(&ephemeral.public_key()),
            vector.ephemeral_public_key_hex,
            "[{}] ephemeral public key",
            vector.name
        );

        let wrap = wrap_room_key_v2_with(
            &key32(&vector.room_symmetric_key_hex),
            &recipient.public_key(),
            &vector.room_id,
            &ephemeral,
            &iv16(&vector.iv_hex),
        );

        assert_eq!(
            wrap.encrypted_symmetric_key, vector.encrypted_symmetric_key_base64,
            "[{}] wrapped key",
            vector.name
        );
        assert_eq!(
            wrap.ephemeral_public_key, vector.ephemeral_public_key_hex,
            "[{}] ephemeral public key",
            vector.name
        );
        assert_eq!(wrap.encryption_iv, vector.iv_hex, "[{}] iv", vector.name);
        assert_eq!(wrap.hmac, vector.hmac_hex, "[{}] hmac", vector.name);

        // Unwrap from the values in the file, not from what we just produced,
        // so a bug shared by both directions cannot cancel itself out.
        let from_file = WrappedRoomKey {
            encrypted_symmetric_key: vector.encrypted_symmetric_key_base64.clone(),
            ephemeral_public_key: vector.ephemeral_public_key_hex.clone(),
            encryption_iv: vector.iv_hex.clone(),
            hmac: vector.hmac_hex.clone(),
        };
        let unwrapped = unwrap_room_key_v2(&from_file, &recipient, &vector.room_id)
            .unwrap_or_else(|e| panic!("[{}] unwrap failed: {e}", vector.name));
        assert_eq!(
            hex::encode(unwrapped),
            vector.room_symmetric_key_hex,
            "[{}] recovered room key",
            vector.name
        );

        // §7.3: the plaintext is the 64-char hex string, so the ciphertext is
        // 80 raw bytes → 108 base64 characters. 44 would mean the raw bytes
        // were encrypted instead and no other client could read it.
        assert_eq!(
            vector.encrypted_symmetric_key_base64.len(),
            108,
            "[{}] wrap ciphertext length",
            vector.name
        );
    }
}

/// The negative checklist from §12.3, run against the canonical values.
#[test]
fn canonical_vectors_reject_tampering() {
    use pocketskynet_core::CryptoError;

    let v = load();

    for vector in &v.messages {
        let key = key32(&vector.symmetric_key_hex);
        let flip_hex = |s: &str| {
            let mut b = s.as_bytes().to_vec();
            b[0] = if b[0] == b'0' { b'1' } else { b'0' };
            String::from_utf8(b).unwrap()
        };
        let flip_b64 = |s: &str| {
            let mut b = s.as_bytes().to_vec();
            b[0] = if b[0] == b'A' { b'B' } else { b'A' };
            String::from_utf8(b).unwrap()
        };

        let attempts: Vec<(&str, String, String, String, String)> = vec![
            (
                "ciphertext bit flip",
                flip_b64(&vector.ciphertext_base64),
                vector.iv_hex.clone(),
                vector.hmac_hex.clone(),
                vector.room_id.clone(),
            ),
            (
                "iv bit flip",
                vector.ciphertext_base64.clone(),
                flip_hex(&vector.iv_hex),
                vector.hmac_hex.clone(),
                vector.room_id.clone(),
            ),
            (
                "hmac bit flip",
                vector.ciphertext_base64.clone(),
                vector.iv_hex.clone(),
                flip_hex(&vector.hmac_hex),
                vector.room_id.clone(),
            ),
            (
                "hmac truncated",
                vector.ciphertext_base64.clone(),
                vector.iv_hex.clone(),
                vector.hmac_hex[..62].to_string(),
                vector.room_id.clone(),
            ),
            (
                "hmac extended",
                vector.ciphertext_base64.clone(),
                vector.iv_hex.clone(),
                format!("{}ab", vector.hmac_hex),
                vector.room_id.clone(),
            ),
            (
                "cross-room replay",
                vector.ciphertext_base64.clone(),
                vector.iv_hex.clone(),
                vector.hmac_hex.clone(),
                format!("{}-other", vector.room_id),
            ),
        ];

        for (what, content, iv, hmac, room) in attempts {
            assert_eq!(
                decrypt_message_v2(&content, &iv, &hmac, &key, &room),
                Err(CryptoError::DecryptionFailed),
                "[{}] {what} must be rejected",
                vector.name
            );
        }
    }

    for vector in &v.room_key_wraps {
        let recipient = secret(&vector.recipient_private_key_hex);
        let good = WrappedRoomKey {
            encrypted_symmetric_key: vector.encrypted_symmetric_key_base64.clone(),
            ephemeral_public_key: vector.ephemeral_public_key_hex.clone(),
            encryption_iv: vector.iv_hex.clone(),
            hmac: vector.hmac_hex.clone(),
        };

        let mut attempts: Vec<(&str, WrappedRoomKey, String)> = Vec::new();

        let mut w = good.clone();
        w.encrypted_symmetric_key.insert(0, 'A');
        attempts.push(("ciphertext mangled", w, vector.room_id.clone()));

        let mut w = good.clone();
        w.encryption_iv.replace_range(0..1, "f");
        attempts.push(("iv bit flip", w, vector.room_id.clone()));

        let mut w = good.clone();
        w.hmac.replace_range(0..1, "f");
        attempts.push(("hmac bit flip", w, vector.room_id.clone()));

        let mut w = good.clone();
        w.hmac.truncate(62);
        attempts.push(("hmac truncated", w, vector.room_id.clone()));

        // A different but entirely valid ephemeral public key.
        let mut w = good.clone();
        let other = secret("3333333333333333333333333333333333333333333333333333333333333333");
        w.ephemeral_public_key = uncompressed_public_key_hex(&other.public_key());
        attempts.push(("ephemeral key substituted", w, vector.room_id.clone()));

        // Not a point on the curve at all.
        let mut w = good.clone();
        w.ephemeral_public_key = format!("04{}", "11".repeat(64));
        attempts.push(("ephemeral key off curve", w, vector.room_id.clone()));

        attempts.push((
            "cross-room replay",
            good.clone(),
            format!("{}-other", vector.room_id),
        ));

        for (what, wrap, room) in attempts {
            assert_eq!(
                unwrap_room_key_v2(&wrap, &recipient, &room),
                Err(CryptoError::DecryptionFailed),
                "[{}] {what} must be rejected",
                vector.name
            );
        }

        // Wrong recipient key entirely.
        let impostor = secret("4444444444444444444444444444444444444444444444444444444444444444");
        assert_eq!(
            unwrap_room_key_v2(&good, &impostor, &vector.room_id),
            Err(CryptoError::DecryptionFailed),
            "[{}] wrong recipient key must be rejected",
            vector.name
        );
    }
}
