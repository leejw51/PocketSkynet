//! Who a message names, and the inbox that follows from it.
//!
//! # Why mentions are stored rather than searched for
//!
//! "Show me everything that mentions me" could in principle be a `LIKE '%@bob%'`
//! over every message in every room the caller belongs to. It is a row here
//! instead for three reasons, and only the third is about speed:
//!
//! * **A username is not stable.** Someone who renames themselves would lose
//!   every mention that used their old name, and inherit every mention of
//!   whoever held the new one. Resolving `@name` to a wallet address *once*, at
//!   the moment the message is written, is what makes a mention refer to a
//!   person rather than to a string.
//! * **An encrypted room has no text to search.** The server cannot read those
//!   messages and must never be able to. A room that is E2EE would silently
//!   have no mentions at all — see [`declared`] for what happens instead.
//! * A correlated `LIKE` across a whole history is the kind of query that is
//!   fine on the machine it was written on and is not fine two years later.
//!
//! # What is *not* stored
//!
//! Nothing about whether a mention has been read. The inbox derives that by
//! comparing each mention's `msg_serial` against the caller's
//! `room_reads.last_read_serial` for that room, so opening a room clears its
//! mentions with no second write and the two can never disagree.

use rusqlite::{params, Connection};

use super::models::{Message, MESSAGE_JOIN_COLUMNS};
use super::now_ms;
use crate::error::ApiResult;

/// How many people one message may name.
///
/// A ceiling rather than an unbounded list because the mention list is the one
/// part of a message whose cost is paid by *other* people's inboxes: a single
/// post naming four hundred wallets is a notification storm with one author.
/// Nothing legitimate needs more than this — a room-wide announcement is what
/// the room itself is for.
pub const MAX_MENTIONS: usize = 32;

/// Write the mention rows for a message.
///
/// Called inside the message's own transaction, and idempotent per
/// (message, address) so an edit that re-derives the same list is a no-op
/// rather than a duplicate.
pub fn record(
    conn: &Connection,
    message_id: &str,
    room_id: &str,
    msg_serial: i64,
    addresses: &[String],
) -> ApiResult<()> {
    if addresses.is_empty() {
        return Ok(());
    }
    let now = now_ms();
    let mut stmt = conn.prepare(
        "INSERT INTO message_mentions
             (message_id, room_id, mentioned_address, msg_serial, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (message_id, mentioned_address)
             DO UPDATE SET msg_serial = excluded.msg_serial",
    )?;
    for address in addresses.iter().take(MAX_MENTIONS) {
        stmt.execute(params![message_id, room_id, address, msg_serial, now])?;
    }
    Ok(())
}

/// Replace a message's mentions with a new set — the edit path.
///
/// An edit that removes someone's name must remove their mention, or the
/// inbox would keep pointing at a message that no longer says what it did.
pub fn replace(
    conn: &Connection,
    message_id: &str,
    room_id: &str,
    msg_serial: i64,
    addresses: &[String],
) -> ApiResult<()> {
    conn.execute(
        "DELETE FROM message_mentions WHERE message_id = ?1",
        params![message_id],
    )?;
    record(conn, message_id, room_id, msg_serial, addresses)
}

/// Forget a message's mentions. Used by the delete path, where the row itself
/// survives as a tombstone but must stop appearing in anybody's inbox.
pub fn forget(conn: &Connection, message_id: &str) -> ApiResult<()> {
    conn.execute(
        "DELETE FROM message_mentions WHERE message_id = ?1",
        params![message_id],
    )?;
    Ok(())
}

/// One entry in the mentions inbox.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MentionView {
    #[serde(rename = "roomId")]
    pub room_id: String,
    #[serde(rename = "roomName")]
    pub room_name: String,
    #[serde(rename = "roomKind")]
    pub room_kind: String,
    /// `false` once the caller's read pointer in that room has passed it.
    #[serde(rename = "isUnread")]
    pub is_unread: bool,
    pub message: Message,
}

/// The caller's mentions, newest first.
///
/// Four filters, each load-bearing:
///
/// * the room must still be one the caller is in — leaving a room takes its
///   mentions with it, rather than leaving an inbox entry that 403s when
///   opened;
/// * the message must not be deleted;
/// * the sender must not be blocked, so the inbox cannot be used to reach
///   somebody who has stopped listening;
/// * self-mentions are dropped, because naming yourself in your own message is
///   not mail.
pub fn inbox(conn: &Connection, viewer: &str, limit: i64) -> ApiResult<Vec<MentionView>> {
    let sql = format!(
        "SELECT {MESSAGE_JOIN_COLUMNS},
                r.name AS room_name, r.kind AS room_kind,
                COALESCE(rr.last_read_serial, 0) AS last_read_serial
         FROM message_mentions mm
         JOIN messages m ON m.id = mm.message_id
         JOIN rooms r ON r.id = mm.room_id
         JOIN room_members rm ON rm.room_id = mm.room_id AND rm.user_address = :viewer
         LEFT JOIN room_reads rr ON rr.room_id = mm.room_id AND rr.user_address = :viewer
         LEFT JOIN users u ON u.wallet_address = m.sender_address
         WHERE mm.mentioned_address = :viewer
           AND m.is_deleted = 0
           AND m.sender_address <> :viewer
           AND m.sender_address NOT IN
               (SELECT blocked_address FROM blocked_users WHERE blocker_address = :viewer)
         ORDER BY mm.msg_serial DESC
         LIMIT :limit"
    );

    let now = now_ms();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        rusqlite::named_params! { ":viewer": viewer, ":limit": limit },
        |row| {
            let last_read: i64 = row.get("last_read_serial")?;
            let message = Message::from_joined_row(row, now)?;
            Ok(MentionView {
                room_id: message.room_id.clone(),
                room_name: row.get("room_name")?,
                room_kind: row.get("room_kind")?,
                is_unread: message.msg_serial > last_read,
                message,
            })
        },
    )?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// How many unread mentions the caller has, per room.
///
/// Separate from [`inbox`] because the badge is wanted on every room-list
/// render and the inbox is opened rarely; making the cheap question pay for
/// the expensive one would put a join over every mention ever written on the
/// path of the app's most frequent request.
pub fn unread_counts(conn: &Connection, viewer: &str) -> ApiResult<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT mm.room_id, COUNT(*) AS n
         FROM message_mentions mm
         JOIN messages m ON m.id = mm.message_id
         JOIN room_members rm ON rm.room_id = mm.room_id AND rm.user_address = :viewer
         LEFT JOIN room_reads rr ON rr.room_id = mm.room_id AND rr.user_address = :viewer
         WHERE mm.mentioned_address = :viewer
           AND m.is_deleted = 0
           AND m.sender_address <> :viewer
           AND mm.msg_serial > COALESCE(rr.last_read_serial, 0)
           AND m.sender_address NOT IN
               (SELECT blocked_address FROM blocked_users WHERE blocker_address = :viewer)
         GROUP BY mm.room_id",
    )?;
    let rows = stmt.query_map(rusqlite::named_params! { ":viewer": viewer }, |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ------------------------------------------------------------ extraction ---

/// Pull `@handle` tokens out of plaintext.
///
/// Deliberately permissive about what a handle *looks* like and strict about
/// what it *resolves* to: anything word-shaped after an `@` is a candidate, and
/// [`resolve`] then keeps only the candidates that name a member of the room.
/// The alternative — encoding the username grammar here — would mean this
/// function and `validate::username` could disagree, and the failure mode of
/// that disagreement is a mention that highlights for the reader and never
/// reaches the person named.
///
/// An `@` must start a token (preceded by whitespace or nothing), so an email
/// address does not mention its own domain.
///
/// This is a *fallback*, not the mechanism. Usernames here may contain spaces
/// and emoji (`validate::username` allows both), which no `@token` grammar can
/// capture — so the client sends the addresses it meant, and this exists to
/// catch the mention somebody typed by hand into a client that did not.
pub fn extract(content: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let bytes: Vec<char> = content.chars().collect();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != '@' {
            i += 1;
            continue;
        }
        // Only at a token boundary — `bob@example.com` names nobody.
        let at_boundary = i == 0 || bytes[i - 1].is_whitespace() || is_opening(bytes[i - 1]);
        if !at_boundary {
            i += 1;
            continue;
        }

        let start = i + 1;
        let mut end = start;
        while end < bytes.len() && is_handle_char(bytes[end]) {
            end += 1;
        }
        if end > start {
            let handle: String = bytes[start..end].iter().collect();
            if !out.iter().any(|h| h.eq_ignore_ascii_case(&handle)) {
                out.push(handle);
            }
        }
        i = end.max(i + 1);
    }
    out
}

/// Characters that may appear inside a handle.
///
/// Sentence punctuation is excluded rather than stripped afterwards, so the
/// scan simply stops at it: "thanks @bob." ends the handle at the full stop
/// because a full stop was never part of it. A `0x…` address is all
/// alphanumeric, so it is covered by the same rule as a name.
fn is_handle_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-')
}

/// Punctuation an `@` may legitimately follow, so `(@bob)` still mentions bob.
fn is_opening(c: char) -> bool {
    matches!(c, '(' | '[' | '{' | '<' | '"' | '\'' | '“' | '‘')
}

/// Turn handles into the wallet addresses of room members.
///
/// Only members resolve. Mentioning somebody who is not in the room would put
/// a message they cannot open into their inbox, which is worse than the
/// mention simply not happening — and it would leak the existence of a room to
/// somebody who was never in it.
///
/// Matching is case-insensitive on both the username and the address, and an
/// ambiguous handle — two members who differ only in case — resolves to
/// neither, because guessing which of two colleagues was meant is worse than
/// not highlighting the word.
pub fn resolve(conn: &Connection, room_id: &str, handles: &[String]) -> ApiResult<Vec<String>> {
    if handles.is_empty() {
        return Ok(Vec::new());
    }

    let mut stmt = conn.prepare(
        "SELECT u.wallet_address, u.username
         FROM room_members rm
         JOIN users u ON u.wallet_address = rm.user_address
         WHERE rm.room_id = ?1",
    )?;
    let roster: Vec<(String, String)> = stmt
        .query_map(params![room_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut out: Vec<String> = Vec::new();
    for handle in handles.iter().take(MAX_MENTIONS) {
        let matches: Vec<&String> = roster
            .iter()
            .filter(|(address, username)| {
                username.eq_ignore_ascii_case(handle) || address.eq_ignore_ascii_case(handle)
            })
            .map(|(address, _)| address)
            .collect();
        if let [only] = matches[..] {
            if !out.contains(only) {
                out.push(only.clone());
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::rooms;
    use crate::db::test_db;
    use crate::db::users::upsert_user;

    const ROOM: &str = "room_1749652739650_ment";
    const ALICE: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BOB: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn handles_are_pulled_from_token_boundaries_only() {
        assert_eq!(extract("hey @bob can you look"), vec!["bob"]);
        assert_eq!(extract("@bob"), vec!["bob"]);
        // Trailing sentence punctuation is not part of the name.
        assert_eq!(extract("thanks @bob."), vec!["bob"]);
        assert_eq!(extract("(@bob) and @carol!"), vec!["bob", "carol"]);
        // An email is not a mention of its domain.
        assert!(extract("write to bob@example.com").is_empty());
        // A bare @ names nobody.
        assert!(extract("cost @ 5 dollars").is_empty());
        // Repeats collapse — one mention per person per message.
        assert_eq!(extract("@bob @Bob @bob"), vec!["bob"]);
        // Addresses are handles too, and survive whole.
        assert_eq!(extract(&format!("ping @{BOB} please")), vec![BOB]);
    }

    #[test]
    fn only_room_members_resolve() {
        let db = test_db();
        db.call_blocking(|conn| {
            upsert_user(conn, ALICE, "alice", None, None).unwrap();
            upsert_user(conn, BOB, "bob", None, None).unwrap();
            const CAROL: &str = "0xcccccccccccccccccccccccccccccccccccccccc";
            upsert_user(conn, CAROL, "carol", None, None).unwrap();
            rooms::create_room(conn, ROOM, "Team", None, ALICE).unwrap();
            rooms::add_member(conn, ROOM, BOB).unwrap();

            // Bob is in the room; Carol is not, and mentioning her must not
            // put a room she cannot open into her inbox.
            let handles = extract("@bob and @carol and @nobody");
            let resolved = resolve(conn, ROOM, &handles).unwrap();
            assert_eq!(resolved, vec![BOB.to_owned()]);

            // An address resolves the same way a name does, and case does not
            // matter on either side.
            let resolved = resolve(conn, ROOM, &["BOB".into()]).unwrap();
            assert_eq!(resolved, vec![BOB.to_owned()]);
            let resolved = resolve(conn, ROOM, &[BOB.to_uppercase()]).unwrap();
            assert_eq!(resolved, vec![BOB.to_owned()]);
            Ok(())
        })
        .unwrap();
    }
}
