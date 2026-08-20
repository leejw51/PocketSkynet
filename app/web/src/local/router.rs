//! The local backend's route table — the maintainer's map.
//!
//! One `match` over `(method, path segments)`. Three kinds of arm:
//!
//! 1. **Implemented**: the local-mode surface — rooms, messages, keys,
//!    knowledge, passwords, profile — answering from IndexedDB in the same
//!    wire shapes the real server sends.
//! 2. **Benign empties**: multi-user endpoints that `refresh_all` and friends
//!    call unconditionally. They answer the empty shape so nothing toasts.
//! 3. **Everything else**: 404 naming the path — its UI is hidden in local
//!    mode, so reaching it is a bug with its own error message.
//!
//! Adding a local endpoint is adding one arm here (and, if it needs new pure
//! shaping, a function in [`logic`] with a host test).

use pocketskynet_core::WalletAddress;
use serde_json::{json, Value};

use crate::api::{ApiResult, SyncPage};
use crate::format;

use super::db::{self, Op};
use super::{logic, not_available, owner};

pub(super) async fn route(method: &str, path: &str, body: Option<Value>) -> ApiResult<Value> {
    let (segments, query) = logic::split_request(path);
    let segs: Vec<&str> = segments.iter().map(String::as_str).collect();
    let body = body.unwrap_or(Value::Null);

    match (method, segs.as_slice()) {
        // --- health: the probe the connectivity layer short-circuits anyway.
        ("GET", ["api", "health"]) => Ok(json!({ "status": "ok" })),

        // --- benign empties: multi-user surfaces with no local meaning. ----
        // Before the variable-segment arms on purpose: `/api/rooms/hidden`
        // must not be captured by `/api/rooms/{room}`.
        ("GET", ["api", "presence"]) => Ok(json!([])),
        ("PUT", ["api", "presence"]) => Ok(json!({})),
        ("GET", ["api", "mentions"]) => Ok(json!([])),
        ("GET", ["api", "invitations"]) => Ok(json!([])),
        ("GET", ["api", "shout", "active"]) => Ok(json!({ "shouts": [] })),
        ("GET", ["api", "users", "blocked"]) => Ok(json!([])),
        ("GET", ["api", "users", "blocked-by"]) => Ok(json!([])),
        ("GET", ["api", "rooms", "hidden"]) => Ok(json!([])),
        ("GET", ["api", "admin", "session"]) => Ok(json!({ "isServerAdmin": false })),
        ("POST", ["api", "auth", "logout"]) => Ok(json!({})),

        // --- auth-lite -----------------------------------------------------
        ("GET", ["api", "auth", "profile"]) => profile().await,
        ("PUT", ["api", "auth", "profile"]) => update_profile(&body).await,
        ("GET", ["api", "auth", "encryption-salt"]) => {
            let salt = super::get_or_create_salt().await.map_err(storage)?;
            Ok(json!({ "salt": salt }))
        }
        ("GET", ["api", "blockchain", "info"]) => Ok(blockchain_info()),
        ("GET", ["api", "networks"]) => {
            serde_json::to_value(pocketskynet_core::chain::builtin_networks())
                .map_err(|e| storage(e.to_string()))
        }

        // --- rooms ---------------------------------------------------------
        ("GET", ["api", "rooms"]) => rooms_list().await,
        ("GET", ["api", "rooms", room]) => room_get(room).await,

        // --- messages ------------------------------------------------------
        ("POST", ["api", "rooms", room, "messages"]) => send_message(room, &body, false).await,
        ("POST", ["api", "rooms", room, "agent"]) => send_message(room, &body, true).await,
        ("GET", ["api", "rooms", room, "messages"]) => {
            let before = query.get("before").and_then(|b| b.parse().ok());
            let limit: usize = query
                .get("limit")
                .and_then(|l| l.parse().ok())
                .unwrap_or(50);
            history(room, before, limit.clamp(1, 100)).await
        }
        ("GET", ["api", "messages", id, "thread"]) => thread(id).await,
        ("PATCH", ["api", "messages", id]) => mutate_message(id, MessageChange::Edit(&body)).await,
        ("DELETE", ["api", "messages", id]) => {
            mutate_message(id, MessageChange::Tombstone).await?;
            Ok(json!({}))
        }
        ("DELETE", ["api", "rooms", room, "messages"]) => purge_room(room).await,
        ("POST", ["api", "messages", id, "emoticons"]) => {
            let code = body["emoticonCode"].as_str().unwrap_or_default().to_owned();
            react(id, &code, true).await
        }
        ("DELETE", ["api", "messages", id, "emoticons", code]) => react(id, code, false).await,
        ("GET", ["api", "messages", id, "emoticons"]) => emoticons(id).await,
        ("GET", ["api", "rooms", room, "sync"]) => {
            let since = query.get("since").and_then(|s| s.parse().ok()).unwrap_or(0);
            let page = sync(room, since).await?;
            // The wire body is the bare event array; `hasMore` normally rides
            // the X-Has-More header, but `Client::sync` never routes through
            // here — it calls `local::sync_page` directly.
            serde_json::to_value(page.0).map_err(|e| storage(e.to_string()))
        }
        ("GET", ["api", "rooms", room, "latest-serial"]) => {
            let serial = latest_serial(room).await?;
            Ok(json!({ "serial": serial }))
        }
        ("POST", ["api", "rooms", room, "read"]) => mark_read(room, &body).await,

        // --- room keys -----------------------------------------------------
        ("POST", ["api", "rooms", room, "keys"]) => put_wrap(room, &body).await,
        ("GET", ["api", "rooms", room, "keys", "versions"]) => wraps_for(room).await,
        ("GET", ["api", "rooms", room, "keys"]) => {
            let all = wraps_for(room).await?;
            match all.as_array().and_then(|a| a.last()) {
                Some(latest) => Ok(latest.clone()),
                None => Err(not_available(path)),
            }
        }

        // --- knowledge -----------------------------------------------------
        ("POST", ["api", "knowledge"]) => teach(&body).await,
        ("GET", ["api", "knowledge"]) => {
            let limit = query
                .get("limit")
                .and_then(|l| l.parse().ok())
                .unwrap_or(50);
            knowledge_list(limit).await
        }
        ("DELETE", ["api", "knowledge", id]) => {
            let d = open_db().await?;
            d.delete(db::KNOWLEDGE, id).await.map_err(storage)?;
            Ok(json!({}))
        }
        ("GET", ["api", "search"]) => {
            let q = query.get("q").map(String::as_str).unwrap_or("");
            let limit = query
                .get("limit")
                .and_then(|l| l.parse().ok())
                .unwrap_or(20);
            search(q, limit).await
        }
        ("GET", ["api", "search", "tags"]) => {
            let limit = query
                .get("limit")
                .and_then(|l| l.parse().ok())
                .unwrap_or(50);
            tags(limit).await
        }

        // --- passwords -----------------------------------------------------
        ("GET", ["api", "passwords"]) => passwords_list().await,
        ("POST", ["api", "passwords"]) => password_create(&body).await,
        ("PUT", ["api", "passwords", id]) => password_update(id, &body).await,
        ("DELETE", ["api", "passwords", id]) => {
            let d = open_db().await?;
            d.delete(db::PASSWORDS, id).await.map_err(storage)?;
            Ok(json!({}))
        }

        _ => Err(not_available(path)),
    }
}

/// The `/sync` entry `Client::sync` calls directly (its response carries
/// `hasMore` out of band, so it bypasses the JSON dispatch).
pub async fn sync_page(room: &pocketskynet_core::RoomId, since: i64) -> ApiResult<SyncPage> {
    let (events, has_more) = sync(room.as_str(), since).await?;
    let events = events
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| crate::api::ApiError::Decode(e.to_string()))?;
    Ok(SyncPage { events, has_more })
}

// --- plumbing --------------------------------------------------------------

fn storage(msg: impl std::fmt::Display) -> crate::api::ApiError {
    crate::api::ApiError::Network(msg.to_string())
}

async fn open_db() -> ApiResult<std::rc::Rc<db::Db>> {
    let owner = owner()?;
    db::open(owner.as_str()).await.map_err(storage)
}

/// Four bytes of id entropy. A CSPRNG refusal is an error, not a shrug —
/// zeroed entropy plus a same-millisecond timestamp would mint colliding ids.
fn rand4() -> ApiResult<[u8; 4]> {
    let mut bytes = [0u8; 4];
    getrandom::getrandom(&mut bytes).map_err(|e| storage(format!("CSPRNG refused: {e}")))?;
    Ok(bytes)
}

/// The two rooms local mode has, or a 403 for anything else — matching the
/// server's refusal to say whether a room exists.
fn check_room(room: &str) -> ApiResult<(WalletAddress, StaticKind)> {
    let owner = owner()?;
    if room == format!("room_note_{}", owner.as_str()) {
        return Ok((owner, StaticKind::Note));
    }
    if room == format!("room_jarvis_{}", owner.as_str()) {
        return Ok((owner, StaticKind::Jarvis));
    }
    Err(crate::api::ApiError::from_response(
        403,
        "{\"message\":\"Not a member of this room\"}",
    ))
}

#[derive(Clone, Copy, PartialEq)]
enum StaticKind {
    Note,
    Jarvis,
}

// --- auth-lite -------------------------------------------------------------

async fn profile() -> ApiResult<Value> {
    let d = open_db().await?;
    let stored = d.get(db::META, "user").await.map_err(storage)?;
    match stored {
        Some(user) => serde_json::from_str(&user).map_err(|e| storage(e.to_string())),
        None => Err(not_available("/api/auth/profile")),
    }
}

async fn update_profile(body: &Value) -> ApiResult<Value> {
    let d = open_db().await?;
    let stored = d.get(db::META, "user").await.map_err(storage)?;
    let mut user: Value = stored
        .as_deref()
        .and_then(|u| serde_json::from_str(u).ok())
        .unwrap_or(json!({}));
    if let Some(name) = body["username"].as_str() {
        if !name.trim().is_empty() {
            user["username"] = json!(name.trim());
        }
    }
    // Wire contract: absent leaves the avatar, "" clears it, a value sets it.
    match body["profileImage"].as_str() {
        Some("") => user["profileImage"] = Value::Null,
        Some(v) => user["profileImage"] = json!(v),
        None => {}
    }
    d.put(db::META, "user", &user.to_string())
        .await
        .map_err(storage)?;
    Ok(user)
}

/// Canned chain facts, from the same registry the wallet uses, so the Bank
/// page and explorer links boot without a server. Everything server-specific
/// (Privy, shouts, publishing) is empty — which each feature reads as "not
/// configured".
fn blockchain_info() -> Value {
    let networks = pocketskynet_core::chain::builtin_networks();
    let first = networks.first();
    json!({
        "chainId": first.and_then(|n| n.chain_id).map(|id| id.to_string()).unwrap_or_default(),
        "privyAppId": "",
        "caCertAvailable": false,
        "chainRpc": first.map(|n| n.rpc_url.clone()).unwrap_or_default(),
        "chainName": first.map(|n| n.name.clone()).unwrap_or_default(),
        "chainExplorer": first.map(|n| n.explorer_url.clone()).unwrap_or_default(),
        "fruitnationHashCro": "",
        "fruitnationWallet": "",
        "shoutPriceCro": "",
        "publishPriceCro": "",
    })
}

// --- rooms -----------------------------------------------------------------

async fn room_extras(d: &db::Db, room_id: &str) -> ApiResult<logic::RoomExtras> {
    let (from, to) = logic::wrap_range(room_id);
    let wraps = d
        .get_range(db::WRAPS, &from, &to, None)
        .await
        .map_err(storage)?;
    let current_key_version = wraps
        .iter()
        .filter_map(|w| serde_json::from_str::<Value>(w).ok())
        .filter_map(|w| w["keyVersion"].as_i64())
        .max()
        .unwrap_or(1);

    let read_key = format!("read:{room_id}");
    let last_read_serial: i64 = d
        .get(db::META, &read_key)
        .await
        .map_err(storage)?
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    // The tail of the room: enough rows to find the last renderable message
    // and count unread ones without loading the whole history.
    let (from, to) = logic::room_range_after(room_id, 0);
    let tail = d
        .get_range(db::MESSAGES, &from, &to, Some(logic::SYNC_PAGE))
        .await
        .map_err(storage)?;
    let rows: Vec<Value> = tail
        .iter()
        .filter_map(|r| serde_json::from_str(r).ok())
        .collect();
    let owner = owner()?;
    let last_message = logic::history_page(&rows, None, 1).pop();
    let unread_count = rows
        .iter()
        .filter(|r| r["msgSerial"].as_i64().unwrap_or(0) > last_read_serial)
        .filter(|r| r["senderAddress"].as_str() != Some(owner.as_str()))
        .filter(|r| matches!(r["msgType"].as_str().unwrap_or("add"), "add" | "edit"))
        .filter(|r| !r["isDeleted"].as_bool().unwrap_or(false))
        .count() as u32;

    Ok(logic::RoomExtras {
        has_encryption: !wraps.is_empty(),
        current_key_version,
        last_message,
        last_read_serial,
        unread_count,
    })
}

async fn rooms_list() -> ApiResult<Value> {
    let owner = owner()?;
    let d = open_db().await?;
    let user: Value = d
        .get(db::META, "user")
        .await
        .map_err(storage)?
        .and_then(|u| serde_json::from_str(&u).ok())
        .unwrap_or(json!({ "walletAddress": owner.as_str(), "username": "" }));
    let agent = WalletAddress::agent_of(&owner);
    let note = room_extras(&d, &format!("room_note_{}", owner.as_str())).await?;
    let jarvis = room_extras(&d, &format!("room_jarvis_{}", owner.as_str())).await?;
    Ok(logic::static_rooms(
        owner.as_str(),
        agent.as_str(),
        &user,
        note,
        jarvis,
    ))
}

async fn room_get(room: &str) -> ApiResult<Value> {
    let (_, kind) = check_room(room)?;
    let rooms = rooms_list().await?;
    let index = match kind {
        StaticKind::Note => 0,
        StaticKind::Jarvis => 1,
    };
    Ok(rooms[index].clone())
}

// --- messages --------------------------------------------------------------

async fn send_message(room: &str, body: &Value, as_agent: bool) -> ApiResult<Value> {
    let (owner, kind) = check_room(room)?;
    if as_agent && kind != StaticKind::Jarvis {
        return Err(not_available("agent reply outside My Jarvis"));
    }
    let sender = if as_agent {
        WalletAddress::agent_of(&owner)
    } else {
        owner
    };
    let now = format::now_ms();
    let id = logic::mint_message_id(now, rand4()?);
    let d = open_db().await?;
    let room_owned = room.to_owned();
    let body = body.clone();
    let row_out = std::rc::Rc::new(std::cell::RefCell::new(Value::Null));
    let row_slot = row_out.clone();
    d.commit_with_serial(move |serial| {
        let row = logic::message_row(&body, &id, &room_owned, sender.as_str(), serial, now);
        let key = logic::message_key(&room_owned, serial);
        *row_slot.borrow_mut() = row.clone();
        vec![
            Op::Put {
                store: db::MESSAGES,
                key: key.clone(),
                value: row.to_string(),
            },
            Op::Put {
                store: db::MSGID,
                key: id.clone(),
                value: key,
            },
        ]
    })
    .await
    .map_err(storage)?;
    let row = row_out.borrow().clone();
    Ok(row)
}

/// Locate a message row by id: `(room, key, row)`.
async fn find_message(d: &db::Db, id: &str) -> ApiResult<(String, String, Value)> {
    let key = d
        .get(db::MSGID, id)
        .await
        .map_err(storage)?
        .ok_or_else(|| not_available("message"))?;
    let row = d
        .get(db::MESSAGES, &key)
        .await
        .map_err(storage)?
        .ok_or_else(|| not_available("message"))?;
    let row: Value = serde_json::from_str(&row).map_err(|e| storage(e.to_string()))?;
    let room = key.split('|').next().unwrap_or_default().to_owned();
    Ok((room, key, row))
}

enum MessageChange<'a> {
    Edit(&'a Value),
    Tombstone,
}

async fn mutate_message(id: &str, change: MessageChange<'_>) -> ApiResult<Value> {
    let d = open_db().await?;
    let (room, old_key, mut row) = find_message(&d, id).await?;
    check_room(&room)?;
    let now = format::now_ms();
    let body = match &change {
        MessageChange::Edit(b) => Some((*b).clone()),
        MessageChange::Tombstone => None,
    };
    let id = id.to_owned();
    let row_out = std::rc::Rc::new(std::cell::RefCell::new(Value::Null));
    let row_slot = row_out.clone();
    d.commit_with_serial(move |serial| {
        match body {
            Some(body) => logic::apply_edit(&mut row, &body, serial, now),
            None => logic::apply_tombstone(&mut row, serial),
        }
        let new_key = logic::message_key(&room, serial);
        *row_slot.borrow_mut() = row.clone();
        vec![
            Op::Delete {
                store: db::MESSAGES,
                key: old_key,
            },
            Op::Put {
                store: db::MESSAGES,
                key: new_key.clone(),
                value: row.to_string(),
            },
            Op::Put {
                store: db::MSGID,
                key: id,
                value: new_key,
            },
        ]
    })
    .await
    .map_err(storage)?;
    let row = row_out.borrow().clone();
    Ok(row)
}

async fn room_rows(d: &db::Db, room: &str) -> ApiResult<Vec<Value>> {
    let (from, to) = logic::room_range(room);
    let raw = d
        .get_range(db::MESSAGES, &from, &to, None)
        .await
        .map_err(storage)?;
    Ok(raw
        .iter()
        .filter_map(|r| serde_json::from_str(r).ok())
        .collect())
}

async fn history(room: &str, before: Option<i64>, limit: usize) -> ApiResult<Value> {
    check_room(room)?;
    let d = open_db().await?;
    let rows = room_rows(&d, room).await?;
    Ok(Value::Array(logic::history_page(&rows, before, limit)))
}

async fn thread(id: &str) -> ApiResult<Value> {
    let d = open_db().await?;
    let (room, _, _) = find_message(&d, id).await?;
    check_room(&room)?;
    let rows = room_rows(&d, &room).await?;
    Ok(Value::Array(logic::thread_of(&rows, id)))
}

async fn purge_room(room: &str) -> ApiResult<Value> {
    let (owner, _) = check_room(room)?;
    let d = open_db().await?;
    let (from, to) = logic::room_range(room);
    d.delete_range(db::MESSAGES, &from, &to)
        .await
        .map_err(storage)?;
    let now = format::now_ms();
    let id = logic::mint_message_id(now, rand4()?);
    let room = room.to_owned();
    d.commit_with_serial(move |serial| {
        let row = logic::purge_marker(&id, &room, owner.as_str(), serial, now);
        vec![Op::Put {
            store: db::MESSAGES,
            key: logic::message_key(&room, serial),
            value: row.to_string(),
        }]
    })
    .await
    .map_err(storage)?;
    Ok(json!({}))
}

async fn react(id: &str, code: &str, add: bool) -> ApiResult<Value> {
    if code.is_empty() {
        return Err(not_available("emoticon without a code"));
    }
    let d = open_db().await?;
    let (room, _, _) = find_message(&d, id).await?;
    let (owner, _) = check_room(&room)?;
    let now = format::now_ms();
    let event_id = logic::mint_message_id(now, rand4()?);
    let target = id.to_owned();
    let code = code.to_owned();
    d.commit_with_serial(move |serial| {
        let row = logic::reaction_row(
            add,
            &event_id,
            &room,
            owner.as_str(),
            &target,
            &code,
            serial,
            now,
        );
        vec![Op::Put {
            store: db::MESSAGES,
            key: logic::message_key(&room, serial),
            value: row.to_string(),
        }]
    })
    .await
    .map_err(storage)?;
    Ok(json!({}))
}

async fn emoticons(id: &str) -> ApiResult<Value> {
    let d = open_db().await?;
    let (room, _, _) = find_message(&d, id).await?;
    check_room(&room)?;
    let rows = room_rows(&d, &room).await?;
    Ok(Value::Array(logic::emoticon_aggregates(&rows, id)))
}

async fn sync(room: &str, since: i64) -> ApiResult<(Vec<Value>, bool)> {
    check_room(room)?;
    let d = open_db().await?;
    let (from, to) = logic::room_range_after(room, since);
    // One row past the cap answers `hasMore` without a count query.
    let raw = d
        .get_range(db::MESSAGES, &from, &to, None)
        .await
        .map_err(storage)?;
    let rows: Vec<Value> = raw
        .iter()
        .filter_map(|r| serde_json::from_str(r).ok())
        .collect();
    Ok(logic::sync_page(rows))
}

async fn latest_serial(room: &str) -> ApiResult<i64> {
    check_room(room)?;
    let d = open_db().await?;
    let (from, to) = logic::room_range(room);
    let last = d
        .get_range(db::MESSAGES, &from, &to, Some(1))
        .await
        .map_err(storage)?;
    Ok(last
        .first()
        .and_then(|r| serde_json::from_str::<Value>(r).ok())
        .and_then(|r| r["msgSerial"].as_i64())
        .unwrap_or(0))
}

async fn mark_read(room: &str, body: &Value) -> ApiResult<Value> {
    check_room(room)?;
    let d = open_db().await?;
    let asked = body["lastReadSerial"].as_i64().unwrap_or(0).max(0);
    let key = format!("read:{room}");
    let current: i64 = d
        .get(db::META, &key)
        .await
        .map_err(storage)?
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    // Monotonic, mirroring the server: a lower value is a no-op.
    let next = current.max(asked);
    d.put(db::META, &key, &next.to_string())
        .await
        .map_err(storage)?;
    Ok(json!({ "roomId": room, "lastReadSerial": next }))
}

// --- room keys -------------------------------------------------------------

async fn put_wrap(room: &str, body: &Value) -> ApiResult<Value> {
    check_room(room)?;
    let d = open_db().await?;
    let version = body["keyVersion"].as_i64().unwrap_or(1);
    // Stored in the full RoomKey wire shape so reads are a passthrough.
    let mut row = body.clone();
    row["id"] = json!(0);
    row["roomId"] = json!(room);
    row["keyVersion"] = json!(version);
    row["createdAt"] = json!(logic::iso8601_ms(format::now_ms()));
    d.put(db::WRAPS, &logic::wrap_key(room, version), &row.to_string())
        .await
        .map_err(storage)?;
    Ok(json!({}))
}

async fn wraps_for(room: &str) -> ApiResult<Value> {
    check_room(room)?;
    let d = open_db().await?;
    let (from, to) = logic::wrap_range(room);
    let wraps = d
        .get_range(db::WRAPS, &from, &to, None)
        .await
        .map_err(storage)?;
    Ok(Value::Array(
        wraps
            .iter()
            .filter_map(|w| serde_json::from_str(w).ok())
            .collect(),
    ))
}

// --- knowledge -------------------------------------------------------------

/// A stored note: the wire `KnowledgeNote` with `content` replaced by the
/// sealed envelope. Opened on the way out.
async fn teach(body: &Value) -> ApiResult<Value> {
    let owner = owner()?;
    let content = body["content"].as_str().unwrap_or("").trim().to_owned();
    if content.is_empty() {
        return Err(not_available("empty note"));
    }
    let d = open_db().await?;
    let now = format::now_ms();
    let id = logic::mint_note_id(now, rand4()?);
    let tags = logic::extract_tags(&content);
    let sealed = super::seal(&format!("local:knowledge:{id}"), &content)?;
    let stored = json!({
        "id": id,
        "ownerAddress": owner.as_str(),
        "sealed": sealed,
        "roomId": body["roomId"].clone(),
        "sourceMessageId": body["sourceMessageId"].clone(),
        "tags": tags,
        "createdAt": now,
        "updatedAt": now,
    });
    d.put(db::KNOWLEDGE, &id, &stored.to_string())
        .await
        .map_err(storage)?;
    let mut note = stored;
    note["content"] = json!(content);
    note.as_object_mut().map(|o| o.remove("sealed"));
    Ok(note)
}

/// Load and unseal every note. Sealed rows a locked session cannot open are
/// skipped — same stance as sealed bubbles: a state, not an error.
async fn open_notes() -> ApiResult<Vec<Value>> {
    let d = open_db().await?;
    let raw = d.get_all(db::KNOWLEDGE).await.map_err(storage)?;
    let mut notes = Vec::new();
    for row in raw {
        let Ok(mut note) = serde_json::from_str::<Value>(&row) else {
            continue;
        };
        let id = note["id"].as_str().unwrap_or_default().to_owned();
        let Some(sealed) = note["sealed"].as_str() else {
            continue;
        };
        let Some(content) = super::open(&format!("local:knowledge:{id}"), sealed) else {
            continue;
        };
        note["content"] = json!(content);
        note.as_object_mut().map(|o| o.remove("sealed"));
        notes.push(note);
    }
    // Newest first, as the server lists them.
    notes.sort_by_key(|n| std::cmp::Reverse(n["createdAt"].as_i64().unwrap_or(0)));
    Ok(notes)
}

async fn knowledge_list(limit: usize) -> ApiResult<Value> {
    let mut notes = open_notes().await?;
    notes.truncate(limit.clamp(1, 200));
    Ok(json!({ "notes": notes }))
}

/// Knowledge notes only, deliberately — parity, not a gap. The server's
/// hybrid `/api/search` indexes *plaintext* messages; every local room is
/// E2EE, and a real server cannot search those either. Message search over
/// encrypted rooms happens where the keys are — Jarvis's `search_rooms`
/// scans decrypted rows on the device, in both modes.
async fn search(q: &str, limit: usize) -> ApiResult<Value> {
    let notes = open_notes().await?;
    let (tags, terms) = logic::parse_query(q);
    let mut hits: Vec<(f32, Value)> = notes
        .iter()
        .filter(|n| {
            tags.iter().all(|t| {
                n["tags"]
                    .as_array()
                    .is_some_and(|nt| nt.iter().any(|x| x.as_str() == Some(t.as_str())))
            })
        })
        .filter_map(|n| {
            let content = n["content"].as_str().unwrap_or_default();
            let score = logic::search_score(content, &terms);
            (score > 0.0).then(|| {
                (
                    score,
                    json!({
                        "kind": "knowledge",
                        "refId": n["id"],
                        "roomId": n["roomId"],
                        "sender": n["ownerAddress"],
                        "timestamp": n["updatedAt"],
                        "text": content,
                        "tags": n["tags"],
                        "score": score,
                    }),
                )
            })
        })
        .collect();
    hits.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.1["timestamp"]
                    .as_i64()
                    .unwrap_or(0)
                    .cmp(&a.1["timestamp"].as_i64().unwrap_or(0))
            })
    });
    hits.truncate(limit.clamp(1, 100));
    Ok(json!({ "results": hits.into_iter().map(|(_, h)| h).collect::<Vec<_>>() }))
}

async fn tags(limit: usize) -> ApiResult<Value> {
    let notes = open_notes().await?;
    let mut counts: std::collections::BTreeMap<String, i64> = Default::default();
    for note in &notes {
        if let Some(tags) = note["tags"].as_array() {
            for tag in tags.iter().filter_map(|t| t.as_str()) {
                *counts.entry(tag.to_owned()).or_default() += 1;
            }
        }
    }
    let mut tags: Vec<(String, i64)> = counts.into_iter().collect();
    tags.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    tags.truncate(limit.clamp(1, 200));
    Ok(json!({
        "tags": tags
            .into_iter()
            .map(|(tag, count)| json!({ "tag": tag, "count": count }))
            .collect::<Vec<_>>()
    }))
}

// --- passwords -------------------------------------------------------------

async fn passwords_list() -> ApiResult<Value> {
    let d = open_db().await?;
    let raw = d.get_all(db::PASSWORDS).await.map_err(storage)?;
    let mut entries: Vec<Value> = raw
        .iter()
        .filter_map(|r| serde_json::from_str(r).ok())
        .collect();
    entries.sort_by_key(|e| std::cmp::Reverse(e["updatedAt"].as_i64().unwrap_or(0)));
    Ok(Value::Array(entries))
}

async fn password_create(body: &Value) -> ApiResult<Value> {
    let d = open_db().await?;
    let id = body["id"].as_str().unwrap_or_default();
    if id.is_empty() {
        return Err(not_available("password without an id"));
    }
    if d.get(db::PASSWORDS, id).await.map_err(storage)?.is_some() {
        // Mirrors the server: a retried create must not clobber an edit.
        return Err(crate::api::ApiError::from_response(
            409,
            "{\"message\":\"That id is taken\"}",
        ));
    }
    let now = format::now_ms();
    let entry = json!({
        "id": id,
        "key": body["key"],
        "value": body["value"],
        "encVer": body["encVer"].as_i64().unwrap_or(1),
        "createdAt": now,
        "updatedAt": now,
    });
    d.put(db::PASSWORDS, id, &entry.to_string())
        .await
        .map_err(storage)?;
    Ok(entry)
}

async fn password_update(id: &str, body: &Value) -> ApiResult<Value> {
    let d = open_db().await?;
    let stored = d
        .get(db::PASSWORDS, id)
        .await
        .map_err(storage)?
        .ok_or_else(|| not_available("password"))?;
    let mut entry: Value = serde_json::from_str(&stored).map_err(|e| storage(e.to_string()))?;
    entry["key"] = body["key"].clone();
    entry["value"] = body["value"].clone();
    entry["encVer"] = json!(body["encVer"].as_i64().unwrap_or(1));
    entry["updatedAt"] = json!(format::now_ms());
    d.put(db::PASSWORDS, id, &entry.to_string())
        .await
        .map_err(storage)?;
    Ok(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_on<T>(fut: impl std::future::Future<Output = T>) -> T {
        futures::executor::block_on(fut)
    }

    #[test]
    fn an_unknown_route_answers_404_naming_itself() {
        let err = block_on(route("POST", "/api/uploads", None)).unwrap_err();
        assert_eq!(err.status(), Some(404));
        assert!(err.user_message().contains("/api/uploads"));
    }

    #[test]
    fn multi_user_surfaces_answer_benign_empties() {
        for (method, path) in [
            ("GET", "/api/presence"),
            ("GET", "/api/mentions?limit=50"),
            ("GET", "/api/invitations"),
            ("GET", "/api/users/blocked"),
            ("GET", "/api/users/blocked-by"),
            ("GET", "/api/rooms/hidden"),
        ] {
            let v = block_on(route(method, path, None)).unwrap();
            assert_eq!(v, json!([]), "{method} {path}");
        }
        assert_eq!(
            block_on(route("GET", "/api/shout/active", None)).unwrap(),
            json!({ "shouts": [] })
        );
        assert_eq!(
            block_on(route("GET", "/api/admin/session", None)).unwrap(),
            json!({ "isServerAdmin": false })
        );
    }

    #[test]
    fn canned_chain_facts_decode_through_the_wire_types() {
        let info: crate::api::BlockchainInfo =
            serde_json::from_value(blockchain_info()).expect("BlockchainInfo shape");
        // Empty strings are the feature flags for everything server-side.
        assert!(info.privy_app_id.is_empty());
        assert!(!info.chain_rpc.is_empty(), "the wallet needs an RPC URL");
        let networks = block_on(route("GET", "/api/networks", None)).unwrap();
        let networks: Vec<pocketskynet_core::chain::Network> =
            serde_json::from_value(networks).expect("Network shapes");
        assert!(!networks.is_empty());
    }
}
