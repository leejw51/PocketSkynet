//! Web publishing (docs/API.md §16.2).
//!
//! Pay the publish price to the server's FruitNation wallet and it hosts your
//! page at `/sites/{id}/`. The upload is raw bytes — an HTML document or a
//! zip — with the metadata in the query string, the same shape as attachment
//! uploads. Deletion is open to any signed-in user by design.

use gloo_net::http::Method;
use serde::Deserialize;

use super::{encode_query, encode_segment, ApiError, ApiResult, Client};

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Site {
    pub id: String,
    #[serde(rename = "ownerAddress")]
    pub owner_address: String,
    pub username: String,
    pub title: String,
    #[serde(rename = "txHash")]
    pub tx_hash: String,
    #[serde(rename = "amountWei", default)]
    pub amount_wei: String,
    #[serde(rename = "sizeBytes")]
    pub size_bytes: i64,
    #[serde(rename = "fileCount")]
    pub file_count: i64,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    /// Where the server serves it: `/sites/{id}/`.
    pub url: String,
}

/// `GET /api/sites`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SitesListing {
    pub sites: Vec<Site>,
    /// The base URL other devices should use to reach this server — the
    /// Tailscale address when the host has one, else a LAN address, absent
    /// when the server is loopback-only. The Publish page prefixes it onto
    /// each site's relative `url`; without it the page falls back to its own
    /// origin.
    #[serde(rename = "shareBase", default)]
    pub share_base: Option<String>,
}

impl Client {
    /// `POST /api/sites` — publish. `bytes` is either an HTML document or a
    /// zip (the server sniffs the magic); `tx_hash` must pay the publish
    /// price and is burned on success.
    pub async fn publish_site(
        &self,
        title: &str,
        tx_hash: &str,
        bytes: Vec<u8>,
    ) -> ApiResult<Site> {
        let path = format!(
            "/api/sites?title={}&txHash={}",
            encode_query(title),
            encode_query(tx_hash),
        );
        let req = self
            .build(Method::POST, &path)
            .header("Content-Type", "application/octet-stream")
            .body(js_sys::Uint8Array::from(bytes.as_slice()))
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let resp = req
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        super::decode(resp).await
    }

    /// `GET /api/sites` — every hosted site, newest first, plus the base URL
    /// worth sharing.
    pub async fn sites(&self) -> ApiResult<SitesListing> {
        self.send(Method::GET, "/api/sites?limit=200").await
    }

    /// `DELETE /api/sites/{id}` — any signed-in user may remove any site.
    pub async fn delete_site(&self, id: &str) -> ApiResult<()> {
        self.send_ok_empty(
            Method::DELETE,
            &format!("/api/sites/{}", encode_segment(id)),
        )
        .await
    }
}
