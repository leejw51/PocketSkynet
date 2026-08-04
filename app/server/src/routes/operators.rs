//! The operator ladder.
//!
//! * `POST /api/operator`    — report this device's progression
//! * `GET  /api/leaderboard` — the board, strongest first
//!
//! The client owns progression; this is a notice board for it. Reports are
//! self-declared and the server cannot check them — see `db/operators.rs` for
//! why that is the right trade on a server you host for people you know, and
//! what the running-maximum rule buys instead.
//!
//! What *is* enforced here is shape: every figure is clamped to a range the
//! real game can produce, so a malformed or mischievous client cannot push a
//! row that breaks the board's layout or overflows an integer downstream.

use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::auth::AuthUser;
use crate::db::now_ms;
use crate::db::operators::{self, OperatorFile, Report};
use crate::error::ApiResult;
use crate::validate::ValidJson;
use crate::AppState;

/// Ceilings, taken from what the client's own ladder can reach.  The top rank
/// begins at 15,800 load; a couple of orders of magnitude above that is
/// generous for any real account and still nowhere near an overflow.
const MAX_LOAD: i64 = 100_000_000;
const MAX_RANK: i64 = 10;
const MAX_STREAK: i64 = 36_500;
const MAX_COUNT: i64 = 1_000_000;

/// Board size.  Long enough to find yourself on a family server, short enough
/// that the response stays one screen of JSON.
const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 200;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/operator", post(report))
        .route("/leaderboard", get(board))
}

#[derive(Debug, Deserialize)]
struct ReportBody {
    load: Option<i64>,
    #[serde(rename = "rankLevel")]
    rank_level: Option<i64>,
    streak: Option<i64>,
    orders: Option<i64>,
    trophies: Option<i64>,
}

async fn report(
    State(state): State<AppState>,
    caller: AuthUser,
    ValidJson(body): ValidJson<ReportBody>,
) -> ApiResult<Json<OperatorFile>> {
    let report = Report {
        wallet_address: caller.to_owned_address(),
        load: body.load.unwrap_or(0).clamp(0, MAX_LOAD),
        rank_level: body.rank_level.unwrap_or(1).clamp(1, MAX_RANK),
        streak: body.streak.unwrap_or(0).clamp(0, MAX_STREAK),
        orders: body.orders.unwrap_or(0).clamp(0, MAX_COUNT),
        trophies: body.trophies.unwrap_or(0).clamp(0, MAX_COUNT),
        now: now_ms(),
    };
    let file = state
        .db
        .call(move |conn| operators::record(conn, report))
        .await?;
    Ok(Json(file))
}

#[derive(Debug, Deserialize)]
struct BoardQuery {
    limit: Option<i64>,
}

async fn board(
    State(state): State<AppState>,
    _caller: AuthUser,
    Query(query): Query<BoardQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let operators = state
        .db
        .call(move |conn| operators::board(conn, limit))
        .await?;
    Ok(Json(json!({ "operators": operators })))
}
