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
        .route("/rooms/dm", post(open_dm))
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

/// Require that the caller can administer this room.
///
/// A server administrator passes without being a room admin, and without
/// being a member. That is the point of the role: a room whose last admin has
/// left is otherwise unmanageable forever, and "ask the person who is gone"
/// is not an answer an operator can act on. It is a real power and it is
/// meant to be — it is also why the list of who holds it lives in the
/// deployment's configuration rather than anywhere a request can reach.
pub(super) async fn require_admin(
    state: &AppState,
    room: &RoomId,
    caller: &WalletAddress,
    message: &str,
) -> ApiResult<()> {
    if super::misc::is_server_admin(caller.as_str()) {
        return Ok(());
    }
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

/// Refuse a verb that only makes sense for a channel.
///
/// A DM has no name anybody chose, no roster to curate and no membership to
/// grant, so rename / invite / kick / promote have nothing to act on. Refusing
/// is better than quietly succeeding: a renamed DM would show one title to the
/// person who set it and a derived one to everybody else, and a DM you could
/// be added to would not be the conversation the other person opened.
///
/// 400 rather than 403 on purpose — the caller is not unauthorised, the
/// request does not apply.
async fn require_channel(state: &AppState, room: &RoomId, verb: &str) -> ApiResult<()> {
    if is_direct(state, room).await? {
        return Err(ApiError::bad_request(format!(
            "Cannot {verb} a direct message."
        )));
    }
    Ok(())
}

/// Whether this room is a DM. Separate from [`require_channel`] so a caller
/// that wants its own wording — `leave` does — can ask the question without
/// having to catch and reinterpret an error, which would also swallow the
/// database failures this can legitimately return.
async fn is_direct(state: &AppState, room: &RoomId) -> ApiResult<bool> {
    let room_id = room.as_str().to_owned();
    let record = state
        .db
        .call(move |conn| rooms::get_room(conn, &room_id))
        .await?
        .ok_or_else(|| ApiError::not_found("Room not found"))?;
    Ok(record.is_direct())
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

#[derive(Debug, Deserialize)]
struct DmBody {
    /// The one-recipient shorthand, which is the overwhelmingly common case.
    #[serde(rename = "walletAddress")]
    wallet_address: Option<String>,
    /// The general form. Merged with the shorthand rather than exclusive with
    /// it, so a client that sends both is not a request anyone has to reason
    /// about.
    #[serde(rename = "walletAddresses")]
    wallet_addresses: Option<Vec<String>>,
}

/// `POST /api/rooms/dm` — open a direct message, or return the existing one.
///
/// **Idempotent by identity, not by convention.** The room is keyed on its
/// member set (`rooms::dm_key`), so this endpoint is the answer to "the
/// conversation between these people" rather than a create call that happens
/// to be safe to retry. Two people can press "message" at the same moment from
/// two devices and land in one room.
///
/// The caller is always a member: you cannot open a conversation you are not
/// in. Naming only yourself is allowed and gives the private room a person
/// keeps notes in — the same mechanism, with a one-element set.
///
/// Recipients must have registered, because a DM to an address nobody has ever
/// signed in with is a room with a member the roster cannot render, and the
/// mistake is far more often a mistyped address than a genuine invitation to
/// somebody who has not arrived yet. Channel invitations remain the way to
/// reach someone who is not here.
///
/// Returns the enriched room, not the bare one: a client has to name a DM
/// after its other members, so it needs the roster in the same response that
/// tells it the room exists.
async fn open_dm(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    ValidJson(body): ValidJson<DmBody>,
) -> ApiResult<Response> {
    let raw: Vec<String> = body
        .wallet_address
        .into_iter()
        .chain(body.wallet_addresses.unwrap_or_default())
        .collect();
    if raw.is_empty() {
        return Err(validate::required("walletAddress", "a wallet address"));
    }

    // The caller first, so a single-element request is a note to self rather
    // than an empty set, and so every parsed address below is a recipient.
    let mut members = vec![caller.as_str().to_owned()];
    for address in &raw {
        let parsed = validate::wallet_address("walletAddress", Some(address.as_str()))?;
        members.push(parsed.as_str().to_owned());
    }

    // All members are admins (see `rooms::create_dm`), so the DM ceiling is
    // the admin ceiling. Counting the canonical set, not the request, means a
    // list padded with duplicates is not rejected for being long.
    let distinct = rooms::dm_key(&members).split('|').count() as i64;
    if distinct > MAX_ADMINS {
        return Err(ApiError::bad_request(format!(
            "A direct message can include at most {MAX_ADMINS} people. Create a room instead."
        )));
    }

    let id = format!("room_{}_{}", crate::db::now_ms(), uuid::Uuid::new_v4());
    let address = caller.as_str().to_owned();
    let (room_id, detail) = state
        .db
        .call({
            let members = members.clone();
            move |conn| {
                for member in &members {
                    if users::get_user(conn, member)?.is_none() {
                        return Err(ApiError::not_found(
                            "That wallet has not signed in to this server yet.",
                        ));
                    }
                }
                let room = rooms::create_dm(conn, &id, &members)?;
                let detail = storage::room_detail(conn, &room.id, &address, true)?;
                Ok((room.id, detail))
            }
        })
        .await?;

    let detail = detail.ok_or_else(|| ApiError::not_found("Room not found"))?;

    // Everyone in it needs the room in their subscription set and in their
    // room list — unlike a channel, whose creator is its only member, a DM is
    // live for somebody who did not ask for it.
    for member in &members {
        let Ok(wallet) = WalletAddress::new(member) else {
            continue;
        };
        if let Err(e) = state.hub.refresh_user_rooms(&wallet).await {
            tracing::warn!(error = %e, "could not refresh room subscriptions after opening a DM");
        }
        if wallet != caller {
            state
                .hub
                .publish_best_effort(Target::User { wallet }, None, ServerEvent::RoomsUpdated)
                .await;
        }
    }

    tracing::debug!(room = %room_id, "direct message opened");
    Ok(Json(detail).into_response())
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
    require_channel(&state, &room, "rename").await?;

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

    // Not just the rows: destroying a room destroys what it was showing and
    // what was attached to it, on disk as well (`crate::purge`). Anything the
    // bytes are still needed for elsewhere — another room's copy, somebody's
    // avatar — survives; nothing that was only this room's does.
    let purged = crate::purge::destroy_room(&state, &room_id, Some(&caller)).await?;

    let _ = state.log.append_audit(
        "room_deleted",
        Some(&caller),
        serde_json::json!({ "roomId": room.as_str(), "purged": purged }),
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

    // The counts ride along so a client can say what was erased rather than
    // only that something was. Additive to the `{message}` shape every other
    // command endpoint returns, so an older client ignores them.
    Ok((
        axum::http::StatusCode::OK,
        Json(serde_json::json!({
            "message": "Room deleted successfully",
            "purged": purged,
        })),
    )
        .into_response())
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
    // Leaving a DM has no meaning the other side would recognise: the
    // conversation is its member set, so a departed member would leave a room
    // that still answers to a key naming them — and re-opening the DM would
    // find it and refuse them entry to their own history. Hiding is the verb
    // for "stop showing me this", and it is reversible.
    if is_direct(&state, &room).await? {
        return Err(ApiError::bad_request(
            "Cannot leave a direct message. Hide it instead.",
        ));
    }

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
    require_channel(&state, &room, "remove someone from").await?;

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
    // Every member of a DM is already an admin of it, so there is nobody to
    // promote and no hierarchy the promotion would express.
    require_channel(&state, &room, "change the admins of").await?;

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
    require_channel(&state, &room, "change the admins of").await?;

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
        assert_eq!(listed.json()[0]["kind"], "channel");
    }

    #[tokio::test]
    async fn opening_a_dm_is_idempotent_from_either_side() {
        let state = state("dm-open");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state);

        let opened = send(
            &router,
            "POST",
            "/api/rooms/dm",
            Some(&alice_token),
            Some(serde_json::json!({ "walletAddress": bob.as_str() })),
        )
        .await;
        assert_eq!(opened.status, StatusCode::OK);
        assert_eq!(opened.json()["kind"], "dm");
        // Enriched, not bare: the client has to name the DM after its other
        // member, so the roster travels with it.
        assert_eq!(opened.json()["memberCount"], 2);
        let room_id = opened.json()["id"].as_str().unwrap().to_owned();

        // Bob opening "the conversation with Alice" must find Alice's room,
        // not open a second one beside it.
        let from_bob = send(
            &router,
            "POST",
            "/api/rooms/dm",
            Some(&bob_token),
            Some(serde_json::json!({ "walletAddress": alice.as_str() })),
        )
        .await;
        assert_eq!(from_bob.json()["id"], room_id);

        for token in [&alice_token, &bob_token] {
            let listed = send(&router, "GET", "/api/rooms", Some(token), None).await;
            assert_eq!(listed.json().as_array().unwrap().len(), 1);
            assert_eq!(listed.json()[0]["id"], room_id);
        }
    }

    #[tokio::test]
    async fn a_dm_refuses_every_verb_that_needs_a_channel() {
        let state = state("dm-verbs");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        register(&state, &bob, "bob");
        let carol = wallet("carol");
        register(&state, &carol, "carol");
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

        // Alice is an admin of the DM, so none of these are refused for
        // *permission* — they are refused because the verb does not apply.
        let cases: Vec<(&str, String, Option<serde_json::Value>)> = vec![
            (
                "PATCH",
                format!("/api/rooms/{room}"),
                Some(serde_json::json!({ "name": "Renamed" })),
            ),
            (
                "POST",
                format!("/api/rooms/{room}/invite"),
                Some(serde_json::json!({ "userAddress": carol.as_str() })),
            ),
            (
                "POST",
                format!("/api/rooms/{room}/kick"),
                Some(serde_json::json!({ "userAddress": bob.as_str() })),
            ),
            (
                "POST",
                format!("/api/rooms/{room}/leave"),
                Some(serde_json::json!({})),
            ),
            (
                "POST",
                format!("/api/rooms/{room}/admins"),
                Some(serde_json::json!({ "walletAddress": bob.as_str() })),
            ),
        ];
        for (method, path, body) in cases {
            let response = send(&router, method, &path, Some(&alice_token), body).await;
            assert_eq!(
                response.status,
                StatusCode::BAD_REQUEST,
                "{method} {path} should not apply to a direct message"
            );
        }

        // Hiding is the one that *is* offered instead of leaving.
        let hidden = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/hide"),
            Some(&alice_token),
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(hidden.status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_dm_to_an_unknown_wallet_is_refused() {
        let state = state("dm-stranger");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let stranger = wallet("nobody-here");
        let router = build(state);

        let response = send(
            &router,
            "POST",
            "/api/rooms/dm",
            Some(&token),
            Some(serde_json::json!({ "walletAddress": stranger.as_str() })),
        )
        .await;
        // A mistyped address is far likelier than an invitation to somebody
        // who has genuinely never arrived, so this fails loudly rather than
        // creating a room with an unrenderable member.
        assert_eq!(response.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_dm_naming_only_yourself_is_a_private_notebook() {
        let state = state("dm-self");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        let opened = send(
            &router,
            "POST",
            "/api/rooms/dm",
            Some(&token),
            Some(serde_json::json!({ "walletAddress": alice.as_str() })),
        )
        .await;
        assert_eq!(opened.status, StatusCode::OK);
        assert_eq!(opened.json()["kind"], "dm");
        assert_eq!(opened.json()["memberCount"], 1);
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
