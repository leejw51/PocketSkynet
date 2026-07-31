//! Rooms, membership, hidden rooms, and admins (`docs/API.md` §6.5, §6.6, §6.8).

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use pocketskynet_core::{RoomId, ServerEvent, Target, WalletAddress};
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::db::rooms::MAX_ADMINS;
use crate::db::{keys, rooms, storage, users};
use crate::error::{ApiError, ApiResult};
use crate::validate::{self, ValidJson};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/rooms", post(create).get(list))
        .route("/rooms/hidden", get(list_hidden))
        .route("/rooms/{roomId}", get(detail).patch(rename).delete(remove))
        .route("/rooms/{roomId}/hide", post(hide).delete(unhide))
        .route("/rooms/{roomId}/leave", post(leave))
        .route("/rooms/{roomId}/kick", post(kick))
        .route("/rooms/{roomId}/members", get(members))
        .route("/rooms/{roomId}/admins", post(add_admin).get(list_admins))
        .route(
            "/rooms/{roomId}/admins/{walletAddress}",
            delete(remove_admin),
        )
}

/// Membership is checked *before* room existence throughout this module, so a
/// caller who is not a member cannot distinguish "not yours" from "does not
/// exist". Rooms are not discoverable — the only ways in are creating one and
/// accepting an invitation — and a 404/403 split would turn any endpoint into
/// a room-id oracle.
async fn require_member(state: &AppState, room: &RoomId, caller: &WalletAddress) -> ApiResult<()> {
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

async fn require_admin(
    state: &AppState,
    room: &RoomId,
    caller: &WalletAddress,
    message: &str,
) -> ApiResult<()> {
    let room_id = room.as_str().to_owned();
    let address = caller.as_str().to_owned();
    let admin = state
        .db
        .call(move |conn| rooms::is_admin(conn, &room_id, &address))
        .await?;
    if admin {
        Ok(())
    } else {
        Err(ApiError::forbidden(message))
    }
}

async fn require_room_exists(state: &AppState, room: &RoomId) -> ApiResult<()> {
    let room_id = room.as_str().to_owned();
    let exists = state
        .db
        .call(move |conn| Ok(rooms::get_room(conn, &room_id)?.is_some()))
        .await?;
    if exists {
        Ok(())
    } else {
        Err(ApiError::not_found("Room not found"))
    }
}

#[derive(Debug, Deserialize)]
struct CreateBody {
    name: Option<String>,
    description: Option<String>,
}

/// `POST /api/rooms` — create a room and seat the caller.
///
/// No realtime event is emitted: the creator is the only member and already
/// knows. Other clients learn about rooms through invitations.
async fn create(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    ValidJson(body): ValidJson<CreateBody>,
) -> ApiResult<Response> {
    let name = validate::room_name(body.name.as_deref())?;
    let description = validate::room_description(body.description.as_deref())?;

    let id = format!("room_{}_{}", crate::db::now_ms(), uuid::Uuid::new_v4());
    let creator = caller.as_str().to_owned();
    let room = state
        .db
        .call({
            let id = id.clone();
            move |conn| rooms::create_room(conn, &id, &name, description.as_deref(), &creator)
        })
        .await?;

    // The creator's live connections need the new room in their subscription
    // set, or they would miss their own room's events until reconnecting.
    if let Err(e) = state.hub.refresh_user_rooms(&caller).await {
        tracing::warn!(error = %e, "could not refresh room subscriptions after create");
    }

    Ok(Json(room).into_response())
}

/// `GET /api/rooms` — the caller's rooms with unread state.
async fn list(State(state): State<AppState>, AuthUser(caller): AuthUser) -> ApiResult<Response> {
    let address = caller.as_str().to_owned();
    let out = state
        .db
        .call(move |conn| storage::visible_rooms(conn, &address))
        .await?;
    Ok(Json(out).into_response())
}

/// `GET /api/rooms/hidden`.
async fn list_hidden(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
) -> ApiResult<Response> {
    let address = caller.as_str().to_owned();
    let out = state
        .db
        .call(move |conn| storage::hidden_rooms(conn, &address))
        .await?;
    Ok(Json(out).into_response())
}

/// `GET /api/rooms/{roomId}` — detail, without read state.
async fn detail(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    require_member(&state, &room, &caller).await?;

    let room_id = room.as_str().to_owned();
    let address = caller.as_str().to_owned();
    let detail = state
        .db
        .call(move |conn| storage::room_detail(conn, &room_id, &address, false))
        .await?
        .ok_or_else(|| ApiError::not_found("Room not found"))?;
    Ok(Json(detail).into_response())
}

#[derive(Debug, Deserialize)]
struct RenameBody {
    name: Option<String>,
}

/// `PATCH /api/rooms/{roomId}` — rename, admins only.
///
/// The description is immutable: there is no endpoint for it in the protocol,
/// and inventing one here would be a wire change no client asked for.
async fn rename(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
    ValidJson(body): ValidJson<RenameBody>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    require_room_exists(&state, &room).await?;
    require_admin(
        &state,
        &room,
        &caller,
        "Only room admins can update the room",
    )
    .await?;

    let name = validate::room_name(body.name.as_deref())?;
    let room_id = room.as_str().to_owned();
    let updated = state
        .db
        .call(move |conn| rooms::update_room_name(conn, &room_id, &name))
        .await?
        .ok_or_else(|| ApiError::not_found("Room not found"))?;
    Ok(Json(updated).into_response())
}

/// `DELETE /api/rooms/{roomId}` — admins only, one transaction.
async fn remove(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    require_room_exists(&state, &room).await?;
    require_admin(
        &state,
        &room,
        &caller,
        "Only room admins can delete the room",
    )
    .await?;

    // Collect the roster before the delete so the remaining members can be
    // told; afterwards there is nothing left to enumerate.
    let room_id = room.as_str().to_owned();
    let member_addresses = state
        .db
        .call({
            let room_id = room_id.clone();
            move |conn| {
                let members = rooms::list_members(conn, &room_id)?;
                Ok(members
                    .into_iter()
                    .map(|m| m.user_address)
                    .collect::<Vec<_>>())
            }
        })
        .await?;

    state
        .db
        .call(move |conn| rooms::delete_room(conn, &room_id))
        .await?;

    let _ = state.log.append_audit(
        "room_deleted",
        Some(&caller),
        serde_json::json!({ "roomId": room.as_str() }),
    );

    // Every former member's subscription set has to shed the room, and their
    // clients need to refetch: /sync on a deleted room now answers 403.
    for address in member_addresses {
        if let Ok(wallet) = WalletAddress::new(&address) {
            let _ = state.hub.refresh_user_rooms(&wallet).await;
            state
                .hub
                .publish_best_effort(
                    Target::User {
                        wallet: wallet.clone(),
                    },
                    None,
                    ServerEvent::RoomsUpdated,
                )
                .await;
        }
    }

    Ok(super::message("Room deleted successfully"))
}

/// `POST /api/rooms/{roomId}/leave`.
///
/// **§15 #1.** The reference had no membership check, so any authenticated
/// caller who learned a room id could "leave" it and set
/// `keyRotationPending`, which blocks *all* encrypted messaging in that room
/// until a member re-keys. That is a remote denial of service on any room
/// whose id leaks; membership is required here.
async fn leave(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    require_room_exists(&state, &room).await?;
    require_member(&state, &room, &caller).await?;

    let room_id = room.as_str().to_owned();
    let address = caller.as_str().to_owned();
    state
        .db
        .call(move |conn| {
            if rooms::is_admin(conn, &room_id, &address)? {
                if rooms::admin_count(conn, &room_id)? <= 1 {
                    return Err(ApiError::bad_request(
                        "Cannot leave room as the last admin. Transfer admin rights first or delete the room.",
                    ));
                }
                rooms::remove_admin(conn, &room_id, &address)?;
            }
            // Order matters: the wraps go before the membership row, so a
            // failure between the two leaves a member with no key rather than
            // a non-member holding one.
            keys::delete_user_keys(conn, &room_id, &address)?;
            rooms::remove_member(conn, &room_id, &address)?;
            // The leaver may still hold the current key, so nothing new may be
            // sealed under it until someone rotates.
            rooms::set_key_rotation_pending(conn, &room_id, true)?;
            Ok(())
        })
        .await?;

    after_membership_change(&state, &room, &caller).await;
    Ok(super::message("Left room successfully"))
}

#[derive(Debug, Deserialize)]
struct KickBody {
    #[serde(rename = "userAddress")]
    user_address: Option<String>,
}

/// `POST /api/rooms/{roomId}/kick` — admins only.
///
/// Admins may remove other admins, but not themselves, which is what keeps at
/// least one admin in every room without a separate last-admin guard.
async fn kick(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
    ValidJson(body): ValidJson<KickBody>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    require_admin(
        &state,
        &room,
        &caller,
        "Only room admins can remove members",
    )
    .await?;

    let target = validate::wallet_address("userAddress", body.user_address.as_deref())?;
    if target == caller {
        return Err(ApiError::bad_request(
            "Cannot kick yourself. Use leave instead.",
        ));
    }

    let room_id = room.as_str().to_owned();
    let address = target.as_str().to_owned();
    state
        .db
        .call(move |conn| {
            if !rooms::is_member(conn, &room_id, &address)? {
                return Err(ApiError::not_found("User is not a member of this room"));
            }
            rooms::remove_admin(conn, &room_id, &address)?;
            keys::delete_user_keys(conn, &room_id, &address)?;
            rooms::remove_member(conn, &room_id, &address)?;
            rooms::set_key_rotation_pending(conn, &room_id, true)?;
            Ok(())
        })
        .await?;

    let _ = state.log.append_audit(
        "member_kicked",
        Some(&caller),
        serde_json::json!({ "roomId": room.as_str(), "target": target.as_str() }),
    );

    after_membership_change(&state, &room, &target).await;

    Ok(Json(serde_json::json!({
        "message": "User removed from room",
        "keyRotationPending": true,
    }))
    .into_response())
}

/// Refresh the departing member's subscriptions and tell everyone the roster
/// moved. `member_removed` means "the roster changed" — it is also emitted on
/// join, which is why clients must treat it as a refresh signal rather than
/// literally "somebody left".
pub async fn after_membership_change(state: &AppState, room: &RoomId, moved: &WalletAddress) {
    if let Err(e) = state.hub.refresh_user_rooms(moved).await {
        tracing::warn!(error = %e, "could not refresh subscriptions after a membership change");
    }
    state
        .hub
        .publish_best_effort(
            Target::User {
                wallet: moved.clone(),
            },
            None,
            ServerEvent::RoomsUpdated,
        )
        .await;
    state
        .hub
        .publish_best_effort(
            Target::Room {
                room_id: room.clone(),
            },
            None,
            ServerEvent::MemberRemoved {
                room_id: room.clone(),
            },
        )
        .await;
}

/// `GET /api/rooms/{roomId}/members` — members only.
///
/// Blocked users are **not** filtered out: they are still in the room, and
/// hiding them would leave the client unable to render their messages' senders.
async fn members(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    require_member(&state, &room, &caller).await?;

    let room_id = room.as_str().to_owned();
    let roster = state
        .db
        .call(move |conn| rooms::list_members(conn, &room_id))
        .await?;
    Ok(Json(roster).into_response())
}

#[derive(Debug, Deserialize)]
struct AddAdminBody {
    #[serde(rename = "walletAddress")]
    wallet_address: Option<String>,
}

/// `POST /api/rooms/{roomId}/admins`.
async fn add_admin(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
    ValidJson(body): ValidJson<AddAdminBody>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    let raw = body
        .wallet_address
        .as_deref()
        .ok_or_else(|| ApiError::bad_request("Wallet address is required"))?;
    let target = WalletAddress::new(raw)
        .map_err(|_| ApiError::bad_request("Invalid wallet address format"))?;

    require_room_exists(&state, &room).await?;
    require_admin(
        &state,
        &room,
        &caller,
        "Only room admins can add new admins",
    )
    .await?;

    let room_id = room.as_str().to_owned();
    let address = target.as_str().to_owned();
    state
        .db
        .call(move |conn| {
            if rooms::admin_count(conn, &room_id)? >= MAX_ADMINS {
                return Err(ApiError::bad_request("Maximum admin count (9) reached"));
            }
            if !users::user_exists(conn, &address)? {
                return Err(ApiError::not_found("User not found"));
            }
            if !rooms::is_member(conn, &room_id, &address)? {
                return Err(ApiError::bad_request(
                    "User must be a member of the room to become an admin",
                ));
            }
            if rooms::is_admin(conn, &room_id, &address)? {
                return Err(ApiError::bad_request("User is already an admin"));
            }
            rooms::add_admin(conn, &room_id, &address)
        })
        .await?;

    Ok(super::message("Admin added successfully"))
}

/// `DELETE /api/rooms/{roomId}/admins/{walletAddress}`.
///
/// An admin may demote themselves as long as another remains. Losing admin
/// status does not remove membership.
async fn remove_admin(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path((room_id, wallet_address)): Path<(String, String)>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    let target = validate::wallet_address("walletAddress", Some(&wallet_address))?;

    require_room_exists(&state, &room).await?;
    require_admin(&state, &room, &caller, "Only room admins can remove admins").await?;

    let room_id = room.as_str().to_owned();
    let address = target.as_str().to_owned();
    state
        .db
        .call(move |conn| {
            if rooms::admin_count(conn, &room_id)? <= 1 {
                return Err(ApiError::bad_request(
                    "Cannot remove the last admin. Room must have at least one admin.",
                ));
            }
            if !rooms::is_admin(conn, &room_id, &address)? {
                return Err(ApiError::bad_request("User is not an admin"));
            }
            rooms::remove_admin(conn, &room_id, &address)
        })
        .await?;

    Ok(super::message("Admin removed successfully"))
}

/// `GET /api/rooms/{roomId}/admins` — members only.
async fn list_admins(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    require_member(&state, &room, &caller).await?;

    let room_id = room.as_str().to_owned();
    let admins = state
        .db
        .call(move |conn| rooms::list_admins(conn, &room_id))
        .await?;
    Ok(Json(admins).into_response())
}

/// `POST /api/rooms/{roomId}/hide` — members only.
async fn hide(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    let room_id = room.as_str().to_owned();
    let address = caller.as_str().to_owned();

    let row = state
        .db
        .call(move |conn| {
            if !rooms::is_member(conn, &room_id, &address)? {
                return Err(ApiError::forbidden(
                    "You must be a member of the room to hide it",
                ));
            }
            rooms::hide_room(conn, &address, &room_id)
        })
        .await?;
    Ok(Json(row).into_response())
}

/// `DELETE /api/rooms/{roomId}/hide` — no membership or existence check.
///
/// Unhiding is purely a client-side list preference; refusing it for a room
/// the caller has since left would strand a dead entry in their hidden list.
async fn unhide(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
) -> ApiResult<Response> {
    // §15 #6: an invalid id here was a 500 in the reference.
    let room = validate::room_id(&room_id)?;
    let room_id = room.as_str().to_owned();
    let address = caller.as_str().to_owned();

    state
        .db
        .call(move |conn| rooms::unhide_room(conn, &address, &room_id))
        .await?;
    Ok(super::message("Room unhidden successfully"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::build;
    use crate::test_support::{register, send, state, wallet, Response as TestResponse};
    use axum::http::StatusCode;
    use axum::Router;

    async fn make_room(router: &Router, token: &str, name: &str) -> String {
        let response = send(
            router,
            "POST",
            "/api/rooms",
            Some(token),
            Some(serde_json::json!({ "name": name })),
        )
        .await;
        response.json()["id"].as_str().unwrap().to_owned()
    }

    async fn join(state: &AppState, room: &str, who: &WalletAddress) {
        let room = room.to_owned();
        let address = who.as_str().to_owned();
        state
            .db
            .call_blocking(move |conn| rooms::add_member(conn, &room, &address))
            .unwrap();
    }

    #[tokio::test]
    async fn creating_a_room_seats_the_creator() {
        let state = state("room-create");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        let created = send(
            &router,
            "POST",
            "/api/rooms",
            Some(&token),
            Some(serde_json::json!({ "name": "Team chat", "description": "  " })),
        )
        .await;

        assert_eq!(created.status, StatusCode::OK);
        assert_eq!(created.json()["name"], "Team chat");
        assert!(
            created.json()["description"].is_null(),
            "a blank description is stored as null"
        );
        assert_eq!(created.json()["currentKeyVersion"], 1);
        assert_eq!(created.json()["keyRotationPending"], false);
        // A bare Room, not the enriched shape.
        assert!(created.json().get("members").is_none());

        let listed = send(&router, "GET", "/api/rooms", Some(&token), None).await;
        assert_eq!(listed.json().as_array().unwrap().len(), 1);
        assert_eq!(listed.json()[0]["memberCount"], 1);
        assert_eq!(listed.json()[0]["unreadCount"], 0);
        assert_eq!(listed.json()[0]["lastReadSerial"], 0);
    }

    #[tokio::test]
    async fn a_non_member_cannot_tell_a_missing_room_from_a_private_one() {
        let state = state("room-oracle");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state);
        let room = make_room(&router, &alice_token, "Private").await;

        let real: TestResponse = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}"),
            Some(&bob_token),
            None,
        )
        .await;
        let fake = send(
            &router,
            "GET",
            "/api/rooms/room_0000000000_does_not_exist",
            Some(&bob_token),
            None,
        )
        .await;

        assert_eq!(real.status, StatusCode::FORBIDDEN);
        assert_eq!(fake.status, StatusCode::FORBIDDEN);
        assert_eq!(real.json()["message"], fake.json()["message"]);
    }

    #[tokio::test]
    async fn renaming_and_deleting_are_admin_only() {
        let state = state("room-admin-only");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state.clone());
        let room = make_room(&router, &alice_token, "Team").await;
        join(&state, &room, &bob).await;

        let refused = send(
            &router,
            "PATCH",
            &format!("/api/rooms/{room}"),
            Some(&bob_token),
            Some(serde_json::json!({ "name": "Hijacked" })),
        )
        .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        assert_eq!(
            refused.json()["message"],
            "Only room admins can update the room"
        );

        let allowed = send(
            &router,
            "PATCH",
            &format!("/api/rooms/{room}"),
            Some(&alice_token),
            Some(serde_json::json!({ "name": "Renamed" })),
        )
        .await;
        assert_eq!(allowed.json()["name"], "Renamed");

        let refused_delete = send(
            &router,
            "DELETE",
            &format!("/api/rooms/{room}"),
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(refused_delete.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn leaving_requires_membership() {
        let state = state("leave-guard");
        let alice = wallet("alice");
        let mallory = wallet("mallory");
        let alice_token = register(&state, &alice, "alice");
        let mallory_token = register(&state, &mallory, "mallory");
        let router = build(state.clone());
        let room = make_room(&router, &alice_token, "Team").await;

        // §15 #1: this used to return 200 and set keyRotationPending, which
        // froze encrypted messaging for everyone in the room.
        let response = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/leave"),
            Some(&mallory_token),
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::FORBIDDEN);

        let pending = state
            .db
            .call_blocking({
                let room = room.clone();
                move |conn| Ok(rooms::get_room(conn, &room)?.unwrap().key_rotation_pending)
            })
            .unwrap();
        assert!(!pending, "an outsider must not be able to freeze the room");
    }

    #[tokio::test]
    async fn the_last_admin_cannot_leave() {
        let state = state("last-admin");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let token = register(&state, &alice, "alice");
        register(&state, &bob, "bob");
        let router = build(state.clone());
        let room = make_room(&router, &token, "Team").await;
        join(&state, &room, &bob).await;

        let response = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/leave"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert!(response.json()["message"]
            .as_str()
            .unwrap()
            .contains("last admin"));
    }

    #[tokio::test]
    async fn leaving_drops_keys_and_flags_a_rotation() {
        let state = state("leave-effects");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state.clone());
        let room = make_room(&router, &alice_token, "Team").await;
        join(&state, &room, &bob).await;

        state
            .db
            .call_blocking({
                let room = room.clone();
                let bob = bob.as_str().to_owned();
                move |conn| {
                    keys::store_key(
                        conn,
                        &room,
                        &keys::KeyWrap {
                            user_address: bob,
                            encrypted_symmetric_key: "wrapped".into(),
                            ephemeral_public_key: "04ab".into(),
                            encryption_iv: "1a2b3c4d5e6f78901234567890abcdef".into(),
                            hmac: "9".repeat(64),
                            enc_ver: 2,
                        },
                        1,
                    )
                }
            })
            .unwrap();

        let response = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/leave"),
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);

        let (still_member, wraps, pending) = state
            .db
            .call_blocking({
                let room = room.clone();
                let bob = bob.as_str().to_owned();
                move |conn| {
                    Ok((
                        rooms::is_member(conn, &room, &bob)?,
                        keys::all_keys(conn, &room, &bob)?.len(),
                        rooms::get_room(conn, &room)?.unwrap().key_rotation_pending,
                    ))
                }
            })
            .unwrap();

        assert!(!still_member);
        assert_eq!(wraps, 0, "a departed member keeps no wraps on the server");
        assert!(pending, "the room must be re-keyed before new ciphertext");
    }

    #[tokio::test]
    async fn kicking_is_admin_only_and_never_self() {
        let state = state("kick");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state.clone());
        let room = make_room(&router, &alice_token, "Team").await;
        join(&state, &room, &bob).await;

        let by_member = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/kick"),
            Some(&bob_token),
            Some(serde_json::json!({ "userAddress": alice.as_str() })),
        )
        .await;
        assert_eq!(by_member.status, StatusCode::FORBIDDEN);

        let myself = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/kick"),
            Some(&alice_token),
            Some(serde_json::json!({ "userAddress": alice.as_str() })),
        )
        .await;
        assert_eq!(myself.status, StatusCode::BAD_REQUEST);
        assert!(myself.json()["message"]
            .as_str()
            .unwrap()
            .contains("Use leave instead"));

        let ok = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/kick"),
            Some(&alice_token),
            Some(serde_json::json!({ "userAddress": bob.as_str() })),
        )
        .await;
        assert_eq!(ok.status, StatusCode::OK);
        assert_eq!(ok.json()["keyRotationPending"], true);

        let missing = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/kick"),
            Some(&alice_token),
            Some(serde_json::json!({ "userAddress": bob.as_str() })),
        )
        .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn admin_promotion_enforces_membership_and_the_ceiling() {
        let state = state("admins");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        register(&state, &bob, "bob");
        let router = build(state.clone());
        let room = make_room(&router, &alice_token, "Team").await;

        let outsider = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/admins"),
            Some(&alice_token),
            Some(serde_json::json!({ "walletAddress": bob.as_str() })),
        )
        .await;
        assert_eq!(outsider.status, StatusCode::BAD_REQUEST);
        assert!(outsider.json()["message"]
            .as_str()
            .unwrap()
            .contains("must be a member"));

        join(&state, &room, &bob).await;
        let promoted = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/admins"),
            Some(&alice_token),
            Some(serde_json::json!({ "walletAddress": bob.as_str() })),
        )
        .await;
        assert_eq!(promoted.status, StatusCode::OK);

        let again = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/admins"),
            Some(&alice_token),
            Some(serde_json::json!({ "walletAddress": bob.as_str() })),
        )
        .await;
        assert_eq!(again.status, StatusCode::BAD_REQUEST);
        assert_eq!(again.json()["message"], "User is already an admin");
    }

    #[tokio::test]
    async fn the_room_never_runs_out_of_admins() {
        let state = state("last-admin-demote");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);
        let room = make_room(&router, &token, "Team").await;

        let response = send(
            &router,
            "DELETE",
            &format!("/api/rooms/{room}/admins/{}", alice.as_str()),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert!(response.json()["message"]
            .as_str()
            .unwrap()
            .contains("at least one admin"));
    }

    #[tokio::test]
    async fn hiding_requires_membership_and_is_idempotent() {
        let state = state("hide");
        let alice = wallet("alice");
        let mallory = wallet("mallory");
        let alice_token = register(&state, &alice, "alice");
        let mallory_token = register(&state, &mallory, "mallory");
        let router = build(state);
        let room = make_room(&router, &alice_token, "Team").await;

        let refused = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/hide"),
            Some(&mallory_token),
            None,
        )
        .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);

        for _ in 0..3 {
            let ok = send(
                &router,
                "POST",
                &format!("/api/rooms/{room}/hide"),
                Some(&alice_token),
                None,
            )
            .await;
            assert_eq!(ok.status, StatusCode::OK);
        }

        let hidden = send(
            &router,
            "GET",
            "/api/rooms/hidden",
            Some(&alice_token),
            None,
        )
        .await;
        assert_eq!(hidden.json().as_array().unwrap().len(), 1, "§15 #4");
        assert!(hidden.json()[0]["room"].get("unreadCount").is_none());

        let visible = send(&router, "GET", "/api/rooms", Some(&alice_token), None).await;
        assert!(visible.json().as_array().unwrap().is_empty());

        send(
            &router,
            "DELETE",
            &format!("/api/rooms/{room}/hide"),
            Some(&alice_token),
            None,
        )
        .await;
        let back = send(&router, "GET", "/api/rooms", Some(&alice_token), None).await;
        assert_eq!(back.json().as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_invalid_room_id_is_a_validation_error_everywhere() {
        let state = state("badroomid");
        let token = register(&state, &wallet("alice"), "alice");
        let router = build(state);

        // §15 #6: hide/unhide answered 500 for this in the reference.
        for (method, path) in [
            ("POST", "/api/rooms/bad!id/hide"),
            ("DELETE", "/api/rooms/bad!id/hide"),
            ("GET", "/api/rooms/bad!id"),
        ] {
            let response = send(&router, method, path, Some(&token), None).await;
            assert_eq!(
                response.status,
                StatusCode::BAD_REQUEST,
                "{method} {path} should be a 400"
            );
        }
    }

    #[tokio::test]
    async fn the_member_roster_is_member_only() {
        let state = state("roster");
        let alice = wallet("alice");
        let mallory = wallet("mallory");
        let alice_token = register(&state, &alice, "alice");
        let mallory_token = register(&state, &mallory, "mallory");
        let router = build(state);
        let room = make_room(&router, &alice_token, "Team").await;

        let refused = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/members"),
            Some(&mallory_token),
            None,
        )
        .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);

        let allowed = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/members"),
            Some(&alice_token),
            None,
        )
        .await;
        assert_eq!(allowed.json().as_array().unwrap().len(), 1);
        assert_eq!(allowed.json()[0]["user"]["username"], "alice");
    }
}
