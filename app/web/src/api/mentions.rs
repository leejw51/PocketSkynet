//! The mentions inbox (API.md §6.13).
//!
//! Read-only. There is no "mark as read" because a mention is read when its
//! room is read — the pointer `POST /rooms/{id}/read` already advances — and a
//! second read state would be a second thing to keep in step with the first.

use gloo_net::http::Method;
use pocketskynet_core::RoomId;
use serde::Deserialize;

use super::{ApiResult, Client, Message};

/// One entry in the inbox: the message, plus enough about its room to render
/// the row without a second request.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mention {
    pub room_id: RoomId,
    #[serde(default)]
    pub room_name: String,
    #[serde(default)]
    pub room_kind: String,
    /// `false` once the caller's read pointer in that room has passed it. The
    /// entry itself does not disappear — it stops being new.
    #[serde(default)]
    pub is_unread: bool,
    pub message: Message,
}

impl Client {
    /// Everything that named the caller, newest first, across every room they
    /// are still in. Leaving a room takes its mentions with it.
    pub async fn mentions(&self, limit: u32) -> ApiResult<Vec<Mention>> {
        self.send(
            Method::GET,
            &format!("/api/mentions?limit={}", limit.clamp(1, 200)),
        )
        .await
    }
}
