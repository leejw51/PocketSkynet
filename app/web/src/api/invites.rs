//! Invite links (API.md §6.7a, ROADMAP §7 M1).
//!
//! The bearer token is the whole credential: it exists in the create response
//! and in whatever link or QR code was made from it, never in any later API
//! answer — the server keeps only a hash. Losing a link therefore means
//! minting a new one, which is also why [`Client::revoke_invite_link`] exists
//! at all: the list endpoint is the admin's ledger of doors still open.

use gloo_net::http::Method;
use pocketskynet_core::{RoomId, WalletAddress};
use serde::{Deserialize, Serialize};

use super::{encode_segment, ApiResult, Client};

/// One invite link as the list/create endpoints describe it — everything
/// except the token.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteLink {
    pub id: String,
    pub room_id: RoomId,
    pub created_by: WalletAddress,
    pub created_at: String,
    pub expires_at: String,
    #[serde(default)]
    pub max_uses: Option<i64>,
    pub use_count: i64,
    /// Computed server-side against the server's clock, so the UI never
    /// parses ISO strings against the browser's.
    pub expired: bool,
}

/// `POST /api/rooms/:roomId/invites` — the only response that ever carries
/// the token.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteCreated {
    pub invite: InviteLink,
    pub token: String,
}

/// `GET /api/invites/:token` — what the landing page shows before sign-in.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvitePeek {
    pub room_name: String,
    pub member_count: i64,
    pub expires_at: String,
}

/// `POST /api/invites/:token/redeem`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteRedeemed {
    pub room_id: RoomId,
    pub room_name: String,
    pub already_member: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateReq {
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_in_hours: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_uses: Option<i64>,
}

impl Client {
    /// Mint an invite link. Admin-only. `expires_in_hours` `None` takes the
    /// server default (a week); `max_uses` `None` means unlimited.
    pub async fn create_invite_link(
        &self,
        room: &RoomId,
        expires_in_hours: Option<i64>,
        max_uses: Option<i64>,
    ) -> ApiResult<InviteCreated> {
        self.send_json(
            Method::POST,
            &format!("/api/rooms/{}/invites", encode_segment(room.as_str())),
            &CreateReq {
                expires_in_hours,
                max_uses,
            },
        )
        .await
    }

    /// The room's live links, newest first. Admin-only. Revoked links are
    /// gone; expired ones remain, flagged, until somebody revokes them.
    pub async fn invite_links(&self, room: &RoomId) -> ApiResult<Vec<InviteLink>> {
        self.send(
            Method::GET,
            &format!("/api/rooms/{}/invites", encode_segment(room.as_str())),
        )
        .await
    }

    /// Kill a link now. Admin-only, immediate, no undo — mint a new one
    /// instead.
    pub async fn revoke_invite_link(&self, room: &RoomId, invite_id: &str) -> ApiResult<()> {
        self.send_ok_empty(
            Method::DELETE,
            &format!(
                "/api/rooms/{}/invites/{}",
                encode_segment(room.as_str()),
                encode_segment(invite_id)
            ),
        )
        .await
    }

    /// Ask what a token opens without spending it. The one endpoint here that
    /// works signed out — the landing page calls it before there is a wallet.
    pub async fn peek_invite(&self, token: &str) -> ApiResult<InvitePeek> {
        self.send(
            Method::GET,
            &format!("/api/invites/{}", encode_segment(token)),
        )
        .await
    }

    /// Redeem a token into membership. Requires a session; redeeming a room
    /// you are already in succeeds without spending a use.
    pub async fn redeem_invite(&self, token: &str) -> ApiResult<InviteRedeemed> {
        self.send_json(
            Method::POST,
            &format!("/api/invites/{}/redeem", encode_segment(token)),
            &serde_json::json!({}),
        )
        .await
    }
}
