//! Invite links (`docs/API.md` §6.7a, ROADMAP §7 M1).
//!
//! Invitations (§6.7) require the invitee to already have a wallet address the
//! admin can type. An invite *link* is the onboarding funnel for everybody
//! else: the token in the URL is the entire credential, so the person opening
//! it can create their wallet on the landing page and still end up in the
//! room. Creation and revocation are admin verbs; redeeming needs only a
//! signed-in wallet and the token.
//!
//! Deliberately **no block check at redeem**, where the address invite refuses
//! both directions. That refusal exists so an admin cannot drag somebody who
//! blocked them into a shared room — with a link nobody is dragged: the holder
//! redeems it for themself, the same opt-in the accept endpoint expresses.
//! Members who block each other already coexist in rooms, and rendering
//! handles it.

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::db::invites::{self, ConsumeOutcome, Invite, Refusal};
use crate::db::models::iso_ms;
use crate::db::{keys, now_ms, rooms};
use crate::error::{ApiError, ApiResult};
use crate::validate::{self, ValidJson};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/rooms/{roomId}/invites", post(create).get(list))
        .route("/rooms/{roomId}/invites/{inviteId}", delete(revoke))
        .route("/invites/{token}", get(peek))
        .route("/invites/{token}/redeem", post(redeem))
}

/// Default lifetime when the admin does not choose one: a week — long enough
/// to survive "I'll join after the weekend", short enough that a link pasted
/// into a channel is not a standing door.
const DEFAULT_EXPIRY_HOURS: i64 = 24 * 7;
/// Hard ceiling: thirty days. A longer door should be a fresh decision.
const MAX_EXPIRY_HOURS: i64 = 24 * 30;
/// Ceiling on `maxUses`, comfortably above the ~100-person deployments this
/// server targets while still refusing nonsense.
const MAX_USE_LIMIT: i64 = 1_000;

/// The public shape of one invite link — everything except the token.
fn view(invite: &Invite, now: i64) -> serde_json::Value {
    serde_json::json!({
        "id": invite.id,
        "roomId": invite.room_id,
        "createdBy": invite.created_by,
        "createdAt": iso_ms(invite.created_at),
        "expiresAt": iso_ms(invite.expires_at),
        "maxUses": invite.max_uses,
        "useCount": invite.use_count,
        // Computed here so the client never parses ISO strings against its
        // own clock just to grey a row out.
        "expired": invite.refusal(now).is_some(),
    })
}

/// Each refusal gets its own wording. All three are 404 — by the time a
/// holder sees any of them the link is equally dead — but the message tells
/// them whether to ask for a fresh link (expired, used up) or to take the
/// hint (revoked).
fn refusal_error(refusal: Refusal) -> ApiError {
    match refusal {
        Refusal::Revoked => ApiError::not_found("This invite link has been revoked"),
        Refusal::Expired => ApiError::not_found("This invite link has expired"),
        Refusal::Exhausted => ApiError::not_found("This invite link has reached its use limit"),
    }
}

/// Shape-check a presented token before hashing it. Anything that is not
/// `inv_` + 64 hex could never have been minted, so it gets the same 404 an
/// unissued token gets — no separate "malformed" answer to enumerate against.
fn presented_token(raw: &str) -> ApiResult<String> {
    let hex_part = raw.strip_prefix("inv_").unwrap_or("");
    if hex_part.len() != 64 || !hex_part.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ApiError::not_found("Invite link not found"));
    }
    Ok(invites::token_hash(raw))
}

#[derive(Debug, Deserialize)]
struct CreateBody {
    #[serde(rename = "expiresInHours")]
    expires_in_hours: Option<i64>,
    #[serde(rename = "maxUses")]
    max_uses: Option<i64>,
}

/// `POST /api/rooms/{roomId}/invites` — admins only.
///
/// The token appears in this response and nowhere else, ever again: only its
/// hash is stored, so a lost link cannot be re-copied from the server — the
/// admin mints a new one and revokes the old, which is also the better habit.
async fn create(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
    ValidJson(body): ValidJson<CreateBody>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    // Same guard order as the address invite (§6.7): existence, then kind,
    // then role — so a DM member hears "this doesn't apply here" rather than
    // a 403 about admin rights no DM has.
    super::rooms::require_room_exists(&state, &room).await?;
    super::rooms::require_channel(&state, &room, "create an invite link for").await?;
    super::rooms::require_admin(
        &state,
        &room,
        &caller,
        "Only room admins can create invite links",
    )
    .await?;

    let hours = body.expires_in_hours.unwrap_or(DEFAULT_EXPIRY_HOURS);
    if !(1..=MAX_EXPIRY_HOURS).contains(&hours) {
        return Err(ApiError::field(
            "expiresInHours",
            "must be between 1 and 720",
        ));
    }
    if let Some(max) = body.max_uses {
        if !(1..=MAX_USE_LIMIT).contains(&max) {
            return Err(ApiError::field("maxUses", "must be between 1 and 1000"));
        }
    }

    let token = invites::mint_token()?;
    let hash = invites::token_hash(&token);
    let id = format!("invite_{}_{}", now_ms(), uuid::Uuid::new_v4());
    let expires_at = now_ms() + hours * 60 * 60 * 1000;

    let invite = state
        .db
        .call({
            let (id, room_id) = (id.clone(), room.as_str().to_owned());
            let creator = caller.as_str().to_owned();
            let max_uses = body.max_uses;
            move |conn| invites::create(conn, &id, &room_id, &hash, &creator, expires_at, max_uses)
        })
        .await?;

    let _ = state.log.append_audit(
        "invite_created",
        Some(&caller),
        serde_json::json!({
            "roomId": room.as_str(),
            "inviteId": invite.id,
            "expiresAt": iso_ms(invite.expires_at),
            "maxUses": invite.max_uses,
        }),
    );

    Ok(Json(serde_json::json!({
        "message": "Invite link created",
        "invite": view(&invite, now_ms()),
        "token": token,
    }))
    .into_response())
}

/// `GET /api/rooms/{roomId}/invites` — admins only. The revocation list:
/// every link still out there, newest first, revoked ones omitted.
async fn list(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    super::rooms::require_room_exists(&state, &room).await?;
    super::rooms::require_admin(
        &state,
        &room,
        &caller,
        "Only room admins can list invite links",
    )
    .await?;

    let room_id = room.as_str().to_owned();
    let rows = state
        .db
        .call(move |conn| invites::list_for_room(conn, &room_id))
        .await?;
    let now = now_ms();
    let out: Vec<_> = rows.iter().map(|i| view(i, now)).collect();
    Ok(Json(out).into_response())
}

/// `DELETE /api/rooms/{roomId}/invites/{inviteId}` — admins only.
///
/// Takes effect immediately: the very next redeem attempt sees `revoked_at`
/// set. There is no undo — mint a new link instead, so "revoked" stays an
/// honest answer to anyone still holding the old one.
async fn revoke(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path((room_id, invite_id)): Path<(String, String)>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    super::rooms::require_room_exists(&state, &room).await?;
    super::rooms::require_admin(
        &state,
        &room,
        &caller,
        "Only room admins can revoke invite links",
    )
    .await?;

    let revoked = state
        .db
        .call({
            let (room_id, invite_id) = (room.as_str().to_owned(), invite_id.clone());
            move |conn| invites::revoke(conn, &room_id, &invite_id)
        })
        .await?;
    if !revoked {
        return Err(ApiError::not_found("Invite link not found"));
    }

    let _ = state.log.append_audit(
        "invite_revoked",
        Some(&caller),
        serde_json::json!({ "roomId": room.as_str(), "inviteId": invite_id }),
    );

    Ok(super::message("Invite link revoked"))
}

/// `GET /api/invites/{token}` — **no auth**, like the login challenge: the
/// person opening an invite link has no account yet, and the landing page
/// needs "you're invited to «Team chat»" before it asks them to make one.
///
/// A valid token reveals the room's name and size — that is the capability
/// working as designed. An invalid one reveals nothing beyond the refusal.
async fn peek(State(state): State<AppState>, Path(token): Path<String>) -> ApiResult<Response> {
    let hash = presented_token(&token)?;
    let out = state
        .db
        .call(move |conn| {
            let Some(invite) = invites::find_by_hash(conn, &hash)? else {
                return Err(ApiError::not_found("Invite link not found"));
            };
            if let Some(refusal) = invite.refusal(now_ms()) {
                return Err(refusal_error(refusal));
            }
            let Some(room) = rooms::get_room(conn, &invite.room_id)? else {
                return Err(ApiError::not_found("Room no longer exists"));
            };
            let member_count = rooms::list_members(conn, &invite.room_id)?.len();
            Ok(serde_json::json!({
                "roomName": room.name,
                "memberCount": member_count,
                "expiresAt": iso_ms(invite.expires_at),
            }))
        })
        .await?;
    Ok(Json(out).into_response())
}

/// `POST /api/invites/{token}/redeem` — any signed-in wallet.
///
/// Redeeming while already a member is a success that spends nothing: the
/// state the caller asked for already holds, and burning a use of a limited
/// link on a double-click would be a hostile way to say so.
///
/// A join sets `keyRotationPending` on encrypted rooms, mirroring leave/kick
/// from the other side: the joiner holds **no** wrap for the current epoch, so
/// nothing sealed under it can reach them until a member rotates — and
/// `keys::rotate` refuses any rotation that does not cover the whole roster,
/// joiner included. An unencrypted room is left unflagged; plaintext posts
/// skip the epoch check, and the flag would only conjure a rotation banner
/// with nothing to rotate.
async fn redeem(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(token): Path<String>,
) -> ApiResult<Response> {
    let hash = presented_token(&token)?;
    let address = caller.as_str().to_owned();
    let (room, already_member) = state
        .db
        .call(move |conn| {
            let Some(invite) = invites::find_by_hash(conn, &hash)? else {
                return Err(ApiError::not_found("Invite link not found"));
            };
            let Some(room) = rooms::get_room(conn, &invite.room_id)? else {
                // Cannot happen — invites cascade with the room — but the
                // check costs nothing and the alternative is a foreign-key
                // error surfacing as a 500.
                return Err(ApiError::not_found("Room no longer exists"));
            };
            // Membership before refusal: the double-click that spent the last
            // use of a one-use link is the very caller most likely to present
            // it again, and the state they asked for already holds.
            if rooms::is_member(conn, &invite.room_id, &address)? {
                return Ok((room, true));
            }
            if let Some(refusal) = invite.refusal(now_ms()) {
                return Err(refusal_error(refusal));
            }
            // The conditional UPDATE re-checks everything the peek above saw,
            // so a revocation or a rival redeem landing in between still
            // refuses correctly.
            match invites::consume(conn, &hash)? {
                ConsumeOutcome::Consumed(_) => {}
                ConsumeOutcome::NotFound => {
                    return Err(ApiError::not_found("Invite link not found"))
                }
                ConsumeOutcome::Refused(refusal) => return Err(refusal_error(refusal)),
            }
            rooms::add_member(conn, &invite.room_id, &address)?;
            if keys::has_encryption(conn, &invite.room_id)? {
                rooms::set_key_rotation_pending(conn, &invite.room_id, true)?;
            }
            Ok((room, false))
        })
        .await?;

    if !already_member {
        let _ = state.log.append_audit(
            "invite_redeemed",
            Some(&caller),
            serde_json::json!({ "roomId": room.id }),
        );
        let room_id = validate::room_id(&room.id)?;
        super::rooms::after_membership_change(&state, &room_id, &caller).await;
    }

    Ok(Json(serde_json::json!({
        "message": if already_member { "Already a member" } else { "Joined room" },
        "roomId": room.id,
        "roomName": room.name,
        "alreadyMember": already_member,
    }))
    .into_response())
}

#[cfg(test)]
mod tests {
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

    async fn make_link(
        router: &Router,
        room: &str,
        token: &str,
        body: serde_json::Value,
    ) -> (String, String) {
        let created = send(
            router,
            "POST",
            &format!("/api/rooms/{room}/invites"),
            Some(token),
            Some(body),
        )
        .await;
        assert_eq!(created.status, StatusCode::OK);
        (
            created.json()["token"].as_str().unwrap().to_owned(),
            created.json()["invite"]["id"].as_str().unwrap().to_owned(),
        )
    }

    #[tokio::test]
    async fn the_token_is_returned_once_and_never_stored() {
        let state = state("invite-link-create");
        let alice = wallet("alice");
        let alice_token = register(&state, &alice, "alice");
        let router = build(state.clone());
        let room = make_room(&router, &alice_token).await;

        let (token, invite_id) =
            make_link(&router, &room, &alice_token, serde_json::json!({})).await;
        assert!(token.starts_with("inv_"));

        // Neither the list nor the database carries the token itself.
        let listed = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/invites"),
            Some(&alice_token),
            None,
        )
        .await;
        assert_eq!(listed.json()[0]["id"], invite_id.as_str());
        assert!(listed.json()[0].get("token").is_none());
        let stored: String = state
            .db
            .call_blocking(move |conn| {
                Ok(conn.query_row("SELECT token_hash FROM room_invites", [], |r| r.get(0))?)
            })
            .unwrap();
        assert_ne!(stored, token, "only the hash may be stored");
        assert_eq!(stored, crate::db::invites::token_hash(&token));
    }

    #[tokio::test]
    async fn creating_listing_and_revoking_are_admin_only() {
        let state = state("invite-link-admin");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state);
        let room = make_room(&router, &alice_token).await;
        let (_, invite_id) = make_link(&router, &room, &alice_token, serde_json::json!({})).await;

        let created = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/invites"),
            Some(&bob_token),
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(created.status, StatusCode::FORBIDDEN);

        let listed = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/invites"),
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(listed.status, StatusCode::FORBIDDEN);

        let revoked = send(
            &router,
            "DELETE",
            &format!("/api/rooms/{room}/invites/{invite_id}"),
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(revoked.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn redeeming_seats_the_caller_without_flagging_a_plaintext_room() {
        let state = state("invite-link-redeem");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state.clone());
        let room = make_room(&router, &alice_token).await;
        let (token, _) = make_link(&router, &room, &alice_token, serde_json::json!({})).await;

        // The peek needs no auth — the holder has no account yet.
        let peeked = send(&router, "GET", &format!("/api/invites/{token}"), None, None).await;
        assert_eq!(peeked.status, StatusCode::OK);
        assert_eq!(peeked.json()["roomName"], "Team");
        assert_eq!(peeked.json()["memberCount"], 1);

        let redeemed = send(
            &router,
            "POST",
            &format!("/api/invites/{token}/redeem"),
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(redeemed.status, StatusCode::OK);
        assert_eq!(redeemed.json()["roomId"], room);
        assert_eq!(redeemed.json()["alreadyMember"], false);

        let rooms_now = send(&router, "GET", "/api/rooms", Some(&bob_token), None).await;
        assert_eq!(rooms_now.json().as_array().unwrap().len(), 1);
        // No wraps exist, so joining must not demand a rotation of nothing.
        assert_eq!(rooms_now.json()[0]["keyRotationPending"], false);
    }

    #[tokio::test]
    async fn joining_an_encrypted_room_demands_a_rotation() {
        let state = state("invite-link-rekey");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state);
        let room = make_room(&router, &alice_token).await;
        // Alice keys the room for herself: it is now encrypted.
        send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/keys"),
            Some(&alice_token),
            Some(serde_json::json!({
                "userAddress": alice.as_str(),
                "encryptedSymmetricKey": "wrapped",
                "ephemeralPublicKey": "04ab",
                "encryptionIV": "1a2b3c4d5e6f78901234567890abcdef",
                "hmac": "9".repeat(64),
                "keyVersion": 1,
            })),
        )
        .await;
        let (token, _) = make_link(&router, &room, &alice_token, serde_json::json!({})).await;

        send(
            &router,
            "POST",
            &format!("/api/invites/{token}/redeem"),
            Some(&bob_token),
            None,
        )
        .await;

        // Bob holds no wrap for the current epoch, so nothing new may be
        // sealed under it until someone rotates — same rule as leave/kick.
        let detail = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}"),
            Some(&alice_token),
            None,
        )
        .await;
        assert_eq!(detail.json()["keyRotationPending"], true);
    }

    #[tokio::test]
    async fn an_expired_link_is_refused_with_its_own_words() {
        let state = state("invite-link-expiry");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state.clone());
        let room = make_room(&router, &alice_token).await;
        let (token, invite_id) =
            make_link(&router, &room, &alice_token, serde_json::json!({})).await;

        // Age the link past its expiry by hand — no clock to wait on.
        state
            .db
            .call_blocking({
                let invite_id = invite_id.clone();
                move |conn| {
                    conn.execute(
                        "UPDATE room_invites SET expires_at = 1 WHERE id = ?1",
                        rusqlite::params![invite_id],
                    )?;
                    Ok(())
                }
            })
            .unwrap();

        for (method, path) in [
            ("GET", format!("/api/invites/{token}")),
            ("POST", format!("/api/invites/{token}/redeem")),
        ] {
            let refused = send(&router, method, &path, Some(&bob_token), None).await;
            assert_eq!(refused.status, StatusCode::NOT_FOUND);
            assert_eq!(refused.json()["message"], "This invite link has expired");
        }

        // Expired links stay on the ledger, marked, until revoked.
        let listed = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/invites"),
            Some(&alice_token),
            None,
        )
        .await;
        assert_eq!(listed.json()[0]["expired"], true);
    }

    #[tokio::test]
    async fn revocation_kills_the_link_between_peek_and_redeem() {
        let state = state("invite-link-revoke");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state);
        let room = make_room(&router, &alice_token).await;
        let (token, invite_id) =
            make_link(&router, &room, &alice_token, serde_json::json!({})).await;

        // The link works…
        let peeked = send(&router, "GET", &format!("/api/invites/{token}"), None, None).await;
        assert_eq!(peeked.status, StatusCode::OK);

        // …until the admin revokes it; the very next attempt is refused.
        let revoked = send(
            &router,
            "DELETE",
            &format!("/api/rooms/{room}/invites/{invite_id}"),
            Some(&alice_token),
            None,
        )
        .await;
        assert_eq!(revoked.status, StatusCode::OK);

        let redeemed = send(
            &router,
            "POST",
            &format!("/api/invites/{token}/redeem"),
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(redeemed.status, StatusCode::NOT_FOUND);
        assert_eq!(
            redeemed.json()["message"],
            "This invite link has been revoked"
        );

        // And it is off the revocation list.
        let listed = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/invites"),
            Some(&alice_token),
            None,
        )
        .await;
        assert!(listed.json().as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_use_limit_spends_on_joins_but_not_on_rejoins() {
        let state = state("invite-link-uses");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let carol = wallet("carol");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let carol_token = register(&state, &carol, "carol");
        let router = build(state);
        let room = make_room(&router, &alice_token).await;
        let (token, _) = make_link(
            &router,
            &room,
            &alice_token,
            serde_json::json!({ "maxUses": 1 }),
        )
        .await;

        let first = send(
            &router,
            "POST",
            &format!("/api/invites/{token}/redeem"),
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(first.status, StatusCode::OK);

        // Bob double-clicking is a success that spends nothing.
        let again = send(
            &router,
            "POST",
            &format!("/api/invites/{token}/redeem"),
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(again.status, StatusCode::OK);
        assert_eq!(again.json()["alreadyMember"], true);

        // Carol finds the budget spent.
        let refused = send(
            &router,
            "POST",
            &format!("/api/invites/{token}/redeem"),
            Some(&carol_token),
            None,
        )
        .await;
        assert_eq!(refused.status, StatusCode::NOT_FOUND);
        assert_eq!(
            refused.json()["message"],
            "This invite link has reached its use limit"
        );
    }

    #[tokio::test]
    async fn a_token_nobody_minted_is_simply_not_found() {
        let state = state("invite-link-guess");
        let alice = wallet("alice");
        let alice_token = register(&state, &alice, "alice");
        let router = build(state);

        for guess in ["inv_not_hex", &crate::db::invites::mint_token().unwrap()] {
            let refused = send(
                &router,
                "POST",
                &format!("/api/invites/{guess}/redeem"),
                Some(&alice_token),
                None,
            )
            .await;
            assert_eq!(refused.status, StatusCode::NOT_FOUND);
            assert_eq!(refused.json()["message"], "Invite link not found");
        }
    }

    #[tokio::test]
    async fn expiry_and_use_limits_are_bounded_at_create() {
        let state = state("invite-link-bounds");
        let alice = wallet("alice");
        let alice_token = register(&state, &alice, "alice");
        let router = build(state);
        let room = make_room(&router, &alice_token).await;

        for body in [
            serde_json::json!({ "expiresInHours": 0 }),
            serde_json::json!({ "expiresInHours": 721 }),
            serde_json::json!({ "maxUses": 0 }),
            serde_json::json!({ "maxUses": 1001 }),
        ] {
            let refused = send(
                &router,
                "POST",
                &format!("/api/rooms/{room}/invites"),
                Some(&alice_token),
                Some(body),
            )
            .await;
            assert_eq!(refused.status, StatusCode::BAD_REQUEST);
        }
    }

    #[tokio::test]
    async fn a_dm_cannot_grow_an_invite_link() {
        let state = state("invite-link-dm");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let _ = register(&state, &bob, "bob");
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
            &format!("/api/rooms/{dm_id}/invites"),
            Some(&alice_token),
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    }
}
