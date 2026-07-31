//! Sending, reading, editing, deleting and anchoring messages
//! (`docs/API.md` §6.10).

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{patch, post};
use axum::{Json, Router};
use pocketskynet_core::{RoomId, ServerEvent, Target, WalletAddress};
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::db::messages::{MessageEdit, NewMessage};
use crate::db::{messages, rooms};
use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::validate::{self, ValidJson};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/rooms/{roomId}/messages",
            post(send).get(list).delete(purge),
        )
        .route("/messages/{messageId}", patch(edit).delete(remove))
        .route("/messages/{messageId}/publish", post(publish))
        .merge(super::emoticons::router())
        .merge(super::sync::router())
}

/// The message body, shared by send and edit.
#[derive(Debug, Deserialize)]
pub struct MessageBody {
    pub content: Option<String>,
    #[serde(rename = "msgHash")]
    pub msg_hash: Option<String>,
    #[serde(rename = "isEncrypted")]
    pub is_encrypted: Option<bool>,
    pub iv: Option<String>,
    pub hmac: Option<String>,
    #[serde(rename = "encVer")]
    pub enc_ver: Option<i64>,
    #[serde(rename = "keyVersion")]
    pub key_version: Option<i64>,
}

pub(super) async fn require_member(
    state: &AppState,
    room: &RoomId,
    caller: &WalletAddress,
) -> ApiResult<()> {
    let room_id = room.as_str().to_owned();
    let address = caller.as_str().to_owned();
    let member = state
        .db
        .call(move |conn| rooms::is_member(conn, &room_id, &address))
        .await?;
    if member {
        Ok(())
    } else {
        Err(ApiError::access_denied())
    }
}

/// The forward-secrecy gate, applied to anything that writes ciphertext.
///
/// Two refusals, both 409 with a machine-readable `code` because the client is
/// expected to *act* rather than display them:
///
/// * `KEY_ROTATION_REQUIRED` — somebody left and the room has not been
///   re-keyed. A departed member still holds the current key, so accepting
///   ciphertext under it would let them read messages sent after they left.
/// * `STALE_KEY_VERSION` — the sender sealed under an old epoch. Storing it
///   would leave a message that current members cannot read.
async fn check_epoch(state: &AppState, room: &RoomId, key_version: i64) -> ApiResult<()> {
    let room_id = room.as_str().to_owned();
    let record = state
        .db
        .call(move |conn| rooms::get_room(conn, &room_id))
        .await?
        .ok_or_else(|| ApiError::not_found("Room not found"))?;

    if record.key_rotation_pending {
        return Err(ApiError::key_conflict(
            ErrorCode::KeyRotationRequired,
            "Room key rotation is pending — an admin must rotate the key before new encrypted messages can be sent.",
            record.current_key_version,
        ));
    }
    if key_version != record.current_key_version {
        return Err(ApiError::key_conflict(
            ErrorCode::StaleKeyVersion,
            "Message key version does not match the room's current epoch — refetch keys and retry.",
            record.current_key_version,
        ));
    }
    Ok(())
}

/// `POST /api/rooms/{roomId}/messages` — members only.
///
/// Blocked users can still post: the block is applied when the blocker
/// *reads*, so the rest of the room is unaffected. Filtering on write would
/// tell the sender they had been blocked.
async fn send(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
    ValidJson(body): ValidJson<MessageBody>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    require_member(&state, &room, &caller).await?;

    let content = validate::message_content(body.content.as_deref())?;
    let msg_hash = validate::msg_hash(body.msg_hash.as_deref())?;
    let iv = validate::message_iv(body.iv.as_deref())?;
    let hmac = validate::message_hmac(body.hmac.as_deref())?;
    let enc_ver = validate::enc_ver(body.enc_ver, 1)?;
    let key_version = validate::key_version("keyVersion", body.key_version, 1)?;
    let is_encrypted = body.is_encrypted.unwrap_or(false);

    // Unencrypted rooms skip the epoch machinery entirely — the room is not
    // even fetched, which is what keeps plaintext rooms cheap.
    if is_encrypted {
        check_epoch(&state, &room, key_version).await?;
    }

    let new = NewMessage {
        id: format!("msg_{}_{}", crate::db::now_ms(), uuid::Uuid::new_v4()),
        room_id: room.as_str().to_owned(),
        sender: caller.as_str().to_owned(),
        content,
        msg_hash,
        is_encrypted,
        iv,
        hmac,
        enc_ver,
        key_version,
    };
    let message = state
        .db
        .call(move |conn| messages::create_message(conn, new))
        .await?;

    announce(&state, &room, &caller, message.msg_serial).await;
    Ok(Json(message).into_response())
}

#[derive(Debug, Deserialize)]
struct PaginationQuery {
    since: Option<String>,
    before: Option<String>,
    limit: Option<String>,
}

/// `GET /api/rooms/{roomId}/messages` — initial load and backward paging.
///
/// `since`/`before` are **timestamps** here; `/sync`'s `since` is a serial.
/// Mixing them up is the most common client bug against this API.
async fn list(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
    Query(query): Query<PaginationQuery>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    require_member(&state, &room, &caller).await?;

    let since = validate::optional_cursor(query.since.as_deref());
    let before = validate::optional_cursor(query.before.as_deref());
    let limit = validate::page_limit(query.limit.as_deref());

    let room_id = room.as_str().to_owned();
    let address = caller.as_str().to_owned();
    let out = state
        .db
        .call(move |conn| messages::get_messages(conn, &room_id, &address, since, before, limit))
        .await?;
    Ok(Json(out).into_response())
}

/// `PATCH /api/messages/{messageId}` — the sender only.
///
/// Two divergences from the reference, both from §15 #7:
///
/// * The epoch gate applies here as well. The reference checked it only on
///   send, so an edit could write ciphertext under a stale epoch that a fresh
///   message would have been refused for.
/// * An encrypted message cannot be edited into plaintext by omitting `iv`
///   and `hmac`. The reference silently downgraded it, dropping the room's
///   end-to-end guarantee for that message without telling anybody.
async fn edit(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(message_id): Path<String>,
    ValidJson(body): ValidJson<MessageBody>,
) -> ApiResult<Response> {
    let id = validate::message_id(&message_id)?;

    let existing = {
        let id = id.as_str().to_owned();
        state
            .db
            .call(move |conn| messages::get_message(conn, &id))
            .await?
            .ok_or_else(|| ApiError::not_found("Message not found"))?
    };

    let room = RoomId::new(&existing.room_id)
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("stored room id is not valid")))?;

    {
        let room_id = existing.room_id.clone();
        let address = caller.as_str().to_owned();
        let member = state
            .db
            .call(move |conn| rooms::is_member(conn, &room_id, &address))
            .await?;
        if !member {
            return Err(ApiError::forbidden("Not a member of this room"));
        }
    }
    if existing.sender_address != caller.as_str() {
        return Err(ApiError::forbidden(
            "Only the message owner can edit this message",
        ));
    }

    let content = validate::message_content(body.content.as_deref())?;
    let msg_hash = validate::msg_hash(body.msg_hash.as_deref())?;
    let iv = validate::message_iv(body.iv.as_deref())?;
    let hmac = validate::message_hmac(body.hmac.as_deref())?;
    let enc_ver = validate::enc_ver(body.enc_ver, 1)?;
    let key_version = validate::key_version("keyVersion", body.key_version, 1)?;
    let stays_encrypted = iv.is_some() && hmac.is_some();

    if existing.is_encrypted && !stays_encrypted {
        return Err(ApiError::bad_request(
            "An encrypted message must be edited with iv and hmac; omitting them would store it as plaintext",
        ));
    }
    if stays_encrypted {
        check_epoch(&state, &room, key_version).await?;
    }

    let updated = state
        .db
        .call({
            let id = id.as_str().to_owned();
            let room_id = existing.room_id.clone();
            move |conn| {
                messages::update_message(
                    conn,
                    &id,
                    &room_id,
                    MessageEdit {
                        content,
                        msg_hash,
                        iv,
                        hmac,
                        enc_ver,
                        key_version,
                    },
                )
            }
        })
        .await?
        .ok_or_else(|| ApiError::not_found("Message not found or unauthorized"))?;

    announce(&state, &room, &caller, updated.msg_serial).await;
    Ok(Json(updated).into_response())
}

/// `DELETE /api/messages/{messageId}` — **any** member of the room.
///
/// Deliberate: this is a "forgetting-first" product, and a member who wants
/// something gone from a room they are in should not have to ask its author.
/// The row survives only as a scrubbed tombstone so offline clients learn to
/// drop it.
async fn remove(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(message_id): Path<String>,
) -> ApiResult<Response> {
    let id = validate::message_id(&message_id)?;

    let existing = {
        let id = id.as_str().to_owned();
        state
            .db
            .call(move |conn| messages::get_message(conn, &id))
            .await?
            .ok_or_else(|| ApiError::not_found("Message not found"))?
    };
    let room = RoomId::new(&existing.room_id)
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("stored room id is not valid")))?;

    {
        let room_id = existing.room_id.clone();
        let address = caller.as_str().to_owned();
        let member = state
            .db
            .call(move |conn| rooms::is_member(conn, &room_id, &address))
            .await?;
        if !member {
            return Err(ApiError::forbidden("Not a member of this room"));
        }
    }

    let deleted = state
        .db
        .call({
            let id = id.as_str().to_owned();
            let room_id = existing.room_id.clone();
            move |conn| messages::soft_delete_message(conn, &id, &room_id)
        })
        .await?;
    if !deleted {
        return Err(ApiError::not_found("Message not found or unauthorized"));
    }

    let serial = {
        let room_id = existing.room_id.clone();
        state
            .db
            .call(move |conn| messages::latest_serial(conn, &room_id))
            .await?
    };
    announce(&state, &room, &caller, serial).await;
    Ok(super::message("Message deleted successfully"))
}

/// `DELETE /api/rooms/{roomId}/messages` — any member may purge the history.
///
/// A single marker row survives so `/sync` clients learn to clear their local
/// cache; without it, a client that was offline during the purge would keep
/// rendering messages the server no longer holds.
async fn purge(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;

    {
        let room_id = room.as_str().to_owned();
        let address = caller.as_str().to_owned();
        let member = state
            .db
            .call(move |conn| rooms::is_member(conn, &room_id, &address))
            .await?;
        if !member {
            return Err(ApiError::forbidden("Not a member of this room"));
        }
    }

    let marker_id = format!("msg_{}_{}", crate::db::now_ms(), uuid::Uuid::new_v4());
    let room_id = room.as_str().to_owned();
    let address = caller.as_str().to_owned();
    let (deleted, marker) = state
        .db
        .call(move |conn| messages::delete_all_messages(conn, &room_id, &address, &marker_id))
        .await?;

    let _ = state.log.append_audit(
        "room_history_purged",
        Some(&caller),
        serde_json::json!({ "roomId": room.as_str(), "deletedCount": deleted }),
    );

    announce(&state, &room, &caller, marker.msg_serial).await;

    Ok(Json(serde_json::json!({
        "message": "All messages deleted successfully",
        "deletedCount": deleted,
    }))
    .into_response())
}

#[derive(Debug, Deserialize)]
struct PublishBody {
    #[serde(rename = "txHash")]
    tx_hash: Option<String>,
    #[serde(rename = "toAddress")]
    to_address: Option<String>,
}

fn is_tx_hash(value: &str) -> bool {
    value.len() == 66
        && value.starts_with("0x")
        && value[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// `POST /api/messages/{messageId}/publish` — record an on-chain anchor.
///
/// §15 #13: the reference answered 400 for everything, including "not found"
/// and "not your message", and could anchor an already-deleted message. Here
/// the codes are the accurate ones and a deleted message is never publishable.
///
/// **Not implemented:** on-chain verification of the transaction. The
/// reference optionally fetched the transaction through `FN_RPC_URL` and
/// checked that its calldata contained the message hash. Doing that here would
/// mean shipping an HTTP client and a JSON-RPC layer in the server's runtime
/// dependencies purely for one endpoint; the format and recipient checks below
/// still run, and a client that cares about provenance can verify the anchor
/// itself from the returned `txHash`.
async fn publish(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(message_id): Path<String>,
    ValidJson(body): ValidJson<PublishBody>,
) -> ApiResult<Response> {
    let id = validate::message_id(&message_id)?;

    let tx_hash = body
        .tx_hash
        .as_deref()
        .filter(|v| is_tx_hash(v))
        .ok_or_else(|| ApiError::bad_request("Invalid transaction hash format"))?
        .to_owned();
    let to_address = body
        .to_address
        .as_deref()
        .and_then(|v| WalletAddress::new(v).ok())
        .ok_or_else(|| ApiError::bad_request("Invalid to address format"))?;

    let server_wallet = super::misc::server_wallet();
    if server_wallet.is_empty() {
        return Err(ApiError::bad_request(
            "Publishing hash failed: this server has no anchor wallet configured",
        ));
    }
    if !server_wallet.eq_ignore_ascii_case(to_address.as_str()) {
        return Err(ApiError::bad_request(
            "Publishing hash failed: transaction recipient does not match server wallet",
        ));
    }

    let existing = {
        let id = id.as_str().to_owned();
        state
            .db
            .call(move |conn| messages::get_message(conn, &id))
            .await?
            .ok_or_else(|| ApiError::not_found("Message not found"))?
    };
    if existing.sender_address != caller.as_str() {
        return Err(ApiError::forbidden(
            "Only the message sender can publish a transaction hash",
        ));
    }
    if existing.tx_hash.is_some() {
        return Err(ApiError::conflict("Message already has a transaction hash"));
    }

    let room = RoomId::new(&existing.room_id)
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("stored room id is not valid")))?;

    let updated = state
        .db
        .call({
            let id = id.as_str().to_owned();
            let room_id = existing.room_id.clone();
            move |conn| messages::publish_tx_hash(conn, &id, &room_id, &tx_hash)
        })
        .await?
        .ok_or_else(|| ApiError::not_found("Message not found"))?;

    // The reference emitted nothing here, which meant the anchor only reached
    // other clients on their next poll even though its serial had already
    // advanced for exactly that purpose.
    announce(&state, &room, &caller, updated.msg_serial).await;
    Ok(Json(updated).into_response())
}

/// Wake the room. `origin` is the actor, so members who blocked them get no
/// notification at all rather than one followed by an empty sync.
pub(super) async fn announce(
    state: &AppState,
    room: &RoomId,
    origin: &WalletAddress,
    msg_serial: i64,
) {
    state
        .hub
        .publish_best_effort(
            Target::Room {
                room_id: room.clone(),
            },
            Some(origin.clone()),
            ServerEvent::NewMessage {
                room_id: room.clone(),
                msg_serial,
            },
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::build;
    use crate::test_support::{register, send as request, state, wallet};
    use axum::http::StatusCode;
    use axum::Router;

    fn hash(seed: char) -> String {
        seed.to_string().repeat(64)
    }

    async fn make_room(router: &Router, token: &str) -> String {
        request(
            router,
            "POST",
            "/api/rooms",
            Some(token),
            Some(serde_json::json!({ "name": "Team" })),
        )
        .await
        .json()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn post_message(
        router: &Router,
        token: &str,
        room: &str,
        text: &str,
    ) -> serde_json::Value {
        request(
            router,
            "POST",
            &format!("/api/rooms/{room}/messages"),
            Some(token),
            Some(serde_json::json!({ "content": text, "msgHash": hash('a') })),
        )
        .await
        .body
    }

    #[tokio::test]
    async fn sending_returns_the_message_with_its_sender() {
        let state = state("send");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);
        let room = make_room(&router, &token).await;

        let message = post_message(&router, &token, &room, "  hello  ").await;

        assert_eq!(message["content"], "hello", "content is trimmed");
        assert_eq!(message["msgType"], "add");
        assert_eq!(message["senderAddress"], alice.as_str());
        assert_eq!(message["sender"]["username"], "alice");
        assert_eq!(message["isDeleted"], false);
        assert!(message["msgSerial"].as_i64().unwrap() > 0);
        assert!(message["txHash"].is_null());
    }

    #[tokio::test]
    async fn a_non_member_can_neither_send_nor_read() {
        let state = state("send-authz");
        let alice = wallet("alice");
        let mallory = wallet("mallory");
        let alice_token = register(&state, &alice, "alice");
        let mallory_token = register(&state, &mallory, "mallory");
        let router = build(state);
        let room = make_room(&router, &alice_token).await;

        let sent = request(
            &router,
            "POST",
            &format!("/api/rooms/{room}/messages"),
            Some(&mallory_token),
            Some(serde_json::json!({ "content": "hi", "msgHash": hash('a') })),
        )
        .await;
        assert_eq!(sent.status, StatusCode::FORBIDDEN);

        let read = request(
            &router,
            "GET",
            &format!("/api/rooms/{room}/messages"),
            Some(&mallory_token),
            None,
        )
        .await;
        assert_eq!(read.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_bad_hash_or_empty_content_is_a_validation_error() {
        let state = state("send-validate");
        let token = register(&state, &wallet("alice"), "alice");
        let router = build(state);
        let room = make_room(&router, &token).await;

        for body in [
            serde_json::json!({ "content": "hi" }),
            serde_json::json!({ "content": "hi", "msgHash": "short" }),
            serde_json::json!({ "content": "hi", "msgHash": "A".repeat(64) }),
            serde_json::json!({ "content": "   ", "msgHash": hash('a') }),
            serde_json::json!({ "msgHash": hash('a') }),
        ] {
            let response = request(
                &router,
                "POST",
                &format!("/api/rooms/{room}/messages"),
                Some(&token),
                Some(body),
            )
            .await;
            assert_eq!(response.status, StatusCode::BAD_REQUEST);
            assert_eq!(response.json()["message"], "Validation failed");
        }
    }

    #[tokio::test]
    async fn encrypted_sends_are_gated_on_the_room_epoch() {
        let state = state("epoch-gate");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state.clone());
        let room = make_room(&router, &token).await;

        let stale = request(
            &router,
            "POST",
            &format!("/api/rooms/{room}/messages"),
            Some(&token),
            Some(serde_json::json!({
                "content": "ciphertext",
                "msgHash": hash('a'),
                "isEncrypted": true,
                "iv": "f".repeat(32),
                "hmac": hash('e'),
                "encVer": 2,
                "keyVersion": 7,
            })),
        )
        .await;
        assert_eq!(stale.status, StatusCode::CONFLICT);
        assert_eq!(stale.json()["code"], "STALE_KEY_VERSION");
        assert_eq!(stale.json()["currentKeyVersion"], 1);

        // A departure flags the room; encrypted sends stop until a re-key.
        state
            .db
            .call_blocking({
                let room = room.clone();
                move |conn| rooms::set_key_rotation_pending(conn, &room, true)
            })
            .unwrap();

        let pending = request(
            &router,
            "POST",
            &format!("/api/rooms/{room}/messages"),
            Some(&token),
            Some(serde_json::json!({
                "content": "ciphertext",
                "msgHash": hash('a'),
                "isEncrypted": true,
                "iv": "f".repeat(32),
                "hmac": hash('e'),
                "keyVersion": 1,
            })),
        )
        .await;
        assert_eq!(pending.status, StatusCode::CONFLICT);
        assert_eq!(pending.json()["code"], "KEY_ROTATION_REQUIRED");

        // Plaintext is unaffected — the room is not even fetched.
        let plain = post_message(&router, &token, &room, "still fine").await;
        assert_eq!(plain["msgType"], "add");
    }

    #[tokio::test]
    async fn edits_are_owner_only_and_keep_the_row() {
        let state = state("edit");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state.clone());
        let room = make_room(&router, &alice_token).await;
        state
            .db
            .call_blocking({
                let room = room.clone();
                let bob = bob.as_str().to_owned();
                move |conn| rooms::add_member(conn, &room, &bob)
            })
            .unwrap();

        let original = post_message(&router, &alice_token, &room, "before").await;
        let id = original["id"].as_str().unwrap();

        let by_other = request(
            &router,
            "PATCH",
            &format!("/api/messages/{id}"),
            Some(&bob_token),
            Some(serde_json::json!({ "content": "hijacked", "msgHash": hash('b') })),
        )
        .await;
        assert_eq!(by_other.status, StatusCode::FORBIDDEN);
        assert_eq!(
            by_other.json()["message"],
            "Only the message owner can edit this message"
        );

        let edited = request(
            &router,
            "PATCH",
            &format!("/api/messages/{id}"),
            Some(&alice_token),
            Some(serde_json::json!({ "content": "after", "msgHash": hash('b') })),
        )
        .await;
        assert_eq!(edited.status, StatusCode::OK);
        assert_eq!(edited.json()["id"], id);
        assert_eq!(edited.json()["msgType"], "edit");
        assert!(edited.json()["editedAt"].is_string());
        assert!(
            edited.json()["msgSerial"].as_i64().unwrap() > original["msgSerial"].as_i64().unwrap()
        );
    }

    #[tokio::test]
    async fn an_encrypted_message_cannot_be_edited_into_plaintext() {
        let state = state("edit-downgrade");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);
        let room = make_room(&router, &token).await;

        let encrypted = request(
            &router,
            "POST",
            &format!("/api/rooms/{room}/messages"),
            Some(&token),
            Some(serde_json::json!({
                "content": "ciphertext",
                "msgHash": hash('a'),
                "isEncrypted": true,
                "iv": "f".repeat(32),
                "hmac": hash('e'),
                "keyVersion": 1,
            })),
        )
        .await;
        assert_eq!(encrypted.status, StatusCode::OK);
        let id = encrypted.json()["id"].as_str().unwrap().to_owned();

        // §15 #7: the reference stored this as plaintext without a word.
        let downgrade = request(
            &router,
            "PATCH",
            &format!("/api/messages/{id}"),
            Some(&token),
            Some(serde_json::json!({ "content": "plain", "msgHash": hash('b') })),
        )
        .await;
        assert_eq!(downgrade.status, StatusCode::BAD_REQUEST);

        let kept = request(
            &router,
            "PATCH",
            &format!("/api/messages/{id}"),
            Some(&token),
            Some(serde_json::json!({
                "content": "newcipher",
                "msgHash": hash('b'),
                "iv": "e".repeat(32),
                "hmac": hash('d'),
                "keyVersion": 1,
            })),
        )
        .await;
        assert_eq!(kept.status, StatusCode::OK);
        assert_eq!(kept.json()["isEncrypted"], true);
    }

    #[tokio::test]
    async fn an_edit_under_a_stale_epoch_is_refused_like_a_send() {
        let state = state("edit-epoch");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);
        let room = make_room(&router, &token).await;

        let original = post_message(&router, &token, &room, "plain").await;
        let id = original["id"].as_str().unwrap();

        // §15 #7: edits bypassed the gate entirely in the reference.
        let response = request(
            &router,
            "PATCH",
            &format!("/api/messages/{id}"),
            Some(&token),
            Some(serde_json::json!({
                "content": "cipher",
                "msgHash": hash('b'),
                "iv": "f".repeat(32),
                "hmac": hash('e'),
                "keyVersion": 9,
            })),
        )
        .await;
        assert_eq!(response.status, StatusCode::CONFLICT);
        assert_eq!(response.json()["code"], "STALE_KEY_VERSION");
    }

    #[tokio::test]
    async fn any_member_may_delete_any_message() {
        let state = state("delete");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state.clone());
        let room = make_room(&router, &alice_token).await;
        state
            .db
            .call_blocking({
                let room = room.clone();
                let bob = bob.as_str().to_owned();
                move |conn| rooms::add_member(conn, &room, &bob)
            })
            .unwrap();

        let message = post_message(&router, &alice_token, &room, "delete me").await;
        let id = message["id"].as_str().unwrap();

        // Deliberately not owner-gated.
        let deleted = request(
            &router,
            "DELETE",
            &format!("/api/messages/{id}"),
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(deleted.status, StatusCode::OK);

        let again = request(
            &router,
            "DELETE",
            &format!("/api/messages/{id}"),
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(again.status, StatusCode::NOT_FOUND);

        let listed = request(
            &router,
            "GET",
            &format!("/api/rooms/{room}/messages"),
            Some(&alice_token),
            None,
        )
        .await;
        assert!(listed.json().as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn purging_reports_the_count_and_leaves_one_marker() {
        let state = state("purge");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);
        let room = make_room(&router, &token).await;

        for i in 0..3 {
            post_message(&router, &token, &room, &format!("m{i}")).await;
        }

        let purged = request(
            &router,
            "DELETE",
            &format!("/api/rooms/{room}/messages"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(purged.status, StatusCode::OK);
        assert_eq!(purged.json()["deletedCount"], 3);

        let listed = request(
            &router,
            "GET",
            &format!("/api/rooms/{room}/messages"),
            Some(&token),
            None,
        )
        .await;
        assert!(listed.json().as_array().unwrap().is_empty());

        let synced = request(
            &router,
            "GET",
            &format!("/api/rooms/{room}/sync"),
            Some(&token),
            None,
        )
        .await;
        let events = synced.json().as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["msgType"], "delete_all");
    }

    #[tokio::test]
    async fn pagination_reads_newest_first_and_returns_ascending() {
        let state = state("paginate");
        let token = register(&state, &wallet("alice"), "alice");
        let router = build(state);
        let room = make_room(&router, &token).await;

        for i in 0..5 {
            post_message(&router, &token, &room, &format!("m{i}")).await;
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }

        let page = request(
            &router,
            "GET",
            &format!("/api/rooms/{room}/messages?limit=3"),
            Some(&token),
            None,
        )
        .await;
        let items = page.json().as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["content"], "m2", "the newest three, ascending");
        assert_eq!(items[2]["content"], "m4");

        let oldest = items[0]["messageTimestamp"].as_i64().unwrap();
        let older = request(
            &router,
            "GET",
            &format!("/api/rooms/{room}/messages?before={oldest}&limit=3"),
            Some(&token),
            None,
        )
        .await;
        let items = older.json().as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["content"], "m0");
    }

    #[tokio::test]
    async fn a_garbage_cursor_disables_the_filter_rather_than_failing() {
        let state = state("paginate-garbage");
        let token = register(&state, &wallet("alice"), "alice");
        let router = build(state);
        let room = make_room(&router, &token).await;
        post_message(&router, &token, &room, "hi").await;

        let response = request(
            &router,
            "GET",
            &format!("/api/rooms/{room}/messages?since=abc&limit=notanumber"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.json().as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn publishing_needs_a_configured_wallet_and_the_right_sender() {
        let state = state("publish");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state.clone());
        let room = make_room(&router, &alice_token).await;
        state
            .db
            .call_blocking({
                let room = room.clone();
                let bob = bob.as_str().to_owned();
                move |conn| rooms::add_member(conn, &room, &bob)
            })
            .unwrap();

        let message = post_message(&router, &alice_token, &room, "anchor me").await;
        let id = message["id"].as_str().unwrap();

        let bad_hash = request(
            &router,
            "POST",
            &format!("/api/messages/{id}/publish"),
            Some(&alice_token),
            Some(serde_json::json!({ "txHash": "0x1234", "toAddress": alice.as_str() })),
        )
        .await;
        assert_eq!(bad_hash.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            bad_hash.json()["message"],
            "Invalid transaction hash format"
        );

        let bad_to = request(
            &router,
            "POST",
            &format!("/api/messages/{id}/publish"),
            Some(&alice_token),
            Some(serde_json::json!({
                "txHash": format!("0x{}", "ab".repeat(32)),
                "toAddress": "0xnope",
            })),
        )
        .await;
        assert_eq!(bad_to.json()["message"], "Invalid to address format");

        // With no server wallet configured the anchor cannot be validated.
        let unconfigured = request(
            &router,
            "POST",
            &format!("/api/messages/{id}/publish"),
            Some(&bob_token),
            Some(serde_json::json!({
                "txHash": format!("0x{}", "ab".repeat(32)),
                "toAddress": bob.as_str(),
            })),
        )
        .await;
        assert_eq!(unconfigured.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_message_id_that_cannot_exist_is_a_validation_error() {
        let state = state("badmsgid");
        let token = register(&state, &wallet("alice"), "alice");
        let router = build(state);

        let response = request(&router, "DELETE", "/api/messages/short", Some(&token), None).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);

        let dotted = request(
            &router,
            "PATCH",
            "/api/messages/msg.with.dots.here",
            Some(&token),
            Some(serde_json::json!({ "content": "x", "msgHash": hash('a') })),
        )
        .await;
        assert_eq!(dotted.status, StatusCode::BAD_REQUEST);
    }
}
