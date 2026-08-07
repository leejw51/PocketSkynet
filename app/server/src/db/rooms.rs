//! Rooms, membership, admins, invitations, hidden rooms, and read state.

use rusqlite::{params, Connection, OptionalExtension};

use super::models::{
    iso_ms, HiddenRoom, InvitationView, Room, RoomMemberWithUser, User, ROOM_KIND_CHANNEL,
    ROOM_KIND_DM, ROOM_KIND_GROUP_DM, ROOM_KIND_JARVIS, ROOM_KIND_LOBBY, ROOM_KIND_NOTE,
    STATIC_ROOM_KINDS,
};
use super::now_ms;
use crate::error::ApiResult;

/// Hard ceiling on admins per room, inherited from the reference. One admin is
/// the floor, enforced by the demote and leave paths.
pub const MAX_ADMINS: i64 = 9;

/// Every column [`Room::from_row`] reads, in one place so a query and the
/// reader cannot drift — the `kind` column was added to both at once and the
/// next one will be too.
pub const ROOM_COLUMNS: &str =
    "id, name, description, current_key_version, key_rotation_pending, kind, created_at";

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
                            key_rotation_pending, kind, dm_key, created_at)
         VALUES (?1, ?2, ?3, 1, 0, ?4, NULL, ?5)",
        params![id, name, description, ROOM_KIND_CHANNEL, now],
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
        &format!("SELECT {ROOM_COLUMNS} FROM rooms WHERE id = ?1"),
        params![id],
        Room::from_row,
    )?;
    tx.commit()?;
    Ok(room)
}

pub fn get_room(conn: &Connection, room_id: &str) -> ApiResult<Option<Room>> {
    let room = conn
        .query_row(
            &format!("SELECT {ROOM_COLUMNS} FROM rooms WHERE id = ?1"),
            params![room_id],
            Room::from_row,
        )
        .optional()?;
    Ok(room)
}

// -------------------------------------------------------- built-in rooms ---

/// The name a built-in room is stored under.
///
/// English, and stored rather than derived, because `rooms.name` is `NOT NULL`
/// and every surface that has never heard of these kinds — the admin console's
/// room list, the storage report, an old client — has to print *something*. The
/// clients that do know them translate by kind (`web/src/rooms.rs`), exactly as
/// they already title a DM after its members rather than after the placeholder
/// the column holds. So this string is the fallback, not the label.
pub fn static_room_name(kind: &str) -> &'static str {
    match kind {
        ROOM_KIND_NOTE => "My Note",
        ROOM_KIND_JARVIS => "My Jarvis",
        ROOM_KIND_LOBBY => "My Lobby",
        _ => "Room",
    }
}

/// The id of one person's built-in room: `room_<kind>_<owner>`.
///
/// Derived rather than allocated, and that is what makes provisioning safe to
/// run on every room-list fetch. An id from `uuid::Uuid::new_v4()` would need a
/// lookup table — "which room is Alice's note?" — and every check that a room
/// is *yours* would become a join. Here the question is a string comparison
/// against a value the caller already holds, so the ownership test cannot be
/// forgotten in the way a lookup can be, and two concurrent provisioning runs
/// collide on the primary key instead of creating two notes.
///
/// The owner is lowercased because [`WalletAddress`] already guarantees it and
/// a caller passing a checksummed string would otherwise mint a second, parallel
/// room for the same person. 52–54 characters of `[a-z0-9_]`, comfortably inside
/// the 10–100 the protocol allows.
pub fn static_room_id(kind: &str, owner: &str) -> String {
    format!("room_{kind}_{}", owner.to_lowercase())
}

/// Ensure `owner` has all three built-in rooms and that their rosters say what
/// the kind promises. Idempotent, and safe to run on every request that needs
/// them to exist.
///
/// # Why the roster is reconciled and not just created
///
/// Two of the three have a membership that is a *function of something else*
/// rather than a record of who joined. "My Lobby" is the owner plus whoever
/// `VITE_FRUITNATION_ADMIN` currently names, and that list is a line in a
/// config file the operator edits and restarts — there is no request that
/// changes it and therefore no place to hang an incremental update. So the set
/// is recomputed here, which also means an operator who adds themselves as an
/// admin appears in everybody's lobby without anybody re-inviting them, and one
/// who is removed leaves the same way.
///
/// "My Jarvis" is the same shape with a set of one: the owner's agent address,
/// which is derived and so can be reconstructed rather than remembered.
///
/// "My Note" is the strict case and the reason this function owns the roster at
/// all: its member set is exactly `[owner]`, enforced here on every pass. The
/// route layer refuses every verb that could add a second member, but a rule
/// with one enforcement point is a rule that a future route can bypass by
/// accident; recomputing the set means the room *heals* rather than merely
/// resisting.
pub fn provision_static_rooms(
    conn: &mut Connection,
    owner: &str,
    server_admins: &[String],
) -> ApiResult<()> {
    let agent = pocketskynet_core::WalletAddress::agent_of(
        &pocketskynet_core::WalletAddress::new(owner)
            .map_err(|e| crate::error::ApiError::Internal(anyhow::anyhow!(e)))?,
    );

    for kind in STATIC_ROOM_KINDS {
        let id = static_room_id(kind, owner);
        // The roster this kind promises, owner first.
        let mut roster = vec![owner.to_lowercase()];
        match kind {
            ROOM_KIND_JARVIS => roster.push(agent.as_str().to_owned()),
            ROOM_KIND_LOBBY => roster.extend(server_admins.iter().cloned()),
            _ => {}
        }
        roster = dedup_sorted(&roster);

        let now = now_ms();
        let tx = conn.transaction()?;
        // `DO NOTHING` rather than a read-then-write: two of the caller's tabs
        // can fetch the room list in the same millisecond, and the loser of
        // that race must find the winner's room, not fail the whole listing.
        tx.execute(
            "INSERT INTO rooms (id, name, description, current_key_version,
                                key_rotation_pending, kind, dm_key, created_at)
             VALUES (?1, ?2, NULL, 1, 0, ?3, NULL, ?4)
             ON CONFLICT (id) DO NOTHING",
            params![id, static_room_name(kind), kind, now],
        )?;
        tx.execute(
            "INSERT INTO room_serials (room_id, next_serial) VALUES (?1, ?2)
             ON CONFLICT (room_id) DO NOTHING",
            params![id, now],
        )?;
        // The owner administers all three. Not much of a power — every verb
        // admin gates is refused for these rooms — but the roster and admin
        // views both render "who runs this", and "nobody" would be a lie about
        // a room that is entirely one person's.
        tx.execute(
            "INSERT INTO room_admins (room_id, wallet_address, created_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (room_id, wallet_address) DO NOTHING",
            params![id, owner.to_lowercase(), now],
        )?;

        for member in &roster {
            tx.execute(
                "INSERT INTO room_members (room_id, user_address, joined_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (room_id, user_address) DO NOTHING",
                params![id, member, now],
            )?;
        }
        // Anyone the roster no longer names goes, along with their read pointer
        // and hidden-room row — the same three deletes `remove_member` does,
        // spelled out here because they have to happen inside this transaction.
        let placeholders = roster
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(", ");
        let mut binds: Vec<&dyn rusqlite::ToSql> = vec![&id];
        for member in &roster {
            binds.push(member);
        }
        for table in ["room_members", "room_reads"] {
            tx.execute(
                &format!(
                    "DELETE FROM {table} WHERE room_id = ?1 AND user_address NOT IN ({placeholders})"
                ),
                binds.as_slice(),
            )?;
        }
        tx.execute(
            &format!(
                "DELETE FROM hidden_rooms
                 WHERE room_id = ?1 AND user_address NOT IN ({placeholders})"
            ),
            binds.as_slice(),
        )?;
        tx.commit()?;
    }

    // The agent needs a profile row or `list_members` drops it and every
    // message it ever posts renders under the synthesised "User 0x0000…"
    // placeholder. Written last, outside the loop, because it belongs to the
    // person rather than to any one room.
    super::users::upsert_user(conn, agent.as_str(), "Jarvis", None, None)?;
    Ok(())
}

// ------------------------------------------------------- direct messages ---

/// The identity of a DM: its member set, as lowercased addresses sorted and
/// joined with `|`.
///
/// Sorting is the whole mechanism. "Alice messages Bob" and "Bob messages
/// Alice" have to name the same conversation, and the only way to guarantee
/// that without a lookup is to make the name independent of who asked — so the
/// set is canonicalised before it is ever written or compared. Deduplicating
/// first means naming yourself twice cannot manufacture a distinct key for the
/// same set of people.
///
/// Addresses are already checksummed [`WalletAddress`] strings by the time
/// they reach here; lowercasing anyway costs nothing and makes the key immune
/// to a caller that ever hands over an unnormalised address.
pub fn dm_key(members: &[String]) -> String {
    let mut keys: Vec<String> = members.iter().map(|m| m.to_lowercase()).collect();
    keys.sort();
    keys.dedup();
    keys.join("|")
}

/// Find the DM for exactly this member set, if it has been opened before.
pub fn find_dm(conn: &Connection, key: &str) -> ApiResult<Option<Room>> {
    let room = conn
        .query_row(
            &format!("SELECT {ROOM_COLUMNS} FROM rooms WHERE dm_key = ?1"),
            params![key],
            Room::from_row,
        )
        .optional()?;
    Ok(room)
}

/// Open a DM between `members`, or return the one that already exists.
///
/// Every member is seated as both a member *and* an admin. That looks
/// over-generous next to a channel, where admin is a role somebody grants, but
/// a DM has no roster to manage and no hierarchy to express: the only verbs
/// admin gates here are deleting the conversation and purging its history, and
/// both of those are things either participant is entitled to do to a
/// conversation that is half theirs. The alternative — a DM whose creator is
/// its admin — would mean the person who said hello first owns the record.
///
/// Idempotence is enforced by the unique index on `dm_key`, not by the check
/// at the top: two requests racing to open the same DM both find nothing, and
/// the loser's INSERT is what refuses. It re-reads instead of failing, so the
/// caller sees one room either way.
pub fn create_dm(conn: &mut Connection, id: &str, members: &[String]) -> ApiResult<Room> {
    let key = dm_key(members);
    if let Some(existing) = find_dm(conn, &key)? {
        return Ok(existing);
    }

    let kind = if key.split('|').count() <= 2 {
        ROOM_KIND_DM
    } else {
        ROOM_KIND_GROUP_DM
    };
    // A DM's name is never shown — clients title it with the other members —
    // but the column is NOT NULL and a raw fallback is better than an empty
    // string in whatever surface forgets to derive one.
    let name = if kind == ROOM_KIND_DM {
        "Direct message"
    } else {
        "Group message"
    };

    let now = now_ms();
    let tx = conn.transaction()?;

    let inserted = tx.execute(
        "INSERT INTO rooms (id, name, description, current_key_version,
                            key_rotation_pending, kind, dm_key, created_at)
         VALUES (?1, ?2, NULL, 1, 0, ?3, ?4, ?5)
         -- The WHERE clause is not decoration: `idx_rooms_dm_key` is a partial
         -- index, and SQLite only matches a conflict target to one when the
         -- target repeats its predicate. Without it this is a parse error, not
         -- a silently ineffective clause.
         ON CONFLICT (dm_key) WHERE dm_key IS NOT NULL DO NOTHING",
        params![id, name, kind, key, now],
    )?;
    if inserted == 0 {
        // Lost the race. The winner's room is the answer.
        tx.rollback()?;
        return find_dm(conn, &key)?.ok_or_else(|| {
            crate::error::ApiError::Internal(anyhow::anyhow!("direct message vanished after race"))
        });
    }

    for member in dedup_sorted(members) {
        tx.execute(
            "INSERT INTO room_members (room_id, user_address, joined_at) VALUES (?1, ?2, ?3)",
            params![id, member, now],
        )?;
        tx.execute(
            "INSERT INTO room_admins (room_id, wallet_address, created_at) VALUES (?1, ?2, ?3)",
            params![id, member, now],
        )?;
    }
    tx.execute(
        "INSERT INTO room_serials (room_id, next_serial) VALUES (?1, ?2)",
        params![id, now],
    )?;

    let room = tx.query_row(
        &format!("SELECT {ROOM_COLUMNS} FROM rooms WHERE id = ?1"),
        params![id],
        Room::from_row,
    )?;
    tx.commit()?;
    Ok(room)
}

/// The member list in canonical order, deduplicated — the *checksummed*
/// addresses, unlike [`dm_key`], because these are what gets stored and shown.
fn dedup_sorted(members: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = members
        .iter()
        .filter(|m| seen.insert(m.to_lowercase()))
        .cloned()
        .collect();
    out.sort();
    out
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
            assert_eq!(room.kind, ROOM_KIND_CHANNEL);
            assert!(!room.is_direct());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_dm_key_names_the_member_set_and_not_the_caller() {
        // The property the whole DM design rests on: whoever asks first, and
        // in whatever order they name the participants, the key is the same.
        assert_eq!(
            dm_key(&[ALICE.into(), BOB.into()]),
            dm_key(&[BOB.into(), ALICE.into()])
        );
        // Case cannot fork it either — a checksummed address and a lowercased
        // one are the same person.
        assert_eq!(
            dm_key(&[ALICE.to_uppercase(), BOB.into()]),
            dm_key(&[ALICE.into(), BOB.into()])
        );
        // Naming yourself twice is the same one-person set as naming yourself
        // once, so it cannot be used to open a second private room.
        assert_eq!(
            dm_key(&[ALICE.into(), ALICE.into()]),
            dm_key(&[ALICE.into()])
        );
        // Different sets stay different.
        assert_ne!(dm_key(&[ALICE.into()]), dm_key(&[ALICE.into(), BOB.into()]));
    }

    #[test]
    fn opening_the_same_dm_twice_returns_one_room() {
        let db = test_db();
        db.call_blocking(|conn| {
            upsert_user(conn, ALICE, "alice", None, None).unwrap();
            upsert_user(conn, BOB, "bob", None, None).unwrap();

            let members = vec![ALICE.to_owned(), BOB.to_owned()];
            let first = create_dm(conn, "room_dm_1", &members).unwrap();
            // Reversed, which is exactly what happens when Bob replies by
            // opening the conversation from his side.
            let second = create_dm(conn, "room_dm_2", &[BOB.to_owned(), ALICE.to_owned()]).unwrap();

            assert_eq!(first.id, second.id, "a DM is its member set, not its id");
            assert_eq!(first.kind, ROOM_KIND_DM);
            assert!(first.is_direct());

            // Both sides are seated, and both can administer it: a DM has no
            // owner, so neither participant can lock the other out of it.
            for who in [ALICE, BOB] {
                assert!(is_member(conn, &first.id, who).unwrap());
                assert!(is_admin(conn, &first.id, who).unwrap());
            }

            // The loser's id was never used, so no orphan room was left.
            assert!(get_room(conn, "room_dm_2").unwrap().is_none());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn three_people_open_a_group_dm() {
        let db = test_db();
        db.call_blocking(|conn| {
            const CAROL: &str = "0xcccccccccccccccccccccccccccccccccccccccc";
            for (address, name) in [(ALICE, "alice"), (BOB, "bob"), (CAROL, "carol")] {
                upsert_user(conn, address, name, None, None).unwrap();
            }

            let room = create_dm(
                conn,
                "room_dm_group",
                &[CAROL.to_owned(), ALICE.to_owned(), BOB.to_owned()],
            )
            .unwrap();
            assert_eq!(room.kind, ROOM_KIND_GROUP_DM);
            assert!(room.is_direct());
            assert_eq!(list_members(conn, &room.id).unwrap().len(), 3);

            // A group DM with a different set is a different conversation,
            // not the same one with someone added.
            let pair =
                create_dm(conn, "room_dm_pair", &[ALICE.to_owned(), BOB.to_owned()]).unwrap();
            assert_ne!(pair.id, room.id);
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

    // ----------------------------------------------------- built-in rooms ---

    /// The address the provisioner derives for Alice's agent.
    fn alice_agent() -> String {
        pocketskynet_core::WalletAddress::agent_of(
            &pocketskynet_core::WalletAddress::new(ALICE).unwrap(),
        )
        .as_str()
        .to_owned()
    }

    #[test]
    fn a_static_room_id_names_its_kind_and_its_owner() {
        // The id *is* the ownership proof — no lookup table — so the two
        // properties that matter are that it is derived and that it is stable.
        assert_eq!(
            static_room_id(ROOM_KIND_NOTE, ALICE),
            format!("room_note_{ALICE}")
        );
        assert_eq!(
            static_room_id(ROOM_KIND_NOTE, ALICE),
            static_room_id(ROOM_KIND_NOTE, &ALICE.to_uppercase()),
            "a checksummed address must not mint a second, parallel note"
        );
        assert_ne!(
            static_room_id(ROOM_KIND_NOTE, ALICE),
            static_room_id(ROOM_KIND_NOTE, BOB)
        );
        assert_ne!(
            static_room_id(ROOM_KIND_NOTE, ALICE),
            static_room_id(ROOM_KIND_JARVIS, ALICE)
        );

        // And every one of them is a legal room id, which is what stops the
        // provisioner from writing rooms no route can address.
        for kind in STATIC_ROOM_KINDS {
            let id = static_room_id(kind, ALICE);
            assert!((10..=100).contains(&id.len()), "{id}");
            assert!(pocketskynet_core::RoomId::new(&id).is_ok(), "{id}");
        }
    }

    #[test]
    fn room_kinds_classify_into_direct_static_and_ordinary() {
        let room = |kind: &str| Room {
            id: ROOM.into(),
            name: "x".into(),
            description: None,
            current_key_version: 1,
            key_rotation_pending: false,
            kind: kind.into(),
            created_at: String::new(),
        };

        // The three axes every route reads, kept apart: a built-in room is not
        // a DM, and neither is an ordinary channel.
        for kind in STATIC_ROOM_KINDS {
            assert!(room(kind).is_static(), "{kind}");
            assert!(!room(kind).is_direct(), "{kind}");
            assert_eq!(room(kind).fixed_roster(), Some("a built-in room"));
        }
        for kind in [ROOM_KIND_DM, ROOM_KIND_GROUP_DM] {
            assert!(!room(kind).is_static(), "{kind}");
            assert!(room(kind).is_direct(), "{kind}");
            assert_eq!(room(kind).fixed_roster(), Some("a direct message"));
        }
        assert!(!room(ROOM_KIND_CHANNEL).is_static());
        assert!(!room(ROOM_KIND_CHANNEL).is_direct());
        assert_eq!(
            room(ROOM_KIND_CHANNEL).fixed_roster(),
            None,
            "an ordinary channel is the only kind whose roster somebody chose"
        );
    }

    #[test]
    fn provisioning_creates_all_three_with_the_rosters_their_kinds_promise() {
        let db = test_db();
        db.call_blocking(|conn| {
            upsert_user(conn, ALICE, "alice", None, None).unwrap();
            upsert_user(conn, BOB, "bob", None, None).unwrap();
            provision_static_rooms(conn, ALICE, &[BOB.to_owned()]).unwrap();

            let note = static_room_id(ROOM_KIND_NOTE, ALICE);
            let jarvis = static_room_id(ROOM_KIND_JARVIS, ALICE);
            let lobby = static_room_id(ROOM_KIND_LOBBY, ALICE);

            for id in [&note, &jarvis, &lobby] {
                let room = get_room(conn, id).unwrap().unwrap();
                assert!(room.is_static(), "{id}");
                assert!(is_member(conn, id, ALICE).unwrap(), "{id}");
                assert!(is_admin(conn, id, ALICE).unwrap(), "{id}");
            }
            assert_eq!(get_room(conn, &note).unwrap().unwrap().kind, ROOM_KIND_NOTE);

            // The note is alone, forever — the property the whole room exists
            // for.
            let members: Vec<String> = list_members(conn, &note)
                .unwrap()
                .into_iter()
                .map(|m| m.user_address)
                .collect();
            assert_eq!(members, vec![ALICE.to_owned()]);

            // Jarvis holds the owner and the derived agent, and the agent has
            // a profile row or the roster would silently drop it.
            let mut jarvis_members: Vec<String> = list_members(conn, &jarvis)
                .unwrap()
                .into_iter()
                .map(|m| m.user_address)
                .collect();
            jarvis_members.sort();
            let mut want = vec![ALICE.to_owned(), alice_agent()];
            want.sort();
            assert_eq!(jarvis_members, want);

            // The lobby holds the owner and the server's admins.
            assert!(is_member(conn, &lobby, BOB).unwrap());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn provisioning_is_idempotent_and_survives_being_run_repeatedly() {
        let db = test_db();
        db.call_blocking(|conn| {
            upsert_user(conn, ALICE, "alice", None, None).unwrap();
            // Every room-list fetch runs this, so "cheap and repeatable" is
            // not a nicety — three tabs do it in the same second.
            for _ in 0..3 {
                provision_static_rooms(conn, ALICE, &[]).unwrap();
            }
            let note = static_room_id(ROOM_KIND_NOTE, ALICE);
            assert_eq!(list_members(conn, &note).unwrap().len(), 1);
            assert_eq!(admin_count(conn, &note).unwrap(), 1);
            assert_eq!(visible_room_ids(conn, ALICE).unwrap().len(), 3);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn the_lobby_roster_follows_the_configured_admins_in_both_directions() {
        let db = test_db();
        db.call_blocking(|conn| {
            for (address, name) in [(ALICE, "alice"), (BOB, "bob")] {
                upsert_user(conn, address, name, None, None).unwrap();
            }
            let lobby = static_room_id(ROOM_KIND_LOBBY, ALICE);

            // Bob is promoted in the deployment's config.
            provision_static_rooms(conn, ALICE, &[BOB.to_owned()]).unwrap();
            assert!(is_member(conn, &lobby, BOB).unwrap());

            // …and demoted again. Nobody issued a kick — the roster is a
            // function of the config, so it has to shrink on its own.
            provision_static_rooms(conn, ALICE, &[]).unwrap();
            assert!(!is_member(conn, &lobby, BOB).unwrap());
            assert!(
                is_member(conn, &lobby, ALICE).unwrap(),
                "the owner is never reconciled away from their own lobby"
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_second_wallet_smuggled_into_a_note_is_reconciled_out_of_it() {
        let db = test_db();
        db.call_blocking(|conn| {
            for (address, name) in [(ALICE, "alice"), (BOB, "bob")] {
                upsert_user(conn, address, name, None, None).unwrap();
            }
            provision_static_rooms(conn, ALICE, &[]).unwrap();
            let note = static_room_id(ROOM_KIND_NOTE, ALICE);

            // The routes refuse every verb that could do this; the point of
            // the test is that the invariant does not *depend* on them. A row
            // inserted straight into the table — a future route, a migration,
            // a hand-edited database — is gone by the next listing.
            add_member(conn, &note, BOB).unwrap();
            mark_read(conn, &note, BOB, 42).unwrap();
            hide_room(conn, BOB, &note).unwrap();
            assert!(is_member(conn, &note, BOB).unwrap());

            provision_static_rooms(conn, ALICE, &[]).unwrap();

            assert!(!is_member(conn, &note, BOB).unwrap());
            assert_eq!(last_read_serial(conn, &note, BOB).unwrap(), 0);
            assert!(list_hidden(conn, BOB).unwrap().is_empty());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn hiding_a_built_in_room_works_exactly_like_hiding_any_other() {
        let db = test_db();
        db.call_blocking(|conn| {
            upsert_user(conn, ALICE, "alice", None, None).unwrap();
            provision_static_rooms(conn, ALICE, &[]).unwrap();
            let note = static_room_id(ROOM_KIND_NOTE, ALICE);

            hide_room(conn, ALICE, &note).unwrap();
            assert_eq!(visible_room_ids(conn, ALICE).unwrap().len(), 2);
            assert_eq!(
                user_room_ids(conn, ALICE).unwrap().len(),
                3,
                "hiding is a list preference, not a departure"
            );

            // Reversible, and — the part that could plausibly have broken —
            // provisioning must not quietly unhide it on the next fetch.
            provision_static_rooms(conn, ALICE, &[]).unwrap();
            assert_eq!(visible_room_ids(conn, ALICE).unwrap().len(), 2);

            unhide_room(conn, ALICE, &note).unwrap();
            assert_eq!(visible_room_ids(conn, ALICE).unwrap().len(), 3);
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
