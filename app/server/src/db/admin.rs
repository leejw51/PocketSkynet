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

// ---------------------------------------------------------------- storage ---
//
// What the files dashboard reads. Everything below is aggregation over rows
// that already exist — `files.created_at` is what makes "growth over time"
// free — and none of it returns a byte of file *content*. The same line the
// rest of this module holds for messages: an operator sees what is on their
// server, not what is in it.
//
// One quirk to keep in mind throughout: storage is content-addressed, so two
// rows may share one blob on disk (see `db/files.rs`). Wherever it matters the
// code says which of the two it is counting — rows are what people uploaded,
// distinct stored names are what the disk holds.

/// What kind of thing a file is, judged by its stored extension.
///
/// The stored extension rather than the declared MIME, because the MIME column
/// is always `application/octet-stream` by design (`routes/files.rs`): the
/// uploader's declared type is recorded nowhere trustworthy, and the extension
/// has at least been through `extension_of`'s reduction. This is a *reporting*
/// classification — nothing security-relevant may ever hang off it.
pub fn category_of(ext: &str) -> &'static str {
    match ext {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "bmp" | "svg" | "heic" | "heif"
        | "tif" | "tiff" | "ico" => "image",
        "mp4" | "m4v" | "webm" | "ogv" | "mov" | "avi" | "mkv" | "wmv" | "flv" => "video",
        "mp3" | "wav" | "m4a" | "aac" | "ogg" | "oga" | "flac" | "opus" | "wma" => "audio",
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "md" | "csv" | "rtf"
        | "odt" | "ods" | "odp" | "epub" => "document",
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" | "dmg" | "iso" => "archive",
        _ => "other",
    }
}

/// The categories, in the order every chart and legend shows them. Fixed so a
/// server with no videos renders the same legend as one full of them.
pub const CATEGORIES: [&str; 6] = ["image", "video", "audio", "document", "archive", "other"];

/// The headline numbers.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StorageTotals {
    /// Attachment rows — what people uploaded.
    pub files: i64,
    /// Distinct blobs — what the disk holds. Less than `files` whenever the
    /// same bytes were uploaded twice; the gap *is* the dedupe.
    pub blobs: i64,
    /// Bytes summed over rows: what members experience as stored.
    #[serde(rename = "logicalBytes")]
    pub logical_bytes: i64,
    /// Bytes summed over distinct blobs: what the disk actually pays.
    #[serde(rename = "diskBytes")]
    pub disk_bytes: i64,
    #[serde(rename = "roomsWithFiles")]
    pub rooms_with_files: i64,
}

pub fn storage_totals(conn: &Connection) -> ApiResult<StorageTotals> {
    // Identical bytes have identical size — that is what content-addressed
    // means — so MAX per stored name is not a choice among candidates, it is
    // the one value phrased in a way SQL accepts.
    let (blobs, disk_bytes) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(size), 0)
         FROM (SELECT MAX(size_bytes) AS size FROM files GROUP BY stored_name)",
        [],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    )?;
    let (files, logical_bytes, rooms_with_files) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(size_bytes), 0), COUNT(DISTINCT room_id) FROM files",
        [],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        },
    )?;
    Ok(StorageTotals {
        files,
        blobs,
        logical_bytes,
        disk_bytes,
        rooms_with_files,
    })
}

/// One slice of the by-kind breakdown. Row-counted, not blob-counted: the
/// question this answers is "what do people put here".
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CategorySlice {
    pub category: String,
    pub files: i64,
    pub bytes: i64,
}

/// Every category, in [`CATEGORIES`] order, zeros included — a chart legend
/// that reshuffles itself whenever a kind appears is a chart nobody can learn.
pub fn category_breakdown(conn: &Connection) -> ApiResult<Vec<CategorySlice>> {
    // The stored name is `{64 hex}.{ext}` by construction (`routes/files.rs`
    // re-validates that shape on every read), so the extension starts at
    // character 66 and SQL can group on it without a string-splitting UDF.
    let mut stmt = conn.prepare(
        "SELECT substr(stored_name, 66) AS ext, COUNT(*), COALESCE(SUM(size_bytes), 0)
         FROM files GROUP BY ext",
    )?;
    let mut by_category = std::collections::HashMap::<&'static str, (i64, i64)>::new();
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (ext, files, bytes) = row?;
        let entry = by_category.entry(category_of(&ext)).or_default();
        entry.0 += files;
        entry.1 += bytes;
    }
    Ok(CATEGORIES
        .iter()
        .map(|&category| {
            let (files, bytes) = by_category.get(category).copied().unwrap_or_default();
            CategorySlice {
                category: category.to_owned(),
                files,
                bytes,
            }
        })
        .collect())
}

/// One room's share of the disk.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RoomUsage {
    #[serde(rename = "roomId")]
    pub room_id: String,
    pub name: String,
    pub kind: String,
    pub files: i64,
    pub bytes: i64,
}

/// Heaviest rooms first. Row-summed — a blob two rooms share is charged to
/// both, because "how much has this room put here" is the question an operator
/// deciding what to purge is actually asking.
pub fn room_usage(conn: &Connection, limit: i64) -> ApiResult<Vec<RoomUsage>> {
    let mut stmt = conn.prepare(
        "SELECT r.id, r.name, r.kind, COUNT(*), COALESCE(SUM(f.size_bytes), 0) AS bytes
         FROM files f JOIN rooms r ON r.id = f.room_id
         GROUP BY r.id
         ORDER BY bytes DESC, r.id
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], |r| {
        Ok(RoomUsage {
            room_id: r.get(0)?,
            name: r.get(1)?,
            kind: r.get(2)?,
            files: r.get(3)?,
            bytes: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// One attachment, as the dashboard's listing sees it.
///
/// Metadata only, like everything an operator sees: name, size, place, who
/// and when — never the bytes, and never `stored_name`, which is a filesystem
/// coupling no wire type carries (`db/files.rs`). Note there is no caption
/// here either: a caption is something somebody wrote *into a room*, and this
/// listing is not a way to read rooms.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AdminFileView {
    pub id: String,
    pub filename: String,
    /// The stored extension, e.g. `"pdf"` — what [`category_of`] judged.
    pub extension: String,
    pub category: String,
    #[serde(rename = "sizeBytes")]
    pub size_bytes: i64,
    #[serde(rename = "roomId")]
    pub room_id: String,
    #[serde(rename = "roomName")]
    pub room_name: String,
    pub uploader: String,
    /// The uploader's username, when their profile still exists.
    #[serde(rename = "uploaderName")]
    pub uploader_name: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

const FILE_VIEW_SQL: &str = "SELECT f.id, f.filename, substr(f.stored_name, 66) AS ext,
        f.size_bytes, f.room_id, r.name AS room_name, f.uploader, u.username, f.created_at
 FROM files f
 JOIN rooms r ON r.id = f.room_id
 LEFT JOIN users u ON u.wallet_address = f.uploader";

fn file_view_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AdminFileView> {
    let extension: String = row.get("ext")?;
    Ok(AdminFileView {
        id: row.get("id")?,
        filename: row.get("filename")?,
        category: category_of(&extension).to_owned(),
        extension,
        size_bytes: row.get("size_bytes")?,
        room_id: row.get("room_id")?,
        room_name: row.get("room_name")?,
        uploader: row.get("uploader")?,
        uploader_name: row.get("username")?,
        created_at: iso_ms(row.get("created_at")?),
    })
}

/// Every attachment on the server, newest first. Sorting and filtering happen
/// client-side — the cap is the same scale decision `list_users` states.
pub fn list_files(conn: &Connection, limit: i64) -> ApiResult<Vec<AdminFileView>> {
    let mut stmt = conn.prepare(&format!(
        "{FILE_VIEW_SQL} ORDER BY f.created_at DESC, f.rowid DESC LIMIT ?1"
    ))?;
    let rows = stmt.query_map(params![limit], file_view_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The biggest attachments — the first thing an operator watching a full disk
/// wants named.
pub fn largest_files(conn: &Connection, limit: i64) -> ApiResult<Vec<AdminFileView>> {
    let mut stmt = conn.prepare(&format!(
        "{FILE_VIEW_SQL} ORDER BY f.size_bytes DESC, f.rowid DESC LIMIT ?1"
    ))?;
    let rows = stmt.query_map(params![limit], file_view_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// One day of upload volume, UTC.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GrowthPoint {
    /// `YYYY-MM-DD`.
    pub day: String,
    pub files: i64,
    pub bytes: i64,
}

/// Upload volume per day over the trailing window, oldest first.
///
/// Derived entirely from `files.created_at` — no new table, no sampling job.
/// Days with no uploads produce no row; the client fills the gaps, because a
/// chart is a presentation concern and a wire format padded with zeros is just
/// a bigger wire format.
pub fn growth(conn: &Connection, days: i64) -> ApiResult<Vec<GrowthPoint>> {
    let since = now_ms() - days.max(1) * 24 * 60 * 60 * 1000;
    let mut stmt = conn.prepare(
        "SELECT date(created_at / 1000, 'unixepoch') AS day,
                COUNT(*), COALESCE(SUM(size_bytes), 0)
         FROM files
         WHERE created_at >= ?1
         GROUP BY day
         ORDER BY day",
    )?;
    let rows = stmt.query_map(params![since], |r| {
        Ok(GrowthPoint {
            day: r.get(0)?,
            files: r.get(1)?,
            bytes: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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

    /// A world with two rooms, two uploaders, and one deliberately duplicated
    /// blob — the storage questions are all about that duplicate.
    fn stored_world(conn: &mut Connection) {
        upsert_user(conn, ALICE, "alice", None, None).unwrap();
        upsert_user(conn, BOB, "bob", None, None).unwrap();
        rooms::create_room(conn, "room_stats_one", "Design", None, ALICE).unwrap();
        rooms::create_room(conn, "room_stats_two", "Films", None, BOB).unwrap();

        let insert = |id: &str,
                      room: &str,
                      who: &str,
                      name: &str,
                      stem: &str,
                      ext: &str,
                      size: i64,
                      at: i64| {
            conn.execute(
                "INSERT INTO files (id, room_id, uploader, filename, stored_name, mime,
                                    size_bytes, caption, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'application/octet-stream', ?6, '', ?7)",
                params![
                    id,
                    room,
                    who,
                    name,
                    format!("{}.{ext}", stem.repeat(64)),
                    size,
                    at
                ],
            )
            .unwrap();
        };
        let now = now_ms();
        insert(
            "f1",
            "room_stats_one",
            ALICE,
            "report.pdf",
            "a",
            "pdf",
            100,
            now,
        );
        insert(
            "f2",
            "room_stats_one",
            ALICE,
            "photo.jpg",
            "b",
            "jpg",
            50,
            now,
        );
        // The same bytes as f1, uploaded again into the other room: two rows,
        // one blob — the dedupe `db/files.rs` promises.
        insert(
            "f3",
            "room_stats_two",
            BOB,
            "report-copy.pdf",
            "a",
            "pdf",
            100,
            now,
        );
        insert(
            "f4",
            "room_stats_two",
            BOB,
            "movie.mp4",
            "c",
            "mp4",
            500,
            now,
        );
    }

    #[test]
    fn storage_totals_count_rows_and_the_disk_separately() {
        let db = test_db();
        db.call_blocking(|conn| {
            stored_world(conn);
            let totals = storage_totals(conn).unwrap();
            assert_eq!(totals.files, 4, "rows: what people uploaded");
            assert_eq!(totals.blobs, 3, "blobs: what the disk holds");
            assert_eq!(totals.logical_bytes, 750);
            assert_eq!(totals.disk_bytes, 650, "the 100-byte gap is the dedupe");
            assert_eq!(totals.rooms_with_files, 2);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn an_empty_server_reports_zero_storage_not_null() {
        let db = test_db();
        db.call_blocking(|conn| {
            // COALESCE matters: SUM over no rows is NULL, and a fresh server
            // must answer zeros, not a deserialisation error.
            assert_eq!(
                storage_totals(conn).unwrap(),
                StorageTotals {
                    files: 0,
                    blobs: 0,
                    logical_bytes: 0,
                    disk_bytes: 0,
                    rooms_with_files: 0
                }
            );
            assert!(growth(conn, 30).unwrap().is_empty());
            assert!(room_usage(conn, 10).unwrap().is_empty());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn the_breakdown_is_every_category_in_fixed_order() {
        let db = test_db();
        db.call_blocking(|conn| {
            stored_world(conn);
            let slices = category_breakdown(conn).unwrap();
            let order: Vec<&str> = slices.iter().map(|s| s.category.as_str()).collect();
            assert_eq!(order, CATEGORIES.to_vec(), "legend order never reshuffles");

            let by = |c: &str| slices.iter().find(|s| s.category == c).unwrap().clone();
            assert_eq!((by("document").files, by("document").bytes), (2, 200));
            assert_eq!((by("image").files, by("image").bytes), (1, 50));
            assert_eq!((by("video").files, by("video").bytes), (1, 500));
            // Present with zeros, not absent.
            assert_eq!(by("audio").files, 0);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn the_classifier_reads_extensions_not_wishes() {
        assert_eq!(category_of("jpg"), "image");
        assert_eq!(category_of("mp4"), "video");
        assert_eq!(category_of("flac"), "audio");
        assert_eq!(category_of("pdf"), "document");
        assert_eq!(category_of("zip"), "archive");
        // `bin` is what `extension_of` assigns when nothing usable came in.
        assert_eq!(category_of("bin"), "other");
        assert_eq!(category_of(""), "other");
    }

    #[test]
    fn rooms_are_ranked_by_what_they_cost() {
        let db = test_db();
        db.call_blocking(|conn| {
            stored_world(conn);
            let usage = room_usage(conn, 10).unwrap();
            assert_eq!(usage[0].name, "Films");
            assert_eq!((usage[0].files, usage[0].bytes), (2, 600));
            assert_eq!(usage[1].name, "Design");
            assert_eq!((usage[1].files, usage[1].bytes), (2, 150));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn the_listing_carries_names_but_never_the_stored_name() {
        let db = test_db();
        db.call_blocking(|conn| {
            stored_world(conn);
            let files = list_files(conn, 100).unwrap();
            assert_eq!(files.len(), 4);
            let movie = files.iter().find(|f| f.filename == "movie.mp4").unwrap();
            assert_eq!(movie.room_name, "Films");
            assert_eq!(movie.uploader_name.as_deref(), Some("bob"));
            assert_eq!(movie.category, "video");
            assert_eq!(movie.extension, "mp4");
            // The wire never learns the filesystem name — same contract as
            // `db/files.rs`.
            let json = serde_json::to_string(&files).unwrap();
            assert!(!json.contains("stored_name") && !json.contains(&"a".repeat(64)));

            let largest = largest_files(conn, 2).unwrap();
            assert_eq!(largest[0].filename, "movie.mp4");
            assert_eq!(largest[0].size_bytes, 500);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn growth_buckets_by_day_and_respects_its_window() {
        let db = test_db();
        db.call_blocking(|conn| {
            upsert_user(conn, ALICE, "alice", None, None).unwrap();
            rooms::create_room(conn, "room_growth", "Team", None, ALICE).unwrap();
            let day = 24 * 60 * 60 * 1000;
            let now = now_ms();
            let insert = |id: &str, size: i64, at: i64| {
                conn.execute(
                    "INSERT INTO files (id, room_id, uploader, filename, stored_name, mime,
                                        size_bytes, caption, created_at)
                     VALUES (?1, 'room_growth', ?2, 'f.bin', ?3, 'application/octet-stream',
                             ?4, '', ?5)",
                    params![id, ALICE, format!("{}.bin", id.repeat(32)), size, at],
                )
                .unwrap();
            };
            insert("aa", 10, now);
            insert("ab", 20, now);
            insert("ac", 40, now - 2 * day);
            insert("ad", 80, now - 40 * day); // outside a 30-day window

            let points = growth(conn, 30).unwrap();
            assert_eq!(points.len(), 2, "the 40-day-old upload is out of frame");
            // Oldest first, ready to draw left to right.
            assert!(points[0].day < points[1].day);
            assert_eq!((points[0].files, points[0].bytes), (1, 40));
            assert_eq!((points[1].files, points[1].bytes), (2, 30));
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
