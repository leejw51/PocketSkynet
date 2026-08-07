//! Invitations (`docs/API.md` §6.7).
//!
//! Rooms are not discoverable: creating one and accepting an invitation are
//! the only two ways to become a member. That is why an invitation is a real
//! row rather than a notification — the invitee's acceptance is what creates
//! the membership, and an admin cannot force somebody into a room.

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use pocketskynet_core::{ServerEvent, Target};
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::db::{keys, rooms, users};
use crate::error::{ApiError, ApiResult};
use crate::validate::{self, ValidJson};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/rooms/{roomId}/invite", post(invite))
        .route("/invitations", get(list))
        .route("/invitations/{roomId}/accept", post(accept))
        .route("/invitations/{roomId}/decline", post(decline))
}

#[derive(Debug, Deserialize)]
struct InviteBody {
    #[serde(rename = "userAddress")]
    user_address: Option<String>,
}

/// `POST /api/rooms/{roomId}/invite` — admins only.
///
/// Blocking is enforced in **both** directions: neither party can drag the
/// other into a shared room, which would otherwise be a way to keep messaging
/// somebody who blocked you.
async fn invite(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
    ValidJson(body): ValidJson<InviteBody>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    let invitee = validate::wallet_address("userAddress", body.user_address.as_deref())?;

    let room_id = room.as_str().to_owned();
    let inviter = caller.as_str().to_owned();
    let target = invitee.as_str().to_owned();

    state
        .db
        .call(move |conn| {
            let Some(record) = rooms::get_room(conn, &room_id)? else {
                return Err(ApiError::not_found("Room not found"));
            };
            // A built-in room's roster is recomputed from its owner and the
            // server's admin list on every listing, so an invitation into one
            // is a promise the next fetch would break. "My Note" is the sharp
            // case: it is the one room on the server whose whole contract is
            // that nobody else is ever in it, and an invitation is precisely
            // the request that would end that.
            if record.is_static() {
                return Err(ApiError::bad_request(
                    "Cannot invite anyone to a built-in room. Its members are decided by the server.",
                ));
            }
            // A DM is the conversation between the people in it. Adding a
            // third would silently turn the room two people believed was
            // private into one that is not, and there is no notion of
            // "accepting" your way into somebody else's DM.
            if record.is_direct() {
                return Err(ApiError::bad_request(
                    "Cannot invite anyone to a direct message. Start a group message instead.",
                ));
            }
            if !rooms::is_admin(conn, &room_id, &inviter)? {
                return Err(ApiError::forbidden("Only room admins can invite users"));
            }
            if !users::user_exists(conn, &target)? {
                return Err(ApiError::not_found("User not found"));
            }
            if rooms::is_member(conn, &room_id, &target)? {
                return Err(ApiError::bad_request(
                    "User is already a member of this room",
                ));
            }
            if users::is_blocked(conn, &target, &inviter)? {
                return Err(ApiError::forbidden(
                    "You cannot invite users who have blocked you",
                ));
            }
            if users::is_blocked(conn, &inviter, &target)? {
                return Err(ApiError::forbidden(
                    "You cannot invite users you have blocked",
                ));
            }
            rooms::create_invitation(conn, &room_id, &target, &inviter)
        })
        .await?;

    state
        .hub
        .publish_best_effort(
            Target::User {
                wallet: invitee.clone(),
            },
            Some(caller.clone()),
            ServerEvent::InvitationReceived {
                room_id: room.clone(),
            },
        )
        .await;

    Ok(Json(serde_json::json!({ "message": "Invitation sent", "pending": true })).into_response())
}

/// `GET /api/invitations` — the caller's pending invitations, newest first.
async fn list(State(state): State<AppState>, AuthUser(caller): AuthUser) -> ApiResult<Response> {
    let address = caller.as_str().to_owned();
    let out = state
        .db
        .call(move |conn| rooms::list_invitations(conn, &address))
        .await?;
    Ok(Json(out).into_response())
}

/// `POST /api/invitations/{roomId}/accept`.
///
/// Any room key an admin pre-wrapped for the invitee at invite time survives
/// and becomes readable now — that pre-wrap is the whole reason invitations
/// exist as a distinct state rather than an immediate join.
async fn accept(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    let room_id = room.as_str().to_owned();
    let address = caller.as_str().to_owned();

    state
        .db
        .call(move |conn| {
            if !rooms::has_pending_invitation(conn, &room_id, &address)? {
                return Err(ApiError::not_found("No pending invitation for this room"));
            }
            if rooms::get_room(conn, &room_id)?.is_none() {
                // Cannot happen now that invitations cascade with the room
                // (§15 #11), but the check costs nothing and the alternative
                // is a foreign-key error surfacing as a 500.
                rooms::delete_invitation(conn, &room_id, &address)?;
                return Err(ApiError::not_found("Room no longer exists"));
            }
            rooms::add_member(conn, &room_id, &address)?;
            rooms::delete_invitation(conn, &room_id, &address)
        })
        .await?;

    // Reuses the roster-changed signal: `member_removed` means "the roster
    // moved", not literally "somebody left".
    super::rooms::after_membership_change(&state, &room, &caller).await;

    Ok(Json(serde_json::json!({
        "message": "Invitation accepted",
        "roomId": room.as_str(),
    }))
    .into_response())
}

/// `POST /api/invitations/{roomId}/decline`.
///
/// Any pre-wrapped key is discarded across every epoch: declining should leave
/// nothing behind that could later be used to read the room.
///
/// The inviter is deliberately not notified — a decline is private.
async fn decline(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    let room_id = room.as_str().to_owned();
    let address = caller.as_str().to_owned();

    state
        .db
        .call(move |conn| {
            if !rooms::has_pending_invitation(conn, &room_id, &address)? {
                return Err(ApiError::not_found("No pending invitation for this room"));
            }
            rooms::delete_invitation(conn, &room_id, &address)?;
            keys::delete_user_keys(conn, &room_id, &address)
        })
        .await?;

    Ok(super::message("Invitation declined"))
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
            Some(serde_json::json!({ "name": "Team" })),
        )
        .await
        .json()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[tokio::test]
    async fn an_invitation_creates_no_membership_until_accepted() {
        let state = state("invite-accept");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state.clone());
        let room = make_room(&router, &alice_token).await;

        let invited = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/invite"),
            Some(&alice_token),
            Some(serde_json::json!({ "userAddress": bob.as_str() })),
        )
        .await;
        assert_eq!(invited.status, StatusCode::OK);
        assert_eq!(invited.json()["pending"], true);

        let is_member = state
            .db
            .call_blocking({
                let room = room.clone();
                let bob = bob.as_str().to_owned();
                move |conn| rooms::is_member(conn, &room, &bob)
            })
            .unwrap();
        assert!(!is_member, "the invitee must opt in");

        let inbox = send(&router, "GET", "/api/invitations", Some(&bob_token), None).await;
        assert_eq!(inbox.json().as_array().unwrap().len(), 1);
        assert_eq!(inbox.json()[0]["inviterUsername"], "alice");
        assert_eq!(inbox.json()[0]["roomName"], "Team");

        let accepted = send(
            &router,
            "POST",
            &format!("/api/invitations/{room}/accept"),
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(accepted.status, StatusCode::OK);
        assert_eq!(accepted.json()["roomId"], room);

        let rooms_now = send(&router, "GET", "/api/rooms", Some(&bob_token), None).await;
        assert_eq!(rooms_now.json().as_array().unwrap().len(), 1);

        // The invitation is consumed.
        let empty = send(&router, "GET", "/api/invitations", Some(&bob_token), None).await;
        assert!(empty.json().as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn re_inviting_is_a_no_op_rather_than_an_error() {
        let state = state("invite-repeat");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state);
        let room = make_room(&router, &alice_token).await;

        for _ in 0..3 {
            let response = send(
                &router,
                "POST",
                &format!("/api/rooms/{room}/invite"),
                Some(&alice_token),
                Some(serde_json::json!({ "userAddress": bob.as_str() })),
            )
            .await;
            assert_eq!(response.status, StatusCode::OK);
        }

        let inbox = send(&router, "GET", "/api/invitations", Some(&bob_token), None).await;
        assert_eq!(inbox.json().as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn inviting_is_admin_only_and_checks_the_target() {
        let state = state("invite-guards");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state.clone());
        let room = make_room(&router, &alice_token).await;

        let by_stranger = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/invite"),
            Some(&bob_token),
            Some(serde_json::json!({ "userAddress": alice.as_str() })),
        )
        .await;
        assert_eq!(by_stranger.status, StatusCode::FORBIDDEN);

        let unknown_target = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/invite"),
            Some(&alice_token),
            Some(serde_json::json!({ "userAddress": wallet("ghost").as_str() })),
        )
        .await;
        assert_eq!(unknown_target.status, StatusCode::NOT_FOUND);

        let already_in = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/invite"),
            Some(&alice_token),
            Some(serde_json::json!({ "userAddress": alice.as_str() })),
        )
        .await;
        assert_eq!(already_in.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            already_in.json()["message"],
            "User is already a member of this room"
        );
    }

    #[tokio::test]
    async fn a_block_stops_an_invitation_in_both_directions() {
        let state = state("invite-blocked");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state);
        let room = make_room(&router, &alice_token).await;

        // Bob blocks Alice; Alice cannot invite him.
        send(
            &router,
            "POST",
            "/api/users/block",
            Some(&bob_token),
            Some(serde_json::json!({ "address": alice.as_str() })),
        )
        .await;

        let blocked_by_target = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/invite"),
            Some(&alice_token),
            Some(serde_json::json!({ "userAddress": bob.as_str() })),
        )
        .await;
        assert_eq!(blocked_by_target.status, StatusCode::FORBIDDEN);
        assert_eq!(
            blocked_by_target.json()["message"],
            "You cannot invite users who have blocked you"
        );

        // And the other way round.
        send(
            &router,
            "DELETE",
            &format!("/api/users/block/{}", alice.as_str()),
            Some(&bob_token),
            None,
        )
        .await;
        send(
            &router,
            "POST",
            "/api/users/block",
            Some(&alice_token),
            Some(serde_json::json!({ "address": bob.as_str() })),
        )
        .await;

        let blocked_target = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/invite"),
            Some(&alice_token),
            Some(serde_json::json!({ "userAddress": bob.as_str() })),
        )
        .await;
        assert_eq!(
            blocked_target.json()["message"],
            "You cannot invite users you have blocked"
        );
    }

    #[tokio::test]
    async fn declining_discards_any_pre_wrapped_key() {
        let state = state("decline");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state.clone());
        let room = make_room(&router, &alice_token).await;

        send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/invite"),
            Some(&alice_token),
            Some(serde_json::json!({ "userAddress": bob.as_str() })),
        )
        .await;

        // Alice pre-wraps the room key while she is online.
        send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/keys"),
            Some(&alice_token),
            Some(serde_json::json!({
                "userAddress": bob.as_str(),
                "encryptedSymmetricKey": "wrapped",
                "ephemeralPublicKey": "04ab",
                "encryptionIV": "1a2b3c4d5e6f78901234567890abcdef",
                "hmac": "9".repeat(64),
                "keyVersion": 1,
            })),
        )
        .await;

        let declined = send(
            &router,
            "POST",
            &format!("/api/invitations/{room}/decline"),
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(declined.status, StatusCode::OK);

        let wraps = state
            .db
            .call_blocking({
                let room = room.clone();
                let bob = bob.as_str().to_owned();
                move |conn| Ok(keys::all_keys(conn, &room, &bob)?.len())
            })
            .unwrap();
        assert_eq!(wraps, 0, "declining must leave nothing readable behind");
    }

    #[tokio::test]
    async fn acting_on_an_invitation_you_do_not_have_is_a_404() {
        let state = state("invite-missing");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state);
        let room = make_room(&router, &alice_token).await;

        for action in ["accept", "decline"] {
            let response = send(
                &router,
                "POST",
                &format!("/api/invitations/{room}/{action}"),
                Some(&bob_token),
                None,
            )
            .await;
            assert_eq!(response.status, StatusCode::NOT_FOUND);
            assert_eq!(
                response.json()["message"],
                "No pending invitation for this room"
            );
        }
    }

    #[tokio::test]
    async fn deleting_a_room_takes_its_invitations_with_it() {
        let state = state("invite-orphans");
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
            "DELETE",
            &format!("/api/rooms/{room}"),
            Some(&alice_token),
            None,
        )
        .await;

        // §15 #11: the reference orphaned these rows forever.
        let inbox = send(&router, "GET", "/api/invitations", Some(&bob_token), None).await;
        assert!(inbox.json().as_array().unwrap().is_empty());

        let accept = send(
            &router,
            "POST",
            &format!("/api/invitations/{room}/accept"),
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(accept.status, StatusCode::NOT_FOUND);
    }
}
