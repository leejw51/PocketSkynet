//! Shout — the paid broadcast (docs/API.md §16.1).
//!
//! Pay at least the shout price (default 10 CRO, `PS_SHOUT_PRICE_CRO`) to the
//! operator's FruitNation wallet, present the transaction hash, and your text
//! lands on every connected screen for up to a minute. The payment is the
//! authorisation: there is no admin approval, no room scoping, and no way to
//! shout for free — which is also why this endpoint is a poor DoS lever, since
//! each broadcast burns a real on-chain transfer.
//!
//! * `POST /api/shout`        — `{text, txHash, durationSecs?}` → the shout
//! * `GET  /api/shout/active` — every shout still burning
//!
//! The realtime event (`ServerEvent::Shout`, `Target::All`) is a wake-up like
//! every other: clients fetch the active set over REST. Dismissing a banner is
//! a per-viewer act in the client; the server never closes a shout early.

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use pocketskynet_core::{ServerEvent, Target};
use serde::Deserialize;
use serde_json::json;

use crate::auth::AuthUser;
use crate::db::now_ms;
use crate::db::shouts::{self, NewShout, Shout};
use crate::error::{ApiError, ApiResult};
use crate::payment::{self, Purpose};
use crate::validate::ValidJson;
use crate::AppState;

/// The longest a shout may burn. The requirement, verbatim: max one minute.
pub const MAX_DURATION_SECS: i64 = 60;

/// The shortest — a banner that blinks out before anyone reads it is a
/// donation, and probably a client bug.
const MIN_DURATION_SECS: i64 = 5;

/// Longest shout text. A banner is a headline, not a post; anything longer
/// stops fitting on the phones half the audience is holding.
const MAX_TEXT_CHARS: usize = 200;

/// Concurrent shouts one wallet may have burning. Money is the gate, but
/// without a ceiling one funding round could wallpaper every screen.
const MAX_ACTIVE_PER_SENDER: i64 = 3;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/shout", post(create))
        .route("/shout/active", get(active))
}

#[derive(Debug, Deserialize)]
struct ShoutBody {
    text: Option<String>,
    #[serde(rename = "txHash")]
    tx_hash: Option<String>,
    #[serde(rename = "durationSecs")]
    duration_secs: Option<i64>,
}

/// Shout text: 1–200 chars after trimming, single line, no control
/// characters. Unicode (CJK, emoji) is welcome — it is a broadcast, people
/// will use it for celebration.
fn shout_text(raw: Option<&str>) -> ApiResult<String> {
    let raw = raw.ok_or_else(|| crate::validate::required("text", "Shout text"))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ApiError::field("text", "Shout text is required"));
    }
    if trimmed.chars().count() > MAX_TEXT_CHARS {
        return Err(ApiError::field(
            "text",
            "Shout text must be at most 200 characters",
        ));
    }
    if trimmed.chars().any(|c| (c as u32) <= 0x1f || c == '\u{7f}') {
        return Err(ApiError::field(
            "text",
            "Shout text must be a single line without control characters",
        ));
    }
    Ok(trimmed.to_owned())
}

/// `POST /api/shout`
async fn create(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    ValidJson(body): ValidJson<ShoutBody>,
) -> ApiResult<Json<Shout>> {
    let text = shout_text(body.text.as_deref())?;
    let tx_hash = body
        .tx_hash
        .as_deref()
        .ok_or_else(|| crate::validate::required("txHash", "Transaction hash"))?;
    let duration_secs = body
        .duration_secs
        .unwrap_or(MAX_DURATION_SECS)
        .clamp(MIN_DURATION_SECS, MAX_DURATION_SECS);

    // The ceiling first: refusing *before* burning the payment hash means the
    // over-eager caller can re-use the same transaction once a slot frees up.
    let sender = caller.as_str().to_owned();
    let burning = state
        .db
        .call(move |conn| shouts::active_count_for(conn, &sender, now_ms()))
        .await?;
    if burning >= MAX_ACTIVE_PER_SENDER {
        return Err(ApiError::bad_request(
            "You already have the maximum number of active shouts — wait for one to expire",
        ));
    }

    let price = payment::price_wei(&payment::shout_price_cro());
    let amount_wei =
        payment::verify_and_record(&state, &caller, tx_hash, price, Purpose::Shout).await?;

    let now = now_ms();
    let new = NewShout {
        id: format!("shout_{}_{}", now, uuid::Uuid::new_v4()),
        sender_address: caller.as_str().to_owned(),
        text,
        tx_hash: payment::normalize_tx_hash(tx_hash)?,
        amount_wei,
        created_at: now,
        expires_at: now + duration_secs * 1000,
    };
    let shout = state.db.call(move |conn| shouts::create(conn, new)).await?;

    let _ = state.log.append_audit(
        "shout_broadcast",
        Some(&caller),
        json!({ "shoutId": shout.id, "txHash": shout.tx_hash, "durationSecs": duration_secs }),
    );
    state
        .hub
        .publish_best_effort(
            Target::All,
            Some(caller),
            ServerEvent::Shout {
                shout_id: shout.id.clone(),
            },
        )
        .await;

    Ok(Json(shout))
}

/// `GET /api/shout/active`
async fn active(
    State(state): State<AppState>,
    AuthUser(_caller): AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let shouts = state.db.call(|conn| shouts::active(conn, now_ms())).await?;
    Ok(Json(json!({ "shouts": shouts })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    use crate::routes::build;
    use crate::test_support::{register, send, state, wallet};

    fn tx(byte: char) -> String {
        format!("0x{}", byte.to_string().repeat(64))
    }

    #[tokio::test]
    async fn a_paid_shout_reaches_the_active_list_with_a_username() {
        let state = state("shout-roundtrip");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        let response = send(
            &router,
            "POST",
            "/api/shout",
            Some(&token),
            Some(json!({ "text": "  We are live! 🎉  ", "txHash": tx('a') })),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
        assert_eq!(response.body["text"], "We are live! 🎉");
        assert_eq!(response.body["username"], "alice");
        let expires = response.body["expiresAt"].as_i64().unwrap();
        let created = response.body["createdAt"].as_i64().unwrap();
        assert_eq!(expires - created, MAX_DURATION_SECS * 1000);

        let response = send(&router, "GET", "/api/shout/active", Some(&token), None).await;
        assert_eq!(response.status, StatusCode::OK);
        let shouts = response.body["shouts"].as_array().unwrap();
        assert_eq!(shouts.len(), 1);
        assert_eq!(shouts[0]["username"], "alice");
    }

    #[tokio::test]
    async fn a_transaction_hash_buys_exactly_one_shout() {
        let state = state("shout-replay");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        let body = json!({ "text": "first", "txHash": tx('b') });
        let response = send(&router, "POST", "/api/shout", Some(&token), Some(body)).await;
        assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);

        // The same receipt again — and case games must not launder it.
        for hash in [tx('b'), tx('b').to_uppercase().replace("0X", "0x")] {
            let response = send(
                &router,
                "POST",
                "/api/shout",
                Some(&token),
                Some(json!({ "text": "again", "txHash": hash })),
            )
            .await;
            assert_eq!(response.status, StatusCode::CONFLICT, "{:?}", response.body);
        }
    }

    #[tokio::test]
    async fn the_duration_is_clamped_to_a_minute() {
        let state = state("shout-clamp");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        let response = send(
            &router,
            "POST",
            "/api/shout",
            Some(&token),
            Some(json!({ "text": "forever!", "txHash": tx('c'), "durationSecs": 86400 })),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
        let expires = response.body["expiresAt"].as_i64().unwrap();
        let created = response.body["createdAt"].as_i64().unwrap();
        assert_eq!(
            expires - created,
            MAX_DURATION_SECS * 1000,
            "a shout must never outlive the minute, whatever the client asks"
        );
    }

    #[tokio::test]
    async fn garbage_text_and_hashes_are_rejected_before_any_payment_burns() {
        let state = state("shout-validate");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        for body in [
            json!({ "txHash": tx('d') }),                          // no text
            json!({ "text": "   ", "txHash": tx('d') }),           // blank
            json!({ "text": "x".repeat(201), "txHash": tx('d') }), // too long
            json!({ "text": "two\nlines", "txHash": tx('d') }),    // control char
            json!({ "text": "fine" }),                             // no hash
            json!({ "text": "fine", "txHash": "0xnothex" }),       // bad hash
        ] {
            let response = send(&router, "POST", "/api/shout", Some(&token), Some(body)).await;
            assert_eq!(
                response.status,
                StatusCode::BAD_REQUEST,
                "{:?}",
                response.body
            );
        }

        // None of those burned the hash: it still buys a real shout.
        let response = send(
            &router,
            "POST",
            "/api/shout",
            Some(&token),
            Some(json!({ "text": "fine", "txHash": tx('d') })),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
    }

    #[tokio::test]
    async fn one_wallet_cannot_wallpaper_every_screen() {
        let state = state("shout-cap");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        for (i, hash_byte) in ['1', '2', '3'].into_iter().enumerate() {
            let response = send(
                &router,
                "POST",
                "/api/shout",
                Some(&token),
                Some(json!({ "text": format!("shout {i}"), "txHash": tx(hash_byte) })),
            )
            .await;
            assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
        }

        let response = send(
            &router,
            "POST",
            "/api/shout",
            Some(&token),
            Some(json!({ "text": "one more", "txHash": tx('4') })),
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert!(
            response.body["message"]
                .as_str()
                .unwrap()
                .contains("maximum number of active shouts"),
            "{:?}",
            response.body
        );
    }

    #[tokio::test]
    async fn a_shout_publishes_a_global_wake_up_event() {
        let state = state("shout-event");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let mut rx = state.hub.subscribe();
        let router = build(state);

        let response = send(
            &router,
            "POST",
            "/api/shout",
            Some(&token),
            Some(json!({ "text": "hear ye", "txHash": tx('9') })),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
        let shout_id = response.body["id"].as_str().unwrap().to_owned();

        let envelope = rx.try_recv().expect("the wake-up must have been broadcast");
        assert_eq!(envelope.target, pocketskynet_core::Target::All);
        assert_eq!(
            envelope.origin.as_ref().map(|w| w.as_str()),
            Some(alice.as_str())
        );
        match envelope.event.as_ref() {
            pocketskynet_core::ServerEvent::Shout { shout_id: id } => assert_eq!(*id, shout_id),
            other => panic!("expected a shout event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn shouting_needs_a_token() {
        let router = build(state("shout-auth"));
        let response = send(
            &router,
            "POST",
            "/api/shout",
            None,
            Some(json!({ "text": "free?", "txHash": tx('e') })),
        )
        .await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED);

        let response = send(&router, "GET", "/api/shout/active", None, None).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    }
}
