//! Shout — the paid broadcast (docs/API.md §16.1).
//!
//! Pay the shout price to the server's FruitNation wallet, present the
//! transaction hash, and the text lands on every connected screen for up to a
//! minute. The realtime `shout` event is a wake-up; this module is the REST
//! half that actually carries the content.

use gloo_net::http::Method;
use serde::{Deserialize, Serialize};

use super::{ApiResult, Client};

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Shout {
    pub id: String,
    #[serde(rename = "senderAddress")]
    pub sender_address: String,
    pub username: String,
    pub text: String,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(rename = "expiresAt")]
    pub expires_at: i64,
    #[serde(rename = "txHash")]
    pub tx_hash: String,
    #[serde(rename = "amountWei", default)]
    pub amount_wei: String,
}

#[derive(Deserialize)]
struct ActiveResponse {
    shouts: Vec<Shout>,
}

#[derive(Serialize)]
struct ShoutRequest<'a> {
    text: &'a str,
    #[serde(rename = "txHash")]
    tx_hash: &'a str,
}

impl Client {
    /// `POST /api/shout` — broadcast. `tx_hash` must pay the server's wallet
    /// at least the shout price; the server verifies it on-chain and burns
    /// the hash, so this either costs the payment exactly once or fails.
    pub async fn shout(&self, text: &str, tx_hash: &str) -> ApiResult<Shout> {
        self.send_json(Method::POST, "/api/shout", &ShoutRequest { text, tx_hash })
            .await
    }

    /// `GET /api/shout/active` — every shout still burning, newest first.
    pub async fn active_shouts(&self) -> ApiResult<Vec<Shout>> {
        let response: ActiveResponse = self.send(Method::GET, "/api/shout/active").await?;
        Ok(response.shouts)
    }
}
