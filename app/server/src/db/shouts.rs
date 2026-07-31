//! Paid broadcasts (docs/API.md §16.1).
//!
//! A shout is not a message. It belongs to no room, is never encrypted, and
//! expires within a minute of being created — the row that outlives it is the
//! operator's revenue ledger, not content anyone re-reads. The active set is
//! always a `WHERE expires_at > now` read; nothing ever "closes" a shout
//! server-side, because dismissal is a per-viewer act that lives entirely in
//! the client.

use rusqlite::{params, Connection};
use serde::Serialize;

use crate::error::ApiResult;

/// The wire form of a shout, username joined in so the banner can name the
/// crier without a second request.
#[derive(Debug, Clone, Serialize)]
pub struct Shout {
    pub id: String,
    #[serde(rename = "senderAddress")]
    pub sender_address: String,
    /// Synthesized from the address when the sender has no `users` row, the
    /// same fallback shape messages use.
    pub username: String,
    pub text: String,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(rename = "expiresAt")]
    pub expires_at: i64,
    #[serde(rename = "txHash")]
    pub tx_hash: String,
    #[serde(rename = "amountWei")]
    pub amount_wei: String,
}

pub struct NewShout {
    pub id: String,
    pub sender_address: String,
    pub text: String,
    pub tx_hash: String,
    pub amount_wei: String,
    pub created_at: i64,
    pub expires_at: i64,
}

pub fn create(conn: &Connection, new: NewShout) -> ApiResult<Shout> {
    conn.execute(
        "INSERT INTO shouts (id, sender_address, text, tx_hash, amount_wei, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            new.id,
            new.sender_address,
            new.text,
            new.tx_hash,
            new.amount_wei,
            new.created_at,
            new.expires_at
        ],
    )?;
    let username = username_of(conn, &new.sender_address)?;
    Ok(Shout {
        id: new.id,
        sender_address: new.sender_address,
        username,
        text: new.text,
        created_at: new.created_at,
        expires_at: new.expires_at,
        tx_hash: new.tx_hash,
        amount_wei: new.amount_wei,
    })
}

/// Every shout still burning, newest first.
pub fn active(conn: &Connection, now: i64) -> ApiResult<Vec<Shout>> {
    let mut stmt = conn.prepare(
        "SELECT s.id, s.sender_address, u.username, s.text,
                s.created_at, s.expires_at, s.tx_hash, s.amount_wei
         FROM shouts s LEFT JOIN users u ON u.wallet_address = s.sender_address
         WHERE s.expires_at > ?1
         ORDER BY s.created_at DESC",
    )?;
    let rows = stmt.query_map(params![now], |r| {
        Ok(Shout {
            id: r.get(0)?,
            sender_address: r.get(1)?,
            username: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            text: r.get(3)?,
            created_at: r.get(4)?,
            expires_at: r.get(5)?,
            tx_hash: r.get(6)?,
            amount_wei: r.get(7)?,
        })
    })?;
    let mut shouts = Vec::new();
    for shout in rows {
        let mut shout = shout?;
        if shout.username.is_empty() {
            shout.username = synthesized_username(&shout.sender_address);
        }
        shouts.push(shout);
    }
    Ok(shouts)
}

/// How many shouts one wallet currently has burning. The route caps this so a
/// whale cannot wallpaper every screen with one funding round.
pub fn active_count_for(conn: &Connection, sender: &str, now: i64) -> ApiResult<i64> {
    let n = conn.query_row(
        "SELECT COUNT(*) FROM shouts WHERE sender_address = ?1 AND expires_at > ?2",
        params![sender, now],
        |r| r.get(0),
    )?;
    Ok(n)
}

fn username_of(conn: &Connection, address: &str) -> ApiResult<String> {
    let name: Option<String> = rusqlite::OptionalExtension::optional(conn.query_row(
        "SELECT username FROM users WHERE wallet_address = ?1",
        params![address],
        |r| r.get(0),
    ))?;
    Ok(name.unwrap_or_else(|| synthesized_username(address)))
}

/// `User 0xabcd...ef01` — the same synthesized fallback messages use for a
/// sender with no profile row.
fn synthesized_username(address: &str) -> String {
    if address.len() >= 10 {
        format!("User {}...{}", &address[..6], &address[address.len() - 4..])
    } else {
        format!("User {address}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    fn shout(id: &str, sender: &str, created: i64, ttl: i64) -> NewShout {
        NewShout {
            id: id.to_owned(),
            sender_address: sender.to_owned(),
            text: "the machines rise".to_owned(),
            tx_hash: format!("0x{}", id.repeat(64 / id.len().max(1)))
                .chars()
                .take(66)
                .collect(),
            amount_wei: "10000000000000000000".to_owned(),
            created_at: created,
            expires_at: created + ttl,
        }
    }

    #[test]
    fn only_unexpired_shouts_are_active_and_newest_leads() {
        let db = test_db();
        db.call_blocking(|conn| {
            let sender = "0x1111111111111111111111111111111111111111";
            create(conn, shout("a", sender, 1_000, 60_000))?;
            create(conn, shout("b", sender, 2_000, 60_000))?;
            create(conn, shout("c", sender, 100, 10))?; // long dead

            let active = active(conn, 5_000)?;
            assert_eq!(
                active.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
                vec!["b", "a"]
            );

            // Without a users row the banner still names someone.
            assert_eq!(active[0].username, "User 0x1111...1111");

            assert_eq!(active_count_for(conn, sender, 5_000)?, 2);
            assert_eq!(active_count_for(conn, sender, 100_000)?, 0);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_known_user_shouts_under_their_username() {
        let db = test_db();
        db.call_blocking(|conn| {
            let sender = "0x2222222222222222222222222222222222222222";
            crate::db::users::upsert_user(conn, sender, "sarah", None, None)?;
            let created = create(conn, shout("x", sender, 1_000, 60_000))?;
            assert_eq!(created.username, "sarah");
            assert_eq!(active(conn, 2_000)?[0].username, "sarah");
            Ok(())
        })
        .unwrap();
    }
}
