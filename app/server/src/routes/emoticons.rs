//! Reactions (`docs/API.md` §6.11).
//!
//! Reactions are not a separate table: each add or remove is an append-only
//! row in `messages` carrying `targetMessageId` and `emoticonCode`. That is
//! what lets them travel through `/sync` on the same cursor as everything
//! else, fold deterministically, and disappear with the room on a purge —
//! none of which a side table would give for free.

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, post};
use axum::{Json, Router};
use pocketskynet_core::RoomId;
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::db::{messages, rooms};
use crate::error::{ApiError, ApiResult};
use crate::validate::{self, ValidJson};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/messages/{messageId}/emoticons", post(add).get(aggregate))
        .route(
            "/messages/{messageId}/emoticons/{emoticonCode}",
            delete(remove),
        )
}

#[derive(Debug, Deserialize)]
struct AddBody {
    #[serde(rename = "emoticonCode")]
    emoticon_code: Option<String>,
}

/// Resolve the target message and check that the caller is in its room.
///
/// Reaction endpoints take a message id, not a room id, so the room has to be
/// discovered before it can be authorised against — a caller must not be able
/// to probe messages in rooms they are not in.
async fn target_room(
    state: &AppState,
    message_id: &str,
    caller: &str,
) -> ApiResult<(RoomId, String)> {
    let id = message_id.to_owned();
    let message = state
        .db
        .call(move |conn| messages::get_message(conn, &id))
        .await?
        .ok_or_else(|| ApiError::not_found("Message not found"))?;

    let room_id = message.room_id.clone();
    let address = caller.to_owned();
    let member = state
        .db
        .call({
            let room_id = room_id.clone();
            move |conn| rooms::is_member(conn, &room_id, &address)
        })
        .await?;
    if !member {
        return Err(ApiError::access_denied());
    }

    let room = RoomId::new(&room_id)
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("stored room id is not valid")))?;
    Ok((room, message.id))
}

/// `POST /api/messages/{messageId}/emoticons`.
///
/// Reacting twice with the same code simply appends another event: the
/// aggregation is set-based, so the visible result is identical. §15 #15 —
/// the reference had an unreachable "you have already added this emoticon"
/// branch; there is deliberately no duplicate check here to replace it.
async fn add(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(message_id): Path<String>,
    ValidJson(body): ValidJson<AddBody>,
) -> ApiResult<Response> {
    let id = validate::message_id(&message_id)?;
    let code = validate::emoticon_code(body.emoticon_code.as_deref())?;
    let (room, target) = target_room(&state, id.as_str(), caller.as_str()).await?;

    let event_id = format!("emoticon_{}_{}", crate::db::now_ms(), uuid::Uuid::new_v4());
    let room_id = room.as_str().to_owned();
    let sender = caller.as_str().to_owned();
    let mut event = state
        .db
        .call(move |conn| {
            messages::create_emoticon_event(
                conn, &event_id, &room_id, &sender, &target, &code, true,
            )
        })
        .await?;

    super::messages::announce(&state, &room, Some(&caller), event.msg_serial).await;
    // A bare `Message`: the reaction row has no `sender` in the wire shape,
    // and the caller already knows who reacted — it was them.
    event.sender = None;
    Ok(Json(event).into_response())
}

/// `DELETE /api/messages/{messageId}/emoticons/{emoticonCode}`.
///
/// §15 #14: the code is percent-decoded exactly once — axum's path extractor
/// does it. The reference decoded a second time by hand, which corrupted any
/// code containing a literal `%`.
///
/// Removing a reaction you never added is allowed and appends a no-op event;
/// refusing would require a read-then-write that the fold makes unnecessary.
async fn remove(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path((message_id, emoticon_code)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = validate::message_id(&message_id)?;
    let code = validate::emoticon_code(Some(&emoticon_code))?;
    let (room, target) = target_room(&state, id.as_str(), caller.as_str()).await?;

    let event_id = format!("emoticon_{}_{}", crate::db::now_ms(), uuid::Uuid::new_v4());
    let room_id = room.as_str().to_owned();
    let sender = caller.as_str().to_owned();
    let event = state
        .db
        .call(move |conn| {
            messages::create_emoticon_event(
                conn, &event_id, &room_id, &sender, &target, &code, false,
            )
        })
        .await?;

    super::messages::announce(&state, &room, Some(&caller), event.msg_serial).await;
    Ok(super::message("Emoticon removed successfully"))
}

/// `GET /api/messages/{messageId}/emoticons` — server-side aggregation.
///
/// §15 #10: the result is block-filtered for the viewer. The reference was
/// not, so a blocker saw blocked users listed as reactors here while the same
/// events were hidden from their `/sync` — two read surfaces disagreeing.
async fn aggregate(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(message_id): Path<String>,
) -> ApiResult<Response> {
    let id = validate::message_id(&message_id)?;
    let (_, target) = target_room(&state, id.as_str(), caller.as_str()).await?;

    let viewer = caller.as_str().to_owned();
    let out = state
        .db
        .call(move |conn| messages::aggregate_emoticons(conn, &target, &viewer))
        .await?;
    Ok(Json(out).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::build;
    use crate::test_support::{register, send, state, wallet};
    use axum::http::StatusCode;
    use axum::Router;

    async fn setup(tag: &str) -> (AppState, Router, String, String, String, String) {
        let state = state(tag);
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

        let message = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/messages"),
            Some(&alice_token),
            Some(serde_json::json!({ "content": "react to me", "msgHash": "a".repeat(64) })),
        )
        .await
        .json()["id"]
            .as_str()
            .unwrap()
            .to_owned();

        (state, router, alice_token, bob_token, room, message)
    }

    #[tokio::test]
    async fn reactions_aggregate_into_sets() {
        let (_, router, alice_token, bob_token, _, message) = setup("emoticon-agg").await;

        for token in [&alice_token, &bob_token] {
            let response = send(
                &router,
                "POST",
                &format!("/api/messages/{message}/emoticons"),
                Some(token),
                Some(serde_json::json!({ "emoticonCode": "🍎" })),
            )
            .await;
            assert_eq!(response.status, StatusCode::OK);
            assert_eq!(response.json()["msgType"], "emoticon_add");
            assert_eq!(response.json()["targetMessageId"], message);
            assert_eq!(response.json()["content"], "");
            assert!(
                response.json().get("sender").is_none(),
                "the created row carries no sender"
            );
        }

        let agg = send(
            &router,
            "GET",
            &format!("/api/messages/{message}/emoticons"),
            Some(&alice_token),
            None,
        )
        .await;
        assert_eq!(agg.json().as_array().unwrap().len(), 1);
        assert_eq!(agg.json()[0]["emoticonCode"], "🍎");
        assert_eq!(agg.json()[0]["count"], 2);
        assert_eq!(agg.json()[0]["users"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_duplicate_reaction_changes_nothing_visible() {
        let (_, router, alice_token, _, _, message) = setup("emoticon-dupe").await;

        for _ in 0..3 {
            send(
                &router,
                "POST",
                &format!("/api/messages/{message}/emoticons"),
                Some(&alice_token),
                Some(serde_json::json!({ "emoticonCode": "🍎" })),
            )
            .await;
        }

        let agg = send(
            &router,
            "GET",
            &format!("/api/messages/{message}/emoticons"),
            Some(&alice_token),
            None,
        )
        .await;
        assert_eq!(agg.json()[0]["count"], 1);
    }

    #[tokio::test]
    async fn removing_the_last_reactor_drops_the_code_entirely() {
        let (_, router, alice_token, _, _, message) = setup("emoticon-remove").await;

        send(
            &router,
            "POST",
            &format!("/api/messages/{message}/emoticons"),
            Some(&alice_token),
            Some(serde_json::json!({ "emoticonCode": "🍎" })),
        )
        .await;

        // Percent-encoded in the path, decoded exactly once (§15 #14).
        let removed = send(
            &router,
            "DELETE",
            &format!("/api/messages/{message}/emoticons/%F0%9F%8D%8E"),
            Some(&alice_token),
            None,
        )
        .await;
        assert_eq!(removed.status, StatusCode::OK);
        assert_eq!(removed.json()["message"], "Emoticon removed successfully");

        let agg = send(
            &router,
            "GET",
            &format!("/api/messages/{message}/emoticons"),
            Some(&alice_token),
            None,
        )
        .await;
        assert!(agg.json().as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn removing_a_reaction_you_never_added_is_harmless() {
        let (_, router, alice_token, bob_token, _, message) = setup("emoticon-noop").await;

        send(
            &router,
            "POST",
            &format!("/api/messages/{message}/emoticons"),
            Some(&alice_token),
            Some(serde_json::json!({ "emoticonCode": "🍎" })),
        )
        .await;

        let response = send(
            &router,
            "DELETE",
            &format!("/api/messages/{message}/emoticons/%F0%9F%8D%8E"),
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);

        let agg = send(
            &router,
            "GET",
            &format!("/api/messages/{message}/emoticons"),
            Some(&alice_token),
            None,
        )
        .await;
        assert_eq!(agg.json()[0]["count"], 1, "alice's reaction is untouched");
    }

    #[tokio::test]
    async fn aggregation_hides_reactors_the_viewer_blocked() {
        let (_, router, alice_token, bob_token, _, message) = setup("emoticon-block").await;

        send(
            &router,
            "POST",
            &format!("/api/messages/{message}/emoticons"),
            Some(&bob_token),
            Some(serde_json::json!({ "emoticonCode": "🍎" })),
        )
        .await;
        send(
            &router,
            "POST",
            "/api/users/block",
            Some(&alice_token),
            Some(serde_json::json!({ "address": wallet("bob").as_str() })),
        )
        .await;

        let blocker_view = send(
            &router,
            "GET",
            &format!("/api/messages/{message}/emoticons"),
            Some(&alice_token),
            None,
        )
        .await;
        // §15 #10: this used to disagree with the blocker's own /sync.
        assert!(blocker_view.json().as_array().unwrap().is_empty());

        let other_view = send(
            &router,
            "GET",
            &format!("/api/messages/{message}/emoticons"),
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(other_view.json().as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn reacting_requires_membership_of_the_targets_room() {
        let (state, router, _, _, _, message) = setup("emoticon-authz").await;
        let mallory = wallet("mallory");
        let mallory_token = register(&state, &mallory, "mallory");

        for (method, uri) in [
            ("POST", format!("/api/messages/{message}/emoticons")),
            ("GET", format!("/api/messages/{message}/emoticons")),
        ] {
            let body = (method == "POST").then(|| serde_json::json!({ "emoticonCode": "🍎" }));
            let response = send(&router, method, &uri, Some(&mallory_token), body).await;
            assert_eq!(response.status, StatusCode::FORBIDDEN);
        }
    }

    #[tokio::test]
    async fn reacting_to_a_missing_message_is_a_404() {
        let (_, router, alice_token, _, _, _) = setup("emoticon-missing").await;

        let response = send(
            &router,
            "POST",
            "/api/messages/msg_0000000000_gone/emoticons",
            Some(&alice_token),
            Some(serde_json::json!({ "emoticonCode": "🍎" })),
        )
        .await;
        assert_eq!(response.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_empty_or_oversized_code_is_refused() {
        let (_, router, alice_token, _, _, message) = setup("emoticon-validate").await;

        for code in ["", "   ", &"x".repeat(65)] {
            let response = send(
                &router,
                "POST",
                &format!("/api/messages/{message}/emoticons"),
                Some(&alice_token),
                Some(serde_json::json!({ "emoticonCode": code })),
            )
            .await;
            assert_eq!(response.status, StatusCode::BAD_REQUEST);
        }
    }
}
