//! Room keys and the epoch model: put/get, `keys/versions`, `rotate-key`, and
//! the two machine-readable 409s. Spec: `docs/API.md` §6.9, §6.10.1, §10.

mod common;

use common::*;
use serde_json::{json, Value};

/// A structurally valid wrap. Storage and authorization do not inspect the
/// ciphertext — `e2ee.rs` covers the genuine crypto round trip.
fn wrap(user_address: &str, key_version: i64) -> Value {
    json!({
        "userAddress": user_address,
        "encryptedSymmetricKey": format!("d3JhcHBlZC1rZXktdjE={key_version}"),
        "ephemeralPublicKey": format!("04{}", "ab".repeat(64)),
        "encryptionIV": "1a2b3c4d5e6f78901a2b3c4d5e6f7890",
        "hmac": "9f".repeat(32),
        "encVer": 2,
        "keyVersion": key_version,
    })
}

fn rotation_entry(user_address: &str) -> Value {
    json!({
        "userAddress": user_address,
        "encryptedSymmetricKey": "cm90YXRlZC1rZXk=",
        "ephemeralPublicKey": format!("04{}", "cd".repeat(64)),
        "encryptionIV": "aabbccddeeff00112233445566778899",
        "hmac": "1a".repeat(32),
        "encVer": 2,
    })
}

// --- storing --------------------------------------------------------------

#[tokio::test]
async fn a_member_can_store_their_own_wrapped_key() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "encrypted").await;

    alice
        .api
        .post(&format!("/api/rooms/{room}/keys"), wrap(&alice.address, 1))
        .await
        .expect_message("Room key stored successfully");

    let stored = alice
        .api
        .get(&format!("/api/rooms/{room}/keys"))
        .await
        .expect_ok();
    expect_room_key_shape(&stored);
    assert_eq!(s(&stored, "roomId"), room);
    assert_eq!(s(&stored, "userAddress"), alice.address);
    assert_eq!(i(&stored, "keyVersion"), 1);
    assert_eq!(i(&stored, "encVer"), 2);
}

#[tokio::test]
async fn storing_a_key_turns_on_has_encryption() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "encrypted").await;
    let before = room_in_list(&alice.api, &room).await.expect("room in list");
    assert!(!b(&before, "hasEncryption"));

    alice
        .api
        .post(&format!("/api/rooms/{room}/keys"), wrap(&alice.address, 1))
        .await
        .expect_status(200);

    let after = room_in_list(&alice.api, &room).await.expect("room in list");
    assert!(b(&after, "hasEncryption"));
}

#[tokio::test]
async fn storing_a_key_does_not_advance_the_epoch() {
    // §15 #22: establishing epoch 1 is a plain store; only /rotate-key moves
    // `currentKeyVersion`.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "encrypted").await;

    alice
        .api
        .post(&format!("/api/rooms/{room}/keys"), wrap(&alice.address, 1))
        .await
        .expect_status(200);

    let detail = alice
        .api
        .get(&format!("/api/rooms/{room}"))
        .await
        .expect_ok();
    assert_eq!(i(&detail, "currentKeyVersion"), 1);
}

#[tokio::test]
async fn a_member_may_overwrite_their_own_wrap_for_an_epoch() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "encrypted").await;
    alice
        .api
        .post(&format!("/api/rooms/{room}/keys"), wrap(&alice.address, 1))
        .await
        .expect_status(200);

    let mut replacement = wrap(&alice.address, 1);
    replacement["encryptedSymmetricKey"] = json!("cmVwbGFjZWQtd3JhcA==");
    alice
        .api
        .post(&format!("/api/rooms/{room}/keys"), replacement)
        .await
        .expect_status(200);

    let versions = alice
        .api
        .get(&format!("/api/rooms/{room}/keys/versions"))
        .await
        .array();
    assert_eq!(versions.len(), 1, "the epoch row is replaced, not appended");
    assert_eq!(
        s(&versions[0], "encryptedSymmetricKey"),
        "cmVwbGFjZWQtd3JhcA=="
    );
}

#[tokio::test]
async fn an_admin_can_pre_wrap_a_key_for_an_invitee() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "encrypted").await;
    alice
        .api
        .post(
            &format!("/api/rooms/{room}/invite"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_status(200);

    alice
        .api
        .post(&format!("/api/rooms/{room}/keys"), wrap(&bob.address, 1))
        .await
        .expect_status(200);
}

#[tokio::test]
async fn an_admin_cannot_clobber_a_members_existing_wrap() {
    // §6.9.1 check 4: overwriting a valid wrap would lock the member out.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "encrypted").await;
    add_member(&alice.api, &bob, &room).await;
    bob.api
        .post(&format!("/api/rooms/{room}/keys"), wrap(&bob.address, 1))
        .await
        .expect_status(200);

    alice
        .api
        .post(&format!("/api/rooms/{room}/keys"), wrap(&bob.address, 1))
        .await
        .expect_error(
            409,
            "That member already has a key for this epoch; use /rotate-key to re-key the room.",
        );
}

#[tokio::test]
async fn a_plain_member_cannot_store_a_key_for_someone_else() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let carol = new_user(&server, "carol").await;
    let room = create_room(&alice.api, "encrypted").await;
    add_member(&alice.api, &bob, &room).await;
    add_member(&alice.api, &carol, &room).await;

    bob.api
        .post(&format!("/api/rooms/{room}/keys"), wrap(&carol.address, 1))
        .await
        .expect_error(403, "Only admins can store keys for other users");
}

#[tokio::test]
async fn a_key_cannot_be_stored_for_a_non_member_and_non_invitee() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let outsider = new_user(&server, "outsider").await;
    let room = create_room(&alice.api, "encrypted").await;

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/keys"),
            wrap(&outsider.address, 1),
        )
        .await
        .expect_error(400, "User must be a room member or invitee");
}

#[tokio::test]
async fn storing_a_key_in_a_room_you_do_not_belong_to_is_a_uniform_403() {
    // Was a 404 "Room not found". The caller's standing in the room is now
    // settled before the room is looked up, so a non-member — which is what
    // you are for a room that does not exist — gets the same 403 whether the
    // room is missing, somebody else's, or the derivable id of somebody's My
    // Note. That uniformity closes the enumeration oracle a note id would
    // otherwise be (see `static_rooms::probing_a_strangers_note_id_reveals_nothing`).
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    alice
        .api
        .post(
            "/api/rooms/room_0000000000_deadbeef-dead-beef-dead-beefdeadbeef/keys",
            wrap(&alice.address, 1),
        )
        .await
        .expect_error(403, "Access denied");
}

#[tokio::test]
async fn room_key_fields_are_validated() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "encrypted").await;

    let mut bad_iv = wrap(&alice.address, 1);
    bad_iv["encryptionIV"] = json!("too short");
    let mut bad_hmac = wrap(&alice.address, 1);
    bad_hmac["hmac"] = json!("nothex");
    let mut bad_eph = wrap(&alice.address, 1);
    bad_eph["ephemeralPublicKey"] = json!("not-hex-at-all!");
    let mut bad_ver = wrap(&alice.address, 1);
    bad_ver["encVer"] = json!(7);
    let mut empty_key = wrap(&alice.address, 1);
    empty_key["encryptedSymmetricKey"] = json!("");

    for body in [bad_iv, bad_hmac, bad_eph, bad_ver, empty_key] {
        alice
            .api
            .post(&format!("/api/rooms/{room}/keys"), body)
            .await
            .expect_validation_failed();
    }
}

#[tokio::test]
async fn room_key_hex_fields_accept_mixed_case() {
    // §3.2: room-key hex is `a-fA-F`, unlike message `iv`/`hmac`.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "encrypted").await;
    let mut body = wrap(&alice.address, 1);
    body["encryptionIV"] = json!("1A2B3C4D5E6F78901a2b3c4d5e6f7890");
    body["hmac"] = json!("9F".repeat(32));
    body["ephemeralPublicKey"] = json!(format!("04{}", "Ab".repeat(64)));

    alice
        .api
        .post(&format!("/api/rooms/{room}/keys"), body)
        .await
        .expect_status(200);
}

#[tokio::test]
async fn enc_ver_and_key_version_default_to_one() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "defaults").await;
    let mut body = wrap(&alice.address, 1);
    body.as_object_mut().expect("object").remove("encVer");
    body.as_object_mut().expect("object").remove("keyVersion");

    alice
        .api
        .post(&format!("/api/rooms/{room}/keys"), body)
        .await
        .expect_status(200);

    let stored = alice
        .api
        .get(&format!("/api/rooms/{room}/keys"))
        .await
        .expect_ok();
    assert_eq!(i(&stored, "encVer"), 1, "/keys defaults encVer to 1");
    assert_eq!(i(&stored, "keyVersion"), 1);
}

// --- reading --------------------------------------------------------------

#[tokio::test]
async fn getting_a_key_when_none_exists_is_a_404() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "plaintext").await;

    alice
        .api
        .get(&format!("/api/rooms/{room}/keys"))
        .await
        .expect_error(404, "Room key not found");
}

#[tokio::test]
async fn the_versions_endpoint_returns_an_empty_array_not_a_404() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "plaintext").await;

    let resp = alice
        .api
        .get(&format!("/api/rooms/{room}/keys/versions"))
        .await;

    resp.expect_status(200);
    assert!(resp.array().is_empty());
}

#[tokio::test]
async fn a_member_only_ever_sees_their_own_wraps() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "encrypted").await;
    add_member(&alice.api, &bob, &room).await;
    alice
        .api
        .post(&format!("/api/rooms/{room}/keys"), wrap(&alice.address, 1))
        .await
        .expect_status(200);
    bob.api
        .post(&format!("/api/rooms/{room}/keys"), wrap(&bob.address, 1))
        .await
        .expect_status(200);

    let alice_keys = alice
        .api
        .get(&format!("/api/rooms/{room}/keys/versions"))
        .await
        .array();
    assert_eq!(alice_keys.len(), 1);
    assert!(alice_keys
        .iter()
        .all(|k| s(k, "userAddress") == alice.address));

    let bob_keys = bob
        .api
        .get(&format!("/api/rooms/{room}/keys/versions"))
        .await
        .array();
    assert_eq!(bob_keys.len(), 1);
    assert!(bob_keys.iter().all(|k| s(k, "userAddress") == bob.address));
}

#[tokio::test]
async fn get_keys_returns_the_latest_epoch_while_versions_returns_all() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "epochs").await;
    alice
        .api
        .post(&format!("/api/rooms/{room}/keys"), wrap(&alice.address, 1))
        .await
        .expect_status(200);
    rotate_to(&alice, &room, 2, &[alice.address.as_str()]).await;
    rotate_to(&alice, &room, 3, &[alice.address.as_str()]).await;

    let latest = alice
        .api
        .get(&format!("/api/rooms/{room}/keys"))
        .await
        .expect_ok();
    assert_eq!(i(&latest, "keyVersion"), 3);

    let versions = alice
        .api
        .get(&format!("/api/rooms/{room}/keys/versions"))
        .await
        .array();
    let epochs: Vec<i64> = versions.iter().map(|k| i(k, "keyVersion")).collect();
    assert_eq!(epochs, vec![1, 2, 3], "ordered ascending, one per epoch");
}

// --- rotation -------------------------------------------------------------

async fn rotate_to(user: &User, room: &str, new_version: i64, members: &[&str]) {
    let keys: Vec<Value> = members.iter().map(|m| rotation_entry(m)).collect();
    let body = user
        .api
        .post(
            &format!("/api/rooms/{room}/rotate-key"),
            json!({ "newVersion": new_version, "keys": keys }),
        )
        .await
        .expect_ok();
    assert_eq!(s(&body, "message"), "Room key rotated");
    assert_eq!(i(&body, "newVersion"), new_version);
}

#[tokio::test]
async fn any_member_may_rotate_the_room_key() {
    // §6.9.4: deliberately not admin-only — gating on admins would freeze a
    // room after a departure until an admin appeared.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "rotating").await;
    add_member(&alice.api, &bob, &room).await;

    rotate_to(
        &bob,
        &room,
        2,
        &[alice.address.as_str(), bob.address.as_str()],
    )
    .await;

    let detail = alice
        .api
        .get(&format!("/api/rooms/{room}"))
        .await
        .expect_ok();
    assert_eq!(i(&detail, "currentKeyVersion"), 2);
    assert!(!b(&detail, "keyRotationPending"));
}

#[tokio::test]
async fn rotation_requires_membership() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let outsider = new_user(&server, "outsider").await;
    let room = create_room(&alice.api, "closed").await;

    outsider
        .api
        .post(
            &format!("/api/rooms/{room}/rotate-key"),
            json!({ "newVersion": 2, "keys": [rotation_entry(&alice.address)] }),
        )
        .await
        .expect_error(403, "Only room members can rotate the room key");
}

#[tokio::test]
async fn rotation_clears_a_pending_flag_after_a_departure() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "departure").await;
    add_member(&alice.api, &bob, &room).await;
    bob.api
        .post_empty(&format!("/api/rooms/{room}/leave"))
        .await
        .expect_status(200);
    let pending = alice
        .api
        .get(&format!("/api/rooms/{room}"))
        .await
        .expect_ok();
    assert!(b(&pending, "keyRotationPending"));

    rotate_to(&alice, &room, 2, &[alice.address.as_str()]).await;

    let detail = alice
        .api
        .get(&format!("/api/rooms/{room}"))
        .await
        .expect_ok();
    assert!(!b(&detail, "keyRotationPending"));
    assert_eq!(i(&detail, "currentKeyVersion"), 2);
}

#[tokio::test]
async fn rotation_must_cover_every_current_member() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "coverage").await;
    add_member(&alice.api, &bob, &room).await;

    let resp = alice
        .api
        .post(
            &format!("/api/rooms/{room}/rotate-key"),
            json!({ "newVersion": 2, "keys": [rotation_entry(&alice.address)] }),
        )
        .await;

    resp.expect_status(400);
    // Whatever envelope carries it, the uncovered member has to be nameable —
    // §10.3 rule 5 tells the client to refetch the roster and retry, and it
    // cannot do that blind.
    assert!(
        resp.text.contains(&bob.address),
        "the rejection must name the uncovered member: {}",
        resp.text
    );
    assert!(
        resp.text
            .contains("Rotation must include a key for every current member"),
        "unexpected rejection: {}",
        resp.text
    );
}

#[tokio::test]
async fn a_coverage_failure_lists_the_missing_members_in_a_missing_array() {
    // §6.9.4: the documented body is
    // `{"message":"Rotation must include a key for every current member",
    //   "missing":["0x…"]}` — a machine-readable list, not prose.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "coverage").await;
    add_member(&alice.api, &bob, &room).await;

    let resp = alice
        .api
        .post(
            &format!("/api/rooms/{room}/rotate-key"),
            json!({ "newVersion": 2, "keys": [rotation_entry(&alice.address)] }),
        )
        .await;

    resp.expect_error(400, "Rotation must include a key for every current member");
    let listed = resp.json()["missing"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("no `missing` array: {}", resp.text));
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].as_str(), Some(bob.address.as_str()));
}

#[tokio::test]
async fn rotation_rejects_a_wrap_for_a_non_member() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let outsider = new_user(&server, "outsider").await;
    let room = create_room(&alice.api, "strays").await;

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/rotate-key"),
            json!({
                "newVersion": 2,
                "keys": [rotation_entry(&alice.address), rotation_entry(&outsider.address)],
            }),
        )
        .await
        .expect_error(400, "Rotation includes a non-member address");
}

#[tokio::test]
async fn rotation_compares_member_addresses_case_insensitively() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "casing").await;
    let upper = format!("0x{}", alice.address[2..].to_uppercase());

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/rotate-key"),
            json!({ "newVersion": 2, "keys": [rotation_entry(&upper)] }),
        )
        .await
        .expect_status(200);
}

#[tokio::test]
async fn rotation_rejects_version_one_as_a_target() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "versions").await;

    for version in [json!(1), json!(0), json!(-1), json!(1_000_001)] {
        alice
            .api
            .post(
                &format!("/api/rooms/{room}/rotate-key"),
                json!({ "newVersion": version, "keys": [rotation_entry(&alice.address)] }),
            )
            .await
            .expect_validation_failed();
    }
}

#[tokio::test]
async fn rotation_rejects_an_empty_or_oversized_key_list() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "bounds").await;

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/rotate-key"),
            json!({ "newVersion": 2, "keys": [] }),
        )
        .await
        .expect_validation_failed();

    let too_many: Vec<Value> = (0..201)
        .map(|_| rotation_entry(Signer::random().address()))
        .collect();
    alice
        .api
        .post(
            &format!("/api/rooms/{room}/rotate-key"),
            json!({ "newVersion": 2, "keys": too_many }),
        )
        .await
        .expect_validation_failed();
}

#[tokio::test]
async fn a_stale_rotation_target_is_a_409() {
    // §6.9.4 check 5: newVersion must be exactly current + 1. This is the
    // concurrency signal for two members racing to re-key.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "racing").await;

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/rotate-key"),
            json!({ "newVersion": 5, "keys": [rotation_entry(&alice.address)] }),
        )
        .await
        .expect_error(409, "Stale key version — refetch and retry");

    // And the losing side of a real race sees the same thing.
    rotate_to(&alice, &room, 2, &[alice.address.as_str()]).await;
    alice
        .api
        .post(
            &format!("/api/rooms/{room}/rotate-key"),
            json!({ "newVersion": 2, "keys": [rotation_entry(&alice.address)] }),
        )
        .await
        .expect_error(409, "Stale key version — refetch and retry");
}

#[tokio::test]
async fn rotation_defaults_enc_ver_to_two() {
    // §6.9.4: per-entry encVer defaults to 2 here, unlike 1 in /keys.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "defaults").await;
    let mut entry = rotation_entry(&alice.address);
    entry.as_object_mut().expect("object").remove("encVer");

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/rotate-key"),
            json!({ "newVersion": 2, "keys": [entry] }),
        )
        .await
        .expect_status(200);

    let stored = alice
        .api
        .get(&format!("/api/rooms/{room}/keys"))
        .await
        .expect_ok();
    assert_eq!(i(&stored, "encVer"), 2);
}

#[tokio::test]
async fn rotation_forces_the_key_version_it_was_asked_for() {
    // §6.9.4: a per-entry `keyVersion` in the body is ignored.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "forced").await;
    let mut entry = rotation_entry(&alice.address);
    entry["keyVersion"] = json!(99);

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/rotate-key"),
            json!({ "newVersion": 2, "keys": [entry] }),
        )
        .await
        .expect_status(200);

    let stored = alice
        .api
        .get(&format!("/api/rooms/{room}/keys"))
        .await
        .expect_ok();
    assert_eq!(i(&stored, "keyVersion"), 2);
}

#[tokio::test]
async fn rotation_preserves_access_to_earlier_epochs() {
    // §10.1: a member accumulates one wrap per epoch they can read.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "history").await;
    alice
        .api
        .post(&format!("/api/rooms/{room}/keys"), wrap(&alice.address, 1))
        .await
        .expect_status(200);

    rotate_to(&alice, &room, 2, &[alice.address.as_str()]).await;

    let versions = alice
        .api
        .get(&format!("/api/rooms/{room}/keys/versions"))
        .await
        .array();
    assert_eq!(
        versions.len(),
        2,
        "the epoch-1 wrap must survive: {versions:?}"
    );
}

// --- the message epoch gate ----------------------------------------------

async fn send_encrypted(user: &User, room: &str, key_version: i64) -> Resp {
    user.api
        .post(
            &format!("/api/rooms/{room}/messages"),
            json!({
                "content": "Y2lwaGVydGV4dA==",
                "msgHash": crypto::sha256_hex(b"Y2lwaGVydGV4dA=="),
                "isEncrypted": true,
                "iv": "1a2b3c4d5e6f78901a2b3c4d5e6f7890",
                "hmac": "9f".repeat(32),
                "encVer": 2,
                "keyVersion": key_version,
            }),
        )
        .await
}

#[tokio::test]
async fn an_encrypted_message_under_the_current_epoch_is_accepted() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "encrypted").await;

    let msg = send_encrypted(&alice, &room, 1).await.expect_ok();

    assert!(b(&msg, "isEncrypted"));
    assert_eq!(i(&msg, "keyVersion"), 1);
    assert_eq!(i(&msg, "encVer"), 2);
    assert_eq!(s(&msg, "iv"), "1a2b3c4d5e6f78901a2b3c4d5e6f7890");
}

#[tokio::test]
async fn a_stale_key_version_is_a_409_naming_the_current_epoch() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "epochs").await;
    rotate_to(&alice, &room, 2, &[alice.address.as_str()]).await;
    rotate_to(&alice, &room, 3, &[alice.address.as_str()]).await;

    let body = send_encrypted(&alice, &room, 1)
        .await
        .expect_conflict_code("STALE_KEY_VERSION");

    assert_eq!(i(&body, "currentKeyVersion"), 3);
    assert_eq!(
        s(&body, "message"),
        "Message key version does not match the room's current epoch — refetch keys and retry."
    );
}

#[tokio::test]
async fn a_future_key_version_is_also_a_stale_version_conflict() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "epochs").await;

    let body = send_encrypted(&alice, &room, 99)
        .await
        .expect_conflict_code("STALE_KEY_VERSION");

    assert_eq!(i(&body, "currentKeyVersion"), 1);
}

#[tokio::test]
async fn a_pending_rotation_blocks_new_encrypted_messages() {
    // §10.2: refusing here is what stops a removed member's cached key from
    // reading anything sent after their departure.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "forward secrecy").await;
    add_member(&alice.api, &bob, &room).await;
    bob.api
        .post_empty(&format!("/api/rooms/{room}/leave"))
        .await
        .expect_status(200);

    let body = send_encrypted(&alice, &room, 1)
        .await
        .expect_conflict_code("KEY_ROTATION_REQUIRED");

    assert_eq!(i(&body, "currentKeyVersion"), 1);
    assert_eq!(
        s(&body, "message"),
        "Room key rotation is pending — an admin must rotate the key before new encrypted messages can be sent."
    );
}

#[tokio::test]
async fn the_pending_check_precedes_the_epoch_check() {
    // §6.10.1: KEY_ROTATION_REQUIRED is step 2, STALE_KEY_VERSION is step 3.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "ordering").await;
    add_member(&alice.api, &bob, &room).await;
    bob.api
        .post_empty(&format!("/api/rooms/{room}/leave"))
        .await
        .expect_status(200);

    send_encrypted(&alice, &room, 99)
        .await
        .expect_conflict_code("KEY_ROTATION_REQUIRED");
}

#[tokio::test]
async fn rotating_unblocks_encrypted_messaging() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "recovery").await;
    add_member(&alice.api, &bob, &room).await;
    bob.api
        .post_empty(&format!("/api/rooms/{room}/leave"))
        .await
        .expect_status(200);
    send_encrypted(&alice, &room, 1).await.expect_status(409);

    rotate_to(&alice, &room, 2, &[alice.address.as_str()]).await;

    send_encrypted(&alice, &room, 2).await.expect_status(200);
}

#[tokio::test]
async fn plaintext_messages_ignore_the_epoch_gate_entirely() {
    // §6.10.1: unencrypted messages skip all three checks; the room is not
    // even fetched.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "plaintext").await;
    add_member(&alice.api, &bob, &room).await;
    bob.api
        .post_empty(&format!("/api/rooms/{room}/leave"))
        .await
        .expect_status(200);

    send_message(&alice.api, &room, "still fine").await;
}

#[tokio::test]
async fn an_encrypted_edit_is_gated_by_the_same_two_conflicts() {
    // §15 #7: the reference lets an edit write content under an epoch that
    // POST would have refused.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "edit gate").await;
    let msg = send_encrypted(&alice, &room, 1).await.expect_ok();
    let id = s(&msg, "id");
    rotate_to(&alice, &room, 2, &[alice.address.as_str()]).await;

    let body = alice
        .api
        .patch(
            &format!("/api/messages/{id}"),
            json!({
                "content": "c3RhbGUtY2lwaGVydGV4dA==",
                "msgHash": crypto::sha256_hex(b"c3RhbGUtY2lwaGVydGV4dA=="),
                "isEncrypted": true,
                "iv": "1a2b3c4d5e6f78901a2b3c4d5e6f7890",
                "hmac": "9f".repeat(32),
                "encVer": 2,
                "keyVersion": 1,
            }),
        )
        .await
        .expect_conflict_code("STALE_KEY_VERSION");

    assert_eq!(i(&body, "currentKeyVersion"), 2);
}

#[tokio::test]
async fn an_edit_cannot_silently_downgrade_an_encrypted_message() {
    // §15 #7: omitting iv/hmac must not turn ciphertext into plaintext.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "downgrade").await;
    let msg = send_encrypted(&alice, &room, 1).await.expect_ok();
    let id = s(&msg, "id");

    let resp = alice
        .api
        .patch(
            &format!("/api/messages/{id}"),
            json!({ "content": "plaintext now", "msgHash": crypto::sha256_hex(b"plaintext now") }),
        )
        .await;

    assert_ne!(
        resp.code(),
        200,
        "a plaintext edit of an encrypted message must be refused, not silently applied: {}",
        resp.text
    );
    let reread = drain_sync(&alice.api, &room).await;
    let row = reread
        .iter()
        .find(|e| s(e, "id") == id)
        .expect("the message still exists");
    assert!(b(row, "isEncrypted"), "the row must stay encrypted: {row}");
}

// --- authorization --------------------------------------------------------

#[tokio::test]
async fn the_key_endpoints_are_member_only() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let outsider = new_user(&server, "outsider").await;
    let room = create_room(&alice.api, "closed").await;

    outsider
        .api
        .get(&format!("/api/rooms/{room}/keys"))
        .await
        .expect_error(403, "Access denied");
    outsider
        .api
        .get(&format!("/api/rooms/{room}/keys/versions"))
        .await
        .expect_error(403, "Access denied");
}

#[tokio::test]
async fn key_endpoints_validate_the_room_id() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    alice
        .api
        .get("/api/rooms/bad!id/keys")
        .await
        .expect_validation_failed();
    alice
        .api
        .get("/api/rooms/bad!id/keys/versions")
        .await
        .expect_validation_failed();
}
