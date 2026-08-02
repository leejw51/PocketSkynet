//! Wire types (API.md §5).
//!
//! These mirror the server's serialisation exactly, including its quirks:
//! `lastMessage` and `sender` are *absent* rather than `null` in some
//! responses, so every optional is `#[serde(default)]` and no field order is
//! assumed. Field names are camelCase throughout — the DB's snake_case column
//! names never appear on the wire.

use pocketskynet_core::{MessageId, RoomId, WalletAddress};
use serde::{Deserialize, Serialize};

/// A user profile. `publicKey`/`publicKeySig` are the E2EE identity binding;
/// both being present is the precondition for wrapping a room key to them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub wallet_address: WalletAddress,
    pub username: String,
    #[serde(default)]
    pub public_key: Option<String>,
    #[serde(default)]
    pub public_key_sig: Option<String>,
    /// The chosen avatar: `preset:<slug>` or an `/api/images/…` URL. `None`
    /// falls back to the hash-derived tile (`identity::art_for`).
    #[serde(default)]
    pub profile_image: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

impl User {
    /// What to show when there is a username; falls back to the abbreviated
    /// address, never to an empty string.
    pub fn display_name(&self) -> String {
        if self.username.trim().is_empty() {
            self.wallet_address.abbreviated()
        } else {
            self.username.clone()
        }
    }
}

/// A room, without the membership expansion.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Room {
    pub id: RoomId,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// The epoch new encrypted messages must be sealed under. Defaults to 1 for
    /// servers or rows predating the field.
    #[serde(default = "one")]
    pub current_key_version: i64,
    /// `true` ⇒ someone left or was kicked and the key has not been rotated;
    /// every encrypted post 409s until it is.
    #[serde(default)]
    pub key_rotation_pending: bool,
    #[serde(default)]
    pub created_at: Option<String>,
}

fn one() -> i64 {
    1
}

/// A room enriched with its roster. `unreadCount`/`lastReadSerial` appear only
/// on `GET /api/rooms` — nowhere else — hence the `Option`s.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomWithMembers {
    #[serde(flatten)]
    pub room: Room,
    #[serde(default)]
    pub member_count: u32,
    #[serde(default)]
    pub members: Vec<RoomMember>,
    #[serde(default)]
    pub admins: Vec<User>,
    /// Absent (not null) when the room has no renderable last message.
    #[serde(default)]
    pub last_message: Option<Message>,
    /// `true` iff **any** `room_keys` row exists for the room, for any user and
    /// any epoch. It is the only signal that a room is E2EE.
    #[serde(default)]
    pub has_encryption: bool,
    #[serde(default)]
    pub unread_count: Option<u32>,
    #[serde(default)]
    pub last_read_serial: Option<i64>,
}

impl RoomWithMembers {
    pub fn id(&self) -> &RoomId {
        &self.room.id
    }

    pub fn is_admin(&self, who: &WalletAddress) -> bool {
        self.admins.iter().any(|a| &a.wallet_address == who)
    }

    /// Sort key for the room list: newest activity first, falling back to the
    /// room's creation time so a brand-new empty room still sorts sensibly
    /// instead of to the bottom (API.md §6.5.2 returns insertion order).
    pub fn activity_ts(&self) -> i64 {
        self.last_message
            .as_ref()
            .map(|m| m.message_timestamp)
            .or_else(|| {
                self.room
                    .created_at
                    .as_deref()
                    .and_then(crate::format::parse_iso8601_ms)
            })
            .unwrap_or(0)
    }
}

/// A membership row with the user joined in.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomMember {
    #[serde(default)]
    pub id: i64,
    pub room_id: RoomId,
    pub user_address: WalletAddress,
    #[serde(default)]
    pub joined_at: Option<String>,
    pub user: User,
}

/// The six `msgType` values, plus a forward-compatible escape hatch.
///
/// An unknown type **must not** abort a sync batch (API.md §9): a server that
/// gains a seventh event type should degrade this client to ignoring it, not to
/// dropping the rest of the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgKind {
    /// `add`, and the never-written DB default `message`, which is an alias.
    Add,
    Edit,
    Delete,
    DeleteAll,
    EmoticonAdd,
    EmoticonRemove,
    Unknown,
}

impl MsgKind {
    pub fn parse(s: &str) -> Self {
        match s {
            // API.md §5.5: no code path ever writes "message", but the column
            // default is "message", so treat them identically for safety.
            "add" | "message" => MsgKind::Add,
            "edit" => MsgKind::Edit,
            "delete" => MsgKind::Delete,
            "delete_all" => MsgKind::DeleteAll,
            "emoticon_add" => MsgKind::EmoticonAdd,
            "emoticon_remove" => MsgKind::EmoticonRemove,
            _ => MsgKind::Unknown,
        }
    }

    /// Whether this event is a message the stream should render (as opposed to
    /// a reaction event or a purge marker).
    pub fn is_renderable(self) -> bool {
        matches!(self, MsgKind::Add | MsgKind::Edit)
    }
}

/// A message row, or a reaction event, or a purge marker — the server uses one
/// table and one shape for all of them, discriminated by `msgType`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: MessageId,
    pub room_id: RoomId,
    pub sender_address: WalletAddress,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub msg_hash: String,
    #[serde(default)]
    pub message_timestamp: i64,
    #[serde(default)]
    pub msg_type: String,
    #[serde(default)]
    pub msg_serial: i64,
    #[serde(default)]
    pub is_deleted: bool,
    #[serde(default)]
    pub edited_at: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub is_encrypted: bool,
    #[serde(default)]
    pub iv: Option<String>,
    #[serde(default)]
    pub hmac: Option<String>,
    /// Missing or null means 1 (CRYPTO.md OQ-3).
    #[serde(default)]
    pub enc_ver: Option<i64>,
    /// Missing or null means 1.
    #[serde(default)]
    pub key_version: Option<i64>,
    #[serde(default)]
    pub tx_hash: Option<String>,
    #[serde(default)]
    pub target_message_id: Option<MessageId>,
    #[serde(default)]
    pub emoticon_code: Option<String>,
    /// Absent on endpoints that return a bare `Message`.
    #[serde(default)]
    pub sender: Option<User>,
}

impl Message {
    pub fn kind(&self) -> MsgKind {
        MsgKind::parse(&self.msg_type)
    }

    /// `encVer ?? 1`.
    pub fn enc_ver(&self) -> i64 {
        self.enc_ver.unwrap_or(1)
    }

    /// `keyVersion ?? 1`.
    pub fn key_version(&self) -> i64 {
        self.key_version.unwrap_or(1)
    }

    /// The 8-character ledger slug shown under the bubble.
    pub fn hash_slug(&self) -> &str {
        self.msg_hash.get(..8).unwrap_or(&self.msg_hash)
    }

    pub fn is_edited(&self) -> bool {
        self.edited_at.is_some()
    }

    /// Whether this row has everything needed to attempt decryption. A message
    /// flagged encrypted but missing `iv` or `hmac` is *not* decryptable and
    /// must render as "Missing metadata" rather than as a decryption failure —
    /// the two mean different things to a user (DESIGN.md §7.3).
    pub fn has_crypto_metadata(&self) -> bool {
        self.iv.as_deref().is_some_and(|v| !v.is_empty())
            && self.hmac.as_deref().is_some_and(|v| !v.is_empty())
    }
}

/// One wrapped room key, for one member, for one epoch.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomKey {
    #[serde(default)]
    pub id: i64,
    pub room_id: RoomId,
    pub user_address: WalletAddress,
    pub encrypted_symmetric_key: String,
    pub ephemeral_public_key: String,
    /// Note the field name: the room-key IV is `encryptionIV`, not `iv`.
    #[serde(rename = "encryptionIV")]
    pub encryption_iv: String,
    pub hmac: String,
    #[serde(default = "one")]
    pub enc_ver: i64,
    #[serde(default = "one")]
    pub key_version: i64,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// The body used both by `POST /rooms/:id/keys` and, without `keyVersion`, by
/// each entry of a `/rotate-key` request (CRYPTO.md OQ-1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomKeyWrap {
    pub user_address: WalletAddress,
    pub encrypted_symmetric_key: String,
    pub ephemeral_public_key: String,
    #[serde(rename = "encryptionIV")]
    pub encryption_iv: String,
    pub hmac: String,
    pub enc_ver: i64,
    /// Omitted in rotation entries — the server forces `newVersion` there and
    /// sending a per-row value that is ignored invites confusion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_version: Option<i64>,
}

/// A block row. Note it carries addresses, not profiles.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockedUser {
    #[serde(default)]
    pub id: i64,
    pub blocker_address: WalletAddress,
    pub blocked_address: WalletAddress,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// A hidden-room row; `room` is present only on `GET /api/rooms/hidden`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HiddenRoom {
    #[serde(default)]
    pub id: i64,
    pub user_address: WalletAddress,
    pub room_id: RoomId,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub room: Option<RoomWithMembers>,
}

/// Server-side reaction aggregation. `count` can exceed `users.len()` when a
/// reactor has no `users` row — API.md §5.8 says to trust `count`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmoticonAggregation {
    pub emoticon_code: String,
    pub count: u32,
    #[serde(default)]
    pub users: Vec<User>,
}

/// A pending invitation as returned by `GET /api/invitations`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Invitation {
    pub room_id: RoomId,
    pub room_name: String,
    pub invited_by: WalletAddress,
    #[serde(default)]
    pub inviter_username: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

impl Invitation {
    pub fn inviter_name(&self) -> String {
        self.inviter_username
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| self.invited_by.abbreviated())
    }
}

/// One entry of `POST /api/users/public-keys`. A `null` `publicKeySig` means
/// **unverifiable** — CRYPTO.md §4.3 requires refusing to wrap in that case.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicKeyEntry {
    pub wallet_address: WalletAddress,
    pub public_key: String,
    #[serde(default)]
    pub public_key_sig: Option<String>,
}

/// `GET /api/blockchain/info`. Every value is a string read from the server's
/// environment; missing vars come back as `""`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockchainInfo {
    /// Decimal, as a string, because that is what the server sends.
    #[serde(default)]
    pub chain_id: String,
    /// The Privy app id, or empty when Privy sign-in is not configured.
    ///
    /// Served rather than compiled in: this client is a shipped `.wasm` and
    /// cannot bake a per-deployment value the way the reference client's Vite
    /// build does. Empty is the feature flag.
    #[serde(default)]
    pub privy_app_id: String,
    /// This server generated its own certificate and offers the CA at
    /// `/ca.crt`. Installing it is what makes MetaMask's in-app browser — which
    /// offers no way past a certificate warning — able to open the app at all.
    #[serde(default)]
    pub ca_cert_available: bool,
    #[serde(default)]
    pub chain_rpc: String,
    #[serde(default)]
    pub chain_name: String,
    #[serde(default)]
    pub chain_explorer: String,
    #[serde(default)]
    pub fruitnation_hash_cro: String,
    #[serde(default)]
    pub fruitnation_wallet: String,
    /// Price of a shout in CRO, decimal string. Served so the number on the
    /// pay button is the number the server enforces. Empty on older servers,
    /// which the dialog treats as "feature unavailable".
    #[serde(default)]
    pub shout_price_cro: String,
    /// Price of hosting a published site, in CRO.
    #[serde(default)]
    pub publish_price_cro: String,
}

/// One address this server answers on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerEndpoint {
    pub url: String,
    /// `local`, `network` or `vpn` — how far away a client has to be.
    pub reach: String,
}

/// The addresses, grouped by transport.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerEndpoints {
    #[serde(default)]
    pub tcp: Vec<ServerEndpoint>,
    #[serde(default)]
    pub http3: Vec<ServerEndpoint>,
}

/// `GET /api/server/info` — where this server is, and how you got here.
///
/// The `protocol` field is the point: a browser moves to HTTP/3 on its own
/// once it has seen `Alt-Svc`, and the page has no way to know that from the
/// inside. Only the end that terminated the connection can say.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    /// What carried *this* request: `h3`, `h2`, `http/1.1`.
    #[serde(default)]
    pub protocol: String,
    #[serde(default)]
    pub scheme: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub http3_port: Option<u16>,
    #[serde(default)]
    pub http3_available: bool,
    #[serde(default)]
    pub endpoints: ServerEndpoints,
    #[serde(default)]
    pub uptime: u64,
    /// Realtime lives only on TCP — there is no WebSocket over HTTP/3.
    #[serde(default)]
    pub websocket_transport: String,
    /// There is a CA at `/ca.crt` to install. Relevant to HTTP/3 specifically:
    /// a browser will not speak QUIC to a certificate it does not genuinely
    /// trust, and unlike TLS-over-TCP there is no click-through.
    #[serde(default)]
    pub ca_cert_available: bool,
}

impl ServerInfo {
    /// Whether this page is talking QUIC right now.
    pub fn is_http3(&self) -> bool {
        self.protocol.starts_with("h3")
    }

    /// HTTP/3 is being served but this page is not using it.
    ///
    /// The interesting state, and the one that needs explaining: the panel
    /// lists QUIC endpoints the reader is demonstrably not on.
    pub fn http3_offered_but_unused(&self) -> bool {
        self.http3_available && !self.is_http3()
    }

    /// The protocol in the form people recognise.
    pub fn protocol_label(&self) -> &str {
        match self.protocol.as_str() {
            "h3" => "HTTP/3",
            "h2" => "HTTP/2",
            "http/1.1" => "HTTP/1.1",
            "http/1.0" => "HTTP/1.0",
            other if other.is_empty() => "unknown",
            other => other,
        }
    }
}

impl BlockchainInfo {
    /// The chain id as a number, or `None` when the server did not say.
    ///
    /// Used to ask a browser wallet to switch networks at sign-in. `None` means
    /// "do not ask" rather than "chain 0" — an unconfigured server should not
    /// send someone's wallet chasing a network.
    pub fn chain_id_num(&self) -> Option<u64> {
        self.chain_id.trim().parse().ok().filter(|n| *n > 0)
    }

    /// The testnet ribbon is shown whenever the chain name says "testnet"
    /// (DESIGN.md §4). It is a fact about the environment, not a notification.
    pub fn is_testnet(&self) -> bool {
        self.chain_name.to_ascii_lowercase().contains("testnet")
    }

    /// Explorer URL for a transaction, when an explorer is configured.
    pub fn tx_url(&self, tx_hash: &str) -> Option<String> {
        if self.chain_explorer.is_empty() {
            return None;
        }
        Some(format!(
            "{}/tx/{}",
            self.chain_explorer.trim_end_matches('/'),
            tx_hash
        ))
    }
}

/// `POST /api/auth/challenge`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Challenge {
    pub challenge_id: String,
    /// Sign these bytes **verbatim** — never reconstruct the message locally.
    pub message: String,
    #[serde(default)]
    pub expires_at: Option<String>,
}

/// `POST /api/auth/login`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginResponse {
    pub user: User,
    pub token: String,
    #[serde(default)]
    pub fruitnation_wallet: String,
    /// The per-account E2EE derivation salt. A **secret**: it is served only to
    /// its owner, because a public salt would let a hostile page reconstruct
    /// the derivation message and phish the signature that *is* the private key.
    #[serde(default)]
    pub encryption_salt: Option<String>,
}

/// `GET /api/rooms/:id/latest-serial`.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct SerialResponse {
    #[serde(default)]
    pub serial: i64,
}

/// `POST /api/rooms/:id/read`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadResponse {
    pub room_id: RoomId,
    pub last_read_serial: i64,
}

/// `GET /api/auth/encryption-salt`.
#[derive(Debug, Clone, Deserialize)]
pub struct SaltResponse {
    pub salt: String,
}

/// One page of `/sync`, plus the `X-Has-More` header the drain loop needs.
#[derive(Debug, Clone)]
pub struct SyncPage {
    pub events: Vec<Message>,
    pub has_more: bool,
}

/// An attachment (`docs/API.md` §14).
///
/// `tags` arrives already extracted from the caption, so the drawer renders
/// chips without re-running the hashtag rule — the server is the authority on
/// what a tag is, and two implementations would eventually disagree.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileMeta {
    pub id: String,
    pub room_id: RoomId,
    pub uploader: WalletAddress,
    pub filename: String,
    pub mime: String,
    pub size_bytes: i64,
    #[serde(default)]
    pub caption: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Authenticated — fetch it with the token, never as a bare `href`.
    pub url: String,
    pub created_at: String,
}

impl FileMeta {
    /// The lowercase extension, for the type badge. Empty when there is none.
    pub fn extension(&self) -> String {
        self.filename
            .rsplit_once('.')
            .map(|(_, e)| e.to_lowercase())
            .filter(|e| !e.is_empty() && e.chars().all(char::is_alphanumeric) && e.len() <= 5)
            .unwrap_or_default()
    }

    /// Whether a preview is worth attempting. Extension-based on purpose: the
    /// server stores every attachment as octet-stream, so the declared mime is
    /// not evidence of anything, and this only decides whether to *try*.
    ///
    /// `svg` is deliberately absent: it is markup and can carry script, and an
    /// `<img>` is not the place to find that out.
    pub fn is_previewable_image(&self) -> bool {
        matches!(
            self.extension().as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "bmp"
        )
    }

    pub fn is_previewable_video(&self) -> bool {
        matches!(self.extension().as_str(), "mp4" | "webm" | "m4v" | "ogv")
    }

    /// The mime to label a preview blob with.
    ///
    /// Needed because the server reports `application/octet-stream` for
    /// everything by design, and a `<video>` handed that plays nothing. Only
    /// reached after `is_previewable_*` has already vouched for the extension.
    pub fn preview_mime(&self) -> &'static str {
        match self.extension().as_str() {
            "png" => "image/png",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "avif" => "image/avif",
            "bmp" => "image/bmp",
            "jpg" | "jpeg" => "image/jpeg",
            "mp4" | "m4v" => "video/mp4",
            "webm" => "video/webm",
            "ogv" => "video/ogg",
            _ => "application/octet-stream",
        }
    }

    /// `1.4 MB`. Binary units would read as wrong to anyone comparing against
    /// their file manager, which shows decimal on both macOS and Windows.
    pub fn human_size(&self) -> String {
        let bytes = self.size_bytes.max(0) as f64;
        const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
        let mut value = bytes;
        let mut unit = 0;
        while value >= 1000.0 && unit < UNITS.len() - 1 {
            value /= 1000.0;
            unit += 1;
        }
        if unit == 0 {
            format!("{} {}", value as i64, UNITS[unit])
        } else {
            format!("{value:.1} {}", UNITS[unit])
        }
    }
}

/// The id inside an attachment path, or `None` if this is not one.
///
/// The shape is `/api/files/{id}/raw`, which is what `FileMeta::url` carries and
/// therefore what a message body quotes. Validating the id charset here is what
/// keeps a hostile message body from turning into an arbitrary request: a token
/// that is *almost* an attachment path renders as plain text instead.
pub fn attachment_id_in(token: &str) -> Option<&str> {
    let rest = token.strip_prefix("/api/files/")?;
    let id = rest.strip_suffix("/raw")?;
    let ok = !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
    ok.then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, size: i64) -> FileMeta {
        FileMeta {
            id: "file_1_a".into(),
            room_id: RoomId::new("room_00000001").unwrap(),
            uploader: WalletAddress::new("0x0000000000000000000000000000000000000001").unwrap(),
            filename: name.into(),
            mime: "application/octet-stream".into(),
            size_bytes: size,
            caption: String::new(),
            tags: vec![],
            url: "/api/files/file_1_a/raw".into(),
            created_at: "2026-01-01T00:00:00.000Z".into(),
        }
    }

    #[test]
    fn an_extension_is_only_taken_when_it_looks_like_one() {
        assert_eq!(file("report.pdf", 1).extension(), "pdf");
        assert_eq!(file("ARCHIVE.TAR.GZ", 1).extension(), "gz");
        assert_eq!(file("Makefile", 1).extension(), "");
        assert_eq!(file("trailing.", 1).extension(), "");
        // Not an extension: too long, or not alphanumeric.
        assert_eq!(file("a.verylongext", 1).extension(), "");
        assert_eq!(file("a.p df", 1).extension(), "");
    }

    #[test]
    fn only_real_image_extensions_are_worth_previewing() {
        for name in ["a.png", "a.JPG", "a.jpeg", "a.gif", "a.webp"] {
            assert!(file(name, 1).is_previewable_image(), "{name}");
        }
        // An .html or .svg must never be previewed: the first is markup and the
        // second carries script, and neither is what an <img> should be handed.
        for name in ["a.pdf", "a.html", "a.svg", "a.zip", "Makefile"] {
            assert!(!file(name, 1).is_previewable_image(), "{name}");
        }
    }

    #[test]
    fn a_chain_id_is_only_offered_to_a_wallet_when_the_server_gave_one() {
        let info = |id: &str| BlockchainInfo {
            chain_id: id.into(),
            ..Default::default()
        };
        assert_eq!(info("25").chain_id_num(), Some(25));
        assert_eq!(info("338").chain_id_num(), Some(338));
        assert_eq!(info(" 25 ").chain_id_num(), Some(25));
        // None means "do not ask the wallet to switch". An unconfigured server
        // must not send someone's wallet chasing chain 0.
        assert_eq!(info("").chain_id_num(), None);
        assert_eq!(info("0").chain_id_num(), None);
        assert_eq!(info("mainnet").chain_id_num(), None);
        assert_eq!(
            info("0x19").chain_id_num(),
            None,
            "the server sends decimal"
        );
    }

    #[test]
    fn videos_are_previewable_and_carry_a_playable_mime() {
        for name in ["a.mp4", "a.MP4", "a.webm", "a.m4v", "a.ogv"] {
            assert!(file(name, 1).is_previewable_video(), "{name}");
            assert!(file(name, 1).preview_mime().starts_with("video/"), "{name}");
        }
        // A <video> handed application/octet-stream plays nothing, which is why
        // preview_mime exists at all.
        assert_eq!(file("a.mp4", 1).preview_mime(), "video/mp4");
        assert_eq!(file("a.webm", 1).preview_mime(), "video/webm");
        // Not video, and must not be mistaken for it.
        for name in ["a.pdf", "a.png", "a.mkv", "a.mov", "Makefile"] {
            assert!(!file(name, 1).is_previewable_video(), "{name}");
        }
        // The two predicates never both claim the same file.
        for name in ["a.mp4", "a.png", "a.pdf"] {
            let f = file(name, 1);
            assert!(
                !(f.is_previewable_image() && f.is_previewable_video()),
                "{name}"
            );
        }
    }

    #[test]
    fn an_attachment_path_is_recognised_only_in_its_exact_shape() {
        assert_eq!(
            attachment_id_in("/api/files/file_123_abc-DEF/raw"),
            Some("file_123_abc-DEF")
        );
        // Anything that is not exactly the shape renders as plain text. These
        // are the ones that matter: a hostile message body must not be able to
        // steer the embed at another path.
        for hostile in [
            "/api/files//raw",
            "/api/files/../../etc/raw",
            "/api/files/a/b/raw",
            "/api/files/a%2Fb/raw",
            "/api/files/abc",
            "/api/files/abc/raw/extra",
            "https://evil.example/api/files/abc/raw",
            "/api/images/abc/raw",
            "api/files/abc/raw",
            "",
        ] {
            assert_eq!(attachment_id_in(hostile), None, "accepted {hostile:?}");
        }
        // Bounded, so a megabyte-long token cannot become a request.
        let long = format!("/api/files/{}/raw", "a".repeat(200));
        assert_eq!(attachment_id_in(&long), None);
    }

    #[test]
    fn sizes_read_the_way_a_file_manager_shows_them() {
        assert_eq!(file("a", 0).human_size(), "0 B");
        assert_eq!(file("a", 999).human_size(), "999 B");
        assert_eq!(file("a", 1_000).human_size(), "1.0 KB");
        assert_eq!(file("a", 1_400_000).human_size(), "1.4 MB");
        assert_eq!(file("a", 25 * 1_000_000).human_size(), "25.0 MB");
        // Never panics or shows a negative, whatever the server sent.
        assert_eq!(file("a", -5).human_size(), "0 B");
    }

    #[test]
    fn message_decodes_the_full_documented_shape() {
        let json = r#"{
            "id":"msg_1749652746620_4cfe1c4c",
            "roomId":"room_1749652739650_304e0eaf",
            "senderAddress":"0x742d35CC6634C0532925a3b8D31cE5bb1C6E6B22",
            "content":"Hello everyone!",
            "msgHash":"b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
            "messageTimestamp":1749652746620,
            "msgType":"add",
            "msgSerial":1749652746620,
            "isDeleted":false,
            "editedAt":null,
            "createdAt":"2025-06-11T14:39:06.000Z",
            "isEncrypted":false,
            "iv":null,"hmac":null,"encVer":1,"keyVersion":1,
            "txHash":null,"targetMessageId":null,"emoticonCode":null
        }"#;
        let m: Message = serde_json::from_str(json).unwrap();
        assert_eq!(m.kind(), MsgKind::Add);
        // The address newtype lowercases on deserialisation — the invariant the
        // whole protocol depends on.
        assert_eq!(
            m.sender_address.as_str(),
            "0x742d35cc6634c0532925a3b8d31ce5bb1c6e6b22"
        );
        assert_eq!(m.hash_slug(), "b94d27b9");
        assert!(!m.is_edited());
        assert!(m.sender.is_none());
    }

    #[test]
    fn absent_optional_fields_decode_without_a_null() {
        // The server omits `undefined` keys entirely (API.md §1.4), so a
        // minimal row must still decode.
        let m: Message = serde_json::from_str(
            r#"{"id":"msg_1_aaaaa","roomId":"room_00000001","senderAddress":"0x0000000000000000000000000000000000000001"}"#,
        )
        .unwrap();
        assert_eq!(m.enc_ver(), 1);
        assert_eq!(m.key_version(), 1);
        assert_eq!(m.kind(), MsgKind::Unknown);
        assert!(!m.has_crypto_metadata());
    }

    #[test]
    fn msg_kind_aliases_the_never_written_db_default() {
        assert_eq!(MsgKind::parse("message"), MsgKind::Add);
        assert_eq!(MsgKind::parse("add"), MsgKind::Add);
        assert_eq!(MsgKind::parse("edit"), MsgKind::Edit);
        assert_eq!(MsgKind::parse("delete"), MsgKind::Delete);
        assert_eq!(MsgKind::parse("delete_all"), MsgKind::DeleteAll);
        assert_eq!(MsgKind::parse("emoticon_add"), MsgKind::EmoticonAdd);
        assert_eq!(MsgKind::parse("emoticon_remove"), MsgKind::EmoticonRemove);
        // Forward compatibility: an unrecognised type is ignorable, not fatal.
        assert_eq!(MsgKind::parse("teleport"), MsgKind::Unknown);
        assert!(!MsgKind::Unknown.is_renderable());
        assert!(MsgKind::Add.is_renderable());
        assert!(MsgKind::Edit.is_renderable());
        assert!(!MsgKind::Delete.is_renderable());
    }

    #[test]
    fn room_with_members_flattens_the_room_fields() {
        let json = r#"{
            "id":"room_1749652739650_304e0eaf","name":"Team chat","description":null,
            "currentKeyVersion":3,"keyRotationPending":true,
            "createdAt":"2025-06-11T14:38:59.000Z",
            "memberCount":2,"members":[],"admins":[],
            "hasEncryption":true,"unreadCount":4,"lastReadSerial":1749652746620
        }"#;
        let r: RoomWithMembers = serde_json::from_str(json).unwrap();
        assert_eq!(r.room.name, "Team chat");
        assert_eq!(r.room.current_key_version, 3);
        assert!(r.room.key_rotation_pending);
        assert_eq!(r.unread_count, Some(4));
        assert!(r.last_message.is_none());
    }

    #[test]
    fn room_missing_the_epoch_fields_defaults_to_epoch_one() {
        let r: Room = serde_json::from_str(r#"{"id":"room_00000001","name":"x"}"#).unwrap();
        assert_eq!(r.current_key_version, 1);
        assert!(!r.key_rotation_pending);
    }

    #[test]
    fn room_key_uses_the_capitalised_iv_field_name() {
        // `encryptionIV`, not `encryptionIv` — serde's camelCase rename would
        // get this wrong, which is why it is spelled out explicitly.
        let k: RoomKey = serde_json::from_str(
            r#"{"roomId":"room_00000001","userAddress":"0x0000000000000000000000000000000000000001",
                "encryptedSymmetricKey":"AAA","ephemeralPublicKey":"04ab",
                "encryptionIV":"1a2b3c4d5e6f7890abcdef1234567890","hmac":"ff","encVer":2,"keyVersion":3}"#,
        )
        .unwrap();
        assert_eq!(k.encryption_iv, "1a2b3c4d5e6f7890abcdef1234567890");
        assert_eq!(k.key_version, 3);
    }

    #[test]
    fn room_key_wrap_omits_key_version_for_rotation_entries() {
        let w = RoomKeyWrap {
            user_address: WalletAddress::new("0x0000000000000000000000000000000000000001").unwrap(),
            encrypted_symmetric_key: "ct".into(),
            ephemeral_public_key: "04ab".into(),
            encryption_iv: "iv".into(),
            hmac: "mac".into(),
            enc_ver: 2,
            key_version: None,
        };
        let json = serde_json::to_string(&w).unwrap();
        assert!(
            !json.contains("keyVersion"),
            "rotation entries must omit it"
        );
        assert!(json.contains("\"encryptionIV\""));

        let w2 = RoomKeyWrap {
            key_version: Some(1),
            ..w
        };
        assert!(serde_json::to_string(&w2)
            .unwrap()
            .contains("\"keyVersion\":1"));
    }

    #[test]
    fn testnet_detection_is_case_insensitive() {
        let mut info = BlockchainInfo {
            chain_name: "Cronos Testnet".into(),
            chain_explorer: "https://explorer.cronos.org/testnet/".into(),
            ..Default::default()
        };
        assert!(info.is_testnet());
        info.chain_name = "CRONOS TESTNET".into();
        assert!(info.is_testnet());
        info.chain_name = "Cronos".into();
        assert!(!info.is_testnet());
        assert_eq!(
            info.tx_url("0xabc").unwrap(),
            "https://explorer.cronos.org/testnet/tx/0xabc"
        );
        info.chain_explorer = String::new();
        assert!(info.tx_url("0xabc").is_none());
    }

    #[test]
    fn display_name_never_renders_as_empty() {
        let u = User {
            wallet_address: WalletAddress::new("0x742d35cc6634c0532925a3b8d31ce5bb1c6e6b22")
                .unwrap(),
            username: "   ".into(),
            public_key: None,
            public_key_sig: None,
            profile_image: None,
            created_at: None,
            updated_at: None,
        };
        assert_eq!(u.display_name(), "0x742d…6b22");
    }

    #[test]
    fn hash_slug_is_safe_on_a_scrubbed_delete_row() {
        let m: Message = serde_json::from_str(
            r#"{"id":"msg_1_aaaaa","roomId":"room_00000001","senderAddress":"0x0000000000000000000000000000000000000001","msgHash":"","msgType":"delete","isDeleted":true}"#,
        )
        .unwrap();
        assert_eq!(m.hash_slug(), "");
        assert_eq!(m.kind(), MsgKind::Delete);
    }
}
