//! Messages, reactions, sync and read state (API.md §6.10–§6.12).

use gloo_net::http::Method;
use pocketskynet_core::{MessageId, RoomId, WalletAddress};
use serde::Serialize;

use super::{
    encode_segment, ApiError, ApiResult, Client, EmoticonAggregation, Message, ReadResponse,
    SerialResponse, SyncPage,
};

/// The body shared by send and edit. `iv`/`hmac` are `Option` because an
/// unencrypted message omits them — but an *encrypted* one must always resend
/// both on edit, or the server silently downgrades the row to plaintext
/// (API.md quirk, §6.10.3).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageBody {
    pub content: String,
    pub msg_hash: String,
    pub is_encrypted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iv: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hmac: Option<String>,
    pub enc_ver: i64,
    pub key_version: i64,
    /// Post into a thread. Send only — an edit cannot move a message between
    /// threads, and the server ignores it there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_message_id: Option<MessageId>,
    /// The people this message names.
    ///
    /// Sent explicitly rather than left to the server's parser, for two
    /// reasons that both matter: a username may contain spaces or emoji, which
    /// no `@token` grammar recovers from plaintext; and in an encrypted room
    /// there is no plaintext to parse at all. Omitted when empty so a plain
    /// message stays a plain request.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<WalletAddress>,
    /// The server-hosted files this message shows, as `{sha256}.{ext}` names.
    ///
    /// Declared for the same reason mentions are, and it matters most for the
    /// same case: in an encrypted room the server holds ciphertext, so this is
    /// the only thing tying a picture to the room it was posted in — and
    /// therefore the only thing that lets destroying the room destroy the
    /// picture instead of orphaning it on disk. Omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<String>,
}

impl MessageBody {
    /// The common case: no thread, nobody named.
    ///
    /// A constructor rather than `Default` because `content` and `msgHash` have
    /// no sensible default — a message with an empty hash is one the server
    /// will refuse, and finding that out at runtime is worse than here.
    pub fn plain(content: String, msg_hash: String) -> Self {
        Self {
            content,
            msg_hash,
            is_encrypted: false,
            iv: None,
            hmac: None,
            enc_ver: 1,
            key_version: 1,
            parent_message_id: None,
            mentions: Vec::new(),
            media: Vec::new(),
        }
    }

    pub fn in_thread(mut self, parent: Option<MessageId>) -> Self {
        self.parent_message_id = parent;
        self
    }

    pub fn naming(mut self, mentions: Vec<WalletAddress>) -> Self {
        self.mentions = mentions;
        self
    }

    /// The hosted files the message shows (`crate::media::hosted_names`).
    pub fn showing(mut self, media: Vec<String>) -> Self {
        self.media = media;
        self
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EmoticonReq<'a> {
    emoticon_code: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReadReq {
    last_read_serial: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PublishReq<'a> {
    tx_hash: &'a str,
    to_address: &'a str,
}

impl Client {
    /// Send a message.
    ///
    /// Two 409s are normal traffic here, not exceptions:
    /// `KEY_ROTATION_REQUIRED` (rotate, then resend) and `STALE_KEY_VERSION`
    /// (re-encrypt under the epoch named in the error, retry **once**).
    pub async fn send_message(&self, room: &RoomId, body: &MessageBody) -> ApiResult<Message> {
        self.send_json(
            Method::POST,
            &format!("/api/rooms/{}/messages", encode_segment(room.as_str())),
            body,
        )
        .await
    }

    /// Initial load and scroll-back.
    ///
    /// Paginate on the oldest returned `messageTimestamp`, **never** on the
    /// returned count: the server applies its `LIMIT` before filtering out
    /// reaction and purge rows, so a full page can come back short while older
    /// messages still exist.
    pub async fn messages(
        &self,
        room: &RoomId,
        before: Option<i64>,
        limit: u32,
    ) -> ApiResult<Vec<Message>> {
        let mut path = format!(
            "/api/rooms/{}/messages?limit={}",
            encode_segment(room.as_str()),
            limit.clamp(1, 100)
        );
        if let Some(b) = before {
            path.push_str(&format!("&before={b}"));
        }
        self.send(Method::GET, &path).await
    }

    /// One thread, root first.
    ///
    /// `id` may name the root or any reply in it — both answer with the same
    /// list, so a client holding only a reply (from `/sync`, say) can open the
    /// thread without first working out where it starts.
    pub async fn thread(&self, id: &MessageId) -> ApiResult<Vec<Message>> {
        self.send(
            Method::GET,
            &format!("/api/messages/{}/thread", encode_segment(id.as_str())),
        )
        .await
    }

    /// Edit. The row is updated in place — same id, same `createdAt`, same
    /// `messageTimestamp` — and only `msgSerial` advances so `/sync` redelivers
    /// it. Encrypted edits must be re-encrypted under the *current* epoch with
    /// a fresh IV.
    pub async fn edit_message(&self, id: &MessageId, body: &MessageBody) -> ApiResult<Message> {
        self.send_json(
            Method::PATCH,
            &format!("/api/messages/{}", encode_segment(id.as_str())),
            body,
        )
        .await
    }

    /// Delete. Note **any room member** may delete **any** message — this is a
    /// deliberate "forgetting-first" property of the product, not a bug, and
    /// the confirmation copy says so.
    pub async fn delete_message(&self, id: &MessageId) -> ApiResult<()> {
        self.send_ok_empty(
            Method::DELETE,
            &format!("/api/messages/{}", encode_segment(id.as_str())),
        )
        .await
    }

    /// Purge a room's entire history. Hard delete plus one `delete_all` marker
    /// so every other client learns to clear its cache.
    pub async fn delete_all_messages(&self, room: &RoomId) -> ApiResult<()> {
        self.send_ok_empty(
            Method::DELETE,
            &format!("/api/rooms/{}/messages", encode_segment(room.as_str())),
        )
        .await
    }

    /// Anchor a message's `msgHash` to an on-chain transaction.
    pub async fn publish_message(
        &self,
        id: &MessageId,
        tx_hash: &str,
        to_address: &str,
    ) -> ApiResult<Message> {
        self.send_json(
            Method::POST,
            &format!("/api/messages/{}/publish", encode_segment(id.as_str())),
            &PublishReq {
                tx_hash,
                to_address,
            },
        )
        .await
    }

    pub async fn add_emoticon(&self, id: &MessageId, code: &str) -> ApiResult<()> {
        self.send_ok(
            Method::POST,
            &format!("/api/messages/{}/emoticons", encode_segment(id.as_str())),
            &EmoticonReq {
                emoticon_code: code,
            },
        )
        .await
    }

    /// Remove a reaction. The code goes in a path segment and is arbitrary
    /// Unicode, so it is percent-encoded exactly once here.
    pub async fn remove_emoticon(&self, id: &MessageId, code: &str) -> ApiResult<()> {
        self.send_ok_empty(
            Method::DELETE,
            &format!(
                "/api/messages/{}/emoticons/{}",
                encode_segment(id.as_str()),
                encode_segment(code)
            ),
        )
        .await
    }

    /// Server-side reaction aggregation. Prefer folding reactions out of
    /// `/sync` — this endpoint is **not** block-filtered and can disagree with
    /// the stream view. Kept for a one-shot refresh after a reaction round-trip.
    pub async fn emoticons(&self, id: &MessageId) -> ApiResult<Vec<EmoticonAggregation>> {
        self.send(
            Method::GET,
            &format!("/api/messages/{}/emoticons", encode_segment(id.as_str())),
        )
        .await
    }

    /// One page of the incremental state-transfer stream.
    ///
    /// Unlike `/messages`, nothing is filtered by type or `isDeleted` — deleted
    /// rows, purge markers and both reaction types all arrive, which is exactly
    /// what makes incremental folding correct.
    ///
    /// `hasMore` lives in the `X-Has-More` header, not the body.
    pub async fn sync(&self, room: &RoomId, since: i64) -> ApiResult<SyncPage> {
        let path = format!(
            "/api/rooms/{}/sync?since={}",
            encode_segment(room.as_str()),
            since.max(0)
        );
        let req = gloo_net::http::RequestBuilder::new(&self.url(&path))
            .method(Method::GET)
            .header(
                "Authorization",
                &format!("Bearer {}", self.token().unwrap_or_default()),
            )
            .build()
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let resp = req
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;

        let status = resp.status();
        // Read the header before consuming the body — `text()` takes ownership.
        let has_more = resp
            .headers()
            .get("x-has-more")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let body = resp
            .text()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;

        if !(200..300).contains(&status) {
            return Err(ApiError::from_response(status, &body));
        }
        let events: Vec<Message> =
            serde_json::from_str(&body).map_err(|e| ApiError::Decode(e.to_string()))?;
        Ok(SyncPage { events, has_more })
    }

    /// A change detector, not a read cursor: it is **not** block-filtered, so
    /// it can sit permanently ahead of a viewer's cursor when the newest
    /// messages are all from blocked senders.
    pub async fn latest_serial(&self, room: &RoomId) -> ApiResult<i64> {
        let r: SerialResponse = self
            .send(
                Method::GET,
                &format!("/api/rooms/{}/latest-serial", encode_segment(room.as_str())),
            )
            .await?;
        Ok(r.serial)
    }

    /// Advance the read pointer. Monotonic server-side — a lower serial is a
    /// no-op — so it is safe to call optimistically and out of order.
    pub async fn mark_read(&self, room: &RoomId, serial: i64) -> ApiResult<i64> {
        let r: ReadResponse = self
            .send_json(
                Method::POST,
                &format!("/api/rooms/{}/read", encode_segment(room.as_str())),
                &ReadReq {
                    last_read_serial: serial.max(0),
                },
            )
            .await?;
        Ok(r.last_read_serial)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_encrypted_body_serialises_every_field_the_server_gates_on() {
        let b = MessageBody {
            content: "ctB64==".into(),
            msg_hash: "a".repeat(64),
            is_encrypted: true,
            iv: Some("0".repeat(32)),
            hmac: Some("f".repeat(64)),
            enc_ver: 2,
            key_version: 3,
            parent_message_id: None,
            mentions: Vec::new(),
            media: Vec::new(),
        };
        let json: serde_json::Value = serde_json::to_value(&b).unwrap();
        assert_eq!(json["isEncrypted"], true);
        assert_eq!(json["encVer"], 2);
        assert_eq!(json["keyVersion"], 3);
        assert_eq!(json["msgHash"], "a".repeat(64));
        assert!(json.get("iv").is_some());
        assert!(json.get("hmac").is_some());
    }

    #[test]
    fn a_plaintext_body_omits_iv_and_hmac_rather_than_sending_null() {
        let b = MessageBody {
            content: "hello".into(),
            msg_hash: "b".repeat(64),
            is_encrypted: false,
            iv: None,
            hmac: None,
            enc_ver: 1,
            key_version: 1,
            parent_message_id: None,
            mentions: Vec::new(),
            media: Vec::new(),
        };
        let json = serde_json::to_string(&b).unwrap();
        assert!(!json.contains("\"iv\""));
        assert!(!json.contains("\"hmac\""));
    }
}
