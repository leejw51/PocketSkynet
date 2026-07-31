//! The persisted room cache: what makes opening a room cost zero requests.
//!
//! # What is cached, and the line that is never crossed
//!
//! Three kinds of entry, all in `localStorage` via [`crate::session::backend`]:
//!
//! | Key | Contents |
//! |---|---|
//! | `ps-cache:rooms` | The room list, as the server sent it. |
//! | `ps-cache:msgs:<roomId>` | The newest message rows **as received** — ciphertext, IVs, HMACs — plus the reaction table and the pagination flag. |
//! | `ps-cache:wraps:<roomId>` | The room's *wrapped* epoch keys, exactly as `GET /keys/versions` returned them. |
//! | `ps-cache:index` | The room ids that have entries, so sign-out can remove them without scanning. |
//!
//! Nothing here weakens the persistence policy in [`crate::session`]: every
//! cached byte is something the server already stores and already sends to
//! this account on request. Message rows are cached in their wire form —
//! encrypted content stays encrypted — and the key wraps are ECDH-wrapped to
//! this account's public key, which is precisely how they rest in the server's
//! database. Plaintext and unwrapped keys remain memory-only. The reference
//! client caches *decrypted* rows in IndexedDB; this cache exists to match its
//! speed without copying that decision.
//!
//! # Why this is the fast path
//!
//! Reopening a room used to be three round trips (keys, history, sync) before
//! anything painted. With a warm cache the keys unwrap from storage and the
//! history hydrates from storage — both synchronous — and only the `/sync`
//! delta touches the network, after the room is already on screen. A room you
//! have seen paints in one frame, offline included.
//!
//! The cache is written through on every fold (see `state.rs`): history pages,
//! sync events, confirmed sends and reaction changes all land here as a
//! side-effect of the reducer, the same way the sync cursor always has.

use pocketskynet_core::RoomId;
use serde::{Deserialize, Serialize};

use crate::api::{Message, RoomKey, RoomWithMembers};
use crate::session::backend;

const KEY_ROOMS: &str = "ps-cache:rooms";
const KEY_INDEX: &str = "ps-cache:index";
const PREFIX_MSGS: &str = "ps-cache:msgs:";
const PREFIX_WRAPS: &str = "ps-cache:wraps:";

/// Rows kept per room. Two hundred is four history pages — enough that the
/// scrollback a returning user actually looks at is instant, small enough that
/// a hundred cached rooms stay well inside a `localStorage` quota.
pub const MAX_ROWS: usize = 200;

/// One room's persisted stream: the newest rows in wire form, the reaction
/// table for those rows, and whether older pages exist behind them.
///
/// Reactions are folded state rather than wire rows (the server event-sources
/// them), so they are persisted as the folded table; losing them would silently
/// blank every chip on reload because `/sync` never re-sends old events.
/// Serialised as pair-vectors, not maps: `serde_json` requires string keys on
/// maps and both key types here are newtypes.
/// One message's reactions: emoticon → the addresses that picked it.
pub type CachedReactions = Vec<(String, Vec<pocketskynet_core::WalletAddress>)>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedRoom {
    pub rows: Vec<Message>,
    pub reactions: Vec<(pocketskynet_core::MessageId, CachedReactions)>,
    pub has_more_history: bool,
}

/// The newest `MAX_ROWS` rows, oldest first. Pure so the cap is testable on
/// the host target.
pub fn cap_rows(mut rows: Vec<Message>) -> Vec<Message> {
    // Newest first for the cut...
    rows.sort_by(|a, b| {
        (b.message_timestamp, b.msg_serial).cmp(&(a.message_timestamp, a.msg_serial))
    });
    rows.truncate(MAX_ROWS);
    // ...then back to stream order for the reader.
    rows.reverse();
    rows
}

pub fn load_rooms() -> Option<Vec<RoomWithMembers>> {
    backend::get(KEY_ROOMS)
}

pub fn save_rooms(rooms: &[RoomWithMembers]) {
    backend::set(KEY_ROOMS, &rooms);
}

pub fn load_room(room_id: &RoomId) -> Option<CachedRoom> {
    backend::get(&format!("{PREFIX_MSGS}{}", room_id.as_str()))
}

pub fn save_room(room_id: &RoomId, cached: &CachedRoom) {
    backend::set(&format!("{PREFIX_MSGS}{}", room_id.as_str()), cached);
    index_add(room_id);
}

pub fn load_wraps(room_id: &RoomId) -> Option<Vec<RoomKey>> {
    backend::get(&format!("{PREFIX_WRAPS}{}", room_id.as_str()))
}

pub fn save_wraps(room_id: &RoomId, wraps: &[RoomKey]) {
    backend::set(&format!("{PREFIX_WRAPS}{}", room_id.as_str()), &wraps);
    index_add(room_id);
}

/// Drop one room's cached stream and wraps — the user asked for a fresh copy.
pub fn forget_room(room_id: &RoomId) {
    backend::delete(&format!("{PREFIX_MSGS}{}", room_id.as_str()));
    backend::delete(&format!("{PREFIX_WRAPS}{}", room_id.as_str()));
}

/// Remove every cache entry. Called on sign-out: a deliberate sign-out means
/// the next person at this browser must not find room names, membership or
/// even ciphertext lying around. (Erase local data clears all of
/// `localStorage` and does not need this.)
pub fn clear_all() {
    if let Some(ids) = backend::get::<Vec<String>>(KEY_INDEX) {
        for id in ids {
            backend::delete(&format!("{PREFIX_MSGS}{id}"));
            backend::delete(&format!("{PREFIX_WRAPS}{id}"));
        }
    }
    backend::delete(KEY_INDEX);
    backend::delete(KEY_ROOMS);
}

fn index_add(room_id: &RoomId) {
    let mut ids: Vec<String> = backend::get(KEY_INDEX).unwrap_or_default();
    if !ids.iter().any(|i| i == room_id.as_str()) {
        ids.push(room_id.as_str().to_owned());
        backend::set(KEY_INDEX, &ids);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pocketskynet_core::{MessageId, WalletAddress};

    fn msg(serial: i64, ts: i64) -> Message {
        Message {
            id: MessageId::new(&format!("msg_test_{serial:04}")).unwrap(),
            room_id: RoomId::new("room_test_1").unwrap(),
            sender_address: WalletAddress::new("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266")
                .unwrap(),
            content: "sealed-bytes".into(),
            msg_hash: "abcd1234".into(),
            message_timestamp: ts,
            msg_type: "add".into(),
            msg_serial: serial,
            is_deleted: false,
            edited_at: None,
            created_at: None,
            is_encrypted: true,
            iv: Some("aXY=".into()),
            hmac: Some("aG1hYw==".into()),
            enc_ver: Some(2),
            key_version: Some(1),
            tx_hash: None,
            target_message_id: None,
            emoticon_code: None,
            sender: None,
        }
    }

    #[test]
    fn the_cap_keeps_the_newest_rows_in_stream_order() {
        let rows: Vec<Message> = (0..(MAX_ROWS as i64 + 50)).map(|i| msg(i, i)).collect();
        let capped = cap_rows(rows);
        assert_eq!(capped.len(), MAX_ROWS);
        // The oldest 50 fell off the back, not the front.
        assert_eq!(capped.first().unwrap().msg_serial, 50);
        assert_eq!(capped.last().unwrap().msg_serial, MAX_ROWS as i64 + 49);
        // Oldest-first, the order history pages merge in.
        assert!(capped.windows(2).all(|w| w[0].msg_serial < w[1].msg_serial));
    }

    #[test]
    fn a_cached_room_round_trips_through_serde_with_ciphertext_intact() {
        // The cache stores wire rows: whatever survives Message's serde must
        // survive the cache. Encrypted fields are the ones that matter — a
        // dropped `iv` would turn every cached row into "Missing metadata".
        let m = msg(7, 1000);
        let cached = CachedRoom {
            rows: vec![m.clone()],
            reactions: vec![(
                m.id.clone(),
                vec![("🍇".to_owned(), vec![m.sender_address.clone()])],
            )],
            has_more_history: true,
        };
        let json = serde_json::to_string(&cached).unwrap();
        let back: CachedRoom = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cached);
        let row = &back.rows[0];
        assert!(row.is_encrypted);
        assert_eq!(row.iv.as_deref(), Some("aXY="));
        assert_eq!(row.hmac.as_deref(), Some("aG1hYw=="));
        assert_eq!(row.content, "sealed-bytes");
    }
}
