//! Web publishing (docs/API.md §16.2).
//!
//! Pay the publish price to the server's FruitNation wallet and it hosts your
//! page at `/sites/{id}/`. The upload itself — an HTML document or a zip, the
//! server sniffs the magic — goes through the chunked session protocol
//! (`api::uploads::upload_in_chunks` with `Target::Site`, driven from
//! `components/publish.rs`), not a function in this module; publishing is
//! large enough (up to 25 MB) to deserve the same treatment as any other
//! upload. Deletion is open to any signed-in user by design.

use gloo_net::http::Method;
use serde::Deserialize;

use super::{encode_segment, ApiResult, Client};

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
