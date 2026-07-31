//! User directory, public keys and blocking (API.md §6.3, §6.4).

use gloo_net::http::Method;
use pocketskynet_core::WalletAddress;
use serde::Serialize;

use super::{encode_query, encode_segment, ApiResult, BlockedUser, Client, PublicKeyEntry, User};

#[derive(Serialize)]
struct AddressesReq<'a> {
    addresses: Vec<&'a str>,
}

#[derive(Serialize)]
struct AddressReq<'a> {
    address: &'a str,
}

impl Client {
    /// Directory search. The server filters blocks **both** ways, so a result
    /// set that omits someone tells you nothing about which direction the block
    /// runs — and the UI must not try to guess (DESIGN.md §9).
    pub async fn search_users(&self, query: &str) -> ApiResult<Vec<User>> {
        self.send(
            Method::GET,
            &format!("/api/users/search?q={}", encode_query(query)),
        )
        .await
    }

    pub async fn get_user(&self, address: &WalletAddress) -> ApiResult<User> {
        self.send(
            Method::GET,
            &format!("/api/users/{}", encode_segment(address.as_str())),
        )
        .await
    }

    /// Fetch encryption public keys for up to 50 addresses.
    ///
    /// The response drops addresses with no user row or no published key, so
    /// the result may be shorter than the request — callers must check for the
    /// addresses they actually need rather than zipping by index.
    ///
    /// **The returned keys are unverified.** `crypto::verify_key_binding` must
    /// pass before any of them is used to wrap a room key (CRYPTO.md §4.3).
    pub async fn public_keys(&self, addresses: &[WalletAddress]) -> ApiResult<Vec<PublicKeyEntry>> {
        if addresses.is_empty() {
            return Ok(Vec::new());
        }
        let list: Vec<&str> = addresses.iter().map(|a| a.as_str()).collect();
        self.send_json(
            Method::POST,
            "/api/users/public-keys",
            &AddressesReq { addresses: list },
        )
        .await
    }

    /// Everyone the caller has blocked.
    pub async fn blocked(&self) -> ApiResult<Vec<BlockedUser>> {
        self.send(Method::GET, "/api/users/blocked").await
    }

    /// Everyone who has blocked the caller. Native clients need this to apply
    /// the same bidirectional filtering the server applies to search — without
    /// it, a blocked-by user's messages would still render locally.
    pub async fn blocked_by(&self) -> ApiResult<Vec<BlockedUser>> {
        self.send(Method::GET, "/api/users/blocked-by").await
    }

    pub async fn block_user(&self, address: &WalletAddress) -> ApiResult<()> {
        self.send_ok(
            Method::POST,
            "/api/users/block",
            &AddressReq {
                address: address.as_str(),
            },
        )
        .await
    }

    /// Idempotent: unblocking someone who was never blocked returns 200, and
    /// the call clears every duplicate row the reference server may have made.
    pub async fn unblock_user(&self, address: &WalletAddress) -> ApiResult<()> {
        self.send_ok_empty(
            Method::DELETE,
            &format!("/api/users/block/{}", encode_segment(address.as_str())),
        )
        .await
    }
}
