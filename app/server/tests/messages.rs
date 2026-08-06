//! Messages: send/list/edit/delete, serial monotonicity, `/sync` semantics,
//! read markers, unread counts, emoticons, and the §9 event fold.
//! Spec: `docs/API.md` §6.10, §6.11, §6.12, §8, §9, §13.

mod common;

use common::*;
use serde_json::{json, Value};

// --- sending --------------------------------------------------------------

#[tokio::test]
async fn a_sent_message_comes_back_with_its_sender() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "chat").await;

    let msg = send_message(&alice.api, &room, "Hello everyone!").await;

    expect_message_shape(&msg);
    expect_user_shape(&msg["sender"]);
    assert_eq!(s(&msg, "roomId"), room);
    assert_eq!(s(&msg, "senderAddress"), alice.address);
    assert_eq!(s(&msg, "content"), "Hello everyone!");
    assert_eq!(s(&msg, "msgType"), "add", "createMessage hard-codes `add`");
    assert!(!b(&msg, "isDeleted"));
    assert!(!b(&msg, "isEncrypted"));
    assert!(msg["editedAt"].is_null());
    assert!(msg["txHash"].is_null());
    assert!(msg["targetMessageId"].is_null());
    assert!(msg["emoticonCode"].is_null());
    assert_eq!(i(&msg, "encVer"), 1);
    assert_eq!(i(&msg, "keyVersion"), 1);
    assert!(msg["id"].as_str().is_some_and(|id| id.starts_with("msg_")));
}

#[tokio::test]
async fn the_sender_address_comes_from_the_token_not_the_body() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let mallory = Signer::random();
    let room = create_room(&alice.api, "spoofing").await;

    let msg = alice
        .api
        .post(
            &format!("/api/rooms/{room}/messages"),
            json!({
                "content": "spoofed?",
                "msgHash": crypto::sha256_hex(b"spoofed?"),
                "senderAddress": mallory.address(),
            }),
        )
        .await
        .expect_ok();

    assert_eq!(s(&msg, "senderAddress"), alice.address);
}

#[tokio::test]
async fn sending_requires_content_and_a_message_hash() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "strict").await;

    for body in [
        json!({}),
        json!({ "content": "no hash" }),
        json!({ "msgHash": crypto::sha256_hex(b"no content") }),
        json!({ "content": "", "msgHash": crypto::sha256_hex(b"") }),
    ] {
        alice
            .api
            .post(&format!("/api/rooms/{room}/messages"), body)
            .await
            .expect_validation_failed();
    }
}

#[tokio::test]
async fn the_message_hash_must_be_lowercase_hex() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "hashes").await;

    for hash in [
        crypto::sha256_hex(b"x").to_uppercase(),
        "not-a-hash".to_string(),
        "ab".repeat(31),
    ] {
        alice
            .api
            .post(
                &format!("/api/rooms/{room}/messages"),
                json!({ "content": "x", "msgHash": hash }),
            )
            .await
            .expect_validation_failed();
    }
}

#[tokio::test]
async fn content_longer_than_five_thousand_characters_is_rejected() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "limits").await;
    let long = "x".repeat(5001);

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/messages"),
            json!({ "content": long, "msgHash": crypto::sha256_hex(long.as_bytes()) }),
        )
        .await
        .expect_validation_failed();
}

#[tokio::test]
async fn content_of_exactly_five_thousand_characters_is_accepted() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "limits").await;
    let content = "x".repeat(5000);

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/messages"),
            json!({ "content": content, "msgHash": crypto::sha256_hex(content.as_bytes()) }),
        )
        .await
        .expect_status(200);
}

#[tokio::test]
async fn whitespace_only_content_is_rejected() {
    // §15 #20: trim *before* the length check, so "   " does not become "".
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "trimming").await;

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/messages"),
            json!({ "content": "     ", "msgHash": crypto::sha256_hex(b"") }),
        )
        .await
        .expect_validation_failed();
}

#[tokio::test]
async fn sending_requires_membership() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let outsider = new_user(&server, "outsider").await;
    let room = create_room(&alice.api, "closed").await;

    try_send_message(&outsider.api, &room, "let me in")
        .await
        .expect_error(403, "Access denied");
}

#[tokio::test]
async fn unicode_content_round_trips_intact() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "unicode").await;

    for content in [
        "한국어 메시지",
        "日本語のメッセージ",
        "🍎🍇🍊 fruit",
        "mixed 混合 🍏",
    ] {
        let msg = send_message(&alice.api, &room, content).await;
        assert_eq!(s(&msg, "content"), content);
    }
}

// --- serials --------------------------------------------------------------

#[tokio::test]
async fn message_serials_increase_strictly_within_a_room() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "serials").await;

    let mut previous = 0i64;
    for n in 0..25 {
        let msg = send_message(&alice.api, &room, &format!("message {n}")).await;
        let serial = i(&msg, "msgSerial");
        assert!(
            serial > previous,
            "serial {serial} did not exceed the previous {previous} (message {n})"
        );
        previous = serial;
    }
}

#[tokio::test]
async fn concurrent_sends_never_collide_on_a_serial() {
    // §15 #2: the reference derives serials from an in-process map, so two
    // writers in one millisecond can produce a duplicate — which makes a
    // `msg_serial > since` client silently *skip* a message.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "racing").await;

    let mut tasks = Vec::new();
    for n in 0..20 {
        let api = alice.api.clone();
        let room = room.clone();
        tasks.push(tokio::spawn(async move {
            let msg = send_message(&api, &room, &format!("concurrent {n}")).await;
            i(&msg, "msgSerial")
        }));
    }
    let mut serials = Vec::new();
    for task in tasks {
        serials.push(task.await.expect("send task"));
    }

    let unique: std::collections::BTreeSet<_> = serials.iter().collect();
    assert_eq!(
        unique.len(),
        serials.len(),
        "duplicate serials would make /sync drop a message: {serials:?}"
    );
}

#[tokio::test]
async fn every_mutation_advances_the_serial() {
    // §8.1: edits, deletes and reactions all bump the row's serial so a
    // /sync client observes them.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "mutations").await;
    let msg = send_message(&alice.api, &room, "original").await;
    let id = s(&msg, "id");
    let sent = i(&msg, "msgSerial");

    let edited = alice
        .api
        .patch(
            &format!("/api/messages/{id}"),
            json!({ "content": "edited", "msgHash": crypto::sha256_hex(b"edited") }),
        )
        .await
        .expect_ok();
    let edit_serial = i(&edited, "msgSerial");
    assert!(edit_serial > sent, "an edit must advance the serial");

    alice
        .api
        .post(
            &format!("/api/messages/{id}/emoticons"),
            json!({ "emoticonCode": "🍎" }),
        )
        .await
        .expect_status(200);
    assert!(latest_serial(&alice.api, &room).await > edit_serial);
}

#[tokio::test]
async fn latest_serial_is_zero_for_an_empty_room() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "empty").await;

    let body = alice
        .api
        .get(&format!("/api/rooms/{room}/latest-serial"))
        .await
        .expect_ok();

    assert_eq!(i(&body, "serial"), 0);
}

#[tokio::test]
async fn latest_serial_tracks_the_newest_message() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "tracking").await;
    send_message(&alice.api, &room, "first").await;
    let newest = send_message(&alice.api, &room, "second").await;

    assert_eq!(
        latest_serial(&alice.api, &room).await,
        i(&newest, "msgSerial")
    );
}

#[tokio::test]
async fn latest_serial_is_not_block_filtered() {
    // §11: it is a change detector, not a read cursor.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "shared").await;
    add_member(&alice.api, &bob, &room).await;
    alice
        .api
        .post("/api/users/block", json!({ "address": bob.address }))
        .await
        .expect_status(200);
    let hidden = send_message(&bob.api, &room, "from a blocked sender").await;

    assert_eq!(
        latest_serial(&alice.api, &room).await,
        i(&hidden, "msgSerial"),
        "the serial of a filtered row is still the room's latest"
    );
}

#[tokio::test]
async fn latest_timestamp_reports_the_newest_message_time() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "timestamps").await;

    let empty = alice
        .api
        .get(&format!("/api/rooms/{room}/latest-timestamp"))
        .await
        .expect_ok();
    assert_eq!(i(&empty, "timestamp"), 0);

    let msg = send_message(&alice.api, &room, "now").await;
    let body = alice
        .api
        .get(&format!("/api/rooms/{room}/latest-timestamp"))
        .await
        .expect_ok();
    assert_eq!(i(&body, "timestamp"), i(&msg, "messageTimestamp"));
}

// --- listing --------------------------------------------------------------

#[tokio::test]
async fn messages_are_listed_chronologically_ascending() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ordering").await;
    for n in 0..5 {
        send_message(&alice.api, &room, &format!("message {n}")).await;
    }

    let listed = alice
        .api
        .get(&format!("/api/rooms/{room}/messages"))
        .await
        .array();

    assert_eq!(listed.len(), 5);
    // §8.4: ordered by `messageTimestamp` ASC. That clock is millisecond
    // resolution, so two messages sent in the same millisecond legitimately
    // tie — §9 leaves breaking those ties to the client. Assert the ordering
    // the server owes, not an incidental one.
    let timestamps: Vec<i64> = listed.iter().map(|m| i(m, "messageTimestamp")).collect();
    assert!(
        timestamps.windows(2).all(|w| w[0] <= w[1]),
        "timestamps must be non-decreasing: {timestamps:?}"
    );
    let mut contents: Vec<String> = listed.iter().map(|m| s(m, "content")).collect();
    contents.sort();
    assert_eq!(
        contents,
        vec![
            "message 0",
            "message 1",
            "message 2",
            "message 3",
            "message 4"
        ]
    );
    for msg in &listed {
        expect_message_shape(msg);
        expect_user_shape(&msg["sender"]);
    }
}

#[tokio::test]
async fn messages_sent_apart_in_time_keep_their_order() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ordering").await;
    // Distinct milliseconds, so the ordering is total and the exact sequence
    // is the server's to get right.
    for n in 0..4 {
        send_message(&alice.api, &room, &format!("message {n}")).await;
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    }

    let listed = alice
        .api
        .get(&format!("/api/rooms/{room}/messages"))
        .await
        .array();

    let contents: Vec<String> = listed.iter().map(|m| s(m, "content")).collect();
    assert_eq!(
        contents,
        vec!["message 0", "message 1", "message 2", "message 3"]
    );
}

#[tokio::test]
async fn the_message_limit_is_clamped_to_one_hundred() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "paging").await;
    for n in 0..12 {
        send_message(&alice.api, &room, &format!("message {n}")).await;
    }

    let five = alice
        .api
        .get(&format!("/api/rooms/{room}/messages?limit=5"))
        .await
        .array();
    assert_eq!(five.len(), 5);

    // Over the cap and garbage both fall back rather than 400.
    let over = alice
        .api
        .get(&format!("/api/rooms/{room}/messages?limit=1000"))
        .await
        .array();
    assert_eq!(over.len(), 12);
    let garbage = alice
        .api
        .get(&format!("/api/rooms/{room}/messages?limit=abc"))
        .await
        .array();
    assert_eq!(garbage.len(), 12);
}

#[tokio::test]
async fn backward_pagination_uses_the_before_timestamp() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "paging").await;
    for n in 0..6 {
        send_message(&alice.api, &room, &format!("message {n}")).await;
    }

    let newest = alice
        .api
        .get(&format!("/api/rooms/{room}/messages?limit=3"))
        .await
        .array();
    let oldest_shown = i(&newest[0], "messageTimestamp");

    let older = alice
        .api
        .get(&format!(
            "/api/rooms/{room}/messages?before={oldest_shown}&limit=3"
        ))
        .await
        .array();

    assert!(!older.is_empty(), "there are older messages to page into");
    for msg in &older {
        assert!(
            i(msg, "messageTimestamp") < oldest_shown,
            "`before` is strictly exclusive: {msg}"
        );
    }
}

#[tokio::test]
async fn a_garbage_since_parameter_disables_the_filter_rather_than_erroring() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "lenient").await;
    send_message(&alice.api, &room, "visible").await;

    let listed = alice
        .api
        .get(&format!("/api/rooms/{room}/messages?since=abc"))
        .await
        .array();

    assert_eq!(
        listed.len(),
        1,
        "a parsed 0 means `no filter`, not `since epoch`"
    );
}

#[tokio::test]
async fn the_message_list_excludes_deleted_rows_and_events() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "filtered").await;
    let kept = send_message(&alice.api, &room, "kept").await;
    let removed = send_message(&alice.api, &room, "removed").await;
    alice
        .api
        .delete(&format!("/api/messages/{}", s(&removed, "id")))
        .await
        .expect_status(200);
    alice
        .api
        .post(
            &format!("/api/messages/{}/emoticons", s(&kept, "id")),
            json!({ "emoticonCode": "🍎" }),
        )
        .await
        .expect_status(200);

    let listed = alice
        .api
        .get(&format!("/api/rooms/{room}/messages"))
        .await
        .array();

    assert_eq!(listed.len(), 1, "only the surviving `add` row: {listed:?}");
    assert_eq!(s(&listed[0], "id"), s(&kept, "id"));
}

#[tokio::test]
async fn a_page_is_full_even_when_events_are_interleaved() {
    // §15 #8: the reference applies LIMIT before dropping emoticon rows, so a
    // page can come back short while older messages still exist.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "interleaved").await;
    let mut ids = Vec::new();
    for n in 0..10 {
        ids.push(s(
            &send_message(&alice.api, &room, &format!("message {n}")).await,
            "id",
        ));
    }
    // Ten reaction events on top of ten messages.
    for id in &ids {
        alice
            .api
            .post(
                &format!("/api/messages/{id}/emoticons"),
                json!({ "emoticonCode": "🍎" }),
            )
            .await
            .expect_status(200);
    }

    let page = alice
        .api
        .get(&format!("/api/rooms/{room}/messages?limit=5"))
        .await
        .array();

    assert_eq!(
        page.len(),
        5,
        "the type filter must run in SQL so pages are full: {page:?}"
    );
}

// --- editing --------------------------------------------------------------

#[tokio::test]
async fn the_owner_can_edit_a_message_in_place() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "edits").await;
    let original = send_message(&alice.api, &room, "before").await;
    let id = s(&original, "id");

    let edited = alice
        .api
        .patch(
            &format!("/api/messages/{id}"),
            json!({ "content": "after", "msgHash": crypto::sha256_hex(b"after") }),
        )
        .await
        .expect_ok();

    expect_message_shape(&edited);
    assert_eq!(s(&edited, "id"), id, "an edit keeps the row's id");
    assert_eq!(s(&edited, "content"), "after");
    assert_eq!(s(&edited, "msgType"), "edit");
    assert!(
        !edited["editedAt"].is_null(),
        "editedAt must be set: {edited}"
    );
    assert_eq!(
        i(&edited, "messageTimestamp"),
        i(&original, "messageTimestamp"),
        "an edit keeps its original display timestamp"
    );
}

#[tokio::test]
async fn only_the_owner_can_edit() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "edits").await;
    add_member(&alice.api, &bob, &room).await;
    let msg = send_message(&alice.api, &room, "alice's words").await;

    bob.api
        .patch(
            &format!("/api/messages/{}", s(&msg, "id")),
            json!({ "content": "bob's words", "msgHash": crypto::sha256_hex(b"bob's words") }),
        )
        .await
        .expect_error(403, "Only the message owner can edit this message");
}

#[tokio::test]
async fn a_non_member_cannot_edit() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let outsider = new_user(&server, "outsider").await;
    let room = create_room(&alice.api, "closed").await;
    let msg = send_message(&alice.api, &room, "private").await;

    outsider
        .api
        .patch(
            &format!("/api/messages/{}", s(&msg, "id")),
            json!({ "content": "hijacked", "msgHash": crypto::sha256_hex(b"hijacked") }),
        )
        .await
        .expect_error(403, "Not a member of this room");
}

#[tokio::test]
async fn editing_an_unknown_or_deleted_message_is_a_404() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "edits").await;
    let msg = send_message(&alice.api, &room, "doomed").await;
    let id = s(&msg, "id");
    alice
        .api
        .delete(&format!("/api/messages/{id}"))
        .await
        .expect_status(200);

    let body = json!({ "content": "x", "msgHash": crypto::sha256_hex(b"x") });
    alice
        .api
        .patch(&format!("/api/messages/{id}"), body.clone())
        .await
        .expect_status(404);
    alice
        .api
        .patch("/api/messages/msg_0000000000_deadbeef", body)
        .await
        .expect_status(404);
}

#[tokio::test]
async fn editing_validates_the_new_content() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "edits").await;
    let msg = send_message(&alice.api, &room, "original").await;
    let id = s(&msg, "id");

    alice
        .api
        .patch(&format!("/api/messages/{id}"), json!({}))
        .await
        .expect_validation_failed();
    alice
        .api
        .patch(
            &format!("/api/messages/{id}"),
            json!({ "content": "x", "msgHash": "NOT-HEX" }),
        )
        .await
        .expect_validation_failed();
}

// --- deleting -------------------------------------------------------------

#[tokio::test]
async fn any_member_can_delete_any_message() {
    // §6.10.4: "forgetting-first" privacy — deliberate, not a bug.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "forgetting").await;
    add_member(&alice.api, &bob, &room).await;
    let msg = send_message(&alice.api, &room, "alice's words").await;

    bob.api
        .delete(&format!("/api/messages/{}", s(&msg, "id")))
        .await
        .expect_message("Message deleted successfully");

    assert!(alice
        .api
        .get(&format!("/api/rooms/{room}/messages"))
        .await
        .array()
        .is_empty());
}

#[tokio::test]
async fn a_deleted_message_is_scrubbed_but_still_syncs() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "tombstones").await;
    let msg = send_message(&alice.api, &room, "secret text").await;
    let id = s(&msg, "id");
    alice
        .api
        .delete(&format!("/api/messages/{id}"))
        .await
        .expect_status(200);

    let events = drain_sync(&alice.api, &room).await;

    let tombstone = events
        .iter()
        .find(|e| s(e, "id") == id)
        .unwrap_or_else(|| panic!("the delete must be delivered over /sync: {events:?}"));
    assert_eq!(s(tombstone, "msgType"), "delete");
    assert!(b(tombstone, "isDeleted"));
    assert_eq!(s(tombstone, "content"), "", "content is scrubbed");
    assert_eq!(s(tombstone, "msgHash"), "");
    assert!(tombstone["iv"].is_null());
    assert!(tombstone["hmac"].is_null());
    assert_eq!(
        s(tombstone, "senderAddress"),
        alice.address,
        "the sender is retained"
    );
}

#[tokio::test]
async fn a_non_member_cannot_delete() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let outsider = new_user(&server, "outsider").await;
    let room = create_room(&alice.api, "closed").await;
    let msg = send_message(&alice.api, &room, "private").await;

    outsider
        .api
        .delete(&format!("/api/messages/{}", s(&msg, "id")))
        .await
        .expect_error(403, "Not a member of this room");
}

#[tokio::test]
async fn deleting_a_message_twice_is_a_404() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "idempotence").await;
    let msg = send_message(&alice.api, &room, "once").await;
    let id = s(&msg, "id");

    alice
        .api
        .delete(&format!("/api/messages/{id}"))
        .await
        .expect_status(200);
    alice
        .api
        .delete(&format!("/api/messages/{id}"))
        .await
        .expect_status(404);
}

#[tokio::test]
async fn delete_all_purges_history_and_leaves_a_marker() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "purge").await;
    for n in 0..4 {
        send_message(&alice.api, &room, &format!("message {n}")).await;
    }

    let body = alice
        .api
        .delete(&format!("/api/rooms/{room}/messages"))
        .await
        .expect_ok();

    assert_eq!(s(&body, "message"), "All messages deleted successfully");
    assert_eq!(i(&body, "deletedCount"), 4);
    assert!(alice
        .api
        .get(&format!("/api/rooms/{room}/messages"))
        .await
        .array()
        .is_empty());

    let events = drain_sync(&alice.api, &room).await;
    assert_eq!(events.len(), 1, "only the marker survives: {events:?}");
    assert_eq!(s(&events[0], "msgType"), "delete_all");
    assert_eq!(s(&events[0], "content"), "");
    assert_eq!(s(&events[0], "senderAddress"), alice.address);
    assert!(!b(&events[0], "isDeleted"));
}

#[tokio::test]
async fn serials_stay_monotonic_across_a_purge() {
    // §8.3: after the table is emptied, `latestSerial` reads back as 0, so
    // monotonicity rests on the persisted counter.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "purge").await;
    let before = i(
        &send_message(&alice.api, &room, "before").await,
        "msgSerial",
    );

    alice
        .api
        .delete(&format!("/api/rooms/{room}/messages"))
        .await
        .expect_status(200);
    let after = i(&send_message(&alice.api, &room, "after").await, "msgSerial");

    assert!(
        after > before,
        "a purge must not let serials regress: {after} <= {before}"
    );
}

#[tokio::test]
async fn only_an_admin_can_purge_a_rooms_history() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let outsider = new_user(&server, "outsider").await;
    let room = create_room(&alice.api, "purge").await;
    add_member(&alice.api, &bob, &room).await;
    send_message(&alice.api, &room, "history").await;

    outsider
        .api
        .delete(&format!("/api/rooms/{room}/messages"))
        .await
        .expect_status(403);
    // Bob is a member, and may delete any single message in the room. Erasing
    // the whole history is a different act — it destroys everybody's record in
    // one request, with no undo — so it belongs to the role the room already
    // has for irreversible things.
    bob.api
        .delete(&format!("/api/rooms/{room}/messages"))
        .await
        .expect_error(403, "Only room admins can delete a room's entire history");
    alice
        .api
        .delete(&format!("/api/rooms/{room}/messages"))
        .await
        .expect_status(200);
}

// --- sync -----------------------------------------------------------------

#[tokio::test]
async fn sync_from_zero_returns_everything_with_has_more_false() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "sync").await;
    for n in 0..3 {
        send_message(&alice.api, &room, &format!("message {n}")).await;
    }

    let (events, has_more) = sync(&alice.api, &room, 0).await;

    assert_eq!(events.len(), 3);
    assert!(!has_more, "three messages fit well inside one page");
    for event in &events {
        expect_message_shape(event);
        expect_user_shape(&event["sender"]);
    }
}

#[tokio::test]
async fn sync_is_ordered_by_serial_and_the_cursor_is_exclusive() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "cursor").await;
    let first = send_message(&alice.api, &room, "first").await;
    let second = send_message(&alice.api, &room, "second").await;

    let (events, _) = sync(&alice.api, &room, i(&first, "msgSerial")).await;

    assert_eq!(events.len(), 1, "`since` is strictly greater: {events:?}");
    assert_eq!(s(&events[0], "id"), s(&second, "id"));

    let (none, _) = sync(&alice.api, &room, i(&second, "msgSerial")).await;
    assert!(none.is_empty());
}

#[tokio::test]
async fn sync_serials_ascend() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ordered").await;
    for n in 0..8 {
        send_message(&alice.api, &room, &format!("message {n}")).await;
    }

    let events = drain_sync(&alice.api, &room).await;

    let serials: Vec<i64> = events.iter().map(|e| i(e, "msgSerial")).collect();
    let mut sorted = serials.clone();
    sorted.sort_unstable();
    assert_eq!(serials, sorted, "/sync must be ordered by msgSerial ASC");
}

#[tokio::test]
async fn sync_delivers_events_that_the_message_list_hides() {
    // §8.4: this asymmetry is what makes incremental folding correct.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "events").await;
    let kept = send_message(&alice.api, &room, "kept").await;
    let doomed = send_message(&alice.api, &room, "doomed").await;
    alice
        .api
        .delete(&format!("/api/messages/{}", s(&doomed, "id")))
        .await
        .expect_status(200);
    alice
        .api
        .post(
            &format!("/api/messages/{}/emoticons", s(&kept, "id")),
            json!({ "emoticonCode": "🍎" }),
        )
        .await
        .expect_status(200);

    let events = drain_sync(&alice.api, &room).await;

    let types: Vec<String> = events.iter().map(|e| s(e, "msgType")).collect();
    assert!(types.contains(&"delete".to_string()), "{types:?}");
    assert!(types.contains(&"emoticon_add".to_string()), "{types:?}");
}

#[tokio::test]
async fn the_has_more_header_is_exposed_to_cross_origin_clients() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "cors").await;

    let resp = alice
        .api
        .get_with_origin(
            &format!("/api/rooms/{room}/sync?since=0"),
            "http://localhost:5173",
        )
        .await;

    resp.expect_status(200);
    let exposed = resp
        .header("access-control-expose-headers")
        .unwrap_or_default();
    assert!(
        exposed.to_ascii_lowercase().contains("x-has-more"),
        "X-Has-More must be CORS-exposed or browsers cannot read it: `{exposed}`"
    );
}

#[tokio::test]
async fn sync_pages_at_five_hundred_and_the_drain_loop_terminates() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "big").await;
    for n in 0..520 {
        send_message(&alice.api, &room, &format!("message {n}")).await;
    }

    let (first_page, has_more) = sync(&alice.api, &room, 0).await;

    assert_eq!(first_page.len(), 500, "SYNC_MESSAGE_LIMIT is 500");
    assert!(has_more, "20 more rows remain");

    let all = drain_sync(&alice.api, &room).await;
    assert_eq!(all.len(), 520, "the drain loop must reach the end");
}

#[tokio::test]
async fn sync_clamps_a_garbage_cursor_to_zero() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "lenient").await;
    send_message(&alice.api, &room, "visible").await;

    for cursor in ["abc", "-1", "99999999999999999999999"] {
        let resp = alice
            .api
            .get(&format!("/api/rooms/{room}/sync?since={cursor}"))
            .await;
        resp.expect_status(200);
        assert_eq!(
            resp.array().len(),
            1,
            "an unparseable cursor falls back to 0, it does not 400 ({cursor})"
        );
    }
}

// --- read state -----------------------------------------------------------

#[tokio::test]
async fn marking_a_room_read_returns_the_stored_pointer() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "reads").await;
    let msg = send_message(&alice.api, &room, "read me").await;
    let serial = i(&msg, "msgSerial");

    let body = alice
        .api
        .post(
            &format!("/api/rooms/{room}/read"),
            json!({ "lastReadSerial": serial }),
        )
        .await
        .expect_ok();

    // §6.12.4: exactly two fields, not the full row.
    assert_eq!(s(&body, "roomId"), room);
    assert_eq!(i(&body, "lastReadSerial"), serial);
    assert_eq!(body.as_object().map(|o| o.len()), Some(2), "{body}");
}

#[tokio::test]
async fn the_read_pointer_never_moves_backwards() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "monotonic").await;
    let msg = send_message(&alice.api, &room, "read me").await;
    let serial = i(&msg, "msgSerial");
    alice
        .api
        .post(
            &format!("/api/rooms/{room}/read"),
            json!({ "lastReadSerial": serial }),
        )
        .await
        .expect_status(200);

    let body = alice
        .api
        .post(
            &format!("/api/rooms/{room}/read"),
            json!({ "lastReadSerial": 1 }),
        )
        .await
        .expect_ok();

    assert_eq!(
        i(&body, "lastReadSerial"),
        serial,
        "a lower serial is a no-op that returns the stored value"
    );
}

#[tokio::test]
async fn an_out_of_range_read_serial_is_rejected() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "reads").await;

    for value in [json!(-1), json!(9007199254740992i64), json!("not a number")] {
        alice
            .api
            .post(
                &format!("/api/rooms/{room}/read"),
                json!({ "lastReadSerial": value }),
            )
            .await
            .expect_validation_failed();
    }
    alice
        .api
        .post(&format!("/api/rooms/{room}/read"), json!({}))
        .await
        .expect_validation_failed();
}

#[tokio::test]
async fn the_unread_count_reflects_other_peoples_messages() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "unread").await;
    add_member(&alice.api, &bob, &room).await;

    send_message(&bob.api, &room, "one").await;
    let two = send_message(&bob.api, &room, "two").await;
    send_message(&alice.api, &room, "mine does not count").await;

    let entry = room_in_list(&alice.api, &room).await.expect("room in list");
    assert_eq!(i(&entry, "unreadCount"), 2, "{entry}");

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/read"),
            json!({ "lastReadSerial": i(&two, "msgSerial") }),
        )
        .await
        .expect_status(200);

    let entry = room_in_list(&alice.api, &room).await.expect("room in list");
    assert_eq!(i(&entry, "unreadCount"), 0);
    assert_eq!(i(&entry, "lastReadSerial"), i(&two, "msgSerial"));
}

#[tokio::test]
async fn only_add_rows_create_unread_badges() {
    // §13: edits, deletes, delete_all and reactions never count.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "badges").await;
    add_member(&alice.api, &bob, &room).await;
    let msg = send_message(&bob.api, &room, "counted once").await;
    alice
        .api
        .post(
            &format!("/api/rooms/{room}/read"),
            json!({ "lastReadSerial": i(&msg, "msgSerial") }),
        )
        .await
        .expect_status(200);

    bob.api
        .patch(
            &format!("/api/messages/{}", s(&msg, "id")),
            json!({ "content": "edited", "msgHash": crypto::sha256_hex(b"edited") }),
        )
        .await
        .expect_status(200);
    bob.api
        .post(
            &format!("/api/messages/{}/emoticons", s(&msg, "id")),
            json!({ "emoticonCode": "🍎" }),
        )
        .await
        .expect_status(200);

    let entry = room_in_list(&alice.api, &room).await.expect("room in list");
    assert_eq!(
        i(&entry, "unreadCount"),
        0,
        "an edit or a reaction must not raise a badge: {entry}"
    );
}

#[tokio::test]
async fn leaving_a_room_clears_the_read_pointer() {
    // §13: the read row (and the hidden-room row) go with the membership.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "reads").await;
    add_member(&alice.api, &bob, &room).await;
    let msg = send_message(&alice.api, &room, "read me").await;
    bob.api
        .post(
            &format!("/api/rooms/{room}/read"),
            json!({ "lastReadSerial": i(&msg, "msgSerial") }),
        )
        .await
        .expect_status(200);

    bob.api
        .post_empty(&format!("/api/rooms/{room}/leave"))
        .await
        .expect_status(200);
    add_member(&alice.api, &bob, &room).await;

    let entry = room_in_list(&bob.api, &room).await.expect("room in list");
    assert_eq!(
        i(&entry, "lastReadSerial"),
        0,
        "the read row was deleted: {entry}"
    );
}

// --- emoticons ------------------------------------------------------------

#[tokio::test]
async fn adding_an_emoticon_creates_an_event_row() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "reactions").await;
    let msg = send_message(&alice.api, &room, "react to me").await;
    let target = s(&msg, "id");

    let event = alice
        .api
        .post(
            &format!("/api/messages/{target}/emoticons"),
            json!({ "emoticonCode": "🍎" }),
        )
        .await
        .expect_ok();

    expect_message_shape(&event);
    // §6.11.1: the created row, without a `sender`.
    expect_no_keys(&event, &["sender"]);
    assert_eq!(s(&event, "msgType"), "emoticon_add");
    assert_eq!(
        s(&event, "roomId"),
        room,
        "the event inherits the target's room"
    );
    assert_eq!(s(&event, "targetMessageId"), target);
    assert_eq!(s(&event, "emoticonCode"), "🍎");
    assert_eq!(s(&event, "content"), "");
    assert_eq!(s(&event, "senderAddress"), alice.address);
    assert!(event["id"]
        .as_str()
        .is_some_and(|id| id.starts_with("emoticon_")));
}

#[tokio::test]
async fn the_emoticon_event_hash_matches_the_specified_preimage() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "hashes").await;
    let msg = send_message(&alice.api, &room, "react to me").await;
    let target = s(&msg, "id");

    let event = alice
        .api
        .post(
            &format!("/api/messages/{target}/emoticons"),
            json!({ "emoticonCode": "🍎" }),
        )
        .await
        .expect_ok();

    // CRYPTO §10.4: "{messageId}:{code}:{add|remove}:{sender}:{timestampMs}".
    assert_eq!(
        s(&event, "msgHash"),
        crypto::emoticon_hash(
            &target,
            "🍎",
            "add",
            &alice.address,
            i(&event, "messageTimestamp")
        )
    );
}

#[tokio::test]
async fn emoticons_aggregate_into_reactor_sets() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "reactions").await;
    add_member(&alice.api, &bob, &room).await;
    let msg = send_message(&alice.api, &room, "react to me").await;
    let target = s(&msg, "id");

    for user in [&alice, &bob] {
        user.api
            .post(
                &format!("/api/messages/{target}/emoticons"),
                json!({ "emoticonCode": "🍎" }),
            )
            .await
            .expect_status(200);
    }
    bob.api
        .post(
            &format!("/api/messages/{target}/emoticons"),
            json!({ "emoticonCode": "🍇" }),
        )
        .await
        .expect_status(200);

    let aggregated = alice
        .api
        .get(&format!("/api/messages/{target}/emoticons"))
        .await
        .array();

    assert_eq!(aggregated.len(), 2);
    let apple = aggregated
        .iter()
        .find(|a| s(a, "emoticonCode") == "🍎")
        .expect("apple aggregation");
    expect_keys(apple, &["emoticonCode", "count", "users"]);
    assert_eq!(i(apple, "count"), 2);
    assert_eq!(apple["users"].as_array().map(Vec::len), Some(2));
    // First-appearance ordering.
    assert_eq!(s(&aggregated[0], "emoticonCode"), "🍎");
}

#[tokio::test]
async fn removing_an_emoticon_shrinks_the_reactor_set() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "reactions").await;
    add_member(&alice.api, &bob, &room).await;
    let msg = send_message(&alice.api, &room, "react to me").await;
    let target = s(&msg, "id");
    for user in [&alice, &bob] {
        user.api
            .post(
                &format!("/api/messages/{target}/emoticons"),
                json!({ "emoticonCode": "🍎" }),
            )
            .await
            .expect_status(200);
    }

    bob.api
        .delete(&format!("/api/messages/{target}/emoticons/%F0%9F%8D%8E"))
        .await
        .expect_message("Emoticon removed successfully");

    let aggregated = alice
        .api
        .get(&format!("/api/messages/{target}/emoticons"))
        .await
        .array();
    assert_eq!(aggregated.len(), 1);
    assert_eq!(i(&aggregated[0], "count"), 1);

    // Removing the last reactor drops the code entirely.
    alice
        .api
        .delete(&format!("/api/messages/{target}/emoticons/%F0%9F%8D%8E"))
        .await
        .expect_status(200);
    assert!(alice
        .api
        .get(&format!("/api/messages/{target}/emoticons"))
        .await
        .array()
        .is_empty());
}

#[tokio::test]
async fn removing_an_emoticon_you_never_added_is_a_no_op() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "reactions").await;
    let msg = send_message(&alice.api, &room, "react to me").await;
    let target = s(&msg, "id");

    alice
        .api
        .delete(&format!("/api/messages/{target}/emoticons/%F0%9F%8D%8E"))
        .await
        .expect_message("Emoticon removed successfully");
    assert!(alice
        .api
        .get(&format!("/api/messages/{target}/emoticons"))
        .await
        .array()
        .is_empty());
}

#[tokio::test]
async fn a_duplicate_reaction_is_idempotent_in_the_aggregation() {
    // §15 #15: the "already added" branch is dead code; aggregation is
    // set-based, so the visible result is unchanged either way.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "reactions").await;
    let msg = send_message(&alice.api, &room, "react to me").await;
    let target = s(&msg, "id");

    for _ in 0..3 {
        let resp = alice
            .api
            .post(
                &format!("/api/messages/{target}/emoticons"),
                json!({ "emoticonCode": "🍎" }),
            )
            .await;
        assert!(
            resp.code() == 200 || resp.code() == 400,
            "a duplicate add is either appended or refused, never a 500: {}",
            resp.text
        );
    }

    let aggregated = alice
        .api
        .get(&format!("/api/messages/{target}/emoticons"))
        .await
        .array();
    assert_eq!(aggregated.len(), 1);
    assert_eq!(
        i(&aggregated[0], "count"),
        1,
        "the reactor set has one member"
    );
}

#[tokio::test]
async fn reacting_requires_membership_and_an_existing_target() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let outsider = new_user(&server, "outsider").await;
    let room = create_room(&alice.api, "closed").await;
    let msg = send_message(&alice.api, &room, "private").await;
    let target = s(&msg, "id");

    outsider
        .api
        .post(
            &format!("/api/messages/{target}/emoticons"),
            json!({ "emoticonCode": "🍎" }),
        )
        .await
        .expect_error(403, "Access denied");
    outsider
        .api
        .get(&format!("/api/messages/{target}/emoticons"))
        .await
        .expect_error(403, "Access denied");
    alice
        .api
        .post(
            "/api/messages/msg_0000000000_deadbeef/emoticons",
            json!({ "emoticonCode": "🍎" }),
        )
        .await
        .expect_error(404, "Message not found");
}

#[tokio::test]
async fn an_emoticon_code_is_length_checked() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "reactions").await;
    let msg = send_message(&alice.api, &room, "react to me").await;
    let target = s(&msg, "id");

    alice
        .api
        .post(
            &format!("/api/messages/{target}/emoticons"),
            json!({ "emoticonCode": "" }),
        )
        .await
        .expect_validation_failed();
    alice
        .api
        .post(
            &format!("/api/messages/{target}/emoticons"),
            json!({ "emoticonCode": "x".repeat(65) }),
        )
        .await
        .expect_validation_failed();
}

#[tokio::test]
async fn a_percent_encoded_emoticon_code_is_decoded_exactly_once() {
    // §15 #14: the reference decodes twice, mangling codes containing `%`.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "encoding").await;
    let msg = send_message(&alice.api, &room, "react to me").await;
    let target = s(&msg, "id");
    alice
        .api
        .post(
            &format!("/api/messages/{target}/emoticons"),
            json!({ "emoticonCode": "100%" }),
        )
        .await
        .expect_status(200);

    // `100%` percent-encodes to `100%25`.
    alice
        .api
        .delete(&format!("/api/messages/{target}/emoticons/100%25"))
        .await
        .expect_status(200);

    assert!(
        alice
            .api
            .get(&format!("/api/messages/{target}/emoticons"))
            .await
            .array()
            .is_empty(),
        "a single decode must round-trip a code containing `%`"
    );
}

#[tokio::test]
async fn reactions_flow_through_sync_as_events() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "reactions").await;
    let msg = send_message(&alice.api, &room, "react to me").await;
    let target = s(&msg, "id");
    alice
        .api
        .post(
            &format!("/api/messages/{target}/emoticons"),
            json!({ "emoticonCode": "🍎" }),
        )
        .await
        .expect_status(200);
    alice
        .api
        .delete(&format!("/api/messages/{target}/emoticons/%F0%9F%8D%8E"))
        .await
        .expect_status(200);

    let events = drain_sync(&alice.api, &room).await;

    let types: Vec<String> = events.iter().map(|e| s(e, "msgType")).collect();
    assert!(types.contains(&"emoticon_add".to_string()), "{types:?}");
    assert!(types.contains(&"emoticon_remove".to_string()), "{types:?}");
}

// --- the §9 fold ----------------------------------------------------------

#[tokio::test]
async fn folding_the_sync_stream_reproduces_the_room_state() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "folding").await;
    add_member(&alice.api, &bob, &room).await;

    let kept = send_message(&alice.api, &room, "kept").await;
    let edited = send_message(&alice.api, &room, "will be edited").await;
    let deleted = send_message(&bob.api, &room, "will be deleted").await;
    alice
        .api
        .patch(
            &format!("/api/messages/{}", s(&edited, "id")),
            json!({ "content": "edited text", "msgHash": crypto::sha256_hex(b"edited text") }),
        )
        .await
        .expect_status(200);
    alice
        .api
        .delete(&format!("/api/messages/{}", s(&deleted, "id")))
        .await
        .expect_status(200);
    bob.api
        .post(
            &format!("/api/messages/{}/emoticons", s(&kept, "id")),
            json!({ "emoticonCode": "🍎" }),
        )
        .await
        .expect_status(200);

    let state = fold_all(&drain_sync(&alice.api, &room).await);

    assert_eq!(
        state.messages.len(),
        2,
        "one add plus one edit survive: {state:?}"
    );
    assert_eq!(s(&state.messages[&s(&kept, "id")], "content"), "kept");
    assert_eq!(
        s(&state.messages[&s(&edited, "id")], "content"),
        "edited text"
    );
    assert!(!state.messages.contains_key(&s(&deleted, "id")));
    assert_eq!(
        state.reactions[&s(&kept, "id")]["🍎"],
        vec![bob.address.clone()]
    );
    assert_eq!(state.cursor, latest_serial(&alice.api, &room).await);
}

#[tokio::test]
async fn an_edit_that_arrives_before_its_original_is_upserted() {
    // §8.1: a message edited before you ever saw it arrives once, already
    // edited, with msgType "edit" — the fold must upsert, not require a
    // pre-existing entry.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "late joiner").await;
    let msg = send_message(&alice.api, &room, "original").await;
    alice
        .api
        .patch(
            &format!("/api/messages/{}", s(&msg, "id")),
            json!({ "content": "final", "msgHash": crypto::sha256_hex(b"final") }),
        )
        .await
        .expect_status(200);
    add_member(&alice.api, &bob, &room).await;

    let events = drain_sync(&bob.api, &room).await;
    let state = fold_all(&events);

    let delivered: Vec<&Value> = events
        .iter()
        .filter(|e| s(e, "id") == s(&msg, "id"))
        .collect();
    assert_eq!(
        delivered.len(),
        1,
        "the row is delivered once, already edited"
    );
    assert_eq!(s(delivered[0], "msgType"), "edit");
    assert_eq!(s(&state.messages[&s(&msg, "id")], "content"), "final");
}

#[tokio::test]
async fn a_delete_all_clears_state_at_its_own_point_in_serial_order() {
    // §9: later events in the same batch are post-purge.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "purge order").await;
    send_message(&alice.api, &room, "before purge").await;
    alice
        .api
        .delete(&format!("/api/rooms/{room}/messages"))
        .await
        .expect_status(200);
    let after = send_message(&alice.api, &room, "after purge").await;

    let state = fold_all(&drain_sync(&alice.api, &room).await);

    assert_eq!(state.messages.len(), 1, "{state:?}");
    assert!(state.messages.contains_key(&s(&after, "id")));
}

#[tokio::test]
async fn an_unknown_message_type_does_not_abort_the_fold() {
    // Forward compatibility, exercised locally: a future msgType must be
    // ignored rather than dropping the rest of the batch.
    let events = vec![
        json!({ "id": "msg_1", "msgType": "add", "content": "kept", "msgSerial": 10 }),
        json!({ "id": "msg_2", "msgType": "some_future_type", "msgSerial": 11 }),
        json!({ "id": "msg_3", "msgType": "add", "content": "also kept", "msgSerial": 12 }),
    ];

    let state = fold_all(&events);

    assert_eq!(state.messages.len(), 2);
    assert_eq!(state.cursor, 12);
}

// --- on-chain publishing --------------------------------------------------

#[tokio::test]
async fn publishing_rejects_a_malformed_transaction_hash() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "publish").await;
    let msg = send_message(&alice.api, &room, "anchor me").await;
    let id = s(&msg, "id");

    let non_hex = format!("0x{}", "z".repeat(64));
    for bad in ["0x1234", "not-a-hash", non_hex.as_str()] {
        alice
            .api
            .post(
                &format!("/api/messages/{id}/publish"),
                json!({ "txHash": bad, "toAddress": alice.address }),
            )
            .await
            .expect_error(400, "Invalid transaction hash format");
    }
}

#[tokio::test]
async fn publishing_rejects_a_malformed_recipient_address() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "publish").await;
    let msg = send_message(&alice.api, &room, "anchor me").await;
    let id = s(&msg, "id");

    alice
        .api
        .post(
            &format!("/api/messages/{id}/publish"),
            json!({ "txHash": format!("0x{}", "ab".repeat(32)), "toAddress": "0xnope" }),
        )
        .await
        .expect_error(400, "Invalid to address format");
}

/// The anchor wallet is environment-only, so the publish tests start a server
/// that has one configured.
const ANCHOR_WALLET: &str = "0x1111111111111111111111111111111111111111";

async fn anchored_server() -> TestServer {
    TestServer::start_with_env(&[("VITE_FRUITNATION_WALLET", ANCHOR_WALLET)]).await
}

#[tokio::test]
async fn publishing_to_a_wallet_other_than_the_servers_is_rejected() {
    let server = anchored_server().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "publish").await;
    let msg = send_message(&alice.api, &room, "anchor me").await;
    let id = s(&msg, "id");

    let resp = alice
        .api
        .post(
            &format!("/api/messages/{id}/publish"),
            json!({
                "txHash": format!("0x{}", "ab".repeat(32)),
                "toAddress": Signer::random().address(),
            }),
        )
        .await;

    resp.expect_error(
        400,
        "Publishing hash failed: transaction recipient does not match server wallet",
    );
}

#[tokio::test]
async fn publishing_an_anchor_records_the_transaction_hash() {
    let server = anchored_server().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "publish").await;
    let msg = send_message(&alice.api, &room, "anchor me").await;
    let id = s(&msg, "id");
    let tx_hash = format!("0x{}", "ab".repeat(32));

    let anchored = alice
        .api
        .post(
            &format!("/api/messages/{id}/publish"),
            json!({ "txHash": tx_hash, "toAddress": ANCHOR_WALLET }),
        )
        .await
        .expect_ok();

    expect_message_shape(&anchored);
    assert_eq!(s(&anchored, "txHash"), tx_hash);
    // §8.1: the serial bump is what makes /sync redeliver the row so clients
    // pick the anchor up.
    assert!(i(&anchored, "msgSerial") > i(&msg, "msgSerial"));
}

#[tokio::test]
async fn a_message_can_only_be_anchored_once() {
    let server = anchored_server().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "publish").await;
    let msg = send_message(&alice.api, &room, "anchor me").await;
    let id = s(&msg, "id");
    let body = json!({ "txHash": format!("0x{}", "ab".repeat(32)), "toAddress": ANCHOR_WALLET });

    alice
        .api
        .post(&format!("/api/messages/{id}/publish"), body.clone())
        .await
        .expect_status(200);

    let resp = alice
        .api
        .post(&format!("/api/messages/{id}/publish"), body)
        .await;
    assert!(
        (400..500).contains(&resp.code()),
        "a second anchor must be refused, got {}: {}",
        resp.code(),
        resp.text
    );
}

#[tokio::test]
async fn only_the_message_sender_can_publish_an_anchor() {
    // §15 #13: authorization failures get an accurate code here, not the
    // reference's blanket 400.
    let server = anchored_server().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "publish").await;
    add_member(&alice.api, &bob, &room).await;
    let msg = send_message(&alice.api, &room, "alice's anchor").await;

    bob.api
        .post(
            &format!("/api/messages/{}/publish", s(&msg, "id")),
            json!({ "txHash": format!("0x{}", "ab".repeat(32)), "toAddress": ANCHOR_WALLET }),
        )
        .await
        .expect_error(
            403,
            "Only the message sender can publish a transaction hash",
        );
}

#[tokio::test]
async fn anchoring_an_unknown_message_is_a_404() {
    let server = anchored_server().await;
    let alice = new_user(&server, "alice").await;

    alice
        .api
        .post(
            "/api/messages/msg_0000000000_deadbeef/publish",
            json!({ "txHash": format!("0x{}", "ab".repeat(32)), "toAddress": ANCHOR_WALLET }),
        )
        .await
        .expect_error(404, "Message not found");
}
