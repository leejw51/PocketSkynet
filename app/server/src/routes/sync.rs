//! Incremental sync and read state (`docs/API.md` §6.12, §8, §13).
//!
//! `/sync` is the primary read path once a client is warm. It is a *state
//! transfer* stream, not an event log: an edit is delivered as the whole
//! updated row with a fresh serial, so a client folding from its high-water
//! mark always converges on the current state without replaying history.

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::db::{messages, rooms};
use crate::error::ApiResult;
use crate::validate::{self, ValidJson};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/rooms/{roomId}/sync", get(sync))
        .route("/rooms/{roomId}/latest-serial", get(latest_serial))
        .route("/rooms/{roomId}/latest-timestamp", get(latest_timestamp))
        .route("/rooms/{roomId}/read", post(mark_read))
}

#[derive(Debug, Deserialize)]
struct SyncQuery {
    since: Option<String>,
}

/// `GET /api/rooms/{roomId}/sync?since=<serial>` — members only.
///
/// Unlike `/messages`, nothing is filtered by type or `isDeleted`: tombstones,
/// purge markers and reaction events are all delivered, because that is
/// exactly what makes incremental folding correct. The only filter is the
/// viewer's block list.
///
/// `hasMore` rides in the `X-Has-More` header rather than the body so the
/// body stays a plain JSON array for clients that parse it positionally.
async fn sync(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
    Query(query): Query<SyncQuery>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    super::messages::require_member(&state, &room, &caller).await?;

    // A garbage cursor becomes 0 — a cold start, which is always safe because
    // the page size is bounded and the client drains with X-Has-More.
    let since = validate::optional_cursor(query.since.as_deref());

    let room_id = room.as_str().to_owned();
    let address = caller.as_str().to_owned();
    let (batch, has_more) = state
        .db
        .call(move |conn| messages::sync_messages(conn, &room_id, &address, since))
        .await?;

    Ok(super::with_has_more(Json(batch), has_more))
}

/// `GET /api/rooms/{roomId}/latest-serial` — a change detector.
///
/// Deliberately **not** block-filtered, so it may exceed the highest serial
/// the caller can actually see. Comparing it against a sync cursor to decide
/// "am I caught up" is therefore wrong: when a room's newest events are all
/// from blocked senders, the cursor legitimately never reaches it.
async fn latest_serial(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    super::messages::require_member(&state, &room, &caller).await?;

    let room_id = room.as_str().to_owned();
    let serial = state
        .db
        .call(move |conn| messages::latest_serial(conn, &room_id))
        .await?;
    Ok(Json(serde_json::json!({ "serial": serial })).into_response())
}

/// `GET /api/rooms/{roomId}/latest-timestamp` — legacy polling aid.
///
/// Kept for older clients; `latest-serial` is strictly better because it is
/// the same cursor `/sync` uses.
async fn latest_timestamp(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    super::messages::require_member(&state, &room, &caller).await?;

    let room_id = room.as_str().to_owned();
    let timestamp = state
        .db
        .call(move |conn| messages::latest_timestamp(conn, &room_id))
        .await?;
    Ok(Json(serde_json::json!({ "timestamp": timestamp })).into_response())
}

#[derive(Debug, Deserialize)]
struct ReadBody {
    #[serde(rename = "lastReadSerial")]
    last_read_serial: Option<i64>,
}

/// `POST /api/rooms/{roomId}/read` — advance the read pointer.
///
/// The response echoes the **stored** value, which may be higher than what
/// was sent: the pointer never moves backwards, so a lagging device cannot
/// resurrect badges another device already cleared.
async fn mark_read(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
    ValidJson(body): ValidJson<ReadBody>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    super::messages::require_member(&state, &room, &caller).await?;
    let serial = validate::serial("lastReadSerial", body.last_read_serial)?;

    let room_id = room.as_str().to_owned();
    let address = caller.as_str().to_owned();
    let stored = state
        .db
        .call(move |conn| rooms::mark_read(conn, &room_id, &address, serial))
        .await?;

    Ok(Json(serde_json::json!({
        "roomId": room.as_str(),
        "lastReadSerial": stored,
    }))
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::build;
    use crate::test_support::{register, send, state, wallet};
    use axum::http::StatusCode;
    use axum::Router;

    fn hash(seed: char) -> String {
        seed.to_string().repeat(64)
    }

    async fn post_message(
        router: &Router,
        token: &str,
        room: &str,
        text: &str,
    ) -> serde_json::Value {
        send(
            router,
            "POST",
            &format!("/api/rooms/{room}/messages"),
            Some(token),
            Some(serde_json::json!({ "content": text, "msgHash": hash('a') })),
        )
        .await
        .body
    }

    async fn setup(tag: &str) -> (crate::AppState, Router, String, String) {
        let state = state(tag);
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state.clone());
        let room = send(
            &router,
            "POST",
            "/api/rooms",
            Some(&token),
            Some(serde_json::json!({ "name": "Team" })),
        )
        .await
        .json()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        (state, router, token, room)
    }

    #[tokio::test]
    async fn sync_carries_the_has_more_header() {
        let (_, router, token, room) = setup("sync-header").await;
        post_message(&router, &token, &room, "hello").await;

        let response = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/sync?since=0"),
            Some(&token),
            None,
        )
        .await;

        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.headers["x-has-more"], "false");
        assert!(response.json().is_array(), "the body stays a plain array");
        assert_eq!(response.json().as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_cursor_is_exclusive() {
        let (_, router, token, room) = setup("sync-cursor").await;
        let first = post_message(&router, &token, &room, "one").await;
        let second = post_message(&router, &token, &room, "two").await;
        let cursor = first["msgSerial"].as_i64().unwrap();

        let response = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/sync?since={cursor}"),
            Some(&token),
            None,
        )
        .await;
        let items = response.json().as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], second["id"]);
    }

    #[tokio::test]
    async fn sync_delivers_tombstones_and_events() {
        let (_, router, token, room) = setup("sync-events").await;
        let message = post_message(&router, &token, &room, "hello").await;
        let id = message["id"].as_str().unwrap();

        send(
            &router,
            "POST",
            &format!("/api/messages/{id}/emoticons"),
            Some(&token),
            Some(serde_json::json!({ "emoticonCode": "🍎" })),
        )
        .await;
        send(
            &router,
            "DELETE",
            &format!("/api/messages/{id}"),
            Some(&token),
            None,
        )
        .await;

        let response = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/sync"),
            Some(&token),
            None,
        )
        .await;
        let types: Vec<&str> = response
            .json()
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["msgType"].as_str().unwrap())
            .collect();

        assert!(types.contains(&"emoticon_add"));
        assert!(types.contains(&"delete"), "the tombstone must be delivered");

        // …while /messages hides both.
        let listed = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/messages"),
            Some(&token),
            None,
        )
        .await;
        assert!(listed.json().as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn every_row_carries_a_sender_even_a_synthesised_one() {
        let (state, router, token, room) = setup("sync-sender").await;
        let ghost = "0x1234560000000000000000000000000000007890";
        state
            .db
            .call_blocking({
                let room = room.clone();
                move |conn| {
                    messages::create_message(
                        conn,
                        crate::db::messages::NewMessage {
                            id: "msg_ghost_00000001".into(),
                            room_id: room,
                            sender: ghost.into(),
                            content: "boo".into(),
                            msg_hash: "f".repeat(64),
                            is_encrypted: false,
                            iv: None,
                            hmac: None,
                            enc_ver: 1,
                            key_version: 1,
                            parent_message_id: None,
                            mentions: Vec::new(),
                        },
                    )?;
                    Ok(())
                }
            })
            .unwrap();

        let response = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/sync"),
            Some(&token),
            None,
        )
        .await;
        let sender = &response.json()[0]["sender"];
        assert_eq!(sender["username"], "User 0x1234...7890");
        // §15 #18: present as null in every code path, never omitted.
        assert!(sender.get("publicKeySig").is_some());
        assert!(sender["publicKeySig"].is_null());
    }

    #[tokio::test]
    async fn read_state_only_moves_forward() {
        let (_, router, token, room) = setup("read").await;

        let up = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/read"),
            Some(&token),
            Some(serde_json::json!({ "lastReadSerial": 100 })),
        )
        .await;
        assert_eq!(up.status, StatusCode::OK);
        assert_eq!(up.json()["lastReadSerial"], 100);
        assert_eq!(up.json()["roomId"], room);

        let back = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/read"),
            Some(&token),
            Some(serde_json::json!({ "lastReadSerial": 50 })),
        )
        .await;
        assert_eq!(
            back.json()["lastReadSerial"],
            100,
            "the response echoes the stored value, not the request"
        );
    }

    #[tokio::test]
    async fn an_out_of_range_serial_is_refused() {
        let (_, router, token, room) = setup("read-range").await;

        for serial in [-1i64, validate::MAX_SAFE_INT + 1] {
            let response = send(
                &router,
                "POST",
                &format!("/api/rooms/{room}/read"),
                Some(&token),
                Some(serde_json::json!({ "lastReadSerial": serial })),
            )
            .await;
            assert_eq!(response.status, StatusCode::BAD_REQUEST);
        }
    }

    #[tokio::test]
    async fn reading_clears_the_unread_badge() {
        let state = state("unread");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state.clone());
        let room = send(
            &router,
            "POST",
            "/api/rooms",
            Some(&alice_token),
            Some(serde_json::json!({ "name": "Team" })),
        )
        .await
        .json()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        state
            .db
            .call_blocking({
                let room = room.clone();
                let bob = bob.as_str().to_owned();
                move |conn| rooms::add_member(conn, &room, &bob)
            })
            .unwrap();

        let sent = post_message(&router, &bob_token, &room, "hi alice").await;

        let before = send(&router, "GET", "/api/rooms", Some(&alice_token), None).await;
        assert_eq!(before.json()[0]["unreadCount"], 1);
        // Your own messages never count.
        let bobs_view = send(&router, "GET", "/api/rooms", Some(&bob_token), None).await;
        assert_eq!(bobs_view.json()[0]["unreadCount"], 0);

        send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/read"),
            Some(&alice_token),
            Some(serde_json::json!({ "lastReadSerial": sent["msgSerial"] })),
        )
        .await;

        let after = send(&router, "GET", "/api/rooms", Some(&alice_token), None).await;
        assert_eq!(after.json()[0]["unreadCount"], 0);
    }

    #[tokio::test]
    async fn the_badge_ignores_senders_the_viewer_blocked() {
        let state = state("unread-blocked");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state.clone());
        let room = send(
            &router,
            "POST",
            "/api/rooms",
            Some(&alice_token),
            Some(serde_json::json!({ "name": "Team" })),
        )
        .await
        .json()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        state
            .db
            .call_blocking({
                let room = room.clone();
                let bob = bob.as_str().to_owned();
                move |conn| rooms::add_member(conn, &room, &bob)
            })
            .unwrap();

        send(
            &router,
            "POST",
            "/api/users/block",
            Some(&alice_token),
            Some(serde_json::json!({ "address": bob.as_str() })),
        )
        .await;
        post_message(&router, &bob_token, &room, "unreadable").await;

        let listed = send(&router, "GET", "/api/rooms", Some(&alice_token), None).await;
        // §15 #9: the badge used to promise a message /sync would never serve.
        assert_eq!(listed.json()[0]["unreadCount"], 0);
    }

    #[tokio::test]
    async fn latest_serial_and_timestamp_track_the_newest_row() {
        let (_, router, token, room) = setup("latest").await;

        let empty = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/latest-serial"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(empty.json()["serial"], 0);

        let sent = post_message(&router, &token, &room, "hi").await;

        let serial = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/latest-serial"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(serial.json()["serial"], sent["msgSerial"]);

        let timestamp = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/latest-timestamp"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(timestamp.json()["timestamp"], sent["messageTimestamp"]);
    }

    #[tokio::test]
    async fn every_sync_endpoint_is_member_only() {
        let (state, router, _, room) = setup("sync-authz").await;
        let mallory = wallet("mallory");
        let token = register(&state, &mallory, "mallory");

        for uri in [
            format!("/api/rooms/{room}/sync"),
            format!("/api/rooms/{room}/latest-serial"),
            format!("/api/rooms/{room}/latest-timestamp"),
        ] {
            let response = send(&router, "GET", &uri, Some(&token), None).await;
            assert_eq!(response.status, StatusCode::FORBIDDEN, "{uri}");
        }

        let read = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/read"),
            Some(&token),
            Some(serde_json::json!({ "lastReadSerial": 1 })),
        )
        .await;
        assert_eq!(read.status, StatusCode::FORBIDDEN);
    }
}
