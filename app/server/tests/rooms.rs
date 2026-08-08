//! Rooms: lifecycle, hiding, membership, admins, and every authorization rule
//! in the §14 index. Spec: `docs/API.md` §6.5, §6.6, §6.8.

mod common;

use common::*;
use serde_json::{json, Value};

// --- creation -------------------------------------------------------------

#[tokio::test]
async fn creating_a_room_returns_a_bare_room_object() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    let body = alice
        .api
        .post("/api/rooms", json!({ "name": "Team chat" }))
        .await
        .expect_ok();

    expect_room_shape(&body);
    assert_eq!(s(&body, "name"), "Team chat");
    assert_eq!(i(&body, "currentKeyVersion"), 1);
    assert!(!b(&body, "keyRotationPending"));
    // §6.5.1: the create response is a bare Room.
    expect_no_keys(&body, &["members", "admins", "memberCount"]);
}

#[tokio::test]
async fn a_new_room_id_satisfies_the_room_id_schema() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "shape").await;

    assert!((10..=100).contains(&room.len()), "roomId length: {room}");
    assert!(
        room.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-'),
        "roomId charset: {room}"
    );
}

#[tokio::test]
async fn the_creator_becomes_the_sole_member_and_admin() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "solo").await;

    let members = alice
        .api
        .get(&format!("/api/rooms/{room}/members"))
        .await
        .array();
    assert_eq!(members.len(), 1);
    expect_keys(&members[0], &["roomId", "userAddress", "joinedAt", "user"]);
    assert_eq!(s(&members[0], "userAddress"), alice.address);
    expect_user_shape(&members[0]["user"]);

    let admins = alice
        .api
        .get(&format!("/api/rooms/{room}/admins"))
        .await
        .array();
    assert_eq!(admins.len(), 1);
    expect_user_shape(&admins[0]);
    assert_eq!(s(&admins[0], "walletAddress"), alice.address);
}

#[tokio::test]
async fn a_description_is_stored_and_an_empty_one_becomes_null() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    let with = alice
        .api
        .post(
            "/api/rooms",
            json!({ "name": "described", "description": "the details" }),
        )
        .await
        .expect_ok();
    assert_eq!(s(&with, "description"), "the details");

    let without = alice
        .api
        .post("/api/rooms", json!({ "name": "empty", "description": "" }))
        .await
        .expect_ok();
    assert!(
        without["description"].is_null(),
        "an empty description is stored as NULL: {without}"
    );
}

#[tokio::test]
async fn creating_a_room_rejects_an_invalid_name() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    for bad in [
        json!({}),
        json!({ "name": "" }),
        json!({ "name": "no<angles>" }),
        json!({ "name": "a;b" }),
    ] {
        alice
            .api
            .post("/api/rooms", bad)
            .await
            .expect_validation_failed();
    }
}

#[tokio::test]
async fn creating_a_room_rejects_an_over_long_name_or_description() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    alice
        .api
        .post("/api/rooms", json!({ "name": "n".repeat(101) }))
        .await
        .expect_validation_failed();
    alice
        .api
        .post(
            "/api/rooms",
            json!({ "name": "ok", "description": "d".repeat(501) }),
        )
        .await
        .expect_validation_failed();
}

#[tokio::test]
async fn creating_a_room_requires_authentication() {
    let server = TestServer::start().await;
    Api::anonymous(&server.base_url)
        .post("/api/rooms", json!({ "name": "nope" }))
        .await
        .expect_status(401);
}

#[tokio::test]
async fn rooms_are_not_publicly_discoverable() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    create_room(&alice.api, "private").await;

    // Bob's listing is his own three built-in rooms and nothing else. Asserted
    // as "nothing anybody made" rather than "empty" because every account now
    // starts with My Note, My Jarvis and My Lobby — and asserting a count of
    // three would still pass if Alice's room had appeared and one of Bob's had
    // not.
    assert!(
        made_rooms(&bob.api.get("/api/rooms").await).is_empty(),
        "membership arrives only via creation or an accepted invitation"
    );
}

// --- listing --------------------------------------------------------------

#[tokio::test]
async fn the_room_list_is_enriched_with_members_admins_and_read_state() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "enriched").await;

    let entry = room_in_list(&alice.api, &room).await.expect("room in list");

    expect_room_shape(&entry);
    expect_keys(
        &entry,
        &[
            "memberCount",
            "members",
            "admins",
            "hasEncryption",
            "unreadCount",
            "lastReadSerial",
        ],
    );
    assert_eq!(i(&entry, "memberCount"), 1);
    assert!(!b(&entry, "hasEncryption"), "no room_keys row yet");
    assert_eq!(i(&entry, "unreadCount"), 0);
    assert_eq!(i(&entry, "lastReadSerial"), 0);
}

#[tokio::test]
async fn the_room_list_carries_the_last_message_preview() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "preview").await;
    send_message(&alice.api, &room, "the newest thing").await;

    let entry = room_in_list(&alice.api, &room).await.expect("room in list");

    let last = entry
        .get("lastMessage")
        .unwrap_or_else(|| panic!("expected lastMessage: {entry}"));
    assert_eq!(s(last, "content"), "the newest thing");
    expect_user_shape(&last["sender"]);
}

#[tokio::test]
async fn an_empty_room_has_no_last_message_key_at_all() {
    // §5.3: the key is *absent*, not null.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "quiet").await;

    let entry = room_in_list(&alice.api, &room).await.expect("room in list");

    expect_no_keys(&entry, &["lastMessage"]);
}

#[tokio::test]
async fn a_delete_all_marker_never_surfaces_as_the_last_message() {
    // §15 #17.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "purged").await;
    send_message(&alice.api, &room, "will be purged").await;
    alice
        .api
        .delete(&format!("/api/rooms/{room}/messages"))
        .await
        .expect_status(200);

    let entry = room_in_list(&alice.api, &room).await.expect("room in list");

    if let Some(last) = entry.get("lastMessage") {
        assert_ne!(
            s(last, "msgType"),
            "delete_all",
            "a delete_all marker must be excluded from the preview: {entry}"
        );
    }
}

// --- detail ---------------------------------------------------------------

#[tokio::test]
async fn room_detail_omits_the_read_state() {
    // §5.3: unreadCount/lastReadSerial live only on GET /api/rooms.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "detail").await;

    let body = alice
        .api
        .get(&format!("/api/rooms/{room}"))
        .await
        .expect_ok();

    expect_room_shape(&body);
    expect_keys(
        &body,
        &["memberCount", "members", "admins", "hasEncryption"],
    );
    expect_no_keys(&body, &["unreadCount", "lastReadSerial"]);
}

#[tokio::test]
async fn room_detail_is_denied_to_a_non_member() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "closed").await;

    bob.api
        .get(&format!("/api/rooms/{room}"))
        .await
        .expect_error(403, "Access denied");
}

#[tokio::test]
async fn a_nonexistent_room_answers_403_not_404() {
    // §6.5.3: membership is checked before existence, so there is no
    // room-existence oracle. This ordering is deliberate — reproduce it.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    alice
        .api
        .get("/api/rooms/room_0000000000_deadbeef-dead-beef-dead-beefdeadbeef")
        .await
        .expect_error(403, "Access denied");
}

#[tokio::test]
async fn a_malformed_room_id_is_a_validation_error() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    let too_long = "x".repeat(101);
    for bad in ["short", too_long.as_str(), "has%20space", "has%3Bsemi"] {
        alice
            .api
            .get(&format!("/api/rooms/{bad}"))
            .await
            .expect_validation_failed();
    }
}

// --- rename ---------------------------------------------------------------

#[tokio::test]
async fn an_admin_can_rename_a_room() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "old name").await;

    let body = alice
        .api
        .patch(&format!("/api/rooms/{room}"), json!({ "name": "new name" }))
        .await
        .expect_ok();

    expect_room_shape(&body);
    assert_eq!(s(&body, "name"), "new name");
    expect_no_keys(&body, &["members", "admins"]);
}

#[tokio::test]
async fn renaming_requires_admin_rights() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "guarded").await;
    add_member(&alice.api, &bob, &room).await;

    bob.api
        .patch(&format!("/api/rooms/{room}"), json!({ "name": "hijacked" }))
        .await
        .expect_error(403, "Only room admins can update the room");
}

#[tokio::test]
async fn renaming_a_nonexistent_room_is_a_404() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    alice
        .api
        .patch(
            "/api/rooms/room_0000000000_deadbeef-dead-beef-dead-beefdeadbeef",
            json!({ "name": "ghost" }),
        )
        .await
        .expect_error(404, "Room not found");
}

#[tokio::test]
async fn renaming_requires_a_valid_name() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "named").await;

    alice
        .api
        .patch(&format!("/api/rooms/{room}"), json!({}))
        .await
        .expect_validation_failed();
    alice
        .api
        .patch(
            &format!("/api/rooms/{room}"),
            json!({ "name": "bad<name>" }),
        )
        .await
        .expect_validation_failed();
}

// --- deletion -------------------------------------------------------------

#[tokio::test]
async fn an_admin_can_delete_a_room_and_it_disappears_for_everyone() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "doomed").await;
    add_member(&alice.api, &bob, &room).await;

    alice
        .api
        .delete(&format!("/api/rooms/{room}"))
        .await
        .expect_message("Room deleted successfully");

    assert!(room_in_list(&alice.api, &room).await.is_none());
    assert!(room_in_list(&bob.api, &room).await.is_none());
    bob.api
        .get(&format!("/api/rooms/{room}/sync?since=0"))
        .await
        .expect_error(403, "Access denied");
}

#[tokio::test]
async fn deleting_requires_admin_rights() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "guarded").await;
    add_member(&alice.api, &bob, &room).await;

    bob.api
        .delete(&format!("/api/rooms/{room}"))
        .await
        .expect_error(403, "Only room admins can delete the room");
}

#[tokio::test]
async fn deleting_a_nonexistent_room_is_a_404() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    alice
        .api
        .delete("/api/rooms/room_0000000000_deadbeef-dead-beef-dead-beefdeadbeef")
        .await
        .expect_error(404, "Room not found");
}

#[tokio::test]
async fn deleting_a_room_also_clears_its_invitations() {
    // §15 #11: the reference orphans room_invitations rows.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "doomed").await;
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

    assert!(
        bob.api.get("/api/invitations").await.array().is_empty(),
        "an invitation to a deleted room must not be listed"
    );
    bob.api
        .post_empty(&format!("/api/invitations/{room}/accept"))
        .await
        .expect_status(404);
}

// --- hiding ---------------------------------------------------------------

#[tokio::test]
async fn hiding_a_room_removes_it_from_the_list_and_unhiding_restores_it() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "hideable").await;

    let hidden = alice
        .api
        .post_empty(&format!("/api/rooms/{room}/hide"))
        .await
        .expect_ok();
    expect_keys(&hidden, &["userAddress", "roomId", "createdAt"]);
    assert_eq!(s(&hidden, "roomId"), room);

    assert!(room_in_list(&alice.api, &room).await.is_none());
    let hidden_list = alice.api.get("/api/rooms/hidden").await.array();
    assert_eq!(hidden_list.len(), 1);
    expect_keys(
        &hidden_list[0],
        &["userAddress", "roomId", "createdAt", "room"],
    );
    expect_no_keys(&hidden_list[0]["room"], &["unreadCount", "lastReadSerial"]);

    alice
        .api
        .delete(&format!("/api/rooms/{room}/hide"))
        .await
        .expect_message("Room unhidden successfully");
    assert!(room_in_list(&alice.api, &room).await.is_some());
    assert!(alice.api.get("/api/rooms/hidden").await.array().is_empty());
}

#[tokio::test]
async fn hiding_is_idempotent() {
    // §15 #4: no duplicate hidden_rooms rows.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "hideable").await;

    alice
        .api
        .post_empty(&format!("/api/rooms/{room}/hide"))
        .await
        .expect_status(200);
    alice
        .api
        .post_empty(&format!("/api/rooms/{room}/hide"))
        .await
        .expect_status(200);

    let hidden = alice.api.get("/api/rooms/hidden").await.array();
    assert_eq!(
        hidden.len(),
        1,
        "repeated hides must not duplicate: {hidden:?}"
    );
}

#[tokio::test]
async fn hiding_requires_membership() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "closed").await;

    bob.api
        .post_empty(&format!("/api/rooms/{room}/hide"))
        .await
        .expect_error(403, "You must be a member of the room to hide it");
}

#[tokio::test]
async fn hiding_a_malformed_room_id_is_a_400_not_a_500() {
    // §15 #6.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    alice
        .api
        .post_empty("/api/rooms/bad!id/hide")
        .await
        .expect_validation_failed();
    alice
        .api
        .delete("/api/rooms/bad!id/hide")
        .await
        .expect_validation_failed();
}

#[tokio::test]
async fn unhiding_a_room_you_never_hid_still_succeeds() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "never hidden").await;

    alice
        .api
        .delete(&format!("/api/rooms/{room}/hide"))
        .await
        .expect_message("Room unhidden successfully");
}

#[tokio::test]
async fn hiding_does_not_affect_membership_or_message_delivery() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "hidden but live").await;
    add_member(&alice.api, &bob, &room).await;
    alice
        .api
        .post_empty(&format!("/api/rooms/{room}/hide"))
        .await
        .expect_status(200);

    send_message(&bob.api, &room, "still delivered").await;

    alice
        .api
        .get(&format!("/api/rooms/{room}"))
        .await
        .expect_status(200);
    let events = drain_sync(&alice.api, &room).await;
    assert!(events.iter().any(|e| s(e, "content") == "still delivered"));
}

#[tokio::test]
async fn the_hidden_list_drops_rooms_you_are_no_longer_a_member_of() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "leaving").await;
    add_member(&alice.api, &bob, &room).await;
    bob.api
        .post_empty(&format!("/api/rooms/{room}/hide"))
        .await
        .expect_status(200);

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/kick"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_status(200);

    assert!(
        bob.api.get("/api/rooms/hidden").await.array().is_empty(),
        "a former member must not keep reading the room through the hidden list"
    );
}

#[tokio::test]
async fn the_hidden_route_is_not_parsed_as_a_room_id() {
    // §14.1: `/api/rooms/hidden` must win over `/api/rooms/:roomId`.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    alice.api.get("/api/rooms/hidden").await.expect_status(200);
}

// --- leave ----------------------------------------------------------------

#[tokio::test]
async fn leave_requires_membership() {
    // §15 #1 — the headline fix. In the reference, any authenticated user can
    // POST /leave on any room whose ID they know and set keyRotationPending,
    // which is a remote denial of service on all encrypted messaging there.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let attacker = new_user(&server, "attacker").await;
    let room = create_room(&alice.api, "target").await;

    attacker
        .api
        .post_empty(&format!("/api/rooms/{room}/leave"))
        .await
        .expect_status(403);

    let detail = alice
        .api
        .get(&format!("/api/rooms/{room}"))
        .await
        .expect_ok();
    assert!(
        !b(&detail, "keyRotationPending"),
        "an outsider must not be able to flip keyRotationPending: {detail}"
    );
}

#[tokio::test]
async fn a_member_can_leave_and_loses_access() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "leavable").await;
    add_member(&alice.api, &bob, &room).await;

    bob.api
        .post_empty(&format!("/api/rooms/{room}/leave"))
        .await
        .expect_message("Left room successfully");

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
    assert_eq!(members.len(), 1);
}

#[tokio::test]
async fn leaving_sets_key_rotation_pending() {
    // The leaver may still hold the current key, so the room fails closed
    // until a remaining member re-keys.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "encrypted").await;
    add_member(&alice.api, &bob, &room).await;

    bob.api
        .post_empty(&format!("/api/rooms/{room}/leave"))
        .await
        .expect_status(200);

    let detail = alice
        .api
        .get(&format!("/api/rooms/{room}"))
        .await
        .expect_ok();
    assert!(b(&detail, "keyRotationPending"));
}

#[tokio::test]
async fn the_last_admin_cannot_leave() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "one admin").await;
    add_member(&alice.api, &bob, &room).await;

    alice
        .api
        .post_empty(&format!("/api/rooms/{room}/leave"))
        .await
        .expect_error(
            400,
            "Cannot leave room as the last admin. Transfer admin rights first or delete the room.",
        );
}

#[tokio::test]
async fn an_admin_can_leave_once_another_admin_exists() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "two admins").await;
    add_member(&alice.api, &bob, &room).await;
    alice
        .api
        .post(
            &format!("/api/rooms/{room}/admins"),
            json!({ "walletAddress": bob.address }),
        )
        .await
        .expect_status(200);

    alice
        .api
        .post_empty(&format!("/api/rooms/{room}/leave"))
        .await
        .expect_status(200);

    let admins = bob
        .api
        .get(&format!("/api/rooms/{room}/admins"))
        .await
        .array();
    assert_eq!(admins.len(), 1);
    assert_eq!(s(&admins[0], "walletAddress"), bob.address);
}

#[tokio::test]
async fn leaving_a_nonexistent_room_is_rejected() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    let resp = alice
        .api
        .post_empty("/api/rooms/room_0000000000_deadbeef-dead-beef-dead-beefdeadbeef/leave")
        .await;

    assert!(
        resp.code() == 403 || resp.code() == 404,
        "a nonexistent room is 404, or 403 under the membership gate; got {} / {}",
        resp.code(),
        resp.text
    );
}

// --- kick -----------------------------------------------------------------

#[tokio::test]
async fn an_admin_can_kick_a_member() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "kickable").await;
    add_member(&alice.api, &bob, &room).await;

    let body = alice
        .api
        .post(
            &format!("/api/rooms/{room}/kick"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_ok();

    assert_eq!(s(&body, "message"), "User removed from room");
    assert!(b(&body, "keyRotationPending"));
    bob.api
        .get(&format!("/api/rooms/{room}"))
        .await
        .expect_error(403, "Access denied");
}

#[tokio::test]
async fn kicking_requires_admin_rights() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let carol = new_user(&server, "carol").await;
    let room = create_room(&alice.api, "guarded").await;
    add_member(&alice.api, &bob, &room).await;
    add_member(&alice.api, &carol, &room).await;

    bob.api
        .post(
            &format!("/api/rooms/{room}/kick"),
            json!({ "userAddress": carol.address }),
        )
        .await
        .expect_error(403, "Only room admins can remove members");
}

#[tokio::test]
async fn the_admin_check_precedes_body_validation_on_kick() {
    // §6.5.7: authorization is decided before the body is parsed, so a
    // non-admin sending garbage still gets a 403, never a 400.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "guarded").await;
    add_member(&alice.api, &bob, &room).await;

    bob.api
        .post(
            &format!("/api/rooms/{room}/kick"),
            json!({ "userAddress": "garbage" }),
        )
        .await
        .expect_error(403, "Only room admins can remove members");
}

#[tokio::test]
async fn kicking_yourself_is_rejected() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "solo").await;

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/kick"),
            json!({ "userAddress": alice.address }),
        )
        .await
        .expect_error(400, "Cannot kick yourself. Use leave instead.");
}

#[tokio::test]
async fn kicking_a_non_member_is_a_404() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "solo").await;

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/kick"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_error(404, "User is not a member of this room");
}

#[tokio::test]
async fn kicking_with_a_malformed_address_is_a_validation_error() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "solo").await;

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/kick"),
            json!({ "userAddress": "0xnope" }),
        )
        .await
        .expect_validation_failed();
}

#[tokio::test]
async fn an_admin_can_kick_another_admin() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "two admins").await;
    add_member(&alice.api, &bob, &room).await;
    alice
        .api
        .post(
            &format!("/api/rooms/{room}/admins"),
            json!({ "walletAddress": bob.address }),
        )
        .await
        .expect_status(200);

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/kick"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_status(200);

    let admins = alice
        .api
        .get(&format!("/api/rooms/{room}/admins"))
        .await
        .array();
    assert_eq!(admins.len(), 1);
}

// --- members / admins listings -------------------------------------------

#[tokio::test]
async fn the_member_list_is_member_only() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "closed").await;

    bob.api
        .get(&format!("/api/rooms/{room}/members"))
        .await
        .expect_error(403, "Access denied");
}

#[tokio::test]
async fn the_admin_list_is_member_only() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "closed").await;

    bob.api
        .get(&format!("/api/rooms/{room}/admins"))
        .await
        .expect_error(403, "Access denied");
}

// --- admin management -----------------------------------------------------

#[tokio::test]
async fn an_admin_can_promote_a_member() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "promotion").await;
    add_member(&alice.api, &bob, &room).await;

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/admins"),
            json!({ "walletAddress": bob.address }),
        )
        .await
        .expect_message("Admin added successfully");

    let admins = alice
        .api
        .get(&format!("/api/rooms/{room}/admins"))
        .await
        .array();
    assert_eq!(admins.len(), 2);
    assert!(admins.iter().any(|a| s(a, "walletAddress") == bob.address));
}

#[tokio::test]
async fn promoting_requires_admin_rights() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let carol = new_user(&server, "carol").await;
    let room = create_room(&alice.api, "guarded").await;
    add_member(&alice.api, &bob, &room).await;
    add_member(&alice.api, &carol, &room).await;

    bob.api
        .post(
            &format!("/api/rooms/{room}/admins"),
            json!({ "walletAddress": carol.address }),
        )
        .await
        .expect_error(403, "Only room admins can add new admins");
}

#[tokio::test]
async fn promoting_a_non_member_is_rejected() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "solo").await;

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/admins"),
            json!({ "walletAddress": bob.address }),
        )
        .await
        .expect_error(400, "User must be a member of the room to become an admin");
}

#[tokio::test]
async fn promoting_an_unknown_user_is_a_404() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "solo").await;

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/admins"),
            json!({ "walletAddress": Signer::random().address() }),
        )
        .await
        .expect_error(404, "User not found");
}

#[tokio::test]
async fn promoting_an_existing_admin_is_rejected() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "solo").await;

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/admins"),
            json!({ "walletAddress": alice.address }),
        )
        .await
        .expect_error(400, "User is already an admin");
}

#[tokio::test]
async fn promoting_rejects_a_missing_or_malformed_address() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "solo").await;

    alice
        .api
        .post(&format!("/api/rooms/{room}/admins"), json!({}))
        .await
        .expect_error(400, "Wallet address is required");
    alice
        .api
        .post(
            &format!("/api/rooms/{room}/admins"),
            json!({ "walletAddress": "0xnope" }),
        )
        .await
        .expect_error(400, "Invalid wallet address format");
}

#[tokio::test]
async fn a_room_admits_at_most_nine_admins() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "crowded").await;

    // Alice is admin #1; promote eight more, then verify the ninth is refused.
    let mut extras = Vec::new();
    for n in 0..9 {
        let member = new_user(&server, &format!("member{n}")).await;
        add_member(&alice.api, &member, &room).await;
        extras.push(member);
    }

    for member in extras.iter().take(8) {
        alice
            .api
            .post(
                &format!("/api/rooms/{room}/admins"),
                json!({ "walletAddress": member.address }),
            )
            .await
            .expect_status(200);
    }

    let admins = alice
        .api
        .get(&format!("/api/rooms/{room}/admins"))
        .await
        .array();
    assert_eq!(admins.len(), 9);

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/admins"),
            json!({ "walletAddress": extras[8].address }),
        )
        .await
        .expect_error(400, "Maximum admin count (9) reached");
}

#[tokio::test]
async fn an_admin_can_be_demoted_while_another_remains() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "demotion").await;
    add_member(&alice.api, &bob, &room).await;
    alice
        .api
        .post(
            &format!("/api/rooms/{room}/admins"),
            json!({ "walletAddress": bob.address }),
        )
        .await
        .expect_status(200);

    alice
        .api
        .delete(&format!("/api/rooms/{room}/admins/{}", bob.address))
        .await
        .expect_message("Admin removed successfully");

    let admins = alice
        .api
        .get(&format!("/api/rooms/{room}/admins"))
        .await
        .array();
    assert_eq!(admins.len(), 1);
    // Demotion must not remove membership.
    let members = alice
        .api
        .get(&format!("/api/rooms/{room}/members"))
        .await
        .array();
    assert!(members.iter().any(|m| s(m, "userAddress") == bob.address));
}

#[tokio::test]
async fn an_admin_may_demote_themselves_when_another_admin_remains() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "self demotion").await;
    add_member(&alice.api, &bob, &room).await;
    alice
        .api
        .post(
            &format!("/api/rooms/{room}/admins"),
            json!({ "walletAddress": bob.address }),
        )
        .await
        .expect_status(200);

    alice
        .api
        .delete(&format!("/api/rooms/{room}/admins/{}", alice.address))
        .await
        .expect_status(200);

    let admins = bob
        .api
        .get(&format!("/api/rooms/{room}/admins"))
        .await
        .array();
    assert_eq!(admins.len(), 1);
    assert_eq!(s(&admins[0], "walletAddress"), bob.address);
}

#[tokio::test]
async fn the_last_admin_cannot_be_removed() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "one admin").await;

    alice
        .api
        .delete(&format!("/api/rooms/{room}/admins/{}", alice.address))
        .await
        .expect_error(
            400,
            "Cannot remove the last admin. Room must have at least one admin.",
        );
}

#[tokio::test]
async fn demoting_a_non_admin_is_rejected() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let carol = new_user(&server, "carol").await;
    let room = create_room(&alice.api, "demotion").await;
    add_member(&alice.api, &bob, &room).await;
    add_member(&alice.api, &carol, &room).await;
    alice
        .api
        .post(
            &format!("/api/rooms/{room}/admins"),
            json!({ "walletAddress": bob.address }),
        )
        .await
        .expect_status(200);

    alice
        .api
        .delete(&format!("/api/rooms/{room}/admins/{}", carol.address))
        .await
        .expect_error(400, "User is not an admin");
}

#[tokio::test]
async fn demoting_requires_admin_rights() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "guarded").await;
    add_member(&alice.api, &bob, &room).await;

    bob.api
        .delete(&format!("/api/rooms/{room}/admins/{}", alice.address))
        .await
        .expect_error(403, "Only room admins can remove admins");
}

#[tokio::test]
async fn demoting_validates_both_path_parameters() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "params").await;

    alice
        .api
        .delete(&format!("/api/rooms/{room}/admins/0xnope"))
        .await
        .expect_validation_failed();
    alice
        .api
        .delete(&format!("/api/rooms/bad!id/admins/{}", alice.address))
        .await
        .expect_validation_failed();
}

// --- authorization sweep --------------------------------------------------

#[tokio::test]
async fn every_member_only_endpoint_denies_a_non_member() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let outsider = new_user(&server, "outsider").await;
    let room = create_room(&alice.api, "closed").await;

    let gets = [
        format!("/api/rooms/{room}"),
        format!("/api/rooms/{room}/members"),
        format!("/api/rooms/{room}/admins"),
        format!("/api/rooms/{room}/messages"),
        format!("/api/rooms/{room}/sync?since=0"),
        format!("/api/rooms/{room}/latest-serial"),
        format!("/api/rooms/{room}/latest-timestamp"),
        format!("/api/rooms/{room}/keys"),
        format!("/api/rooms/{room}/keys/versions"),
    ];
    for path in gets {
        outsider
            .api
            .get(&path)
            .await
            .expect_error(403, "Access denied");
    }

    let posts: [(String, Value); 3] = [
        (
            format!("/api/rooms/{room}/messages"),
            json!({ "content": "x", "msgHash": crypto::sha256_hex(b"x") }),
        ),
        (
            format!("/api/rooms/{room}/read"),
            json!({ "lastReadSerial": 1 }),
        ),
        (
            format!("/api/rooms/{room}/rotate-key"),
            json!({ "newVersion": 2, "keys": [] }),
        ),
    ];
    for (path, body) in posts {
        outsider.api.post(&path, body).await.expect_status(403);
    }

    outsider
        .api
        .delete(&format!("/api/rooms/{room}/messages"))
        .await
        .expect_status(403);
}

#[tokio::test]
async fn every_admin_only_endpoint_denies_a_plain_member() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "guarded").await;
    add_member(&alice.api, &bob, &room).await;

    bob.api
        .patch(&format!("/api/rooms/{room}"), json!({ "name": "no" }))
        .await
        .expect_status(403);
    bob.api
        .delete(&format!("/api/rooms/{room}"))
        .await
        .expect_status(403);
    bob.api
        .post(
            &format!("/api/rooms/{room}/kick"),
            json!({ "userAddress": alice.address }),
        )
        .await
        .expect_status(403);
    bob.api
        .post(
            &format!("/api/rooms/{room}/invite"),
            json!({ "userAddress": alice.address }),
        )
        .await
        .expect_status(403);
    bob.api
        .post(
            &format!("/api/rooms/{room}/admins"),
            json!({ "walletAddress": bob.address }),
        )
        .await
        .expect_status(403);
    bob.api
        .delete(&format!("/api/rooms/{room}/admins/{}", alice.address))
        .await
        .expect_status(403);
}
