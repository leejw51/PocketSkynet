//! Users, encryption salts, login challenges, and the block list.

use std::collections::HashSet;

use rusqlite::{params, Connection, OptionalExtension};

use super::models::{iso_ms, BlockedUser, PublicKeyEntry, User};
use super::now_ms;
use crate::error::ApiResult;

// ----------------------------------------------------------------- users ---

pub fn get_user(conn: &Connection, address: &str) -> ApiResult<Option<User>> {
    let user = conn
        .query_row(
            "SELECT wallet_address, username, public_key, public_key_sig, profile_image,
                    created_at, updated_at
             FROM users WHERE wallet_address = ?1",
            params![address],
            User::from_row,
        )
        .optional()?;
    Ok(user)
}

pub fn user_exists(conn: &Connection, address: &str) -> ApiResult<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM users WHERE wallet_address = ?1",
        params![address],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Create or update a user at login.
///
/// **§15 #3.** The reference computed `publicKeySig` as `null` whenever
/// `publicKey` was absent, and Drizzle then emitted that `null` in the `SET`
/// clause — so every ordinary login silently un-bound the account's
/// encryption key, leaving `public_key` populated but unverifiable until the
/// client happened to re-publish. Here an absent `public_key` means "do not
/// touch either key column".
pub fn upsert_user(
    conn: &Connection,
    address: &str,
    username: &str,
    public_key: Option<&str>,
    public_key_sig: Option<&str>,
) -> ApiResult<User> {
    let now = now_ms();

    match public_key {
        Some(pk) => {
            conn.execute(
                "INSERT INTO users (wallet_address, username, public_key, public_key_sig,
                                    created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                 ON CONFLICT (wallet_address) DO UPDATE SET
                     username       = excluded.username,
                     public_key     = excluded.public_key,
                     public_key_sig = excluded.public_key_sig,
                     updated_at     = excluded.updated_at",
                params![address, username, pk, public_key_sig, now],
            )?;
        }
        None => {
            conn.execute(
                "INSERT INTO users (wallet_address, username, public_key, public_key_sig,
                                    created_at, updated_at)
                 VALUES (?1, ?2, NULL, NULL, ?3, ?3)
                 ON CONFLICT (wallet_address) DO UPDATE SET
                     username   = excluded.username,
                     updated_at = excluded.updated_at",
                params![address, username, now],
            )?;
        }
    }

    get_user(conn, address)?.ok_or_else(|| {
        crate::error::ApiError::Internal(anyhow::anyhow!("user vanished after upsert"))
    })
}

/// Update a user's profile. Returns `None` when there is no such account, so
/// the caller can answer 404 instead of dereferencing nothing (the reference
/// threw here and answered 500).
///
/// `profile_image` is three-valued: `None` leaves the stored avatar alone,
/// `Some(None)` clears it back to the hash-derived default, `Some(Some(v))`
/// sets it. The outer option exists so a rename does not silently wipe an
/// avatar the request never mentioned.
pub fn update_profile(
    conn: &Connection,
    address: &str,
    username: &str,
    profile_image: Option<Option<&str>>,
) -> ApiResult<Option<User>> {
    let changed = match profile_image {
        None => conn.execute(
            "UPDATE users SET username = ?2, updated_at = ?3 WHERE wallet_address = ?1",
            params![address, username, now_ms()],
        )?,
        Some(image) => conn.execute(
            "UPDATE users SET username = ?2, profile_image = ?3, updated_at = ?4
             WHERE wallet_address = ?1",
            params![address, username, image, now_ms()],
        )?,
    };
    if changed == 0 {
        return Ok(None);
    }
    get_user(conn, address)
}

/// Publish or rotate the caller's encryption key together with its binding
/// signature. The two columns always move as a pair: a key without a valid
/// signature is unusable, and clients are required to refuse to wrap to one.
pub fn set_encryption_key(
    conn: &Connection,
    address: &str,
    public_key: &str,
    public_key_sig: &str,
) -> ApiResult<bool> {
    let changed = conn.execute(
        "UPDATE users SET public_key = ?2, public_key_sig = ?3, updated_at = ?4
         WHERE wallet_address = ?1",
        params![address, public_key, public_key_sig, now_ms()],
    )?;
    Ok(changed > 0)
}

/// Substring search over username and address.
///
/// Blocking is **bidirectional** here: people the viewer blocked and people
/// who blocked the viewer are both invisible, so a block cannot be worked
/// around by searching.
///
/// §15 #19: the reference had no `LIMIT` and no `ORDER BY`, which made
/// `?q=0x` a full user dump in indeterminate order. Results are capped and
/// ordered by username.
///
/// Case-insensitivity is SQLite's ASCII-only `LIKE`. Matching Unicode case
/// would need the ICU extension; usernames in other scripts are matched
/// exactly, which is the same behaviour Postgres `ILIKE` gives without a
/// collation.
pub fn search_users(
    conn: &Connection,
    viewer: &str,
    query: &str,
    limit: i64,
) -> ApiResult<Vec<User>> {
    // Escape LIKE metacharacters so a query of "100%" is a literal search
    // rather than a wildcard that matches everything.
    let mut escaped = String::with_capacity(query.len() + 8);
    for c in query.chars() {
        if matches!(c, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    let pattern = format!("%{escaped}%");

    // Webhook identities (`WEBHOOK_SENDER_PREFIX`) are users rows so their
    // posts render with a name, but they are not people: everything this
    // search feeds — a DM, an invitation, a block — acts on an address, and
    // acting on one nobody holds only manufactures dead-end errors. A human
    // who ground out a vanity address under the reserved prefix is filtered
    // too; looking like a robot comes with being treated as one.
    let mut stmt = conn.prepare(&format!(
        "SELECT wallet_address, username, public_key, public_key_sig, profile_image,
                created_at, updated_at
         FROM users
         WHERE (username LIKE ?1 ESCAPE '\\' OR wallet_address LIKE ?1 ESCAPE '\\')
           AND wallet_address NOT LIKE '{}%'
           AND wallet_address NOT IN
               (SELECT blocked_address FROM blocked_users WHERE blocker_address = ?2)
           AND wallet_address NOT IN
               (SELECT blocker_address FROM blocked_users WHERE blocked_address = ?2)
         ORDER BY username, wallet_address
         LIMIT ?3",
        pocketskynet_core::WEBHOOK_SENDER_PREFIX
    ))?;
    let rows = stmt.query_map(params![pattern, viewer, limit], User::from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Resolve published encryption keys, preserving the request order.
///
/// Addresses that are unknown or have no key are dropped silently: the caller
/// asked "who can I wrap to", and the answer for those is "nobody", not an
/// error. A `null` `publicKeySig` is returned as-is so the client can refuse
/// to wrap — it must never be treated as "unsigned but fine".
pub fn get_public_keys(conn: &Connection, addresses: &[String]) -> ApiResult<Vec<PublicKeyEntry>> {
    let mut out = Vec::with_capacity(addresses.len());
    let mut stmt = conn.prepare(
        "SELECT wallet_address, public_key, public_key_sig
         FROM users WHERE wallet_address = ?1 AND public_key IS NOT NULL",
    )?;

    for address in addresses {
        let entry = stmt
            .query_row(params![address], |row| {
                Ok(PublicKeyEntry {
                    wallet_address: row.get(0)?,
                    public_key: row.get(1)?,
                    public_key_sig: row.get(2)?,
                })
            })
            .optional()?;
        if let Some(entry) = entry {
            out.push(entry);
        }
    }
    Ok(out)
}

// ------------------------------------------------------ encryption salts ---

/// Return the caller's derivation salt, creating one on first use.
///
/// The insert is `ON CONFLICT DO NOTHING` followed by a read, so two
/// concurrent first logins both end up with the winner's salt rather than one
/// of them silently deriving a different encryption identity.
///
/// The candidate is 64 hex characters from [`pocketskynet_core::random::hex_32`]
/// — the same choke point every secret in the deployment draws from, but *not*
/// `auth::random_hex_32`, whose name is reserved for bearer tokens; a salt is
/// key-derivation input, not a credential anybody presents. It is drawn *before*
/// the insert even though it is usually thrown away. That order is deliberate: a
/// salt is what separates this account's E2EE identity from the one the same
/// wallet would derive on another deployment, and a predictable salt would
/// quietly undo that for the first login it ever served. Refusing the login is
/// the correct answer; there is no salt worth writing that the OS did not
/// produce.
pub fn get_or_create_salt(conn: &Connection, address: &str) -> ApiResult<String> {
    let candidate = pocketskynet_core::random::hex_32()?;

    conn.execute(
        "INSERT INTO encryption_salts (wallet_address, salt, created_at)
         VALUES (?1, ?2, ?3) ON CONFLICT (wallet_address) DO NOTHING",
        params![address, candidate, now_ms()],
    )?;

    let salt = conn.query_row(
        "SELECT salt FROM encryption_salts WHERE wallet_address = ?1",
        params![address],
        |r| r.get(0),
    )?;
    Ok(salt)
}

// ------------------------------------------------------- auth challenges ---

pub struct Challenge {
    pub wallet_address: String,
    pub message: String,
    pub expires_at: i64,
}

/// Delete expired challenges. Called opportunistically on every challenge
/// request, which keeps the table bounded without a background task.
pub fn gc_challenges(conn: &Connection) -> ApiResult<()> {
    conn.execute(
        "DELETE FROM auth_challenges WHERE expires_at < ?1",
        params![now_ms()],
    )?;
    Ok(())
}

pub fn insert_challenge(
    conn: &Connection,
    id: &str,
    address: &str,
    nonce: &str,
    message: &str,
    expires_at: i64,
) -> ApiResult<()> {
    conn.execute(
        "INSERT INTO auth_challenges (id, wallet_address, nonce, message, expires_at, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, address, nonce, message, expires_at, now_ms()],
    )?;
    Ok(())
}

/// Atomically take a challenge, whatever the outcome of the login.
///
/// Consuming before validation is what makes a challenge single-use: a signature
/// replayed against the same challenge finds nothing to verify against, and a
/// failed attempt costs the attacker a fresh (rate-limited) challenge request.
pub fn consume_challenge(conn: &Connection, id: &str) -> ApiResult<Option<Challenge>> {
    let challenge = conn
        .query_row(
            "DELETE FROM auth_challenges WHERE id = ?1
             RETURNING wallet_address, message, expires_at",
            params![id],
            |row| {
                Ok(Challenge {
                    wallet_address: row.get(0)?,
                    message: row.get(1)?,
                    expires_at: row.get(2)?,
                })
            },
        )
        .optional()?;
    Ok(challenge)
}

// -------------------------------------------------------------- blocking ---

fn block_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<BlockedUser> {
    Ok(BlockedUser {
        id: row.get("id")?,
        blocker_address: row.get("blocker_address")?,
        blocked_address: row.get("blocked_address")?,
        created_at: iso_ms(row.get("created_at")?),
    })
}

/// Everyone `viewer` has blocked.
pub fn list_blocked(conn: &Connection, viewer: &str) -> ApiResult<Vec<BlockedUser>> {
    let mut stmt = conn.prepare(
        "SELECT id, blocker_address, blocked_address, created_at
         FROM blocked_users WHERE blocker_address = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map(params![viewer], block_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Everyone who has blocked `viewer`.
///
/// This is deliberately observable by the blocked party: native clients need
/// it to apply the same symmetric filtering the web client does. It leaks the
/// fact of a block, which is a conscious trade for consistent behaviour.
pub fn list_blocked_by(conn: &Connection, viewer: &str) -> ApiResult<Vec<BlockedUser>> {
    let mut stmt = conn.prepare(
        "SELECT id, blocker_address, blocked_address, created_at
         FROM blocked_users WHERE blocked_address = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map(params![viewer], block_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Block `target`, idempotently.
///
/// §15 #4: with a unique index in place the conflict clause finally does what
/// the reference intended, so repeated calls no longer duplicate the row (and
/// no longer duplicate the entry in `GET /api/users/blocked`).
pub fn block_user(conn: &Connection, blocker: &str, target: &str) -> ApiResult<BlockedUser> {
    conn.execute(
        "INSERT INTO blocked_users (blocker_address, blocked_address, created_at)
         VALUES (?1, ?2, ?3) ON CONFLICT (blocker_address, blocked_address) DO NOTHING",
        params![blocker, target, now_ms()],
    )?;
    let row = conn.query_row(
        "SELECT id, blocker_address, blocked_address, created_at
         FROM blocked_users WHERE blocker_address = ?1 AND blocked_address = ?2",
        params![blocker, target],
        block_row,
    )?;
    Ok(row)
}

/// Unblock, permissively: unblocking someone who was never blocked succeeds.
/// There is nothing for a caller to do about "you had not blocked them", and
/// answering 404 would leak whether a block exists.
pub fn unblock_user(conn: &Connection, blocker: &str, target: &str) -> ApiResult<()> {
    conn.execute(
        "DELETE FROM blocked_users WHERE blocker_address = ?1 AND blocked_address = ?2",
        params![blocker, target],
    )?;
    Ok(())
}

/// "Have I blocked them?" — directed, not symmetric.
pub fn is_blocked(conn: &Connection, blocker: &str, target: &str) -> ApiResult<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM blocked_users WHERE blocker_address = ?1 AND blocked_address = ?2",
        params![blocker, target],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// The viewer-side read filter: senders whose content the viewer must not see.
pub fn blocked_by_viewer(conn: &Connection, viewer: &str) -> ApiResult<HashSet<String>> {
    let mut stmt =
        conn.prepare("SELECT blocked_address FROM blocked_users WHERE blocker_address = ?1")?;
    let rows = stmt.query_map(params![viewer], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<HashSet<_>>>()?)
}

/// The realtime filter: everyone the viewer blocked **union** everyone who
/// blocked the viewer. Presence signals (typing) must not cross a block in
/// either direction, or the block becomes observable as an activity oracle.
pub fn mutual_block_set(conn: &Connection, viewer: &str) -> ApiResult<HashSet<String>> {
    let mut stmt = conn.prepare(
        "SELECT blocked_address FROM blocked_users WHERE blocker_address = ?1
         UNION
         SELECT blocker_address FROM blocked_users WHERE blocked_address = ?1",
    )?;
    let rows = stmt.query_map(params![viewer], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<HashSet<_>>>()?)
}

/// Everyone the viewer shares at least one room with, themselves excluded.
///
/// The visibility rule for presence, and the reason presence is not a
/// directory: sharing a room is the whole of what entitles you to know whether
/// somebody is at their desk. A server admin gets no exemption — knowing who is
/// online in a conversation you are not in is exactly the kind of thing §0 says
/// the admin role does not grant.
///
/// Hidden rooms are *not* excluded. Hiding a room removes it from your list; it
/// does not remove you from the room, and the people in it can still see you
/// arrive, so pretending you cannot see them would be a one-way mirror rather
/// than a privacy setting.
pub fn room_peers(conn: &Connection, viewer: &str) -> ApiResult<HashSet<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT peer.user_address
           FROM room_members mine
           JOIN room_members peer ON peer.room_id = mine.room_id
          WHERE mine.user_address = ?1
            AND peer.user_address <> ?1",
    )?;
    let rows = stmt.query_map(params![viewer], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<HashSet<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    const ALICE: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BOB: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const CAROL: &str = "0xcccccccccccccccccccccccccccccccccccccccc";

    fn seed(conn: &Connection) {
        upsert_user(conn, ALICE, "alice", None, None).unwrap();
        upsert_user(conn, BOB, "bob", None, None).unwrap();
        upsert_user(conn, CAROL, "carol", None, None).unwrap();
    }

    #[test]
    fn a_plain_login_never_unbinds_the_encryption_key() {
        let db = test_db();
        db.call_blocking(|conn| {
            upsert_user(conn, ALICE, "alice", Some("04aa"), Some("0xsig")).unwrap();
            // §15 #3: this is the login that used to NULL out public_key_sig.
            let after = upsert_user(conn, ALICE, "alice2", None, None).unwrap();

            assert_eq!(after.username, "alice2");
            assert_eq!(after.public_key.as_deref(), Some("04aa"));
            assert_eq!(
                after.public_key_sig.as_deref(),
                Some("0xsig"),
                "an ordinary login must not silently un-bind the key"
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn republishing_a_key_replaces_both_columns_together() {
        let db = test_db();
        db.call_blocking(|conn| {
            upsert_user(conn, ALICE, "alice", Some("04aa"), Some("0xsig1")).unwrap();
            let after = upsert_user(conn, ALICE, "alice", Some("04bb"), Some("0xsig2")).unwrap();
            assert_eq!(after.public_key.as_deref(), Some("04bb"));
            assert_eq!(after.public_key_sig.as_deref(), Some("0xsig2"));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn search_hides_blocks_in_both_directions() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            block_user(conn, ALICE, BOB).unwrap();
            block_user(conn, CAROL, ALICE).unwrap();

            let names: Vec<String> = search_users(conn, ALICE, "0x", 50)
                .unwrap()
                .into_iter()
                .map(|u| u.username)
                .collect();

            assert!(names.contains(&"alice".to_string()), "self is not excluded");
            assert!(!names.contains(&"bob".to_string()), "alice blocked bob");
            assert!(!names.contains(&"carol".to_string()), "carol blocked alice");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn search_escapes_like_metacharacters() {
        let db = test_db();
        db.call_blocking(|conn| {
            upsert_user(conn, ALICE, "100%off", None, None).unwrap();
            upsert_user(conn, BOB, "bob", None, None).unwrap();

            let hits = search_users(conn, CAROL, "%", 50).unwrap();
            assert_eq!(hits.len(), 1, "a bare % must not match everything");
            assert_eq!(hits[0].username, "100%off");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn search_is_capped_and_ordered() {
        let db = test_db();
        db.call_blocking(|conn| {
            for i in 0..10 {
                // Not `{:040x}`: a small integer zero-padded to 40 hex digits
                // lands inside `WEBHOOK_SENDER_PREFIX`, and the search rightly
                // hides webhook identities — these must look like people.
                let addr = format!("0xaa{:038x}", i);
                upsert_user(conn, &addr, &format!("user{:02}", 9 - i), None, None).unwrap();
            }
            let hits = search_users(conn, ALICE, "user", 3).unwrap();
            assert_eq!(hits.len(), 3, "the cap must be applied");
            assert_eq!(
                hits[0].username, "user00",
                "results are ordered by username"
            );
            assert_eq!(hits[2].username, "user02");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn public_keys_keep_request_order_and_drop_the_keyless() {
        let db = test_db();
        db.call_blocking(|conn| {
            upsert_user(conn, ALICE, "alice", Some("04aa"), Some("0xsig")).unwrap();
            upsert_user(conn, BOB, "bob", None, None).unwrap();

            let out = get_public_keys(
                conn,
                &[BOB.to_string(), ALICE.to_string(), CAROL.to_string()],
            )
            .unwrap();

            assert_eq!(out.len(), 1, "keyless and unknown addresses are dropped");
            assert_eq!(out[0].wallet_address, ALICE);
            assert_eq!(out[0].public_key_sig.as_deref(), Some("0xsig"));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn salts_are_stable_once_created() {
        let db = test_db();
        db.call_blocking(|conn| {
            let first = get_or_create_salt(conn, ALICE).unwrap();
            let second = get_or_create_salt(conn, ALICE).unwrap();
            assert_eq!(
                first, second,
                "a changed salt would orphan the E2EE identity"
            );
            assert_eq!(first.len(), 64);
            assert_ne!(first, get_or_create_salt(conn, BOB).unwrap());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_challenge_can_only_be_consumed_once() {
        let db = test_db();
        db.call_blocking(|conn| {
            insert_challenge(conn, "chal-1", ALICE, "nonce", "message", now_ms() + 1000).unwrap();

            let first = consume_challenge(conn, "chal-1").unwrap();
            assert!(first.is_some());
            assert!(
                consume_challenge(conn, "chal-1").unwrap().is_none(),
                "a replayed challenge must find nothing to verify against"
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn expired_challenges_are_collected() {
        let db = test_db();
        db.call_blocking(|conn| {
            insert_challenge(conn, "old", ALICE, "n", "m", now_ms() - 1).unwrap();
            insert_challenge(conn, "new", ALICE, "n", "m", now_ms() + 60_000).unwrap();
            gc_challenges(conn).unwrap();

            assert!(consume_challenge(conn, "old").unwrap().is_none());
            assert!(consume_challenge(conn, "new").unwrap().is_some());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn blocking_is_idempotent_and_directed() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            let first = block_user(conn, ALICE, BOB).unwrap();
            let again = block_user(conn, ALICE, BOB).unwrap();
            assert_eq!(first.id, again.id, "§15 #4: no duplicate rows");
            assert_eq!(list_blocked(conn, ALICE).unwrap().len(), 1);

            assert!(is_blocked(conn, ALICE, BOB).unwrap());
            assert!(
                !is_blocked(conn, BOB, ALICE).unwrap(),
                "storage is one-directional"
            );
            assert_eq!(list_blocked_by(conn, BOB).unwrap().len(), 1);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn unblocking_is_permissive() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            unblock_user(conn, ALICE, BOB).unwrap();
            block_user(conn, ALICE, BOB).unwrap();
            unblock_user(conn, ALICE, BOB).unwrap();
            assert!(list_blocked(conn, ALICE).unwrap().is_empty());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn the_realtime_block_set_is_the_union_of_both_directions() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed(conn);
            block_user(conn, ALICE, BOB).unwrap();
            block_user(conn, CAROL, ALICE).unwrap();

            let mutual = mutual_block_set(conn, ALICE).unwrap();
            assert!(mutual.contains(BOB) && mutual.contains(CAROL));

            let directed = blocked_by_viewer(conn, ALICE).unwrap();
            assert!(directed.contains(BOB));
            assert!(
                !directed.contains(CAROL),
                "read filtering is viewer-side only"
            );
            Ok(())
        })
        .unwrap();
    }
}
