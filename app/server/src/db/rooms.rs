//! Rooms, membership, admins, invitations, hidden rooms, and read state.

use rusqlite::{params, Connection, OptionalExtension};

use super::models::{iso_ms, HiddenRoom, InvitationView, Room, RoomMemberWithUser, User};
use super::now_ms;
use crate::error::ApiResult;

/// Hard ceiling on admins per room, inherited from the reference. One admin is
/// the floor, enforced by the demote and leave paths.
pub const MAX_ADMINS: i64 = 9;

// ----------------------------------------------------------------- rooms ---

/// Create a room and seat its creator as the first member and first admin.
///
/// Everything happens in one transaction: a room whose creator failed to
/// become an admin would be permanently unadministrable, and a half-created
/// room is not something any endpoint knows how to repair.
pub fn create_room(
    conn: &mut Connection,
    id: &str,
    name: &str,
    description: Option<&str>,
    creator: &str,
) -> ApiResult<Room> {
    let now = now_ms();
    let tx = conn.transaction()?;

    tx.execute(
        "INSERT INTO rooms (id, name, description, current_key_version,
                            key_rotation_pending, created_at)
         VALUES (?1, ?2, ?3, 1, 0, ?4)",
        params![id, name, description, now],
    )?;
    tx.execute(
        "INSERT INTO room_admins (room_id, wallet_address, created_at) VALUES (?1, ?2, ?3)",
        params![id, creator, now],
    )?;
    tx.execute(
        "INSERT INTO room_members (room_id, user_address, joined_at) VALUES (?1, ?2, ?3)",
        params![id, creator, now],
    )?;
    // Seed the serial counter so the first message does not have to create it.
    tx.execute(
        "INSERT INTO room_serials (room_id, next_serial) VALUES (?1, ?2)",
        params![id, now],
    )?;

    let room = tx.query_row(
        "SELECT id, name, description, current_key_version, key_rotation_pending, created_at
         FROM rooms WHERE id = ?1",
        params![id],
        Room::from_row,
    )?;
    tx.commit()?;
    Ok(room)
}

pub fn get_room(conn: &Connection, room_id: &str) -> ApiResult<Option<Room>> {
    let room = conn
        .query_row(
            "SELECT id, name, description, current_key_version, key_rotation_pending, created_at
             FROM rooms WHERE id = ?1",
            params![room_id],
            Room::from_row,
        )
        .optional()?;
    Ok(room)
}

pub fn update_room_name(conn: &Connection, room_id: &str, name: &str) -> ApiResult<Option<Room>> {
    let changed = conn.execute(
        "UPDATE rooms SET name = ?2 WHERE id = ?1",
        params![room_id, name],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    get_room(conn, room_id)
}

/// Delete a room and everything hanging off it, atomically.
///
/// §15 #11: the reference issued seven unsynchronised deletes and skipped
/// `room_invitations` entirely, so accepting an orphaned invitation later
/// returned a confusing 404. Here the schema's `ON DELETE CASCADE` clauses do
/// the work inside one transaction — nothing can be left behind and nothing
/// is visible half-deleted.
pub fn delete_room(conn: &mut Connection, room_id: &str) -> ApiResult<()> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM rooms WHERE id = ?1", params![room_id])?;
    // The cascade clears the room's tables; the search index has no FK to
    // rooms, so it forgets here, in the same transaction.
    crate::search::store::unindex_room_messages(&tx, room_id)?;
    tx.commit()?;
    Ok(())
}

/// Flag that someone left or was removed and the key has not been re-keyed.
///
/// While set, every encrypted send is refused with 409, because a departed
/// member still holds the current key and could read anything sent under it.
pub fn set_key_rotation_pending(conn: &Connection, room_id: &str, pending: bool) -> ApiResult<()> {
    conn.execute(
        "UPDATE rooms SET key_rotation_pending = ?2 WHERE id = ?1",
        params![room_id, i64::from(pending)],
    )?;
    Ok(())
}

// ------------------------------------------------------------ membership ---

pub fn is_member(conn: &Connection, room_id: &str, address: &str) -> ApiResult<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM room_members WHERE room_id = ?1 AND user_address = ?2",
        params![room_id, address],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Add a member idempotently (§15 #4 — the reference could insert duplicates).
pub fn add_member(conn: &Connection, room_id: &str, address: &str) -> ApiResult<()> {
    conn.execute(
        "INSERT INTO room_members (room_id, user_address, joined_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (room_id, user_address) DO NOTHING",
        params![room_id, address, now_ms()],
    )?;
    Ok(())
}

/// Remove a member, along with their read pointer and hidden-room entry.
///
/// Dropping the hidden-room row matters for access control: `GET
/// /api/rooms/hidden` would otherwise still surface the room's roster and
/// last-message preview to somebody who is no longer in it.
pub fn remove_member(conn: &mut Connection, room_id: &str, address: &str) -> ApiResult<()> {
    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM room_members WHERE room_id = ?1 AND user_address = ?2",
        params![room_id, address],
    )?;
    tx.execute(
        "DELETE FROM room_reads WHERE room_id = ?1 AND user_address = ?2",
        params![room_id, address],
    )?;
    tx.execute(
        "DELETE FROM hidden_rooms WHERE room_id = ?1 AND user_address = ?2",
        params![room_id, address],
    )?;
    tx.commit()?;
    Ok(())
}

/// The roster, in join order. Members whose profile row is missing are
/// dropped, which is why `memberCount` is the length of this list rather than
/// a `COUNT(*)` over the table.
pub fn list_members(conn: &Connection, room_id: &str) -> ApiResult<Vec<RoomMemberWithUser>> {
    let mut stmt = conn.prepare(
        "SELECT rm.id AS rm_id, rm.room_id, rm.user_address, rm.joined_at,
                u.wallet_address, u.username, u.public_key, u.public_key_sig,
                u.profile_image, u.created_at, u.updated_at
         FROM room_members rm
         JOIN users u ON u.wallet_address = rm.user_address
         WHERE rm.room_id = ?1
         ORDER BY rm.id",
    )?;
    let rows = stmt.query_map(params![room_id], |row| {
        Ok(RoomMemberWithUser {
            id: row.get("rm_id")?,
            room_id: row.get("room_id")?,
            user_address: row.get("user_address")?,
            joined_at: iso_ms(row.get("joined_at")?),
            user: User::from_row(row)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Room ids the user belongs to. This is the WebSocket/SSE subscription set —
/// clients cannot subscribe to anything else, and cannot unsubscribe.
pub fn user_room_ids(conn: &Connection, address: &str) -> ApiResult<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT room_id FROM room_members WHERE user_address = ?1 ORDER BY id")?;
    let rows = stmt.query_map(params![address], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Room ids the user belongs to and has not hidden — the `GET /api/rooms` set.
pub fn visible_room_ids(conn: &Connection, address: &str) -> ApiResult<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT rm.room_id FROM room_members rm
         WHERE rm.user_address = ?1
           AND rm.room_id NOT IN
               (SELECT room_id FROM hidden_rooms WHERE user_address = ?1)
         ORDER BY rm.id",
    )?;
    let rows = stmt.query_map(params![address], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---------------------------------------------------------------- admins ---

pub fn is_admin(conn: &Connection, room_id: &str, address: &str) -> ApiResult<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM room_admins WHERE room_id = ?1 AND wallet_address = ?2",
        params![room_id, address],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

pub fn admin_count(conn: &Connection, room_id: &str) -> ApiResult<i64> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM room_admins WHERE room_id = ?1",
        params![room_id],
        |r| r.get(0),
    )?;
    Ok(n)
}

pub fn add_admin(conn: &Connection, room_id: &str, address: &str) -> ApiResult<()> {
    conn.execute(
        "INSERT INTO room_admins (room_id, wallet_address, created_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (room_id, wallet_address) DO NOTHING",
        params![room_id, address, now_ms()],
    )?;
    Ok(())
}

pub fn remove_admin(conn: &Connection, room_id: &str, address: &str) -> ApiResult<()> {
    conn.execute(
        "DELETE FROM room_admins WHERE room_id = ?1 AND wallet_address = ?2",
        params![room_id, address],
    )?;
    Ok(())
}

/// Full profiles of a room's admins, in the order they were appointed.
pub fn list_admins(conn: &Connection, room_id: &str) -> ApiResult<Vec<User>> {
    let mut stmt = conn.prepare(
        "SELECT u.wallet_address, u.username, u.public_key, u.public_key_sig,
                u.profile_image, u.created_at, u.updated_at
         FROM room_admins ra
         JOIN users u ON u.wallet_address = ra.wallet_address
         WHERE ra.room_id = ?1
         ORDER BY ra.id",
    )?;
    let rows = stmt.query_map(params![room_id], User::from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ----------------------------------------------------------- invitations ---

/// Record a pending invitation. Re-inviting is a no-op rather than an error:
/// the caller's intent ("this person should be able to join") already holds.
pub fn create_invitation(
    conn: &Connection,
    room_id: &str,
    invitee: &str,
    inviter: &str,
) -> ApiResult<()> {
    conn.execute(
        "INSERT INTO room_invitations (room_id, invited_address, invited_by, created_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (room_id, invited_address) DO NOTHING",
        params![room_id, invitee, inviter, now_ms()],
    )?;
    Ok(())
}

pub fn has_pending_invitation(conn: &Connection, room_id: &str, invitee: &str) -> ApiResult<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM room_invitations WHERE room_id = ?1 AND invited_address = ?2",
        params![room_id, invitee],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

pub fn delete_invitation(conn: &Connection, room_id: &str, invitee: &str) -> ApiResult<()> {
    conn.execute(
        "DELETE FROM room_invitations WHERE room_id = ?1 AND invited_address = ?2",
        params![room_id, invitee],
    )?;
    Ok(())
}

/// Pending invitations for a user, newest first.
///
/// §15 #12: the reference dropped invitations whose room was gone by comparing
/// the room name against the literal `"(deleted room)"`, so a room actually
/// named that disappeared from everyone's inbox. The join here drops rows with
/// no room, which is the condition that was meant.
pub fn list_invitations(conn: &Connection, invitee: &str) -> ApiResult<Vec<InvitationView>> {
    let mut stmt = conn.prepare(
        "SELECT ri.room_id, r.name AS room_name, ri.invited_by,
                u.username AS inviter_username, ri.created_at
         FROM room_invitations ri
         JOIN rooms r ON r.id = ri.room_id
         LEFT JOIN users u ON u.wallet_address = ri.invited_by
         WHERE ri.invited_address = ?1
         ORDER BY ri.created_at DESC, ri.id DESC",
    )?;
    let rows = stmt.query_map(params![invitee], |row| {
        let invited_by: String = row.get("invited_by")?;
        let username: Option<String> = row.get("inviter_username")?;
        Ok(InvitationView {
            room_id: row.get("room_id")?,
            room_name: row.get("room_name")?,
            inviter_username: username.unwrap_or_else(|| invited_by.clone()),
            invited_by,
            created_at: iso_ms(row.get("created_at")?),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---------------------------------------------------------- hidden rooms ---

/// Hide a room from the caller's list. Idempotent (§15 #4).
pub fn hide_room(conn: &Connection, address: &str, room_id: &str) -> ApiResult<HiddenRoom> {
    conn.execute(
        "INSERT INTO hidden_rooms (user_address, room_id, created_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (user_address, room_id) DO NOTHING",
        params![address, room_id, now_ms()],
    )?;
    let row = conn.query_row(
        "SELECT id, user_address, room_id, created_at
         FROM hidden_rooms WHERE user_address = ?1 AND room_id = ?2",
        params![address, room_id],
        |row| {
            Ok(HiddenRoom {
                id: row.get("id")?,
                user_address: row.get("user_address")?,
                room_id: row.get("room_id")?,
                created_at: iso_ms(row.get("created_at")?),
            })
        },
    )?;
    Ok(row)
}

pub fn unhide_room(conn: &Connection, address: &str, room_id: &str) -> ApiResult<()> {
    conn.execute(
        "DELETE FROM hidden_rooms WHERE user_address = ?1 AND room_id = ?2",
        params![address, room_id],
    )?;
    Ok(())
}

/// Hidden rooms the caller is **still a member of**. The membership re-check
/// is the point: a former member who had hidden a room must not keep reading
/// its roster through this endpoint.
pub fn list_hidden(conn: &Connection, address: &str) -> ApiResult<Vec<HiddenRoom>> {
    let mut stmt = conn.prepare(
        "SELECT h.id, h.user_address, h.room_id, h.created_at
         FROM hidden_rooms h
         JOIN room_members rm
           ON rm.room_id = h.room_id AND rm.user_address = h.user_address
         WHERE h.user_address = ?1
         ORDER BY h.id",
    )?;
    let rows = stmt.query_map(params![address], |row| {
        Ok(HiddenRoom {
            id: row.get("id")?,
            user_address: row.get("user_address")?,
            room_id: row.get("room_id")?,
            created_at: iso_ms(row.get("created_at")?),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ------------------------------------------------------------ read state ---

pub fn last_read_serial(conn: &Connection, room_id: &str, address: &str) -> ApiResult<i64> {
    let serial = conn
        .query_row(
            "SELECT last_read_serial FROM room_reads WHERE room_id = ?1 AND user_address = ?2",
            params![room_id, address],
            |r| r.get::<_, i64>(0),
        )
        .optional()?;
    Ok(serial.unwrap_or(0))
}

/// Advance the read pointer, never backwards.
///
/// Two devices syncing the same room race constantly; letting the slower one
/// rewind the pointer would resurrect unread badges the user already cleared.
/// The stored value is returned so the caller echoes the truth rather than
/// what was asked for.
pub fn mark_read(conn: &Connection, room_id: &str, address: &str, serial: i64) -> ApiResult<i64> {
    let stored: i64 = conn.query_row(
        "INSERT INTO room_reads (room_id, user_address, last_read_serial, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (room_id, user_address) DO UPDATE SET
             last_read_serial = MAX(room_reads.last_read_serial, excluded.last_read_serial),
             updated_at = CASE
                 WHEN excluded.last_read_serial > room_reads.last_read_serial
                 THEN excluded.updated_at ELSE room_reads.updated_at END
         RETURNING last_read_serial",
        params![room_id, address, serial, now_ms()],
        |r| r.get(0),
    )?;
    Ok(stored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;
    use crate::db::users::upsert_user;

    const ROOM: &str = "room_1749652739650_test";
    const ALICE: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BOB: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn seed(conn: &mut Connection) {
        upsert_user(conn, ALICE, "alice", None, None).unwrap();
        upsert_user(conn, BOB, "bob", None, None).unwrap();
        create_room(conn, ROOM, "Team", None, ALICE).unwrap();
    }

    #[test]
    fn creating_a_room_seats_the_creator_as_member_and_admin() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            assert!(is_member(conn, ROOM, ALICE).unwrap());
            assert!(is_admin(conn, ROOM, ALICE).unwrap());
            assert_eq!(admin_count(conn, ROOM).unwrap(), 1);

            let room = get_room(conn, ROOM).unwrap().unwrap();
            assert_eq!(room.current_key_version, 1);
            assert!(!room.key_rotation_pending);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn membership_and_admin_inserts_are_idempotent() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            for _ in 0..3 {
                add_member(conn, ROOM, BOB).unwrap();
                add_admin(conn, ROOM, BOB).unwrap();
            }
            assert_eq!(list_members(conn, ROOM).unwrap().len(), 2);
            assert_eq!(admin_count(conn, ROOM).unwrap(), 2);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn members_without_a_profile_row_are_dropped_from_the_roster() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            conn.execute(
                "INSERT INTO room_members (room_id, user_address, joined_at) VALUES (?1, ?2, 1)",
                params![ROOM, "0xdeadbeef00000000000000000000000000000000"],
            )
            .unwrap();
            let members = list_members(conn, ROOM).unwrap();
            assert_eq!(
                members.len(),
                1,
                "orphan members are not part of the roster"
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn removing_a_member_clears_their_read_and_hidden_rows() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            add_member(conn, ROOM, BOB).unwrap();
            mark_read(conn, ROOM, BOB, 500).unwrap();
            hide_room(conn, BOB, ROOM).unwrap();

            remove_member(conn, ROOM, BOB).unwrap();

            assert!(!is_member(conn, ROOM, BOB).unwrap());
            assert_eq!(last_read_serial(conn, ROOM, BOB).unwrap(), 0);
            assert!(
                list_hidden(conn, BOB).unwrap().is_empty(),
                "a former member must not keep reading via /rooms/hidden"
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn hidden_rooms_are_excluded_from_the_visible_list_only() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            hide_room(conn, ALICE, ROOM).unwrap();

            assert!(visible_room_ids(conn, ALICE).unwrap().is_empty());
            assert_eq!(
                user_room_ids(conn, ALICE).unwrap(),
                vec![ROOM.to_string()],
                "hiding does not affect membership or delivery"
            );

            unhide_room(conn, ALICE, ROOM).unwrap();
            assert_eq!(visible_room_ids(conn, ALICE).unwrap().len(), 1);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn hiding_twice_produces_one_row() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            let a = hide_room(conn, ALICE, ROOM).unwrap();
            let b = hide_room(conn, ALICE, ROOM).unwrap();
            assert_eq!(a.id, b.id);
            assert_eq!(list_hidden(conn, ALICE).unwrap().len(), 1);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn the_read_pointer_only_moves_forward() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            assert_eq!(mark_read(conn, ROOM, ALICE, 100).unwrap(), 100);
            assert_eq!(
                mark_read(conn, ROOM, ALICE, 50).unwrap(),
                100,
                "a lagging device must not resurrect cleared badges"
            );
            assert_eq!(mark_read(conn, ROOM, ALICE, 150).unwrap(), 150);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn invitations_are_idempotent_and_newest_first() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            create_invitation(conn, ROOM, BOB, ALICE).unwrap();
            create_invitation(conn, ROOM, BOB, ALICE).unwrap();

            let list = list_invitations(conn, BOB).unwrap();
            assert_eq!(list.len(), 1);
            assert_eq!(list[0].inviter_username, "alice");
            assert!(has_pending_invitation(conn, ROOM, BOB).unwrap());

            delete_invitation(conn, ROOM, BOB).unwrap();
            assert!(!has_pending_invitation(conn, ROOM, BOB).unwrap());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn an_invitation_to_a_room_named_deleted_room_still_shows() {
        let db = test_db();
        db.call_blocking(|conn| {
            upsert_user(conn, ALICE, "alice", None, None).unwrap();
            upsert_user(conn, BOB, "bob", None, None).unwrap();
            create_room(conn, ROOM, "(deleted room)", None, ALICE).unwrap();
            create_invitation(conn, ROOM, BOB, ALICE).unwrap();

            // §15 #12: the reference filtered on this exact literal string.
            assert_eq!(list_invitations(conn, BOB).unwrap().len(), 1);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn deleting_a_room_removes_its_invitations_too() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            create_invitation(conn, ROOM, BOB, ALICE).unwrap();
            delete_room(conn, ROOM).unwrap();

            assert!(get_room(conn, ROOM).unwrap().is_none());
            assert!(list_invitations(conn, BOB).unwrap().is_empty());
            assert!(!has_pending_invitation(conn, ROOM, BOB).unwrap());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn key_rotation_pending_is_a_room_level_flag() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            set_key_rotation_pending(conn, ROOM, true).unwrap();
            assert!(get_room(conn, ROOM).unwrap().unwrap().key_rotation_pending);
            set_key_rotation_pending(conn, ROOM, false).unwrap();
            assert!(!get_room(conn, ROOM).unwrap().unwrap().key_rotation_pending);
            Ok(())
        })
        .unwrap();
    }
}
