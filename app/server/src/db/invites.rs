//! Invite links: the row half of the onboarding funnel (`docs/API.md` §6.7a,
//! ROADMAP §7 M1).
//!
//! An invite link is a bearer capability: `inv_` plus 32 CSPRNG bytes in hex,
//! minted by [`mint_token`] and never written down anywhere — only its SHA-256
//! is stored, so recognising a presented token is a hash and a lookup while a
//! copy of the table yields nothing redeemable. That asymmetry is the whole
//! design: the same property a password hash buys a password, bought for a
//! credential that grants room membership.
//!
//! The interesting rule is in [`consume`]: expiry, revocation and the use
//! budget are all enforced by one *conditional* UPDATE, never by a read
//! followed by a write. Two redeems racing on a one-use link would both read
//! `use_count = 0` and both believe they were first; the `WHERE` makes the
//! loser's update affect zero rows, and the re-read afterwards only decides
//! which refusal to phrase.

use rusqlite::{params, Connection, OptionalExtension, Row};
use sha2::{Digest, Sha256};

use crate::db::now_ms;
use crate::error::ApiResult;

/// One invite link, minus the token that opens it.
///
/// The token exists in exactly two places: the response to the create call,
/// and whatever link or QR code the admin made from it. This struct is what
/// the list and revoke endpoints see — enough to answer "what did I hand out
/// and is it spent", not enough to reconstruct the link.
#[derive(Debug, Clone)]
pub struct Invite {
    pub id: String,
    pub room_id: String,
    pub created_by: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub max_uses: Option<i64>,
    pub use_count: i64,
    pub revoked_at: Option<i64>,
}

const SELECT_COLUMNS: &str =
    "id, room_id, created_by, created_at, expires_at, max_uses, use_count, revoked_at";

impl Invite {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            room_id: row.get("room_id")?,
            created_by: row.get("created_by")?,
            created_at: row.get("created_at")?,
            expires_at: row.get("expires_at")?,
            max_uses: row.get("max_uses")?,
            use_count: row.get("use_count")?,
            revoked_at: row.get("revoked_at")?,
        })
    }

    /// Whether a redeem presented right now would be refused, and why.
    /// `None` means the link is live. Order matters and matches [`consume`]:
    /// a revoked link reports "revoked" even after it also expired, because
    /// revocation was somebody's decision and expiry is just time passing.
    pub fn refusal(&self, now: i64) -> Option<Refusal> {
        if self.revoked_at.is_some() {
            return Some(Refusal::Revoked);
        }
        if self.expires_at <= now {
            return Some(Refusal::Expired);
        }
        if let Some(max) = self.max_uses {
            if self.use_count >= max {
                return Some(Refusal::Exhausted);
            }
        }
        None
    }
}

/// Why a presented token bought nothing. Each variant gets its own wording at
/// the route layer — the holder of a real token deserves the truth about what
/// happened to it, and "not found" is reserved for tokens that never existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Revoked,
    Expired,
    Exhausted,
}

/// What one redeem attempt did.
#[derive(Debug)]
pub enum ConsumeOutcome {
    /// The use was counted; the invite (post-increment) rides along so the
    /// caller can seat the member without a second lookup.
    Consumed(Invite),
    /// No row has this hash — the token was never issued (or guessed).
    NotFound,
    /// The row exists but refused; see [`Invite::refusal`].
    Refused(Refusal),
}

/// The bearer token: `inv_` + 32 CSPRNG bytes, hex. Same generator as the
/// login challenge nonce (`auth::random_hex_32`), and the prefix keeps a token
/// visually distinct from a row id (`invite_…`) in logs and bug reports.
///
/// Fallible for the reason that generator is: only the hash of this string is
/// ever stored, so a predictable token is a room membership anybody can mint
/// and nothing in the table would show it. Refusing to issue the link is the
/// only safe answer to an entropy failure.
pub fn mint_token() -> ApiResult<String> {
    Ok(format!("inv_{}", crate::auth::random_hex_32()?))
}

/// Lowercase-hex SHA-256 of a presented token — the only form that ever
/// reaches the database, on write and on lookup alike.
pub fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn create(
    conn: &Connection,
    id: &str,
    room_id: &str,
    token_hash: &str,
    created_by: &str,
    expires_at: i64,
    max_uses: Option<i64>,
) -> ApiResult<Invite> {
    let now = now_ms();
    conn.execute(
        "INSERT INTO room_invites (id, room_id, token_hash, created_by,
                                   created_at, expires_at, max_uses, use_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
        params![id, room_id, token_hash, created_by, now, expires_at, max_uses],
    )?;
    Ok(Invite {
        id: id.to_owned(),
        room_id: room_id.to_owned(),
        created_by: created_by.to_owned(),
        created_at: now,
        expires_at,
        max_uses,
        use_count: 0,
        revoked_at: None,
    })
}

/// Every non-revoked invite for a room, newest first. Expired and exhausted
/// links are included — the list is the admin's ledger of what is out there,
/// and a link that just lapsed is exactly the one they want to see and reissue.
pub fn list_for_room(conn: &Connection, room_id: &str) -> ApiResult<Vec<Invite>> {
    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM room_invites
         WHERE room_id = ?1 AND revoked_at IS NULL
         ORDER BY created_at DESC, id DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![room_id], Invite::from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Look a token up by its hash without spending a use — what the landing page
/// calls to show "you're invited to «Team chat»" before anybody signs in.
pub fn find_by_hash(conn: &Connection, hash: &str) -> ApiResult<Option<Invite>> {
    let sql = format!("SELECT {SELECT_COLUMNS} FROM room_invites WHERE token_hash = ?1");
    let invite = conn
        .query_row(&sql, params![hash], Invite::from_row)
        .optional()?;
    Ok(invite)
}

/// Kill a link now. Scoped to the room so an id learned elsewhere cannot
/// revoke across rooms; returns whether a live link was actually revoked, and
/// revoking twice is `false` rather than an error — the state the admin asked
/// for already holds.
pub fn revoke(conn: &Connection, room_id: &str, invite_id: &str) -> ApiResult<bool> {
    let changed = conn.execute(
        "UPDATE room_invites SET revoked_at = ?3
         WHERE id = ?1 AND room_id = ?2 AND revoked_at IS NULL",
        params![invite_id, room_id, now_ms()],
    )?;
    Ok(changed > 0)
}

/// Spend one use of a token, atomically.
///
/// The UPDATE's `WHERE` re-states every condition a redeem requires — live,
/// unexpired, under budget — so the increment and the checks are one
/// statement and there is no window in which a racing redeem sees stale
/// state. Zero rows changed means *something* refused; only then is the row
/// re-read, purely to phrase the refusal.
pub fn consume(conn: &Connection, hash: &str) -> ApiResult<ConsumeOutcome> {
    let now = now_ms();
    let changed = conn.execute(
        "UPDATE room_invites SET use_count = use_count + 1
         WHERE token_hash = ?1
           AND revoked_at IS NULL
           AND expires_at > ?2
           AND (max_uses IS NULL OR use_count < max_uses)",
        params![hash, now],
    )?;
    let Some(invite) = find_by_hash(conn, hash)? else {
        return Ok(ConsumeOutcome::NotFound);
    };
    if changed > 0 {
        return Ok(ConsumeOutcome::Consumed(invite));
    }
    match invite.refusal(now) {
        Some(refusal) => Ok(ConsumeOutcome::Refused(refusal)),
        // The row was refused by the UPDATE yet looks live on re-read: the
        // only way is a clock moving backwards between the two statements.
        // Expired is the conservative reading — it tells the holder to ask
        // for a fresh link rather than to keep retrying this one.
        None => Ok(ConsumeOutcome::Refused(Refusal::Expired)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;
    use rusqlite::Connection;

    fn seed_room(conn: &mut Connection, room_id: &str) {
        crate::db::rooms::create_room(conn, room_id, "Team", None, "0xabc").expect("room");
    }

    fn make(conn: &Connection, expires_at: i64, max_uses: Option<i64>) -> (String, Invite) {
        let token = mint_token().unwrap();
        let invite = create(
            conn,
            &format!("invite_{}", uuid::Uuid::new_v4()),
            "room_1",
            &token_hash(&token),
            "0xabc",
            expires_at,
            max_uses,
        )
        .expect("invite");
        (token, invite)
    }

    #[test]
    fn a_minted_token_is_unguessable_shaped_and_hashes_stably() {
        let token = mint_token().unwrap();
        assert!(token.starts_with("inv_"));
        assert_eq!(token.len(), 4 + 64);
        assert_ne!(token, mint_token().unwrap(), "two mints must differ");
        assert_eq!(token_hash(&token), token_hash(&token));
        assert_eq!(token_hash(&token).len(), 64);
        assert_ne!(
            token_hash(&token),
            token,
            "the stored form must not be the token"
        );
    }

    #[test]
    fn a_valid_token_is_found_by_hash_and_consumed_counts_a_use() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed_room(conn, "room_1");
            let (token, invite) = make(conn, now_ms() + 60_000, None);

            let found = find_by_hash(conn, &token_hash(&token))?.expect("row");
            assert_eq!(found.id, invite.id);
            assert_eq!(found.use_count, 0);

            match consume(conn, &token_hash(&token))? {
                ConsumeOutcome::Consumed(after) => assert_eq!(after.use_count, 1),
                other => panic!("expected Consumed, got {other:?}"),
            }
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn an_unissued_token_is_not_found_not_refused() {
        let db = test_db();
        db.call_blocking(|conn| {
            let outcome = consume(conn, &token_hash(&mint_token().unwrap()))?;
            assert!(matches!(outcome, ConsumeOutcome::NotFound));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn expiry_is_enforced_at_consume_time() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed_room(conn, "room_1");
            let (token, _) = make(conn, now_ms() - 1, None);
            let outcome = consume(conn, &token_hash(&token))?;
            assert!(matches!(outcome, ConsumeOutcome::Refused(Refusal::Expired)));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn revocation_takes_effect_immediately_and_twice_is_a_no_op() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed_room(conn, "room_1");
            let (token, invite) = make(conn, now_ms() + 60_000, None);

            assert!(revoke(conn, "room_1", &invite.id)?);
            assert!(!revoke(conn, "room_1", &invite.id)?, "already revoked");

            let outcome = consume(conn, &token_hash(&token))?;
            assert!(matches!(outcome, ConsumeOutcome::Refused(Refusal::Revoked)));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn revoking_from_the_wrong_room_changes_nothing() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed_room(conn, "room_1");
            let (token, invite) = make(conn, now_ms() + 60_000, None);
            assert!(!revoke(conn, "room_2", &invite.id)?);
            assert!(matches!(
                consume(conn, &token_hash(&token))?,
                ConsumeOutcome::Consumed(_)
            ));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn the_use_budget_runs_out_exactly_on_the_boundary() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed_room(conn, "room_1");
            let (token, _) = make(conn, now_ms() + 60_000, Some(2));
            let hash = token_hash(&token);

            assert!(matches!(consume(conn, &hash)?, ConsumeOutcome::Consumed(_)));
            assert!(matches!(consume(conn, &hash)?, ConsumeOutcome::Consumed(_)));
            assert!(matches!(
                consume(conn, &hash)?,
                ConsumeOutcome::Refused(Refusal::Exhausted)
            ));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn the_room_list_hides_revoked_but_keeps_expired_links() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed_room(conn, "room_1");
            let (_, live) = make(conn, now_ms() + 60_000, None);
            let (_, expired) = make(conn, now_ms() - 1, None);
            let (_, revoked) = make(conn, now_ms() + 60_000, None);
            assert!(revoke(conn, "room_1", &revoked.id)?);

            let listed = list_for_room(conn, "room_1")?;
            let ids: Vec<&str> = listed.iter().map(|i| i.id.as_str()).collect();
            assert!(ids.contains(&live.id.as_str()));
            assert!(ids.contains(&expired.id.as_str()), "expired stays visible");
            assert!(!ids.contains(&revoked.id.as_str()), "revoked disappears");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn deleting_the_room_takes_its_invites_with_it() {
        let db = test_db();
        db.call_blocking(|conn| {
            seed_room(conn, "room_1");
            let (token, _) = make(conn, now_ms() + 60_000, None);
            crate::db::rooms::delete_room(conn, "room_1")?;
            assert!(find_by_hash(conn, &token_hash(&token))?.is_none());
            Ok(())
        })
        .unwrap();
    }
}
