//! Room keys and epoch rotation (`docs/API.md` §6.9, §10).

use std::collections::HashSet;

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use pocketskynet_core::{ServerEvent, Target, WalletAddress};
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::db::keys::{self, KeyWrap, RotateOutcome};
use crate::db::rooms;
use crate::error::{ApiError, ApiResult};
use crate::validate::{self, ValidJson};
use crate::AppState;

/// A rotation must fit in one request, and 200 recipients is far past any
/// realistic room while still bounding the work a single call can demand.
const MAX_ROTATION_KEYS: usize = 200;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/rooms/{roomId}/keys", post(store).get(latest))
        .route("/rooms/{roomId}/keys/versions", get(versions))
        .route("/rooms/{roomId}/rotate-key", post(rotate))
}

/// The wrap fields, shared by `/keys` and each entry of `/rotate-key`.
#[derive(Debug, Deserialize)]
struct WrapBody {
    #[serde(rename = "userAddress")]
    user_address: Option<String>,
    #[serde(rename = "encryptedSymmetricKey")]
    encrypted_symmetric_key: Option<String>,
    #[serde(rename = "ephemeralPublicKey")]
    ephemeral_public_key: Option<String>,
    #[serde(rename = "encryptionIV")]
    encryption_iv: Option<String>,
    hmac: Option<String>,
    #[serde(rename = "encVer")]
    enc_ver: Option<i64>,
    #[serde(rename = "keyVersion")]
    key_version: Option<i64>,
}

/// Validate one wrap. `default_enc_ver` differs by endpoint: `/keys` defaults
/// to 1 for compatibility with clients that predate `encVer`, while
/// `/rotate-key` defaults to 2 because a client capable of rotating is
/// certainly capable of the modern scheme.
fn parse_wrap(body: &WrapBody, default_enc_ver: i64) -> ApiResult<(WalletAddress, KeyWrap, i64)> {
    let user = validate::wallet_address("userAddress", body.user_address.as_deref())?;
    let wrap = KeyWrap {
        user_address: user.as_str().to_owned(),
        encrypted_symmetric_key: validate::wrapped_key(body.encrypted_symmetric_key.as_deref())?,
        ephemeral_public_key: validate::ephemeral_public_key(body.ephemeral_public_key.as_deref())?,
        // Room-key hex accepts mixed case, unlike the message-level fields.
        encryption_iv: validate::room_key_hex("encryptionIV", body.encryption_iv.as_deref(), 32)?,
        hmac: validate::room_key_hex("hmac", body.hmac.as_deref(), 64)?,
        enc_ver: validate::enc_ver(body.enc_ver, default_enc_ver)?,
    };
    let key_version = validate::key_version("keyVersion", body.key_version, 1)?;
    Ok((user, wrap, key_version))
}

/// `POST /api/rooms/{roomId}/keys` — store one wrap for one epoch.
///
/// Built-in rooms key like any other room now: the server never indexed their
/// ciphertext anyway (`docs/SEARCH.md` §2 — encrypted content is never
/// indexed, built-in or not), so keeping them plaintext bought searchability
/// at the cost of the operator's box being able to read My Note. Search over
/// these rooms is a client-side concern now (`web/src/components/search.rs`),
/// which is what makes the trade unnecessary.
///
/// This endpoint never touches `currentKeyVersion`: establishing epoch 1 is a
/// plain store, and advancing an epoch is what `/rotate-key` is for. Mixing
/// the two would let a single member move the room forward without providing
/// wraps for anybody else.
async fn store(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
    ValidJson(body): ValidJson<WrapBody>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    let (target, wrap, key_version) = parse_wrap(&body, 1)?;

    let room_id = room.as_str().to_owned();
    let caller_address = caller.as_str().to_owned();
    let target_address = target.as_str().to_owned();
    let storing_for_self = target == caller;

    state
        .db
        .call(move |conn| {
            // The caller's own standing in the room is settled *before* the
            // room is looked up or classified, so a non-member probing the
            // derivable id of somebody's note cannot tell it from a room that
            // does not exist — both are the same 403. Learning the room is
            // "static" is enough to confirm the victim is an active account,
            // which is the enumeration oracle this ordering closes; the read
            // paths already follow it (`require_member` before existence).
            let caller_belongs = rooms::is_member(conn, &room_id, &caller_address)?
                || rooms::is_admin(conn, &room_id, &caller_address)?
                || rooms::has_pending_invitation(conn, &room_id, &caller_address)?;
            if !caller_belongs {
                return Err(ApiError::access_denied());
            }
            if rooms::get_room(conn, &room_id)?.is_none() {
                return Err(ApiError::not_found("Room not found"));
            }
            // An invitee is a legitimate recipient: pre-wrapping at invite
            // time is the point, because the admin is online then and the
            // invitee may not be.
            let eligible = rooms::is_member(conn, &room_id, &target_address)?
                || rooms::has_pending_invitation(conn, &room_id, &target_address)?;
            if !eligible {
                return Err(ApiError::bad_request("User must be a room member or invitee"));
            }
            if !storing_for_self && !rooms::is_admin(conn, &room_id, &caller_address)? {
                return Err(ApiError::forbidden(
                    "Only admins can store keys for other users",
                ));
            }
            // An admin must not be able to clobber a member's working wrap and
            // lock them out; re-keying goes through /rotate-key.
            if !storing_for_self && keys::key_exists(conn, &room_id, &target_address, key_version)? {
                return Err(ApiError::conflict(
                    "That member already has a key for this epoch; use /rotate-key to re-key the room.",
                ));
            }
            keys::store_key(conn, &room_id, &wrap, key_version)
        })
        .await?;

    Ok(super::message("Room key stored successfully"))
}

/// `GET /api/rooms/{roomId}/keys` — the caller's newest wrap.
async fn latest(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    require_member(&state, &room, &caller).await?;

    let room_id = room.as_str().to_owned();
    let address = caller.as_str().to_owned();
    let key = state
        .db
        .call(move |conn| keys::latest_key(conn, &room_id, &address))
        .await?
        .ok_or_else(|| ApiError::not_found("Room key not found"))?;
    Ok(Json(key).into_response())
}

/// `GET /api/rooms/{roomId}/keys/versions` — every epoch the caller can read.
///
/// Decrypting history needs all of them, so this is the endpoint clients
/// should use; `/keys` returns only the newest and is enough to *send*.
/// An empty list is a 200 with `[]`, not a 404: "this room is not encrypted
/// for me" is a normal state.
async fn versions(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    require_member(&state, &room, &caller).await?;

    let room_id = room.as_str().to_owned();
    let address = caller.as_str().to_owned();
    let all = state
        .db
        .call(move |conn| keys::all_keys(conn, &room_id, &address))
        .await?;
    Ok(Json(all).into_response())
}

#[derive(Debug, Deserialize)]
struct RotateBody {
    #[serde(rename = "newVersion")]
    new_version: Option<i64>,
    keys: Option<Vec<WrapBody>>,
}

/// `POST /api/rooms/{roomId}/rotate-key` — any **member**, deliberately not
/// admin-only.
///
/// Signal-style: whoever needs to send drives the re-key. Restricting it to
/// admins would freeze a room after a departure until an admin happened to
/// appear, and a member already holds the current key, so rotating reveals
/// nothing to them they could not already read. The two coverage checks below
/// are what make that safe.
async fn rotate(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
    ValidJson(body): ValidJson<RotateBody>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;

    let member = {
        let room_id = room.as_str().to_owned();
        let address = caller.as_str().to_owned();
        state
            .db
            .call(move |conn| rooms::is_member(conn, &room_id, &address))
            .await?
    };
    if !member {
        return Err(ApiError::forbidden(
            "Only room members can rotate the room key",
        ));
    }

    let new_version = validate::key_version("newVersion", body.new_version, 0)?;
    if new_version < 2 {
        return Err(ApiError::field(
            "newVersion",
            "Key version must be between 2 and 1000000",
        ));
    }

    let entries = body
        .keys
        .ok_or_else(|| validate::required("keys", "Keys"))?;
    if entries.is_empty() || entries.len() > MAX_ROTATION_KEYS {
        return Err(ApiError::field(
            "keys",
            "Keys must contain between 1 and 200 entries",
        ));
    }

    let mut wraps = Vec::with_capacity(entries.len());
    let mut covered: HashSet<String> = HashSet::new();
    for entry in &entries {
        // The per-entry keyVersion is ignored: the server forces every wrap in
        // a rotation to the new epoch, so a client cannot smuggle a wrap for a
        // different one into the same transaction.
        let (user, wrap, _) = parse_wrap(entry, 2)?;
        covered.insert(user.as_str().to_owned());
        wraps.push(wrap);
    }

    let room_id = room.as_str().to_owned();
    let roster: HashSet<String> = state
        .db
        .call({
            let room_id = room_id.clone();
            move |conn| {
                Ok(rooms::list_members(conn, &room_id)?
                    .into_iter()
                    .map(|m| m.user_address)
                    .collect())
            }
        })
        .await?;

    // Full coverage: a member left out would silently lose the room at the
    // next message, with no signal that a rotation had happened.
    let mut missing: Vec<String> = roster.difference(&covered).cloned().collect();
    if !missing.is_empty() {
        missing.sort();
        // A machine-readable array, not prose. A client that loses a rotation
        // race is expected to refetch and retry automatically, and recovering
        // the uncovered addresses by parsing them out of an English sentence
        // is exactly what this field exists to avoid.
        return Ok((
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "message": "Rotation must include a key for every current member",
                "missing": missing,
            })),
        )
            .into_response());
    }
    // No strays: wrapping to a non-member would hand the new epoch to somebody
    // the room never admitted.
    if covered.difference(&roster).next().is_some() {
        return Err(ApiError::bad_request(
            "Rotation includes a non-member address",
        ));
    }

    let outcome = state
        .db
        .call(move |conn| keys::rotate(conn, &room_id, new_version, &wraps))
        .await?;

    match outcome {
        RotateOutcome::Rotated => {}
        RotateOutcome::RoomNotFound => return Err(ApiError::conflict("Room not found")),
        RotateOutcome::StaleVersion { .. } => {
            // 409, not 400: two members raced. The loser refetches the room
            // and retries only if keyRotationPending is still set.
            return Err(ApiError::conflict("Stale key version — refetch and retry"));
        }
    }

    let _ = state.log.append_audit(
        "room_key_rotated",
        Some(&caller),
        serde_json::json!({ "roomId": room.as_str(), "newVersion": new_version }),
    );

    // `new_message` doubles as "something changed here"; clients must refetch
    // key versions on it, since the epoch may have advanced.
    state
        .hub
        .publish_best_effort(
            Target::Room {
                room_id: room.clone(),
            },
            None,
            ServerEvent::NewMessage {
                room_id: room.clone(),
                msg_serial: 0,
            },
        )
        .await;

    Ok(Json(serde_json::json!({
        "message": "Room key rotated",
        "newVersion": new_version,
    }))
    .into_response())
}

async fn require_member(
    state: &AppState,
    room: &pocketskynet_core::RoomId,
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

#[cfg(test)]
mod tests {
    use crate::routes::build;
    use crate::test_support::{register, send, state, wallet};
    use axum::http::StatusCode;
    use axum::Router;

    fn wrap_json(user: &str, tag: &str, version: Option<i64>) -> serde_json::Value {
        let mut value = serde_json::json!({
            "userAddress": user,
            "encryptedSymmetricKey": format!("wrapped-{tag}"),
            "ephemeralPublicKey": "04AbCd",
            "encryptionIV": "1a2b3c4d5e6f78901234567890AbCdEf",
            "hmac": "9".repeat(64),
            "encVer": 2,
        });
        if let Some(version) = version {
            value["keyVersion"] = serde_json::json!(version);
        }
        value
    }

    async fn make_room(router: &Router, token: &str) -> String {
        send(
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

    #[tokio::test]
    async fn a_member_can_store_and_read_back_their_own_wrap() {
        let state = state("keys-self");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);
        let room = make_room(&router, &token).await;

        let stored = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/keys"),
            Some(&token),
            Some(wrap_json(alice.as_str(), "v1", Some(1))),
        )
        .await;
        assert_eq!(stored.status, StatusCode::OK);

        let latest = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/keys"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(latest.json()["keyVersion"], 1);
        assert_eq!(latest.json()["encVer"], 2);
        assert_eq!(latest.json()["encryptedSymmetricKey"], "wrapped-v1");

        let detail = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(detail.json()["hasEncryption"], true);
    }

    #[tokio::test]
    async fn versions_is_an_empty_array_not_a_404() {
        let state = state("keys-empty");
        let token = register(&state, &wallet("alice"), "alice");
        let router = build(state);
        let room = make_room(&router, &token).await;

        let versions = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/keys/versions"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(versions.status, StatusCode::OK);
        assert!(versions.json().as_array().unwrap().is_empty());

        let latest = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/keys"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(latest.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_admin_cannot_clobber_a_members_existing_wrap() {
        let state = state("keys-clobber");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state);
        let room = make_room(&router, &alice_token).await;

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

        // Bob stores his own wrap for epoch 1.
        send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/keys"),
            Some(&bob_token),
            Some(wrap_json(bob.as_str(), "bobs", Some(1))),
        )
        .await;

        let clobber = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/keys"),
            Some(&alice_token),
            Some(wrap_json(bob.as_str(), "alices", Some(1))),
        )
        .await;
        assert_eq!(clobber.status, StatusCode::CONFLICT);

        // But Bob may always overwrite his own.
        let own = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/keys"),
            Some(&bob_token),
            Some(wrap_json(bob.as_str(), "bobs2", Some(1))),
        )
        .await;
        assert_eq!(own.status, StatusCode::OK);
    }

    #[tokio::test]
    async fn storing_for_someone_else_requires_admin() {
        let state = state("keys-authz");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state);
        let room = make_room(&router, &alice_token).await;

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

        let by_member = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/keys"),
            Some(&bob_token),
            Some(wrap_json(alice.as_str(), "sneaky", Some(1))),
        )
        .await;
        assert_eq!(by_member.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn an_outsider_is_not_an_eligible_recipient() {
        let state = state("keys-outsider");
        let alice = wallet("alice");
        let mallory = wallet("mallory");
        let token = register(&state, &alice, "alice");
        register(&state, &mallory, "mallory");
        let router = build(state);
        let room = make_room(&router, &token).await;

        let response = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/keys"),
            Some(&token),
            Some(wrap_json(mallory.as_str(), "leak", Some(1))),
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            response.json()["message"],
            "User must be a room member or invitee"
        );
    }

    #[tokio::test]
    async fn rotation_requires_covering_exactly_the_current_roster() {
        let state = state("rotate-coverage");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let mallory = wallet("mallory");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        register(&state, &mallory, "mallory");
        let router = build(state);
        let room = make_room(&router, &alice_token).await;

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

        let incomplete = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/rotate-key"),
            Some(&alice_token),
            Some(serde_json::json!({
                "newVersion": 2,
                "keys": [wrap_json(alice.as_str(), "v2", None)],
            })),
        )
        .await;
        assert_eq!(incomplete.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            incomplete.json()["message"],
            "Rotation must include a key for every current member"
        );
        // Machine-readable, so a client can retry without parsing prose.
        assert_eq!(
            incomplete.json()["missing"].as_array().unwrap(),
            &vec![serde_json::json!(bob.as_str())]
        );

        let stray = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/rotate-key"),
            Some(&alice_token),
            Some(serde_json::json!({
                "newVersion": 2,
                "keys": [
                    wrap_json(alice.as_str(), "v2", None),
                    wrap_json(bob.as_str(), "v2", None),
                    wrap_json(mallory.as_str(), "v2", None),
                ],
            })),
        )
        .await;
        assert_eq!(stray.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            stray.json()["message"],
            "Rotation includes a non-member address"
        );
    }

    #[tokio::test]
    async fn any_member_may_rotate_and_the_loser_of_a_race_gets_409() {
        let state = state("rotate-race");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state);
        let room = make_room(&router, &alice_token).await;

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

        let body = serde_json::json!({
            "newVersion": 2,
            "keys": [
                wrap_json(alice.as_str(), "v2", None),
                wrap_json(bob.as_str(), "v2", None),
            ],
        });

        // A plain member, not an admin, drives the re-key.
        let first = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/rotate-key"),
            Some(&bob_token),
            Some(body.clone()),
        )
        .await;
        assert_eq!(first.status, StatusCode::OK);
        assert_eq!(first.json()["newVersion"], 2);

        let second = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/rotate-key"),
            Some(&alice_token),
            Some(body),
        )
        .await;
        assert_eq!(second.status, StatusCode::CONFLICT);
        assert!(second.json()["message"]
            .as_str()
            .unwrap()
            .contains("Stale key version"));

        let versions = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/keys/versions"),
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(versions.json().as_array().unwrap().len(), 1);
        assert_eq!(versions.json()[0]["keyVersion"], 2);
    }

    #[tokio::test]
    async fn rotating_to_version_one_is_refused() {
        let state = state("rotate-v1");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);
        let room = make_room(&router, &token).await;

        let response = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/rotate-key"),
            Some(&token),
            Some(serde_json::json!({
                "newVersion": 1,
                "keys": [wrap_json(alice.as_str(), "v1", None)],
            })),
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_non_member_cannot_rotate() {
        let state = state("rotate-outsider");
        let alice = wallet("alice");
        let mallory = wallet("mallory");
        let alice_token = register(&state, &alice, "alice");
        let mallory_token = register(&state, &mallory, "mallory");
        let router = build(state);
        let room = make_room(&router, &alice_token).await;

        let response = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/rotate-key"),
            Some(&mallory_token),
            Some(serde_json::json!({
                "newVersion": 2,
                "keys": [wrap_json(mallory.as_str(), "v2", None)],
            })),
        )
        .await;
        assert_eq!(response.status, StatusCode::FORBIDDEN);
        assert_eq!(
            response.json()["message"],
            "Only room members can rotate the room key"
        );
    }
}
