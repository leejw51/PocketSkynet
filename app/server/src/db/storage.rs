//! Composite reads that stitch several tables into one wire object.
//!
//! The per-table queries live in the sibling modules; what is here is the
//! assembly the API's richer shapes need. Keeping it separate means the
//! narrow queries stay individually testable, and the expensive shapes are in
//! one place where their cost is visible.

use rusqlite::Connection;

use super::models::{HiddenRoomWithRoom, RoomWithMembers};
use super::{keys, messages, rooms};
use crate::error::ApiResult;

/// Build the enriched room view.
///
/// `viewer` is not decoration: the last-message preview is block-filtered, so
/// two members of the same room can legitimately see different previews.
///
/// `with_unread` is a parameter rather than always-on because `unreadCount`
/// and `lastReadSerial` appear only on `GET /api/rooms`. The detail endpoint
/// and the nested room inside `GET /api/rooms/hidden` omit them, and emitting
/// them anyway would be a wire change clients have no use for.
pub fn room_detail(
    conn: &Connection,
    room_id: &str,
    viewer: &str,
    with_unread: bool,
) -> ApiResult<Option<RoomWithMembers>> {
    let Some(room) = rooms::get_room(conn, room_id)? else {
        return Ok(None);
    };

    let members = rooms::list_members(conn, room_id)?;
    let admins = rooms::list_admins(conn, room_id)?;
    let last_message = messages::last_message(conn, room_id, viewer)?;
    let has_encryption = keys::has_encryption(conn, room_id)?;

    let (unread_count, last_read_serial) = if with_unread {
        let last_read = rooms::last_read_serial(conn, room_id, viewer)?;
        let unread = messages::unread_count(conn, room_id, viewer, last_read)?;
        (Some(unread), Some(last_read))
    } else {
        (None, None)
    };

    Ok(Some(RoomWithMembers {
        room,
        // The count follows the roster, so a member with no profile row is
        // absent from both rather than inflating a list nobody can render.
        member_count: members.len(),
        members,
        admins,
        last_message,
        has_encryption,
        unread_count,
        last_read_serial,
    }))
}

/// Every room the caller is in and has not hidden, enriched with read state.
///
/// Ordering is membership order, not activity: the reference had no `ORDER BY`
/// at all and clients already sort by `lastMessage.messageTimestamp`
/// themselves, so imposing an order here would only be a second opinion they
/// would immediately override.
pub fn visible_rooms(conn: &Connection, viewer: &str) -> ApiResult<Vec<RoomWithMembers>> {
    let ids = rooms::visible_room_ids(conn, viewer)?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(detail) = room_detail(conn, &id, viewer, true)? {
            out.push(detail);
        }
    }
    Ok(out)
}

/// Hidden rooms with their detail folded in.
///
/// Membership is re-checked by [`rooms::list_hidden`] itself, so a former
/// member who had hidden a room cannot keep reading its roster here.
pub fn hidden_rooms(conn: &Connection, viewer: &str) -> ApiResult<Vec<HiddenRoomWithRoom>> {
    let hidden = rooms::list_hidden(conn, viewer)?;
    let mut out = Vec::with_capacity(hidden.len());
    for row in hidden {
        if let Some(room) = room_detail(conn, &row.room_id, viewer, false)? {
            out.push(HiddenRoomWithRoom { hidden: row, room });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::messages::NewMessage;
    use crate::db::test_db;
    use crate::db::users::{block_user, upsert_user};

    const ROOM: &str = "room_1749652739650_test";
    const ALICE: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BOB: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn seed(conn: &mut Connection) {
        upsert_user(conn, ALICE, "alice", None, None).unwrap();
        upsert_user(conn, BOB, "bob", None, None).unwrap();
        rooms::create_room(conn, ROOM, "Team", Some("desc"), ALICE).unwrap();
        rooms::add_member(conn, ROOM, BOB).unwrap();
    }

    fn send(conn: &mut Connection, sender: &str, id: &str, content: &str) {
        messages::create_message(
            conn,
            NewMessage {
                id: id.into(),
                room_id: ROOM.into(),
                sender: sender.into(),
                content: content.into(),
                msg_hash: "a".repeat(64),
                is_encrypted: false,
                iv: None,
                hmac: None,
                enc_ver: 1,
                key_version: 1,
            },
        )
        .unwrap();
    }

    #[test]
    fn room_detail_carries_roster_admins_and_encryption_state() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            let detail = room_detail(conn, ROOM, ALICE, false).unwrap().unwrap();

            assert_eq!(detail.member_count, 2);
            assert_eq!(detail.members.len(), 2);
            assert_eq!(detail.admins.len(), 1);
            assert_eq!(detail.admins[0].username, "alice");
            assert!(!detail.has_encryption);
            assert!(detail.unread_count.is_none(), "detail omits read state");
            assert_eq!(detail.room.description.as_deref(), Some("desc"));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn read_state_appears_only_when_asked_for() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            send(conn, BOB, "msg_00000000001", "hi alice");

            let listed = visible_rooms(conn, ALICE).unwrap();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].unread_count, Some(1));
            assert_eq!(listed[0].last_read_serial, Some(0));

            let detail = room_detail(conn, ROOM, ALICE, false).unwrap().unwrap();
            assert!(detail.unread_count.is_none() && detail.last_read_serial.is_none());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn the_preview_is_block_filtered_per_viewer() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            send(conn, ALICE, "msg_00000000001", "from alice");
            send(conn, BOB, "msg_00000000002", "from bob");
            block_user(conn, ALICE, BOB).unwrap();

            let alice_view = room_detail(conn, ROOM, ALICE, false).unwrap().unwrap();
            let bob_view = room_detail(conn, ROOM, BOB, false).unwrap().unwrap();

            assert_eq!(
                alice_view.last_message.unwrap().content,
                "from alice",
                "a blocked sender must not supply the preview"
            );
            assert_eq!(bob_view.last_message.unwrap().content, "from bob");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn hidden_rooms_carry_the_detail_but_not_the_read_state() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            rooms::hide_room(conn, ALICE, ROOM).unwrap();

            assert!(visible_rooms(conn, ALICE).unwrap().is_empty());
            let hidden = hidden_rooms(conn, ALICE).unwrap();
            assert_eq!(hidden.len(), 1);
            assert_eq!(hidden[0].room.member_count, 2);
            assert!(hidden[0].room.unread_count.is_none());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_former_member_loses_the_hidden_room_view() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            rooms::hide_room(conn, BOB, ROOM).unwrap();
            assert_eq!(hidden_rooms(conn, BOB).unwrap().len(), 1);

            rooms::remove_member(conn, ROOM, BOB).unwrap();
            assert!(
                hidden_rooms(conn, BOB).unwrap().is_empty(),
                "leaving must not leave a readable back door"
            );
            Ok(())
        })
        .unwrap();
    }
}
