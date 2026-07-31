//! WebSocket and SSE. Spec: `docs/REALTIME.md` §1–§6 (WebSocket) and §8 (SSE),
//! plus `docs/API.md` §12.

mod common;

use std::time::Duration;

use common::*;
use eventsource_stream::{Event as SseEvent, Eventsource};
use futures_util::stream::Stream;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Long enough to absorb a loaded CI machine, short enough that a genuinely
/// missing event does not stall the suite.
const EVENT_TIMEOUT: Duration = Duration::from_secs(6);
/// How long to wait before concluding that an event will *not* arrive.
const SILENCE_WINDOW: Duration = Duration::from_millis(1200);

// --- WebSocket helpers ----------------------------------------------------

/// Connect with the preferred `Sec-WebSocket-Protocol: fnauth, <JWT>` form.
async fn connect_subprotocol(
    server: &TestServer,
    token: &str,
) -> Result<(Ws, Option<String>), WsError> {
    let mut request = server.ws_url("/ws").into_client_request()?;
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_str(&format!("fnauth, {token}")).expect("header value"),
    );
    let (stream, response) = connect_async(request).await?;
    let echoed = response
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    Ok((stream, echoed))
}

/// Connect with the `?token=` fallback retained for native CLIs.
async fn connect_query(server: &TestServer, token: &str) -> Result<Ws, WsError> {
    let (stream, _) = connect_async(server.ws_url(&format!("/ws?token={token}"))).await?;
    Ok(stream)
}

async fn send_json<S>(ws: &mut S, value: Value)
where
    S: futures_util::sink::Sink<Message, Error = WsError> + Unpin,
{
    ws.send(Message::Text(value.to_string()))
        .await
        .expect("send frame");
}

/// Next JSON frame, or `None` if the socket goes quiet / closes first.
async fn next_event<S>(ws: &mut S, timeout: Duration) -> Option<Value>
where
    S: Stream<Item = Result<Message, WsError>> + Unpin,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    return Some(value);
                }
            }
            // Ping/pong control frames and binary noise are not app events.
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(_))) | Ok(None) | Err(_) => return None,
        }
    }
}

/// Wait for a frame whose `type` matches, ignoring unrelated wake-ups.
async fn next_event_of<S>(ws: &mut S, kind: &str, timeout: Duration) -> Option<Value>
where
    S: Stream<Item = Result<Message, WsError>> + Unpin,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let event = next_event(ws, remaining).await?;
        if event.get("type").and_then(Value::as_str) == Some(kind) {
            return Some(event);
        }
    }
}

/// The close code and reason, if the peer closes within `timeout`.
async fn next_close<S>(ws: &mut S, timeout: Duration) -> Option<(u16, String)>
where
    S: Stream<Item = Result<Message, WsError>> + Unpin,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(Message::Close(frame)))) => {
                return Some(match frame {
                    Some(f) => (u16::from(f.code), f.reason.to_string()),
                    None => (1005, String::new()),
                })
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(WsError::ConnectionClosed))) | Ok(None) => {
                return Some((1006, String::new()))
            }
            Ok(Some(Err(WsError::Protocol(_)))) | Err(_) => return None,
            Ok(Some(Err(_))) => return None,
        }
    }
}

// --- WebSocket: handshake -------------------------------------------------

#[tokio::test]
async fn the_websocket_accepts_the_fnauth_subprotocol_and_echoes_only_the_marker() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    let (_ws, echoed) = connect_subprotocol(&server, alice.api.token())
        .await
        .expect("handshake with the fnauth subprotocol");

    assert_eq!(
        echoed.as_deref(),
        Some("fnauth"),
        "the server must echo `fnauth` and never the token"
    );
}

#[tokio::test]
async fn the_websocket_accepts_the_token_query_fallback_when_it_is_enabled() {
    let server = TestServer::start_with_args(&["--sse-token-query"]).await;
    let alice = new_user(&server, "alice").await;

    let mut ws = connect_query(&server, alice.api.token())
        .await
        .expect("handshake with ?token=");

    // Prove the connection is live and authenticated, not merely upgraded.
    send_json(&mut ws, json!({ "type": "ping" })).await;
    let pong = next_event_of(&mut ws, "pong", EVENT_TIMEOUT)
        .await
        .expect("a ping must be answered");
    assert_eq!(pong["type"], "pong");
}

#[tokio::test]
async fn a_gated_token_query_handshake_names_the_flag_rather_than_blaming_the_token() {
    // Settled divergence: `?token=` is gated on /ws as well as /api/events
    // (docs/REALTIME.md §1, docs/API.md §12.1). A native CLI that hits the gate
    // must be told the server disabled the mechanism — "Invalid token" would
    // send the reader off to re-check a credential that is perfectly good.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    let err = connect_query(&server, alice.api.token())
        .await
        .expect_err("an ungated `?token=` handshake must be refused");

    // The body has to be decoded rather than read out of `{err:?}` — the Debug
    // impl renders it as a byte array, which would make a `contains` assertion
    // pass or fail for reasons unrelated to the message.
    let WsError::Http(response) = err else {
        panic!("expected an HTTP handshake rejection, got: {err:?}");
    };
    assert_eq!(response.status(), 401);

    let body = response
        .body()
        .as_ref()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();

    assert!(
        body.contains("--sse-token-query"),
        "the rejection must name the flag that enables this path, got: {body}"
    );
    assert!(
        !body.contains("Invalid token"),
        "the rejection must not blame the credential, got: {body}"
    );
}

#[tokio::test]
async fn the_websocket_token_query_fallback_is_off_by_default() {
    // The behaviour as built: the credential-in-a-URL path is opt-in for both
    // transports. Recorded so the hardening is not lost by accident.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    match connect_query(&server, alice.api.token()).await {
        Err(_) => {}
        Ok(mut ws) => {
            assert!(
                next_close(&mut ws, EVENT_TIMEOUT).await.is_some(),
                "an ungated `?token=` handshake must not be left open"
            );
        }
    }
}

#[tokio::test]
async fn a_websocket_without_a_token_is_refused() {
    let server = TestServer::start().await;

    match connect_async(server.ws_url("/ws")).await {
        // Rejected at the HTTP layer: acceptable and preferable.
        Err(_) => {}
        Ok((mut ws, _)) => {
            let (code, _) = next_close(&mut ws, EVENT_TIMEOUT)
                .await
                .expect("an unauthenticated socket must be closed");
            assert_eq!(code, 4001, "REALTIME §1: missing token closes with 4001");
        }
    }
}

#[tokio::test]
async fn a_websocket_with_an_invalid_token_is_refused() {
    let server = TestServer::start().await;

    match connect_query(&server, "not.a.jwt").await {
        Err(_) => {}
        Ok(mut ws) => {
            let (code, _) = next_close(&mut ws, EVENT_TIMEOUT)
                .await
                .expect("an invalid token must be closed");
            assert_eq!(code, 4001);
        }
    }
}

#[tokio::test]
async fn a_websocket_with_an_expired_token_is_refused() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let stale = mint_token(&alice.address, now_secs() - 7200, now_secs() - 3600);

    match connect_query(&server, &stale).await {
        Err(_) => {}
        Ok(mut ws) => {
            let (code, _) = next_close(&mut ws, EVENT_TIMEOUT)
                .await
                .expect("an expired token must be closed");
            assert_eq!(code, 4001);
        }
    }
}

#[tokio::test]
async fn a_websocket_with_an_alg_none_token_is_refused() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let forged = mint_alg_none_token(&alice.address);

    match connect_query(&server, &forged).await {
        Err(_) => {}
        Ok(mut ws) => {
            let (code, _) = next_close(&mut ws, EVENT_TIMEOUT)
                .await
                .expect("HS256 is pinned; alg:none must be refused");
            assert_eq!(code, 4001);
        }
    }
}

#[tokio::test]
async fn a_handshake_offering_no_fnauth_marker_is_refused() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let mut request = server
        .ws_url("/ws")
        .into_client_request()
        .expect("client request");
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_str(&format!("chat, {}", alice.api.token())).expect("header value"),
    );

    // REALTIME §1: `handleProtocols` returns false when `fnauth` is absent, so
    // the handshake is rejected outright rather than upgraded.
    match connect_async(request).await {
        Err(_) => {}
        Ok((mut ws, response)) => {
            assert_ne!(
                response
                    .headers()
                    .get("sec-websocket-protocol")
                    .and_then(|v| v.to_str().ok()),
                Some("fnauth"),
                "`fnauth` was never offered, so it must not be selected"
            );
            assert!(
                next_close(&mut ws, SILENCE_WINDOW).await.is_some(),
                "a handshake without the fnauth marker must not be left open"
            );
        }
    }
}

// --- WebSocket: keepalive -------------------------------------------------

#[tokio::test]
async fn a_client_ping_is_answered_with_a_pong() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let (mut ws, _) = connect_subprotocol(&server, alice.api.token())
        .await
        .expect("connect");

    send_json(&mut ws, json!({ "type": "ping" })).await;

    let pong = next_event_of(&mut ws, "pong", EVENT_TIMEOUT)
        .await
        .expect("pong");
    assert_eq!(pong, json!({ "type": "pong" }));
}

#[tokio::test]
async fn a_non_json_frame_is_silently_ignored() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let (mut ws, _) = connect_subprotocol(&server, alice.api.token())
        .await
        .expect("connect");

    ws.send(Message::Text("this is not json".into()))
        .await
        .expect("send");
    send_json(&mut ws, json!({ "type": "some_unknown_type" })).await;
    send_json(&mut ws, json!({ "type": "ping" })).await;

    // The socket survives both and still answers.
    let pong = next_event_of(&mut ws, "pong", EVENT_TIMEOUT)
        .await
        .expect("garbage must not kill the socket");
    assert_eq!(pong["type"], "pong");
}

// --- WebSocket: wake-ups --------------------------------------------------

#[tokio::test]
async fn a_message_from_one_member_wakes_another() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "live").await;
    add_member(&alice.api, &bob, &room).await;
    let (mut ws, _) = connect_subprotocol(&server, bob.api.token())
        .await
        .expect("connect");

    let sent = send_message(&alice.api, &room, "wake up").await;

    let event = next_event_of(&mut ws, "new_message", EVENT_TIMEOUT)
        .await
        .expect("bob must be woken by alice's message");
    assert_eq!(event["roomId"].as_str(), Some(room.as_str()));
    assert_eq!(
        event["msgSerial"].as_i64(),
        Some(i(&sent, "msgSerial")),
        "REALTIME §8.4: the wake-up carries the room-scoped serial as a hint"
    );
    assert!(
        event.get("content").is_none(),
        "a wake-up must never carry message content: {event}"
    );
}

#[tokio::test]
async fn edits_deletes_and_reactions_all_produce_a_wake_up() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "live").await;
    add_member(&alice.api, &bob, &room).await;
    let msg = send_message(&alice.api, &room, "original").await;
    let id = s(&msg, "id");
    let (mut ws, _) = connect_subprotocol(&server, bob.api.token())
        .await
        .expect("connect");

    alice
        .api
        .patch(
            &format!("/api/messages/{id}"),
            json!({ "content": "edited", "msgHash": crypto::sha256_hex(b"edited") }),
        )
        .await
        .expect_status(200);
    next_event_of(&mut ws, "new_message", EVENT_TIMEOUT)
        .await
        .expect("an edit wakes the room");

    alice
        .api
        .post(
            &format!("/api/messages/{id}/emoticons"),
            json!({ "emoticonCode": "🍎" }),
        )
        .await
        .expect_status(200);
    next_event_of(&mut ws, "new_message", EVENT_TIMEOUT)
        .await
        .expect("a reaction wakes the room");

    alice
        .api
        .delete(&format!("/api/messages/{id}"))
        .await
        .expect_status(200);
    next_event_of(&mut ws, "new_message", EVENT_TIMEOUT)
        .await
        .expect("a delete wakes the room");
}

#[tokio::test]
async fn a_key_rotation_wakes_the_room() {
    // §12.4: rotation emits `new_message` so clients refetch keys/versions.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "rotating").await;
    add_member(&alice.api, &bob, &room).await;
    let (mut ws, _) = connect_subprotocol(&server, bob.api.token())
        .await
        .expect("connect");

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/rotate-key"),
            json!({
                "newVersion": 2,
                "keys": [
                    { "userAddress": alice.address, "encryptedSymmetricKey": "a2V5", "ephemeralPublicKey": format!("04{}", "ab".repeat(64)), "encryptionIV": "1a2b3c4d5e6f78901a2b3c4d5e6f7890", "hmac": "9f".repeat(32) },
                    { "userAddress": bob.address, "encryptedSymmetricKey": "a2V5", "ephemeralPublicKey": format!("04{}", "cd".repeat(64)), "encryptionIV": "1a2b3c4d5e6f78901a2b3c4d5e6f7890", "hmac": "1a".repeat(32) },
                ],
            }),
        )
        .await
        .expect_status(200);

    let event = next_event_of(&mut ws, "new_message", EVENT_TIMEOUT)
        .await
        .expect("a rotation must wake the room");
    assert_eq!(event["roomId"].as_str(), Some(room.as_str()));
}

#[tokio::test]
async fn a_non_member_is_never_woken() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let outsider = new_user(&server, "outsider").await;
    let room = create_room(&alice.api, "closed").await;
    let (mut ws, _) = connect_subprotocol(&server, outsider.api.token())
        .await
        .expect("connect");

    send_message(&alice.api, &room, "not for you").await;

    assert!(
        next_event_of(&mut ws, "new_message", SILENCE_WINDOW)
            .await
            .is_none(),
        "subscriptions are derived from membership, not requested by the client"
    );
}

#[tokio::test]
async fn an_invitee_receives_invitation_received() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "invites").await;
    let (mut ws, _) = connect_subprotocol(&server, bob.api.token())
        .await
        .expect("connect");

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/invite"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_status(200);

    let event = next_event_of(&mut ws, "invitation_received", EVENT_TIMEOUT)
        .await
        .expect("the invitee must be notified");
    assert_eq!(event["roomId"].as_str(), Some(room.as_str()));
}

#[tokio::test]
async fn accepting_an_invitation_updates_the_accepters_room_list() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "joining").await;
    alice
        .api
        .post(
            &format!("/api/rooms/{room}/invite"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_status(200);
    let (mut ws, _) = connect_subprotocol(&server, bob.api.token())
        .await
        .expect("connect");

    bob.api
        .post_empty(&format!("/api/invitations/{room}/accept"))
        .await
        .expect_status(200);

    next_event_of(&mut ws, "rooms_updated", EVENT_TIMEOUT)
        .await
        .expect("refreshUserRooms emits rooms_updated to the accepter");
}

#[tokio::test]
async fn remaining_members_are_told_the_roster_changed() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "roster").await;
    add_member(&alice.api, &bob, &room).await;
    let (mut ws, _) = connect_subprotocol(&server, alice.api.token())
        .await
        .expect("connect");

    bob.api
        .post_empty(&format!("/api/rooms/{room}/leave"))
        .await
        .expect_status(200);

    let event = next_event_of(&mut ws, "member_removed", EVENT_TIMEOUT)
        .await
        .expect("`member_removed` means `roster changed`");
    assert_eq!(event["roomId"].as_str(), Some(room.as_str()));
}

#[tokio::test]
async fn a_kicked_member_loses_their_subscription() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "kicking").await;
    add_member(&alice.api, &bob, &room).await;
    let (mut ws, _) = connect_subprotocol(&server, bob.api.token())
        .await
        .expect("connect");

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/kick"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_status(200);
    next_event_of(&mut ws, "rooms_updated", EVENT_TIMEOUT)
        .await
        .expect("the kicked user's room list changed");

    send_message(&alice.api, &room, "after the kick").await;
    assert!(
        next_event_of(&mut ws, "new_message", SILENCE_WINDOW)
            .await
            .is_none(),
        "a kicked member's live socket must stop receiving the room"
    );
}

// --- WebSocket: typing ----------------------------------------------------

#[tokio::test]
async fn typing_is_relayed_to_other_members_with_the_sender_address() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "typing").await;
    add_member(&alice.api, &bob, &room).await;
    let (mut alice_ws, _) = connect_subprotocol(&server, alice.api.token())
        .await
        .expect("connect");
    let (mut bob_ws, _) = connect_subprotocol(&server, bob.api.token())
        .await
        .expect("connect");

    send_json(&mut alice_ws, json!({ "type": "typing", "roomId": room })).await;

    let event = next_event_of(&mut bob_ws, "typing", EVENT_TIMEOUT)
        .await
        .expect("bob must see alice typing");
    assert_eq!(event["roomId"].as_str(), Some(room.as_str()));
    assert_eq!(
        event["from"].as_str(),
        Some(alice.address.as_str()),
        "`from` is the only server→client field that names a user"
    );
}

#[tokio::test]
async fn typing_is_never_echoed_to_the_sender() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "typing").await;
    add_member(&alice.api, &bob, &room).await;
    let (mut alice_ws, _) = connect_subprotocol(&server, alice.api.token())
        .await
        .expect("connect");
    let (mut bob_ws, _) = connect_subprotocol(&server, bob.api.token())
        .await
        .expect("connect");

    send_json(&mut alice_ws, json!({ "type": "typing", "roomId": room })).await;
    next_event_of(&mut bob_ws, "typing", EVENT_TIMEOUT)
        .await
        .expect("relayed to bob");

    assert!(
        next_event_of(&mut alice_ws, "typing", SILENCE_WINDOW)
            .await
            .is_none(),
        "the sender's own sockets must not receive the relay"
    );
}

#[tokio::test]
async fn typing_is_throttled_to_one_per_second_per_socket() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "typing").await;
    add_member(&alice.api, &bob, &room).await;
    let (mut alice_ws, _) = connect_subprotocol(&server, alice.api.token())
        .await
        .expect("connect");
    let (mut bob_ws, _) = connect_subprotocol(&server, bob.api.token())
        .await
        .expect("connect");

    for _ in 0..5 {
        send_json(&mut alice_ws, json!({ "type": "typing", "roomId": room })).await;
    }

    next_event_of(&mut bob_ws, "typing", EVENT_TIMEOUT)
        .await
        .expect("the first relay goes through");
    assert!(
        next_event_of(&mut bob_ws, "typing", Duration::from_millis(600))
            .await
            .is_none(),
        "the remaining four must be dropped by the 1/s throttle"
    );
}

#[tokio::test]
async fn typing_for_a_room_you_are_not_in_is_silently_dropped() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let outsider = new_user(&server, "outsider").await;
    let room = create_room(&alice.api, "closed").await;
    add_member(&alice.api, &bob, &room).await;
    let (mut outsider_ws, _) = connect_subprotocol(&server, outsider.api.token())
        .await
        .expect("connect");
    let (mut bob_ws, _) = connect_subprotocol(&server, bob.api.token())
        .await
        .expect("connect");

    send_json(
        &mut outsider_ws,
        json!({ "type": "typing", "roomId": room }),
    )
    .await;

    assert!(
        next_event_of(&mut bob_ws, "typing", SILENCE_WINDOW)
            .await
            .is_none(),
        "membership comes from the connect-time subscription set, not the frame"
    );
    // The socket is not punished for it either.
    send_json(&mut outsider_ws, json!({ "type": "ping" })).await;
    assert!(next_event_of(&mut outsider_ws, "pong", EVENT_TIMEOUT)
        .await
        .is_some());
}

#[tokio::test]
async fn typing_is_filtered_in_both_directions_across_a_block() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "typing").await;
    add_member(&alice.api, &bob, &room).await;
    alice
        .api
        .post("/api/users/block", json!({ "address": bob.address }))
        .await
        .expect_status(200);
    let (mut alice_ws, _) = connect_subprotocol(&server, alice.api.token())
        .await
        .expect("connect");
    let (mut bob_ws, _) = connect_subprotocol(&server, bob.api.token())
        .await
        .expect("connect");

    send_json(&mut bob_ws, json!({ "type": "typing", "roomId": room })).await;
    assert!(
        next_event_of(&mut alice_ws, "typing", SILENCE_WINDOW)
            .await
            .is_none(),
        "the blocker must not see the blocked user typing"
    );

    send_json(&mut alice_ws, json!({ "type": "typing", "roomId": room })).await;
    assert!(
        next_event_of(&mut bob_ws, "typing", SILENCE_WINDOW)
            .await
            .is_none(),
        "typing is a presence side-channel; the filter is bidirectional"
    );
}

#[tokio::test]
async fn a_blocked_users_message_produces_no_wake_up_at_all() {
    // REALTIME §5 residual leak, closed by carrying `origin` in the envelope:
    // a blocker must not even learn that something happened.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let carol = new_user(&server, "carol").await;
    let room = create_room(&alice.api, "blocking").await;
    add_member(&alice.api, &bob, &room).await;
    add_member(&alice.api, &carol, &room).await;
    alice
        .api
        .post("/api/users/block", json!({ "address": bob.address }))
        .await
        .expect_status(200);
    let (mut alice_ws, _) = connect_subprotocol(&server, alice.api.token())
        .await
        .expect("connect");

    send_message(&bob.api, &room, "from a blocked sender").await;
    assert!(
        next_event_of(&mut alice_ws, "new_message", SILENCE_WINDOW)
            .await
            .is_none(),
        "a blocked-only event must produce no wake-up (timing side channel)"
    );

    // Control: the socket is healthy and still receives everyone else.
    send_message(&carol.api, &room, "from carol").await;
    assert!(
        next_event_of(&mut alice_ws, "new_message", EVENT_TIMEOUT)
            .await
            .is_some(),
        "the socket must still deliver unblocked senders"
    );
}

// --- SSE ------------------------------------------------------------------

/// `POST /api/events/ticket` → the short-lived, single-use SSE credential.
async fn request_ticket(user: &User) -> String {
    let body = user.api.post_empty("/api/events/ticket").await.expect_ok();
    expect_keys(&body, &["ticket", "expiresAt", "ttlSeconds"]);
    assert!(
        i(&body, "ttlSeconds") > 0 && i(&body, "ttlSeconds") <= 60,
        "REALTIME §8.1: a ticket is short-lived: {body}"
    );
    s(&body, "ticket")
}

async fn open_sse(
    server: &TestServer,
    query: &str,
    last_event_id: Option<&str>,
) -> reqwest::Response {
    let mut request = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client")
        .get(server.url(&format!("/api/events{query}")))
        .header("Accept", "text/event-stream");
    if let Some(id) = last_event_id {
        request = request.header("Last-Event-ID", id);
    }
    request.send().await.expect("open the event stream")
}

/// Read SSE frames until `predicate` matches or the timeout expires.
async fn next_sse_matching<F>(
    stream: &mut (impl Stream<Item = Result<SseEvent, eventsource_stream::EventStreamError<reqwest::Error>>>
              + Unpin),
    timeout: Duration,
    mut predicate: F,
) -> Option<SseEvent>
where
    F: FnMut(&SseEvent) -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(event))) => {
                if predicate(&event) {
                    return Some(event);
                }
            }
            Ok(Some(Err(_))) | Ok(None) | Err(_) => return None,
        }
    }
}

#[tokio::test]
async fn an_sse_ticket_is_issued_to_an_authenticated_caller() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    let ticket = request_ticket(&alice).await;

    assert!(!ticket.is_empty());
    assert!(
        ticket.len() >= 16,
        "32 bytes of CSPRNG entropy, encoded: `{ticket}`"
    );
}

#[tokio::test]
async fn requesting_an_sse_ticket_requires_authentication() {
    let server = TestServer::start().await;

    Api::anonymous(&server.base_url)
        .post_empty("/api/events/ticket")
        .await
        .expect_status(401);
}

#[tokio::test]
async fn the_event_stream_sets_the_sse_response_headers() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let ticket = request_ticket(&alice).await;

    let response = open_sse(&server, &format!("?ticket={ticket}"), None).await;

    assert_eq!(response.status().as_u16(), 200);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.starts_with("text/event-stream"),
        "wrong content type: `{content_type}`"
    );
    let cache = response
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(cache.contains("no-store"), "wrong cache-control: `{cache}`");
}

#[tokio::test]
async fn the_event_stream_rejects_a_missing_or_bogus_ticket() {
    let server = TestServer::start().await;

    assert_eq!(open_sse(&server, "", None).await.status().as_u16(), 401);
    assert_eq!(
        open_sse(&server, "?ticket=evt_definitely_not_real", None)
            .await
            .status()
            .as_u16(),
        401
    );
}

#[tokio::test]
async fn an_sse_ticket_is_single_use() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let ticket = request_ticket(&alice).await;

    let first = open_sse(&server, &format!("?ticket={ticket}"), None).await;
    assert_eq!(first.status().as_u16(), 200);
    drop(first);

    let second = open_sse(&server, &format!("?ticket={ticket}"), None).await;
    assert_eq!(
        second.status().as_u16(),
        401,
        "a ticket is consumed atomically on connect"
    );
}

#[tokio::test]
async fn the_jwt_query_fallback_is_off_by_default() {
    // REALTIME §8.1 option 3: gated behind a config flag, default off.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    let response = open_sse(&server, &format!("?token={}", alice.api.token()), None).await;

    assert_eq!(
        response.status().as_u16(),
        401,
        "a full-lifetime bearer token in a URL must be opt-in"
    );
}

#[tokio::test]
async fn the_jwt_query_fallback_works_when_explicitly_enabled() {
    let server = TestServer::start_with_args(&["--sse-token-query"]).await;
    let alice = new_user(&server, "alice").await;

    let response = open_sse(&server, &format!("?token={}", alice.api.token()), None).await;

    assert_eq!(response.status().as_u16(), 200);
}

#[tokio::test]
async fn the_stream_starts_with_a_reconnect_hint() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let ticket = request_ticket(&alice).await;
    let response = open_sse(&server, &format!("?ticket={ticket}"), None).await;

    let mut body = response;
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + EVENT_TIMEOUT;
    while tokio::time::Instant::now() < deadline && !seen.contains("retry:") {
        match tokio::time::timeout(Duration::from_secs(2), body.chunk()).await {
            Ok(Ok(Some(chunk))) => seen.push_str(&String::from_utf8_lossy(&chunk)),
            _ => break,
        }
    }

    assert!(
        seen.contains("retry:"),
        "REALTIME §8.5: `retry:` is sent once at stream start; saw: {seen:?}"
    );
}

#[tokio::test]
async fn the_stream_emits_heartbeat_comments() {
    // §8.5: a bare `:hb` every 15 s keeps intermediaries from reaping the
    // connection. This is the one test that genuinely has to wait.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let ticket = request_ticket(&alice).await;
    let mut response = open_sse(&server, &format!("?ticket={ticket}"), None).await;

    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    while tokio::time::Instant::now() < deadline && !seen.contains(":hb") {
        match tokio::time::timeout(Duration::from_secs(20), response.chunk()).await {
            Ok(Ok(Some(chunk))) => seen.push_str(&String::from_utf8_lossy(&chunk)),
            _ => break,
        }
    }

    // A comment frame is `:` followed by optional whitespace and the text, so
    // both `:hb` and `: hb` are correct on the wire.
    assert!(
        seen.contains(":hb") || seen.contains(": hb"),
        "no heartbeat comment within 25 s; saw: {seen:?}"
    );
}

#[tokio::test]
async fn the_stream_delivers_new_message_events() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "sse").await;
    add_member(&alice.api, &bob, &room).await;
    let ticket = request_ticket(&bob).await;
    let response = open_sse(&server, &format!("?ticket={ticket}"), None).await;
    let mut stream = Box::pin(response.bytes_stream().eventsource());

    let sent = send_message(&alice.api, &room, "over sse").await;

    let event = next_sse_matching(&mut stream, EVENT_TIMEOUT, |e| e.event == "new_message")
        .await
        .expect("a new_message frame");
    let data: Value = serde_json::from_str(&event.data).expect("data is compact JSON");
    assert_eq!(data["roomId"].as_str(), Some(room.as_str()));
    assert_eq!(data["msgSerial"].as_i64(), Some(i(&sent, "msgSerial")));
    assert!(
        !event.id.is_empty() && event.id.parse::<u64>().is_ok(),
        "§8.3: `id:` is the global monotonic event_seq, got `{}`",
        event.id
    );
}

#[tokio::test]
async fn the_stream_event_names_mirror_the_websocket_types() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "sse").await;
    let ticket = request_ticket(&bob).await;
    let response = open_sse(&server, &format!("?ticket={ticket}"), None).await;
    let mut stream = Box::pin(response.bytes_stream().eventsource());

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/invite"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_status(200);

    let event = next_sse_matching(&mut stream, EVENT_TIMEOUT, |e| {
        e.event == "invitation_received"
    })
    .await
    .expect("an invitation_received frame");
    let data: Value = serde_json::from_str(&event.data).expect("valid JSON data");
    assert_eq!(data["roomId"].as_str(), Some(room.as_str()));
}

#[tokio::test]
async fn a_stream_resumes_from_last_event_id() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "resume").await;
    add_member(&alice.api, &bob, &room).await;

    // First connection: read one event, remember its id, then disconnect.
    let ticket = request_ticket(&bob).await;
    let response = open_sse(&server, &format!("?ticket={ticket}"), None).await;
    let mut stream = Box::pin(response.bytes_stream().eventsource());
    send_message(&alice.api, &room, "first").await;
    let first = next_sse_matching(&mut stream, EVENT_TIMEOUT, |e| e.event == "new_message")
        .await
        .expect("the first event");
    let cursor = first.id.clone();
    drop(stream);

    // Missed while disconnected.
    let missed = send_message(&alice.api, &room, "missed while away").await;

    let ticket = request_ticket(&bob).await;
    let response = open_sse(&server, &format!("?ticket={ticket}"), Some(&cursor)).await;
    let mut stream = Box::pin(response.bytes_stream().eventsource());

    let replayed = next_sse_matching(&mut stream, EVENT_TIMEOUT, |e| e.event == "new_message")
        .await
        .expect("the missed event must be replayed");
    let data: Value = serde_json::from_str(&replayed.data).expect("valid JSON data");
    assert_eq!(
        data["msgSerial"].as_i64(),
        Some(i(&missed, "msgSerial")),
        "replay must start strictly after the supplied cursor"
    );
    assert!(
        replayed.id.parse::<u64>().unwrap_or(0) > cursor.parse::<u64>().unwrap_or(u64::MAX),
        "the resumed cursor must advance: {} -> {}",
        cursor,
        replayed.id
    );
}

#[tokio::test]
async fn a_malformed_last_event_id_falls_back_to_live_tail() {
    // §8.3 step 1: malformed → treat as "no cursor", never an error.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "resume").await;
    add_member(&alice.api, &bob, &room).await;
    let ticket = request_ticket(&bob).await;

    let response = open_sse(&server, &format!("?ticket={ticket}"), Some("not-a-number")).await;

    assert_eq!(response.status().as_u16(), 200);
    let mut stream = Box::pin(response.bytes_stream().eventsource());
    send_message(&alice.api, &room, "live").await;
    assert!(
        next_sse_matching(&mut stream, EVENT_TIMEOUT, |e| e.event == "new_message")
            .await
            .is_some(),
        "the stream must still live-tail"
    );
}

#[tokio::test]
async fn the_stream_only_carries_rooms_the_caller_belongs_to() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let outsider = new_user(&server, "outsider").await;
    let room = create_room(&alice.api, "closed").await;
    let ticket = request_ticket(&outsider).await;
    let response = open_sse(&server, &format!("?ticket={ticket}"), None).await;
    let mut stream = Box::pin(response.bytes_stream().eventsource());

    send_message(&alice.api, &room, "not for you").await;

    assert!(
        next_sse_matching(&mut stream, SILENCE_WINDOW, |e| e.event == "new_message")
            .await
            .is_none(),
        "SSE applies the same membership rules as the WebSocket"
    );
}

#[tokio::test]
async fn typing_frames_carry_no_id_so_a_resume_never_replays_them() {
    // §8.4: ephemeral events are deliberately unnumbered.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "typing").await;
    add_member(&alice.api, &bob, &room).await;
    let ticket = request_ticket(&bob).await;
    let response = open_sse(&server, &format!("?ticket={ticket}"), None).await;
    let mut stream = Box::pin(response.bytes_stream().eventsource());
    let (mut alice_ws, _) = connect_subprotocol(&server, alice.api.token())
        .await
        .expect("connect");

    send_json(&mut alice_ws, json!({ "type": "typing", "roomId": room })).await;

    let event = next_sse_matching(&mut stream, EVENT_TIMEOUT, |e| e.event == "typing")
        .await
        .expect("typing reaches the SSE stream too");
    assert!(
        event.id.is_empty(),
        "a typing frame must not advance the resume cursor: id=`{}`",
        event.id
    );
    let data: Value = serde_json::from_str(&event.data).expect("valid JSON data");
    assert_eq!(data["from"].as_str(), Some(alice.address.as_str()));
}
