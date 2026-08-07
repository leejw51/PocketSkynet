//! Incoming webhooks (API.md §17): create, list, revoke.
//!
//! All three are admin verbs on a room. The post endpoint itself
//! (`POST /api/webhooks/{token}`) has no method here on purpose — it is for
//! CI scripts and monitoring rigs, not for this client, which holds a wallet
//! and posts as itself.

use gloo_net::http::Method;
use pocketskynet_core::RoomId;
use serde::Serialize;

use super::{encode_segment, ApiResult, Client, Webhook};

#[derive(Serialize)]
struct CreateWebhookReq<'a> {
    name: &'a str,
}

impl Client {
    /// Create a webhook. Admin-only; the server refuses encrypted rooms and
    /// DMs, so the dialog should not have offered them in the first place.
    pub async fn create_webhook(&self, room: &RoomId, name: &str) -> ApiResult<Webhook> {
        self.send_json(
            Method::POST,
            &format!("/api/rooms/{}/webhooks", encode_segment(room.as_str())),
            &CreateWebhookReq { name },
        )
        .await
    }

    /// A room's webhooks, newest first, tokens included — this list is the
    /// credential store, which is why the server only shows it to admins.
    pub async fn webhooks(&self, room: &RoomId) -> ApiResult<Vec<Webhook>> {
        self.send(
            Method::GET,
            &format!("/api/rooms/{}/webhooks", encode_segment(room.as_str())),
        )
        .await
    }

    /// Revoke. Immediate: the next POST bearing the token is a 404.
    pub async fn revoke_webhook(&self, room: &RoomId, webhook_id: &str) -> ApiResult<()> {
        self.send_ok_empty(
            Method::DELETE,
            &format!(
                "/api/rooms/{}/webhooks/{}",
                encode_segment(room.as_str()),
                encode_segment(webhook_id)
            ),
        )
        .await
    }
}
