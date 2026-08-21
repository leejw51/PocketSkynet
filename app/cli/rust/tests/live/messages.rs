//! Messages: the plaintext send/read path, its hashing contract, and the
//! refusals around it.

use std::sync::Arc;

use pocketskynet_client::ClientError;

use crate::common::{self, TestServer};

#[tokio::test]
async fn a_room_id_with_a_path_separator_is_refused_before_any_request() {
    // Defence in depth: even authenticated, a `/` or `?` in room_id must not
    // retarget the request at another endpoint — the client refuses locally.
    let server = TestServer::start().await;
    let (client, _wallet) = common::login_new_user(&server, "escaper").await;

    for bad in [
        "room/../../auth/profile",
        "room?limit=1",
        "room_1/messages#x",
        "..%2Fadmin",
    ] {
        match client.send_message(bad, "hi").await {
            Err(ClientError::InvalidArgument { kind, .. }) => assert_eq!(kind, "room id"),
            other => panic!("{bad:?} must be refused locally, got {other:?}"),
        }
        match client.messages(bad, 10).await {
            Err(ClientError::InvalidArgument { .. }) => {}
            other => panic!("{bad:?} must be refused locally, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn sent_messages_read_back_in_order_with_their_hashes() {
    let server = TestServer::start().await;
    let (client, wallet) = common::login_new_user(&server, "chatter").await;
    let room = client.create_room("thread", None).await.unwrap();

    for text in ["first", "second", "third"] {
        let sent = client.send_message(&room.id, text).await.unwrap();
        assert_eq!(sent.content, text);
        assert_eq!(sent.room_id, room.id);
        assert_eq!(sent.sender_address, wallet.address().as_str());
        assert_eq!(
            sent.msg_hash.as_deref(),
            Some(pocketskynet_core::msg_hash_plaintext(text).as_str()),
            "the server stores the client-computed msgHash verbatim"
        );
        assert_eq!(sent.msg_type.as_deref(), Some("add"));
        assert_eq!(sent.is_encrypted, Some(false));
        assert_eq!(
            sent.sender.as_ref().map(|s| s.username.as_str()),
            Some("chatter"),
            "POST attaches the sender"
        );
    }

    let messages = client.messages(&room.id, 50).await.unwrap();
    let contents: Vec<&str> = messages.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        contents,
        vec!["first", "second", "third"],
        "chronologically ascending"
    );
    let timestamps: Vec<i64> = messages
        .iter()
        .map(|m| m.message_timestamp.expect("a timestamp"))
        .collect();
    assert!(
        timestamps.windows(2).all(|pair| pair[0] <= pair[1]),
        "timestamps ascend with the order (same-millisecond sends are legal): {timestamps:?}"
    );
}

#[tokio::test]
async fn content_is_trimmed_before_hashing_and_sending() {
    // The server trims content before storing (API.md §3.1 QUIRK); the
    // client trims first and hashes what it sends, so the stored msgHash
    // still matches the stored content.
    let server = TestServer::start().await;
    let (client, _wallet) = common::login_new_user(&server, "trimmer").await;
    let room = client.create_room("tidy", None).await.unwrap();

    let sent = client
        .send_message(&room.id, "  padded hello \n")
        .await
        .unwrap();
    assert_eq!(sent.content, "padded hello");
    assert_eq!(
        sent.msg_hash.as_deref(),
        Some(pocketskynet_core::msg_hash_plaintext("padded hello").as_str())
    );
}

#[tokio::test]
async fn the_limit_caps_the_page_at_the_newest_messages() {
    let server = TestServer::start().await;
    let (client, _wallet) = common::login_new_user(&server, "pager").await;
    let room = client.create_room("pages", None).await.unwrap();

    for i in 1..=5 {
        client
            .send_message(&room.id, &format!("m{i}"))
            .await
            .unwrap();
    }

    let page = client.messages(&room.id, 2).await.unwrap();
    let contents: Vec<&str> = page.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        contents,
        vec!["m4", "m5"],
        "the newest `limit` messages, still ascending"
    );
}

#[tokio::test]
async fn sending_into_a_room_you_are_not_a_member_of_is_403() {
    let server = TestServer::start().await;
    let (owner, _w1) = common::login_new_user(&server, "host").await;
    let (outsider, _w2) = common::login_new_user(&server, "gatecrasher").await;

    let room = owner.create_room("members only", None).await.unwrap();
    match outsider.send_message(&room.id, "let me in").await {
        Err(ClientError::Api {
            status: 403,
            message,
            ..
        }) => assert_eq!(message, "Access denied"),
        other => panic!("a non-member send must be a 403, got {other:?}"),
    }

    // And the room's owner never sees a trace of it.
    assert!(owner.messages(&room.id, 50).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_message_over_5000_characters_is_refused() {
    let server = TestServer::start().await;
    let (client, _wallet) = common::login_new_user(&server, "novelist").await;
    let room = client.create_room("long form", None).await.unwrap();

    match client.send_message(&room.id, &"x".repeat(5001)).await {
        Err(ClientError::Api { status: 400, .. }) => {}
        other => panic!("5001 chars must be a 400, got {other:?}"),
    }

    // The boundary itself is fine.
    let sent = client.send_message(&room.id, &"x".repeat(5000)).await;
    assert!(sent.is_ok(), "exactly 5000 chars is allowed: {sent:?}");
}

#[tokio::test]
async fn a_body_over_the_100kb_cap_is_413() {
    // API.md §1.2: the API-wide body limit is 100 KB. The content limit
    // would also catch this, but the transport-level refusal comes first and
    // is a different status the client must surface faithfully.
    let server = TestServer::start().await;
    let (client, _wallet) = common::login_new_user(&server, "bulk").await;
    let room = client.create_room("bulk drop", None).await.unwrap();

    match client.send_message(&room.id, &"y".repeat(200_000)).await {
        Err(ClientError::Api { status: 413, .. }) => {}
        other => panic!("a 200KB body must be a 413, got {other:?}"),
    }
}

#[tokio::test]
async fn parallel_sends_all_land_with_distinct_serials() {
    let server = TestServer::start().await;
    let (client, _wallet) = common::login_new_user(&server, "swarm").await;
    let room = client.create_room("burst", None).await.unwrap();

    let client = Arc::new(client);
    let mut handles = Vec::new();
    for i in 0..8 {
        let client = Arc::clone(&client);
        let room_id = room.id.clone();
        handles.push(tokio::spawn(async move {
            client
                .send_message(&room_id, &format!("burst-{i}"))
                .await
                .expect("a concurrent send must land")
        }));
    }
    for handle in handles {
        handle.await.expect("no send task may panic");
    }

    let messages = client.messages(&room.id, 50).await.unwrap();
    let burst: Vec<_> = messages
        .iter()
        .filter(|m| m.content.starts_with("burst-"))
        .collect();
    assert_eq!(burst.len(), 8, "every concurrent send landed exactly once");

    let mut serials: Vec<i64> = burst
        .iter()
        .map(|m| m.msg_serial.expect("a serial"))
        .collect();
    serials.sort_unstable();
    serials.dedup();
    assert_eq!(serials.len(), 8, "serials are distinct across the burst");
}
