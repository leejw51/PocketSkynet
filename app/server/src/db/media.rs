//! Which hosted images and videos a room is showing.
//!
//! `data/images/` is content-addressed and rowless: the AI assistant stores a
//! generated picture under the SHA-256 of its bytes and the room carries a
//! `/api/images/{sha256}.{ext}` URL. That is what makes the link survive the
//! provider's CDN, and it is also why nothing on the server knows the file
//! belongs to a conversation — the URL is the only reference there is.
//!
//! Destroying a room has to know. "Delete the room and everything it showed"
//! cannot be answered by reading the messages: it can for a plaintext room,
//! and for an encrypted one the server holds ciphertext and must keep holding
//! only that. So a reference is recorded the way a mention is (`db/mentions.rs`
//! makes the argument at length): extracted from plaintext when there is
//! plaintext, declared by the client when there is not. A declaration names a
//! file and nothing else — no caption, no context — so it says only "this room
//! shows these bytes", which the URL itself already said to anyone holding it.
//!
//! The other half is [`is_referenced`]: the same bytes may be in a second room,
//! or be somebody's avatar, and a purge must not reach through the room it was
//! asked about into one it was not.

use rusqlite::{params, Connection};

use super::now_ms;
use crate::error::ApiResult;

/// How many files one message may name.
///
/// A message renders a handful of images at most; a list longer than this is
/// not a post, it is someone using the declaration as a delete list for a room
/// purge to act on later.
pub const MAX_MEDIA: usize = 32;

/// Is this a name `routes/images.rs` could have written and would serve?
///
/// The stem must be exactly a SHA-256 and the extension must be one the media
/// allow-list knows — the same two checks the serving route makes, for the same
/// reason. It is what keeps a declared name from being a path: no separators,
/// no dots, nothing that could escape `data/images/`.
pub fn is_media_name(name: &str) -> bool {
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    stem.len() == 64
        && stem.bytes().all(|b| b.is_ascii_hexdigit())
        && crate::routes::images::mime_for(ext).is_some()
}

/// The prefix every hosted-media URL carries, absolute or same-origin.
const URL_PREFIX: &str = "/api/images/";

/// The media names a plaintext message points at.
///
/// Deliberately a substring scan rather than a URL parser: the text is chat,
/// not a document, and the link may arrive bare, in markdown, inside an
/// `<img src>` a client wrote, or with an origin in front of it. What every
/// one of those forms has in common is `/api/images/` followed by the name,
/// and the name's own grammar says where it ends.
pub fn extract(content: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = content;
    while let Some(at) = rest.find(URL_PREFIX) {
        let after = &rest[at + URL_PREFIX.len()..];
        let end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.'))
            .unwrap_or(after.len());
        let candidate = &after[..end];
        if is_media_name(candidate) && !out.iter().any(|n| n == candidate) {
            out.push(candidate.to_owned());
        }
        rest = &after[end..];
    }
    out
}

/// Write a message's media references, inside the message's own transaction.
///
/// Idempotent per (message, name), so an edit that re-derives the same list is
/// a no-op rather than a duplicate.
pub fn record(
    conn: &Connection,
    message_id: &str,
    room_id: &str,
    names: &[String],
) -> ApiResult<()> {
    if names.is_empty() {
        return Ok(());
    }
    let now = now_ms();
    let mut stmt = conn.prepare(
        "INSERT INTO message_media (message_id, room_id, image_name, created_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (message_id, image_name) DO NOTHING",
    )?;
    for name in names.iter().take(MAX_MEDIA) {
        stmt.execute(params![message_id, room_id, name, now])?;
    }
    Ok(())
}

/// Replace a message's media references — the edit path.
///
/// An edit that removes a picture must remove the reference, or destroying the
/// room later would keep bytes alive on the strength of a message that has not
/// shown them since.
pub fn replace(
    conn: &Connection,
    message_id: &str,
    room_id: &str,
    names: &[String],
) -> ApiResult<()> {
    forget(conn, message_id)?;
    record(conn, message_id, room_id, names)
}

/// Forget a message's media. Used by the delete path, where the row survives as
/// a tombstone but is showing nothing any more.
pub fn forget(conn: &Connection, message_id: &str) -> ApiResult<()> {
    conn.execute(
        "DELETE FROM message_media WHERE message_id = ?1",
        params![message_id],
    )?;
    Ok(())
}

/// Every media file one room is showing, from both sources at once: the
/// recorded references, and a scan of whatever plaintext the room still holds.
///
/// The scan is not redundant. `message_media` starts empty on a database that
/// predates it, and every plaintext message written before this table existed
/// would otherwise leave its pictures behind on a purge — which is exactly the
/// history a right-to-forget request is about.
pub fn names_for_room(conn: &Connection, room_id: &str) -> ApiResult<Vec<String>> {
    let mut out: Vec<String> = Vec::new();

    let mut stmt =
        conn.prepare("SELECT DISTINCT image_name FROM message_media WHERE room_id = ?1")?;
    for name in stmt.query_map(params![room_id], |r| r.get::<_, String>(0))? {
        let name = name?;
        if is_media_name(&name) && !out.contains(&name) {
            out.push(name);
        }
    }

    let mut stmt =
        conn.prepare("SELECT content FROM messages WHERE room_id = ?1 AND is_encrypted = 0")?;
    for content in stmt.query_map(params![room_id], |r| r.get::<_, String>(0))? {
        for name in extract(&content?) {
            if !out.contains(&name) {
                out.push(name);
            }
        }
    }

    Ok(out)
}

/// Does anything that survived still point at these bytes?
///
/// Four places can, and a purge that skipped any of them would break somebody
/// else's room, avatar or note: a recorded reference, a plaintext message, a
/// profile picture, a taught note.
///
/// What it cannot see is an *encrypted* message written before `message_media`
/// existed. That is a deliberate ranking, not an oversight: a room being
/// destroyed is a request to forget, and the alternative — keep the bytes on
/// the chance that ciphertext somewhere names them — answers it with "no".
/// The cost is a broken thumbnail in an old encrypted room; the bytes are gone
/// either way once the room that owned them asked.
pub fn is_referenced(conn: &Connection, name: &str) -> ApiResult<bool> {
    let referenced: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM message_media WHERE image_name = ?1)
             OR EXISTS (SELECT 1 FROM messages
                         WHERE is_encrypted = 0 AND content LIKE '%' || ?1 || '%')
             OR EXISTS (SELECT 1 FROM users WHERE profile_image LIKE '%' || ?1 || '%')
             OR EXISTS (SELECT 1 FROM knowledge_notes WHERE content LIKE '%' || ?1 || '%')",
        params![name],
        |r| r.get(0),
    )?;
    Ok(referenced)
}

/// Which of these files nothing surviving still points at — the purge's
/// unlink list, and the plural form of [`is_referenced`].
///
/// Mirrors `files::orphan_candidates` deliberately: same question, same shape,
/// one for each of the two directories a room can be holding bytes in.
pub fn unreferenced(conn: &Connection, names: &[String]) -> ApiResult<Vec<String>> {
    let mut out = Vec::new();
    for name in names {
        if !is_referenced(conn, name)? {
            out.push(name.clone());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(byte: u8) -> String {
        std::iter::repeat_n(format!("{byte:02x}"), 32).collect()
    }

    fn world() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("schema.sql")).unwrap();
        conn.execute(
            "INSERT INTO rooms (id, name, current_key_version, created_at)
             VALUES ('r1', 'Room', 1, 1), ('r2', 'Other', 1, 1)",
            [],
        )
        .unwrap();
        conn
    }

    fn message(conn: &Connection, id: &str, room: &str, content: &str, encrypted: bool) {
        conn.execute(
            "INSERT INTO messages (id, room_id, sender_address, content, msg_hash,
                                   message_timestamp, created_at, is_encrypted)
             VALUES (?1, ?2, 'alice', ?3, 'h', 1, 1, ?4)",
            params![id, room, content, i64::from(encrypted)],
        )
        .unwrap();
    }

    #[test]
    fn a_name_must_be_a_digest_and_a_servable_extension() {
        assert!(is_media_name(&format!("{}.png", digest(0xab))));
        assert!(is_media_name(&format!("{}.mp4", digest(0x01))));
        // Anything that is not exactly a hash, or not a type the server would
        // serve, cannot name a file — which is what makes a declared name safe
        // to join onto `data/images/`.
        assert!(!is_media_name(&format!("{}.exe", digest(0x01))));
        assert!(!is_media_name(&format!("{}.png", "a".repeat(63))));
        assert!(!is_media_name("../../jwt.secret.png"));
        assert!(!is_media_name(&format!("dir/{}.png", digest(0x01))));
        assert!(!is_media_name(""));
    }

    #[test]
    fn extraction_finds_a_link_in_every_shape_chat_produces() {
        let png = format!("{}.png", digest(0x11));
        let mp4 = format!("{}.mp4", digest(0x22));
        let content = format!(
            "look ![it](/api/images/{png}) and \
             <video src=\"http://host:9099/api/images/{mp4}\"></video>, \
             plus /api/images/{png} again"
        );
        // Deduped: the same file twice in one message is one reference.
        assert_eq!(extract(&content), vec![png, mp4]);
    }

    #[test]
    fn extraction_ignores_a_prefix_that_names_nothing_servable() {
        let bad = format!("/api/images/{}.exe", digest(0x33));
        assert!(extract(&bad).is_empty());
        assert!(extract("/api/images/").is_empty());
        assert!(extract("no links here").is_empty());
    }

    #[test]
    fn a_rooms_media_is_its_records_and_its_plaintext_together() {
        let conn = world();
        let recorded = format!("{}.png", digest(0x44));
        let typed = format!("{}.webp", digest(0x55));
        let elsewhere = format!("{}.gif", digest(0x66));

        message(&conn, "m1", "r1", "ciphertext", true);
        record(&conn, "m1", "r1", std::slice::from_ref(&recorded)).unwrap();
        message(
            &conn,
            "m2",
            "r1",
            &format!("see /api/images/{typed}"),
            false,
        );
        message(
            &conn,
            "m3",
            "r2",
            &format!("see /api/images/{elsewhere}"),
            false,
        );

        let mut names = names_for_room(&conn, "r1").unwrap();
        names.sort();
        let mut want = vec![recorded, typed];
        want.sort();
        assert_eq!(names, want, "the other room's picture is not this room's");
    }

    #[test]
    fn a_file_a_second_room_still_shows_is_referenced() {
        let conn = world();
        let shared = format!("{}.png", digest(0x77));
        message(&conn, "m1", "r2", &format!("/api/images/{shared}"), false);
        assert!(is_referenced(&conn, &shared).unwrap());

        conn.execute("DELETE FROM messages WHERE id = 'm1'", [])
            .unwrap();
        assert!(!is_referenced(&conn, &shared).unwrap());
    }

    #[test]
    fn an_avatar_and_a_note_count_as_references() {
        let conn = world();
        let avatar = format!("{}.png", digest(0x88));
        let taught = format!("{}.jpg", digest(0x99));
        conn.execute(
            "INSERT INTO users (wallet_address, username, profile_image, created_at, updated_at)
             VALUES ('alice', 'alice', ?1, 1, 1)",
            params![format!("/api/images/{avatar}")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO knowledge_notes (id, owner_address, content, created_at, updated_at)
             VALUES ('n1', 'alice', ?1, 1, 1)",
            params![format!("chart: /api/images/{taught}")],
        )
        .unwrap();

        assert!(is_referenced(&conn, &avatar).unwrap());
        assert!(is_referenced(&conn, &taught).unwrap());
    }

    #[test]
    fn an_edit_drops_what_the_message_stopped_showing_and_a_delete_drops_it_all() {
        let conn = world();
        let before = format!("{}.png", digest(0xaa));
        let after = format!("{}.png", digest(0xbb));
        message(&conn, "m1", "r1", "ciphertext", true);

        record(&conn, "m1", "r1", std::slice::from_ref(&before)).unwrap();
        replace(&conn, "m1", "r1", std::slice::from_ref(&after)).unwrap();
        assert_eq!(names_for_room(&conn, "r1").unwrap(), vec![after]);

        forget(&conn, "m1").unwrap();
        assert!(names_for_room(&conn, "r1").unwrap().is_empty());
    }

    #[test]
    fn deleting_a_room_takes_its_references_with_it() {
        let conn = world();
        let name = format!("{}.png", digest(0xcc));
        message(&conn, "m1", "r1", "ciphertext", true);
        record(&conn, "m1", "r1", std::slice::from_ref(&name)).unwrap();

        conn.execute("PRAGMA foreign_keys = ON", []).unwrap();
        conn.execute("DELETE FROM rooms WHERE id = 'r1'", [])
            .unwrap();
        assert!(!is_referenced(&conn, &name).unwrap(), "ON DELETE CASCADE");
    }
}
