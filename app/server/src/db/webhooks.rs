//! Incoming webhook credentials (docs/API.md §17).
//!
//! A webhook is a row: the token in it is the entire credential, and deleting
//! the row is the entire revocation. There is no session, no expiry and no
//! refresh — the post handler looks the token up on every request, so a
//! revoked webhook is refused on the very next POST.

use rusqlite::{params, Connection, OptionalExtension};

use super::models::Webhook;
use super::now_ms;
use crate::error::{ApiError, ApiResult};

/// Webhooks per room. A bound, not a budget: one feed per integration is the
/// pattern, and a room approaching this many robots has a different problem.
/// Mirrors the shape of the nine-admin cap — a small named limit beats an
/// unbounded list that only ever grows by accident.
pub const MAX_WEBHOOKS_PER_ROOM: usize = 20;

const SELECT_COLUMNS: &str = "id, room_id, name, token, created_by, created_at";

/// Create one webhook, and the `users` row its posts will be attributed to.
///
/// One transaction: a webhook whose sender has no profile row would render as
/// the `User 0x0000…` placeholder, which is exactly the anonymous look this
/// row exists to prevent. The upsert is safe because the derived address is
/// keyed on the webhook id, which is fresh per call.
pub fn create(
    conn: &mut Connection,
    id: &str,
    room_id: &str,
    name: &str,
    token: &str,
    created_by: &str,
) -> ApiResult<Webhook> {
    let tx = conn.transaction()?;
    let now = now_ms();

    tx.execute(
        "INSERT INTO room_webhooks (id, room_id, name, token, created_by, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, room_id, name, token, created_by, now],
    )?;

    // The display identity. The row outlives the webhook on purpose: history
    // keeps its attribution after a revoke, the same way a departed member's
    // old messages keep their name.
    let sender = pocketskynet_core::WalletAddress::webhook_sender(id);
    super::users::upsert_user(&tx, sender.as_str(), name, None, None)?;

    let webhook = get(&tx, room_id, id)?
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("webhook vanished after insert")))?;
    tx.commit()?;
    Ok(webhook)
}

pub fn get(conn: &Connection, room_id: &str, id: &str) -> ApiResult<Option<Webhook>> {
    let out = conn
        .query_row(
            &format!("SELECT {SELECT_COLUMNS} FROM room_webhooks WHERE room_id = ?1 AND id = ?2"),
            params![room_id, id],
            Webhook::from_row,
        )
        .optional()?;
    Ok(out)
}

/// The post path's lookup: the token is the whole credential, so this is the
/// authentication. `None` covers unknown and revoked alike — the two are the
/// same fact by the time a request arrives.
pub fn find_by_token(conn: &Connection, token: &str) -> ApiResult<Option<Webhook>> {
    let out = conn
        .query_row(
            &format!("SELECT {SELECT_COLUMNS} FROM room_webhooks WHERE token = ?1"),
            params![token],
            Webhook::from_row,
        )
        .optional()?;
    Ok(out)
}

/// A room's webhooks, newest first — the order the management list shows.
pub fn list_for_room(conn: &Connection, room_id: &str) -> ApiResult<Vec<Webhook>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SELECT_COLUMNS} FROM room_webhooks
         WHERE room_id = ?1
         ORDER BY created_at DESC, id DESC"
    ))?;
    let rows = stmt.query_map(params![room_id], Webhook::from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn count_for_room(conn: &Connection, room_id: &str) -> ApiResult<usize> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM room_webhooks WHERE room_id = ?1",
        params![room_id],
        |r| r.get(0),
    )?;
    Ok(n as usize)
}

/// Revoke. Returns whether anything was deleted, so the route can 404 on an
/// id that names nothing rather than claim a revocation it did not perform.
pub fn delete(conn: &Connection, room_id: &str, id: &str) -> ApiResult<bool> {
    let n = conn.execute(
        "DELETE FROM room_webhooks WHERE room_id = ?1 AND id = ?2",
        params![room_id, id],
    )?;
    Ok(n > 0)
}
