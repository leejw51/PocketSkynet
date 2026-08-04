//! The operator ladder — the game layer's only server-side state.
//!
//! Progression is computed and stored on the device. This table is a board,
//! not a referee: clients report where they have got to, and the server keeps
//! the best figure it has seen per wallet so it can rank them.
//!
//! `load` is stored as a running maximum, which buys two things for one line
//! of SQL. A device that reinstalls and reports zero cannot erase a standing,
//! and a client that reports a stale figure (an old phone opened after a new
//! one has moved ahead) cannot drag the board backwards.

use rusqlite::{params, Connection};
use serde::Serialize;

use crate::error::ApiResult;

/// One row of the board.
#[derive(Debug, Clone, Serialize)]
pub struct OperatorFile {
    #[serde(rename = "walletAddress")]
    pub wallet_address: String,
    /// Synthesized from the address when there is no `users` row, matching
    /// what messages and shouts do for the same case.
    pub username: String,
    pub load: i64,
    #[serde(rename = "rankLevel")]
    pub rank_level: i64,
    pub streak: i64,
    pub orders: i64,
    pub trophies: i64,
    #[serde(rename = "updatedAt")]
    pub updated_at: i64,
}

pub struct Report {
    pub wallet_address: String,
    pub load: i64,
    pub rank_level: i64,
    pub streak: i64,
    pub orders: i64,
    pub trophies: i64,
    pub now: i64,
}

/// Record a report, keeping the best load seen.
///
/// The companion fields follow whichever report won on load rather than being
/// maxed independently — a board row should describe one coherent moment, not
/// a personal best assembled from six different days.
pub fn record(conn: &Connection, report: Report) -> ApiResult<OperatorFile> {
    conn.execute(
        "INSERT INTO operator_files
             (wallet_address, load, rank_level, streak, orders, trophies, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(wallet_address) DO UPDATE SET
             rank_level = CASE WHEN excluded.load >= operator_files.load
                               THEN excluded.rank_level ELSE operator_files.rank_level END,
             streak     = CASE WHEN excluded.load >= operator_files.load
                               THEN excluded.streak ELSE operator_files.streak END,
             orders     = CASE WHEN excluded.load >= operator_files.load
                               THEN excluded.orders ELSE operator_files.orders END,
             trophies   = CASE WHEN excluded.load >= operator_files.load
                               THEN excluded.trophies ELSE operator_files.trophies END,
             updated_at = excluded.updated_at,
             -- Last, so the CASEs above still see the previous value.
             load       = MAX(operator_files.load, excluded.load)",
        params![
            report.wallet_address,
            report.load,
            report.rank_level,
            report.streak,
            report.orders,
            report.trophies,
            report.now
        ],
    )?;
    one(conn, &report.wallet_address)
}

pub fn one(conn: &Connection, address: &str) -> ApiResult<OperatorFile> {
    let file = conn.query_row(
        "SELECT o.wallet_address, u.username, o.load, o.rank_level,
                o.streak, o.orders, o.trophies, o.updated_at
         FROM operator_files o LEFT JOIN users u ON u.wallet_address = o.wallet_address
         WHERE o.wallet_address = ?1",
        params![address],
        row_to_file,
    )?;
    Ok(named(file))
}

/// The board, strongest first.
pub fn board(conn: &Connection, limit: i64) -> ApiResult<Vec<OperatorFile>> {
    let mut stmt = conn.prepare(
        "SELECT o.wallet_address, u.username, o.load, o.rank_level,
                o.streak, o.orders, o.trophies, o.updated_at
         FROM operator_files o LEFT JOIN users u ON u.wallet_address = o.wallet_address
         ORDER BY o.load DESC, o.updated_at ASC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], row_to_file)?;
    let mut files = Vec::new();
    for file in rows {
        files.push(named(file?));
    }
    Ok(files)
}

fn row_to_file(r: &rusqlite::Row<'_>) -> rusqlite::Result<OperatorFile> {
    Ok(OperatorFile {
        wallet_address: r.get(0)?,
        username: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
        load: r.get(2)?,
        rank_level: r.get(3)?,
        streak: r.get(4)?,
        orders: r.get(5)?,
        trophies: r.get(6)?,
        updated_at: r.get(7)?,
    })
}

/// Fill in a display name for a wallet that has never set one.
fn named(mut file: OperatorFile) -> OperatorFile {
    if file.username.is_empty() {
        let addr = &file.wallet_address;
        let tail = addr.get(addr.len().saturating_sub(4)..).unwrap_or(addr);
        file.username = format!("operator-{tail}");
    }
    file
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    fn report(address: &str, load: i64, level: i64) -> Report {
        Report {
            wallet_address: address.to_owned(),
            load,
            rank_level: level,
            streak: 1,
            orders: 0,
            trophies: 0,
            now: 1_000,
        }
    }

    #[test]
    fn load_only_ever_climbs() {
        let db = test_db();
        db.call_blocking(|conn| {
            record(conn, report("0xa", 500, 4))?;
            // A reinstall reporting nothing must not erase the standing.
            let after = record(conn, report("0xa", 0, 1))?;
            assert_eq!(after.load, 500);
            // And the companion fields stayed with the winning report rather
            // than being dragged down to the stale one.
            assert_eq!(after.rank_level, 4);

            let climbed = record(conn, report("0xa", 900, 6))?;
            assert_eq!(climbed.load, 900);
            assert_eq!(climbed.rank_level, 6);
            Ok::<_, crate::error::ApiError>(())
        })
        .expect("record");
    }

    #[test]
    fn board_is_strongest_first_and_names_the_nameless() {
        let db = test_db();
        db.call_blocking(|conn| {
            record(conn, report("0xaaaabbbb", 100, 2))?;
            record(conn, report("0xccccdddd", 900, 6))?;
            record(conn, report("0xeeeeffff", 400, 4))?;
            let board = board(conn, 10)?;
            assert_eq!(
                board.iter().map(|f| f.load).collect::<Vec<_>>(),
                vec![900, 400, 100]
            );
            // No `users` row anywhere, so every name is synthesized.
            assert!(board.iter().all(|f| f.username.starts_with("operator-")));
            Ok::<_, crate::error::ApiError>(())
        })
        .expect("board");
    }

    #[test]
    fn limit_is_honoured() {
        let db = test_db();
        db.call_blocking(|conn| {
            for i in 0..12 {
                record(conn, report(&format!("0x{i:040x}"), i * 10, 1))?;
            }
            assert_eq!(board(conn, 5)?.len(), 5);
            Ok::<_, crate::error::ApiError>(())
        })
        .expect("limit");
    }
}
