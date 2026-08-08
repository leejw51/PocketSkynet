//! Incoming webhooks (`docs/API.md` §17) — the minimum integration surface.
//!
//! Two very different routers live here. The management routes are ordinary
//! wallet-authenticated admin verbs: create, list, revoke. The post route is
//! the odd one out in the whole API — no wallet, no JWT, the token in the URL
//! *is* the auth — which is why it is mounted outside the general limiter
//! under its own budget ([`crate::ratelimit::Scope::Webhook`]) and why every
//! failure to present a valid token is the same 404.
//!
//! Plaintext rooms only, checked at create **and** at post time. Not policy —
//! arithmetic: a webhook holds no room key, so anything it posted into an
//! encrypted room would be plaintext sitting in a room whose members promised
//! each other ciphertext. The double check matters because a room can be keyed
//! *after* a webhook is created; the post-time check is what keeps that
//! ordering from quietly downgrading the room.

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, post};
use axum::{Json, Router};
use pocketskynet_core::RoomId;
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::db::messages::NewMessage;
use crate::db::webhooks::MAX_WEBHOOKS_PER_ROOM;
use crate::db::{keys, messages, rooms, webhooks};
use crate::error::{ApiError, ApiResult};
use crate::validate::{self, ValidJson};
use crate::AppState;

/// The admin-facing management routes, mounted with the general limiter.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/rooms/{roomId}/webhooks", post(create).get(list))
        .route("/rooms/{roomId}/webhooks/{webhookId}", delete(revoke))
}

/// The unauthenticated post route, mounted under its own rate-limit budget.
pub fn post_router() -> Router<AppState> {
    Router::new().route("/webhooks/{token}", post(post_message))
}

#[derive(Debug, Deserialize)]
struct CreateBody {
    name: Option<String>,
}

/// What an external system sends: one field, so that "post to this URL" fits
/// in a CI config's one-liner. Anything richer (usernames, blocks, colours)
/// is a formatting language, and messages here are plain text by design.
#[derive(Debug, Deserialize)]
struct PostBody {
    text: Option<String>,
}

/// The admin gate shared by all three management verbs.
///
/// Same shape as `rooms::require_admin`, but the room lookup and the decision
/// both happen inside the caller's database closure so create can go on to
/// check encryption and the cap against the same connection.
fn check_admin(
    conn: &rusqlite::Connection,
    room_id: &str,
    caller: &str,
    is_server_admin: bool,
) -> ApiResult<()> {
    let Some(record) = rooms::get_room(conn, room_id)? else {
        return Err(ApiError::not_found("Room not found"));
    };
    // A DM is the conversation between the people in it. Wiring a feed into
    // one turns it into something neither person opened; a channel is free
    // and says what it is.
    if record.is_direct() {
        return Err(ApiError::bad_request(
            "Webhooks cannot post into direct messages. Use a channel instead.",
        ));
    }
    if !is_server_admin && !rooms::is_admin(conn, room_id, caller)? {
        return Err(ApiError::forbidden("Only room admins can manage webhooks"));
    }
    Ok(())
}

/// `POST /api/rooms/{roomId}/webhooks` — admins only, plaintext channels only.
///
/// The response carries the token and the URL. This is not "shown once":
/// listing returns them too, because the admin who lost the CI config is the
/// same admin who may re-copy the URL — see the schema comment on why the
/// token is stored as issued.
async fn create(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
    ValidJson(body): ValidJson<CreateBody>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    let name = validate::webhook_name(body.name.as_deref())?;

    let id = format!("hook_{}_{}", crate::db::now_ms(), uuid::Uuid::new_v4());
    let token = format!("whk_{}", crate::auth::random_hex_32()?);
    let is_server_admin = super::misc::is_server_admin(caller.as_str());
    let room_id = room.as_str().to_owned();
    let creator = caller.as_str().to_owned();

    let webhook = state
        .db
        .call(move |conn| {
            check_admin(conn, &room_id, &creator, is_server_admin)?;
            // The plaintext-only rule, first of its two enforcement points.
            // `has_encryption` is the same fact the client's lock icon reads:
            // somebody holds a wrapped key for this room.
            if keys::has_encryption(conn, &room_id)? {
                return Err(ApiError::bad_request(
                    "This room is end-to-end encrypted. Webhooks hold no room key, so they can only post into plaintext rooms.",
                ));
            }
            if webhooks::count_for_room(conn, &room_id)? >= MAX_WEBHOOKS_PER_ROOM {
                return Err(ApiError::bad_request(
                    "This room already has the maximum number of webhooks. Revoke one first.",
                ));
            }
            webhooks::create(conn, &id, &room_id, &name, &token, &creator)
        })
        .await?;

    Ok(Json(webhook).into_response())
}

/// `GET /api/rooms/{roomId}/webhooks` — admins only, tokens included.
///
/// Admin-only rather than member-visible on purpose: the list *is* the
/// credentials, and a member who can read a token can post as the webhook.
async fn list(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    let is_server_admin = super::misc::is_server_admin(caller.as_str());
    let room_id = room.as_str().to_owned();
    let address = caller.as_str().to_owned();

    let out = state
        .db
        .call(move |conn| {
            check_admin(conn, &room_id, &address, is_server_admin)?;
            webhooks::list_for_room(conn, &room_id)
        })
        .await?;
    Ok(Json(out).into_response())
}

/// `DELETE /api/rooms/{roomId}/webhooks/{webhookId}` — admins only.
///
/// Revocation is the row's deletion and nothing else, which is exactly why it
/// is immediate: the post handler re-reads the table on every request, so
/// there is no cached session or unexpired credential to wait out. Old posts
/// keep their attribution — the webhook's identity row survives, the way a
/// departed member's name stays on their history.
async fn revoke(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path((room_id, webhook_id)): Path<(String, String)>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    let is_server_admin = super::misc::is_server_admin(caller.as_str());
    let room_id = room.as_str().to_owned();
    let address = caller.as_str().to_owned();

    let deleted = state
        .db
        .call(move |conn| {
            check_admin(conn, &room_id, &address, is_server_admin)?;
            webhooks::delete(conn, &room_id, &webhook_id)
        })
        .await?;
    if !deleted {
        return Err(ApiError::not_found("Webhook not found"));
    }
    Ok(super::message("Webhook revoked"))
}

/// `POST /api/webhooks/{token}` — no wallet auth; the token is the auth.
///
/// The message then takes the ordinary path end to end: same insert, same
/// serial allocation, same mention and media extraction from plaintext, same
/// search indexing, same fan-out — so every connected client sees a webhook
/// post arrive exactly as it sees a person's.
///
/// Every way of not presenting a currently-valid token — malformed, unknown,
/// revoked — is the same 404. A distinct "revoked" answer would tell whoever
/// is still holding a leaked token that it used to work, which is a fact they
/// have no business confirming.
async fn post_message(
    State(state): State<AppState>,
    Path(token): Path<String>,
    ValidJson(body): ValidJson<PostBody>,
) -> ApiResult<Response> {
    let Some(token) = validate::webhook_token(&token) else {
        return Err(ApiError::not_found("Unknown webhook"));
    };
    let content = validate::message_content(body.text.as_deref())?;
    // The server computes the hash a client would have: the webhook caller is
    // a shell script, and asking it to ship a SHA-256 would only teach every
    // integration to compute it wrong once.
    let msg_hash = pocketskynet_core::msg_hash_plaintext(&content);

    let id = format!("msg_{}_{}", crate::db::now_ms(), uuid::Uuid::new_v4());
    let token = token.to_owned();
    let message = state
        .db
        .call(move |conn| {
            let Some(webhook) = webhooks::find_by_token(conn, &token)? else {
                return Err(ApiError::not_found("Unknown webhook"));
            };
            // The plaintext-only rule's second enforcement point: the room
            // may have been keyed since this webhook was created, and the
            // webhook must go dark at that moment, not keep writing plaintext
            // into a room that has since promised ciphertext.
            if keys::has_encryption(conn, &webhook.room_id)? {
                return Err(ApiError::conflict(
                    "This room is now end-to-end encrypted; webhooks can no longer post into it",
                ));
            }
            // Plaintext, so the same server-side extraction people's messages
            // get: an `@name` in a CI failure notice becomes a real mention,
            // and a hosted image URL keeps the purge accounting honest.
            let mentions = super::messages::resolve_mentions(
                conn,
                &webhook.room_id,
                &content,
                false,
                Vec::new(),
            )?;
            let media = super::messages::resolve_media(&content, false, Vec::new());

            messages::create_message(
                conn,
                NewMessage {
                    id,
                    room_id: webhook.room_id.clone(),
                    sender: webhook.sender_address,
                    content,
                    msg_hash,
                    is_encrypted: false,
                    iv: None,
                    hmac: None,
                    enc_ver: 1,
                    key_version: 1,
                    parent_message_id: None,
                    mentions,
                    media,
                },
            )
        })
        .await?;

    let room = RoomId::new(&message.room_id)
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("stored room id is not valid")))?;
    // `None` origin: no wallet acted, so nobody's block list mutes the wake-up.
    super::messages::announce(&state, &room, None, message.msg_serial).await;
    Ok(Json(message).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::build;
    use crate::test_support::{register, send, state, wallet};
    use axum::http::StatusCode;
    use axum::Router;

    async fn make_room(router: &Router, token: &str) -> String {
        send(
            router,
            "POST",
            "/api/rooms",
            Some(token),
            Some(serde_json::json!({ "name": "Ops" })),
        )
        .await
        .json()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn make_webhook(
        router: &Router,
        token: &str,
        room: &str,
        name: &str,
    ) -> serde_json::Value {
        send(
            router,
            "POST",
            &format!("/api/rooms/{room}/webhooks"),
            Some(token),
            Some(serde_json::json!({ "name": name })),
        )
        .await
        .body
    }

    /// Key a room the way a client would: one self-wrap is enough to make
    /// `hasEncryption` true, which is the fact the webhook rules read.
    async fn encrypt_room(router: &Router, token: &str, room: &str, address: &str) {
        let stored = send(
            router,
            "POST",
            &format!("/api/rooms/{room}/keys"),
            Some(token),
            Some(serde_json::json!({
                "userAddress": address,
                "encryptedSymmetricKey": "wrapped",
                "ephemeralPublicKey": "04ab",
                "encryptionIV": "1a2b3c4d5e6f78901234567890abcdef",
                "hmac": "9".repeat(64),
                "keyVersion": 1,
            })),
        )
        .await;
        assert_eq!(stored.status, StatusCode::OK, "{:?}", stored.body);
    }

    #[tokio::test]
    async fn a_webhook_post_is_an_ordinary_message_with_a_webhook_identity() {
        let state = state("webhook-post");
        let alice = wallet("alice");
        let alice_token = register(&state, &alice, "alice");
        let router = build(state);
        let room = make_room(&router, &alice_token).await;

        let hook = make_webhook(&router, &alice_token, &room, "CI").await;
        assert!(hook["token"].as_str().unwrap().starts_with("whk_"));
        assert_eq!(
            hook["url"],
            format!("/api/webhooks/{}", hook["token"].as_str().unwrap())
        );
        let sender = hook["senderAddress"].as_str().unwrap();
        assert!(
            sender.starts_with(pocketskynet_core::WEBHOOK_SENDER_PREFIX),
            "the reserved prefix is the attribution: {sender}"
        );

        // No Authorization header: the token is the whole credential.
        let posted = send(
            &router,
            "POST",
            hook["url"].as_str().unwrap(),
            None,
            Some(serde_json::json!({ "text": "build #42 green" })),
        )
        .await;
        assert_eq!(posted.status, StatusCode::OK, "{:?}", posted.body);
        assert_eq!(posted.json()["senderAddress"], sender);
        assert!(posted.json()["msgSerial"].as_i64().unwrap() > 0);

        // Members read it through the normal path, named and badged.
        let listed = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/messages"),
            Some(&alice_token),
            None,
        )
        .await;
        let listed = listed.json();
        let msg = &listed.as_array().unwrap()[0];
        assert_eq!(msg["content"], "build #42 green");
        assert_eq!(msg["senderAddress"], sender);
        assert_eq!(
            msg["sender"]["username"], "CI",
            "the webhook's name is its display identity"
        );
    }

    #[tokio::test]
    async fn webhook_management_is_admin_only() {
        let state = state("webhook-admin");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state);
        let room = make_room(&router, &alice_token).await;

        // Bob joins as an ordinary member.
        send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/invite"),
            Some(&alice_token),
            Some(serde_json::json!({ "userAddress": bob.as_str() })),
        )
        .await;
        send(
            &router,
            "POST",
            &format!("/api/invitations/{room}/accept"),
            Some(&bob_token),
            None,
        )
        .await;

        for (method, path) in [
            ("POST", format!("/api/rooms/{room}/webhooks")),
            ("GET", format!("/api/rooms/{room}/webhooks")),
            ("DELETE", format!("/api/rooms/{room}/webhooks/hook_x")),
        ] {
            let refused = send(
                &router,
                method,
                &path,
                Some(&bob_token),
                (method == "POST").then(|| serde_json::json!({ "name": "CI" })),
            )
            .await;
            assert_eq!(
                refused.status,
                StatusCode::FORBIDDEN,
                "{method} {path} must be admin-only"
            );
            assert_eq!(
                refused.json()["message"],
                "Only room admins can manage webhooks"
            );
        }

        // The admin's list works — and carries the token, which is the point
        // of restricting it.
        make_webhook(&router, &alice_token, &room, "CI").await;
        let listed = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/webhooks"),
            Some(&alice_token),
            None,
        )
        .await;
        assert_eq!(listed.json().as_array().unwrap().len(), 1);
        assert!(listed.json()[0]["token"]
            .as_str()
            .unwrap()
            .starts_with("whk_"));
    }

    #[tokio::test]
    async fn an_encrypted_room_refuses_webhooks_at_create_time() {
        let state = state("webhook-e2ee-create");
        let alice = wallet("alice");
        let alice_token = register(&state, &alice, "alice");
        let router = build(state);
        let room = make_room(&router, &alice_token).await;
        encrypt_room(&router, &alice_token, &room, alice.as_str()).await;

        let refused = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/webhooks"),
            Some(&alice_token),
            Some(serde_json::json!({ "name": "CI" })),
        )
        .await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);
        assert!(refused.json()["message"]
            .as_str()
            .unwrap()
            .contains("end-to-end encrypted"));
    }

    #[tokio::test]
    async fn keying_a_room_silences_its_existing_webhooks() {
        // The ordering the create-time check cannot see: webhook first,
        // encryption second. The post-time check is what catches it.
        let state = state("webhook-e2ee-post");
        let alice = wallet("alice");
        let alice_token = register(&state, &alice, "alice");
        let router = build(state);
        let room = make_room(&router, &alice_token).await;
        let hook = make_webhook(&router, &alice_token, &room, "CI").await;
        let url = hook["url"].as_str().unwrap().to_owned();

        // Worked while the room was plaintext…
        let before = send(
            &router,
            "POST",
            &url,
            None,
            Some(serde_json::json!({ "text": "still plaintext" })),
        )
        .await;
        assert_eq!(before.status, StatusCode::OK);

        // …and goes dark the moment somebody holds a room key.
        encrypt_room(&router, &alice_token, &room, alice.as_str()).await;
        let after = send(
            &router,
            "POST",
            &url,
            None,
            Some(serde_json::json!({ "text": "must not land" })),
        )
        .await;
        assert_eq!(after.status, StatusCode::CONFLICT);
        assert!(after.json()["message"]
            .as_str()
            .unwrap()
            .contains("end-to-end encrypted"));
    }

    #[tokio::test]
    async fn revocation_takes_effect_on_the_next_post() {
        let state = state("webhook-revoke");
        let alice = wallet("alice");
        let alice_token = register(&state, &alice, "alice");
        let router = build(state);
        let room = make_room(&router, &alice_token).await;
        let hook = make_webhook(&router, &alice_token, &room, "CI").await;
        let url = hook["url"].as_str().unwrap().to_owned();
        let id = hook["id"].as_str().unwrap().to_owned();

        let ok = send(
            &router,
            "POST",
            &url,
            None,
            Some(serde_json::json!({ "text": "one" })),
        )
        .await;
        assert_eq!(ok.status, StatusCode::OK);

        let revoked = send(
            &router,
            "DELETE",
            &format!("/api/rooms/{room}/webhooks/{id}"),
            Some(&alice_token),
            None,
        )
        .await;
        assert_eq!(revoked.status, StatusCode::OK);

        // Same 404 as a token that never existed — a leaked-then-revoked
        // token must not confirm that it used to work.
        let dead = send(
            &router,
            "POST",
            &url,
            None,
            Some(serde_json::json!({ "text": "two" })),
        )
        .await;
        assert_eq!(dead.status, StatusCode::NOT_FOUND);
        assert_eq!(dead.json()["message"], "Unknown webhook");

        // Revoking again: the id names nothing now.
        let again = send(
            &router,
            "DELETE",
            &format!("/api/rooms/{room}/webhooks/{id}"),
            Some(&alice_token),
            None,
        )
        .await;
        assert_eq!(again.status, StatusCode::NOT_FOUND);

        // The one post that landed keeps its name after the revoke.
        let listed = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/messages"),
            Some(&alice_token),
            None,
        )
        .await;
        assert_eq!(listed.json()[0]["sender"]["username"], "CI");
    }

    #[tokio::test]
    async fn unknown_and_malformed_tokens_are_the_same_404() {
        let state = state("webhook-guess");
        let router = build(state);

        for guess in [
            format!("whk_{}", "0".repeat(64)), // well-formed, never issued
            "whk_short".to_owned(),            // wrong length
            format!("whk_{}", "G".repeat(64)), // not hex
            "not-a-token".to_owned(),          // wrong prefix
        ] {
            let refused = send(
                &router,
                "POST",
                &format!("/api/webhooks/{guess}"),
                None,
                Some(serde_json::json!({ "text": "hello" })),
            )
            .await;
            assert_eq!(refused.status, StatusCode::NOT_FOUND, "token {guess:?}");
            assert_eq!(refused.json()["message"], "Unknown webhook");
        }
    }

    #[tokio::test]
    async fn a_dm_refuses_webhooks() {
        let state = state("webhook-dm");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        register(&state, &bob, "bob");
        let router = build(state);

        let dm = send(
            &router,
            "POST",
            "/api/rooms/dm",
            Some(&alice_token),
            Some(serde_json::json!({ "walletAddress": bob.as_str() })),
        )
        .await;
        let dm_id = dm.json()["id"].as_str().unwrap().to_owned();

        let refused = send(
            &router,
            "POST",
            &format!("/api/rooms/{dm_id}/webhooks"),
            Some(&alice_token),
            Some(serde_json::json!({ "name": "CI" })),
        )
        .await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);
        assert!(refused.json()["message"]
            .as_str()
            .unwrap()
            .contains("direct messages"));
    }

    #[tokio::test]
    async fn webhook_posts_draw_on_their_own_rate_budget() {
        use std::sync::Arc;

        let mut state = state("webhook-ratelimit");
        state.limiter = Arc::new(crate::ratelimit::RateLimiter::new(true));
        let alice = wallet("alice");
        let alice_token = register(&state, &alice, "alice");
        let router = build(state);
        let room = make_room(&router, &alice_token).await;
        let hook = make_webhook(&router, &alice_token, &room, "CI").await;
        let url = hook["url"].as_str().unwrap().to_owned();

        let budget = crate::ratelimit::Scope::Webhook.max_per_minute();
        let mut refused = 0;
        for i in 0..budget + 5 {
            let response = send(
                &router,
                "POST",
                &url,
                None,
                Some(serde_json::json!({ "text": format!("post {i}") })),
            )
            .await;
            if response.status == StatusCode::TOO_MANY_REQUESTS {
                refused += 1;
                assert_eq!(
                    response.json()["message"],
                    "Too many webhook posts, please slow down"
                );
            }
        }
        assert_eq!(refused, 5, "the budget is its own, and it is enforced");
    }

    #[tokio::test]
    async fn a_webhook_mentioning_a_member_lands_in_their_inbox() {
        // Plaintext goes through the same @-extraction a person's message
        // does, so "@alice the build broke" actually reaches alice.
        let state = state("webhook-mention");
        let alice = wallet("alice");
        let alice_token = register(&state, &alice, "alice");
        let router = build(state);
        let room = make_room(&router, &alice_token).await;
        let hook = make_webhook(&router, &alice_token, &room, "CI").await;

        send(
            &router,
            "POST",
            hook["url"].as_str().unwrap(),
            None,
            Some(serde_json::json!({ "text": "@alice the build broke" })),
        )
        .await;

        let inbox = send(&router, "GET", "/api/mentions", Some(&alice_token), None).await;
        let inbox = inbox.json();
        let items = inbox.as_array().unwrap();
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0]["message"]["content"], "@alice the build broke");
    }

    #[tokio::test]
    async fn the_webhook_cap_is_enforced() {
        let state = state("webhook-cap");
        let alice = wallet("alice");
        let alice_token = register(&state, &alice, "alice");
        let router = build(state);
        let room = make_room(&router, &alice_token).await;

        for i in 0..MAX_WEBHOOKS_PER_ROOM {
            let made = send(
                &router,
                "POST",
                &format!("/api/rooms/{room}/webhooks"),
                Some(&alice_token),
                Some(serde_json::json!({ "name": format!("feed {i}") })),
            )
            .await;
            assert_eq!(made.status, StatusCode::OK);
        }

        let over = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/webhooks"),
            Some(&alice_token),
            Some(serde_json::json!({ "name": "one too many" })),
        )
        .await;
        assert_eq!(over.status, StatusCode::BAD_REQUEST);
        assert!(over.json()["message"].as_str().unwrap().contains("maximum"));
    }

    #[tokio::test]
    async fn webhook_identities_do_not_appear_in_user_search() {
        let state = state("webhook-search");
        let alice = wallet("alice");
        let alice_token = register(&state, &alice, "alice");
        let router = build(state);
        let room = make_room(&router, &alice_token).await;
        make_webhook(&router, &alice_token, &room, "Nightly CI").await;

        let found = send(
            &router,
            "GET",
            "/api/users/search?q=Nightly",
            Some(&alice_token),
            None,
        )
        .await;
        assert!(
            found.json().as_array().unwrap().is_empty(),
            "a webhook is not somebody to DM, invite or block"
        );
    }
}
