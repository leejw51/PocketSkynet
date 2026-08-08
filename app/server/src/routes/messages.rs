//! Sending, reading, editing, deleting and anchoring messages
//! (`docs/API.md` §6.10).

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use pocketskynet_core::{RoomId, ServerEvent, Target, WalletAddress};
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::db::messages::{MessageEdit, NewMessage};
use crate::db::{mentions, messages, rooms};
use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::validate::{self, ValidJson};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/rooms/{roomId}/messages",
            post(send).get(list).delete(purge),
        )
        .route("/rooms/{roomId}/agent", post(agent_reply))
        .route("/messages/{messageId}", patch(edit).delete(remove))
        .route("/messages/{messageId}/thread", get(thread))
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
    /// Post into the thread this message belongs to. Send only; an edit
    /// cannot move a message between threads, because the reply it is an
    /// answer to would then be answering something else.
    #[serde(rename = "parentMessageId")]
    pub parent_message_id: Option<String>,
    /// The people this message names, as wallet addresses.
    ///
    /// Sent by the client rather than left entirely to the server's parser for
    /// two reasons. Usernames may contain spaces and emoji, which no `@token`
    /// grammar recovers from plaintext; and in an encrypted room there *is* no
    /// plaintext — the server holds ciphertext and must keep holding only
    /// that. Declaring the addresses leaks nothing the room does not already
    /// publish: the server knows exactly who is in every room, so "this
    /// message names Bob, who is in this room" adds no fact it did not have.
    /// What stays private is the thing that matters — what was said.
    ///
    /// Advisory, not trusted: every address is checked against the room's
    /// roster before it becomes a mention.
    pub mentions: Option<Vec<String>>,
    /// The hosted files this message shows — `{sha256}.{ext}` names under
    /// `data/images/`, as returned by `POST /api/images`.
    ///
    /// Declared for the same reason mentions are, and only for encrypted
    /// rooms in practice: a picture in a plaintext message is a link the
    /// server can read for itself, and one in an encrypted message is not.
    /// Recording it is what lets destroying a room destroy the pictures it
    /// showed rather than orphaning them on disk (`db/media.rs`).
    ///
    /// Advisory and additive: a name here only ever ties bytes to *this* room.
    pub media: Option<Vec<String>>,
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
        // The built-in rooms hold no keys (`routes/keys.rs::plaintext_only`),
        // so ciphertext here could only be a client sealing under a key nobody
        // else has. Refused at the door rather than left to fail the epoch
        // check, whose message would blame a rotation that cannot happen.
        if super::rooms::fetch_room(&state, &room).await?.is_static() {
            return Err(ApiError::conflict(
                "Built-in rooms are plaintext so their contents stay searchable; \
                 they cannot carry encrypted messages.",
            ));
        }
        check_epoch(&state, &room, key_version).await?;
    }

    // Validated here so a malformed id is a 400 before any database work,
    // rather than a "message not found" from inside the transaction.
    let parent = body
        .parent_message_id
        .as_deref()
        .map(validate::message_id)
        .transpose()?;
    let declared = validate::mention_addresses(body.mentions)?;
    let declared_media = validate::media_names(body.media)?;

    let id = format!("msg_{}_{}", crate::db::now_ms(), uuid::Uuid::new_v4());
    let room_id = room.as_str().to_owned();
    let sender = caller.as_str().to_owned();
    let message = state
        .db
        .call(move |conn| {
            let parent_message_id = match parent {
                Some(parent) => Some(resolve_parent(conn, parent.as_str(), &room_id)?),
                None => None,
            };
            let mentions = resolve_mentions(conn, &room_id, &content, is_encrypted, declared)?;
            let media = resolve_media(&content, is_encrypted, declared_media);

            messages::create_message(
                conn,
                NewMessage {
                    id,
                    room_id,
                    sender,
                    content,
                    msg_hash,
                    is_encrypted,
                    iv,
                    hmac,
                    enc_ver,
                    key_version,
                    parent_message_id,
                    mentions,
                    media,
                },
            )
        })
        .await?;

    announce(&state, &room, Some(&caller), message.msg_serial).await;
    Ok(Json(message).into_response())
}

#[derive(Debug, Deserialize)]
struct AgentBody {
    text: Option<String>,
}

/// `POST /api/rooms/{roomId}/agent` — post the AI's reply into "My Jarvis".
///
/// # Why the client sends the answer instead of the server fetching it
///
/// The user's API keys live in their browser and nowhere else
/// (`web/src/ai.rs`), and the search feature already established the shape:
/// retrieval here, generation there, with the model call made from the device
/// that holds the credential (`docs/SEARCH.md` §5). Making the server call the
/// model would mean either shipping it the key on every turn or asking the
/// operator to configure one for everybody — the first turns a self-hosted
/// messenger into a credential store, the second makes "your own AI agent" the
/// operator's agent. Neither is the product.
///
/// So the browser talks to the model and hands the text back, and this endpoint
/// exists for the one thing the browser cannot do: write a message under an
/// address that is not the caller's. It is the same trick incoming webhooks
/// use, with the same reasoning — a reply needs a `senderAddress`, and the
/// alternative was a per-message "this one was the AI" flag that every read
/// path would have to learn.
///
/// # What stops this being a way to forge messages
///
/// Three conditions, all necessary. The room must be of kind `jarvis`; its id
/// must be the one [`rooms::static_room_id`] derives for *the caller*, so a
/// wallet cannot post into somebody else's agent room even knowing its id; and
/// the sender written is [`WalletAddress::agent_of`] the caller — never a value
/// from the request. The most a caller can do with this endpoint is put words
/// in their own agent's mouth, in a room only they can read.
async fn agent_reply(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
    ValidJson(body): ValidJson<AgentBody>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    let expected = rooms::static_room_id(crate::db::models::ROOM_KIND_JARVIS, caller.as_str());
    if room.as_str() != expected {
        // Deliberately the same refusal a non-member gets anywhere else: a
        // distinct "that is not your agent room" would confirm which room ids
        // are somebody's, which is exactly the oracle `require_member` avoids.
        return Err(ApiError::access_denied());
    }
    require_member(&state, &room, &caller).await?;

    let content = validate::message_content(body.text.as_deref())?;
    // Computed here for the same reason a webhook's is: the caller is relaying
    // somebody else's words and has no business asserting their hash.
    let msg_hash = pocketskynet_core::msg_hash_plaintext(&content);

    let id = format!("msg_{}_{}", crate::db::now_ms(), uuid::Uuid::new_v4());
    let room_id = room.as_str().to_owned();
    let sender = WalletAddress::agent_of(&caller).as_str().to_owned();
    let message = state
        .db
        .call(move |conn| {
            let mentions = resolve_mentions(conn, &room_id, &content, false, Vec::new())?;
            let media = resolve_media(&content, false, Vec::new());
            messages::create_message(
                conn,
                NewMessage {
                    id,
                    room_id,
                    sender,
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

    // `None` origin: no wallet acted, so nobody's block list mutes the wake-up
    // — and the caller's other tabs are exactly who needs to see this land.
    announce(&state, &room, None, message.msg_serial).await;
    Ok(Json(message).into_response())
}

/// The thread a reply belongs in, given the message the client replied to.
///
/// Flattens: replying to a reply joins its thread rather than nesting under
/// it, so `parent_message_id` is always a thread root and a thread is always
/// one flat list (see `db::messages::thread_root`).
///
/// Cross-room parents are refused rather than silently ignored. Accepting one
/// would file a message in room A into a thread in room B, where members of B
/// would read it without being in A — a membership check bypassed by a typo.
fn resolve_parent(
    conn: &rusqlite::Connection,
    parent_id: &str,
    room_id: &str,
) -> ApiResult<String> {
    let (root, parent_room) = messages::thread_root(conn, parent_id, false)?
        .ok_or_else(|| ApiError::not_found("The message being replied to no longer exists"))?;
    if parent_room != room_id {
        return Err(ApiError::bad_request(
            "Cannot reply to a message in another room",
        ));
    }
    Ok(root)
}

/// Who a message names, as wallet addresses that are actually in the room.
///
/// Two sources, unioned:
///
/// * what the client declared, which is the only source available for an
///   encrypted room and the only one that can carry a username with a space
///   in it;
/// * what the server can parse out of plaintext, which catches an `@name`
///   typed by hand into a client that does not build the list.
///
/// Both go through the same roster check, so neither is trusted: a declared
/// address that is not a member resolves to nothing, exactly like an `@name`
/// that matches nobody.
pub(super) fn resolve_mentions(
    conn: &rusqlite::Connection,
    room_id: &str,
    content: &str,
    is_encrypted: bool,
    declared: Vec<String>,
) -> ApiResult<Vec<String>> {
    let mut handles = declared;
    if !is_encrypted {
        handles.extend(mentions::extract(content));
    }
    mentions::resolve(conn, room_id, &handles)
}

/// Which hosted files a message shows, from the same two sources as its
/// mentions: what the client declared, plus what the server can read out of
/// plaintext for itself.
///
/// No roster check to make here — a media name is a claim about bytes, not
/// about a person — so the union is the answer. `validate::media_names` has
/// already refused anything that is not a servable filename, which is what
/// keeps a declaration from naming a path.
pub(super) fn resolve_media(
    content: &str,
    is_encrypted: bool,
    declared: Vec<String>,
) -> Vec<String> {
    let mut names = declared;
    if !is_encrypted {
        for name in crate::db::media::extract(content) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

#[derive(Debug, Deserialize)]
struct PaginationQuery {
    since: Option<String>,
    before: Option<String>,
    limit: Option<String>,
    /// `?includeReplies=true` puts thread replies back into the channel view.
    /// Off by default — that is what threads are for — and offered because a
    /// client with no thread UI at all still wants to show every message
    /// rather than silently hide half a conversation.
    #[serde(rename = "includeReplies")]
    include_replies: Option<String>,
}

/// `GET /api/rooms/{roomId}/messages` — initial load and backward paging.
///
/// `since`/`before` are **timestamps** here; `/sync`'s `since` is a serial.
/// Mixing them up is the most common client bug against this API.
///
/// Thread replies are **not** in this list. Each message that has any carries
/// `replyCount` and `lastReplyAt` instead, so the channel shows one line per
/// thread and says how much is under it.
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
    let include_replies = matches!(query.include_replies.as_deref(), Some("true" | "1" | "yes"));

    let room_id = room.as_str().to_owned();
    let address = caller.as_str().to_owned();
    let out = state
        .db
        .call(move |conn| {
            messages::get_messages(
                conn,
                &room_id,
                &address,
                since,
                before,
                limit,
                include_replies,
            )
        })
        .await?;
    Ok(Json(out).into_response())
}

/// `GET /api/messages/{messageId}/thread` — one thread, root first.
///
/// The id may name the root or any reply in it; both answer with the same
/// list, so a client holding only a reply (from `/sync`, say) can open the
/// thread without first working out where it starts.
///
/// Membership is checked against the room the thread lives in, not against
/// anything the caller supplied — a thread is not a way into a room.
async fn thread(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(message_id): Path<String>,
) -> ApiResult<Response> {
    let id = validate::message_id(&message_id)?;

    let (root, room_id) = {
        let id = id.as_str().to_owned();
        state
            .db
            // Deleted included: a tombstoned root still heads its thread.
            .call(move |conn| messages::thread_root(conn, &id, true))
            .await?
            .ok_or_else(|| ApiError::not_found("Message not found"))?
    };

    let room = RoomId::new(&room_id)
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("stored room id is not valid")))?;
    require_member(&state, &room, &caller).await?;

    let address = caller.as_str().to_owned();
    let out = state
        .db
        .call(move |conn| messages::get_thread(conn, &root, &address))
        .await?
        // `None` here means the root's sender is blocked by the viewer, which
        // is a thread they have chosen not to see rather than one that is
        // missing — but there is nothing to render either way.
        .ok_or_else(|| ApiError::not_found("Message not found"))?;
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

    let declared = validate::mention_addresses(body.mentions)?;
    let declared_media = validate::media_names(body.media)?;
    let updated = state
        .db
        .call({
            let id = id.as_str().to_owned();
            let room_id = existing.room_id.clone();
            move |conn| {
                let mentions =
                    resolve_mentions(conn, &room_id, &content, stays_encrypted, declared)?;
                let media = resolve_media(&content, stays_encrypted, declared_media);
                // Replaced rather than added to, inside the edit's own
                // transaction: an edit that takes somebody's name out has to
                // take the mention with it, or their inbox keeps pointing at
                // a message that no longer says it. The same holds for a
                // picture an edit removed — it must stop keeping bytes alive.
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
                    &mentions,
                    &media,
                )
            }
        })
        .await?
        .ok_or_else(|| ApiError::not_found("Message not found or unauthorized"))?;

    announce(&state, &room, Some(&caller), updated.msg_serial).await;
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
    announce(&state, &room, Some(&caller), serial).await;
    Ok(super::message("Message deleted successfully"))
}

/// `DELETE /api/rooms/{roomId}/messages` — purge the history. **Admins only.**
///
/// Room admins and server admins; not, as it was, any member. Deleting a
/// single message is open to everybody in the room on purpose — this is a
/// forgetting-first product and a member should not have to ask its author.
/// Erasing *everything* is a different act: it destroys other people's record
/// of the conversation with one request, and there is no undo. The roadmap
/// called this out as a gap (§3, "history protection"), and admin is the role
/// the room already has for irreversible things.
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
    super::rooms::require_admin(
        &state,
        &room,
        &caller,
        "Only room admins can delete a room's entire history",
    )
    .await?;
    // A built-in room's history is its owner's to clear and no one else's.
    // `require_admin` passed a server admin through without membership, which
    // is correct for an ordinary room and exactly wrong here — so the
    // ownership of a static room is confirmed separately.
    super::rooms::require_static_owner(&state, &room, &caller).await?;

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

    announce(&state, &room, Some(&caller), marker.msg_serial).await;

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
    announce(&state, &room, Some(&caller), updated.msg_serial).await;
    Ok(Json(updated).into_response())
}

/// Wake the room. `origin` is the actor, so members who blocked them get no
/// notification at all rather than one followed by an empty sync. `None` is a
/// webhook post — no wallet acted, so nobody's block list applies here (the
/// read paths still filter for anyone who blocked the webhook's own address).
pub(super) async fn announce(
    state: &AppState,
    room: &RoomId,
    origin: Option<&WalletAddress>,
    msg_serial: i64,
) {
    state
        .hub
        .publish_best_effort(
            Target::Room {
                room_id: room.clone(),
            },
            origin.cloned(),
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

    async fn post_reply(
        router: &Router,
        token: &str,
        room: &str,
        parent: &str,
        text: &str,
    ) -> serde_json::Value {
        request(
            router,
            "POST",
            &format!("/api/rooms/{room}/messages"),
            Some(token),
            Some(serde_json::json!({
                "content": text,
                "msgHash": hash('b'),
                "parentMessageId": parent,
            })),
        )
        .await
        .body
    }

    #[tokio::test]
    async fn a_thread_costs_the_channel_one_line() {
        let state = state("threads-fold");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);
        let room = make_room(&router, &token).await;

        let root = post_message(&router, &token, &room, "shipping today?").await;
        let root_id = root["id"].as_str().unwrap().to_owned();
        assert!(root["parentMessageId"].is_null());
        assert!(
            root.get("replyCount").is_none(),
            "a message with no thread carries no count to test"
        );

        for text in ["checking", "two tests left", "green"] {
            let reply = post_reply(&router, &token, &room, &root_id, text).await;
            assert_eq!(reply["parentMessageId"], root_id);
        }
        post_message(&router, &token, &room, "unrelated").await;

        // The channel shows the root and the unrelated message — not the
        // three replies. That is the whole point of a thread.
        let listed = request(
            &router,
            "GET",
            &format!("/api/rooms/{room}/messages"),
            Some(&token),
            None,
        )
        .await;
        let listed = listed.json();
        let listed = listed.as_array().unwrap();
        assert_eq!(listed.len(), 2, "{listed:?}");
        let root_row = listed.iter().find(|m| m["id"] == root_id).unwrap();
        assert_eq!(root_row["replyCount"], 3);
        assert!(root_row["lastReplyAt"].as_i64().unwrap() > 0);

        // A client with no thread UI can still see everything.
        let all = request(
            &router,
            "GET",
            &format!("/api/rooms/{room}/messages?includeReplies=true"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(all.json().as_array().unwrap().len(), 5);

        // The thread itself is the root followed by its replies, in order.
        let thread = request(
            &router,
            "GET",
            &format!("/api/messages/{root_id}/thread"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(thread.status, StatusCode::OK);
        let thread = thread.json();
        let thread = thread.as_array().unwrap();
        assert_eq!(thread.len(), 4);
        assert_eq!(thread[0]["id"], root_id);
        assert_eq!(thread[1]["content"], "checking");
        assert_eq!(thread[3]["content"], "green");
    }

    #[tokio::test]
    async fn replying_to_a_reply_joins_its_thread_rather_than_nesting() {
        let state = state("threads-flat");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);
        let room = make_room(&router, &token).await;

        let root_id = post_message(&router, &token, &room, "root").await["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let first = post_reply(&router, &token, &room, &root_id, "first").await;
        let first_id = first["id"].as_str().unwrap().to_owned();

        // Replying to the reply. A tree of unbounded depth is not a shape any
        // renderer can bound, so the second level flattens into the first.
        let second = post_reply(&router, &token, &room, &first_id, "second").await;
        assert_eq!(
            second["parentMessageId"], root_id,
            "a thread is one flat list, always rooted at the message that started it"
        );

        // Which means asking for the thread of *any* member of it — a reply
        // a client happened to receive from /sync, say — answers the same.
        let from_reply = request(
            &router,
            "GET",
            &format!("/api/messages/{first_id}/thread"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(from_reply.json().as_array().unwrap().len(), 3);
        assert_eq!(from_reply.json()[0]["id"], root_id);
    }

    #[tokio::test]
    async fn a_reply_cannot_cross_into_another_room() {
        let state = state("threads-cross");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);
        let public = make_room(&router, &token).await;
        let private = make_room(&router, &token).await;

        let elsewhere = post_message(&router, &token, &private, "secret").await["id"]
            .as_str()
            .unwrap()
            .to_owned();

        // Filing a message in one room into a thread in another would put it
        // in front of people who are not in the room it was posted to.
        let response = request(
            &router,
            "POST",
            &format!("/api/rooms/{public}/messages"),
            Some(&token),
            Some(serde_json::json!({
                "content": "leaking",
                "msgHash": hash('c'),
                "parentMessageId": elsewhere,
            })),
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);

        // And a parent that never existed is a 404, not a silent top-level post.
        let response = request(
            &router,
            "POST",
            &format!("/api/rooms/{public}/messages"),
            Some(&token),
            Some(serde_json::json!({
                "content": "orphan",
                "msgHash": hash('d'),
                "parentMessageId": "msg_1749652746620_ffffffff",
            })),
        )
        .await;
        assert_eq!(response.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_deleted_root_keeps_its_thread_together() {
        let state = state("threads-tombstone");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);
        let room = make_room(&router, &token).await;

        let root_id = post_message(&router, &token, &room, "root").await["id"]
            .as_str()
            .unwrap()
            .to_owned();
        post_reply(&router, &token, &room, &root_id, "still here").await;

        request(
            &router,
            "DELETE",
            &format!("/api/messages/{root_id}"),
            Some(&token),
            None,
        )
        .await;

        // Destroying the first message of a thread must not orphan what was
        // said under it; the tombstone stays at the head so the client can
        // render "message deleted" above the replies rather than a list with
        // no beginning.
        let thread = request(
            &router,
            "GET",
            &format!("/api/messages/{root_id}/thread"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(thread.status, StatusCode::OK);
        let thread = thread.json();
        let thread = thread.as_array().unwrap();
        assert_eq!(thread.len(), 2);
        assert_eq!(thread[0]["isDeleted"], true);
        assert_eq!(thread[0]["content"], "");
        assert_eq!(thread[1]["content"], "still here");
    }

    #[tokio::test]
    async fn a_thread_is_not_a_way_into_a_room() {
        let state = state("threads-access");
        let alice = wallet("alice");
        let mallory = wallet("mallory");
        let alice_token = register(&state, &alice, "alice");
        let mallory_token = register(&state, &mallory, "mallory");
        let router = build(state);
        let room = make_room(&router, &alice_token).await;

        let root_id = post_message(&router, &alice_token, &room, "internal").await["id"]
            .as_str()
            .unwrap()
            .to_owned();

        let response = request(
            &router,
            "GET",
            &format!("/api/messages/{root_id}/thread"),
            Some(&mallory_token),
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::FORBIDDEN);
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
