//! The invitation lifecycle: invite → list → accept/decline, plus block gating
//! and non-invitee rejection. Spec: `docs/API.md` §6.7, §10.

mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn an_admin_can_invite_a_user() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "invited").await;

    let body = alice
        .api
        .post(
            &format!("/api/rooms/{room}/invite"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_ok();

    assert_eq!(s(&body, "message"), "Invitation sent");
    assert!(b(&body, "pending"));
}

#[tokio::test]
async fn an_invitation_creates_no_membership() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "pending").await;
    alice
        .api
        .post(
            &format!("/api/rooms/{room}/invite"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_status(200);

    assert!(room_in_list(&bob.api, &room).await.is_none());
    bob.api
        .get(&format!("/api/rooms/{room}"))
        .await
        .expect_error(403, "Access denied");
    let members = alice
        .api
        .get(&format!("/api/rooms/{room}/members"))
        .await
        .array();
    assert_eq!(members.len(), 1, "the invitee must not be a member yet");
}

#[tokio::test]
async fn the_invitation_list_is_enriched_with_room_and_inviter() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "Team chat").await;
    alice
        .api
        .post(
            &format!("/api/rooms/{room}/invite"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_status(200);

    let invitations = bob.api.get("/api/invitations").await.array();

    assert_eq!(invitations.len(), 1);
    expect_keys(
        &invitations[0],
        &[
            "roomId",
            "roomName",
            "invitedBy",
            "inviterUsername",
            "createdAt",
        ],
    );
    assert_eq!(s(&invitations[0], "roomId"), room);
    assert_eq!(s(&invitations[0], "roomName"), "Team chat");
    assert_eq!(s(&invitations[0], "invitedBy"), alice.address);
    assert_eq!(s(&invitations[0], "inviterUsername"), "alice");
}

#[tokio::test]
async fn the_inviter_does_not_see_the_invitation_in_their_own_list() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "one way").await;
    alice
        .api
        .post(
            &format!("/api/rooms/{room}/invite"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_status(200);

    assert!(alice.api.get("/api/invitations").await.array().is_empty());
}

#[tokio::test]
async fn invitations_are_listed_newest_first() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let first = create_room(&alice.api, "first room").await;
    alice
        .api
        .post(
            &format!("/api/rooms/{first}/invite"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_status(200);
    let second = create_room(&alice.api, "second room").await;
    alice
        .api
        .post(
            &format!("/api/rooms/{second}/invite"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_status(200);

    let invitations = bob.api.get("/api/invitations").await.array();

    assert_eq!(invitations.len(), 2);
    assert_eq!(
        s(&invitations[0], "roomId"),
        second,
        "ordered by created_at DESC: {invitations:?}"
    );
}

#[tokio::test]
async fn accepting_an_invitation_grants_membership() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "joinable").await;
    alice
        .api
        .post(
            &format!("/api/rooms/{room}/invite"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_status(200);

    let body = bob
        .api
        .post_empty(&format!("/api/invitations/{room}/accept"))
        .await
        .expect_ok();

    assert_eq!(s(&body, "message"), "Invitation accepted");
    assert_eq!(s(&body, "roomId"), room);
    assert!(room_in_list(&bob.api, &room).await.is_some());
    bob.api
        .get(&format!("/api/rooms/{room}"))
        .await
        .expect_status(200);
    assert!(
        bob.api.get("/api/invitations").await.array().is_empty(),
        "an accepted invitation must be consumed"
    );
}

#[tokio::test]
async fn an_invitation_cannot_be_accepted_twice() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "joinable").await;
    add_member(&alice.api, &bob, &room).await;

    bob.api
        .post_empty(&format!("/api/invitations/{room}/accept"))
        .await
        .expect_error(404, "No pending invitation for this room");
}

#[tokio::test]
async fn a_non_invitee_cannot_accept() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let mallory = new_user(&server, "mallory").await;
    let room = create_room(&alice.api, "targeted").await;
    alice
        .api
        .post(
            &format!("/api/rooms/{room}/invite"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_status(200);

    mallory
        .api
        .post_empty(&format!("/api/invitations/{room}/accept"))
        .await
        .expect_error(404, "No pending invitation for this room");
    assert!(room_in_list(&mallory.api, &room).await.is_none());
}

#[tokio::test]
async fn declining_removes_the_invitation_without_granting_membership() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "declinable").await;
    alice
        .api
        .post(
            &format!("/api/rooms/{room}/invite"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_status(200);

    bob.api
        .post_empty(&format!("/api/invitations/{room}/decline"))
        .await
        .expect_message("Invitation declined");

    assert!(bob.api.get("/api/invitations").await.array().is_empty());
    assert!(room_in_list(&bob.api, &room).await.is_none());
    bob.api
        .post_empty(&format!("/api/invitations/{room}/accept"))
        .await
        .expect_error(404, "No pending invitation for this room");
}

#[tokio::test]
async fn declining_without_an_invitation_is_a_404() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "untouched").await;

    bob.api
        .post_empty(&format!("/api/invitations/{room}/decline"))
        .await
        .expect_error(404, "No pending invitation for this room");
}

#[tokio::test]
async fn declining_discards_a_pre_wrapped_room_key() {
    // §6.7.4 step 3: the wrap must not survive a decline, across all epochs.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    bob.publish_encryption_key().await;
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
        .post(
            &format!("/api/rooms/{room}/keys"),
            fake_wrap(&bob.address, 1),
        )
        .await
        .expect_status(200);

    bob.api
        .post_empty(&format!("/api/invitations/{room}/decline"))
        .await
        .expect_status(200);

    // Re-invite and accept: the discarded wrap must not reappear.
    add_member(&alice.api, &bob, &room).await;
    let versions = bob
        .api
        .get(&format!("/api/rooms/{room}/keys/versions"))
        .await
        .array();
    assert!(
        versions.is_empty(),
        "a declined invitation's wrap must be deleted: {versions:?}"
    );
}

#[tokio::test]
async fn inviting_requires_admin_rights() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let carol = new_user(&server, "carol").await;
    let room = create_room(&alice.api, "guarded").await;
    add_member(&alice.api, &bob, &room).await;

    bob.api
        .post(
            &format!("/api/rooms/{room}/invite"),
            json!({ "userAddress": carol.address }),
        )
        .await
        .expect_error(403, "Only room admins can invite users");
}

#[tokio::test]
async fn inviting_an_unknown_user_is_a_404() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "solo").await;

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/invite"),
            json!({ "userAddress": Signer::random().address() }),
        )
        .await
        .expect_error(404, "User not found");
}

#[tokio::test]
async fn inviting_an_existing_member_is_rejected() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "full").await;
    add_member(&alice.api, &bob, &room).await;

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/invite"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_error(400, "User is already a member of this room");
}

#[tokio::test]
async fn inviting_into_a_nonexistent_room_is_a_404() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;

    alice
        .api
        .post(
            "/api/rooms/room_0000000000_deadbeef-dead-beef-dead-beefdeadbeef/invite",
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_error(404, "Room not found");
}

#[tokio::test]
async fn re_inviting_is_idempotent() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "repeat").await;

    for _ in 0..3 {
        alice
            .api
            .post(
                &format!("/api/rooms/{room}/invite"),
                json!({ "userAddress": bob.address }),
            )
            .await
            .expect_status(200);
    }

    let invitations = bob.api.get("/api/invitations").await.array();
    assert_eq!(
        invitations.len(),
        1,
        "UNIQUE(room_id, invited_address): {invitations:?}"
    );
}

#[tokio::test]
async fn inviting_rejects_a_malformed_address() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "solo").await;

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/invite"),
            json!({ "userAddress": "0xnope" }),
        )
        .await
        .expect_validation_failed();
    alice
        .api
        .post(&format!("/api/rooms/{room}/invite"), json!({}))
        .await
        .expect_validation_failed();
}

#[tokio::test]
async fn an_invitee_who_blocked_the_inviter_cannot_be_invited() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "blocked").await;
    bob.api
        .post("/api/users/block", json!({ "address": alice.address }))
        .await
        .expect_status(200);

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/invite"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_error(403, "You cannot invite users who have blocked you");
}

#[tokio::test]
async fn an_inviter_cannot_invite_someone_they_blocked() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "blocked").await;
    alice
        .api
        .post("/api/users/block", json!({ "address": bob.address }))
        .await
        .expect_status(200);

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/invite"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_error(403, "You cannot invite users you have blocked");
}

#[tokio::test]
async fn the_blocked_by_check_precedes_the_blocker_check() {
    // §6.7.1 check order 5 then 6: when both directions block, the
    // "have blocked you" message wins.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "mutual").await;
    bob.api
        .post("/api/users/block", json!({ "address": alice.address }))
        .await
        .expect_status(200);
    alice
        .api
        .post("/api/users/block", json!({ "address": bob.address }))
        .await
        .expect_status(200);

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/invite"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_error(403, "You cannot invite users who have blocked you");
}

#[tokio::test]
async fn an_invitation_to_a_deleted_room_is_dropped_from_the_list() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let live = create_room(&alice.api, "still here").await;
    let doomed = create_room(&alice.api, "going away").await;
    for room in [&live, &doomed] {
        alice
            .api
            .post(
                &format!("/api/rooms/{room}/invite"),
                json!({ "userAddress": bob.address }),
            )
            .await
            .expect_status(200);
    }

    alice
        .api
        .delete(&format!("/api/rooms/{doomed}"))
        .await
        .expect_status(200);

    let invitations = bob.api.get("/api/invitations").await.array();
    assert_eq!(invitations.len(), 1);
    assert_eq!(s(&invitations[0], "roomId"), live);
}

#[tokio::test]
async fn a_room_literally_named_deleted_room_is_still_listed() {
    // §15 #12: the reference filters on the literal string "(deleted room)",
    // which would hide a genuine room with that name from every invitee.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "(deleted room)").await;
    alice
        .api
        .post(
            &format!("/api/rooms/{room}/invite"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_status(200);

    let invitations = bob.api.get("/api/invitations").await.array();

    assert_eq!(
        invitations.len(),
        1,
        "the filter must be on room-missing, not on the name"
    );
    assert_eq!(s(&invitations[0], "roomName"), "(deleted room)");
}

#[tokio::test]
async fn accepting_an_invitation_to_a_deleted_room_is_a_404() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "vanishing").await;
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
        .delete(&format!("/api/rooms/{room}"))
        .await
        .expect_status(200);

    let resp = bob
        .api
        .post_empty(&format!("/api/invitations/{room}/accept"))
        .await;

    resp.expect_status(404);
    let message = resp.message();
    assert!(
        message == "Room no longer exists" || message == "No pending invitation for this room",
        "unexpected 404 body: {message}"
    );
}

#[tokio::test]
async fn accept_and_decline_validate_the_room_id() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    alice
        .api
        .post_empty("/api/invitations/bad!id/accept")
        .await
        .expect_validation_failed();
    alice
        .api
        .post_empty("/api/invitations/bad!id/decline")
        .await
        .expect_validation_failed();
}

#[tokio::test]
async fn a_pre_wrapped_key_survives_acceptance() {
    // §10.2: pre-wrapping at invite time is the whole point — the admin is
    // online then, the invitee may not be.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    bob.publish_encryption_key().await;
    let room = create_room(&alice.api, "prewrapped").await;
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
        .post(
            &format!("/api/rooms/{room}/keys"),
            fake_wrap(&bob.address, 1),
        )
        .await
        .expect_status(200);

    bob.api
        .post_empty(&format!("/api/invitations/{room}/accept"))
        .await
        .expect_status(200);

    let versions = bob
        .api
        .get(&format!("/api/rooms/{room}/keys/versions"))
        .await
        .array();
    assert_eq!(
        versions.len(),
        1,
        "the pre-wrap becomes readable on acceptance"
    );
    assert_eq!(i(&versions[0], "keyVersion"), 1);
}

/// A structurally valid wrap. These invitation tests only exercise storage and
/// authorization, so the ciphertext never has to decrypt — `e2ee.rs` covers the
/// genuine round trip.
fn fake_wrap(user_address: &str, key_version: i64) -> serde_json::Value {
    json!({
        "userAddress": user_address,
        "encryptedSymmetricKey": "c2hhcmVkLXJvb20ta2V5LWNpcGhlcnRleHQ=",
        "ephemeralPublicKey": format!("04{}", "ab".repeat(64)),
        "encryptionIV": "1a2b3c4d5e6f78901a2b3c4d5e6f7890",
        "hmac": "9f".repeat(32),
        "encVer": 2,
        "keyVersion": key_version,
    })
}
