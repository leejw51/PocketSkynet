//! Attachments: metadata in SQLite, bytes on the filesystem.
//!
//! The split is the whole design. `files.stored_name` is a path *component* —
//! the row says `data/files/{stored_name}`, never the content — because the
//! alternative puts megabytes into a database whose every other row is a few
//! hundred bytes, and makes every backup and every `VACUUM` pay for it.
//!
//! Two consequences worth stating out loud, because both look like bugs:
//!
//! * **Rows are not unique per file.** Storage is content-addressed, so two
//!   people uploading identical bytes share one file on disk and get two rows.
//!   That is what makes the dedupe free, and it is why the primary key is a
//!   generated id rather than the hash.
//! * **Deleting a row does not delete the file.** It cannot: another row —
//!   possibly in another room, owned by someone else — may name the same bytes.
//!   Reference-counting across a cascade delete would need a transaction that
//!   spans the filesystem, which SQLite cannot give us. Orphans are therefore
//!   accepted, and are the price of the dedupe above.
//!
//! Accepted for a *row* delete, that is. Destroying a whole room is the one
//! caller that must not accept them — "delete the room and everything in it"
//! is a promise about the disk, not about a table — so `crate::purge` takes
//! [`stored_names_for_room`] before the cascade and [`orphan_candidates`]
//! after it, and unlinks exactly what no surviving row still names.

use rusqlite::{params, Connection, Row};
use serde::Serialize;

use crate::db::now_ms;
use crate::error::{ApiError, ApiResult};

/// One attachment, as `docs/API.md` sees it.
///
/// `storedName` is deliberately **not** on the wire: it is a filesystem
/// implementation detail, and publishing it would invite clients to build
/// their own URLs against a directory whose access rules live in the route.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FileMeta {
    pub id: String,
    #[serde(rename = "roomId")]
    pub room_id: String,
    pub uploader: String,
    pub filename: String,
    pub mime: String,
    #[serde(rename = "sizeBytes")]
    pub size_bytes: i64,
    pub caption: String,
    /// Extracted from the caption, so a client can render the chips without
    /// re-implementing the hashtag rule.
    pub tags: Vec<String>,
    /// Where to fetch the bytes. Authenticated — see `routes/files.rs`.
    pub url: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

impl FileMeta {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let id: String = row.get("id")?;
        let caption: String = row.get("caption")?;
        Ok(Self {
            url: format!("/api/files/{id}/raw"),
            tags: crate::search::text::hashtags(&caption),
            id,
            room_id: row.get("room_id")?,
            uploader: row.get("uploader")?,
            filename: row.get("filename")?,
            mime: row.get("mime")?,
            size_bytes: row.get("size_bytes")?,
            caption,
            created_at: crate::db::models::iso_ms(row.get("created_at")?),
        })
    }
}

/// Everything the route has already validated, ready to insert.
pub struct NewFile {
    pub id: String,
    pub room_id: String,
    pub uploader: String,
    pub filename: String,
    pub stored_name: String,
    pub mime: String,
    pub size_bytes: i64,
    pub caption: String,
}

const COLUMNS: &str =
    "id, room_id, uploader, filename, mime, size_bytes, caption, created_at, stored_name";

/// Insert the metadata and index the caption in the same transaction, so an
/// attachment and its searchability appear together or not at all — the same
/// contract `messages::create_message` holds.
///
/// The bytes are already on disk by the time this runs. That ordering is
/// deliberate: a file with no row is an invisible orphan, whereas a row with no
/// file is a broken download every client has to handle.
pub fn create(conn: &mut Connection, new: NewFile) -> ApiResult<FileMeta> {
    let tx = conn.transaction()?;
    let now = now_ms();
    tx.execute(
        "INSERT INTO files
             (id, room_id, uploader, filename, stored_name, mime, size_bytes, caption, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            new.id,
            new.room_id,
            new.uploader,
            new.filename,
            new.stored_name,
            new.mime,
            new.size_bytes,
            new.caption,
            now,
        ],
    )?;
    let file = read(&tx, &new.id)?
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("file vanished after insert")))?;
    crate::search::store::index_file(&tx, &file)?;
    tx.commit()?;
    Ok(file)
}

pub fn read(conn: &Connection, id: &str) -> ApiResult<Option<FileMeta>> {
    let mut stmt = conn.prepare(&format!("SELECT {COLUMNS} FROM files WHERE id = ?1"))?;
    let mut rows = stmt.query_map(params![id], FileMeta::from_row)?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

/// The `stored_name` for an id — the only place the filesystem coupling is
/// read. Kept separate from [`read`] so the wire type never has to carry it.
pub fn stored_name(conn: &Connection, id: &str) -> ApiResult<Option<String>> {
    let mut stmt = conn.prepare("SELECT stored_name FROM files WHERE id = ?1")?;
    let mut rows = stmt.query_map(params![id], |r| r.get::<_, String>(0))?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

/// Newest first, the only order the drawer reads in. `tag` filters on an exact
/// hashtag; the caller has already lowercased it.
pub fn list_for_room(
    conn: &Connection,
    room_id: &str,
    tag: Option<&str>,
    limit: i64,
) -> ApiResult<Vec<FileMeta>> {
    // Two statements rather than one with a conditional predicate: the tag
    // filter needs a join against the search index, and threading that through
    // an always-true branch would make the common query pay for it.
    let mut out = Vec::new();
    match tag {
        None => {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM files
                 WHERE room_id = ?1
                 ORDER BY created_at DESC, rowid DESC
                 LIMIT ?2"
            ))?;
            for row in stmt.query_map(params![room_id, limit], FileMeta::from_row)? {
                out.push(row?);
            }
        }
        Some(tag) => {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM files f
                 WHERE f.room_id = ?1
                   AND EXISTS (
                       SELECT 1 FROM search_docs d
                       JOIN hashtags h ON h.doc_id = d.id
                       WHERE d.kind = 'file' AND d.ref_id = f.id AND h.tag = ?2
                   )
                 ORDER BY f.created_at DESC, f.rowid DESC
                 LIMIT ?3"
            ))?;
            for row in stmt.query_map(params![room_id, tag, limit], FileMeta::from_row)? {
                out.push(row?);
            }
        }
    }
    Ok(out)
}

/// Drop the metadata and the search entry. The bytes stay — see the module
/// docs; another row may name them.
pub fn delete(conn: &mut Connection, id: &str) -> ApiResult<bool> {
    let tx = conn.transaction()?;
    let n = tx.execute("DELETE FROM files WHERE id = ?1", params![id])?;
    crate::search::store::unindex(&tx, crate::search::store::KIND_FILE, id)?;
    tx.commit()?;
    Ok(n > 0)
}

/// The bytes one room's attachments are stored in, deduped.
///
/// Read *before* the room is deleted — afterwards the rows are gone and the
/// names with them, which is the whole reason the purge is a two-step.
pub fn stored_names_for_room(conn: &Connection, room_id: &str) -> ApiResult<Vec<String>> {
    let mut stmt = conn.prepare("SELECT DISTINCT stored_name FROM files WHERE room_id = ?1")?;
    let rows = stmt.query_map(params![room_id], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Stored names no live row references any more — what a garbage collector
/// would unlink, and what `crate::purge` unlinks after a room is destroyed.
pub fn orphan_candidates(conn: &Connection, on_disk: &[String]) -> ApiResult<Vec<String>> {
    let mut out = Vec::new();
    let mut stmt = conn.prepare("SELECT 1 FROM files WHERE stored_name = ?1 LIMIT 1")?;
    for name in on_disk {
        if !stmt.exists(params![name])? {
            out.push(name.clone());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("schema.sql")).unwrap();
        conn.execute(
            "INSERT INTO rooms (id, name, current_key_version, created_at)
             VALUES ('r1', 'Room', 1, 1), ('r2', 'Other', 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO room_members (room_id, user_address, joined_at)
             VALUES ('r1', 'alice', 1), ('r2', 'alice', 1)",
            [],
        )
        .unwrap();
        conn
    }

    fn upload(conn: &mut Connection, id: &str, room: &str, caption: &str) -> FileMeta {
        create(
            conn,
            NewFile {
                id: id.into(),
                room_id: room.into(),
                uploader: "alice".into(),
                filename: "report.pdf".into(),
                stored_name: format!("{}.pdf", "a".repeat(64)),
                mime: "application/pdf".into(),
                size_bytes: 12,
                caption: caption.into(),
            },
        )
        .unwrap()
    }

    #[test]
    fn a_caption_becomes_searchable_tags() {
        let mut conn = world();
        let f = upload(&mut conn, "f1", "r1", "Q3 numbers #finance #urgent");
        assert_eq!(f.tags, vec!["finance", "urgent"]);
        // The wire url never leaks the stored name.
        assert_eq!(f.url, "/api/files/f1/raw");
        assert!(!serde_json::to_string(&f).unwrap().contains("stored_name"));
    }

    #[test]
    fn the_stored_name_is_readable_only_through_its_own_accessor() {
        let mut conn = world();
        upload(&mut conn, "f1", "r1", "");
        assert_eq!(
            stored_name(&conn, "f1").unwrap(),
            Some(format!("{}.pdf", "a".repeat(64)))
        );
        assert_eq!(stored_name(&conn, "nope").unwrap(), None);
    }

    #[test]
    fn listing_is_newest_first_and_scoped_to_one_room() {
        let mut conn = world();
        upload(&mut conn, "f1", "r1", "");
        upload(&mut conn, "f2", "r1", "");
        upload(&mut conn, "elsewhere", "r2", "");

        let ids: Vec<_> = list_for_room(&conn, "r1", None, 50)
            .unwrap()
            .into_iter()
            .map(|f| f.id)
            .collect();
        // Same millisecond, which is the case the tiebreak exists for — and
        // the case the *old* tiebreak got wrong. `id DESC` looked stable and
        // was not: a real id is `file_{millis}_{uuid}`, so within a
        // millisecond it ordered by a random UUID. This test passed anyway
        // because it hand-writes ids that happen to sort. `rowid` is
        // insertion order, which is what "newest first" meant all along.
        assert_eq!(ids, vec!["f2", "f1"]);
    }

    #[test]
    fn a_tag_filter_is_exact_not_a_substring() {
        let mut conn = world();
        upload(&mut conn, "f1", "r1", "#finance");
        upload(&mut conn, "f2", "r1", "#fin");
        let ids: Vec<_> = list_for_room(&conn, "r1", Some("fin"), 50)
            .unwrap()
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(ids, vec!["f2"], "#finance must not match a #fin filter");
    }

    #[test]
    fn deleting_drops_the_row_and_the_index_but_keeps_the_bytes() {
        let mut conn = world();
        let f = upload(&mut conn, "f1", "r1", "#finance");
        let name = stored_name(&conn, "f1").unwrap().unwrap();

        assert!(delete(&mut conn, "f1").unwrap());
        assert_eq!(read(&conn, "f1").unwrap(), None);
        assert!(
            !delete(&mut conn, "f1").unwrap(),
            "second delete is a no-op"
        );

        // The search doc is gone, so the tag no longer resolves.
        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM search_docs WHERE kind = 'file' AND ref_id = ?1",
                params![f.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 0);

        // And the bytes are now an orphan rather than having been unlinked
        // out from under a row that might still reference them elsewhere.
        assert_eq!(
            orphan_candidates(&conn, std::slice::from_ref(&name)).unwrap(),
            vec![name]
        );
    }

    #[test]
    fn identical_bytes_in_two_rooms_share_one_file_and_neither_is_an_orphan() {
        let mut conn = world();
        upload(&mut conn, "f1", "r1", "");
        upload(&mut conn, "f2", "r2", "");
        let name = stored_name(&conn, "f1").unwrap().unwrap();
        assert_eq!(stored_name(&conn, "f2").unwrap().unwrap(), name);

        // Deleting one row must not make the shared bytes collectable.
        delete(&mut conn, "f1").unwrap();
        assert!(
            orphan_candidates(&conn, &[name]).unwrap().is_empty(),
            "the surviving row still names these bytes"
        );
    }

    #[test]
    fn deleting_a_room_takes_its_files_with_it() {
        let mut conn = world();
        upload(&mut conn, "f1", "r1", "");
        conn.execute("DELETE FROM rooms WHERE id = 'r1'", [])
            .unwrap();
        assert_eq!(read(&conn, "f1").unwrap(), None, "ON DELETE CASCADE");
    }
}
