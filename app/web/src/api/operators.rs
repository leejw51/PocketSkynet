//! The operator ladder (the game layer's only server-side state).
//!
//! Progression itself is computed on the device against
//! [`pocketskynet_core::progression`] and kept in local storage; these two
//! calls are how a device puts its figure on the shared board and reads back
//! where that leaves it.
//!
//! Both are best-effort by design. A server that predates the endpoint
//! answers 404, and the right response to that is a hidden section — never an
//! error on a screen whose other half is entirely local.

use gloo_net::http::Method;
use serde::{Deserialize, Serialize};

use super::{ApiResult, Client};

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OperatorFile {
    #[serde(rename = "walletAddress")]
    pub wallet_address: String,
    pub username: String,
    pub load: i64,
    #[serde(rename = "rankLevel")]
    pub rank_level: i64,
    pub streak: i64,
    pub orders: i64,
    pub trophies: i64,
    #[serde(rename = "updatedAt", default)]
    pub updated_at: i64,
}

#[derive(Debug, Serialize)]
pub struct OperatorReport {
    pub load: i64,
    #[serde(rename = "rankLevel")]
    pub rank_level: i64,
    pub streak: i64,
    pub orders: i64,
    pub trophies: i64,
}

#[derive(Deserialize)]
struct BoardResponse {
    operators: Vec<OperatorFile>,
}

impl Client {
    /// `POST /api/operator` — report this device's progression.
    pub async fn report_operator(&self, report: &OperatorReport) -> ApiResult<OperatorFile> {
        self.send_json(Method::POST, "/api/operator", report).await
    }

    /// `GET /api/leaderboard` — the board, strongest first.
    pub async fn leaderboard(&self, limit: u32) -> ApiResult<Vec<OperatorFile>> {
        let response: BoardResponse = self
            .send(Method::GET, &format!("/api/leaderboard?limit={limit}"))
            .await?;
        Ok(response.operators)
    }
}
