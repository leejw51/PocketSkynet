//! Users, public-key distribution and blocking. Spec: `docs/API.md` §6.3, §6.4
//! and the exhaustive block-semantics table in §11.

mod common;

use common::*;
use serde_json::json;

// --- search ---------------------------------------------------------------

#[tokio::test]
async fn search_finds_a_user_by_username() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice_searchable").await;
    let _bob = new_user(&server, "bob_other").await;

    let resp = alice.api.get("/api/users/search?q=searchable").await;
    resp.expect_status(200);
    let hits = resp.array();

    assert_eq!(hits.len(), 1, "expected exactly one hit: {hits:?}");
    expect_user_shape(&hits[0]);
    assert_eq!(s(&hits[0], "username"), "alice_searchable");
}

#[tokio::test]
async fn search_finds_a_user_by_wallet_address() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;

    let resp = alice
        .api
        .get(&format!("/api/users/search?q={}", bob.address))
        .await;
    resp.expect_status(200);

    let hits = resp.array();
    assert!(
        hits.iter().any(|u| s(u, "walletAddress") == bob.address),
        "searching a full address must find it: {hits:?}"
    );
}

#[tokio::test]
async fn search_returns_an_empty_array_when_nothing_matches() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    let resp = alice.api.get("/api/users/search?q=nobodyhasthisname").await;
    resp.expect_status(200);

    assert!(resp.array().is_empty());
}

#[tokio::test]
async fn search_requires_a_query_parameter() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    alice
        .api
        .get("/api/users/search")
        .await
        .expect_validation_failed();
    alice
        .api
        .get("/api/users/search?q=")
        .await
        .expect_validation_failed();
}

#[tokio::test]
async fn search_rejects_forbidden_characters_in_the_query() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    for bad in ["%3Cscript%3E", "a%22b", "a%3Bb", "a%60b", "a%5Cb"] {
        alice
            .api
            .get(&format!("/api/users/search?q={bad}"))
            .await
            .expect_validation_failed();
    }
}

#[tokio::test]
async fn search_treats_like_metacharacters_literally() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice_pct").await;

    // `%` is a LIKE wildcard; escaped, it must match nothing rather than
    // returning every user.
    let resp = alice.api.get("/api/users/search?q=%25").await;
    resp.expect_status(200);
    assert!(
        resp.array().is_empty(),
        "an escaped LIKE wildcard must not match every user: {}",
        resp.text
    );
}

#[tokio::test]
async fn search_requires_authentication() {
    let server = TestServer::start().await;
    Api::anonymous(&server.base_url)
        .get("/api/users/search?q=alice")
        .await
        .expect_status(401);
}

// --- lookup ---------------------------------------------------------------

#[tokio::test]
async fn a_user_can_be_looked_up_by_address() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;

    let body = alice
        .api
        .get(&format!("/api/users/{}", bob.address))
        .await
        .expect_ok();

    expect_user_shape(&body);
    assert_eq!(s(&body, "walletAddress"), bob.address);
    assert_eq!(s(&body, "username"), "bob");
}

#[tokio::test]
async fn looking_up_a_user_accepts_a_mixed_case_address() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let mixed = format!("0x{}", bob.address[2..].to_uppercase());

    let body = alice
        .api
        .get(&format!("/api/users/{mixed}"))
        .await
        .expect_ok();

    assert_eq!(s(&body, "walletAddress"), bob.address);
}

#[tokio::test]
async fn looking_up_an_unknown_address_is_a_404() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    alice
        .api
        .get(&format!("/api/users/{}", Signer::random().address()))
        .await
        .expect_error(404, "User not found");
}

#[tokio::test]
async fn looking_up_a_malformed_address_is_a_validation_error() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    alice
        .api
        .get("/api/users/not-an-address")
        .await
        .expect_validation_failed();
    alice
        .api
        .get("/api/users/0x1234")
        .await
        .expect_validation_failed();
}

#[tokio::test]
async fn the_literal_user_routes_win_over_the_address_pattern() {
    // §14.1: `search`, `blocked` and `blocked-by` must not be parsed as an
    // `:address`, which would turn them into 400s.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    alice
        .api
        .get("/api/users/search?q=alice")
        .await
        .expect_status(200);
    alice.api.get("/api/users/blocked").await.expect_status(200);
    alice
        .api
        .get("/api/users/blocked-by")
        .await
        .expect_status(200);
}

#[tokio::test]
async fn blocking_does_not_hide_a_profile_lookup() {
    // §11: `GET /api/users/:address` is deliberately not block-filtered.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    block(&alice, &bob.address).await;

    alice
        .api
        .get(&format!("/api/users/{}", bob.address))
        .await
        .expect_status(200);
    bob.api
        .get(&format!("/api/users/{}", alice.address))
        .await
        .expect_status(200);
}

// --- public keys ----------------------------------------------------------

#[tokio::test]
async fn public_keys_returns_only_users_that_published_one() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let carol = new_user(&server, "carol").await;
    let bob_identity = bob.publish_encryption_key().await;

    let resp = alice
        .api
        .post(
            "/api/users/public-keys",
            json!({ "addresses": [bob.address, carol.address] }),
        )
        .await;
    resp.expect_status(200);
    let entries = resp.array();

    assert_eq!(
        entries.len(),
        1,
        "carol has no key and must be dropped: {entries:?}"
    );
    expect_keys(&entries[0], &["walletAddress", "publicKey", "publicKeySig"]);
    assert_eq!(s(&entries[0], "walletAddress"), bob.address);
    assert_eq!(s(&entries[0], "publicKey"), bob_identity.public_key);
}

#[tokio::test]
async fn public_keys_are_verifiable_against_the_binding_message() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    bob.publish_encryption_key().await;

    let resp = alice
        .api
        .post(
            "/api/users/public-keys",
            json!({ "addresses": [bob.address] }),
        )
        .await;
    let entry = resp.array().remove(0);

    assert!(
        crypto::verify_key_binding(
            &s(&entry, "walletAddress"),
            &s(&entry, "publicKey"),
            &s(&entry, "publicKeySig"),
        ),
        "a client MUST be able to verify the binding before wrapping (CRYPTO §4.3)"
    );
}

#[tokio::test]
async fn public_keys_silently_drops_unknown_addresses() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    bob.publish_encryption_key().await;

    let resp = alice
        .api
        .post(
            "/api/users/public-keys",
            json!({ "addresses": [Signer::random().address(), bob.address] }),
        )
        .await;
    resp.expect_status(200);

    let entries = resp.array();
    assert_eq!(entries.len(), 1);
    assert_eq!(s(&entries[0], "walletAddress"), bob.address);
}

#[tokio::test]
async fn public_keys_rejects_a_malformed_request_with_400() {
    // §15 #5: the reference returns 500 here; PocketSkynet must return the
    // standard validation envelope.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    for body in [
        json!({ "addresses": [] }),
        json!({ "addresses": ["nope"] }),
        json!({ "addresses": "not-an-array" }),
        json!({}),
    ] {
        alice
            .api
            .post("/api/users/public-keys", body)
            .await
            .expect_validation_failed();
    }
}

#[tokio::test]
async fn public_keys_rejects_more_than_fifty_addresses() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let addresses: Vec<String> = (0..51)
        .map(|_| Signer::random().address().to_string())
        .collect();

    alice
        .api
        .post("/api/users/public-keys", json!({ "addresses": addresses }))
        .await
        .expect_validation_failed();
}

#[tokio::test]
async fn public_keys_accepts_exactly_fifty_addresses() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let addresses: Vec<String> = (0..50)
        .map(|_| Signer::random().address().to_string())
        .collect();

    alice
        .api
        .post("/api/users/public-keys", json!({ "addresses": addresses }))
        .await
        .expect_status(200);
}

// --- blocking -------------------------------------------------------------

async fn block(user: &User, target: &str) {
    user.api
        .post("/api/users/block", json!({ "address": target }))
        .await
        .expect_status(200);
}

#[tokio::test]
async fn blocking_returns_the_blocked_user_row() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;

    let body = alice
        .api
        .post("/api/users/block", json!({ "address": bob.address }))
        .await
        .expect_ok();

    expect_keys(&body, &["blockerAddress", "blockedAddress", "createdAt"]);
    assert_eq!(s(&body, "blockerAddress"), alice.address);
    assert_eq!(s(&body, "blockedAddress"), bob.address);
}

#[tokio::test]
async fn the_blocked_list_holds_addresses_not_profiles() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    block(&alice, &bob.address).await;

    let resp = alice.api.get("/api/users/blocked").await;
    resp.expect_status(200);
    let rows = resp.array();

    assert_eq!(rows.len(), 1);
    expect_keys(&rows[0], &["blockerAddress", "blockedAddress", "createdAt"]);
    expect_no_keys(&rows[0], &["username"]);
}

#[tokio::test]
async fn blocking_twice_does_not_duplicate_the_row() {
    // §15 #4: the reference inserts duplicates; PocketSkynet must be idempotent.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;

    block(&alice, &bob.address).await;
    block(&alice, &bob.address).await;

    let rows = alice.api.get("/api/users/blocked").await.array();
    assert_eq!(
        rows.len(),
        1,
        "repeated blocks must be idempotent: {rows:?}"
    );
}

#[tokio::test]
async fn blocking_yourself_is_rejected() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    alice
        .api
        .post("/api/users/block", json!({ "address": alice.address }))
        .await
        .expect_error(400, "Cannot block yourself");
}

#[tokio::test]
async fn blocking_an_unknown_user_is_a_404() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    alice
        .api
        .post(
            "/api/users/block",
            json!({ "address": Signer::random().address() }),
        )
        .await
        .expect_error(404, "User not found");
}

#[tokio::test]
async fn blocking_without_an_address_is_rejected() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    alice
        .api
        .post("/api/users/block", json!({}))
        .await
        .expect_error(400, "Wallet address is required");
}

#[tokio::test]
async fn blocking_with_a_non_string_address_is_rejected() {
    // §6.4.3 check 1 names one message for "missing or not a string"; the
    // wrong-type half is caught by body deserialization instead, so only the
    // status is asserted here. See FINDINGS.md.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    for value in [json!(12345), json!(true), json!(["0x0"]), json!(null)] {
        let resp = alice
            .api
            .post("/api/users/block", json!({ "address": value }))
            .await;
        resp.expect_status(400);
        assert!(
            !resp.message().is_empty(),
            "a rejection must explain itself: {}",
            resp.text
        );
    }
}

#[tokio::test]
async fn blocking_a_malformed_address_is_a_plain_message_not_an_envelope() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    alice
        .api
        .post("/api/users/block", json!({ "address": "0xnope" }))
        .await
        .expect_error(400, "Invalid wallet address format");
}

#[tokio::test]
async fn unblocking_removes_the_row() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    block(&alice, &bob.address).await;

    alice
        .api
        .delete(&format!("/api/users/block/{}", bob.address))
        .await
        .expect_message("User unblocked successfully");

    assert!(alice.api.get("/api/users/blocked").await.array().is_empty());
}

#[tokio::test]
async fn unblocking_someone_never_blocked_still_succeeds() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;

    alice
        .api
        .delete(&format!("/api/users/block/{}", bob.address))
        .await
        .expect_message("User unblocked successfully");
}

#[tokio::test]
async fn unblocking_a_malformed_address_is_rejected() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    alice
        .api
        .delete("/api/users/block/0xnope")
        .await
        .expect_error(400, "Invalid wallet address format");
}

#[tokio::test]
async fn is_blocked_reports_only_my_own_direction() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    block(&alice, &bob.address).await;

    let mine = alice
        .api
        .get(&format!("/api/users/{}/is-blocked", bob.address))
        .await
        .expect_ok();
    assert!(b(&mine, "isBlocked"), "alice blocked bob");

    // The question is "have I blocked them", never "did they block me".
    let theirs = bob
        .api
        .get(&format!("/api/users/{}/is-blocked", alice.address))
        .await
        .expect_ok();
    assert!(!b(&theirs, "isBlocked"), "bob did not block alice");
}

#[tokio::test]
async fn blocked_by_tells_you_who_blocked_you() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    block(&alice, &bob.address).await;

    let rows = bob.api.get("/api/users/blocked-by").await.array();

    assert_eq!(rows.len(), 1);
    assert_eq!(s(&rows[0], "blockerAddress"), alice.address);
    assert_eq!(s(&rows[0], "blockedAddress"), bob.address);
    assert!(
        alice
            .api
            .get("/api/users/blocked-by")
            .await
            .array()
            .is_empty(),
        "nobody blocked alice"
    );
}

// --- §11: what blocking actually filters ---------------------------------

#[tokio::test]
async fn blocking_hides_the_target_from_search_in_both_directions() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice_unique").await;
    let bob = new_user(&server, "bob_unique").await;
    block(&alice, &bob.address).await;

    let alice_sees = alice.api.get("/api/users/search?q=unique").await.array();
    assert!(
        !alice_sees
            .iter()
            .any(|u| s(u, "walletAddress") == bob.address),
        "the blocker must not see the blocked user: {alice_sees:?}"
    );

    let bob_sees = bob.api.get("/api/users/search?q=unique").await.array();
    assert!(
        !bob_sees
            .iter()
            .any(|u| s(u, "walletAddress") == alice.address),
        "invisibility is bidirectional: {bob_sees:?}"
    );
}

#[tokio::test]
async fn blocking_hides_the_blocked_users_messages_from_the_blocker_only() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "shared").await;
    add_member(&alice.api, &bob, &room).await;
    block(&alice, &bob.address).await;

    send_message(&bob.api, &room, "from bob").await;
    send_message(&alice.api, &room, "from alice").await;

    let alice_sees = alice
        .api
        .get(&format!("/api/rooms/{room}/messages"))
        .await
        .array();
    assert!(
        alice_sees
            .iter()
            .all(|m| s(m, "senderAddress") != bob.address),
        "bob's messages must be filtered out of alice's list: {alice_sees:?}"
    );

    // The filter is viewer-side: alice's own messages still reach bob.
    let bob_sees = bob
        .api
        .get(&format!("/api/rooms/{room}/messages"))
        .await
        .array();
    assert!(
        bob_sees
            .iter()
            .any(|m| s(m, "senderAddress") == alice.address),
        "the block is directed, not mutual censorship: {bob_sees:?}"
    );
}

#[tokio::test]
async fn blocking_filters_the_sync_stream_for_the_blocker_only() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "shared").await;
    add_member(&alice.api, &bob, &room).await;
    block(&alice, &bob.address).await;

    send_message(&bob.api, &room, "hidden").await;
    send_message(&alice.api, &room, "visible").await;

    let alice_events = drain_sync(&alice.api, &room).await;
    assert!(
        alice_events
            .iter()
            .all(|e| s(e, "senderAddress") != bob.address),
        "blocked senders' events never reach the blocker: {alice_events:?}"
    );

    let bob_events = drain_sync(&bob.api, &room).await;
    assert!(bob_events
        .iter()
        .any(|e| s(e, "senderAddress") == alice.address));
}

#[tokio::test]
async fn a_blocked_user_can_still_post() {
    // §11: blocking is a read filter, not a write ban.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let carol = new_user(&server, "carol").await;
    let room = create_room(&alice.api, "shared").await;
    add_member(&alice.api, &bob, &room).await;
    add_member(&alice.api, &carol, &room).await;
    block(&alice, &bob.address).await;

    send_message(&bob.api, &room, "still posts").await;

    let carol_sees = carol
        .api
        .get(&format!("/api/rooms/{room}/messages"))
        .await
        .array();
    assert!(
        carol_sees.iter().any(|m| s(m, "content") == "still posts"),
        "a third party is unaffected by alice's block: {carol_sees:?}"
    );
}

#[tokio::test]
async fn blocking_does_not_filter_the_member_roster() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "shared").await;
    add_member(&alice.api, &bob, &room).await;
    block(&alice, &bob.address).await;

    let members = alice
        .api
        .get(&format!("/api/rooms/{room}/members"))
        .await
        .array();

    assert!(
        members.iter().any(|m| s(m, "userAddress") == bob.address),
        "§11: the roster is not block-filtered: {members:?}"
    );
}

#[tokio::test]
async fn blocking_does_not_remove_either_party_from_a_shared_room() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "shared").await;
    add_member(&alice.api, &bob, &room).await;
    block(&alice, &bob.address).await;

    assert!(room_in_list(&alice.api, &room).await.is_some());
    assert!(room_in_list(&bob.api, &room).await.is_some());
    bob.api
        .get(&format!("/api/rooms/{room}"))
        .await
        .expect_status(200);
}

#[tokio::test]
async fn the_unread_count_excludes_blocked_senders() {
    // §15 #9: the reference inflates the badge with messages the blocker can
    // never fetch. PocketSkynet must exclude them.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let carol = new_user(&server, "carol").await;
    let room = create_room(&alice.api, "shared").await;
    add_member(&alice.api, &bob, &room).await;
    add_member(&alice.api, &carol, &room).await;
    block(&alice, &bob.address).await;

    send_message(&bob.api, &room, "blocked one").await;
    send_message(&bob.api, &room, "blocked two").await;
    send_message(&carol.api, &room, "visible").await;

    let entry = room_in_list(&alice.api, &room).await.expect("room in list");
    assert_eq!(
        i(&entry, "unreadCount"),
        1,
        "only carol's message may count as unread: {entry}"
    );
}

#[tokio::test]
async fn emoticon_aggregation_excludes_blocked_reactors() {
    // §15 #10: keep every read surface consistent with /sync.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "shared").await;
    add_member(&alice.api, &bob, &room).await;
    let message = send_message(&alice.api, &room, "react to me").await;
    let message_id = s(&message, "id");

    bob.api
        .post(
            &format!("/api/messages/{message_id}/emoticons"),
            json!({ "emoticonCode": "🍎" }),
        )
        .await
        .expect_status(200);
    block(&alice, &bob.address).await;

    let aggregated = alice
        .api
        .get(&format!("/api/messages/{message_id}/emoticons"))
        .await
        .array();

    let reactors: Vec<String> = aggregated
        .iter()
        .filter_map(|a| a["users"].as_array())
        .flatten()
        .map(|u| s(u, "walletAddress"))
        .collect();
    assert!(
        !reactors.contains(&bob.address),
        "a blocked reactor must not appear in the blocker's aggregation: {aggregated:?}"
    );
}

#[tokio::test]
async fn blocking_gates_invitations_in_both_directions() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let alice_room = create_room(&alice.api, "alice room").await;
    let bob_room = create_room(&bob.api, "bob room").await;
    block(&alice, &bob.address).await;

    alice
        .api
        .post(
            &format!("/api/rooms/{alice_room}/invite"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_error(403, "You cannot invite users you have blocked");

    bob.api
        .post(
            &format!("/api/rooms/{bob_room}/invite"),
            json!({ "userAddress": alice.address }),
        )
        .await
        .expect_error(403, "You cannot invite users who have blocked you");
}

#[tokio::test]
async fn unblocking_restores_visibility_everywhere() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice_vis").await;
    let bob = new_user(&server, "bob_vis").await;
    let room = create_room(&alice.api, "shared").await;
    add_member(&alice.api, &bob, &room).await;
    send_message(&bob.api, &room, "before block").await;

    block(&alice, &bob.address).await;
    assert!(alice
        .api
        .get(&format!("/api/rooms/{room}/messages"))
        .await
        .array()
        .is_empty());

    alice
        .api
        .delete(&format!("/api/users/block/{}", bob.address))
        .await
        .expect_status(200);

    let visible = alice
        .api
        .get(&format!("/api/rooms/{room}/messages"))
        .await
        .array();
    assert_eq!(visible.len(), 1, "unblocking restores history: {visible:?}");
    let found = alice.api.get("/api/users/search?q=vis").await.array();
    assert!(found.iter().any(|u| s(u, "walletAddress") == bob.address));
}
