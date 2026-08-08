//! Wire shapes, exactly as `docs/API.md` §5 specifies them.
//!
//! Field names are camelCase because they are TypeScript property names in the
//! reference server, never the snake_case column names. `serde(rename)` is
//! spelled out per field rather than using a container-level
//! `rename_all = "camelCase"` so that a column rename cannot silently change
//! the wire contract.
//!
//! Two serialisation rules carry meaning:
//!
//! * `Option::is_none` → **omitted**, not `null`. JavaScript's
//!   `JSON.stringify` drops `undefined` keys, and clients distinguish "absent"
//!   from "null" for `lastMessage` and `sender`.
//! * Nullable *columns* (`publicKey`, `iv`, `txHash`, …) always serialise,
//!   as `null` when unset. They are `Option` in Rust but not `skip`ped.

use rusqlite::Row;
use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Format epoch milliseconds as `2025-06-11T14:39:06.000Z`.
///
/// The reference emits `Date.toISOString()`, which is always UTC with exactly
/// three fractional digits and a `Z` suffix. `time`'s RFC 3339 formatter emits
/// a variable number of digits and `+00:00`, so the tail is rewritten here
/// rather than letting clients meet two different shapes.
pub fn iso_ms(ms: i64) -> String {
    let dt = OffsetDateTime::from_unix_timestamp_nanos((ms as i128) * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let secs = dt.replace_nanosecond(0).unwrap_or(dt);
    let base = secs.format(&Rfc3339).unwrap_or_default();
    let base = base.trim_end_matches("+00:00").trim_end_matches('Z');
    let millis = ms.rem_euclid(1000);
    format!("{base}.{millis:03}Z")
}

fn iso_opt(ms: Option<i64>) -> Option<String> {
    ms.map(iso_ms)
}

/// A user profile. `encryptionSalt` is deliberately absent: it lives in its
/// own table and is served only to its owner.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct User {
    #[serde(rename = "walletAddress")]
    pub wallet_address: String,
    pub username: String,
    #[serde(rename = "publicKey")]
    pub public_key: Option<String>,
    #[serde(rename = "publicKeySig")]
    pub public_key_sig: Option<String>,
    /// `preset:<name>` or an `/api/images/…` URL; `None` lets clients fall
    /// back to the hash-derived avatar.
    #[serde(rename = "profileImage")]
    pub profile_image: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
}

impl User {
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            wallet_address: row.get("wallet_address")?,
            username: row.get("username")?,
            public_key: row.get("public_key")?,
            public_key_sig: row.get("public_key_sig")?,
            profile_image: row.get("profile_image")?,
            created_at: iso_ms(row.get("created_at")?),
            updated_at: iso_ms(row.get("updated_at")?),
        })
    }

    /// The profile substituted when a message's sender has no `users` row.
    ///
    /// The reference builds this in two places and one of them omits
    /// `publicKeySig` entirely; §15 #18 asks for `null` in both, so there is
    /// exactly one shape here.
    pub fn placeholder(address: &str, now_ms: i64) -> Self {
        let short = if address.len() >= 10 {
            format!("User {}...{}", &address[..6], &address[address.len() - 4..])
        } else {
            format!("User {address}")
        };
        Self {
            wallet_address: address.to_owned(),
            username: short,
            public_key: None,
            public_key_sig: None,
            profile_image: None,
            created_at: iso_ms(now_ms),
            updated_at: iso_ms(now_ms),
        }
    }
}

/// A user's published encryption key, as returned by `POST /api/users/public-keys`.
#[derive(Debug, Clone, Serialize)]
pub struct PublicKeyEntry {
    #[serde(rename = "walletAddress")]
    pub wallet_address: String,
    #[serde(rename = "publicKey")]
    pub public_key: String,
    #[serde(rename = "publicKeySig")]
    pub public_key_sig: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Room {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "currentKeyVersion")]
    pub current_key_version: i64,
    #[serde(rename = "keyRotationPending")]
    pub key_rotation_pending: bool,
    /// `"channel"`, `"dm"`, `"group_dm"`, `"note"`, `"jarvis"` or `"lobby"` —
    /// see [`ROOM_KIND_CHANNEL`] and friends. Always sent, never omitted: a
    /// client that files rooms into a channel list and a DM list needs an
    /// answer for every room, and an absent field would put DMs under the
    /// wrong heading rather than fail loudly.
    pub kind: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

/// An ordinary named room: the thing everything before DMs was.
pub const ROOM_KIND_CHANNEL: &str = "channel";
/// Exactly two people. Has no name, no invitations and no roster management.
pub const ROOM_KIND_DM: &str = "dm";
/// Three or more people, opened the same way and with the same restrictions.
pub const ROOM_KIND_GROUP_DM: &str = "group_dm";

// --- the built-in rooms ------------------------------------------------------
//
// Three more kinds rather than an `is_builtin` flag beside `kind`, and that is
// a decision worth writing down because the flag looked cheaper.
//
// `kind` already answers exactly the question these rooms raise: *which
// management verbs apply, and which list does the client file this under*. A
// parallel boolean would answer half of it and leave every call site to join
// the two — `if room.is_builtin && room.kind == "channel"` — which is the shape
// that eventually disagrees with itself. It would also make the room list's
// categories a function of two fields instead of one, and the wire carry a
// field older clients would ignore *while still showing the room under
// "Channels"*. A kind they do not recognise is at least visibly unknown.
//
// The cost is the honest one: every `match` on kind, and every `is_direct()`
// caller, has to be revisited. That happened once, here, rather than
// indefinitely at each site that forgot the flag.

/// The owner's private notebook. Exactly one member, forever: a place to talk
/// to yourself, which nobody else can read, join or be invited to.
pub const ROOM_KIND_NOTE: &str = "note";
/// The owner and their own AI agent. The agent's replies are ordinary messages
/// sent from [`pocketskynet_core::WalletAddress::agent_of`].
pub const ROOM_KIND_JARVIS: &str = "jarvis";
/// The owner and this server's administrators — the standing line to whoever
/// runs the box, without either side having to invite the other.
pub const ROOM_KIND_LOBBY: &str = "lobby";

/// The three built-in kinds, in the order a client pins them.
pub const STATIC_ROOM_KINDS: [&str; 3] = [ROOM_KIND_NOTE, ROOM_KIND_JARVIS, ROOM_KIND_LOBBY];

impl Room {
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            name: row.get("name")?,
            description: row.get("description")?,
            current_key_version: row.get("current_key_version")?,
            key_rotation_pending: row.get::<_, i64>("key_rotation_pending")? != 0,
            kind: row.get("kind")?,
            created_at: iso_ms(row.get("created_at")?),
        })
    }

    /// Whether this room is a conversation between named people rather than a
    /// channel. The management verbs a DM refuses all key on this, not on the
    /// two kinds separately, so that adding a third kind of DM later cannot
    /// leave one of those checks behind.
    pub fn is_direct(&self) -> bool {
        self.kind == ROOM_KIND_DM || self.kind == ROOM_KIND_GROUP_DM
    }

    /// Whether this room is one of the three the server provisions for every
    /// account ([`STATIC_ROOM_KINDS`]).
    pub fn is_static(&self) -> bool {
        STATIC_ROOM_KINDS.contains(&self.kind.as_str())
    }

    /// The noun phrase for "this room's roster and name are not yours to
    /// change", or `None` when they are.
    ///
    /// One predicate rather than two checks at every call site, because the
    /// *rule* is one rule — rename, invite, kick, promote and demote all need
    /// a room whose membership somebody chose — and it was the DM half of it
    /// that had already been open-coded at half a dozen places. Returning the
    /// phrase rather than a bool keeps the refusal readable: the caller says
    /// what the verb was, this says what the room is, and the sentence reads
    /// correctly for a kind that did not exist when the caller was written.
    pub fn fixed_roster(&self) -> Option<&'static str> {
        if self.is_static() {
            Some("a built-in room")
        } else if self.is_direct() {
            Some("a direct message")
        } else {
            None
        }
    }
}

/// A room membership row joined to the member's profile.
#[derive(Debug, Clone, Serialize)]
pub struct RoomMemberWithUser {
    pub id: i64,
    #[serde(rename = "roomId")]
    pub room_id: String,
    #[serde(rename = "userAddress")]
    pub user_address: String,
    #[serde(rename = "joinedAt")]
    pub joined_at: String,
    pub user: User,
}

/// `Room` enriched with everything a room list or detail view needs.
///
/// `unreadCount` and `lastReadSerial` are populated only by `GET /api/rooms`;
/// the detail endpoint and the nested room inside `GET /api/rooms/hidden` omit
/// them, which is why they are skipped when `None` rather than sent as 0.
#[derive(Debug, Clone, Serialize)]
pub struct RoomWithMembers {
    #[serde(flatten)]
    pub room: Room,
    #[serde(rename = "memberCount")]
    pub member_count: usize,
    pub members: Vec<RoomMemberWithUser>,
    pub admins: Vec<User>,
    #[serde(rename = "lastMessage", skip_serializing_if = "Option::is_none")]
    pub last_message: Option<Message>,
    #[serde(rename = "hasEncryption")]
    pub has_encryption: bool,
    #[serde(rename = "unreadCount", skip_serializing_if = "Option::is_none")]
    pub unread_count: Option<i64>,
    #[serde(rename = "lastReadSerial", skip_serializing_if = "Option::is_none")]
    pub last_read_serial: Option<i64>,
    /// Unread messages in this room that name the caller.
    ///
    /// A separate number from `unreadCount` because it answers a different
    /// question — "is any of this addressed to me?" — and that is the one
    /// people actually triage by. Populated alongside `unreadCount`, on
    /// `GET /api/rooms` only.
    #[serde(rename = "mentionCount", skip_serializing_if = "Option::is_none")]
    pub mention_count: Option<i64>,
}

/// A message row. `sender` is attached by the endpoints that document it and
/// omitted — not nulled — everywhere else.
#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub id: String,
    #[serde(rename = "roomId")]
    pub room_id: String,
    #[serde(rename = "senderAddress")]
    pub sender_address: String,
    pub content: String,
    #[serde(rename = "msgHash")]
    pub msg_hash: String,
    #[serde(rename = "messageTimestamp")]
    pub message_timestamp: i64,
    #[serde(rename = "msgType")]
    pub msg_type: String,
    #[serde(rename = "msgSerial")]
    pub msg_serial: i64,
    #[serde(rename = "isDeleted")]
    pub is_deleted: bool,
    #[serde(rename = "editedAt")]
    pub edited_at: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "isEncrypted")]
    pub is_encrypted: bool,
    pub iv: Option<String>,
    pub hmac: Option<String>,
    #[serde(rename = "encVer")]
    pub enc_ver: i64,
    #[serde(rename = "keyVersion")]
    pub key_version: i64,
    #[serde(rename = "txHash")]
    pub tx_hash: Option<String>,
    #[serde(rename = "targetMessageId")]
    pub target_message_id: Option<String>,
    #[serde(rename = "emoticonCode")]
    pub emoticon_code: Option<String>,
    /// The thread this message belongs to, or `null` for a top-level post.
    /// Always the thread's **root**, never the message directly replied to —
    /// see the column comment in `schema.sql`.
    #[serde(rename = "parentMessageId")]
    pub parent_message_id: Option<String>,
    /// How many replies this message has, and when the newest arrived.
    ///
    /// Present only on `GET /api/rooms/{id}/messages`, which is the one read
    /// path that hides replies and therefore owes the caller a summary of what
    /// it hid. `/sync` omits both, deliberately: it delivers the reply rows
    /// themselves, so a client folding the stream can keep its own counts
    /// exact — and a count computed against an unfiltered query would
    /// contradict the block-filtered rows the client actually received.
    #[serde(rename = "replyCount", skip_serializing_if = "Option::is_none")]
    pub reply_count: Option<i64>,
    #[serde(rename = "lastReplyAt", skip_serializing_if = "Option::is_none")]
    pub last_reply_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender: Option<User>,
}

/// `msgType` values a client may see. The DB default `"message"` is never
/// written by any code path; §15 #21 asks that it be read as `"add"`, which
/// happens here so no downstream match has to know about it.
pub const MSG_TYPE_ADD: &str = "add";
pub const MSG_TYPE_EDIT: &str = "edit";
pub const MSG_TYPE_DELETE: &str = "delete";
pub const MSG_TYPE_DELETE_ALL: &str = "delete_all";
pub const MSG_TYPE_EMOTICON_ADD: &str = "emoticon_add";
pub const MSG_TYPE_EMOTICON_REMOVE: &str = "emoticon_remove";

impl Message {
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let raw_type: String = row.get("msg_type")?;
        Ok(Self {
            id: row.get("id")?,
            room_id: row.get("room_id")?,
            sender_address: row.get("sender_address")?,
            content: row.get("content")?,
            msg_hash: row.get("msg_hash")?,
            message_timestamp: row.get("message_timestamp")?,
            msg_type: if raw_type == "message" {
                MSG_TYPE_ADD.to_owned()
            } else {
                raw_type
            },
            msg_serial: row.get("msg_serial")?,
            is_deleted: row.get::<_, i64>("is_deleted")? != 0,
            edited_at: iso_opt(row.get("edited_at")?),
            created_at: iso_ms(row.get("created_at")?),
            is_encrypted: row.get::<_, i64>("is_encrypted")? != 0,
            iv: row.get("iv")?,
            hmac: row.get("hmac")?,
            enc_ver: row.get("enc_ver")?,
            key_version: row.get("key_version")?,
            tx_hash: row.get("tx_hash")?,
            target_message_id: row.get("target_message_id")?,
            emoticon_code: row.get("emoticon_code")?,
            parent_message_id: row.get("parent_message_id")?,
            reply_count: None,
            last_reply_at: None,
            sender: None,
        })
    }

    /// Read a row from a `messages LEFT JOIN users` query, attaching the
    /// sender profile or the synthesised placeholder.
    pub fn from_joined_row(row: &Row<'_>, now_ms: i64) -> rusqlite::Result<Self> {
        let mut msg = Self::from_row(row)?;
        let username: Option<String> = row.get("u_username")?;
        msg.sender = Some(match username {
            Some(username) => User {
                wallet_address: msg.sender_address.clone(),
                username,
                public_key: row.get("u_public_key")?,
                public_key_sig: row.get("u_public_key_sig")?,
                profile_image: row.get("u_profile_image")?,
                created_at: iso_ms(row.get("u_created_at")?),
                updated_at: iso_ms(row.get("u_updated_at")?),
            },
            None => User::placeholder(&msg.sender_address, now_ms),
        });
        Ok(msg)
    }

    /// [`Message::from_joined_row`] for a query that also selected
    /// [`MESSAGE_THREAD_COLUMNS`].
    ///
    /// `reply_count` is normalised to `None` when it is zero. A message with
    /// no thread should not carry a `replyCount: 0` that every client then has
    /// to test before deciding not to render a footer — absent already means
    /// that, and it keeps the common row smaller.
    pub fn from_threaded_row(row: &Row<'_>, now_ms: i64) -> rusqlite::Result<Self> {
        let mut msg = Self::from_joined_row(row, now_ms)?;
        let replies: i64 = row.get("reply_count")?;
        if replies > 0 {
            msg.reply_count = Some(replies);
            msg.last_reply_at = row.get("last_reply_at")?;
        }
        Ok(msg)
    }
}

/// The column list for a `messages LEFT JOIN users` query. Kept in one place
/// so [`Message::from_joined_row`] and every caller cannot drift apart.
pub const MESSAGE_JOIN_COLUMNS: &str = "\
    m.id, m.room_id, m.sender_address, m.content, m.msg_hash, m.message_timestamp, \
    m.msg_type, m.msg_serial, m.is_deleted, m.edited_at, m.created_at, m.is_encrypted, \
    m.iv, m.hmac, m.enc_ver, m.key_version, m.tx_hash, m.target_message_id, m.emoticon_code, \
    m.parent_message_id, \
    u.username AS u_username, u.public_key AS u_public_key, \
    u.public_key_sig AS u_public_key_sig, u.profile_image AS u_profile_image, \
    u.created_at AS u_created_at, u.updated_at AS u_updated_at";

/// Two more columns a thread-aware query selects: how many replies each row
/// has and when the newest landed.
///
/// Correlated subqueries rather than a `GROUP BY` join, because the outer
/// query already filters replies *out* — joining the very rows being excluded
/// back in to count them reads as a contradiction, and the partial index on
/// `parent_message_id` makes each lookup a direct seek.
pub const MESSAGE_THREAD_COLUMNS: &str = "\
    (SELECT COUNT(*) FROM messages r \
       WHERE r.parent_message_id = m.id AND r.is_deleted = 0 AND r.msg_type IN ('add', 'edit')) \
     AS reply_count, \
    (SELECT MAX(r.message_timestamp) FROM messages r \
       WHERE r.parent_message_id = m.id AND r.is_deleted = 0 AND r.msg_type IN ('add', 'edit')) \
     AS last_reply_at";

#[derive(Debug, Clone, Serialize)]
pub struct RoomKey {
    pub id: i64,
    #[serde(rename = "roomId")]
    pub room_id: String,
    #[serde(rename = "userAddress")]
    pub user_address: String,
    #[serde(rename = "encryptedSymmetricKey")]
    pub encrypted_symmetric_key: String,
    #[serde(rename = "ephemeralPublicKey")]
    pub ephemeral_public_key: String,
    #[serde(rename = "encryptionIV")]
    pub encryption_iv: String,
    pub hmac: String,
    #[serde(rename = "encVer")]
    pub enc_ver: i64,
    #[serde(rename = "keyVersion")]
    pub key_version: i64,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

impl RoomKey {
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            room_id: row.get("room_id")?,
            user_address: row.get("user_address")?,
            encrypted_symmetric_key: row.get("encrypted_symmetric_key")?,
            ephemeral_public_key: row.get("ephemeral_public_key")?,
            encryption_iv: row.get("encryption_iv")?,
            hmac: row.get("hmac")?,
            enc_ver: row.get("enc_ver")?,
            key_version: row.get("key_version")?,
            created_at: iso_ms(row.get("created_at")?),
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BlockedUser {
    pub id: i64,
    #[serde(rename = "blockerAddress")]
    pub blocker_address: String,
    #[serde(rename = "blockedAddress")]
    pub blocked_address: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct HiddenRoom {
    pub id: i64,
    #[serde(rename = "userAddress")]
    pub user_address: String,
    #[serde(rename = "roomId")]
    pub room_id: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

/// A hidden-room row with the room detail folded in.
#[derive(Debug, Clone, Serialize)]
pub struct HiddenRoomWithRoom {
    #[serde(flatten)]
    pub hidden: HiddenRoom,
    pub room: RoomWithMembers,
}

/// A pending invitation, enriched for display.
#[derive(Debug, Clone, Serialize)]
pub struct InvitationView {
    #[serde(rename = "roomId")]
    pub room_id: String,
    #[serde(rename = "roomName")]
    pub room_name: String,
    #[serde(rename = "invitedBy")]
    pub invited_by: String,
    #[serde(rename = "inviterUsername")]
    pub inviter_username: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

/// An incoming webhook (docs/API.md §17), token included.
///
/// The token appears in every serialisation on purpose: these objects are only
/// ever returned to a room admin, and the admin's one job on this screen is to
/// copy the URL — a listing that redacted it would force a revoke-and-recreate
/// cycle every time a CI config is rebuilt.
#[derive(Debug, Clone, Serialize)]
pub struct Webhook {
    pub id: String,
    #[serde(rename = "roomId")]
    pub room_id: String,
    pub name: String,
    pub token: String,
    /// The path an external system POSTs to. Derived, but sent anyway so no
    /// integrator has to learn the URL shape from prose.
    pub url: String,
    /// The address this webhook's messages are sent from — see
    /// `WalletAddress::webhook_sender`.
    #[serde(rename = "senderAddress")]
    pub sender_address: String,
    #[serde(rename = "createdBy")]
    pub created_by: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

impl Webhook {
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let id: String = row.get("id")?;
        let token: String = row.get("token")?;
        Ok(Self {
            url: format!("/api/webhooks/{token}"),
            sender_address: pocketskynet_core::WalletAddress::webhook_sender(&id)
                .as_str()
                .to_owned(),
            id,
            room_id: row.get("room_id")?,
            name: row.get("name")?,
            token,
            created_by: row.get("created_by")?,
            created_at: iso_ms(row.get("created_at")?),
        })
    }
}

/// One reaction code and the set of users currently holding it.
///
/// `count` is the size of the reactor set. It can exceed `users.len()` when a
/// reactor has no profile row, so clients are told to trust `count`.
#[derive(Debug, Clone, Serialize)]
pub struct EmoticonAggregation {
    #[serde(rename = "emoticonCode")]
    pub emoticon_code: String,
    pub count: usize,
    pub users: Vec<User>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_match_the_javascript_iso_form() {
        // Exactly three fractional digits, `Z`, no offset — what
        // `Date.toISOString()` produces and what every client parses.
        assert_eq!(iso_ms(1_749_652_746_000), "2025-06-11T14:39:06.000Z");
        assert_eq!(iso_ms(1_749_652_746_620), "2025-06-11T14:39:06.620Z");
        assert_eq!(iso_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_ms(1_749_652_746_620).len(), 24);
    }

    #[test]
    fn placeholder_users_use_the_documented_username_shape() {
        let u = User::placeholder("0xabcd00000000000000000000000000000000ef01", 0);
        assert_eq!(u.username, "User 0xabcd...ef01");
        // §15 #18: emitted as null in both code paths, never omitted.
        let json = serde_json::to_value(&u).unwrap();
        assert!(json.get("publicKeySig").is_some());
        assert!(json["publicKeySig"].is_null());
    }

    #[test]
    fn absent_optionals_are_omitted_while_null_columns_are_sent() {
        let msg = Message {
            id: "msg_1749652746620_abcd".into(),
            room_id: "room_1749652739650_ab".into(),
            sender_address: "0xaa".into(),
            content: "hi".into(),
            msg_hash: "b9".into(),
            message_timestamp: 1,
            msg_type: MSG_TYPE_ADD.into(),
            msg_serial: 2,
            is_deleted: false,
            edited_at: None,
            created_at: iso_ms(0),
            is_encrypted: false,
            iv: None,
            hmac: None,
            enc_ver: 1,
            key_version: 1,
            tx_hash: None,
            target_message_id: None,
            emoticon_code: None,
            parent_message_id: None,
            reply_count: None,
            last_reply_at: None,
            sender: None,
        };
        let json = serde_json::to_value(&msg).unwrap();

        assert!(json.get("sender").is_none(), "sender is omitted, not null");
        assert!(json["iv"].is_null(), "nullable columns stay in the object");
        assert!(json["editedAt"].is_null());
        assert_eq!(json["msgSerial"], 2);
        assert_eq!(json["isDeleted"], false);
    }

    #[test]
    fn room_with_members_flattens_the_room_fields() {
        let room = Room {
            id: "room_1749652739650_ab".into(),
            name: "Team".into(),
            description: None,
            current_key_version: 1,
            key_rotation_pending: false,
            kind: ROOM_KIND_CHANNEL.into(),
            created_at: iso_ms(0),
        };
        let enriched = RoomWithMembers {
            room,
            member_count: 0,
            members: vec![],
            admins: vec![],
            last_message: None,
            has_encryption: false,
            unread_count: None,
            last_read_serial: None,
            mention_count: None,
        };
        let json = serde_json::to_value(&enriched).unwrap();

        assert_eq!(json["id"], "room_1749652739650_ab");
        assert_eq!(json["currentKeyVersion"], 1);
        assert_eq!(json["hasEncryption"], false);
        // Absent on the detail endpoint; present only on GET /api/rooms.
        assert!(json.get("unreadCount").is_none());
        assert!(json.get("lastMessage").is_none());
    }
}
