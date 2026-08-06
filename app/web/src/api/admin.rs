//! Server administration (API.md §6.14).
//!
//! Every call here except [`Client::admin_session`] answers 403 unless the
//! signed-in wallet is listed in the server's `VITE_FRUITNATION_ADMIN`. That is
//! deployment configuration, not something the client can inspect or change —
//! which is why `admin_session` exists at all: it is the only way for a client
//! holding a restored token to find out whether to offer the console.

use gloo_net::http::Method;
use pocketskynet_core::{RoomId, WalletAddress};
use serde::{Deserialize, Serialize};

use super::{encode_segment, ApiResult, Client};

/// Whether the caller administers this server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminSession {
    #[serde(default)]
    pub is_server_admin: bool,
}

/// Server-wide totals for the console header.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminTotals {
    #[serde(default)]
    pub users: i64,
    #[serde(default)]
    pub suspended: i64,
    #[serde(default)]
    pub channels: i64,
    #[serde(default)]
    pub direct_messages: i64,
    #[serde(default)]
    pub messages: i64,
    #[serde(default)]
    pub files: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminOverview {
    #[serde(default)]
    pub totals: AdminTotals,
    /// The addresses the server parsed out of `VITE_FRUITNATION_ADMIN`,
    /// lowercased. Shown verbatim so an operator can see a typo there — which
    /// is otherwise completely silent, since its only symptom is a colleague
    /// who mysteriously has no powers.
    #[serde(default)]
    pub admins: Vec<String>,
}

/// One account, as an operator sees it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminUser {
    pub wallet_address: WalletAddress,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub profile_image: Option<String>,
    #[serde(default)]
    pub room_count: i64,
    #[serde(default)]
    pub message_count: i64,
    #[serde(default)]
    pub is_suspended: bool,
    #[serde(default)]
    pub suspended_reason: Option<String>,
    #[serde(default)]
    pub is_server_admin: bool,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// One room, as an operator sees it — **metadata only**. There is deliberately
/// no endpoint that hands an admin the contents of a room they are not in.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminRoom {
    pub id: RoomId,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub member_count: i64,
    #[serde(default)]
    pub message_count: i64,
    #[serde(default)]
    pub has_encryption: bool,
    #[serde(default)]
    pub last_activity_at: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

#[derive(Serialize)]
struct SuspendReq<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
}

impl Client {
    /// Whether *this* caller is a server admin.
    ///
    /// Never 403s — that is the point. A client restoring a stored token has no
    /// login response to read `isServerAdmin` from, and an endpoint that
    /// refused non-admins would make "you are not one" indistinguishable from
    /// "the server is unreachable".
    pub async fn admin_session(&self) -> ApiResult<bool> {
        let session: AdminSession = self.send(Method::GET, "/api/admin/session").await?;
        Ok(session.is_server_admin)
    }

    pub async fn admin_overview(&self) -> ApiResult<AdminOverview> {
        self.send(Method::GET, "/api/admin/overview").await
    }

    pub async fn admin_users(&self) -> ApiResult<Vec<AdminUser>> {
        self.send(Method::GET, "/api/admin/users").await
    }

    pub async fn admin_rooms(&self) -> ApiResult<Vec<AdminRoom>> {
        self.send(Method::GET, "/api/admin/rooms").await
    }

    /// Suspend an account. Takes effect on the target's *existing* tokens, not
    /// only their next sign-in.
    pub async fn admin_suspend(&self, who: &WalletAddress, reason: Option<&str>) -> ApiResult<()> {
        self.send_ok(
            Method::POST,
            &format!("/api/admin/users/{}/suspend", encode_segment(who.as_str())),
            &SuspendReq {
                reason: reason.filter(|r| !r.trim().is_empty()),
            },
        )
        .await
    }

    pub async fn admin_reinstate(&self, who: &WalletAddress) -> ApiResult<()> {
        self.send_ok_empty(
            Method::DELETE,
            &format!("/api/admin/users/{}/suspend", encode_segment(who.as_str())),
        )
        .await
    }

    /// Remove somebody from every room on the server and suspend them.
    ///
    /// Their messages stay where they are, attributed to them: a room's record
    /// of a conversation is not the operator's to rewrite.
    pub async fn admin_remove_user(&self, who: &WalletAddress) -> ApiResult<()> {
        self.send_ok_empty(
            Method::DELETE,
            &format!("/api/admin/users/{}", encode_segment(who.as_str())),
        )
        .await
    }

    /// Delete any room, including one whose last admin has left — which is the
    /// case the room-level endpoint cannot reach.
    pub async fn admin_delete_room(&self, room: &RoomId) -> ApiResult<()> {
        self.send_ok_empty(
            Method::DELETE,
            &format!("/api/admin/rooms/{}", encode_segment(room.as_str())),
        )
        .await
    }
}
