//! The mentions inbox (`docs/API.md` §6.13).
//!
//! Everything that named the caller, across every room they are still in,
//! newest first. There is no write endpoint and no "mark as read": a mention
//! is read when the room it lives in is read, which is a pointer the client
//! already advances (`POST /api/rooms/{id}/read`). Giving mentions their own
//! read state would create a second thing to keep in step with the first, and
//! the two would drift the first time a client crashed between the two calls.

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::db::mentions;
use crate::error::ApiResult;
use crate::AppState;

/// The inbox page size, and its ceiling.
///
/// The inbox is a triage surface, not an archive: what matters is everything
/// recent, and a caller who wants the whole history of being named has the
/// rooms themselves. Fifty is roughly a screen and a half.
const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 200;

pub fn router() -> Router<AppState> {
    Router::new().route("/mentions", get(list))
}

#[derive(Debug, Deserialize)]
struct LimitQuery {
    limit: Option<String>,
}

/// `GET /api/mentions` — the caller's mentions, newest first.
///
/// Scoped by *current* membership, so leaving a room takes its mentions out of
/// the inbox rather than leaving entries that 403 when opened.
async fn list(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Query(query): Query<LimitQuery>,
) -> ApiResult<Response> {
    let limit = query
        .limit
        .as_deref()
        .and_then(|raw| raw.trim().parse::<i64>().ok())
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);

    let address = caller.as_str().to_owned();
    let out = state
        .db
        .call(move |conn| mentions::inbox(conn, &address, limit))
        .await?;
    Ok(Json(out).into_response())
}

#[cfg(test)]
mod tests {
    use crate::routes::build;
    use crate::test_support::{register, send, state, wallet};
    use axum::http::StatusCode;

    #[tokio::test]
    async fn a_mention_reaches_the_inbox_and_clears_when_the_room_is_read() {
        let state = state("mentions-inbox");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state);

        // A DM is the quickest two-member room to build, and mentions work the
        // same in one as in a channel.
        let room = send(
            &router,
            "POST",
            "/api/rooms/dm",
            Some(&alice_token),
            Some(serde_json::json!({ "walletAddress": bob.as_str() })),
        )
        .await
        .json()["id"]
            .as_str()
            .unwrap()
            .to_owned();

        let sent = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/messages"),
            Some(&alice_token),
            Some(serde_json::json!({
                "content": "morning @bob, can you look at this",
                "msgHash": "a".repeat(64),
            })),
        )
        .await;
        assert_eq!(sent.status, StatusCode::OK);
        let serial = sent.json()["msgSerial"].as_i64().unwrap();

        let inbox = send(&router, "GET", "/api/mentions", Some(&bob_token), None).await;
        assert_eq!(inbox.status, StatusCode::OK);
        let entries = inbox.json();
        let entries = entries.as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["roomId"], room);
        assert_eq!(entries[0]["isUnread"], true);
        assert_eq!(entries[0]["message"]["senderAddress"], alice.as_str());

        // Alice mentioned Bob, not herself: her own inbox stays empty.
        let hers = send(&router, "GET", "/api/mentions", Some(&alice_token), None).await;
        assert!(hers.json().as_array().unwrap().is_empty());

        // The room list carries the badge.
        let rooms = send(&router, "GET", "/api/rooms", Some(&bob_token), None).await;
        assert_eq!(rooms.json()[0]["mentionCount"], 1);

        // Reading the room is what clears it — there is no second pointer.
        send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/read"),
            Some(&bob_token),
            Some(serde_json::json!({ "lastReadSerial": serial })),
        )
        .await;

        let inbox = send(&router, "GET", "/api/mentions", Some(&bob_token), None).await;
        assert_eq!(
            inbox.json()[0]["isUnread"],
            false,
            "the entry stays, it just stops being new"
        );
        let rooms = send(&router, "GET", "/api/rooms", Some(&bob_token), None).await;
        assert_eq!(rooms.json()[0]["mentionCount"], 0);
    }

    #[tokio::test]
    async fn an_encrypted_message_mentions_through_the_declared_list() {
        // The server holds ciphertext here and can parse nothing out of it,
        // so the client's `mentions` array is the only source — and it is
        // still checked against the room's roster before it counts.
        let state = state("mentions-encrypted");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let carol = wallet("carol");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let carol_token = register(&state, &carol, "carol");
        let router = build(state);

        let room = send(
            &router,
            "POST",
            "/api/rooms/dm",
            Some(&alice_token),
            Some(serde_json::json!({ "walletAddress": bob.as_str() })),
        )
        .await
        .json()["id"]
            .as_str()
            .unwrap()
            .to_owned();

        let sent = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/messages"),
            Some(&alice_token),
            Some(serde_json::json!({
                // Opaque to the server, as an encrypted body is.
                "content": "9tYbG0mQ2sZ1Xh==",
                "msgHash": "b".repeat(64),
                // Carol is not in this room, so naming her must do nothing —
                // a mention that reached her would leak the room's existence.
                "mentions": [bob.as_str(), carol.as_str()],
            })),
        )
        .await;
        assert_eq!(sent.status, StatusCode::OK);

        let bobs = send(&router, "GET", "/api/mentions", Some(&bob_token), None).await;
        assert_eq!(bobs.json().as_array().unwrap().len(), 1);

        let carols = send(&router, "GET", "/api/mentions", Some(&carol_token), None).await;
        assert!(
            carols.json().as_array().unwrap().is_empty(),
            "a non-member cannot be mentioned into a room"
        );
    }

    #[tokio::test]
    async fn deleting_a_message_takes_its_mention_out_of_the_inbox() {
        let state = state("mentions-deleted");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state);

        let room = send(
            &router,
            "POST",
            "/api/rooms/dm",
            Some(&alice_token),
            Some(serde_json::json!({ "walletAddress": bob.as_str() })),
        )
        .await
        .json()["id"]
            .as_str()
            .unwrap()
            .to_owned();

        let message = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/messages"),
            Some(&alice_token),
            Some(serde_json::json!({
                "content": "@bob look at this",
                "msgHash": "c".repeat(64),
            })),
        )
        .await
        .json()["id"]
            .as_str()
            .unwrap()
            .to_owned();

        assert_eq!(
            send(&router, "GET", "/api/mentions", Some(&bob_token), None)
                .await
                .json()
                .as_array()
                .unwrap()
                .len(),
            1
        );

        send(
            &router,
            "DELETE",
            &format!("/api/messages/{message}"),
            Some(&alice_token),
            None,
        )
        .await;

        let inbox = send(&router, "GET", "/api/mentions", Some(&bob_token), None).await;
        assert!(
            inbox.json().as_array().unwrap().is_empty(),
            "the content is gone, so the pointer into it must be too"
        );
    }

    #[tokio::test]
    async fn editing_a_mention_away_removes_it() {
        let state = state("mentions-edited");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state);

        let room = send(
            &router,
            "POST",
            "/api/rooms/dm",
            Some(&alice_token),
            Some(serde_json::json!({ "walletAddress": bob.as_str() })),
        )
        .await
        .json()["id"]
            .as_str()
            .unwrap()
            .to_owned();

        let message = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/messages"),
            Some(&alice_token),
            Some(serde_json::json!({
                "content": "@bob one more thing",
                "msgHash": "d".repeat(64),
            })),
        )
        .await
        .json()["id"]
            .as_str()
            .unwrap()
            .to_owned();

        send(
            &router,
            "PATCH",
            &format!("/api/messages/{message}"),
            Some(&alice_token),
            Some(serde_json::json!({
                "content": "never mind, sorted it",
                "msgHash": "e".repeat(64),
            })),
        )
        .await;

        let inbox = send(&router, "GET", "/api/mentions", Some(&bob_token), None).await;
        assert!(
            inbox.json().as_array().unwrap().is_empty(),
            "the message no longer names Bob, so his inbox must not claim it does"
        );
    }
}
