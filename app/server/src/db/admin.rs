//! Server-wide administration: suspensions, and the counts an operator needs
//! to see what is on their server.
//!
//! Who *is* an admin is not here — that is `routes::misc::server_admins`,
//! read from the deployment's configuration. What is here is everything an
//! admin can then do that no room-level role can express.

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use super::models::iso_ms;
use super::now_ms;
use crate::error::ApiResult;

// ----------------------------------------------------------- suspensions ---

/// Suspend an account. Idempotent, and re-suspending refreshes the reason
/// rather than refusing — an admin correcting the note they left should not
/// have to reinstate first.
pub fn suspend(conn: &Connection, address: &str, reason: Option<&str>, by: &str) -> ApiResult<()> {
    conn.execute(
        "INSERT INTO suspended_users (wallet_address, reason, suspended_by, created_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (wallet_address) DO UPDATE SET
             reason = excluded.reason,
             suspended_by = excluded.suspended_by",
        params![address, reason, by, now_ms()],
    )?;
    Ok(())
}

/// Lift a suspension. Permissive: reinstating somebody who was never
/// suspended is a no-op, not an error.
pub fn reinstate(conn: &Connection, address: &str) -> ApiResult<()> {
    conn.execute(
        "DELETE FROM suspended_users WHERE wallet_address = ?1",
        params![address],
    )?;
    Ok(())
}

pub fn is_suspended(conn: &Connection, address: &str) -> ApiResult<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM suspended_users WHERE wallet_address = ?1",
        params![address],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Every suspended address, for the cache that fronts this table.
pub fn suspended_addresses(conn: &Connection) -> ApiResult<Vec<String>> {
    let mut stmt = conn.prepare("SELECT wallet_address FROM suspended_users")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Remove an account's presence from every room on the server.
///
/// This is the closest thing to deleting a person, and it deliberately stops
/// short of it. Their `users` row survives, because every message they ever
/// sent carries their address as its sender and removing the profile would
/// turn a year of history into unattributed text. What goes is everything that
/// grants *access*: memberships, admin roles, wrapped room keys, pending
/// invitations, read pointers.
///
/// The key rows matter most. A departed member who keeps their wrapped key
/// keeps the ability to read anything sent under that epoch, so every room
/// they were in is flagged for rotation — the same guarantee `leave` provides,
/// applied everywhere at once.
///
/// Returns the rooms that now need re-keying, so the caller can tell them.
pub fn evict_from_all_rooms(conn: &mut Connection, address: &str) -> ApiResult<Vec<String>> {
    let tx = conn.transaction()?;

    let rooms: Vec<String> = {
        let mut stmt = tx.prepare("SELECT room_id FROM room_members WHERE user_address = ?1")?;
        let rows = stmt.query_map(params![address], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    tx.execute(
        "DELETE FROM room_keys WHERE user_address = ?1",
        params![address],
    )?;
    tx.execute(
        "DELETE FROM room_members WHERE user_address = ?1",
        params![address],
    )?;
    tx.execute(
        "DELETE FROM room_admins WHERE wallet_address = ?1",
        params![address],
    )?;
    tx.execute(
        "DELETE FROM room_invitations WHERE invited_address = ?1",
        params![address],
    )?;
    tx.execute(
        "DELETE FROM room_reads WHERE user_address = ?1",
        params![address],
    )?;
    tx.execute(
        "DELETE FROM hidden_rooms WHERE user_address = ?1",
        params![address],
    )?;
    // Their published encryption key is what other members wrap new room keys
    // to. Leaving it would let a re-keying member seal the next epoch to
    // somebody who is no longer meant to have it.
    tx.execute(
        "UPDATE users SET public_key = NULL, public_key_sig = NULL WHERE wallet_address = ?1",
        params![address],
    )?;

    for room in &rooms {
        tx.execute(
            "UPDATE rooms SET key_rotation_pending = 1 WHERE id = ?1",
            params![room],
        )?;
    }

    tx.commit()?;
    Ok(rooms)
}

// -------------------------------------------------------------- overview ---

/// One account, as an operator sees it.
#[derive(Debug, Clone, Serialize)]
pub struct AdminUserView {
    #[serde(rename = "walletAddress")]
    pub wallet_address: String,
    pub username: String,
    #[serde(rename = "profileImage")]
    pub profile_image: Option<String>,
    #[serde(rename = "roomCount")]
    pub room_count: i64,
    #[serde(rename = "messageCount")]
    pub message_count: i64,
    #[serde(rename = "isSuspended")]
    pub is_suspended: bool,
    #[serde(rename = "suspendedReason")]
    pub suspended_reason: Option<String>,
    /// Whether this wallet is in `VITE_FRUITNATION_ADMIN`. Filled in by the
    /// route, which is where the configuration is read.
    #[serde(rename = "isServerAdmin")]
    pub is_server_admin: bool,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

/// Every account on the server, newest first.
///
/// Not paginated, and that is a scale decision rather than an oversight: this
/// is a self-hosted server for a team, the roadmap says single-node, and an
/// operator who wants to see who is on their server wants to see all of them.
/// The cap exists so that assumption failing is a slow page rather than an
/// unbounded response.
pub fn list_users(conn: &Connection, limit: i64) -> ApiResult<Vec<AdminUserView>> {
    let mut stmt = conn.prepare(
        "SELECT u.wallet_address, u.username, u.profile_image, u.created_at,
                (SELECT COUNT(*) FROM room_members rm
                  WHERE rm.user_address = u.wallet_address) AS room_count,
                (SELECT COUNT(*) FROM messages m
                  WHERE m.sender_address = u.wallet_address
                    AND m.msg_type = 'add' AND m.is_deleted = 0) AS message_count,
                s.reason AS suspended_reason,
                s.wallet_address IS NOT NULL AS is_suspended
         FROM users u
         LEFT JOIN suspended_users s ON s.wallet_address = u.wallet_address
         ORDER BY u.created_at DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], |row| {
        Ok(AdminUserView {
            wallet_address: row.get("wallet_address")?,
            username: row.get("username")?,
            profile_image: row.get("profile_image")?,
            room_count: row.get("room_count")?,
            message_count: row.get("message_count")?,
            is_suspended: row.get::<_, i64>("is_suspended")? != 0,
            suspended_reason: row.get("suspended_reason")?,
            is_server_admin: false,
            created_at: iso_ms(row.get("created_at")?),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// One room, as an operator sees it.
///
/// Note what is *not* here: any message content. An admin can see that a room
/// exists, how big it is and how busy, and can delete it — but this endpoint
/// is not a way to read a conversation they were never in. Half the rooms on
/// a server like this are end-to-end encrypted and could not be read anyway;
/// the other half should not be readable by a different mechanism just
/// because they are not.
#[derive(Debug, Clone, Serialize)]
pub struct AdminRoomView {
    pub id: String,
    pub name: String,
    pub kind: String,
    #[serde(rename = "memberCount")]
    pub member_count: i64,
    #[serde(rename = "messageCount")]
    pub message_count: i64,
    #[serde(rename = "hasEncryption")]
    pub has_encryption: bool,
    #[serde(rename = "lastActivityAt")]
    pub last_activity_at: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

pub fn list_rooms(conn: &Connection, limit: i64) -> ApiResult<Vec<AdminRoomView>> {
    let mut stmt = conn.prepare(
        "SELECT r.id, r.name, r.kind, r.created_at,
                (SELECT COUNT(*) FROM room_members rm WHERE rm.room_id = r.id) AS member_count,
                (SELECT COUNT(*) FROM messages m
                  WHERE m.room_id = r.id AND m.msg_type = 'add' AND m.is_deleted = 0)
                  AS message_count,
                (SELECT MAX(m.message_timestamp) FROM messages m WHERE m.room_id = r.id)
                  AS last_activity_at,
                EXISTS (SELECT 1 FROM room_keys k WHERE k.room_id = r.id) AS has_encryption
         FROM rooms r
         ORDER BY r.created_at DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], |row| {
        Ok(AdminRoomView {
            id: row.get("id")?,
            name: row.get("name")?,
            kind: row.get("kind")?,
            member_count: row.get("member_count")?,
            message_count: row.get("message_count")?,
            has_encryption: row.get::<_, i64>("has_encryption")? != 0,
            last_activity_at: row.get::<_, Option<i64>>("last_activity_at")?.map(iso_ms),
            created_at: iso_ms(row.get("created_at")?),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Server-wide totals for the admin console header.
#[derive(Debug, Clone, Serialize)]
pub struct AdminTotals {
    pub users: i64,
    pub suspended: i64,
    pub channels: i64,
    #[serde(rename = "directMessages")]
    pub direct_messages: i64,
    pub messages: i64,
    pub files: i64,
}

pub fn totals(conn: &Connection) -> ApiResult<AdminTotals> {
    let one = |sql: &str| -> ApiResult<i64> { Ok(conn.query_row(sql, [], |r| r.get(0))?) };
    Ok(AdminTotals {
        users: one("SELECT COUNT(*) FROM users")?,
        suspended: one("SELECT COUNT(*) FROM suspended_users")?,
        channels: one("SELECT COUNT(*) FROM rooms WHERE kind = 'channel'")?,
        direct_messages: one("SELECT COUNT(*) FROM rooms WHERE kind <> 'channel'")?,
        messages: one("SELECT COUNT(*) FROM messages WHERE msg_type = 'add' AND is_deleted = 0")?,
        files: one("SELECT COUNT(*) FROM files")?,
    })
}

/// The name a room is known by, for an audit line. `None` if it is gone.
pub fn room_name(conn: &Connection, room_id: &str) -> ApiResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT name FROM rooms WHERE id = ?1",
            params![room_id],
            |r| r.get(0),
        )
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::keys;
    use crate::db::rooms;
    use crate::db::test_db;
    use crate::db::users::upsert_user;

    const ROOM: &str = "room_1749652739650_adm";
    const ALICE: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BOB: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn suspending_is_idempotent_and_reversible() {
        let db = test_db();
        db.call_blocking(|conn| {
            upsert_user(conn, BOB, "bob", None, None).unwrap();

            assert!(!is_suspended(conn, BOB).unwrap());
            suspend(conn, BOB, Some("spam"), ALICE).unwrap();
            suspend(conn, BOB, Some("spam, repeatedly"), ALICE).unwrap();
            assert!(is_suspended(conn, BOB).unwrap());
            assert_eq!(suspended_addresses(conn).unwrap(), vec![BOB.to_owned()]);

            let listed = list_users(conn, 50).unwrap();
            let bob = listed.iter().find(|u| u.wallet_address == BOB).unwrap();
            assert!(bob.is_suspended);
            assert_eq!(bob.suspended_reason.as_deref(), Some("spam, repeatedly"));

            reinstate(conn, BOB).unwrap();
            assert!(!is_suspended(conn, BOB).unwrap());
            // Reinstating twice is not an error.
            reinstate(conn, BOB).unwrap();
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn eviction_removes_access_and_flags_every_room_for_rekeying() {
        let db = test_db();
        db.call_blocking(|conn| {
            upsert_user(conn, ALICE, "alice", None, None).unwrap();
            upsert_user(conn, BOB, "bob", Some(&"11".repeat(33)), None).unwrap();
            rooms::create_room(conn, ROOM, "Team", None, ALICE).unwrap();
            rooms::add_member(conn, ROOM, BOB).unwrap();
            rooms::add_admin(conn, ROOM, BOB).unwrap();
            keys::store_key(
                conn,
                ROOM,
                &keys::KeyWrap {
                    user_address: BOB.into(),
                    encrypted_symmetric_key: "aa".into(),
                    ephemeral_public_key: "bb".into(),
                    encryption_iv: "cc".into(),
                    hmac: "dd".into(),
                    enc_ver: 1,
                },
                1,
            )
            .unwrap();

            let touched = evict_from_all_rooms(conn, BOB).unwrap();
            assert_eq!(touched, vec![ROOM.to_owned()]);

            assert!(!rooms::is_member(conn, ROOM, BOB).unwrap());
            assert!(!rooms::is_admin(conn, ROOM, BOB).unwrap());
            // The wrapped key is what would have let them keep reading.
            assert!(keys::latest_key(conn, ROOM, BOB).unwrap().is_none());
            assert!(
                rooms::get_room(conn, ROOM)
                    .unwrap()
                    .unwrap()
                    .key_rotation_pending
            );

            // The profile survives, so their old messages stay attributed —
            // but the published encryption key does not, so nobody can wrap
            // the next epoch to them.
            let bob = crate::db::users::get_user(conn, BOB).unwrap().unwrap();
            assert_eq!(bob.username, "bob");
            assert_eq!(bob.public_key, None);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn the_overview_counts_what_is_on_the_server() {
        let db = test_db();
        db.call_blocking(|conn| {
            upsert_user(conn, ALICE, "alice", None, None).unwrap();
            upsert_user(conn, BOB, "bob", None, None).unwrap();
            rooms::create_room(conn, ROOM, "Team", None, ALICE).unwrap();
            rooms::create_dm(conn, "room_dm_x", &[ALICE.into(), BOB.into()]).unwrap();

            let totals = totals(conn).unwrap();
            assert_eq!(totals.users, 2);
            assert_eq!(totals.channels, 1);
            assert_eq!(totals.direct_messages, 1);
            assert_eq!(totals.suspended, 0);

            let listed = list_rooms(conn, 50).unwrap();
            assert_eq!(listed.len(), 2);
            let channel = listed.iter().find(|r| r.kind == "channel").unwrap();
            assert_eq!(channel.member_count, 1);
            assert_eq!(channel.message_count, 0);
            assert!(!channel.has_encryption);
            assert_eq!(channel.last_activity_at, None);
            Ok(())
        })
        .unwrap();
    }
}
