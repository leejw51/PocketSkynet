//! Skynet Password storage (`docs/API.md` §18).
//!
//! Six opaque strings and two timestamps per row. Nothing in this module can
//! read an entry, and that is not an accident of the current implementation —
//! the key that opens one is derived in the browser from a wallet signature and
//! is never sent here (`core/src/secrets.rs`).
//!
//! # Every function takes an owner, and every statement uses it
//!
//! The scoping is in the `WHERE` clause of each statement rather than in a
//! separate "does this belong to you" read followed by an unscoped write. Two
//! reasons, and the second is the important one:
//!
//! 1. It is one round trip instead of two, with no window between the check and
//!    the act.
//! 2. A check that lives in a different statement from the mutation is a check
//!    somebody can forget to call. Here, forgetting the owner is a compile
//!    error — the parameter is not optional.
//!
//! The consequence is that "somebody else's row" and "no such row" are the same
//! answer: `false`, which the route turns into a 404. That is deliberate; see
//! `routes/passwords.rs`.

use pocketskynet_core::secrets::SealedField;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::error::ApiResult;

/// One stored entry, as the API serialises it.
///
/// The field names are the wire names. `key` and `value` are objects rather
/// than six flat members so the client can hand each one to
/// `secrets::open_field` without re-assembling it, and so it is obvious at a
/// glance that both halves carry their own IV and MAC.
///
/// [`SealedField`] is `core`'s own type, not a look-alike. An earlier version
/// redeclared it here on the theory that the server "must not be able to
/// construct a core sealed field" — but `core`'s fields are `pub`, so that
/// barrier never existed, and the copy was pure drift risk: two structs the
/// wire has to keep byte-identical. Reusing the one type is what makes them so
/// by construction. The server still cannot *open* one; that needs the vault
/// key, which never arrives here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PasswordEntry {
    pub id: String,
    pub key: SealedField,
    pub value: SealedField,
    #[serde(rename = "encVer")]
    pub enc_ver: i64,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(rename = "updatedAt")]
    pub updated_at: i64,
}

/// What a create or replace writes.
pub struct NewEntry {
    pub id: String,
    pub owner_address: String,
    pub key: SealedField,
    pub value: SealedField,
    pub enc_ver: i64,
}

/// One owner's entries, most recently changed first.
///
/// Not paginated. A password store is a list somebody scrolls, the client has
/// to decrypt every row to filter it at all (the key is ciphertext too), and a
/// cursor over an order the client cannot reproduce would be a worse experience
/// than one request. `limit` bounds the blast radius of a runaway account.
pub fn list(conn: &Connection, owner: &str, limit: usize) -> ApiResult<Vec<PasswordEntry>> {
    let mut stmt = conn.prepare(
        "SELECT id, key_ciphertext, key_iv, key_hmac,
                value_ciphertext, value_iv, value_hmac,
                enc_ver, created_at, updated_at
         FROM password_entries
         WHERE owner_address = ?1
         ORDER BY updated_at DESC, id ASC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![owner, limit as i64], row_to_entry)?;
    let mut out = Vec::new();
    for entry in rows {
        out.push(entry?);
    }
    Ok(out)
}

/// One entry, but only if this owner has it.
pub fn get(conn: &Connection, owner: &str, id: &str) -> ApiResult<Option<PasswordEntry>> {
    let entry = conn
        .query_row(
            "SELECT id, key_ciphertext, key_iv, key_hmac,
                    value_ciphertext, value_iv, value_hmac,
                    enc_ver, created_at, updated_at
             FROM password_entries
             WHERE owner_address = ?1 AND id = ?2",
            params![owner, id],
            row_to_entry,
        )
        .optional()?;
    Ok(entry)
}

/// How many entries this owner has. Used to enforce the per-account cap.
pub fn count(conn: &Connection, owner: &str) -> ApiResult<i64> {
    let n = conn.query_row(
        "SELECT COUNT(*) FROM password_entries WHERE owner_address = ?1",
        params![owner],
        |r| r.get(0),
    )?;
    Ok(n)
}

/// Insert a new entry. `None` when the id is already taken — by anybody.
///
/// The id is a PRIMARY KEY over the whole table, not per owner, so a collision
/// with a *different* account's row is refused too. That leaks one bit ("this
/// 128-bit random id is in use"), which is worth strictly less than the
/// alternative: an id space partitioned by owner would let one account claim an
/// id another account is already using, and the two rows would then be
/// distinguishable only by a column every query has to remember to compare.
///
/// `ON CONFLICT DO NOTHING` rather than a read-then-insert: the conflict is
/// decided by SQLite inside the one statement, so two concurrent creates of the
/// same id cannot both pass a check and then have one of them surface as a
/// constraint violation dressed up as a 500. It is emphatically **not** an
/// upsert — a client retrying a create that actually succeeded must not
/// overwrite an edit made in between.
pub fn create(conn: &Connection, new: &NewEntry, now: i64) -> ApiResult<Option<PasswordEntry>> {
    let inserted = conn.execute(
        "INSERT INTO password_entries
             (id, owner_address, key_ciphertext, key_iv, key_hmac,
              value_ciphertext, value_iv, value_hmac, enc_ver, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)
         ON CONFLICT (id) DO NOTHING",
        params![
            new.id,
            new.owner_address,
            new.key.ciphertext,
            new.key.iv,
            new.key.hmac,
            new.value.ciphertext,
            new.value.iv,
            new.value.hmac,
            new.enc_ver,
            now,
        ],
    )?;
    if inserted == 0 {
        return Ok(None);
    }
    Ok(Some(PasswordEntry {
        id: new.id.clone(),
        key: new.key.clone(),
        value: new.value.clone(),
        enc_ver: new.enc_ver,
        created_at: now,
        updated_at: now,
    }))
}

/// What [`create_capped`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum CreateOutcome {
    Created(PasswordEntry),
    /// This owner already has `max_entries` rows.
    CapReached,
    /// The id was already taken — by this owner or another.
    IdTaken,
}

/// Check the per-owner cap and insert, atomically.
///
/// The count and the insert run inside one `IMMEDIATE` transaction.
/// `IMMEDIATE` takes SQLite's write lock at `BEGIN`, before the count runs, so
/// a second connection racing the same owner blocks — and retries, via
/// `busy_timeout` — until this transaction commits or rolls back, rather than
/// reading a count from its own snapshot that this transaction's insert has
/// not landed in yet. Without that, two concurrent creates near the cap could
/// both read the same count on separate pooled connections and both pass it.
pub fn create_capped(
    conn: &mut Connection,
    new: &NewEntry,
    now: i64,
    max_entries: i64,
) -> ApiResult<CreateOutcome> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if count(&tx, &new.owner_address)? >= max_entries {
        return Ok(CreateOutcome::CapReached);
    }
    let outcome = match create(&tx, new, now)? {
        Some(entry) => CreateOutcome::Created(entry),
        None => CreateOutcome::IdTaken,
    };
    if matches!(outcome, CreateOutcome::Created(_)) {
        tx.commit()?;
    }
    Ok(outcome)
}

/// Replace both fields of an existing entry.
///
/// Both halves are overwritten together even when only one changed. An edit
/// re-seals with a fresh IV either way, so a partial update would save nothing
/// and would let the two fields drift into different `enc_ver`s.
///
/// The old ciphertext is *replaced*, not versioned: there is no history table,
/// because a password store that quietly kept every value you ever rotated away
/// from would be a liability dressed as a feature.
///
/// Returns `None` when no row of this owner has that id — which covers both
/// "there is no such entry" and "it is somebody else's".
pub fn replace(
    conn: &Connection,
    owner: &str,
    id: &str,
    key: &SealedField,
    value: &SealedField,
    enc_ver: i64,
    now: i64,
) -> ApiResult<Option<PasswordEntry>> {
    // `RETURNING created_at` in the same statement, the way `db/users.rs` and
    // `db/rooms.rs` return their upserted rows: the update *is* the read, so
    // there is no second SELECT and no window in which a concurrent write could
    // slip between the two. `created_at` is the only column the update does not
    // already have in hand — everything else it just wrote.
    let created_at: Option<i64> = conn
        .query_row(
            "UPDATE password_entries
                SET key_ciphertext = ?3, key_iv = ?4, key_hmac = ?5,
                    value_ciphertext = ?6, value_iv = ?7, value_hmac = ?8,
                    enc_ver = ?9, updated_at = ?10
              WHERE owner_address = ?1 AND id = ?2
              RETURNING created_at",
            params![
                owner,
                id,
                key.ciphertext,
                key.iv,
                key.hmac,
                value.ciphertext,
                value.iv,
                value.hmac,
                enc_ver,
                now,
            ],
            |r| r.get(0),
        )
        .optional()?;

    Ok(created_at.map(|created_at| PasswordEntry {
        id: id.to_owned(),
        key: key.clone(),
        value: value.clone(),
        enc_ver,
        created_at,
        updated_at: now,
    }))
}

/// Delete one entry. `false` when this owner has no such row.
pub fn delete(conn: &Connection, owner: &str, id: &str) -> ApiResult<bool> {
    let changed = conn.execute(
        "DELETE FROM password_entries WHERE owner_address = ?1 AND id = ?2",
        params![owner, id],
    )?;
    Ok(changed > 0)
}

fn row_to_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<PasswordEntry> {
    Ok(PasswordEntry {
        id: r.get(0)?,
        key: SealedField {
            ciphertext: r.get(1)?,
            iv: r.get(2)?,
            hmac: r.get(3)?,
        },
        value: SealedField {
            ciphertext: r.get(4)?,
            iv: r.get(5)?,
            hmac: r.get(6)?,
        },
        enc_ver: r.get(7)?,
        created_at: r.get(8)?,
        updated_at: r.get(9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    const ALICE: &str = "0x1111111111111111111111111111111111111111";
    const BOB: &str = "0x2222222222222222222222222222222222222222";

    fn sealed(tag: &str) -> SealedField {
        SealedField {
            ciphertext: format!("Y2lwaGVydGV4dA=={tag}"),
            iv: "0".repeat(32),
            hmac: "a".repeat(64),
        }
    }

    fn entry(id: &str, owner: &str) -> NewEntry {
        NewEntry {
            id: id.to_owned(),
            owner_address: owner.to_owned(),
            key: sealed("k"),
            value: sealed("v"),
            enc_ver: 1,
        }
    }

    #[test]
    fn an_entry_round_trips_and_carries_both_sealed_halves() {
        let db = test_db();
        db.call_blocking(|conn| {
            let made = create(conn, &entry("sec_aaaaaaaaaa", ALICE), 1_000)?.expect("inserted");
            assert_eq!(made.created_at, 1_000);
            assert_eq!(made.updated_at, 1_000, "a fresh row is its own last edit");

            let read = get(conn, ALICE, "sec_aaaaaaaaaa")?.expect("stored");
            assert_eq!(read, made);
            assert_eq!(read.key.ciphertext, "Y2lwaGVydGV4dA==k");
            assert_eq!(read.value.ciphertext, "Y2lwaGVydGV4dA==v");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn the_list_is_scoped_to_its_owner_and_ordered_by_last_change() {
        let db = test_db();
        db.call_blocking(|conn| {
            create(conn, &entry("sec_aaaaaaaaaa", ALICE), 1_000)?;
            create(conn, &entry("sec_bbbbbbbbbb", ALICE), 3_000)?;
            create(conn, &entry("sec_cccccccccc", BOB), 2_000)?;

            let hers = list(conn, ALICE, 50)?;
            assert_eq!(
                hers.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
                vec!["sec_bbbbbbbbbb", "sec_aaaaaaaaaa"]
            );
            assert_eq!(count(conn, ALICE)?, 2);

            let his = list(conn, BOB, 50)?;
            assert_eq!(his.len(), 1, "Bob sees only his own");
            assert_eq!(his[0].id, "sec_cccccccccc");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn another_wallet_cannot_read_edit_or_delete_by_guessing_the_id() {
        // The whole authorisation model, at the layer that enforces it. The id
        // is known exactly — this is not a search, it is a targeted attempt.
        let db = test_db();
        db.call_blocking(|conn| {
            create(conn, &entry("sec_aaaaaaaaaa", ALICE), 1_000)?;

            assert!(get(conn, BOB, "sec_aaaaaaaaaa")?.is_none());
            assert!(replace(
                conn,
                BOB,
                "sec_aaaaaaaaaa",
                &sealed("evil-k"),
                &sealed("evil-v"),
                1,
                2_000
            )?
            .is_none());
            assert!(!delete(conn, BOB, "sec_aaaaaaaaaa")?);

            // And Alice's row is untouched by any of it.
            let still = get(conn, ALICE, "sec_aaaaaaaaaa")?.expect("still there");
            assert_eq!(still.key.ciphertext, "Y2lwaGVydGV4dA==k");
            assert_eq!(still.updated_at, 1_000);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn an_edit_replaces_both_halves_and_moves_the_timestamp() {
        let db = test_db();
        db.call_blocking(|conn| {
            create(conn, &entry("sec_aaaaaaaaaa", ALICE), 1_000)?;

            let new_key = SealedField {
                ciphertext: "bmV3LWtleQ==".into(),
                iv: "1".repeat(32),
                hmac: "b".repeat(64),
            };
            let new_value = SealedField {
                ciphertext: "bmV3LXZhbHVl".into(),
                iv: "2".repeat(32),
                hmac: "c".repeat(64),
            };
            let updated = replace(
                conn,
                ALICE,
                "sec_aaaaaaaaaa",
                &new_key,
                &new_value,
                1,
                5_000,
            )?
            .expect("updated");

            assert_eq!(updated.key, new_key);
            assert_eq!(updated.value, new_value);
            assert_eq!(updated.updated_at, 5_000);
            assert_eq!(updated.created_at, 1_000, "creation time is not rewritten");

            // Nothing of the previous ciphertext survives anywhere in the row.
            let raw: String = conn.query_row(
                "SELECT key_ciphertext || value_ciphertext || key_iv || value_iv
                 FROM password_entries WHERE id = 'sec_aaaaaaaaaa'",
                [],
                |r| r.get(0),
            )?;
            assert!(!raw.contains("Y2lwaGVydGV4dA=="), "the old value lingered");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn deleting_twice_reports_not_found_the_second_time() {
        let db = test_db();
        db.call_blocking(|conn| {
            create(conn, &entry("sec_aaaaaaaaaa", ALICE), 1_000)?;
            assert!(delete(conn, ALICE, "sec_aaaaaaaaaa")?);
            assert!(!delete(conn, ALICE, "sec_aaaaaaaaaa")?);
            assert_eq!(count(conn, ALICE)?, 0);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_duplicate_id_is_refused_rather_than_overwriting() {
        // Create must never be a silent upsert: a client that retried a create
        // it had already made would otherwise destroy the entry it edited in
        // between.
        let db = test_db();
        db.call_blocking(|conn| {
            assert!(create(conn, &entry("sec_aaaaaaaaaa", ALICE), 1_000)?.is_some());
            assert!(create(conn, &entry("sec_aaaaaaaaaa", ALICE), 2_000)?.is_none());
            // Including across accounts.
            assert!(create(conn, &entry("sec_aaaaaaaaaa", BOB), 2_000)?.is_none());
            // …and the original row is untouched by the refused insert.
            let still = get(conn, ALICE, "sec_aaaaaaaaaa")?.expect("still there");
            assert_eq!(still.created_at, 1_000);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn the_list_limit_is_honoured() {
        let db = test_db();
        db.call_blocking(|conn| {
            for i in 0..10 {
                create(conn, &entry(&format!("sec_{i:07}xxx"), ALICE), 1_000 + i)?;
            }
            assert_eq!(list(conn, ALICE, 4)?.len(), 4);
            assert_eq!(list(conn, ALICE, 100)?.len(), 10);
            Ok(())
        })
        .unwrap();
    }
}
