//! A genuine two-wallet end-to-end encryption run, driven entirely through the
//! HTTP API with real secp256k1 keys and real AES-CBC ciphertext.
//!
//! Nothing here trusts the server: every public key is verified against its
//! wallet binding before it is wrapped to (CRYPTO §4.3), every MAC is checked
//! before decryption (§6.2), and the epoch model is exercised by rotating and
//! then reading both old and new messages.

mod common;

use common::*;
use serde_json::{json, Value};

/// Fetch a peer's public key and verify its binding, exactly as §4.3 requires.
/// Returns `None` when the key is unpublished or unverifiable — a client must
/// abort rather than warn-and-continue.
async fn fetch_verified_public_key(api: &Api, address: &str) -> Option<String> {
    let resp = api
        .post("/api/users/public-keys", json!({ "addresses": [address] }))
        .await;
    resp.expect_status(200);
    let entry = resp.array().into_iter().next()?;

    let public_key = entry.get("publicKey")?.as_str()?.to_string();
    let signature = entry.get("publicKeySig")?.as_str()?.to_string();
    // Rebuild the binding from the address we intend to share with, never one
    // echoed back by the server.
    crypto::verify_key_binding(address, &public_key, &signature).then_some(public_key)
}

/// Store a wrap of `room_key` for `recipient`, at `key_version`.
async fn put_wrapped_key(
    api: &Api,
    room: &str,
    recipient: &str,
    recipient_public_key: &str,
    room_key: &str,
    key_version: i64,
) {
    let wrapped = crypto::wrap_room_key(room_key, recipient_public_key, room);
    api.post(
        &format!("/api/rooms/{room}/keys"),
        json!({
            "userAddress": recipient,
            "encryptedSymmetricKey": wrapped.encrypted_symmetric_key,
            "ephemeralPublicKey": wrapped.ephemeral_public_key,
            "encryptionIV": wrapped.encryption_iv,
            "hmac": wrapped.hmac,
            "encVer": 2,
            "keyVersion": key_version,
        }),
    )
    .await
    .expect_status(200);
}

/// Unwrap every epoch the caller holds, as `keyVersion -> roomKeyHex`.
async fn unwrap_all_epochs(
    api: &Api,
    identity: &Identity,
    room: &str,
) -> std::collections::BTreeMap<i64, String> {
    let versions = api.get(&format!("/api/rooms/{room}/keys/versions")).await;
    versions.expect_status(200);

    let mut keys = std::collections::BTreeMap::new();
    for wrap in versions.array() {
        let unwrapped = crypto::unwrap_room_key(
            identity,
            room,
            &s(&wrap, "encryptedSymmetricKey"),
            &s(&wrap, "ephemeralPublicKey"),
            &s(&wrap, "encryptionIV"),
            &s(&wrap, "hmac"),
        );
        // §9.2: a single corrupt row must not black out all of history.
        if let Some(key) = unwrapped {
            keys.insert(i(&wrap, "keyVersion"), key);
        }
    }
    keys
}

async fn send_encrypted(
    api: &Api,
    room: &str,
    room_key: &str,
    key_version: i64,
    plaintext: &str,
) -> Value {
    let sealed = crypto::encrypt_message(room_key, room, plaintext);
    api.post(
        &format!("/api/rooms/{room}/messages"),
        json!({
            "content": sealed.content,
            "msgHash": sealed.msg_hash,
            "isEncrypted": true,
            "iv": sealed.iv,
            "hmac": sealed.hmac,
            "encVer": 2,
            "keyVersion": key_version,
        }),
    )
    .await
    .expect_ok()
}

/// Decrypt a synced row with the epoch key it names (§9.2).
fn read(row: &Value, room: &str, keys: &std::collections::BTreeMap<i64, String>) -> Option<String> {
    let key = keys.get(&i(row, "keyVersion"))?;
    crypto::decrypt_message(
        key,
        room,
        &s(row, "content"),
        &s(row, "iv"),
        &s(row, "hmac"),
    )
}

// --- the full run ---------------------------------------------------------

#[tokio::test]
async fn two_wallets_exchange_encrypted_messages_across_a_rotation() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let carol = new_user(&server, "carol").await;

    // Every participant derives their E2EE identity and publishes the binding.
    let alice_id = alice.publish_encryption_key().await;
    let bob_id = bob.publish_encryption_key().await;
    let carol_id = carol.publish_encryption_key().await;

    // --- alice creates an encrypted room ---------------------------------
    let room = create_room(&alice.api, "secrets").await;
    let epoch1 = crypto::generate_room_key();
    let alice_pub = fetch_verified_public_key(&alice.api, &alice.address)
        .await
        .expect("alice's own binding verifies");
    put_wrapped_key(&alice.api, &room, &alice.address, &alice_pub, &epoch1, 1).await;

    let detail = alice
        .api
        .get(&format!("/api/rooms/{room}"))
        .await
        .expect_ok();
    assert!(b(&detail, "hasEncryption"));

    // --- alice invites bob and carol, wrapping to their verified keys -----
    for (peer, identity) in [(&bob, &bob_id), (&carol, &carol_id)] {
        alice
            .api
            .post(
                &format!("/api/rooms/{room}/invite"),
                json!({ "userAddress": peer.address }),
            )
            .await
            .expect_status(200);

        let verified = fetch_verified_public_key(&alice.api, &peer.address)
            .await
            .expect("the peer's binding must verify before wrapping");
        assert_eq!(
            verified, identity.public_key,
            "the server must not be able to substitute a key"
        );
        put_wrapped_key(&alice.api, &room, &peer.address, &verified, &epoch1, 1).await;

        peer.api
            .post_empty(&format!("/api/invitations/{room}/accept"))
            .await
            .expect_status(200);
    }

    // --- both sides recover the same room key ----------------------------
    let bob_keys = unwrap_all_epochs(&bob.api, &bob_id, &room).await;
    assert_eq!(
        bob_keys.get(&1).map(String::as_str),
        Some(epoch1.as_str()),
        "bob must unwrap exactly the key alice generated"
    );

    // --- messages in both directions -------------------------------------
    let from_alice =
        send_encrypted(&alice.api, &room, &epoch1, 1, "hello bob, this is private").await;
    assert!(b(&from_alice, "isEncrypted"));
    assert_ne!(
        s(&from_alice, "content"),
        "hello bob, this is private",
        "the server must never see plaintext"
    );
    assert_eq!(
        s(&from_alice, "msgHash"),
        crypto::sha256_hex(s(&from_alice, "content").as_bytes()),
        "CRYPTO §10.1: msgHash covers the ciphertext, never the plaintext"
    );

    send_encrypted(&bob.api, &room, &epoch1, 1, "hello alice, received").await;

    let alice_keys = unwrap_all_epochs(&alice.api, &alice_id, &room).await;
    let decrypted: Vec<String> = drain_sync(&alice.api, &room)
        .await
        .iter()
        .filter(|row| b(row, "isEncrypted"))
        .filter_map(|row| read(row, &room, &alice_keys))
        .collect();
    assert_eq!(
        decrypted,
        vec!["hello bob, this is private", "hello alice, received"],
        "alice must read both her own and bob's messages"
    );

    // --- carol is removed, which fails the room closed --------------------
    alice
        .api
        .post(
            &format!("/api/rooms/{room}/kick"),
            json!({ "userAddress": carol.address }),
        )
        .await
        .expect_status(200);

    let blocked = crypto::encrypt_message(&epoch1, &room, "should never be written");
    alice
        .api
        .post(
            &format!("/api/rooms/{room}/messages"),
            json!({
                "content": blocked.content,
                "msgHash": blocked.msg_hash,
                "isEncrypted": true,
                "iv": blocked.iv,
                "hmac": blocked.hmac,
                "encVer": 2,
                "keyVersion": 1,
            }),
        )
        .await
        .expect_conflict_code("KEY_ROTATION_REQUIRED");

    // --- alice rotates to epoch 2, covering exactly the current roster ----
    let epoch2 = crypto::generate_room_key();
    assert_ne!(epoch1, epoch2);
    let roster = alice
        .api
        .get(&format!("/api/rooms/{room}/members"))
        .await
        .array();
    let mut wraps = Vec::new();
    for member in &roster {
        let address = s(member, "userAddress");
        let verified = fetch_verified_public_key(&alice.api, &address)
            .await
            .expect("every member's binding is re-verified on every rotation");
        let wrapped = crypto::wrap_room_key(&epoch2, &verified, &room);
        wraps.push(json!({
            "userAddress": address,
            "encryptedSymmetricKey": wrapped.encrypted_symmetric_key,
            "ephemeralPublicKey": wrapped.ephemeral_public_key,
            "encryptionIV": wrapped.encryption_iv,
            "hmac": wrapped.hmac,
            "encVer": 2,
        }));
    }
    alice
        .api
        .post(
            &format!("/api/rooms/{room}/rotate-key"),
            json!({ "newVersion": 2, "keys": wraps }),
        )
        .await
        .expect_status(200);

    let from_alice_v2 = send_encrypted(&alice.api, &room, &epoch2, 2, "post-rotation secret").await;
    assert_eq!(i(&from_alice_v2, "keyVersion"), 2);

    // --- bob keeps history and gains the new epoch ------------------------
    let bob_keys = unwrap_all_epochs(&bob.api, &bob_id, &room).await;
    assert_eq!(
        bob_keys.keys().copied().collect::<Vec<_>>(),
        vec![1, 2],
        "a member accumulates one wrap per epoch they can read"
    );
    assert_eq!(bob_keys.get(&2).map(String::as_str), Some(epoch2.as_str()));

    let bob_reads: Vec<String> = drain_sync(&bob.api, &room)
        .await
        .iter()
        .filter(|row| b(row, "isEncrypted"))
        .filter_map(|row| read(row, &room, &bob_keys))
        .collect();
    assert_eq!(
        bob_reads,
        vec![
            "hello bob, this is private",
            "hello alice, received",
            "post-rotation secret",
        ],
        "bob reads history from epoch 1 and the new message from epoch 2"
    );

    // --- carol's cached epoch-1 key cannot read the new message ----------
    assert!(
        crypto::decrypt_message(
            &epoch1,
            &room,
            &s(&from_alice_v2, "content"),
            &s(&from_alice_v2, "iv"),
            &s(&from_alice_v2, "hmac"),
        )
        .is_none(),
        "forward secrecy: the removed member's key must not open epoch 2"
    );
    // And she has no server-side path to the new wrap either.
    carol
        .api
        .get(&format!("/api/rooms/{room}/keys/versions"))
        .await
        .expect_error(403, "Access denied");
}

// --- focused properties ---------------------------------------------------

#[tokio::test]
async fn a_wrapped_room_key_round_trips_through_the_server() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let identity = alice.publish_encryption_key().await;
    let room = create_room(&alice.api, "roundtrip").await;
    let room_key = crypto::generate_room_key();

    put_wrapped_key(
        &alice.api,
        &room,
        &alice.address,
        &identity.public_key,
        &room_key,
        1,
    )
    .await;

    let keys = unwrap_all_epochs(&alice.api, &identity, &room).await;
    assert_eq!(keys.get(&1).map(String::as_str), Some(room_key.as_str()));
}

#[tokio::test]
async fn a_wrap_for_someone_else_cannot_be_opened_with_your_key() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let alice_id = alice.publish_encryption_key().await;
    let bob_id = bob.publish_encryption_key().await;
    let room = create_room(&alice.api, "targeted").await;
    let room_key = crypto::generate_room_key();

    let wrapped = crypto::wrap_room_key(&room_key, &bob_id.public_key, &room);

    assert!(
        crypto::unwrap_room_key(
            &alice_id,
            &room,
            &wrapped.encrypted_symmetric_key,
            &wrapped.ephemeral_public_key,
            &wrapped.encryption_iv,
            &wrapped.hmac,
        )
        .is_none(),
        "ECDH binds the wrap to exactly one recipient"
    );
}

#[tokio::test]
async fn a_wrap_is_bound_to_its_room_id() {
    // §7.1: roomId is inside the MAC input, so a wrap cannot be replayed into
    // another room.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let identity = alice.publish_encryption_key().await;
    let room = create_room(&alice.api, "bound").await;
    let other = create_room(&alice.api, "other").await;
    let room_key = crypto::generate_room_key();
    let wrapped = crypto::wrap_room_key(&room_key, &identity.public_key, &room);

    assert!(
        crypto::unwrap_room_key(
            &identity,
            &other,
            &wrapped.encrypted_symmetric_key,
            &wrapped.ephemeral_public_key,
            &wrapped.encryption_iv,
            &wrapped.hmac,
        )
        .is_none(),
        "the MAC covers the roomId"
    );
}

#[tokio::test]
async fn a_message_is_bound_to_its_room_id() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "bound").await;
    let other = create_room(&alice.api, "other").await;
    let room_key = crypto::generate_room_key();
    let sealed = crypto::encrypt_message(&room_key, &room, "context bound");

    assert_eq!(
        crypto::decrypt_message(&room_key, &room, &sealed.content, &sealed.iv, &sealed.hmac)
            .as_deref(),
        Some("context bound")
    );
    assert!(
        crypto::decrypt_message(&room_key, &other, &sealed.content, &sealed.iv, &sealed.hmac)
            .is_none(),
        "§6.1: the MAC input carries the roomId"
    );
}

#[tokio::test]
async fn a_tampered_ciphertext_fails_the_mac_before_any_decryption() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "integrity").await;
    let room_key = crypto::generate_room_key();
    let sealed = crypto::encrypt_message(&room_key, &room, "authentic");

    // Flip one base64 character of the ciphertext.
    let mut tampered = sealed.content.clone();
    let first = tampered.remove(0);
    tampered.insert(0, if first == 'A' { 'B' } else { 'A' });

    assert!(
        crypto::decrypt_message(&room_key, &room, &tampered, &sealed.iv, &sealed.hmac).is_none(),
        "encrypt-then-MAC: verification must fail closed"
    );
}

#[tokio::test]
async fn a_message_sealed_under_the_wrong_key_does_not_open() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "keys").await;
    let right = crypto::generate_room_key();
    let wrong = crypto::generate_room_key();
    let sealed = crypto::encrypt_message(&right, &room, "for the right key only");

    assert!(
        crypto::decrypt_message(&wrong, &room, &sealed.content, &sealed.iv, &sealed.hmac).is_none()
    );
}

#[tokio::test]
async fn an_unverifiable_public_key_is_never_wrapped_to() {
    // §4.3 steps 2–3: a missing key or a null signature means abort.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;

    assert!(
        fetch_verified_public_key(&alice.api, &bob.address)
            .await
            .is_none(),
        "bob never published a key, so there is nothing to verify"
    );
}

#[tokio::test]
async fn a_substituted_public_key_fails_verification() {
    // The attack the binding exists to stop: a hostile server hands back its
    // own encryption key for the address you asked about.
    let server = TestServer::start().await;
    let bob = new_user(&server, "bob").await;
    bob.publish_encryption_key().await;
    let attacker = Signer::random();
    let attacker_id = crypto::derive_encryption_identity(&attacker, &"ab".repeat(32));
    let attacker_sig = crypto::key_binding_signature(&attacker, &attacker_id.public_key);

    assert!(
        !crypto::verify_key_binding(&bob.address, &attacker_id.public_key, &attacker_sig),
        "a binding signed by the attacker must not verify for bob's address"
    );
}

#[tokio::test]
async fn the_encryption_identity_is_deterministic_for_a_wallet_and_salt() {
    // Multi-device access depends on RFC 6979 determinism: the same wallet and
    // salt must always derive the same E2EE key.
    let signer = Signer::random();
    let salt = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    let first = crypto::derive_encryption_identity(&signer, salt);
    let second = crypto::derive_encryption_identity(&signer, salt);

    assert_eq!(first.public_key, second.public_key);
    assert_eq!(first.private_key_hex(), second.private_key_hex());
    assert_eq!(first.public_key.len(), 130);
    assert!(first.public_key.starts_with("04"));
}

#[tokio::test]
async fn a_different_salt_derives_a_different_identity() {
    let signer = Signer::random();

    let first = crypto::derive_encryption_identity(&signer, &"11".repeat(32));
    let second = crypto::derive_encryption_identity(&signer, &"22".repeat(32));

    assert_ne!(first.public_key, second.public_key);
}

#[tokio::test]
async fn encrypted_content_is_opaque_to_every_server_read_surface() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "opaque").await;
    add_member(&alice.api, &bob, &room).await;
    let room_key = crypto::generate_room_key();
    let secret = "the-quick-brown-fox-jumps";

    send_encrypted(&alice.api, &room, &room_key, 1, secret).await;

    for path in [
        format!("/api/rooms/{room}/messages"),
        format!("/api/rooms/{room}/sync?since=0"),
        "/api/rooms".to_string(),
    ] {
        let resp = alice.api.get(&path).await;
        resp.expect_status(200);
        assert!(
            !resp.text.contains(secret),
            "plaintext leaked through {path}: {}",
            resp.text
        );
    }
}
