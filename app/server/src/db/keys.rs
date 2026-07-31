//! Wrapped room keys and epoch rotation.
//!
//! One row per `(room, user, epoch)`. A member accumulates one wrap per epoch
//! they were present for, which is exactly what lets them still decrypt
//! history after a rotation — and what makes deleting *all* of a departing
//! member's wraps the right move on leave/kick.

use rusqlite::{params, Connection, OptionalExtension};

use super::models::RoomKey;
use super::now_ms;
use crate::error::ApiResult;

/// One wrapped key for one recipient.
#[derive(Debug, Clone)]
pub struct KeyWrap {
    pub user_address: String,
    pub encrypted_symmetric_key: String,
    pub ephemeral_public_key: String,
    pub encryption_iv: String,
    pub hmac: String,
    pub enc_ver: i64,
}

const SELECT_COLUMNS: &str = "id, room_id, user_address, encrypted_symmetric_key, \
    ephemeral_public_key, encryption_iv, hmac, enc_ver, key_version, created_at";

/// Whether *anyone* holds a wrap for this room, at any epoch. This is the
/// `hasEncryption` flag: it says the room has been keyed, not that the caller
/// can read it.
pub fn has_encryption(conn: &Connection, room_id: &str) -> ApiResult<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM room_keys WHERE room_id = ?1",
        params![room_id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Store one wrap, replacing whatever occupied that exact epoch.
///
/// Only the targeted epoch is touched: other epochs survive so the recipient
/// keeps access to the history they were already able to read.
pub fn store_key(
    conn: &Connection,
    room_id: &str,
    wrap: &KeyWrap,
    key_version: i64,
) -> ApiResult<()> {
    conn.execute(
        "INSERT INTO room_keys (room_id, user_address, encrypted_symmetric_key,
                                ephemeral_public_key, encryption_iv, hmac, enc_ver,
                                key_version, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (room_id, user_address, key_version) DO UPDATE SET
             encrypted_symmetric_key = excluded.encrypted_symmetric_key,
             ephemeral_public_key    = excluded.ephemeral_public_key,
             encryption_iv           = excluded.encryption_iv,
             hmac                    = excluded.hmac,
             enc_ver                 = excluded.enc_ver,
             created_at              = excluded.created_at",
        params![
            room_id,
            wrap.user_address,
            wrap.encrypted_symmetric_key,
            wrap.ephemeral_public_key,
            wrap.encryption_iv,
            wrap.hmac,
            wrap.enc_ver,
            key_version,
            now_ms(),
        ],
    )?;
    Ok(())
}

pub fn key_exists(
    conn: &Connection,
    room_id: &str,
    user: &str,
    key_version: i64,
) -> ApiResult<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM room_keys
         WHERE room_id = ?1 AND user_address = ?2 AND key_version = ?3",
        params![room_id, user, key_version],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// The caller's newest wrap. Enough to send, never enough to read history —
/// clients decrypting old messages must use [`all_keys`].
pub fn latest_key(conn: &Connection, room_id: &str, user: &str) -> ApiResult<Option<RoomKey>> {
    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM room_keys
         WHERE room_id = ?1 AND user_address = ?2
         ORDER BY key_version DESC, id DESC LIMIT 1"
    );
    let key = conn
        .query_row(&sql, params![room_id, user], RoomKey::from_row)
        .optional()?;
    Ok(key)
}

/// Every wrap the caller holds, oldest epoch first. An empty result is an
/// empty array, not a 404: "this room is not encrypted for me" is a normal
/// state, not an error.
pub fn all_keys(conn: &Connection, room_id: &str, user: &str) -> ApiResult<Vec<RoomKey>> {
    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM room_keys
         WHERE room_id = ?1 AND user_address = ?2
         ORDER BY key_version ASC, id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![room_id, user], RoomKey::from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Drop every wrap a user holds for a room, across all epochs.
///
/// Called on leave, kick and decline. Combined with `keyRotationPending`, this
/// is what makes a departure actually cost the departing member access: they
/// keep whatever they cached, but nothing sent afterwards is readable to them.
pub fn delete_user_keys(conn: &Connection, room_id: &str, user: &str) -> ApiResult<()> {
    conn.execute(
        "DELETE FROM room_keys WHERE room_id = ?1 AND user_address = ?2",
        params![room_id, user],
    )?;
    Ok(())
}

/// What a rotation attempt did.
#[derive(Debug, PartialEq, Eq)]
pub enum RotateOutcome {
    Rotated,
    RoomNotFound,
    /// Another member rotated first. The loser refetches the room and retries
    /// only if `keyRotationPending` is still set.
    StaleVersion {
        current: i64,
    },
}

/// Advance the room to `new_version`, installing one wrap per member.
///
/// Everything happens in one transaction, including the epoch check: two
/// members racing to re-key after a departure must not both succeed, or half
/// the room would be sealed under an epoch the other half never received.
///
/// The caller is responsible for the coverage checks (every current member
/// present, no strays) — they need the roster and the request body, and doing
/// them here would mean passing both down for no gain.
pub fn rotate(
    conn: &mut Connection,
    room_id: &str,
    new_version: i64,
    wraps: &[KeyWrap],
) -> ApiResult<RotateOutcome> {
    let tx = conn.transaction()?;

    let current: Option<i64> = tx
        .query_row(
            "SELECT current_key_version FROM rooms WHERE id = ?1",
            params![room_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(current) = current else {
        return Ok(RotateOutcome::RoomNotFound);
    };

    // Exactly one step forward. Allowing a jump would let a client skip an
    // epoch, and members holding only the skipped one would silently lose the
    // room without any signal that a rotation happened.
    if new_version != current + 1 {
        return Ok(RotateOutcome::StaleVersion { current });
    }

    for wrap in wraps {
        store_key(&tx, room_id, wrap, new_version)?;
    }

    tx.execute(
        "UPDATE rooms SET current_key_version = ?2, key_rotation_pending = 0 WHERE id = ?1",
        params![room_id, new_version],
    )?;
    tx.commit()?;
    Ok(RotateOutcome::Rotated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::rooms::{add_member, create_room, get_room, set_key_rotation_pending};
    use crate::db::test_db;
    use crate::db::users::upsert_user;

    const ROOM: &str = "room_1749652739650_test";
    const ALICE: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BOB: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn wrap(user: &str, tag: &str) -> KeyWrap {
        KeyWrap {
            user_address: user.into(),
            encrypted_symmetric_key: format!("wrapped-{tag}"),
            ephemeral_public_key: "04ab".into(),
            encryption_iv: "1a2b3c4d5e6f78901234567890abcdef".into(),
            hmac: "9".repeat(64),
            enc_ver: 2,
        }
    }

    fn seed(conn: &mut Connection) {
        upsert_user(conn, ALICE, "alice", None, None).unwrap();
        upsert_user(conn, BOB, "bob", None, None).unwrap();
        create_room(conn, ROOM, "Team", None, ALICE).unwrap();
        add_member(conn, ROOM, BOB).unwrap();
    }

    #[test]
    fn has_encryption_flips_on_the_first_wrap_from_anyone() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            assert!(!has_encryption(conn, ROOM).unwrap());
            store_key(conn, ROOM, &wrap(ALICE, "v1"), 1).unwrap();
            assert!(has_encryption(conn, ROOM).unwrap());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn storing_replaces_only_the_targeted_epoch() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            store_key(conn, ROOM, &wrap(ALICE, "v1"), 1).unwrap();
            store_key(conn, ROOM, &wrap(ALICE, "v2"), 2).unwrap();
            store_key(conn, ROOM, &wrap(ALICE, "v1-again"), 1).unwrap();

            let keys = all_keys(conn, ROOM, ALICE).unwrap();
            assert_eq!(keys.len(), 2, "history access must survive a re-store");
            assert_eq!(keys[0].key_version, 1);
            assert_eq!(keys[0].encrypted_symmetric_key, "wrapped-v1-again");
            assert_eq!(keys[1].key_version, 2);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn latest_key_is_the_highest_epoch() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            store_key(conn, ROOM, &wrap(ALICE, "v1"), 1).unwrap();
            store_key(conn, ROOM, &wrap(ALICE, "v3"), 3).unwrap();
            store_key(conn, ROOM, &wrap(ALICE, "v2"), 2).unwrap();

            assert_eq!(
                latest_key(conn, ROOM, ALICE).unwrap().unwrap().key_version,
                3
            );
            assert!(latest_key(conn, ROOM, BOB).unwrap().is_none());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn deleting_a_users_keys_spans_every_epoch() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            for v in 1..=3 {
                store_key(conn, ROOM, &wrap(BOB, &format!("v{v}")), v).unwrap();
            }
            delete_user_keys(conn, ROOM, BOB).unwrap();

            assert!(all_keys(conn, ROOM, BOB).unwrap().is_empty());
            assert!(!key_exists(conn, ROOM, BOB, 2).unwrap());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn rotation_advances_exactly_one_epoch_and_clears_the_flag() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            store_key(conn, ROOM, &wrap(ALICE, "v1"), 1).unwrap();
            set_key_rotation_pending(conn, ROOM, true).unwrap();

            let outcome = rotate(conn, ROOM, 2, &[wrap(ALICE, "v2"), wrap(BOB, "v2")]).unwrap();
            assert_eq!(outcome, RotateOutcome::Rotated);

            let room = get_room(conn, ROOM).unwrap().unwrap();
            assert_eq!(room.current_key_version, 2);
            assert!(!room.key_rotation_pending);
            assert_eq!(all_keys(conn, ROOM, ALICE).unwrap().len(), 2);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_second_racing_rotation_loses_with_the_current_epoch() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            rotate(conn, ROOM, 2, &[wrap(ALICE, "v2"), wrap(BOB, "v2")]).unwrap();

            // The loser submitted the same target version.
            let outcome = rotate(conn, ROOM, 2, &[wrap(ALICE, "x"), wrap(BOB, "x")]).unwrap();
            assert_eq!(outcome, RotateOutcome::StaleVersion { current: 2 });

            // Skipping ahead is refused too: nobody would hold epoch 3.
            let skipped = rotate(conn, ROOM, 4, &[wrap(ALICE, "y"), wrap(BOB, "y")]).unwrap();
            assert_eq!(skipped, RotateOutcome::StaleVersion { current: 2 });
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn rotating_a_missing_room_is_reported_not_thrown() {
        let db = test_db();
        db.call_blocking(|conn| {
            let outcome = rotate(conn, "room_does_not_exist", 2, &[]).unwrap();
            assert_eq!(outcome, RotateOutcome::RoomNotFound);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_failed_rotation_installs_nothing() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            let outcome = rotate(conn, ROOM, 5, &[wrap(ALICE, "v5")]).unwrap();
            assert!(matches!(outcome, RotateOutcome::StaleVersion { .. }));
            assert!(
                all_keys(conn, ROOM, ALICE).unwrap().is_empty(),
                "a rejected rotation must not leave orphan wraps behind"
            );
            Ok(())
        })
        .unwrap();
    }
}
