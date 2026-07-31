//! Invitations (API.md §6.7).
//!
//! An invitation creates no membership until it is accepted. That is the whole
//! point of the screen it drives: consent, not notification.

use gloo_net::http::Method;
use pocketskynet_core::{RoomId, WalletAddress};
use serde::Serialize;

use super::{encode_segment, ApiResult, Client, Invitation};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UserAddressReq<'a> {
    user_address: &'a str,
}

impl Client {
    /// Invite someone. Admin-only, and gated by blocks in **both** directions:
    /// the server returns 403 whether the inviter blocked the invitee or the
    /// invitee blocked the inviter. The two messages differ, but the UI must
    /// not relay which one it got — that would leak "this person blocked you".
    pub async fn invite(&self, room: &RoomId, who: &WalletAddress) -> ApiResult<()> {
        self.send_ok(
            Method::POST,
            &format!("/api/rooms/{}/invite", encode_segment(room.as_str())),
            &UserAddressReq {
                user_address: who.as_str(),
            },
        )
        .await
    }

    /// The caller's pending invitations, newest first. Invitations whose room
    /// has been deleted are dropped server-side.
    pub async fn invitations(&self) -> ApiResult<Vec<Invitation>> {
        self.send(Method::GET, "/api/invitations").await
    }

    /// Accept. Any room key an admin pre-wrapped at invite time becomes
    /// readable at this point — it was already stored, it was just unreachable
    /// while the caller was not a member.
    ///
    /// Note the path is under `/api/invitations`, not `/api/rooms`.
    pub async fn accept_invitation(&self, room: &RoomId) -> ApiResult<()> {
        self.send_ok(
            Method::POST,
            &format!("/api/invitations/{}/accept", encode_segment(room.as_str())),
            &serde_json::json!({}),
        )
        .await
    }

    /// Decline. This also discards any pre-wrapped room key for every epoch, so
    /// declining genuinely gives up access rather than merely hiding it.
    pub async fn decline_invitation(&self, room: &RoomId) -> ApiResult<()> {
        self.send_ok(
            Method::POST,
            &format!("/api/invitations/{}/decline", encode_segment(room.as_str())),
            &serde_json::json!({}),
        )
        .await
    }
}
