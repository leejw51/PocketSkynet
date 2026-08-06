//! Messages, reactions, and the per-room serial counter that drives `/sync`.

use std::collections::HashSet;

use rusqlite::{params, Connection, OptionalExtension};

use super::models::{
    EmoticonAggregation, Message, User, MESSAGE_JOIN_COLUMNS, MESSAGE_THREAD_COLUMNS, MSG_TYPE_ADD,
    MSG_TYPE_DELETE, MSG_TYPE_DELETE_ALL, MSG_TYPE_EDIT, MSG_TYPE_EMOTICON_ADD,
    MSG_TYPE_EMOTICON_REMOVE,
};
use super::now_ms;
use crate::error::{ApiError, ApiResult};
use crate::validate::MAX_SAFE_INT;

/// `/sync` never returns more than this many rows in one response; one extra
/// row is fetched to decide `X-Has-More`. Bounding the page is what makes a
/// cold start at `since=0` safe on a room with a decade of history.
pub const SYNC_LIMIT: i64 = 500;

/// Types that are event records rather than displayable messages.
const EVENT_TYPES: &str = "'emoticon_add', 'emoticon_remove', 'delete_all'";

/// Allocate the next `msgSerial` for a room.
///
/// **§15 #2, the single most important fix in this port.** The reference kept
/// the last issued serial in a process-local `Map`, so two replicas sharing
/// one database could hand out the same serial in the same millisecond — and
/// a client paging on `msg_serial > since` would silently skip one of the two
/// messages, permanently. Here the counter is a row, and this statement runs
/// inside the same transaction as the insert that consumes it, so the
/// allocation is serialised by SQLite itself.
///
/// The `max(stored, now)` keeps serials timestamp-like (they track wall-clock
/// milliseconds, which is what makes them roughly comparable across rooms)
/// while the `+ 1` guarantees strict monotonicity even when several messages
/// land inside one millisecond, or when the clock steps backwards.
pub fn next_serial(conn: &Connection, room_id: &str) -> ApiResult<i64> {
    let now = now_ms();
    let serial: i64 = conn.query_row(
        "INSERT INTO room_serials (room_id, next_serial) VALUES (?1, ?2 + 1)
         ON CONFLICT (room_id) DO UPDATE SET
             next_serial = MAX(room_serials.next_serial, ?2) + 1
         RETURNING next_serial - 1",
        params![room_id, now],
        |r| r.get(0),
    )?;

    if serial > MAX_SAFE_INT {
        // Past this, JavaScript clients silently lose precision and their
        // cursors stop advancing. Refusing is the only honest option.
        return Err(ApiError::Internal(anyhow::anyhow!(
            "message serial exceeded the JavaScript safe integer range"
        )));
    }
    Ok(serial)
}

/// The block filter used on every viewer-facing read path.
///
/// Written as a correlated subquery rather than a materialised `NOT IN (...)`
/// list so the SQL text is constant: a parameter list whose length varies per
/// request defeats SQLite's statement cache and makes the query harder to read
/// than the single join it replaces.
const NOT_BLOCKED: &str = "m.sender_address NOT IN \
    (SELECT blocked_address FROM blocked_users WHERE blocker_address = :viewer)";

// -------------------------------------------------------------- creation ---

/// Everything the caller controls about a new message. Server-owned fields
/// (`id`, `senderAddress`, `msgSerial`, `msgType`, timestamps) are not here by
/// design — a client cannot set them even by accident.
pub struct NewMessage {
    pub id: String,
    pub room_id: String,
    pub sender: String,
    pub content: String,
    pub msg_hash: String,
    pub is_encrypted: bool,
    pub iv: Option<String>,
    pub hmac: Option<String>,
    pub enc_ver: i64,
    pub key_version: i64,
    /// The thread to post into, already resolved to its root by the route.
    /// `None` is a top-level message.
    pub parent_message_id: Option<String>,
    /// Addresses this message names. Written as `message_mentions` rows in the
    /// same transaction as the message, so a mention cannot exist for a
    /// message that failed to insert, nor a message arrive in a room without
    /// the inbox entry that was supposed to accompany it.
    pub mentions: Vec<String>,
}

pub fn create_message(conn: &mut Connection, new: NewMessage) -> ApiResult<Message> {
    let tx = conn.transaction()?;
    let now = now_ms();
    let serial = next_serial(&tx, &new.room_id)?;

    tx.execute(
        "INSERT INTO messages (id, room_id, sender_address, content, msg_hash,
                               message_timestamp, msg_type, msg_serial, is_deleted,
                               edited_at, created_at, is_encrypted, iv, hmac,
                               enc_ver, key_version, tx_hash, target_message_id, emoticon_code,
                               parent_message_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, NULL, ?6, ?9, ?10, ?11, ?12, ?13,
                 NULL, NULL, NULL, ?14)",
        params![
            new.id,
            new.room_id,
            new.sender,
            new.content,
            new.msg_hash,
            now,
            MSG_TYPE_ADD,
            serial,
            i64::from(new.is_encrypted),
            new.iv,
            new.hmac,
            new.enc_ver,
            new.key_version,
            new.parent_message_id,
        ],
    )?;

    super::mentions::record(&tx, &new.id, &new.room_id, serial, &new.mentions)?;

    let message = read_message(&tx, &new.id, true)?
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("message vanished after insert")))?;
    // Inside the transaction on purpose: a message and its index entry
    // appear together or not at all. Encrypted rows no-op inside.
    crate::search::store::index_message(&tx, &message)?;
    tx.commit()?;
    Ok(message)
}

/// Every reply in a thread, oldest first, with the root at the head.
///
/// The root is fetched separately rather than with an `OR id = :root` on the
/// reply query, because it has to be there even when it was soft-deleted:
/// destroying the first message of a thread must not orphan the twenty replies
/// under it, and the tombstone is what tells the client to render "message
/// deleted" above them rather than an unexplained list.
///
/// Block-filtered like every other read path — including the root, which is
/// why the return is an `Option`: a thread started by somebody the viewer has
/// blocked is a thread they cannot open.
pub fn get_thread(
    conn: &Connection,
    root_id: &str,
    viewer: &str,
) -> ApiResult<Option<Vec<Message>>> {
    let root_sql = format!(
        "SELECT {MESSAGE_JOIN_COLUMNS} FROM messages m
         LEFT JOIN users u ON u.wallet_address = m.sender_address
         WHERE m.id = :root AND {NOT_BLOCKED}"
    );
    let now = now_ms();
    let root = conn
        .query_row(
            &root_sql,
            rusqlite::named_params! { ":root": root_id, ":viewer": viewer },
            |row| Message::from_joined_row(row, now),
        )
        .optional()?;
    let Some(root) = root else {
        return Ok(None);
    };

    let sql = format!(
        "SELECT {MESSAGE_JOIN_COLUMNS} FROM messages m
         LEFT JOIN users u ON u.wallet_address = m.sender_address
         WHERE m.parent_message_id = :root
           AND m.is_deleted = 0
           AND m.msg_type NOT IN ({EVENT_TYPES})
           AND {NOT_BLOCKED}
         ORDER BY m.message_timestamp ASC, m.msg_serial ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        rusqlite::named_params! { ":root": root_id, ":viewer": viewer },
        |row| Message::from_joined_row(row, now),
    )?;

    let mut out = vec![root];
    out.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
    Ok(Some(out))
}

/// The thread a reply to `id` belongs in.
///
/// Replying to a reply joins the thread that reply is already in rather than
/// starting a nested one — one level, always. Anything deeper would make a
/// thread a tree whose depth no renderer can bound and whose "reply count"
/// would have to mean something different at every level.
///
/// `include_deleted` splits the two callers, and they genuinely want opposite
/// things. *Replying* to a deleted message must fail — there is nothing left
/// to answer. *Reading* the thread of one must not, or destroying the first
/// message would take the whole conversation under it with it.
///
/// `None` means there is no such message at all.
pub fn thread_root(
    conn: &Connection,
    id: &str,
    include_deleted: bool,
) -> ApiResult<Option<(String, String)>> {
    let sql = format!(
        "SELECT id, parent_message_id, room_id FROM messages WHERE id = ?1 {}",
        if include_deleted {
            ""
        } else {
            "AND is_deleted = 0"
        }
    );
    let found: Option<(String, Option<String>, String)> = conn
        .query_row(&sql, params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    let Some((own_id, parent, room_id)) = found else {
        return Ok(None);
    };
    Ok(Some((parent.unwrap_or(own_id), room_id)))
}

/// Append a reaction event.
///
/// Reactions are messages, not a side table, so they travel through `/sync`
/// on the same cursor as everything else and fold deterministically. Adding a
/// reaction twice simply appends a second event: aggregation is set-based, so
/// the visible result is identical and no duplicate check is needed
/// (§15 #15 — the reference's "already added" 400 was unreachable code).
pub fn create_emoticon_event(
    conn: &mut Connection,
    id: &str,
    room_id: &str,
    sender: &str,
    target_message_id: &str,
    code: &str,
    add: bool,
) -> ApiResult<Message> {
    let tx = conn.transaction()?;
    let now = now_ms();
    let serial = next_serial(&tx, room_id)?;
    let msg_type = if add {
        MSG_TYPE_EMOTICON_ADD
    } else {
        MSG_TYPE_EMOTICON_REMOVE
    };

    // The preimage is a plain UTF-8 string joined with ':'. Reproduced
    // byte-for-byte from the reference so a client can recompute it.
    let action = if add { "add" } else { "remove" };
    let hash = sha256_hex(&format!(
        "{target_message_id}:{code}:{action}:{sender}:{now}"
    ));

    tx.execute(
        "INSERT INTO messages (id, room_id, sender_address, content, msg_hash,
                               message_timestamp, msg_type, msg_serial, is_deleted,
                               edited_at, created_at, is_encrypted, iv, hmac,
                               enc_ver, key_version, tx_hash, target_message_id, emoticon_code)
         VALUES (?1, ?2, ?3, '', ?4, ?5, ?6, ?7, 0, NULL, ?5, 0, NULL, NULL, 1, 1,
                 NULL, ?8, ?9)",
        params![
            id,
            room_id,
            sender,
            hash,
            now,
            msg_type,
            serial,
            target_message_id,
            code
        ],
    )?;

    let message = read_message(&tx, id, true)?
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("emoticon event vanished")))?;
    tx.commit()?;
    Ok(message)
}

fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(input.as_bytes()))
}

// ----------------------------------------------------------------- reads ---

fn read_message(conn: &Connection, id: &str, include_deleted: bool) -> ApiResult<Option<Message>> {
    let sql = format!(
        "SELECT {MESSAGE_JOIN_COLUMNS} FROM messages m
         LEFT JOIN users u ON u.wallet_address = m.sender_address
         WHERE m.id = ?1 {}",
        if include_deleted {
            ""
        } else {
            "AND m.is_deleted = 0"
        }
    );
    let now = now_ms();
    let message = conn
        .query_row(&sql, params![id], |row| Message::from_joined_row(row, now))
        .optional()?;
    Ok(message)
}

/// Look up a live message. Deleted rows are excluded, which is what makes
/// "edit a deleted message" a 404 rather than a resurrection.
pub fn get_message(conn: &Connection, id: &str) -> ApiResult<Option<Message>> {
    read_message(conn, id, false)
}

/// Look up a message including soft-deleted ones. Used only where the caller
/// must distinguish "never existed" from "was deleted".
pub fn get_message_any(conn: &Connection, id: &str) -> ApiResult<Option<Message>> {
    read_message(conn, id, true)
}

/// The display/backfill query: newest-first inside SQL, returned ascending.
///
/// §15 #8: the reference applied `LIMIT` and *then* dropped event rows in
/// application code, so a page could come back nearly empty while plenty of
/// older messages existed. Filtering in SQL means a full page is a full page,
/// and clients can still paginate on the oldest `messageTimestamp` returned.
///
/// `since`/`before` are **timestamps** here, unlike `/sync`'s serial cursor. A
/// value of `0` means "no filter" rather than "since the epoch", which is what
/// makes an unparseable query parameter degrade instead of failing.
///
/// # The tiebreak
///
/// `message_timestamp` is milliseconds, and a burst of messages shares one.
/// The tiebreak used to be `m.id`, which reads as a stable secondary sort and
/// is not one: an id is `msg_{millis}_{uuid}`, so within a millisecond the
/// ordering was the ordering of a random UUID — different on every insert, and
/// wrong roughly half the time. `msg_serial` is the room's own monotonic
/// counter, allocated in the same transaction as the row (see [`next_serial`]),
/// which makes it the only column that is guaranteed to increase with
/// insertion order.
///
/// It is not perfectly immutable — an edit advances a message's serial so
/// `/sync` redelivers it — so an edited message can move within its own
/// millisecond. That is a far smaller wrong than randomness, and `FINDINGS.md`
/// proposed exactly this fix.
/// `include_replies` puts thread replies back into the channel view. Off by
/// default, and that default is the point of threads: a twenty-message thread
/// should cost the channel one line, not twenty. What the channel gets instead
/// is [`MESSAGE_THREAD_COLUMNS`] on the parent — a count and a timestamp — so
/// the line it does cost says how much was said under it.
pub fn get_messages(
    conn: &Connection,
    room_id: &str,
    viewer: &str,
    since: i64,
    before: i64,
    limit: i64,
    include_replies: bool,
) -> ApiResult<Vec<Message>> {
    let thread_filter = if include_replies {
        ""
    } else {
        "AND m.parent_message_id IS NULL"
    };
    let sql = format!(
        "SELECT {MESSAGE_JOIN_COLUMNS}, {MESSAGE_THREAD_COLUMNS} FROM messages m
         LEFT JOIN users u ON u.wallet_address = m.sender_address
         WHERE m.room_id = :room
           AND m.is_deleted = 0
           AND m.msg_type NOT IN ({EVENT_TYPES})
           {thread_filter}
           AND (:since = 0 OR m.message_timestamp >= :since)
           AND (:before = 0 OR m.message_timestamp < :before)
           AND {NOT_BLOCKED}
         ORDER BY m.message_timestamp DESC, m.msg_serial DESC
         LIMIT :limit"
    );

    let now = now_ms();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        rusqlite::named_params! {
            ":room": room_id,
            ":since": since,
            ":before": before,
            ":viewer": viewer,
            ":limit": limit,
        },
        |row| Message::from_threaded_row(row, now),
    )?;

    let mut out = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    out.reverse(); // chronologically ascending, as clients render it
    Ok(out)
}

/// The incremental state-transfer query behind `/sync`.
///
/// Nothing is filtered by type or `isDeleted` here — deletions, `delete_all`
/// markers and reaction events are all delivered, because that is precisely
/// what lets a client fold an incremental batch into correct state. The only
/// filter is the viewer's block list.
///
/// Returns the page plus whether more rows are waiting.
pub fn sync_messages(
    conn: &Connection,
    room_id: &str,
    viewer: &str,
    since: i64,
) -> ApiResult<(Vec<Message>, bool)> {
    let sql = format!(
        "SELECT {MESSAGE_JOIN_COLUMNS} FROM messages m
         LEFT JOIN users u ON u.wallet_address = m.sender_address
         WHERE m.room_id = :room AND m.msg_serial > :since AND {NOT_BLOCKED}
         ORDER BY m.msg_serial ASC
         LIMIT :limit"
    );

    let now = now_ms();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        rusqlite::named_params! {
            ":room": room_id,
            ":since": since,
            ":viewer": viewer,
            ":limit": SYNC_LIMIT + 1,
        },
        |row| Message::from_joined_row(row, now),
    )?;

    let mut out = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let has_more = out.len() as i64 > SYNC_LIMIT;
    out.truncate(SYNC_LIMIT as usize);
    Ok((out, has_more))
}

/// The highest serial in a room, or 0. Deliberately **not** block-filtered:
/// it is a change detector, not a read cursor, and a viewer's own cursor can
/// legitimately trail it forever when the newest events are all from blocked
/// senders.
pub fn latest_serial(conn: &Connection, room_id: &str) -> ApiResult<i64> {
    let serial: Option<i64> = conn.query_row(
        "SELECT MAX(msg_serial) FROM messages WHERE room_id = ?1",
        params![room_id],
        |r| r.get(0),
    )?;
    Ok(serial.unwrap_or(0))
}

pub fn latest_timestamp(conn: &Connection, room_id: &str) -> ApiResult<i64> {
    let ts: Option<i64> = conn.query_row(
        "SELECT MAX(message_timestamp) FROM messages WHERE room_id = ?1",
        params![room_id],
        |r| r.get(0),
    )?;
    Ok(ts.unwrap_or(0))
}

/// Unread messages for one viewer in one room.
///
/// Only `add` rows count: edits, deletes, purges and reactions must never
/// raise a badge, or every reaction to an old message would look like new mail.
/// Own messages never count either.
///
/// §15 #9: blocked senders are excluded here, unlike in the reference, so the
/// badge can no longer promise messages that `/sync` will never hand over.
pub fn unread_count(
    conn: &Connection,
    room_id: &str,
    viewer: &str,
    last_read_serial: i64,
) -> ApiResult<i64> {
    let sql = format!(
        "SELECT COUNT(*) FROM messages m
         WHERE m.room_id = :room
           AND m.msg_serial > :since
           AND m.msg_type = 'add'
           AND m.is_deleted = 0
           AND m.sender_address <> :viewer
           AND {NOT_BLOCKED}"
    );
    let n: i64 = conn.query_row(
        &sql,
        rusqlite::named_params! {
            ":room": room_id,
            ":since": last_read_serial,
            ":viewer": viewer,
        },
        |r| r.get(0),
    )?;
    Ok(n)
}

/// The newest displayable message, for a room-list preview.
///
/// §15 #17: `delete_all` markers are excluded. The reference let them through,
/// so a purged room previewed as an empty message from whoever purged it.
pub fn last_message(conn: &Connection, room_id: &str, viewer: &str) -> ApiResult<Option<Message>> {
    let sql = format!(
        "SELECT {MESSAGE_JOIN_COLUMNS} FROM messages m
         LEFT JOIN users u ON u.wallet_address = m.sender_address
         WHERE m.room_id = :room
           AND m.is_deleted = 0
           AND m.msg_type NOT IN ({EVENT_TYPES})
           AND {NOT_BLOCKED}
         ORDER BY m.message_timestamp DESC, m.msg_serial DESC
         LIMIT 1"
    );
    let now = now_ms();
    let message = conn
        .query_row(
            &sql,
            rusqlite::named_params! { ":room": room_id, ":viewer": viewer },
            |row| Message::from_joined_row(row, now),
        )
        .optional()?;
    Ok(message)
}

// -------------------------------------------------------------- mutation ---

/// Fields an edit may change. `iv`/`hmac` absent means the client is sending
/// plaintext; see [`update_message`] for why that is refused on an encrypted
/// message rather than silently honoured.
pub struct MessageEdit {
    pub content: String,
    pub msg_hash: String,
    pub iv: Option<String>,
    pub hmac: Option<String>,
    pub enc_ver: i64,
    pub key_version: i64,
}

/// Edit in place: the row keeps its id, `createdAt` and `messageTimestamp`,
/// and only the serial advances so `/sync` re-delivers the current state.
///
/// An edit that carries `iv` + `hmac` stays encrypted; one that carries
/// neither becomes plaintext. §15 #7 asks that an *encrypted* message never be
/// downgraded this way — that check lives in the route, which knows the
/// caller's intent; this function performs whichever transition it is told to.
/// `mentions` is the message's *new* mention set, replacing the old one in the
/// same transaction as the content it was derived from. Kept together on
/// purpose: an edit that commits and then fails to rewrite its mentions leaves
/// an inbox pointing at a message that no longer says what it did — the exact
/// drift the "message and its index appear together or not at all" rule in
/// [`create_message`] exists to prevent.
pub fn update_message(
    conn: &mut Connection,
    id: &str,
    room_id: &str,
    edit: MessageEdit,
    mentions: &[String],
) -> ApiResult<Option<Message>> {
    let tx = conn.transaction()?;
    let now = now_ms();
    let serial = next_serial(&tx, room_id)?;
    let encrypted = edit.iv.is_some() && edit.hmac.is_some();

    let changed = tx.execute(
        "UPDATE messages SET
             content = ?2, msg_hash = ?3, edited_at = ?4, msg_type = ?5, msg_serial = ?6,
             iv = ?7, hmac = ?8, is_encrypted = ?9, enc_ver = ?10, key_version = ?11
         WHERE id = ?1 AND is_deleted = 0",
        params![
            id,
            edit.content,
            edit.msg_hash,
            now,
            MSG_TYPE_EDIT,
            serial,
            if encrypted { edit.iv.clone() } else { None },
            if encrypted { edit.hmac.clone() } else { None },
            i64::from(encrypted),
            if encrypted { edit.enc_ver } else { 1 },
            if encrypted { edit.key_version } else { 1 },
        ],
    )?;
    if changed == 0 {
        return Ok(None);
    }

    let message = read_message(&tx, id, true)?;
    if let Some(m) = &message {
        super::mentions::replace(&tx, id, room_id, m.msg_serial, mentions)?;
        // An edit that turned encrypted also *unindexes* — the index must
        // never remember a plaintext the message no longer shows.
        crate::search::store::reindex_message(
            &tx,
            id,
            room_id,
            &m.sender_address,
            m.message_timestamp,
            &m.content,
            m.is_encrypted,
        )?;
    }
    tx.commit()?;
    Ok(message)
}

/// Soft-delete with a maximal scrub: content and hash are emptied, the crypto
/// fields are cleared, and the serial advances so every client learns of it.
/// The row survives only as a tombstone that `/sync` can deliver.
///
/// `parent_message_id` is deliberately **kept**. The tombstone has to stay in
/// its thread, or deleting one reply would move it into the channel as an
/// unexplained deleted message while its thread lost a row.
pub fn soft_delete_message(conn: &mut Connection, id: &str, room_id: &str) -> ApiResult<bool> {
    let tx = conn.transaction()?;
    let serial = next_serial(&tx, room_id)?;
    let changed = tx.execute(
        "UPDATE messages SET
             is_deleted = 1, content = '', msg_hash = '', msg_type = ?3,
             msg_serial = ?4, iv = NULL, hmac = NULL
         WHERE id = ?1 AND room_id = ?2 AND is_deleted = 0",
        params![id, room_id, MSG_TYPE_DELETE, serial],
    )?;
    if changed > 0 {
        crate::search::store::unindex(&tx, crate::search::store::KIND_MESSAGE, id)?;
        // Part of the scrub, not bookkeeping: a mention is a pointer into the
        // content, and the content is gone. Leaving the row would keep a
        // deleted message in somebody's inbox forever.
        super::mentions::forget(&tx, id)?;
    }
    tx.commit()?;
    Ok(changed > 0)
}

/// Purge a room's history and leave one marker behind.
///
/// The marker is what tells `/sync` clients to clear their local cache; without
/// it, a client that was offline during the purge would keep showing messages
/// the server no longer has. Because the counter lives in `room_serials` and
/// not in the deleted rows, the marker's serial is still greater than every
/// serial the clients already saw — which the reference could not guarantee
/// after a restart.
pub fn delete_all_messages(
    conn: &mut Connection,
    room_id: &str,
    caller: &str,
    marker_id: &str,
) -> ApiResult<(i64, Message)> {
    let tx = conn.transaction()?;

    let deleted: i64 = tx.query_row(
        "SELECT COUNT(*) FROM messages WHERE room_id = ?1",
        params![room_id],
        |r| r.get(0),
    )?;
    tx.execute("DELETE FROM messages WHERE room_id = ?1", params![room_id])?;
    crate::search::store::unindex_room_messages(&tx, room_id)?;

    let now = now_ms();
    let serial = next_serial(&tx, room_id)?;
    tx.execute(
        "INSERT INTO messages (id, room_id, sender_address, content, msg_hash,
                               message_timestamp, msg_type, msg_serial, is_deleted,
                               edited_at, created_at, is_encrypted, iv, hmac,
                               enc_ver, key_version, tx_hash, target_message_id, emoticon_code)
         VALUES (?1, ?2, ?3, '', '', ?4, ?5, ?6, 0, NULL, ?4, 0, NULL, NULL, 1, 1,
                 NULL, NULL, NULL)",
        params![marker_id, room_id, caller, now, MSG_TYPE_DELETE_ALL, serial],
    )?;

    let marker = read_message(&tx, marker_id, true)?
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("purge marker vanished")))?;
    tx.commit()?;
    Ok((deleted, marker))
}

/// Anchor a message to an on-chain transaction.
///
/// The serial bump makes `/sync` redeliver the row so every client picks up
/// the anchor; the message keeps its type, because publishing is not a new
/// event in the room's history, only new metadata on an old one.
pub fn publish_tx_hash(
    conn: &mut Connection,
    id: &str,
    room_id: &str,
    tx_hash: &str,
) -> ApiResult<Option<Message>> {
    let tx = conn.transaction()?;
    let serial = next_serial(&tx, room_id)?;
    let changed = tx.execute(
        "UPDATE messages SET tx_hash = ?2, msg_serial = ?3 WHERE id = ?1",
        params![id, tx_hash, serial],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    let message = read_message(&tx, id, true)?;
    tx.commit()?;
    Ok(message)
}

// ------------------------------------------------------------- reactions ---

/// Replay every reaction event for one message and report the surviving sets.
///
/// Aggregation is a fold rather than a count so that add/remove pairs cancel
/// exactly, and so a client that folds `/sync` itself reaches the same answer.
/// Codes whose set empties are dropped entirely.
///
/// §15 #10: the viewer's block list is applied here. The reference did not
/// filter, so a blocker saw blocked users listed as reactors even though the
/// same events were hidden from their `/sync` — two read surfaces disagreeing
/// about the same fact.
pub fn aggregate_emoticons(
    conn: &Connection,
    message_id: &str,
    viewer: &str,
) -> ApiResult<Vec<EmoticonAggregation>> {
    let sql = format!(
        "SELECT m.msg_type, m.sender_address, m.emoticon_code FROM messages m
         WHERE m.target_message_id = :target AND {NOT_BLOCKED}
         ORDER BY m.msg_serial ASC"
    );

    // Insertion-ordered: the response lists codes by first appearance, which
    // keeps the reaction row from reshuffling as counts change.
    let mut order: Vec<String> = Vec::new();
    let mut sets: std::collections::HashMap<String, HashSet<String>> =
        std::collections::HashMap::new();

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::named_params! {
        ":target": message_id,
        ":viewer": viewer,
    })?;

    while let Some(row) = rows.next()? {
        let msg_type: String = row.get(0)?;
        let sender: String = row.get(1)?;
        let Some(code) = row.get::<_, Option<String>>(2)? else {
            continue;
        };

        match msg_type.as_str() {
            MSG_TYPE_EMOTICON_ADD => {
                if !sets.contains_key(&code) {
                    order.push(code.clone());
                }
                sets.entry(code).or_default().insert(sender);
            }
            MSG_TYPE_EMOTICON_REMOVE => {
                if let Some(set) = sets.get_mut(&code) {
                    set.remove(&sender);
                }
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    for code in order {
        let Some(set) = sets.get(&code) else { continue };
        if set.is_empty() {
            continue;
        }
        // Sort for a deterministic response; the set itself is unordered.
        let mut addresses: Vec<&String> = set.iter().collect();
        addresses.sort();

        let mut users = Vec::with_capacity(addresses.len());
        for address in &addresses {
            let user = conn
                .query_row(
                    "SELECT wallet_address, username, public_key, public_key_sig,
                            profile_image, created_at, updated_at
                     FROM users WHERE wallet_address = ?1",
                    params![address],
                    User::from_row,
                )
                .optional()?;
            if let Some(user) = user {
                users.push(user);
            }
        }

        out.push(EmoticonAggregation {
            emoticon_code: code,
            // The reactor set, not `users.len()`: a reactor with no profile
            // row is still a reactor, and clients are told to trust `count`.
            count: set.len(),
            users,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::rooms::{add_member, create_room};
    use crate::db::test_db;
    use crate::db::users::{block_user, upsert_user};

    const ROOM: &str = "room_1749652739650_test";
    const ALICE: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BOB: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn seed(conn: &mut Connection) {
        upsert_user(conn, ALICE, "alice", None, None).unwrap();
        upsert_user(conn, BOB, "bob", None, None).unwrap();
        create_room(conn, ROOM, "Team", None, ALICE).unwrap();
        add_member(conn, ROOM, BOB).unwrap();
    }

    fn send(conn: &mut Connection, sender: &str, content: &str) -> Message {
        let id = format!("msg_{}_{}", now_ms(), uuid::Uuid::new_v4());
        create_message(
            conn,
            NewMessage {
                id,
                room_id: ROOM.into(),
                sender: sender.into(),
                content: content.into(),
                msg_hash: "a".repeat(64),
                is_encrypted: false,
                iv: None,
                hmac: None,
                enc_ver: 1,
                key_version: 1,
                parent_message_id: None,
                mentions: Vec::new(),
            },
        )
        .unwrap()
    }

    #[test]
    fn serials_are_strictly_increasing_within_a_room() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            let serials: Vec<i64> = (0..50)
                .map(|i| send(conn, ALICE, &format!("m{i}")).msg_serial)
                .collect();
            for pair in serials.windows(2) {
                assert!(
                    pair[1] > pair[0],
                    "serials must strictly increase: {pair:?}"
                );
            }
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn serials_survive_a_full_purge() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            let before = send(conn, ALICE, "hello").msg_serial;
            let (_, marker) = delete_all_messages(conn, ROOM, ALICE, "msg_marker_0001").unwrap();

            // The reference read the highest serial back from the (now empty)
            // table, so only its process-local map kept this true.
            assert!(
                marker.msg_serial > before,
                "the purge marker must outrank everything clients already saw"
            );
            assert!(send(conn, ALICE, "after").msg_serial > marker.msg_serial);
            Ok(())
        })
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writers_never_share_a_serial() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            Ok(())
        })
        .unwrap();

        let mut tasks = Vec::new();
        for i in 0..40 {
            let db = db.clone();
            tasks.push(tokio::spawn(async move {
                db.call(move |conn| {
                    create_message(
                        conn,
                        NewMessage {
                            id: format!("msg_conc_{i:04}_xxxxxxxx"),
                            room_id: ROOM.into(),
                            sender: ALICE.into(),
                            content: format!("m{i}"),
                            msg_hash: "b".repeat(64),
                            is_encrypted: false,
                            iv: None,
                            hmac: None,
                            enc_ver: 1,
                            key_version: 1,
                            parent_message_id: None,
                            mentions: Vec::new(),
                        },
                    )
                })
                .await
                .unwrap()
                .msg_serial
            }));
        }

        let mut serials = Vec::new();
        for task in tasks {
            serials.push(task.await.unwrap());
        }
        let unique: HashSet<i64> = serials.iter().copied().collect();
        assert_eq!(
            unique.len(),
            serials.len(),
            "a duplicate serial makes /sync skip a message forever"
        );
    }

    #[test]
    fn sync_delivers_events_that_messages_hides() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            let m = send(conn, ALICE, "hello");
            create_emoticon_event(conn, "emoticon_0001_aaaa", ROOM, BOB, &m.id, "🍎", true)
                .unwrap();
            soft_delete_message(conn, &m.id, ROOM).unwrap();

            let (synced, has_more) = sync_messages(conn, ROOM, ALICE, 0).unwrap();
            assert!(!has_more);
            let types: Vec<&str> = synced.iter().map(|m| m.msg_type.as_str()).collect();
            assert!(types.contains(&"emoticon_add"));
            assert!(types.contains(&"delete"));

            let listed = get_messages(conn, ROOM, ALICE, 0, 0, 50, false).unwrap();
            assert!(listed.is_empty(), "/messages hides deleted and event rows");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn sync_is_exclusive_of_its_cursor_and_ascending() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            let first = send(conn, ALICE, "one");
            let second = send(conn, ALICE, "two");

            let (page, _) = sync_messages(conn, ROOM, ALICE, first.msg_serial).unwrap();
            assert_eq!(page.len(), 1);
            assert_eq!(page[0].id, second.id);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn sync_reports_has_more_past_the_page_size() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            for i in 0..(SYNC_LIMIT + 5) {
                send(conn, ALICE, &format!("m{i}"));
            }
            let (page, has_more) = sync_messages(conn, ROOM, ALICE, 0).unwrap();
            assert_eq!(page.len(), SYNC_LIMIT as usize);
            assert!(has_more);

            let cursor = page.last().unwrap().msg_serial;
            let (rest, more) = sync_messages(conn, ROOM, ALICE, cursor).unwrap();
            assert_eq!(rest.len(), 5);
            assert!(!more);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn blocked_senders_disappear_from_every_viewer_read_path() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            block_user(conn, ALICE, BOB).unwrap();
            send(conn, BOB, "from bob");
            let mine = send(conn, ALICE, "from alice");

            let (synced, _) = sync_messages(conn, ROOM, ALICE, 0).unwrap();
            assert_eq!(synced.len(), 1, "alice must not see bob's events");
            assert_eq!(synced[0].id, mine.id);

            let listed = get_messages(conn, ROOM, ALICE, 0, 0, 50, false).unwrap();
            assert_eq!(listed.len(), 1);

            // §15 #9: the badge must agree with what /sync will hand over.
            assert_eq!(unread_count(conn, ROOM, ALICE, 0).unwrap(), 0);
            // Bob is not blocking anyone, so he still sees alice.
            assert_eq!(sync_messages(conn, ROOM, BOB, 0).unwrap().0.len(), 2);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn unread_counts_only_new_add_rows_from_other_people() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            let own = send(conn, ALICE, "mine");
            let theirs = send(conn, BOB, "theirs");
            create_emoticon_event(conn, "emoticon_0002_bbbb", ROOM, BOB, &own.id, "🍎", true)
                .unwrap();

            assert_eq!(unread_count(conn, ROOM, ALICE, 0).unwrap(), 1);
            assert_eq!(
                unread_count(conn, ROOM, ALICE, theirs.msg_serial).unwrap(),
                0,
                "reactions must never raise a badge"
            );
            assert_eq!(unread_count(conn, ROOM, BOB, 0).unwrap(), 1);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn editing_keeps_the_row_and_advances_only_the_serial() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            let original = send(conn, ALICE, "before");

            let edited = update_message(
                conn,
                &original.id,
                ROOM,
                MessageEdit {
                    content: "after".into(),
                    msg_hash: "c".repeat(64),
                    iv: None,
                    hmac: None,
                    enc_ver: 1,
                    key_version: 1,
                },
                &[],
            )
            .unwrap()
            .unwrap();

            assert_eq!(edited.id, original.id);
            assert_eq!(edited.content, "after");
            assert_eq!(edited.msg_type, "edit");
            assert_eq!(edited.message_timestamp, original.message_timestamp);
            assert_eq!(edited.created_at, original.created_at);
            assert!(edited.msg_serial > original.msg_serial);
            assert!(edited.edited_at.is_some());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn an_edit_carrying_iv_and_hmac_stays_encrypted() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            let original = send(conn, ALICE, "cipher");

            let edited = update_message(
                conn,
                &original.id,
                ROOM,
                MessageEdit {
                    content: "newcipher".into(),
                    msg_hash: "d".repeat(64),
                    iv: Some("f".repeat(32)),
                    hmac: Some("e".repeat(64)),
                    enc_ver: 2,
                    key_version: 3,
                },
                &[],
            )
            .unwrap()
            .unwrap();

            assert!(edited.is_encrypted);
            assert_eq!(edited.enc_ver, 2);
            assert_eq!(edited.key_version, 3);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn deleting_scrubs_the_row_but_keeps_the_tombstone() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            let m = send(conn, ALICE, "secret");
            assert!(soft_delete_message(conn, &m.id, ROOM).unwrap());

            assert!(get_message(conn, &m.id).unwrap().is_none());
            let tombstone = get_message_any(conn, &m.id).unwrap().unwrap();
            assert_eq!(tombstone.content, "");
            assert_eq!(tombstone.msg_hash, "");
            assert_eq!(tombstone.msg_type, "delete");
            assert!(tombstone.is_deleted);
            assert!(tombstone.msg_serial > m.msg_serial);

            assert!(
                !soft_delete_message(conn, &m.id, ROOM).unwrap(),
                "deleting twice reports no change"
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn reactions_fold_to_the_surviving_set() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            let target = send(conn, ALICE, "react to me");

            for (i, (sender, code, add)) in [
                (ALICE, "🍎", true),
                (BOB, "🍎", true),
                (BOB, "🍌", true),
                (BOB, "🍌", false),
                (ALICE, "🍎", true), // duplicate add is a no-op after folding
            ]
            .into_iter()
            .enumerate()
            {
                create_emoticon_event(
                    conn,
                    &format!("emoticon_{i:04}_cccc"),
                    ROOM,
                    sender,
                    &target.id,
                    code,
                    add,
                )
                .unwrap();
            }

            let agg = aggregate_emoticons(conn, &target.id, ALICE).unwrap();
            assert_eq!(agg.len(), 1, "an emptied code is dropped entirely");
            assert_eq!(agg[0].emoticon_code, "🍎");
            assert_eq!(agg[0].count, 2);
            assert_eq!(agg[0].users.len(), 2);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn reaction_aggregation_hides_blocked_reactors() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            let target = send(conn, ALICE, "react to me");
            create_emoticon_event(
                conn,
                "emoticon_0010_dddd",
                ROOM,
                BOB,
                &target.id,
                "🍎",
                true,
            )
            .unwrap();

            assert_eq!(aggregate_emoticons(conn, &target.id, BOB).unwrap().len(), 1);
            block_user(conn, ALICE, BOB).unwrap();
            // §15 #10: this used to disagree with the blocker's own /sync.
            assert!(aggregate_emoticons(conn, &target.id, ALICE)
                .unwrap()
                .is_empty());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn last_message_skips_purge_markers_and_reactions() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            let real = send(conn, ALICE, "the real last message");
            create_emoticon_event(conn, "emoticon_0020_eeee", ROOM, BOB, &real.id, "🍎", true)
                .unwrap();

            let preview = last_message(conn, ROOM, ALICE).unwrap().unwrap();
            assert_eq!(preview.id, real.id);

            let (_, _marker) = delete_all_messages(conn, ROOM, ALICE, "msg_marker_0002").unwrap();
            // §15 #17: the marker must not become the preview text.
            assert!(last_message(conn, ROOM, ALICE).unwrap().is_none());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn purging_reports_every_row_it_removed() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            let m = send(conn, ALICE, "one");
            send(conn, BOB, "two");
            create_emoticon_event(conn, "emoticon_0030_ffff", ROOM, BOB, &m.id, "🍎", true)
                .unwrap();

            let (count, marker) =
                delete_all_messages(conn, ROOM, ALICE, "msg_marker_0003").unwrap();
            assert_eq!(count, 3, "reactions count towards the purge total");
            assert_eq!(marker.msg_type, "delete_all");
            assert_eq!(marker.sender_address, ALICE);

            let (rows, _) = sync_messages(conn, ROOM, ALICE, 0).unwrap();
            assert_eq!(rows.len(), 1, "only the marker survives");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn publishing_bumps_the_serial_so_sync_redelivers_the_row() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            let m = send(conn, ALICE, "anchor me");
            let published = publish_tx_hash(conn, &m.id, ROOM, "0xdeadbeef")
                .unwrap()
                .unwrap();

            assert_eq!(published.tx_hash.as_deref(), Some("0xdeadbeef"));
            assert_eq!(published.msg_type, "add", "publishing is not a new event");
            assert!(published.msg_serial > m.msg_serial);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_sender_without_a_profile_gets_the_placeholder() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            let ghost = "0x1234560000000000000000000000000000007890";
            create_message(
                conn,
                NewMessage {
                    id: "msg_ghost_00000001".into(),
                    room_id: ROOM.into(),
                    sender: ghost.into(),
                    content: "boo".into(),
                    msg_hash: "f".repeat(64),
                    is_encrypted: false,
                    iv: None,
                    hmac: None,
                    enc_ver: 1,
                    key_version: 1,
                    parent_message_id: None,
                    mentions: Vec::new(),
                },
            )
            .unwrap();

            let (rows, _) = sync_messages(conn, ROOM, ALICE, 0).unwrap();
            let sender = rows[0].sender.as_ref().unwrap();
            assert_eq!(sender.username, "User 0x1234...7890");
            assert!(sender.public_key.is_none());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn timestamp_pagination_walks_backwards_without_overlap() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            for i in 0..5 {
                send(conn, ALICE, &format!("m{i}"));
                std::thread::sleep(std::time::Duration::from_millis(2));
            }

            let page = get_messages(conn, ROOM, ALICE, 0, 0, 3, false).unwrap();
            assert_eq!(page.len(), 3, "§15 #8: pages are full, not short");
            assert!(
                page[0].message_timestamp <= page[2].message_timestamp,
                "results come back ascending"
            );

            let oldest = page[0].message_timestamp;
            let older = get_messages(conn, ROOM, ALICE, 0, oldest, 3, false).unwrap();
            assert_eq!(older.len(), 2);
            assert!(older.iter().all(|m| m.message_timestamp < oldest));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn latest_serial_and_timestamp_default_to_zero_on_an_empty_room() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            assert_eq!(latest_serial(conn, ROOM).unwrap(), 0);
            assert_eq!(latest_timestamp(conn, ROOM).unwrap(), 0);

            let m = send(conn, ALICE, "x");
            assert_eq!(latest_serial(conn, ROOM).unwrap(), m.msg_serial);
            assert_eq!(latest_timestamp(conn, ROOM).unwrap(), m.message_timestamp);
            Ok(())
        })
        .unwrap();
    }
}
