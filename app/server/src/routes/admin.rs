//! Server administration (`docs/API.md` §6.14).
//!
//! # What an admin is
//!
//! A wallet listed in `VITE_FRUITNATION_ADMIN`. Not a row, not a role anybody
//! can grant at runtime — see [`super::misc::server_admins`] for why. Every
//! handler below takes the [`ServerAdmin`] extractor, so the check is the
//! signature rather than a line inside the body that a later edit could drop.
//!
//! # What an admin can do, and what they deliberately cannot
//!
//! Can: see who is on the server and what rooms exist, suspend and reinstate
//! accounts, remove somebody from every room at once, delete any room, manage
//! any room as though they were one of its admins, and read the storage
//! report — how much disk the attachments hold, in which rooms, moving at
//! what rate ([`storage`], [`list_files`]).
//!
//! Cannot: read a conversation they are not in. There is no endpoint here that
//! returns message content, and that is a design decision rather than an
//! omission. Half the rooms on a server like this are end-to-end encrypted and
//! could not be read even with a route for it; giving the other half a
//! side door would mean the privacy of a room depended on which checkbox was
//! ticked when it was made. An admin who needs to be in a room can be invited
//! into it, which is visible to everybody already there.

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use pocketskynet_core::{ServerEvent, Target, WalletAddress};
use serde::Deserialize;

use crate::auth::{AuthUser, ServerAdmin};
use crate::db::{admin, rooms};
use crate::error::{ApiError, ApiResult};
use crate::validate::{self, ValidJson};
use crate::AppState;

/// The ceiling on a listing. Not a page size — there is no paging — but the
/// point past which this stops being a team server and the console should not
/// try to render the answer in one screen.
const LIST_LIMIT: i64 = 2_000;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/session", get(session))
        .route("/admin/overview", get(overview))
        .route("/admin/users", get(list_users))
        .route(
            "/admin/users/{walletAddress}/suspend",
            post(suspend).delete(reinstate),
        )
        .route("/admin/users/{walletAddress}", delete(evict))
        .route("/admin/rooms", get(list_rooms))
        .route("/admin/rooms/{roomId}", delete(delete_room))
        .route("/admin/storage", get(storage))
        .route("/admin/files", get(list_files))
        .route("/admin/stats", get(stats))
}

/// `GET /api/admin/session` — whether *the caller* is an admin.
///
/// Takes [`AuthUser`], not [`ServerAdmin`], because the whole point is to be
/// answerable for somebody who is not one. A client restoring a stored session
/// has a token but not the login response that came with it, and needs to know
/// whether to offer the console at all; asking an endpoint that 403s would
/// make "you are not an admin" indistinguishable from "the server is down".
async fn session(
    State(_state): State<AppState>,
    AuthUser(caller): AuthUser,
) -> ApiResult<Response> {
    Ok(Json(serde_json::json!({
        "isServerAdmin": super::misc::is_server_admin(caller.as_str()),
    }))
    .into_response())
}

/// `GET /api/admin/overview` — totals, plus the configured admin list.
///
/// The admin list is echoed back so an operator can see what the server
/// actually parsed out of `VITE_FRUITNATION_ADMIN`. A mistyped address there
/// is silent by construction — the person it was meant for simply has no
/// powers — and this is the one place that can say so.
async fn overview(State(state): State<AppState>, _admin: ServerAdmin) -> ApiResult<Response> {
    let totals = state.db.call(|conn| admin::totals(conn)).await?;
    Ok(Json(serde_json::json!({
        "totals": totals,
        "admins": super::misc::server_admins(),
    }))
    .into_response())
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    limit: Option<String>,
}

fn limit_of(query: &ListQuery) -> i64 {
    query
        .limit
        .as_deref()
        .and_then(|raw| raw.trim().parse::<i64>().ok())
        .unwrap_or(LIST_LIMIT)
        .clamp(1, LIST_LIMIT)
}

/// `GET /api/admin/users` — every account, newest first.
async fn list_users(
    State(state): State<AppState>,
    _admin: ServerAdmin,
    Query(query): Query<ListQuery>,
) -> ApiResult<Response> {
    let limit = limit_of(&query);
    let mut users = state
        .db
        .call(move |conn| admin::list_users(conn, limit))
        .await?;
    // Stamped here rather than in SQL: the admin list is configuration, and
    // the database has never heard of it.
    for user in &mut users {
        user.is_server_admin = super::misc::is_server_admin(&user.wallet_address);
    }
    Ok(Json(users).into_response())
}

#[derive(Debug, Deserialize)]
struct SuspendBody {
    reason: Option<String>,
}

/// `POST /api/admin/users/{walletAddress}/suspend`.
///
/// Takes effect immediately for tokens already issued — that is the whole
/// value of it — because [`AuthUser`] consults the set this refreshes on every
/// request. Their live realtime connections are dropped for the same reason:
/// a socket opened before the suspension would otherwise keep delivering
/// events until it happened to reconnect.
async fn suspend(
    State(state): State<AppState>,
    admin: ServerAdmin,
    Path(wallet_address): Path<String>,
    ValidJson(body): ValidJson<SuspendBody>,
) -> ApiResult<Response> {
    let target = validate::wallet_address("walletAddress", Some(&wallet_address))?;
    if target.as_str().eq_ignore_ascii_case(admin.address()) {
        return Err(ApiError::bad_request(
            "You cannot suspend yourself. Remove your address from VITE_FRUITNATION_ADMIN instead.",
        ));
    }
    // An admin suspending another admin would be undone by the target simply
    // reinstating themselves, and the resulting fight is not a state this
    // server should be able to reach. The admin list is a config file; that
    // is where an admin is removed.
    if super::misc::is_server_admin(target.as_str()) {
        return Err(ApiError::bad_request(
            "That wallet is a server administrator. Remove it from VITE_FRUITNATION_ADMIN first.",
        ));
    }

    let reason = body
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(|r| r.chars().take(500).collect::<String>());

    let address = target.as_str().to_owned();
    let by = admin.address().to_owned();
    state
        .db
        .call(move |conn| admin::suspend(conn, &address, reason.as_deref(), &by))
        .await?;
    state.refresh_suspensions().await?;

    let _ = state.log.append_audit(
        "user_suspended",
        Some(&admin.0),
        serde_json::json!({ "walletAddress": target.as_str() }),
    );
    disconnect(&state, &target).await;

    Ok(super::message("Account suspended"))
}

/// `DELETE /api/admin/users/{walletAddress}/suspend` — lift a suspension.
async fn reinstate(
    State(state): State<AppState>,
    admin: ServerAdmin,
    Path(wallet_address): Path<String>,
) -> ApiResult<Response> {
    let target = validate::wallet_address("walletAddress", Some(&wallet_address))?;
    let address = target.as_str().to_owned();
    state
        .db
        .call(move |conn| admin::reinstate(conn, &address))
        .await?;
    state.refresh_suspensions().await?;

    let _ = state.log.append_audit(
        "user_reinstated",
        Some(&admin.0),
        serde_json::json!({ "walletAddress": target.as_str() }),
    );
    Ok(super::message("Account reinstated"))
}

/// `DELETE /api/admin/users/{walletAddress}` — remove somebody from the
/// server: out of every room, and suspended so they cannot walk back in.
///
/// Not a deletion of the person's history. Their messages stay where they are,
/// attributed to them, because a room's record of a conversation is not the
/// operator's to rewrite — and a year of unattributed text is worse for
/// everybody still in the room than a name they know has left. Purging
/// specific messages is the room's own `DELETE` endpoint.
async fn evict(
    State(state): State<AppState>,
    admin: ServerAdmin,
    Path(wallet_address): Path<String>,
) -> ApiResult<Response> {
    let target = validate::wallet_address("walletAddress", Some(&wallet_address))?;
    if target.as_str().eq_ignore_ascii_case(admin.address()) {
        return Err(ApiError::bad_request("You cannot remove yourself."));
    }
    if super::misc::is_server_admin(target.as_str()) {
        return Err(ApiError::bad_request(
            "That wallet is a server administrator. Remove it from VITE_FRUITNATION_ADMIN first.",
        ));
    }

    let address = target.as_str().to_owned();
    let by = admin.address().to_owned();
    let touched = state
        .db
        .call(move |conn| {
            let rooms = admin::evict_from_all_rooms(conn, &address)?;
            admin::suspend(conn, &address, Some("Removed from the server"), &by)?;
            Ok(rooms)
        })
        .await?;
    state.refresh_suspensions().await?;

    let _ = state.log.append_audit(
        "user_removed",
        Some(&admin.0),
        serde_json::json!({ "walletAddress": target.as_str(), "rooms": touched.len() }),
    );

    // Everyone left behind needs to know their rooms changed and that a
    // rotation is now pending; the removed wallet needs its socket closed.
    for room_id in &touched {
        if let Ok(room) = pocketskynet_core::RoomId::new(room_id) {
            state
                .hub
                .publish_best_effort(
                    Target::Room {
                        room_id: room.clone(),
                    },
                    None,
                    ServerEvent::RoomsUpdated,
                )
                .await;
        }
    }
    disconnect(&state, &target).await;

    Ok(super::message("Account removed from the server"))
}

/// `GET /api/admin/rooms` — every room, newest first. Metadata only.
async fn list_rooms(
    State(state): State<AppState>,
    _admin: ServerAdmin,
    Query(query): Query<ListQuery>,
) -> ApiResult<Response> {
    let limit = limit_of(&query);
    let out = state
        .db
        .call(move |conn| admin::list_rooms(conn, limit))
        .await?;
    Ok(Json(out).into_response())
}

/// `DELETE /api/admin/rooms/{roomId}` — delete any room on the server.
///
/// The room-level `DELETE /api/rooms/{id}` already lets a *room* admin do this
/// for a room they administer. This one exists for the case that endpoint
/// cannot reach: a room whose last admin has gone, which nobody remaining can
/// delete or rename, and which would otherwise be permanent.
///
/// It stops at the three built-in rooms, and an operator's reach is exactly why
/// the check has to be repeated here rather than left to the room-level guard.
/// This route exists to reach past a room's own admins; if it also reached past
/// the rule that a person's note cannot be destroyed, then "nobody else can
/// ever read this" would quietly mean "except whoever runs the server", which
/// is not what the room promises. Provisioning would recreate it empty on the
/// owner's next fetch, so the only thing the destroy could accomplish is
/// deleting somebody's notes.
async fn delete_room(
    State(state): State<AppState>,
    admin: ServerAdmin,
    Path(room_id): Path<String>,
) -> ApiResult<Response> {
    let room = validate::room_id(&room_id)?;
    super::rooms::refuse_static_removal(&state, &room, "destroy").await?;
    let room_id = room.as_str().to_owned();

    let members = state
        .db
        .call({
            let room_id = room_id.clone();
            move |conn| {
                if admin::room_name(conn, &room_id)?.is_none() {
                    return Err(ApiError::not_found("Room not found"));
                }
                let members = rooms::list_members(conn, &room_id)?;
                Ok(members
                    .into_iter()
                    .map(|m| m.user_address)
                    .collect::<Vec<_>>())
            }
        })
        .await?;

    // The same total destruction the room's own admins get: rows, and the
    // bytes underneath them. A room reachable only from here is exactly the
    // room nobody is left to clean up by hand.
    let purged = crate::purge::destroy_room(&state, &room_id, Some(&admin.0)).await?;

    let _ = state.log.append_audit(
        "room_deleted_by_admin",
        Some(&admin.0),
        serde_json::json!({ "roomId": room.as_str(), "purged": purged }),
    );

    for address in members {
        if let Ok(wallet) = WalletAddress::new(&address) {
            let _ = state.hub.refresh_user_rooms(&wallet).await;
            state
                .hub
                .publish_best_effort(Target::User { wallet }, None, ServerEvent::RoomsUpdated)
                .await;
        }
    }

    Ok(super::message("Room deleted"))
}

/// How many rooms the storage report ranks. Not paging — the point of the
/// card is "which rooms are heavy", and past a dozen the answer is "none in
/// particular".
const ROOM_USAGE_LIMIT: i64 = 12;

/// How many attachments the "largest" list names.
const LARGEST_LIMIT: i64 = 8;

/// How far back the growth series reaches. A month of daily buckets is enough
/// to see a trend and cheap enough to compute on every load.
const GROWTH_DAYS: i64 = 30;

/// `GET /api/admin/storage` — everything the files dashboard aggregates.
///
/// One response rather than five endpoints, because the dashboard renders them
/// as one screen and they are all answered by one trip into the database.
/// `activity` is the exception: it comes from `state.metrics`, the in-process
/// transfer counters (`metrics.rs`) — since server start, gone at restart,
/// exactly as presence treats its own truth.
///
/// Aggregates and metadata only. The bytes themselves stay behind the
/// membership check on `GET /api/files/{id}/raw` — a server admin gets no
/// exemption there, for the same reason no admin endpoint returns message
/// content: an attachment *is* part of a conversation, and this dashboard
/// must not become the side door the module docs above rule out.
async fn storage(State(state): State<AppState>, _admin: ServerAdmin) -> ApiResult<Response> {
    let (totals, categories, rooms, largest, growth) = state
        .db
        .call(|conn| {
            Ok((
                admin::storage_totals(conn)?,
                admin::category_breakdown(conn)?,
                admin::room_usage(conn, ROOM_USAGE_LIMIT)?,
                admin::largest_files(conn, LARGEST_LIMIT)?,
                admin::growth(conn, GROWTH_DAYS)?,
            ))
        })
        .await?;
    Ok(Json(serde_json::json!({
        "totals": totals,
        "categories": categories,
        "rooms": rooms,
        "largest": largest,
        "growth": growth,
        "activity": state.metrics.snapshot(),
    }))
    .into_response())
}

/// How many of the busiest rooms the stats report ranks — same reasoning as
/// [`ROOM_USAGE_LIMIT`], same number, kept separate because the two lists
/// answer different questions and need not move together.
const BUSIEST_LIMIT: i64 = 12;

/// How far back the message-activity series reaches, matching the file
/// growth window so the two charts share an x-axis a reader can compare.
const ACTIVITY_DAYS: i64 = 30;

/// `GET /api/admin/stats` — the whole deployment, in counts.
///
/// The Skynet Dashboard's server half: rooms by kind and by encryption,
/// accounts by standing, message volume in total and per day, the loudest
/// rooms, who is connected right now, and how long this process has been up.
///
/// Every number is an aggregate. The busiest-rooms list carries names, sizes
/// and counts — the same fields `/admin/rooms` already shows — and nothing
/// here reads `messages.content` or ever could: the queries live in
/// `db/admin.rs` and select counts. Presence is two integers, not a roster:
/// *who* is online is knowledge scoped to shared rooms (§6.15), and a server
/// admin gets no exemption from that — a head-count is the whole of what
/// this reports.
async fn stats(State(state): State<AppState>, _admin: ServerAdmin) -> ApiResult<Response> {
    let (rooms, people, messages, activity, busiest) = state
        .db
        .call(|conn| {
            Ok((
                admin::room_composition(conn)?,
                admin::people_stats(conn)?,
                admin::message_stats(conn)?,
                admin::message_activity(conn, ACTIVITY_DAYS)?,
                admin::busiest_rooms(conn, BUSIEST_LIMIT)?,
            ))
        })
        .await?;

    // Derived from the hub's live connection registry, exactly as presence
    // is (ROADMAP.md §0a): nothing stored, nothing surviving a restart.
    let mut online = 0i64;
    let mut away = 0i64;
    for (_, status) in state.hub.present_wallets() {
        match status {
            pocketskynet_core::PresenceStatus::Online => online += 1,
            pocketskynet_core::PresenceStatus::Away => away += 1,
            pocketskynet_core::PresenceStatus::Offline => {}
        }
    }

    Ok(Json(serde_json::json!({
        "uptimeSeconds": state.started.elapsed().as_secs(),
        "presence": { "online": online, "away": away },
        "rooms": rooms,
        "people": people,
        "messages": messages,
        "activity": activity,
        "busiest": busiest,
    }))
    .into_response())
}

/// `GET /api/admin/files` — every attachment's metadata, newest first.
///
/// Sortable and filterable client-side; the cap is the same scale decision as
/// `/admin/users`. Note what a row does *not* carry: no download URL, no
/// stored name, no caption. An operator sees that a file exists and how big it
/// is; reading it still requires being in its room.
async fn list_files(
    State(state): State<AppState>,
    _admin: ServerAdmin,
    Query(query): Query<ListQuery>,
) -> ApiResult<Response> {
    let limit = limit_of(&query);
    let out = state
        .db
        .call(move |conn| admin::list_files(conn, limit))
        .await?;
    Ok(Json(out).into_response())
}

/// Tell a wallet's live connections that their credential is no longer good.
///
/// Best-effort by design: the authoritative check is at the extractor, on the
/// next request. This only saves a suspended client from sitting on an open
/// stream believing everything is fine until something makes it reconnect.
async fn disconnect(state: &AppState, wallet: &WalletAddress) {
    state
        .hub
        .publish_best_effort(
            Target::User {
                wallet: wallet.clone(),
            },
            None,
            ServerEvent::SessionExpired {
                reason: "Account suspended".to_owned(),
            },
        )
        .await;
}

#[cfg(test)]
mod tests {
    use crate::routes::build;
    use crate::test_support::{arm_server_admin, boss, register, send, state, wallet};
    use axum::http::StatusCode;
    use sha2::Digest;

    #[tokio::test]
    async fn the_admin_routes_refuse_everybody_else() {
        arm_server_admin();
        let state = state("admin-gate");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        for path in [
            "/api/admin/overview",
            "/api/admin/users",
            "/api/admin/rooms",
            "/api/admin/storage",
            "/api/admin/files",
            "/api/admin/stats",
        ] {
            let response = send(&router, "GET", path, Some(&token), None).await;
            assert_eq!(
                response.status,
                StatusCode::FORBIDDEN,
                "{path} must not answer a non-admin"
            );
        }

        // But every signed-in caller may ask whether *they* are one — a client
        // has to know whether to offer the console at all.
        let session = send(&router, "GET", "/api/admin/session", Some(&token), None).await;
        assert_eq!(session.status, StatusCode::OK);
        assert_eq!(session.json()["isServerAdmin"], false);
    }

    #[tokio::test]
    async fn an_admin_sees_the_server_and_is_named_as_one() {
        arm_server_admin();
        let state = state("admin-overview");
        let boss = boss();
        let alice = wallet("alice");
        let boss_token = register(&state, &boss, "boss");
        let alice_token = register(&state, &alice, "alice");
        let router = build(state);

        send(
            &router,
            "POST",
            "/api/rooms",
            Some(&alice_token),
            Some(serde_json::json!({ "name": "Engineering" })),
        )
        .await;

        let session = send(
            &router,
            "GET",
            "/api/admin/session",
            Some(&boss_token),
            None,
        )
        .await;
        assert_eq!(session.json()["isServerAdmin"], true);

        let overview = send(
            &router,
            "GET",
            "/api/admin/overview",
            Some(&boss_token),
            None,
        )
        .await;
        assert_eq!(overview.status, StatusCode::OK);
        assert_eq!(overview.json()["totals"]["users"], 2);
        assert_eq!(overview.json()["totals"]["channels"], 1);
        // Echoed back so an operator can see what the server parsed out of
        // VITE_FRUITNATION_ADMIN — a mistyped address there is otherwise
        // completely silent.
        assert_eq!(overview.json()["admins"][0], boss.as_str().to_lowercase());

        let users = send(&router, "GET", "/api/admin/users", Some(&boss_token), None).await;
        let users = users.json();
        let users = users.as_array().unwrap();
        let listed_boss = users
            .iter()
            .find(|u| u["walletAddress"] == boss.as_str())
            .unwrap();
        assert_eq!(listed_boss["isServerAdmin"], true);
        let listed_alice = users
            .iter()
            .find(|u| u["walletAddress"] == alice.as_str())
            .unwrap();
        assert_eq!(listed_alice["isServerAdmin"], false);
        assert_eq!(listed_alice["roomCount"], 1);

        let rooms = send(&router, "GET", "/api/admin/rooms", Some(&boss_token), None).await;
        assert_eq!(rooms.json()[0]["name"], "Engineering");
        assert_eq!(rooms.json()[0]["memberCount"], 1);
    }

    #[tokio::test]
    async fn suspending_invalidates_a_token_already_issued() {
        arm_server_admin();
        let state = state("admin-suspend");
        let boss_token = register(&state, &boss(), "boss");
        let alice = wallet("alice");
        let alice_token = register(&state, &alice, "alice");
        let router = build(state);

        // Alice's token works before.
        assert_eq!(
            send(&router, "GET", "/api/rooms", Some(&alice_token), None)
                .await
                .status,
            StatusCode::OK
        );

        let suspended = send(
            &router,
            "POST",
            &format!("/api/admin/users/{}/suspend", alice.as_str()),
            Some(&boss_token),
            Some(serde_json::json!({ "reason": "posting from a compromised laptop" })),
        )
        .await;
        assert_eq!(suspended.status, StatusCode::OK, "{:?}", suspended.body);

        // The same token, unchanged, now fails — which is the whole point:
        // there is no revocation list, so the decision is remade per request.
        let after = send(&router, "GET", "/api/rooms", Some(&alice_token), None).await;
        assert_eq!(after.status, StatusCode::UNAUTHORIZED);

        let listed = send(&router, "GET", "/api/admin/users", Some(&boss_token), None).await;
        let listed = listed.json();
        let alice_row = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["walletAddress"] == alice.as_str())
            .unwrap()
            .clone();
        assert_eq!(alice_row["isSuspended"], true);
        assert_eq!(
            alice_row["suspendedReason"],
            "posting from a compromised laptop"
        );

        // Reinstating puts it back.
        let reinstated = send(
            &router,
            "DELETE",
            &format!("/api/admin/users/{}/suspend", alice.as_str()),
            Some(&boss_token),
            None,
        )
        .await;
        assert_eq!(reinstated.status, StatusCode::OK);
        assert_eq!(
            send(&router, "GET", "/api/rooms", Some(&alice_token), None)
                .await
                .status,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn suspension_closes_the_realtime_door_too() {
        // The regression this pins down: `AuthUser` refused suspended accounts
        // on every REST request, but the stream credentials (`StreamAuth`)
        // verified the JWT directly — so a suspended user could reconnect over
        // SSE or WebSocket and keep receiving room activity until their token
        // expired. The extractor now routes through the same deny set.
        arm_server_admin();
        let state = state("admin-suspend-sse");
        let boss_token = register(&state, &boss(), "boss");
        let alice = wallet("alice");
        let alice_token = register(&state, &alice, "alice");
        let router = build(state);

        send(
            &router,
            "POST",
            &format!("/api/admin/users/{}/suspend", alice.as_str()),
            Some(&boss_token),
            Some(serde_json::json!({})),
        )
        .await;

        // The same bearer token the REST paths now refuse must be refused by
        // the stream handshake as well — this returns immediately with 401
        // rather than opening a stream.
        let refused = send(&router, "GET", "/api/events", Some(&alice_token), None).await;
        assert_eq!(
            refused.status,
            StatusCode::UNAUTHORIZED,
            "{:?}",
            refused.body
        );

        // And a ticket cannot be minted either (AuthUser gates minting).
        let ticket = send(
            &router,
            "POST",
            "/api/events/ticket",
            Some(&alice_token),
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(ticket.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn an_admin_cannot_suspend_themselves_or_another_admin() {
        arm_server_admin();
        let state = state("admin-selfharm");
        let boss = boss();
        let boss_token = register(&state, &boss, "boss");
        let router = build(state);

        let response = send(
            &router,
            "POST",
            &format!("/api/admin/users/{}/suspend", boss.as_str()),
            Some(&boss_token),
            Some(serde_json::json!({})),
        )
        .await;
        // Locking the only administrator out of their own server is not a
        // state a request should be able to reach; the admin list is a config
        // file, and that is where an admin is removed.
        assert_eq!(response.status, StatusCode::BAD_REQUEST);

        let response = send(
            &router,
            "DELETE",
            &format!("/api/admin/users/{}", boss.as_str()),
            Some(&boss_token),
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn removing_someone_evicts_them_and_flags_a_rekey() {
        arm_server_admin();
        let state = state("admin-evict");
        let boss_token = register(&state, &boss(), "boss");
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

        let removed = send(
            &router,
            "DELETE",
            &format!("/api/admin/users/{}", bob.as_str()),
            Some(&boss_token),
            None,
        )
        .await;
        assert_eq!(removed.status, StatusCode::OK, "{:?}", removed.body);

        // Bob is out of the room and out of the server.
        assert_eq!(
            send(&router, "GET", "/api/rooms", Some(&bob_token), None)
                .await
                .status,
            StatusCode::UNAUTHORIZED
        );
        let alices = send(&router, "GET", "/api/rooms", Some(&alice_token), None).await;
        assert_eq!(alices.json()[0]["id"], room);
        assert_eq!(alices.json()[0]["memberCount"], 1);
        // He may still hold the room key, so nothing may be sealed under it
        // until Alice rotates — the same guarantee leaving gives.
        assert_eq!(alices.json()[0]["keyRotationPending"], true);
    }

    #[tokio::test]
    async fn an_admin_can_manage_and_delete_a_room_they_were_never_in() {
        arm_server_admin();
        let state = state("admin-rooms");
        let boss_token = register(&state, &boss(), "boss");
        let alice = wallet("alice");
        let alice_token = register(&state, &alice, "alice");
        let router = build(state);

        let room = send(
            &router,
            "POST",
            "/api/rooms",
            Some(&alice_token),
            Some(serde_json::json!({ "name": "Abandoned" })),
        )
        .await
        .json()["id"]
            .as_str()
            .unwrap()
            .to_owned();

        // Room admin powers, without being a member — this is what makes a
        // room whose last admin left recoverable rather than permanent.
        let renamed = send(
            &router,
            "PATCH",
            &format!("/api/rooms/{room}"),
            Some(&boss_token),
            Some(serde_json::json!({ "name": "Reclaimed" })),
        )
        .await;
        assert_eq!(renamed.status, StatusCode::OK, "{:?}", renamed.body);

        let deleted = send(
            &router,
            "DELETE",
            &format!("/api/admin/rooms/{room}"),
            Some(&boss_token),
            None,
        )
        .await;
        assert_eq!(deleted.status, StatusCode::OK, "{:?}", deleted.body);
        assert!(send(&router, "GET", "/api/rooms", Some(&alice_token), None)
            .await
            .json()
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn the_stats_report_counts_the_deployment_without_reading_it() {
        arm_server_admin();
        let state = state("admin-stats");
        let boss_token = register(&state, &boss(), "boss");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state);

        let room = send(
            &router,
            "POST",
            "/api/rooms",
            Some(&alice_token),
            Some(serde_json::json!({ "name": "Engineering" })),
        )
        .await
        .json()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        send(
            &router,
            "POST",
            "/api/rooms/dm",
            Some(&alice_token),
            Some(serde_json::json!({ "walletAddress": bob.as_str() })),
        )
        .await;
        // A real message, hashed the way the protocol demands (§13: plain
        // SHA-256 of the trimmed plaintext).
        let content = "the plans are in the vault";
        let msg_hash = hex::encode(sha2::Sha256::digest(content.as_bytes()));
        let posted = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/messages"),
            Some(&alice_token),
            Some(serde_json::json!({ "content": content, "msgHash": msg_hash })),
        )
        .await;
        assert!(posted.status.is_success(), "{:?}", posted.body);

        let refused = send(&router, "GET", "/api/admin/stats", Some(&bob_token), None).await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);

        let stats = send(&router, "GET", "/api/admin/stats", Some(&boss_token), None).await;
        assert_eq!(stats.status, StatusCode::OK, "{:?}", stats.body);
        let report = stats.json();
        assert_eq!(report["rooms"]["total"], 2);
        assert_eq!(report["rooms"]["channels"], 1);
        assert_eq!(report["rooms"]["directMessages"], 1);
        assert_eq!(report["people"]["total"], 3);
        assert_eq!(report["messages"]["total"], 1);
        assert_eq!(report["activity"].as_array().unwrap().len(), 1);
        assert_eq!(report["busiest"][0]["name"], "Engineering");
        assert_eq!(report["busiest"][0]["messages"], 1);
        assert!(report["uptimeSeconds"].is_number());
        assert!(report["presence"]["online"].is_number());

        // The privacy line, stated as a property of the bytes on the wire:
        // a message was posted, and no admin surface ever echoes it.
        let body = serde_json::to_string(&report).unwrap();
        assert!(
            !body.contains("vault"),
            "the stats report must never carry message content"
        );
    }

    #[tokio::test]
    async fn the_storage_report_sees_the_shelf_but_never_the_books() {
        arm_server_admin();
        let state = state("admin-storage");
        let boss_token = register(&state, &boss(), "boss");
        let alice = wallet("alice");
        let alice_token = register(&state, &alice, "alice");
        let router = build(state);

        let room = send(
            &router,
            "POST",
            "/api/rooms",
            Some(&alice_token),
            Some(serde_json::json!({ "name": "Design" })),
        )
        .await
        .json()["id"]
            .as_str()
            .unwrap()
            .to_owned();

        let uploaded = crate::test_support::send_raw(
            &router,
            "POST",
            &format!("/api/rooms/{room}/files?filename=report.pdf"),
            Some(&alice_token),
            b"nine byte".to_vec(),
            "application/octet-stream",
        )
        .await;
        assert_eq!(uploaded.status, StatusCode::CREATED, "{:?}", uploaded.body);
        let file_id = uploaded.json()["id"].as_str().unwrap().to_owned();

        // The uploader downloading their own file is the traffic the activity
        // card should see.
        let raw = send(
            &router,
            "GET",
            &format!("/api/files/{file_id}/raw"),
            Some(&alice_token),
            None,
        )
        .await;
        assert_eq!(raw.status, StatusCode::OK);

        let storage = send(
            &router,
            "GET",
            "/api/admin/storage",
            Some(&boss_token),
            None,
        )
        .await;
        assert_eq!(storage.status, StatusCode::OK, "{:?}", storage.body);
        let report = storage.json();
        assert_eq!(report["totals"]["files"], 1);
        assert_eq!(report["totals"]["blobs"], 1);
        assert_eq!(report["totals"]["diskBytes"], 9);
        assert_eq!(report["rooms"][0]["name"], "Design");
        assert_eq!(report["largest"][0]["filename"], "report.pdf");
        let documents = report["categories"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["category"] == "document")
            .unwrap()
            .clone();
        assert_eq!(documents["files"], 1);
        assert_eq!(report["growth"].as_array().unwrap().len(), 1);
        // The in-process counters saw the single-shot upload land and the
        // download stream to its end.
        assert_eq!(report["activity"]["uploads"]["transfers"], 1);
        assert_eq!(report["activity"]["downloads"]["transfers"], 1);
        assert_eq!(report["activity"]["downloads"]["bytes"], 9);

        // The listing is metadata: names, sizes, places — no URL, no stored
        // name, nothing that fetches bytes.
        let files = send(&router, "GET", "/api/admin/files", Some(&boss_token), None).await;
        let row = files.json()[0].clone();
        assert_eq!(row["filename"], "report.pdf");
        assert_eq!(row["roomName"], "Design");
        assert_eq!(row["uploaderName"], "alice");
        assert_eq!(row["category"], "document");
        assert!(row.get("url").is_none());
        assert!(row.get("storedName").is_none());

        // And the stance the whole dashboard rests on: the admin, not being a
        // member, cannot read the file itself — a uniform 404, exactly what a
        // stranger gets. Metadata is the ceiling.
        let refused = send(
            &router,
            "GET",
            &format!("/api/files/{file_id}/raw"),
            Some(&boss_token),
            None,
        )
        .await;
        assert_eq!(refused.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn purging_a_rooms_history_is_admin_only() {
        arm_server_admin();
        let state = state("admin-purge");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let boss_token = register(&state, &boss(), "boss");
        let router = build(state);

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
            Some(serde_json::json!({})),
        )
        .await;

        // A member erasing everybody's history with one request was the gap
        // the roadmap named. Deleting one message is still open to him.
        let refused = send(
            &router,
            "DELETE",
            &format!("/api/rooms/{room}/messages"),
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);

        // The room's own admin may.
        let allowed = send(
            &router,
            "DELETE",
            &format!("/api/rooms/{room}/messages"),
            Some(&alice_token),
            None,
        )
        .await;
        assert_eq!(allowed.status, StatusCode::OK, "{:?}", allowed.body);

        // And so may a server admin, who is not even a member.
        let by_boss = send(
            &router,
            "DELETE",
            &format!("/api/rooms/{room}/messages"),
            Some(&boss_token),
            None,
        )
        .await;
        assert_eq!(by_boss.status, StatusCode::OK, "{:?}", by_boss.body);
    }
}
