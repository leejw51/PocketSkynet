//! User lookup, key distribution, and blocking (`docs/API.md` §6.3, §6.4).

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::db::users;
use crate::error::{ApiError, ApiResult};
use crate::validate::{self, ValidJson, SEARCH_LIMIT};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        // Static segments are registered first for readability; axum's
        // matcher prefers them over `{address}` regardless of order.
        .route("/users/search", get(search))
        .route("/users/blocked", get(blocked))
        .route("/users/blocked-by", get(blocked_by))
        .route("/users/block", post(block))
        .route("/users/block/{address}", delete(unblock))
        .route("/users/public-keys", post(public_keys))
        .route("/users/{address}", get(get_by_address))
        .route("/users/{address}/is-blocked", get(is_blocked))
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    q: Option<String>,
}

/// `GET /api/users/search?q=` — substring match, blocks hidden both ways.
async fn search(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Query(query): Query<SearchQuery>,
) -> ApiResult<Response> {
    let q = validate::search_query(query.q.as_deref())?;
    let address = caller.as_str().to_owned();

    let found = state
        .db
        .call(move |conn| users::search_users(conn, &address, &q, SEARCH_LIMIT))
        .await?;
    Ok(Json(found).into_response())
}

/// `GET /api/users/{address}` — any authenticated caller may read any profile.
///
/// Blocking deliberately does **not** apply here. A block hides content and
/// presence, not the existence of an account: a client still has to render the
/// name of someone it blocked in a shared room's roster.
async fn get_by_address(
    State(state): State<AppState>,
    AuthUser(_caller): AuthUser,
    Path(address): Path<String>,
) -> ApiResult<Response> {
    let target = validate::wallet_address("address", Some(&address))?;
    let address = target.as_str().to_owned();

    let user = state
        .db
        .call(move |conn| users::get_user(conn, &address))
        .await?
        .ok_or_else(|| ApiError::not_found("User not found"))?;
    Ok(Json(user).into_response())
}

#[derive(Debug, Deserialize)]
struct PublicKeysBody {
    addresses: Option<Vec<String>>,
}

/// `POST /api/users/public-keys` — resolve encryption keys in bulk.
///
/// §15 #5: the reference's handler had no validation branch, so a malformed
/// request came back as a 500. It is a 400 here; nothing depended on the 500.
async fn public_keys(
    State(state): State<AppState>,
    AuthUser(_caller): AuthUser,
    ValidJson(body): ValidJson<PublicKeysBody>,
) -> ApiResult<Response> {
    let raw = body
        .addresses
        .ok_or_else(|| validate::required("addresses", "Addresses"))?;
    if raw.is_empty() || raw.len() > 50 {
        return Err(ApiError::field(
            "addresses",
            "Addresses must contain between 1 and 50 entries",
        ));
    }

    let mut addresses = Vec::with_capacity(raw.len());
    for entry in &raw {
        addresses.push(
            validate::wallet_address("addresses", Some(entry))?
                .as_str()
                .to_owned(),
        );
    }

    let entries = state
        .db
        .call(move |conn| users::get_public_keys(conn, &addresses))
        .await?;
    Ok(Json(entries).into_response())
}

/// `GET /api/users/blocked` — addresses the caller has blocked.
async fn blocked(State(state): State<AppState>, AuthUser(caller): AuthUser) -> ApiResult<Response> {
    let address = caller.as_str().to_owned();
    let rows = state
        .db
        .call(move |conn| users::list_blocked(conn, &address))
        .await?;
    Ok(Json(rows).into_response())
}

/// `GET /api/users/blocked-by` — who has blocked the caller.
///
/// This is observable by the blocked party by design: native clients need it
/// to apply the same bidirectional filtering the web client does, and the
/// alternative is every client showing a half-broken conversation.
async fn blocked_by(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
) -> ApiResult<Response> {
    let address = caller.as_str().to_owned();
    let rows = state
        .db
        .call(move |conn| users::list_blocked_by(conn, &address))
        .await?;
    Ok(Json(rows).into_response())
}

#[derive(Debug, Deserialize)]
struct BlockBody {
    address: Option<String>,
}

/// `POST /api/users/block`.
async fn block(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    ValidJson(body): ValidJson<BlockBody>,
) -> ApiResult<Response> {
    let raw = body
        .address
        .as_deref()
        .ok_or_else(|| ApiError::bad_request("Wallet address is required"))?;
    // A plain message, not the validation envelope: the reference used
    // `safeParse` here and clients display this string directly.
    let target = pocketskynet_core::WalletAddress::new(raw)
        .map_err(|_| ApiError::bad_request("Invalid wallet address format"))?;

    if target == caller {
        return Err(ApiError::bad_request("Cannot block yourself"));
    }
    // A webhook or an agent is not somebody there is anything to block: it
    // holds no key, signs nothing, and only ever speaks in a room its owner
    // already controls.
    if target.is_reserved() {
        return Err(ApiError::bad_request("That is not a person you can block."));
    }

    let blocker = caller.as_str().to_owned();
    let blocked = target.as_str().to_owned();
    let row = state
        .db
        .call(move |conn| {
            if !users::user_exists(conn, &blocked)? {
                return Err(ApiError::not_found("User not found"));
            }
            users::block_user(conn, &blocker, &blocked)
        })
        .await?;

    // Both parties' live connections are refreshed so the block takes effect
    // on open sockets immediately — otherwise a typing indicator would keep
    // crossing the block until one of them reconnected.
    refresh_both(&state, &caller, &target).await;

    Ok(Json(row).into_response())
}

/// `DELETE /api/users/block/{address}` — idempotent and permissive.
///
/// Unblocking somebody who was never blocked succeeds: there is nothing the
/// caller could do about a 404, and answering one would leak whether a block
/// exists.
async fn unblock(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(address): Path<String>,
) -> ApiResult<Response> {
    let target = pocketskynet_core::WalletAddress::new(&address)
        .map_err(|_| ApiError::bad_request("Invalid wallet address format"))?;

    let blocker = caller.as_str().to_owned();
    let blocked = target.as_str().to_owned();
    state
        .db
        .call(move |conn| users::unblock_user(conn, &blocker, &blocked))
        .await?;

    refresh_both(&state, &caller, &target).await;
    Ok(super::message("User unblocked successfully"))
}

/// `GET /api/users/{address}/is-blocked` — "have **I** blocked them?".
///
/// Directed, not symmetric: `GET /api/users/blocked-by` answers the other
/// direction.
async fn is_blocked(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(address): Path<String>,
) -> ApiResult<Response> {
    let target = pocketskynet_core::WalletAddress::new(&address)
        .map_err(|_| ApiError::bad_request("Invalid wallet address format"))?;

    let blocker = caller.as_str().to_owned();
    let blocked = target.as_str().to_owned();
    let answer = state
        .db
        .call(move |conn| users::is_blocked(conn, &blocker, &blocked))
        .await?;
    Ok(Json(serde_json::json!({ "isBlocked": answer })).into_response())
}

async fn refresh_both(
    state: &AppState,
    a: &pocketskynet_core::WalletAddress,
    b: &pocketskynet_core::WalletAddress,
) {
    for wallet in [a, b] {
        if let Err(e) = state.hub.refresh_user_blocks(wallet).await {
            tracing::warn!(error = %e, "could not refresh live block sets");
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::routes::build;
    use crate::test_support::{register, send, state, wallet};
    use axum::http::StatusCode;

    #[tokio::test]
    async fn search_requires_a_query_parameter() {
        let state = state("search-required");
        let token = register(&state, &wallet("alice"), "alice");
        let router = build(state);

        let response = send(&router, "GET", "/api/users/search", Some(&token), None).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert_eq!(response.json()["message"], "Validation failed");
    }

    #[tokio::test]
    async fn search_hides_both_directions_of_a_block() {
        let state = state("search-blocks");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state);

        send(
            &router,
            "POST",
            "/api/users/block",
            Some(&alice_token),
            Some(serde_json::json!({ "address": bob.as_str() })),
        )
        .await;

        let alice_results = send(
            &router,
            "GET",
            "/api/users/search?q=b",
            Some(&alice_token),
            None,
        )
        .await;
        assert!(alice_results
            .json()
            .as_array()
            .unwrap()
            .iter()
            .all(|u| u["username"] != "bob"));

        let bob_results = send(
            &router,
            "GET",
            "/api/users/search?q=a",
            Some(&bob_token),
            None,
        )
        .await;
        assert!(
            bob_results
                .json()
                .as_array()
                .unwrap()
                .iter()
                .all(|u| u["username"] != "alice"),
            "the blocked party must not see the blocker either"
        );
    }

    #[tokio::test]
    async fn blocking_yourself_and_unknown_users_is_refused() {
        let state = state("block-guards");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        let myself = send(
            &router,
            "POST",
            "/api/users/block",
            Some(&token),
            Some(serde_json::json!({ "address": alice.as_str() })),
        )
        .await;
        assert_eq!(myself.status, StatusCode::BAD_REQUEST);
        assert_eq!(myself.json()["message"], "Cannot block yourself");

        let stranger = send(
            &router,
            "POST",
            "/api/users/block",
            Some(&token),
            Some(serde_json::json!({ "address": wallet("nobody").as_str() })),
        )
        .await;
        assert_eq!(stranger.status, StatusCode::NOT_FOUND);
        assert_eq!(stranger.json()["message"], "User not found");

        let malformed = send(
            &router,
            "POST",
            "/api/users/block",
            Some(&token),
            Some(serde_json::json!({ "address": "0xnope" })),
        )
        .await;
        assert_eq!(malformed.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            malformed.json()["message"],
            "Invalid wallet address format",
            "a plain message here, not the validation envelope"
        );
    }

    #[tokio::test]
    async fn repeated_blocks_do_not_duplicate_the_list_entry() {
        let state = state("block-dedup");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let token = register(&state, &alice, "alice");
        register(&state, &bob, "bob");
        let router = build(state);

        for _ in 0..3 {
            let response = send(
                &router,
                "POST",
                "/api/users/block",
                Some(&token),
                Some(serde_json::json!({ "address": bob.as_str() })),
            )
            .await;
            assert_eq!(response.status, StatusCode::OK);
        }

        let listed = send(&router, "GET", "/api/users/blocked", Some(&token), None).await;
        // §15 #4: the reference returned the same address three times.
        assert_eq!(listed.json().as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn block_state_is_readable_from_both_sides() {
        let state = state("block-visibility");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state);

        send(
            &router,
            "POST",
            "/api/users/block",
            Some(&alice_token),
            Some(serde_json::json!({ "address": bob.as_str() })),
        )
        .await;

        let mine = send(
            &router,
            "GET",
            &format!("/api/users/{}/is-blocked", bob.as_str()),
            Some(&alice_token),
            None,
        )
        .await;
        assert_eq!(mine.json()["isBlocked"], true);

        let reverse = send(
            &router,
            "GET",
            &format!("/api/users/{}/is-blocked", alice.as_str()),
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(
            reverse.json()["isBlocked"],
            false,
            "is-blocked answers one direction only"
        );

        let who = send(
            &router,
            "GET",
            "/api/users/blocked-by",
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(who.json().as_array().unwrap().len(), 1);
        assert_eq!(who.json()[0]["blockerAddress"], alice.as_str());
    }

    #[tokio::test]
    async fn unblocking_someone_never_blocked_still_succeeds() {
        let state = state("unblock");
        let token = register(&state, &wallet("alice"), "alice");
        let router = build(state);

        let response = send(
            &router,
            "DELETE",
            &format!("/api/users/block/{}", wallet("bob").as_str()),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.json()["message"], "User unblocked successfully");
    }

    #[tokio::test]
    async fn profiles_remain_readable_across_a_block() {
        let state = state("profile-across-block");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let token = register(&state, &alice, "alice");
        register(&state, &bob, "bob");
        let router = build(state);

        send(
            &router,
            "POST",
            "/api/users/block",
            Some(&token),
            Some(serde_json::json!({ "address": bob.as_str() })),
        )
        .await;

        let response = send(
            &router,
            "GET",
            &format!("/api/users/{}", bob.as_str()),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.json()["username"], "bob");
    }

    #[tokio::test]
    async fn public_key_lookup_validates_instead_of_erroring() {
        let state = state("publickeys");
        let token = register(&state, &wallet("alice"), "alice");
        let router = build(state);

        for bad in [
            serde_json::json!({ "addresses": [] }),
            serde_json::json!({ "addresses": ["nope"] }),
            serde_json::json!({}),
        ] {
            let response = send(
                &router,
                "POST",
                "/api/users/public-keys",
                Some(&token),
                Some(bad),
            )
            .await;
            // §15 #5: these were 500s in the reference.
            assert_eq!(response.status, StatusCode::BAD_REQUEST);
        }

        let ok = send(
            &router,
            "POST",
            "/api/users/public-keys",
            Some(&token),
            Some(serde_json::json!({ "addresses": [wallet("alice").as_str()] })),
        )
        .await;
        assert_eq!(ok.status, StatusCode::OK);
        assert!(
            ok.json().as_array().unwrap().is_empty(),
            "a user with no published key is dropped from the result"
        );
    }

    #[tokio::test]
    async fn an_unknown_address_is_a_404() {
        let state = state("unknown-user");
        let token = register(&state, &wallet("alice"), "alice");
        let router = build(state);

        let response = send(
            &router,
            "GET",
            &format!("/api/users/{}", wallet("ghost").as_str()),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::NOT_FOUND);
        assert_eq!(response.json()["message"], "User not found");
    }
}
