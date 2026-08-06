//! Who is at their desk (API.md §6.15).

use gloo_net::http::Method;
use pocketskynet_core::{PresenceStatus, WalletAddress};
use serde::{Deserialize, Serialize};

use super::{ApiResult, Client};

/// One person's status, as the snapshot reports it.
///
/// The response omits anyone offline, so a client keys a map on this and treats
/// a missing address as offline — which is also the right default for somebody
/// it has simply never heard about.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PresenceEntry {
    pub wallet_address: WalletAddress,
    pub status: PresenceStatus,
}

#[derive(Serialize)]
struct DeclareReq {
    status: &'static str,
}

impl Client {
    /// The authoritative snapshot: everyone the caller shares a room with who
    /// is not offline, plus the caller.
    ///
    /// Called whenever a transport comes up, because presence events are
    /// transient and never replayed — a reconnect leaves a hole, not a stale
    /// value, and this is what fills it.
    pub async fn presence(&self) -> ApiResult<Vec<PresenceEntry>> {
        self.send(Method::GET, "/api/presence").await
    }

    /// Declare this client's own status.
    ///
    /// On WebSocket the same thing goes over the socket as a `presence` frame,
    /// which is cheaper and needs no round trip; this is the path for the SSE
    /// and polling tiers, which have no upstream channel — and their heartbeat,
    /// since the server ages a silent stream into *away* without one.
    pub async fn set_presence(&self, status: PresenceStatus) -> ApiResult<()> {
        self.send_ok(
            Method::PUT,
            "/api/presence",
            &DeclareReq {
                status: status.as_str(),
            },
        )
        .await
    }
}
