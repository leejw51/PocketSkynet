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

#[cfg(test)]
mod tests {
    //! Deserialization pinned to the wire shapes in API.md §1.4/§5:
    //! camelCase keys, `null` for nullable-and-unset, keys *omitted* entirely
    //! for `undefined`, and new server-side fields ignored by old clients.

    use super::*;

    #[test]
    fn a_full_user_round_trips() {
        let user: User = serde_json::from_str(
            r#"{
              "walletAddress": "0x742d35cc6634c0532925a3b8d31ce5bb1c6e6b22",
              "username": "alice",
              "publicKey": "04f3579e",
              "publicKeySig": "0xe98d1b",
              "profileImage": "preset:tp-coder-f",
              "createdAt": "2025-06-11T14:39:06.000Z",
              "updatedAt": "2025-06-11T14:39:06.000Z"
            }"#,
        )
        .unwrap();
        assert_eq!(
            user.wallet_address,
            "0x742d35cc6634c0532925a3b8d31ce5bb1c6e6b22"
        );
        assert_eq!(user.username, "alice");
        assert_eq!(user.public_key.as_deref(), Some("04f3579e"));
        assert_eq!(user.profile_image.as_deref(), Some("preset:tp-coder-f"));
    }

    #[test]
    fn a_user_with_nulls_and_missing_keys_still_parses() {
        // `publicKey: null` (nullable, unset) and `publicKeySig` omitted
        // entirely — API.md documents both spellings and a client must accept
        // either (the synthesized-sender QUIRK in §5.1 emits both).
        let user: User = serde_json::from_str(
            r#"{"walletAddress": "0xabc0000000000000000000000000000000000001",
                "username": "User 0xabc0...0001",
                "publicKey": null}"#,
        )
        .unwrap();
        assert_eq!(user.public_key, None);
        assert_eq!(user.public_key_sig, None);
        assert_eq!(user.created_at, None);
    }

    #[test]
    fn unknown_fields_are_ignored_not_fatal() {
        // The server adds fields over time; an old client keeps working.
        let user: User = serde_json::from_str(
            r#"{"walletAddress": "0xabc0000000000000000000000000000000000001",
                "username": "bob",
                "someFutureField": {"nested": [1, 2, 3]}}"#,
        )
        .unwrap();
        assert_eq!(user.username, "bob");
    }

    #[test]
    fn a_bare_room_and_an_enriched_room_both_parse() {
        // POST /api/rooms returns a bare Room; GET /api/rooms enriches it.
        let bare: Room = serde_json::from_str(
            r#"{"id": "room_1_a", "name": "Team chat", "description": null,
                "currentKeyVersion": 1, "keyRotationPending": false,
                "createdAt": "2025-06-11T14:38:59.000Z", "kind": "channel"}"#,
        )
        .unwrap();
        assert_eq!(bare.name.as_deref(), Some("Team chat"));
        assert_eq!(bare.description, None);
        assert_eq!(bare.member_count, None, "enrichment absent on a bare Room");
        assert_eq!(bare.unread_count, None);

        let enriched: Room = serde_json::from_str(
            r#"{"id": "room_1_a", "name": null, "kind": "dm",
                "memberCount": 2, "hasEncryption": true,
                "unreadCount": 4, "lastReadSerial": 1749652746620,
                "lastMessage": {
                    "id": "msg_1_b", "roomId": "room_1_a",
                    "senderAddress": "0xabc0000000000000000000000000000000000001",
                    "content": "hi"
                }}"#,
        )
        .unwrap();
        assert_eq!(enriched.name, None, "a DM has no name of its own");
        assert_eq!(enriched.member_count, Some(2));
        assert_eq!(enriched.unread_count, Some(4));
        assert_eq!(enriched.last_read_serial, Some(1_749_652_746_620));
        assert_eq!(enriched.last_message.unwrap().content, "hi");
    }

    #[test]
    fn a_message_parses_with_and_without_its_sender() {
        // §5.5: bare Message endpoints never include `sender`; the POST and
        // sync paths attach it.
        let full: Message = serde_json::from_str(
            r#"{
              "id": "msg_1749652746620_4cfe",
              "roomId": "room_1_a",
              "senderAddress": "0x742d35cc6634c0532925a3b8d31ce5bb1c6e6b22",
              "content": "Hello everyone!",
              "msgHash": "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
              "messageTimestamp": 1749652746620,
              "msgType": "add",
              "msgSerial": 1749652746620,
              "isDeleted": false,
              "editedAt": null,
              "createdAt": "2025-06-11T14:39:06.000Z",
              "isEncrypted": false,
              "iv": null,
              "hmac": null,
              "encVer": 1,
              "keyVersion": 1,
              "txHash": null,
              "sender": {
                "walletAddress": "0x742d35cc6634c0532925a3b8d31ce5bb1c6e6b22",
                "username": "alice"
              }
            }"#,
        )
        .unwrap();
        assert_eq!(full.msg_type.as_deref(), Some("add"));
        assert_eq!(full.msg_serial, Some(1_749_652_746_620));
        assert_eq!(full.sender.unwrap().username, "alice");

        let bare: Message = serde_json::from_str(
            r#"{"id": "m", "roomId": "r",
                "senderAddress": "0xabc0000000000000000000000000000000000001",
                "content": "x"}"#,
        )
        .unwrap();
        assert!(bare.sender.is_none());
        assert_eq!(bare.message_timestamp, None);
    }

    #[test]
    fn login_and_challenge_responses_parse() {
        let challenge: ChallengeResponse = serde_json::from_str(
            r#"{"challengeId": "6f1e2c30-1111-2222-3333-444455556666",
                "message": "Welcome to FruitNation!\n\n…",
                "expiresAt": "2025-06-11T14:49:06.000Z"}"#,
        )
        .unwrap();
        assert!(challenge.message.starts_with("Welcome to FruitNation!"));

        // And without the optional expiry.
        let minimal: ChallengeResponse =
            serde_json::from_str(r#"{"challengeId": "id", "message": "m"}"#).unwrap();
        assert_eq!(minimal.expires_at, None);

        let login: LoginResponse = serde_json::from_str(
            r#"{"user": {"walletAddress": "0xabc0000000000000000000000000000000000001",
                         "username": "alice"},
                "token": "eyJ.x.y",
                "fruitnationWallet": "0xF39fd6",
                "encryptionSalt": "ab"}"#,
        )
        .unwrap();
        assert_eq!(login.token, "eyJ.x.y");
        assert_eq!(login.encryption_salt.as_deref(), Some("ab"));
    }

    #[test]
    fn health_uses_status_not_message() {
        let health: HealthResponse =
            serde_json::from_str(r#"{"status": "ok", "uptime": 12345}"#).unwrap();
        assert_eq!(health.status, "ok");
        assert_eq!(health.uptime, Some(12345));

        // The 503 body carries no uptime at all.
        let down: HealthResponse = serde_json::from_str(r#"{"status": "unavailable"}"#).unwrap();
        assert_eq!(down.uptime, None);
    }

    #[test]
    fn all_three_error_envelope_shapes_parse() {
        // §1.5 shape 1: message only.
        let plain: ErrorEnvelope = serde_json::from_str(r#"{"message": "Access denied"}"#).unwrap();
        assert_eq!(plain.message.as_deref(), Some("Access denied"));
        assert_eq!(plain.code, None);
        assert_eq!(plain.errors, None);

        // Shape 2: the Zod validation envelope.
        let validation: ErrorEnvelope = serde_json::from_str(
            r#"{"message": "Validation failed",
                "errors": ["roomId: Room ID contains invalid characters"]}"#,
        )
        .unwrap();
        assert_eq!(
            validation.errors.unwrap(),
            vec!["roomId: Room ID contains invalid characters"]
        );

        // Shape 3: machine-readable code (the 409 key conflicts).
        let conflict: ErrorEnvelope = serde_json::from_str(
            r#"{"code": "KEY_ROTATION_REQUIRED", "message": "…", "currentKeyVersion": 3}"#,
        )
        .unwrap();
        assert_eq!(conflict.code.as_deref(), Some("KEY_ROTATION_REQUIRED"));
    }

    #[test]
    fn a_non_envelope_body_fails_to_a_decode_error() {
        // What Client::call falls back to string-quoting; the typed parse
        // must fail cleanly, not panic.
        assert!(serde_json::from_str::<HealthResponse>("<html>bad gateway</html>").is_err());
        let err: crate::error::ClientError = serde_json::from_str::<HealthResponse>("not json")
            .unwrap_err()
            .into();
        assert!(matches!(err, crate::error::ClientError::Decode(_)));
    }
}
