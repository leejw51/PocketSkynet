//! Published sites (docs/API.md §16.2).
//!
//! The row is metadata and provenance; the bytes live under
//! `data/sites/{id}/` and the searchable text lives in `search_docs` under
//! kind `site`. Deleting a site removes all three — unlike attachments, the
//! bytes are *not* kept, because hosting is the whole product here and a
//! deleted site must actually stop being served.

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::error::ApiResult;

#[derive(Debug, Clone, Serialize)]
pub struct Site {
    pub id: String,
    #[serde(rename = "ownerAddress")]
    pub owner_address: String,
    /// Owner's username, joined for display; synthesized when absent.
    pub username: String,
    pub title: String,
    #[serde(rename = "txHash")]
    pub tx_hash: String,
    #[serde(rename = "amountWei")]
    pub amount_wei: String,
    #[serde(rename = "sizeBytes")]
    pub size_bytes: i64,
    #[serde(rename = "fileCount")]
    pub file_count: i64,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    /// Where it is served: `/sites/{id}/`.
    pub url: String,
}

pub struct NewSite {
    pub id: String,
    pub owner_address: String,
    pub title: String,
    pub tx_hash: String,
    pub amount_wei: String,
    pub size_bytes: i64,
    pub file_count: i64,
    pub created_at: i64,
}

fn site_url(id: &str) -> String {
    format!("/sites/{id}/")
}

fn synthesized_username(address: &str) -> String {
    if address.len() >= 10 {
        format!("User {}...{}", &address[..6], &address[address.len() - 4..])
    } else {
        format!("User {address}")
    }
}

pub fn create(conn: &Connection, new: NewSite) -> ApiResult<Site> {
    conn.execute(
        "INSERT INTO published_sites
             (id, owner_address, title, tx_hash, amount_wei, size_bytes, file_count, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            new.id,
            new.owner_address,
            new.title,
            new.tx_hash,
            new.amount_wei,
            new.size_bytes,
            new.file_count,
            new.created_at
        ],
    )?;
    let username: Option<String> = conn
        .query_row(
            "SELECT username FROM users WHERE wallet_address = ?1",
            params![new.owner_address],
            |r| r.get(0),
        )
        .optional()?;
    Ok(Site {
        url: site_url(&new.id),
        username: username.unwrap_or_else(|| synthesized_username(&new.owner_address)),
        id: new.id,
        owner_address: new.owner_address,
        title: new.title,
        tx_hash: new.tx_hash,
        amount_wei: new.amount_wei,
        size_bytes: new.size_bytes,
        file_count: new.file_count,
        created_at: new.created_at,
    })
}

pub fn read(conn: &Connection, id: &str) -> ApiResult<Option<Site>> {
    let site = conn
        .query_row(
            "SELECT s.id, s.owner_address, u.username, s.title, s.tx_hash, s.amount_wei,
                    s.size_bytes, s.file_count, s.created_at
             FROM published_sites s
             LEFT JOIN users u ON u.wallet_address = s.owner_address
             WHERE s.id = ?1",
            params![id],
            row_to_site,
        )
        .optional()?;
    Ok(site)
}

/// Newest first.
pub fn list(conn: &Connection, limit: usize) -> ApiResult<Vec<Site>> {
    let mut stmt = conn.prepare(
        "SELECT s.id, s.owner_address, u.username, s.title, s.tx_hash, s.amount_wei,
                s.size_bytes, s.file_count, s.created_at
         FROM published_sites s
         LEFT JOIN users u ON u.wallet_address = s.owner_address
         ORDER BY s.created_at DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], row_to_site)?;
    let mut sites = Vec::new();
    for site in rows {
        sites.push(site?);
    }
    Ok(sites)
}

pub fn count(conn: &Connection) -> ApiResult<i64> {
    let n = conn.query_row("SELECT COUNT(*) FROM published_sites", [], |r| r.get(0))?;
    Ok(n)
}

/// Remove the row and its search document. The caller removes the directory —
/// filesystem work does not belong inside a database transaction.
pub fn delete(conn: &Connection, id: &str) -> ApiResult<bool> {
    let changed = conn.execute("DELETE FROM published_sites WHERE id = ?1", params![id])?;
    crate::search::store::unindex(conn, crate::search::store::KIND_SITE, id)?;
    Ok(changed > 0)
}

fn row_to_site(r: &rusqlite::Row<'_>) -> rusqlite::Result<Site> {
    let id: String = r.get(0)?;
    let owner: String = r.get(1)?;
    let username: Option<String> = r.get(2)?;
    Ok(Site {
        url: site_url(&id),
        username: username.unwrap_or_else(|| synthesized_username(&owner)),
        id,
        owner_address: owner,
        title: r.get(3)?,
        tx_hash: r.get(4)?,
        amount_wei: r.get(5)?,
        size_bytes: r.get(6)?,
        file_count: r.get(7)?,
        created_at: r.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    fn site(id: &str, owner: &str, created: i64) -> NewSite {
        NewSite {
            id: id.to_owned(),
            owner_address: owner.to_owned(),
            title: format!("Site {id}"),
            tx_hash: format!("0x{}", "c".repeat(64)),
            amount_wei: "1000000000000000000".to_owned(),
            size_bytes: 1024,
            file_count: 3,
            created_at: created,
        }
    }

    #[test]
    fn sites_round_trip_newest_first_with_urls() {
        let db = test_db();
        db.call_blocking(|conn| {
            let owner = "0x3333333333333333333333333333333333333333";
            create(conn, site("aaa", owner, 1_000))?;
            create(conn, site("bbb", owner, 2_000))?;

            let all = list(conn, 50)?;
            assert_eq!(
                all.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
                vec!["bbb", "aaa"]
            );
            assert_eq!(all[0].url, "/sites/bbb/");
            assert_eq!(all[0].username, "User 0x3333...3333");
            assert_eq!(count(conn)?, 2);

            assert!(read(conn, "aaa")?.is_some());
            assert!(delete(conn, "aaa")?);
            assert!(read(conn, "aaa")?.is_none());
            assert!(!delete(conn, "aaa")?, "double delete reports not-found");
            assert_eq!(count(conn)?, 1);
            Ok(())
        })
        .unwrap();
    }
}
