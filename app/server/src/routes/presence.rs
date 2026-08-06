//! Who is at their desk (`docs/API.md` §6.15).
//!
//! Presence is derived, never stored. The truth is the set of live connections
//! the hub is already holding plus how recently each one showed a sign of life,
//! so there is no table to migrate, nothing to backfill, and nothing that
//! outlives the process — which is the point. A durable presence record is a
//! log of when each person was at their computer, and this feature does not
//! need one to answer the only question anyone asks of it: *is she there right
//! now?*
//!
//! Two endpoints, and the asymmetry between them is deliberate.
//!
//! `GET` is the authority. Events are the fast path and they are transient by
//! design (`ServerEvent::is_replayable`), so a client that was disconnected for
//! a minute has a hole rather than a stale value, and the way to fill a hole is
//! to ask. Every client calls this when a stream comes up.
//!
//! `PUT` exists because one thing the server genuinely cannot observe is a tab
//! going to the background. Over WebSocket a client says so with a
//! `presence` frame; the SSE and polling tiers have no upstream channel, so
//! they say it here — and repeat it, which is what keeps an otherwise silent
//! SSE stream from ageing into a false *away*.

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use pocketskynet_core::PresenceStatus;
use serde::{Deserialize, Serialize};

use crate::auth::AuthUser;
use crate::db::users;
use crate::error::{ApiError, ApiResult};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/presence", get(snapshot))
        .route("/presence", put(declare))
}

/// One person's current status.
#[derive(Debug, Serialize)]
struct Entry {
    #[serde(rename = "walletAddress")]
    wallet_address: String,
    status: &'static str,
}

/// A string rather than a `PresenceStatus`, so an unknown value lands in this
/// module's own 400 with a sentence naming the two it accepts, instead of
/// serde's 422 "unknown variant" — which reads as a server fault for what is
/// squarely a client one.
#[derive(Debug, Deserialize)]
struct DeclareBody {
    status: String,
}

/// `GET /api/presence` — everyone the caller shares a room with who is not
/// offline.
///
/// Offline is the absence of an entry rather than an entry saying "offline".
/// It is the overwhelmingly common state, it is the client's default for
/// anybody it has never heard about, and enumerating it would turn a response
/// that is normally a handful of rows into one proportional to the size of
/// every room the caller is in.
///
/// The caller's own status is included. A client showing "you're away" needs to
/// know what the server thinks it is, and the answer is the only way to notice
/// that a declaration was lost.
async fn snapshot(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
) -> ApiResult<Response> {
    let address = caller.as_str().to_owned();
    let blocked = address.clone();
    let (peers, blocks) = state
        .db
        .call(move |conn| {
            Ok((
                users::room_peers(conn, &address)?,
                users::mutual_block_set(conn, &blocked)?,
            ))
        })
        .await?;

    // Walking the present set rather than the peer set: the former is bounded
    // by how many people are actually connected to this server, the latter by
    // how many people the caller shares a room with, and only the first has a
    // ceiling that does not grow with the size of the deployment.
    let mut out: Vec<Entry> = state
        .hub
        .present_wallets()
        .into_iter()
        .filter(|(wallet, _)| {
            let addr = wallet.as_str();
            // Yourself always; a peer unless a block runs either way. The
            // bidirectional test is what stops presence being an oracle for
            // "did they block me?" — see `mutual_block_set`.
            (addr == caller.as_str() || peers.contains(addr)) && !blocks.contains(addr)
        })
        .map(|(wallet, status)| Entry {
            wallet_address: wallet.as_str().to_owned(),
            status: status.as_str(),
        })
        .collect();

    // Stable order, so a client diffing two snapshots sees the changes rather
    // than a reshuffle of a hash map's iteration order.
    out.sort_by(|a, b| a.wallet_address.cmp(&b.wallet_address));
    Ok(Json(out).into_response())
}

/// `PUT /api/presence` — "my tab went to the background", or "it came back".
///
/// Also the heartbeat for the tiers that cannot send one any other way, which
/// is why a repeat of the status the caller already has is a success rather
/// than a no-op: the timestamp is the payload.
async fn declare(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Json(body): Json<DeclareBody>,
) -> ApiResult<Response> {
    let status = match body.status.as_str() {
        "online" => PresenceStatus::Online,
        "away" => PresenceStatus::Away,
        // Answered separately, because a client that sent this is not making a
        // typo: it is asking to look absent while plainly connected. Telling it
        // the value exists but is derived is the useful reply.
        "offline" => {
            return Err(ApiError::bad_request(
                "\"offline\" is derived from holding no connection; it cannot be declared",
            ))
        }
        _ => {
            return Err(ApiError::bad_request(
                "status must be \"online\" or \"away\"",
            ))
        }
    };

    state.hub.beacon(&caller, status).await;

    let now = crate::db::now_ms();
    Ok(Json(serde_json::json!({
        "status": state.hub.derive_presence(&caller, now).as_str(),
    }))
    .into_response())
}

#[cfg(test)]
mod tests {
    use crate::routes::build;
    use crate::test_support::{register, send, state, wallet};
    use axum::http::StatusCode;
    use pocketskynet_core::PresenceStatus;

    /// Nobody is connected in a router-driven test, so the snapshot is empty —
    /// which is the honest answer and the one a fresh client starts from.
    #[tokio::test]
    async fn an_unconnected_server_reports_nobody_present() {
        let state = state("presence-empty");
        let token = register(&state, &wallet("alice"), "alice");
        let router = build(state);

        let response = send(&router, "GET", "/api/presence", Some(&token), None).await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.json().as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn presence_needs_a_credential() {
        let router = build(state("presence-auth"));
        let response = send(&router, "GET", "/api/presence", None, None).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_beacon_puts_the_caller_in_their_own_snapshot() {
        let state = state("presence-beacon");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state.clone());

        let declared = send(
            &router,
            "PUT",
            "/api/presence",
            Some(&token),
            Some(serde_json::json!({ "status": "away" })),
        )
        .await;
        assert_eq!(declared.status, StatusCode::OK);
        assert_eq!(declared.json()["status"], "away");

        // A polling client holds no connection at all, so the beacon is the
        // only thing standing between it and being reported absent.
        assert_eq!(
            state.hub.announced_presence(&alice),
            PresenceStatus::Away,
            "a beacon is the polling tier's only way to be present"
        );

        let snapshot = send(&router, "GET", "/api/presence", Some(&token), None).await;
        let rows = snapshot.json();
        let rows = rows.as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["walletAddress"], alice.as_str());
        assert_eq!(rows[0]["status"], "away");
    }

    #[tokio::test]
    async fn a_client_may_not_declare_itself_offline() {
        let state = state("presence-offline");
        let token = register(&state, &wallet("alice"), "alice");
        let router = build(state);

        let refused = send(
            &router,
            "PUT",
            "/api/presence",
            Some(&token),
            Some(serde_json::json!({ "status": "offline" })),
        )
        .await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);

        let nonsense = send(
            &router,
            "PUT",
            "/api/presence",
            Some(&token),
            Some(serde_json::json!({ "status": "in a meeting" })),
        )
        .await;
        assert_eq!(nonsense.status, StatusCode::BAD_REQUEST);
    }

    /// The visibility rule. Presence is not a directory: a stranger's status is
    /// none of your business, and a shared room is what makes it yours.
    #[tokio::test]
    async fn presence_reaches_only_people_you_share_a_room_with() {
        let state = state("presence-strangers");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state.clone());

        for token in [&alice_token, &bob_token] {
            send(
                &router,
                "PUT",
                "/api/presence",
                Some(token),
                Some(serde_json::json!({ "status": "online" })),
            )
            .await;
        }

        // No shared room yet: each sees only themselves.
        let alone = send(&router, "GET", "/api/presence", Some(&alice_token), None).await;
        let alone = alone.json();
        assert_eq!(alone.as_array().unwrap().len(), 1);
        assert_eq!(alone[0]["walletAddress"], alice.as_str());

        // A DM is the shortest way to share one.
        let dm = send(
            &router,
            "POST",
            "/api/rooms/dm",
            Some(&alice_token),
            Some(serde_json::json!({ "walletAddress": bob.as_str() })),
        )
        .await;
        assert_eq!(dm.status, StatusCode::OK);

        let together = send(&router, "GET", "/api/presence", Some(&alice_token), None).await;
        let together = together.json();
        let addresses: Vec<&str> = together
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["walletAddress"].as_str().unwrap())
            .collect();
        assert!(addresses.contains(&bob.as_str()));
        assert!(addresses.contains(&alice.as_str()));
    }

    /// Presence is an activity oracle, so it must not cross a block in either
    /// direction — the same rule typing already follows.
    #[tokio::test]
    async fn a_block_hides_presence_both_ways() {
        let state = state("presence-blocks");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state.clone());

        send(
            &router,
            "POST",
            "/api/rooms/dm",
            Some(&alice_token),
            Some(serde_json::json!({ "walletAddress": bob.as_str() })),
        )
        .await;
        for token in [&alice_token, &bob_token] {
            send(
                &router,
                "PUT",
                "/api/presence",
                Some(token),
                Some(serde_json::json!({ "status": "online" })),
            )
            .await;
        }

        // Alice blocks Bob. Neither should now see the other.
        let blocked = send(
            &router,
            "POST",
            "/api/users/block",
            Some(&alice_token),
            Some(serde_json::json!({ "address": bob.as_str() })),
        )
        .await;
        assert_eq!(blocked.status, StatusCode::OK);

        for (token, other) in [(&alice_token, &bob), (&bob_token, &alice)] {
            let seen = send(&router, "GET", "/api/presence", Some(token), None).await;
            let seen = seen.json();
            let addresses: Vec<&str> = seen
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["walletAddress"].as_str().unwrap())
                .collect();
            assert!(
                !addresses.contains(&other.as_str()),
                "a block must hide presence in both directions, not just the blocker's"
            );
        }
    }
}
