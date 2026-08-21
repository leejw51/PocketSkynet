//! Wire types, exactly as `app/docs/API.md` §5 serializes them.
//!
//! Every field name on the wire is camelCase; the structs say so once via
//! `serde(rename_all)` rather than per-field, so a new field cannot silently
//! ship in snake_case. Deserialization is deliberately lenient (`default` on
//! optional fields, unknown fields ignored) — the server adds fields over
//! time and an old client must keep working.

use serde::{Deserialize, Serialize};

// --- requests --------------------------------------------------------------

/// `POST /api/auth/challenge`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChallengeRequest {
    pub wallet_address: String,
}

/// `POST /api/auth/login`.
///
/// `username` is skipped when `None`: the server reuses the stored username
/// for a returning account, and sending an explicit value would overwrite it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginRequest {
    pub wallet_address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub challenge_id: String,
    pub signature: String,
}

/// `POST /api/rooms`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateRoomRequest {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// `POST /api/rooms/{roomId}/messages` — plaintext only (E2EE is out of
/// scope for this client). `msg_hash` is required by the server and must be
/// the lowercase-hex SHA-256 of `content` exactly as sent.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageRequest {
    pub content: String,
    pub msg_hash: String,
    pub is_encrypted: bool,
}

// --- responses -------------------------------------------------------------

/// `POST /api/auth/challenge` → 200.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChallengeResponse {
    pub challenge_id: String,
    /// The exact string to sign. Verbatim — never rebuilt locally.
    pub message: String,
    #[serde(default)]
    pub expires_at: Option<String>,
}

/// `POST /api/auth/login` → 200.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginResponse {
    pub user: User,
    pub token: String,
    #[serde(default)]
    pub fruitnation_wallet: Option<String>,
    /// Served only to the authenticated owner; needed for E2EE key
    /// derivation, which this client does not perform. Kept so a caller can.
    #[serde(default)]
    pub encryption_salt: Option<String>,
}

/// API.md §5.1.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub wallet_address: String,
    pub username: String,
    #[serde(default)]
    pub public_key: Option<String>,
    #[serde(default)]
    pub public_key_sig: Option<String>,
    #[serde(default)]
    pub profile_image: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// API.md §5.2/§5.3 — a room, with the enrichment fields `GET /api/rooms`
/// adds. `name` is optional because a DM has no name of its own.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Room {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub current_key_version: Option<i64>,
    #[serde(default)]
    pub key_rotation_pending: Option<bool>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub member_count: Option<i64>,
    #[serde(default)]
    pub has_encryption: Option<bool>,
    #[serde(default)]
    pub unread_count: Option<i64>,
    #[serde(default)]
    pub last_read_serial: Option<i64>,
    #[serde(default)]
    pub last_message: Option<Message>,
}

/// API.md §5.5 — a message, with the optional `sender` enrichment.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    pub room_id: String,
    pub sender_address: String,
    pub content: String,
    #[serde(default)]
    pub msg_hash: Option<String>,
    #[serde(default)]
    pub message_timestamp: Option<i64>,
    #[serde(default)]
    pub msg_type: Option<String>,
    #[serde(default)]
    pub msg_serial: Option<i64>,
    #[serde(default)]
    pub is_deleted: Option<bool>,
    #[serde(default)]
    pub is_encrypted: Option<bool>,
    #[serde(default)]
    pub enc_ver: Option<i64>,
    #[serde(default)]
    pub key_version: Option<i64>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub sender: Option<User>,
}

/// `GET /api/health` → 200. Note the key is `status`, not `message`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthResponse {
    pub status: String,
    #[serde(default)]
    pub uptime: Option<i64>,
}

/// The error envelope (§1.5): always a `message`, sometimes a machine
/// readable `code` (the two 409 key-conflict responses), sometimes a Zod
/// style `errors` array.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorEnvelope {
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub errors: Option<Vec<String>>,
}
