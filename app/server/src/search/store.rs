//! The knowledge index: writing documents and running hybrid retrieval
//! (docs/SEARCH.md).
//!
//! The server side of search is retrieval only. Ranking is BM25 (FTS5) fused
//! with cosine over local hashed embeddings; any AI synthesis happens on the
//! client, from these results, with the user's explicit consent — the server
//! never talks to a model, cloud or otherwise.
//!
//! # What gets indexed
//!
//! * Plaintext chat messages (`msg_type = 'add'`, `is_encrypted = 0`).
//!   **Encrypted messages are never indexed** — the server cannot read them,
//!   and no derived table may learn what the messages table does not say.
//! * Knowledge notes ("teach").
//! * Nothing else: reactions, edits and deletes mutate or remove the
//!   document they concern; they are not documents.
//!
//! # Who sees what
//!
//! Messages are scoped to rooms the searcher is currently a member of, with
//! the same blocked-sender filter as every other read path. Knowledge is
//! server-global by design — a self-hosted shared brain — and only its author
//! may delete it.

use rusqlite::{params, Connection, OptionalExtension};

use super::{embed, text};
use crate::db::models::{Message, MSG_TYPE_ADD};
use crate::error::{ApiError, ApiResult};

pub const KIND_MESSAGE: &str = "message";
pub const KIND_KNOWLEDGE: &str = "knowledge";
pub const KIND_FILE: &str = "file";
pub const KIND_SITE: &str = "site";

/// Hybrid fusion is over the top slices of each ranking, not the whole
/// corpus: 128 from each side is far deeper than any result page.
const CANDIDATES_PER_SIDE: usize = 128;

/// Reciprocal-rank-fusion constant — the standard 60: high enough that a
/// document must do well on a list to matter, low enough that rank 1 and
/// rank 10 are meaningfully different.
const RRF_K: f32 = 60.0;

// ------------------------------------------------------------- indexing ---

/// Index a freshly created message. A no-op for anything that is not
/// readable plaintext chat: encrypted rows, empty content, reaction events.
pub fn index_message(conn: &Connection, m: &Message) -> ApiResult<()> {
    if m.is_encrypted || m.content.trim().is_empty() || m.msg_type != MSG_TYPE_ADD {
        return Ok(());
    }
    upsert_doc(
        conn,
        KIND_MESSAGE,
        &m.id,
        Some(&m.room_id),
        Some(&m.sender_address),
        m.message_timestamp,
        &m.content,
    )
}

/// Re-index an edited message with its new content.
pub fn reindex_message(
    conn: &Connection,
    id: &str,
    room_id: &str,
    sender: &str,
    ts: i64,
    content: &str,
    is_encrypted: bool,
) -> ApiResult<()> {
    if is_encrypted || content.trim().is_empty() {
        return unindex(conn, KIND_MESSAGE, id);
    }
    upsert_doc(
        conn,
        KIND_MESSAGE,
        id,
        Some(room_id),
        Some(sender),
        ts,
        content,
    )
}

/// Remove one document. Deleting a message must forget it here too —
/// "forgetting-first" applies to the index as much as to the room.
pub fn unindex(conn: &Connection, kind: &str, ref_id: &str) -> ApiResult<()> {
    conn.execute(
        "DELETE FROM search_docs WHERE kind = ?1 AND ref_id = ?2",
        params![kind, ref_id],
    )?;
    Ok(())
}

/// Remove every message document of a room (delete-all, room deletion).
pub fn unindex_room_messages(conn: &Connection, room_id: &str) -> ApiResult<()> {
    conn.execute(
        "DELETE FROM search_docs WHERE kind = ?1 AND room_id = ?2",
        params![KIND_MESSAGE, room_id],
    )?;
    Ok(())
}

/// Index a published site: its title plus the readable text of its front
/// page. Global like knowledge — a hosted site is *published*, and a search
/// that cannot find it defeats the point of paying to host it.
pub fn index_site(conn: &Connection, id: &str, owner: &str, body: &str, ts: i64) -> ApiResult<()> {
    upsert_doc(conn, KIND_SITE, id, None, Some(owner), ts, body)
}

/// Index an attachment. The document is `filename + caption`, in that order,
/// because both are worth matching but only one is chosen text: a search for
/// "invoice" should find `invoice-q3.pdf` even with an empty caption, and the
/// caption is where the hashtags live.
///
/// Note that `upsert_doc` derives tags from this same string, so a `#tag` in a
/// *filename* counts too. That is a feature, not an accident — files arrive
/// named `#draft-final.pdf` more often than you would like.
pub fn index_file(conn: &Connection, f: &crate::db::files::FileMeta) -> ApiResult<()> {
    let body = if f.caption.trim().is_empty() {
        f.filename.clone()
    } else {
        format!("{} {}", f.filename, f.caption)
    };
    upsert_doc(
        conn,
        KIND_FILE,
        &f.id,
        Some(&f.room_id),
        Some(&f.uploader),
        crate::db::now_ms(),
        &body,
    )
}

fn upsert_doc(
    conn: &Connection,
    kind: &str,
    ref_id: &str,
    room_id: Option<&str>,
    sender: Option<&str>,
    ts: i64,
    body: &str,
) -> ApiResult<()> {
    let tags = text::hashtags(body);
    let blob = embed::to_blob(&embed::embed(body));
    conn.execute(
        "INSERT INTO search_docs (kind, ref_id, room_id, sender, ts, text, tags, embedding)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT (kind, ref_id) DO UPDATE SET
             room_id = excluded.room_id, sender = excluded.sender,
             ts = excluded.ts, text = excluded.text,
             tags = excluded.tags, embedding = excluded.embedding",
        params![
            kind,
            ref_id,
            room_id,
            sender,
            ts,
            body,
            tags.join(" "),
            blob
        ],
    )?;
    let doc_id: i64 = conn.query_row(
        "SELECT id FROM search_docs WHERE kind = ?1 AND ref_id = ?2",
        params![kind, ref_id],
        |r| r.get(0),
    )?;
    conn.execute("DELETE FROM hashtags WHERE doc_id = ?1", params![doc_id])?;
    for tag in &tags {
        conn.execute(
            "INSERT OR IGNORE INTO hashtags (tag, doc_id) VALUES (?1, ?2)",
            params![tag, doc_id],
        )?;
    }
    Ok(())
}

/// Index every readable message the index does not know yet — the upgrade
/// path for databases that predate search. Runs once at startup; on an
/// already-indexed database the anti-join finds nothing and this is one
/// query. Returns how many documents were added.
pub fn backfill(conn: &Connection) -> ApiResult<usize> {
    let mut stmt = conn.prepare(
        "SELECT id, room_id, sender_address, message_timestamp, content
         FROM messages m
         WHERE m.msg_type = 'add' AND m.is_encrypted = 0 AND m.is_deleted = 0
           AND m.content != ''
           AND NOT EXISTS (SELECT 1 FROM search_docs d
                           WHERE d.kind = 'message' AND d.ref_id = m.id)",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut added = 0;
    for row in rows {
        let (id, room_id, sender, ts, content) = row?;
        upsert_doc(
            conn,
            KIND_MESSAGE,
            &id,
            Some(&room_id),
            Some(&sender),
            ts,
            &content,
        )?;
        added += 1;
    }
    Ok(added)
}

// ------------------------------------------------------------ knowledge ---

#[derive(Debug, Clone, serde::Serialize)]
pub struct KnowledgeNote {
    pub id: String,
    #[serde(rename = "ownerAddress")]
    pub owner_address: String,
    pub content: String,
    #[serde(rename = "roomId")]
    pub room_id: Option<String>,
    #[serde(rename = "sourceMessageId")]
    pub source_message_id: Option<String>,
    pub tags: Vec<String>,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(rename = "updatedAt")]
    pub updated_at: i64,
}

/// Teach: store a knowledge note and index it in the same breath.
pub fn teach(
    conn: &Connection,
    id: &str,
    owner: &str,
    content: &str,
    room_id: Option<&str>,
    source_message_id: Option<&str>,
    now: i64,
) -> ApiResult<KnowledgeNote> {
    conn.execute(
        "INSERT INTO knowledge_notes
             (id, owner_address, content, room_id, source_message_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![id, owner, content, room_id, source_message_id, now],
    )?;
    upsert_doc(conn, KIND_KNOWLEDGE, id, room_id, Some(owner), now, content)?;
    Ok(KnowledgeNote {
        id: id.to_owned(),
        owner_address: owner.to_owned(),
        content: content.to_owned(),
        room_id: room_id.map(str::to_owned),
        source_message_id: source_message_id.map(str::to_owned),
        tags: text::hashtags(content),
        created_at: now,
        updated_at: now,
    })
}

/// Delete a note — author only. `Ok(false)` when the note exists but belongs
/// to someone else, so the route can 403 rather than 404.
pub fn forget(conn: &Connection, id: &str, caller: &str) -> ApiResult<Option<bool>> {
    let owner: Option<String> = conn
        .query_row(
            "SELECT owner_address FROM knowledge_notes WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(owner) = owner else { return Ok(None) };
    if owner != caller {
        return Ok(Some(false));
    }
    conn.execute("DELETE FROM knowledge_notes WHERE id = ?1", params![id])?;
    unindex(conn, KIND_KNOWLEDGE, id)?;
    Ok(Some(true))
}

/// Newest-first page of notes, optionally only one owner's.
pub fn list_knowledge(
    conn: &Connection,
    owner: Option<&str>,
    limit: usize,
) -> ApiResult<Vec<KnowledgeNote>> {
    let mut stmt = conn.prepare(
        "SELECT id, owner_address, content, room_id, source_message_id, created_at, updated_at
         FROM knowledge_notes
         WHERE (?1 IS NULL OR owner_address = ?1)
         ORDER BY created_at DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![owner, limit as i64], |r| {
        Ok(KnowledgeNote {
            id: r.get(0)?,
            owner_address: r.get(1)?,
            content: r.get(2)?,
            room_id: r.get(3)?,
            source_message_id: r.get(4)?,
            tags: Vec::new(),
            created_at: r.get(5)?,
            updated_at: r.get(6)?,
        })
    })?;
    let mut notes = Vec::new();
    for note in rows {
        let mut note = note?;
        note.tags = text::hashtags(&note.content);
        notes.push(note);
    }
    Ok(notes)
}

// --------------------------------------------------------------- search ---

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    pub kind: String,
    #[serde(rename = "refId")]
    pub ref_id: String,
    #[serde(rename = "roomId")]
    pub room_id: Option<String>,
    pub sender: Option<String>,
    #[serde(rename = "timestamp")]
    pub ts: i64,
    pub text: String,
    pub tags: Vec<String>,
    /// Fused relevance, higher is better. Meaningful only for ordering
    /// within one response — not comparable across queries.
    pub score: f32,
}

/// The membership + block scope every search runs inside. `?1` is the
/// searcher. Knowledge is global; messages and files require current room
/// membership and an unblocked uploader.
///
/// This is an **allow-list on `d.kind`**: a kind that is not named here is
/// indexed but unfindable, silently, with no error anywhere. Adding a kind to
/// the index without adding it here is the mistake this comment exists to
/// prevent. Files get the message rule verbatim — an attachment is exactly as
/// private as the room it was posted in.
const VISIBLE: &str = "(d.kind IN ('knowledge', 'site')
      OR (d.kind IN ('message', 'file')
          AND d.room_id IN (SELECT room_id FROM room_members WHERE user_address = ?1)
          AND d.sender NOT IN
              (SELECT blocked_address FROM blocked_users WHERE blocker_address = ?1)))";

/// Hybrid retrieval: BM25 and cosine ranked independently over the visible
/// scope, fused by reciprocal rank. `#tags` in the query become hard filters.
/// A tag-only query is a browse: newest tagged documents first.
pub fn search(
    conn: &Connection,
    viewer: &str,
    query: &str,
    kind: Option<&str>,
    limit: usize,
) -> ApiResult<Vec<SearchHit>> {
    let (text_part, tags) = text::split_query(query);
    let limit = limit.clamp(1, 100);

    // Tag filter as a doc-id set. Documents must carry EVERY queried tag.
    let tag_filter: Option<Vec<i64>> = if tags.is_empty() {
        None
    } else {
        let mut ids: Option<Vec<i64>> = None;
        for tag in &tags {
            let mut stmt = conn.prepare("SELECT doc_id FROM hashtags WHERE tag = ?1")?;
            let set: Vec<i64> = stmt
                .query_map(params![tag], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            ids = Some(match ids {
                None => set,
                Some(prev) => prev.into_iter().filter(|id| set.contains(id)).collect(),
            });
        }
        Some(ids.unwrap_or_default())
    };

    let allowed = |doc_id: i64| tag_filter.as_ref().is_none_or(|ids| ids.contains(&doc_id));

    // Tag-only query: browse newest-first within the tag set.
    if text::fts_query(&text_part).is_none() {
        return browse(conn, viewer, kind, tag_filter.as_deref(), limit);
    }

    // BM25 side. FTS5's `rank` is ascending-better (more negative = better).
    let fts = text::fts_query(&text_part).expect("checked above");
    let mut bm25_ranked: Vec<i64> = Vec::new();
    {
        let sql = format!(
            "SELECT d.id FROM search_fts f
             JOIN search_docs d ON d.id = f.rowid
             WHERE search_fts MATCH ?2 AND {VISIBLE}
               AND (?3 IS NULL OR d.kind = ?3)
             ORDER BY f.rank LIMIT ?4"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![viewer, fts, kind, CANDIDATES_PER_SIDE as i64],
            |r| r.get::<_, i64>(0),
        )?;
        for row in rows {
            let id = row?;
            if allowed(id) {
                bm25_ranked.push(id);
            }
        }
    }

    // Semantic side: brute-force cosine over the visible scope.
    let query_vec = embed::embed(&text_part);
    let mut semantic: Vec<(i64, f32)> = Vec::new();
    {
        let sql = format!(
            "SELECT d.id, d.embedding FROM search_docs d
             WHERE {VISIBLE} AND (?2 IS NULL OR d.kind = ?2)"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![viewer, kind], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        for row in rows {
            let (id, blob) = row?;
            if !allowed(id) {
                continue;
            }
            if let Some(vector) = embed::from_blob(&blob) {
                let score = embed::cosine(&query_vec, &vector);
                if score > 0.0 {
                    semantic.push((id, score));
                }
            }
        }
    }
    semantic.sort_by(|a, b| b.1.total_cmp(&a.1));
    semantic.truncate(CANDIDATES_PER_SIDE);

    // Reciprocal-rank fusion.
    let mut fused: std::collections::HashMap<i64, f32> = std::collections::HashMap::new();
    for (rank, id) in bm25_ranked.iter().enumerate() {
        *fused.entry(*id).or_default() += 1.0 / (RRF_K + rank as f32 + 1.0);
    }
    for (rank, (id, _)) in semantic.iter().enumerate() {
        *fused.entry(*id).or_default() += 1.0 / (RRF_K + rank as f32 + 1.0);
    }
    let mut order: Vec<(i64, f32)> = fused.into_iter().collect();
    order.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.0.cmp(&a.0)));
    order.truncate(limit);

    hits_for(conn, &order)
}

/// Newest-first documents, optionally within a tag set — the no-query browse.
fn browse(
    conn: &Connection,
    viewer: &str,
    kind: Option<&str>,
    tag_filter: Option<&[i64]>,
    limit: usize,
) -> ApiResult<Vec<SearchHit>> {
    let sql = format!(
        "SELECT d.id FROM search_docs d
         WHERE {VISIBLE} AND (?2 IS NULL OR d.kind = ?2)
         ORDER BY d.ts DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![viewer, kind], |r| r.get::<_, i64>(0))?;
    let mut order = Vec::new();
    for row in rows {
        let id = row?;
        if tag_filter.is_none_or(|ids| ids.contains(&id)) {
            order.push((id, 0.0));
            if order.len() == limit {
                break;
            }
        }
    }
    hits_for(conn, &order)
}

fn hits_for(conn: &Connection, order: &[(i64, f32)]) -> ApiResult<Vec<SearchHit>> {
    let mut stmt = conn.prepare(
        "SELECT kind, ref_id, room_id, sender, ts, text, tags
         FROM search_docs WHERE id = ?1",
    )?;
    let mut hits = Vec::with_capacity(order.len());
    for &(id, score) in order {
        let hit = stmt
            .query_row(params![id], |r| {
                Ok(SearchHit {
                    kind: r.get(0)?,
                    ref_id: r.get(1)?,
                    room_id: r.get(2)?,
                    sender: r.get(3)?,
                    ts: r.get(4)?,
                    text: r.get(5)?,
                    tags: r
                        .get::<_, String>(6)?
                        .split_whitespace()
                        .map(str::to_owned)
                        .collect(),
                    score,
                })
            })
            .optional()?;
        if let Some(hit) = hit {
            hits.push(hit);
        }
    }
    Ok(hits)
}

/// Tags visible to the viewer with document counts, most-used first.
pub fn tag_counts(conn: &Connection, viewer: &str, limit: usize) -> ApiResult<Vec<(String, i64)>> {
    let sql = format!(
        "SELECT h.tag, COUNT(*) FROM hashtags h
         JOIN search_docs d ON d.id = h.doc_id
         WHERE {VISIBLE}
         GROUP BY h.tag ORDER BY COUNT(*) DESC, h.tag LIMIT ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![viewer, limit as i64], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })?;
    let mut tags = Vec::new();
    for row in rows {
        tags.push(row?);
    }
    Ok(tags)
}

// A tiny consistency guard: ApiError::from(rusqlite::Error) must exist for
// the ?s above; this keeps the compile error here if it ever changes shape.
const _: fn(rusqlite::Error) -> ApiError = |e| ApiError::from(e);

#[cfg(test)]
mod tests {
    use super::*;

    /// A schema-loaded in-memory database with two rooms and three users:
    /// alice and bob share "shared"; carol is in neither; alice alone is in
    /// "private".
    fn world() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../db/schema.sql"))
            .unwrap();
        for (id, name) in [("shared", "Shared"), ("private", "Private")] {
            conn.execute(
                "INSERT INTO rooms (id, name, created_at) VALUES (?1, ?2, 0)",
                params![id, name],
            )
            .unwrap();
        }
        for (room, user) in [("shared", "alice"), ("shared", "bob"), ("private", "alice")] {
            conn.execute(
                "INSERT INTO room_members (room_id, user_address, joined_at) VALUES (?1, ?2, 0)",
                params![room, user],
            )
            .unwrap();
        }
        conn
    }

    fn msg(id: &str, room: &str, sender: &str, ts: i64, content: &str) -> Message {
        Message {
            id: id.to_owned(),
            room_id: room.to_owned(),
            sender_address: sender.to_owned(),
            content: content.to_owned(),
            msg_hash: String::new(),
            message_timestamp: ts,
            msg_type: "add".to_owned(),
            msg_serial: ts,
            is_deleted: false,
            edited_at: None,
            created_at: String::new(),
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
        }
    }

    fn texts(hits: &[SearchHit]) -> Vec<&str> {
        hits.iter().map(|h| h.text.as_str()).collect()
    }

    #[test]
    fn a_plaintext_message_becomes_searchable() {
        let conn = world();
        index_message(
            &conn,
            &msg("m1", "shared", "alice", 1, "the wifi password is hunter2"),
        )
        .unwrap();
        let hits = search(&conn, "bob", "wifi password", None, 10).unwrap();
        assert_eq!(texts(&hits), ["the wifi password is hunter2"]);
    }

    #[test]
    fn an_encrypted_message_is_never_indexed() {
        let conn = world();
        let mut m = msg("m1", "shared", "alice", 1, "ciphertext-base64-blob");
        m.is_encrypted = true;
        index_message(&conn, &m).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM search_docs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 0,
            "an encrypted message must leave no trace in the index"
        );
    }

    #[test]
    fn reaction_events_are_not_documents() {
        let conn = world();
        let mut m = msg("m1", "shared", "alice", 1, "👍");
        m.msg_type = "emoticon_add".to_owned();
        index_message(&conn, &m).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM search_docs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn membership_scopes_message_results() {
        let conn = world();
        index_message(
            &conn,
            &msg("m1", "private", "alice", 1, "secret launch date friday"),
        )
        .unwrap();
        // Alice, a member, finds it; bob and carol do not.
        assert_eq!(
            search(&conn, "alice", "launch date", None, 10)
                .unwrap()
                .len(),
            1
        );
        assert!(search(&conn, "bob", "launch date", None, 10)
            .unwrap()
            .is_empty());
        assert!(search(&conn, "carol", "launch date", None, 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn blocked_senders_are_filtered_out() {
        let conn = world();
        index_message(
            &conn,
            &msg("m1", "shared", "bob", 1, "spam about crypto gains"),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO blocked_users (blocker_address, blocked_address, created_at)
             VALUES ('alice', 'bob', 0)",
            [],
        )
        .unwrap();
        assert!(search(&conn, "alice", "crypto gains", None, 10)
            .unwrap()
            .is_empty());
        // The block is one-directional and per-viewer.
        assert_eq!(
            search(&conn, "bob", "crypto gains", None, 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn bm25_prefers_the_denser_match() {
        let conn = world();
        index_message(&conn, &msg("m1", "shared", "alice", 1, "coffee")).unwrap();
        index_message(
            &conn,
            &msg(
                "m2",
                "shared",
                "alice",
                2,
                "meeting notes budget planning roadmap",
            ),
        )
        .unwrap();
        let hits = search(&conn, "alice", "coffee", None, 10).unwrap();
        assert_eq!(hits[0].text, "coffee");
    }

    #[test]
    fn a_typo_still_retrieves_through_the_semantic_side() {
        let conn = world();
        index_message(
            &conn,
            &msg(
                "m1",
                "shared",
                "alice",
                1,
                "kubernetes cluster upgrade steps",
            ),
        )
        .unwrap();
        index_message(
            &conn,
            &msg("m2", "shared", "alice", 2, "grocery list eggs milk"),
        )
        .unwrap();
        // "kubernets" matches no FTS token — only trigram cosine finds it.
        let hits = search(&conn, "alice", "kubernets", None, 10).unwrap();
        assert_eq!(
            hits.first().map(|h| h.text.as_str()),
            Some("kubernetes cluster upgrade steps")
        );
    }

    #[test]
    fn korean_query_finds_korean_message() {
        let conn = world();
        index_message(
            &conn,
            &msg(
                "m1",
                "shared",
                "alice",
                1,
                "김치찌개 레시피: 돼지고기와 묵은지",
            ),
        )
        .unwrap();
        index_message(
            &conn,
            &msg("m2", "shared", "alice", 2, "car insurance renewal"),
        )
        .unwrap();
        let hits = search(&conn, "bob", "김치찌개 레시피", None, 10).unwrap();
        assert_eq!(
            hits.first().map(|h| h.text.as_str()),
            Some("김치찌개 레시피: 돼지고기와 묵은지")
        );
    }

    #[test]
    fn editing_a_message_reindexes_it() {
        let conn = world();
        index_message(&conn, &msg("m1", "shared", "alice", 1, "old content here")).unwrap();
        reindex_message(
            &conn,
            "m1",
            "shared",
            "alice",
            2,
            "the new topic is pottery",
            false,
        )
        .unwrap();
        assert!(search(&conn, "alice", "old content", None, 10)
            .unwrap()
            .is_empty());
        assert_eq!(
            search(&conn, "alice", "pottery", None, 10).unwrap().len(),
            1
        );
    }

    #[test]
    fn deleting_a_message_forgets_it() {
        let conn = world();
        index_message(
            &conn,
            &msg("m1", "shared", "alice", 1, "remember this number 42"),
        )
        .unwrap();
        unindex(&conn, KIND_MESSAGE, "m1").unwrap();
        assert!(search(&conn, "alice", "number 42", None, 10)
            .unwrap()
            .is_empty());
        // The hashtag rows cascade with the doc.
        let orphans: i64 = conn
            .query_row("SELECT COUNT(*) FROM hashtags", [], |r| r.get(0))
            .unwrap();
        assert_eq!(orphans, 0);
    }

    #[test]
    fn deleting_a_room_forgets_all_its_messages() {
        let conn = world();
        index_message(&conn, &msg("m1", "shared", "alice", 1, "first shared note")).unwrap();
        index_message(&conn, &msg("m2", "shared", "bob", 2, "second shared note")).unwrap();
        index_message(&conn, &msg("m3", "private", "alice", 3, "private note")).unwrap();
        unindex_room_messages(&conn, "shared").unwrap();
        // "note" still OR-matches the surviving private doc — the property
        // is that nothing from the deleted room can come back.
        let hits = search(&conn, "alice", "shared note", None, 10).unwrap();
        assert!(
            hits.iter().all(|h| h.room_id.as_deref() != Some("shared")),
            "{hits:?}"
        );
        assert_eq!(
            search(&conn, "alice", "private note", None, 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn taught_knowledge_is_global_and_searchable_by_anyone() {
        let conn = world();
        teach(
            &conn,
            "k1",
            "alice",
            "server rack fuse is in the garage #home",
            None,
            None,
            1,
        )
        .unwrap();
        // Carol shares no room with alice, and still finds knowledge.
        let hits = search(&conn, "carol", "where is the fuse", None, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, KIND_KNOWLEDGE);
        assert_eq!(hits[0].tags, ["home"]);
    }

    #[test]
    fn only_the_author_may_forget_knowledge() {
        let conn = world();
        teach(&conn, "k1", "alice", "a fact worth keeping", None, None, 1).unwrap();
        assert_eq!(forget(&conn, "k1", "bob").unwrap(), Some(false));
        assert_eq!(
            search(&conn, "bob", "fact worth keeping", None, 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(forget(&conn, "k1", "alice").unwrap(), Some(true));
        assert!(search(&conn, "bob", "fact worth keeping", None, 10)
            .unwrap()
            .is_empty());
        assert_eq!(forget(&conn, "missing", "alice").unwrap(), None);
    }

    #[test]
    fn hashtag_query_is_a_hard_filter() {
        let conn = world();
        index_message(
            &conn,
            &msg("m1", "shared", "alice", 1, "#recipe kimchi with pork"),
        )
        .unwrap();
        index_message(
            &conn,
            &msg("m2", "shared", "alice", 2, "kimchi photo from the market"),
        )
        .unwrap();
        let hits = search(&conn, "alice", "#recipe kimchi", None, 10).unwrap();
        assert_eq!(texts(&hits), ["#recipe kimchi with pork"]);
    }

    #[test]
    fn a_tag_only_query_browses_newest_first() {
        let conn = world();
        index_message(&conn, &msg("m1", "shared", "alice", 1, "#todo buy milk")).unwrap();
        index_message(
            &conn,
            &msg("m2", "shared", "alice", 2, "#todo call the bank"),
        )
        .unwrap();
        index_message(&conn, &msg("m3", "shared", "alice", 3, "unrelated chatter")).unwrap();
        let hits = search(&conn, "alice", "#todo", None, 10).unwrap();
        assert_eq!(texts(&hits), ["#todo call the bank", "#todo buy milk"]);
    }

    #[test]
    fn multiple_tags_require_all_of_them() {
        let conn = world();
        index_message(
            &conn,
            &msg("m1", "shared", "alice", 1, "#work #urgent ship the fix"),
        )
        .unwrap();
        index_message(
            &conn,
            &msg("m2", "shared", "alice", 2, "#work regular standup"),
        )
        .unwrap();
        let hits = search(&conn, "alice", "#work #urgent", None, 10).unwrap();
        assert_eq!(texts(&hits), ["#work #urgent ship the fix"]);
    }

    #[test]
    fn kind_filter_narrows_to_knowledge() {
        let conn = world();
        index_message(
            &conn,
            &msg("m1", "shared", "alice", 1, "backup runs nightly"),
        )
        .unwrap();
        teach(
            &conn,
            "k1",
            "alice",
            "backup key is in the safe",
            None,
            None,
            2,
        )
        .unwrap();
        let hits = search(&conn, "alice", "backup", Some(KIND_KNOWLEDGE), 10).unwrap();
        assert_eq!(texts(&hits), ["backup key is in the safe"]);
    }

    #[test]
    fn tag_counts_respect_visibility() {
        let conn = world();
        index_message(&conn, &msg("m1", "shared", "alice", 1, "#food kimchi")).unwrap();
        index_message(&conn, &msg("m2", "shared", "bob", 2, "#food bibimbap")).unwrap();
        index_message(
            &conn,
            &msg("m3", "private", "alice", 3, "#food secret sauce"),
        )
        .unwrap();
        assert_eq!(
            tag_counts(&conn, "alice", 10).unwrap(),
            [("food".to_owned(), 3)]
        );
        assert_eq!(
            tag_counts(&conn, "bob", 10).unwrap(),
            [("food".to_owned(), 2)]
        );
        assert!(tag_counts(&conn, "carol", 10).unwrap().is_empty());
    }

    #[test]
    fn an_empty_query_browses_everything_visible() {
        let conn = world();
        index_message(&conn, &msg("m1", "shared", "alice", 5, "newest")).unwrap();
        index_message(&conn, &msg("m2", "shared", "alice", 1, "oldest")).unwrap();
        let hits = search(&conn, "alice", "", None, 10).unwrap();
        assert_eq!(texts(&hits), ["newest", "oldest"]);
    }

    #[test]
    fn results_never_exceed_the_limit() {
        let conn = world();
        for i in 0..30 {
            index_message(
                &conn,
                &msg(&format!("m{i}"), "shared", "alice", i, "repeated topic"),
            )
            .unwrap();
        }
        assert_eq!(
            search(&conn, "alice", "repeated topic", None, 7)
                .unwrap()
                .len(),
            7
        );
    }

    #[test]
    fn list_knowledge_pages_newest_first_and_filters_by_owner() {
        let conn = world();
        teach(&conn, "k1", "alice", "first fact", None, None, 1).unwrap();
        teach(&conn, "k2", "bob", "second fact", None, None, 2).unwrap();
        let all = list_knowledge(&conn, None, 10).unwrap();
        assert_eq!(
            all.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            ["k2", "k1"]
        );
        let alices = list_knowledge(&conn, Some("alice"), 10).unwrap();
        assert_eq!(alices.len(), 1);
        assert_eq!(alices[0].id, "k1");
    }

    #[test]
    fn backfill_indexes_old_plaintext_but_never_encrypted_or_deleted_rows() {
        let conn = world();
        // Rows written "before search existed": straight SQL, no hooks.
        let insert = "INSERT INTO messages (id, room_id, sender_address, content, msg_hash,
                          message_timestamp, msg_type, msg_serial, is_deleted, created_at,
                          is_encrypted)
                      VALUES (?1, 'shared', 'alice', ?2, '', ?3, ?4, ?3, ?5, ?3, ?6)";
        conn.execute(
            insert,
            params!["m1", "an old plaintext message", 1, "add", 0, 0],
        )
        .unwrap();
        conn.execute(insert, params!["m2", "old ciphertext", 2, "add", 0, 1])
            .unwrap();
        conn.execute(insert, params!["m3", "was deleted", 3, "add", 1, 0])
            .unwrap();
        conn.execute(insert, params!["m4", "👍", 4, "emoticon_add", 0, 0])
            .unwrap();

        assert_eq!(backfill(&conn).unwrap(), 1);
        let hits = search(&conn, "alice", "old", None, 10).unwrap();
        assert_eq!(texts(&hits), ["an old plaintext message"]);
        // Second run: nothing left to do.
        assert_eq!(backfill(&conn).unwrap(), 0);
    }

    #[test]
    fn fts_syntax_in_a_query_cannot_break_the_search() {
        let conn = world();
        index_message(
            &conn,
            &msg("m1", "shared", "alice", 1, "perfectly normal message"),
        )
        .unwrap();
        for hostile in ["\"unbalanced", "a AND b OR c*", "NEAR/3 (x y)", "col:value"] {
            // Must not error — quoting neutralises the operators.
            let _ = search(&conn, "alice", hostile, None, 10).unwrap();
        }
    }
}
