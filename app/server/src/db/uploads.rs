//! Resumable upload sessions: the row half. The bytes live in
//! `data/uploads/{temp_name}` — see `routes/uploads.rs` for the protocol and
//! `schema.sql` for why the two are split.
//!
//! Every function here is deliberately small and total. The interesting rule is
//! in [`advance`]: the stored `received` is the only authority on where the next
//! chunk goes, and advancing it is a *conditional* update rather than a read
//! followed by a write. Two appends racing on the same session would both read
//! the same offset and both believe they were next; the `WHERE received = ?`
//! makes the loser's update affect zero rows, which the route turns into a 409.

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::Serialize;

use crate::db::now_ms;
use crate::error::{ApiError, ApiResult};

/// What kind of resource a session will become when it finishes.
///
/// Stored as a string rather than an integer so a database opened by hand is
/// readable, and matched exhaustively on the way back in so an unknown value
/// from a future build is an error rather than a default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UploadKind {
    /// A room attachment (`routes/files.rs`).
    File,
    /// An image or video (`routes/images.rs`).
    Image,
    /// A published site archive (`routes/sites.rs`).
    Site,
}

impl UploadKind {
    pub fn as_str(self) -> &'static str {
        match self {
            UploadKind::File => "file",
            UploadKind::Image => "image",
            UploadKind::Site => "site",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "file" => Some(UploadKind::File),
            "image" => Some(UploadKind::Image),
            "site" => Some(UploadKind::Site),
            _ => None,
        }
    }
}

/// One in-flight upload.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub owner: String,
    pub kind: UploadKind,
    pub room_id: Option<String>,
    pub filename: String,
    pub caption: String,
    pub mime: String,
    pub declared_size: i64,
    pub received: i64,
    pub sha256: Option<String>,
    pub temp_name: String,
    pub extra: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Session {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let kind: String = row.get("kind")?;
        Ok(Self {
            id: row.get("id")?,
            owner: row.get("owner")?,
            // A row whose kind this build does not know is not something to
            // guess at: it would finish as the wrong resource.
            kind: UploadKind::parse(&kind).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    format!("unknown upload kind {kind:?}").into(),
                )
            })?,
            room_id: row.get("room_id")?,
            filename: row.get("filename")?,
            caption: row.get("caption")?,
            mime: row.get("mime")?,
            declared_size: row.get("declared_size")?,
            received: row.get("received")?,
            sha256: row.get("sha256")?,
            temp_name: row.get("temp_name")?,
            extra: row.get("extra")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }
}

/// Everything the route has validated, ready to insert.
pub struct NewSession {
    pub id: String,
    pub owner: String,
    pub kind: UploadKind,
    pub room_id: Option<String>,
    pub filename: String,
    pub caption: String,
    pub mime: String,
    pub declared_size: i64,
    pub sha256: Option<String>,
    pub temp_name: String,
    pub extra: String,
}

pub fn create(conn: &Connection, new: NewSession) -> ApiResult<Session> {
    let now = now_ms();
    conn.execute(
        "INSERT INTO upload_sessions
           (id, owner, kind, room_id, filename, caption, mime,
            declared_size, received, sha256, temp_name, extra, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, ?10, ?11, ?12, ?12)",
        params![
            new.id,
            new.owner,
            new.kind.as_str(),
            new.room_id,
            new.filename,
            new.caption,
            new.mime,
            new.declared_size,
            new.sha256,
            new.temp_name,
            new.extra,
            now,
        ],
    )
    .map_err(|e| ApiError::Internal(e.into()))?;

    read(conn, &new.id)?.ok_or_else(|| ApiError::Internal(anyhow::anyhow!("session vanished")))
}

pub fn read(conn: &Connection, id: &str) -> ApiResult<Option<Session>> {
    conn.query_row(
        "SELECT * FROM upload_sessions WHERE id = ?1",
        params![id],
        Session::from_row,
    )
    .optional()
    .map_err(|e| ApiError::Internal(e.into()))
}

/// Move `received` from `from` to `from + written`, but only if it is still
/// `from`.
///
/// Returns whether the row moved. `false` means somebody else appended in
/// between — the caller must not treat its own write as having landed at the
/// offset it assumed, because it did not.
///
/// This is the concurrency control for the whole protocol. It is a single
/// statement rather than a transaction around a read because the condition and
/// the write have to be the same operation; anything else has a window.
pub fn advance(conn: &Connection, id: &str, from: i64, written: i64) -> ApiResult<bool> {
    let rows = conn
        .execute(
            "UPDATE upload_sessions
                SET received = received + ?3, updated_at = ?4
              WHERE id = ?1 AND received = ?2",
            params![id, from, written, now_ms()],
        )
        .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(rows == 1)
}

/// Keep a session alive without moving its offset — used by the status probe
/// so that polling a paused upload does not let the sweep reap it underneath.
pub fn touch(conn: &Connection, id: &str) -> ApiResult<()> {
    conn.execute(
        "UPDATE upload_sessions SET updated_at = ?2 WHERE id = ?1",
        params![id, now_ms()],
    )
    .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(())
}

pub fn delete(conn: &Connection, id: &str) -> ApiResult<()> {
    conn.execute("DELETE FROM upload_sessions WHERE id = ?1", params![id])
        .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(())
}

/// Sessions untouched since `cutoff`, for the sweep.
///
/// Returns whole rows rather than ids because the caller has to delete the temp
/// file too, and the name of that file is only in the row.
pub fn stale(conn: &Connection, cutoff: i64, limit: i64) -> ApiResult<Vec<Session>> {
    let mut stmt = conn
        .prepare(
            "SELECT * FROM upload_sessions
              WHERE updated_at < ?1
              ORDER BY updated_at ASC
              LIMIT ?2",
        )
        .map_err(|e| ApiError::Internal(e.into()))?;
    let rows = stmt
        .query_map(params![cutoff, limit], Session::from_row)
        .map_err(|e| ApiError::Internal(e.into()))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(rows)
}

/// Every session aimed at one room, for the purge that destroys it.
///
/// Nothing cascades here — the table has no foreign key, by the design note in
/// `schema.sql` — so an upload still in flight when its room is destroyed would
/// otherwise sit in `data/uploads/` until the age sweep noticed, holding bytes
/// somebody has just asked to have forgotten. Whole rows, like [`stale`] and
/// for the same reason: the temp file's name is only in the row.
pub fn for_room(conn: &Connection, room_id: &str) -> ApiResult<Vec<Session>> {
    let mut stmt = conn
        .prepare("SELECT * FROM upload_sessions WHERE room_id = ?1")
        .map_err(|e| ApiError::Internal(e.into()))?;
    let rows = stmt
        .query_map(params![room_id], Session::from_row)
        .map_err(|e| ApiError::Internal(e.into()))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(rows)
}

/// How many sessions this wallet has open, so one client cannot hold the disk
/// hostage with a thousand abandoned uploads.
pub fn open_count(conn: &Connection, owner: &str) -> ApiResult<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM upload_sessions WHERE owner = ?1",
        params![owner],
        |r| r.get(0),
    )
    .map_err(|e| ApiError::Internal(e.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    fn seed(conn: &Connection, id: &str, owner: &str) -> Session {
        create(
            conn,
            NewSession {
                id: id.to_owned(),
                owner: owner.to_owned(),
                kind: UploadKind::File,
                room_id: Some("room_1".to_owned()),
                filename: "big.bin".to_owned(),
                caption: String::new(),
                mime: "application/octet-stream".to_owned(),
                declared_size: 1_000,
                sha256: None,
                temp_name: format!("{id}.part"),
                extra: String::new(),
            },
        )
        .unwrap()
    }

    #[test]
    fn a_session_starts_at_zero_and_round_trips() {
        let db = test_db();
        db.call_blocking(|conn| {
            let s = seed(conn, "up_1", "0xabc");
            assert_eq!(s.received, 0);
            assert_eq!(s.kind, UploadKind::File);
            assert_eq!(s.declared_size, 1_000);

            let back = read(conn, "up_1").unwrap().unwrap();
            assert_eq!(back.temp_name, "up_1.part");
            assert!(read(conn, "up_nope").unwrap().is_none());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn advancing_only_works_from_the_offset_actually_reached() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn, "up_1", "0xabc");

            assert!(advance(conn, "up_1", 0, 400).unwrap());
            assert_eq!(read(conn, "up_1").unwrap().unwrap().received, 400);

            // The whole point: a chunk that believes it is writing at 0 when
            // 400 bytes have landed does not get to append. Replayed chunks
            // and out-of-order retries both look like this.
            assert!(!advance(conn, "up_1", 0, 400).unwrap());
            assert_eq!(read(conn, "up_1").unwrap().unwrap().received, 400);

            // And a chunk from the future cannot leave a hole in the file.
            assert!(!advance(conn, "up_1", 900, 100).unwrap());
            assert_eq!(read(conn, "up_1").unwrap().unwrap().received, 400);

            assert!(advance(conn, "up_1", 400, 600).unwrap());
            assert_eq!(read(conn, "up_1").unwrap().unwrap().received, 1_000);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn the_sweep_sees_only_what_has_gone_quiet() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn, "up_old", "0xabc");
            seed(conn, "up_new", "0xabc");
            // Backdate one of them by an hour.
            conn.execute(
                "UPDATE upload_sessions SET updated_at = ?2 WHERE id = ?1",
                params!["up_old", now_ms() - 3_600_000],
            )
            .unwrap();

            let quiet = stale(conn, now_ms() - 60_000, 10).unwrap();
            assert_eq!(quiet.len(), 1);
            assert_eq!(quiet[0].id, "up_old");

            // Touching it is what a client polling a paused upload does, and
            // it must take the row back out of the sweep's reach.
            touch(conn, "up_old").unwrap();
            assert!(stale(conn, now_ms() - 60_000, 10).unwrap().is_empty());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn open_sessions_are_counted_per_owner() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn, "up_1", "0xabc");
            seed(conn, "up_2", "0xabc");
            seed(conn, "up_3", "0xdef");

            assert_eq!(open_count(conn, "0xabc").unwrap(), 2);
            assert_eq!(open_count(conn, "0xdef").unwrap(), 1);

            delete(conn, "up_1").unwrap();
            assert_eq!(open_count(conn, "0xabc").unwrap(), 1);
            assert!(read(conn, "up_1").unwrap().is_none());
            Ok(())
        })
        .unwrap();
    }
}
