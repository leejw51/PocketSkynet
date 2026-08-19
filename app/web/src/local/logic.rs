//! Pure request/response logic for the local backend. No browser APIs, no
//! storage — everything here runs under a plain `cargo test` on the host.
//!
//! Rows are built and mutated as `serde_json::Value`s in the **wire shape** —
//! camelCase, exactly what the real server sends — and the anti-drift tests
//! at the bottom round-trip every one of them through the typed structs in
//! `crate::api::types`. A shape this module gets wrong fails `cargo test`,
//! not a screen at runtime.

use serde_json::{json, Value};

use std::collections::BTreeMap;

/// The `/sync` page cap, matching the server's.
pub const SYNC_PAGE: usize = 200;

/// Split a request path into its segments and decoded query parameters.
///
/// `"/api/rooms/room_1/sync?since=42"` →
/// `(["api", "rooms", "room_1", "sync"], {"since": "42"})`.
///
/// Segments are percent-decoded too: the client encodes emoticon codes into
/// path segments (`/messages/:id/emoticons/:code`), and the router compares
/// the decoded form — matching what the real server does when axum decodes
/// the path.
pub fn split_request(path_and_query: &str) -> (Vec<String>, BTreeMap<String, String>) {
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path_and_query, None),
    };
    let segments = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(percent_decode)
        .collect();
    let mut params = BTreeMap::new();
    if let Some(query) = query {
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            params.insert(percent_decode(k), percent_decode(v));
        }
    }
    (segments, params)
}

/// Decode `%XX` escapes (and `+` as space, for query values). Invalid escapes
/// pass through literally rather than erroring — a decoder that fails on bad
/// input turns a typo into a dead request.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                    out.push(h * 16 + l);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// --- keys ------------------------------------------------------------------

/// A serial, zero-padded so IndexedDB's string key order *is* serial order.
/// Twelve digits outlives any plausible local history.
pub fn pad_serial(serial: i64) -> String {
    format!("{:012}", serial.max(0))
}

/// The `messages` store key: `<roomId>|<serial>`. Room ids cannot contain
/// `|`, so the composite is unambiguous.
pub fn message_key(room: &str, serial: i64) -> String {
    format!("{room}|{}", pad_serial(serial))
}

/// The inclusive key range covering a whole room's rows.
pub fn room_range(room: &str) -> (String, String) {
    (message_key(room, 0), format!("{room}|999999999999"))
}

/// The inclusive key range covering a room's rows with `serial > since`.
pub fn room_range_after(room: &str, since: i64) -> (String, String) {
    (
        message_key(room, since.saturating_add(1)),
        format!("{room}|999999999999"),
    )
}

/// The `wraps` store key: `<roomId>|<keyVersion>`.
pub fn wrap_key(room: &str, key_version: i64) -> String {
    format!("{room}|{:06}", key_version.max(0))
}

pub fn wrap_range(room: &str) -> (String, String) {
    (wrap_key(room, 0), format!("{room}|999999"))
}

// --- ids and timestamps ----------------------------------------------------

/// Mint a message id the wire grammar accepts: `msg_<ms>_<8 hex>`.
/// Entropy comes in as a parameter — this module stays clock- and CSPRNG-free.
pub fn mint_message_id(now_ms: i64, rand: [u8; 4]) -> String {
    format!("msg_{}_{}", now_ms.max(0), hex::encode(rand))
}

/// Mint a knowledge-note id, same shape under a different prefix.
pub fn mint_note_id(now_ms: i64, rand: [u8; 4]) -> String {
    format!("note_{}_{}", now_ms.max(0), hex::encode(rand))
}

/// Milliseconds since the epoch → `YYYY-MM-DDTHH:MM:SS.mmmZ`.
///
/// Hand-rolled (days-from-civil, Howard Hinnant's algorithm) because the wasm
/// bundle has no `chrono` and `std::time` panics there; ten lines beat a
/// dependency.
pub fn iso8601_ms(ms: i64) -> String {
    let ms = ms.max(0);
    let (secs, millis) = (ms / 1000, ms % 1000);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil-from-days, shifted so the era starts on 0000-03-01.
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

// --- row builders ----------------------------------------------------------

/// Build a wire-form message row from the body the client sent.
///
/// Handles both `MessageBody` (`content`) and `AgentReplyBody` (`text`) —
/// they carry the same sealed-or-plain shape under different field names.
pub fn message_row(
    body: &Value,
    id: &str,
    room: &str,
    sender: &str,
    serial: i64,
    now_ms: i64,
) -> Value {
    let content = body["content"]
        .as_str()
        .or_else(|| body["text"].as_str())
        .unwrap_or_default();
    json!({
        "id": id,
        "roomId": room,
        "senderAddress": sender,
        "content": content,
        "msgHash": body["msgHash"].as_str().unwrap_or_default(),
        "messageTimestamp": now_ms,
        "msgType": "add",
        "msgSerial": serial,
        "isDeleted": false,
        "createdAt": iso8601_ms(now_ms),
        "isEncrypted": body["isEncrypted"].as_bool().unwrap_or(false),
        "iv": body["iv"].clone(),
        "hmac": body["hmac"].clone(),
        "encVer": body["encVer"].as_i64().unwrap_or(1),
        "keyVersion": body["keyVersion"].as_i64().unwrap_or(1),
        "parentMessageId": body["parentMessageId"].clone(),
    })
}

/// A reaction event row (`emoticon_add` / `emoticon_remove`).
///
/// Eight positional facts is at clippy's line, but a params struct here would
/// be eight named fields built once at the one call site — ceremony, not
/// clarity, for a function whose output is pinned by its own round-trip test.
#[allow(clippy::too_many_arguments)]
pub fn reaction_row(
    add: bool,
    id: &str,
    room: &str,
    sender: &str,
    target: &str,
    code: &str,
    serial: i64,
    now_ms: i64,
) -> Value {
    json!({
        "id": id,
        "roomId": room,
        "senderAddress": sender,
        "content": "",
        "messageTimestamp": now_ms,
        "msgType": if add { "emoticon_add" } else { "emoticon_remove" },
        "msgSerial": serial,
        "isDeleted": false,
        "targetMessageId": target,
        "emoticonCode": code,
    })
}

/// The `delete_all` marker a purge leaves behind, so `/sync` folding clears
/// every other tab's state at the right point in serial order.
pub fn purge_marker(id: &str, room: &str, sender: &str, serial: i64, now_ms: i64) -> Value {
    json!({
        "id": id,
        "roomId": room,
        "senderAddress": sender,
        "content": "",
        "messageTimestamp": now_ms,
        "msgType": "delete_all",
        "msgSerial": serial,
        "isDeleted": false,
    })
}

/// Edit a row in place: same id, same `messageTimestamp`, new sealed-or-plain
/// halves, `msgType: "edit"` and a fresh serial so `/sync` redelivers it.
pub fn apply_edit(row: &mut Value, body: &Value, serial: i64, now_ms: i64) {
    row["content"] = body["content"].clone();
    row["msgHash"] = body["msgHash"].clone();
    row["isEncrypted"] = body["isEncrypted"].clone();
    row["iv"] = body["iv"].clone();
    row["hmac"] = body["hmac"].clone();
    row["encVer"] = json!(body["encVer"].as_i64().unwrap_or(1));
    row["keyVersion"] = json!(body["keyVersion"].as_i64().unwrap_or(1));
    row["msgType"] = json!("edit");
    row["msgSerial"] = json!(serial);
    row["editedAt"] = json!(iso8601_ms(now_ms));
}

/// Tombstone a row: `msgType: "delete"` with a fresh serial, content cleared.
/// `/sync` folding removes it by id; `/messages` filters it out.
pub fn apply_tombstone(row: &mut Value, serial: i64) {
    row["isDeleted"] = json!(true);
    row["msgType"] = json!("delete");
    row["msgSerial"] = json!(serial);
    row["content"] = json!("");
    row["iv"] = Value::Null;
    row["hmac"] = Value::Null;
}

// --- reads -----------------------------------------------------------------

fn msg_type(row: &Value) -> &str {
    row["msgType"].as_str().unwrap_or("add")
}

fn is_event_row(row: &Value) -> bool {
    matches!(
        msg_type(row),
        "emoticon_add" | "emoticon_remove" | "delete_all"
    )
}

fn is_renderable(row: &Value) -> bool {
    !is_event_row(row) && !row["isDeleted"].as_bool().unwrap_or(false)
}

fn row_ts(row: &Value) -> i64 {
    row["messageTimestamp"].as_i64().unwrap_or(0)
}

fn row_serial(row: &Value) -> i64 {
    row["msgSerial"].as_i64().unwrap_or(0)
}

/// `GET /rooms/{id}/messages` semantics over a room's rows: renderable,
/// top-level only, newest `limit` before `before`, with
/// `replyCount`/`lastReplyAt` summarising the hidden replies.
///
/// Rows arrive in serial-key order, but an **edit** moves its row to a fresh
/// serial while keeping its original `messageTimestamp` — so serial order is
/// not chronological order. Sort exactly as the server does (`ORDER BY
/// message_timestamp, msg_serial`) before paging; without it, editing an old
/// message would push it past genuinely newer rows and strand those off the
/// first page, unreachable by the timestamp-based `before` cursor.
pub fn history_page(rows: &[Value], before: Option<i64>, limit: usize) -> Vec<Value> {
    // Reply summaries are computed over the whole room, not the page — a root
    // inside the page can have replies outside it.
    let mut replies: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    for row in rows.iter().filter(|r| is_renderable(r)) {
        if let Some(parent) = row["parentMessageId"].as_str() {
            let ts = row_ts(row);
            let entry = replies.entry(parent.to_owned()).or_insert((0, 0));
            entry.0 += 1;
            entry.1 = entry.1.max(ts);
        }
    }

    let mut page: Vec<Value> = rows
        .iter()
        .filter(|r| is_renderable(r) && r["parentMessageId"].as_str().is_none())
        .filter(|r| match before {
            Some(b) => row_ts(r) < b,
            None => true,
        })
        .cloned()
        .collect();
    page.sort_by_key(|r| (row_ts(r), row_serial(r)));
    if page.len() > limit {
        page.drain(..page.len() - limit);
    }
    for row in &mut page {
        if let Some(id) = row["id"].as_str() {
            if let Some((count, last)) = replies.get(id) {
                row["replyCount"] = json!(count);
                row["lastReplyAt"] = json!(last);
            }
        }
    }
    page
}

/// `GET /messages/{id}/thread` semantics: the root (by id, or the root of the
/// reply named) followed by its replies, ascending. Empty if the id names
/// nothing renderable.
pub fn thread_of(rows: &[Value], id: &str) -> Vec<Value> {
    let root_id = rows.iter().find(|r| r["id"].as_str() == Some(id)).map(|r| {
        match r["parentMessageId"].as_str() {
            Some(parent) => parent.to_owned(),
            None => id.to_owned(),
        }
    });
    let Some(root_id) = root_id else {
        return Vec::new();
    };
    let mut thread: Vec<Value> = rows
        .iter()
        .filter(|r| is_renderable(r))
        .filter(|r| {
            r["id"].as_str() == Some(root_id.as_str())
                || r["parentMessageId"].as_str() == Some(root_id.as_str())
        })
        .cloned()
        .collect();
    // Chronological, not serial order — an edited reply keeps its place.
    thread.sort_by_key(|r| (row_ts(r), row_serial(r)));
    thread
}

/// `GET /messages/{id}/emoticons` semantics: fold the reaction event rows for
/// one target into aggregates.
pub fn emoticon_aggregates(rows: &[Value], target: &str) -> Vec<Value> {
    // code → set of senders, folded in serial order (rows arrive ascending).
    let mut folded: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in rows {
        if row["targetMessageId"].as_str() != Some(target) {
            continue;
        }
        let (Some(code), Some(sender)) =
            (row["emoticonCode"].as_str(), row["senderAddress"].as_str())
        else {
            continue;
        };
        match msg_type(row) {
            "emoticon_add" => {
                let senders = folded.entry(code.to_owned()).or_default();
                if !senders.iter().any(|s| s == sender) {
                    senders.push(sender.to_owned());
                }
            }
            "emoticon_remove" => {
                if let Some(senders) = folded.get_mut(code) {
                    senders.retain(|s| s != sender);
                    if senders.is_empty() {
                        folded.remove(code);
                    }
                }
            }
            _ => {}
        }
    }
    folded
        .into_iter()
        .map(|(code, senders)| {
            json!({
                "emoticonCode": code,
                "count": senders.len(),
                "users": [],
            })
        })
        .collect()
}

/// `/sync` paging over rows already filtered to `serial > since` and
/// ascending: cap the page, report whether more remained.
pub fn sync_page(mut rows: Vec<Value>) -> (Vec<Value>, bool) {
    let has_more = rows.len() > SYNC_PAGE;
    rows.truncate(SYNC_PAGE);
    (rows, has_more)
}

// --- rooms -----------------------------------------------------------------

/// Synthesize the two local static rooms as `GET /api/rooms` shapes them.
///
/// `user` is the owner's wire-form `User`; `last` and `wraps` are per-room
/// lookups the router did. Nothing about these rooms is stored — they exist
/// by construction, exactly as the server provisions them on sign-in.
pub fn static_rooms(
    owner: &str,
    agent: &str,
    user: &Value,
    note_extras: RoomExtras,
    jarvis_extras: RoomExtras,
) -> Value {
    let agent_user = json!({
        "walletAddress": agent,
        "username": "Jarvis",
    });
    let note = static_room(owner, "note", "My Note", vec![user.clone()], note_extras);
    let jarvis = static_room(
        owner,
        "jarvis",
        "My Jarvis",
        vec![user.clone(), agent_user],
        jarvis_extras,
    );
    json!([note, jarvis])
}

/// The per-room facts the router looks up before synthesis.
#[derive(Default)]
pub struct RoomExtras {
    pub has_encryption: bool,
    pub current_key_version: i64,
    pub last_message: Option<Value>,
    pub last_read_serial: i64,
    pub unread_count: u32,
}

/// One synthesized `RoomWithMembers`.
pub fn static_room(
    owner: &str,
    kind: &str,
    name: &str,
    member_users: Vec<Value>,
    extras: RoomExtras,
) -> Value {
    let id = format!("room_{kind}_{owner}");
    let members: Vec<Value> = member_users
        .iter()
        .map(|u| {
            json!({
                "id": 0,
                "roomId": id,
                "userAddress": u["walletAddress"],
                "user": u,
            })
        })
        .collect();
    json!({
        "id": id,
        "name": name,
        "currentKeyVersion": extras.current_key_version.max(1),
        "keyRotationPending": false,
        "kind": kind,
        "memberCount": members.len(),
        "members": members,
        "admins": [],
        "lastMessage": extras.last_message,
        "hasEncryption": extras.has_encryption,
        "unreadCount": extras.unread_count,
        "lastReadSerial": extras.last_read_serial,
        "mentionCount": 0,
    })
}

// --- knowledge search ------------------------------------------------------

/// Split a query into `#tag` filters and plain lowercase terms.
pub fn parse_query(q: &str) -> (Vec<String>, Vec<String>) {
    let mut tags = Vec::new();
    let mut terms = Vec::new();
    for token in q.split_whitespace() {
        if let Some(tag) = token.strip_prefix('#') {
            if !tag.is_empty() {
                tags.push(tag.to_lowercase());
            }
        } else {
            terms.push(token.to_lowercase());
        }
    }
    (tags, terms)
}

/// Extract `#tag` tokens from note content, lowercased, deduped, in order.
pub fn extract_tags(content: &str) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for token in content.split_whitespace() {
        if let Some(tag) = token.strip_prefix('#') {
            let tag: String = tag
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
                .collect::<String>()
                .to_lowercase();
            if !tag.is_empty() && !tags.contains(&tag) {
                tags.push(tag);
            }
        }
    }
    tags
}

/// Score one note against parsed query terms. Zero means no match. Simple
/// term-count scoring is deliberate v1; upgrading the ranking is swapping
/// this function's internals, nothing else.
pub fn search_score(content: &str, terms: &[String]) -> f32 {
    if terms.is_empty() {
        // Tag-only or empty queries browse newest-first; every note matches
        // with a flat score and the caller orders by recency.
        return 1.0;
    }
    let haystack = content.to_lowercase();
    let mut score = 0.0f32;
    for term in terms {
        let hits = haystack.matches(term.as_str()).count();
        score += hits as f32;
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_splits_into_decoded_segments_and_query() {
        let (segs, q) = split_request("/api/rooms/room_1/sync?since=42&limit=200");
        assert_eq!(segs, vec!["api", "rooms", "room_1", "sync"]);
        assert_eq!(q.get("since").map(String::as_str), Some("42"));
        assert_eq!(q.get("limit").map(String::as_str), Some("200"));
    }

    #[test]
    fn a_path_without_a_query_has_no_params() {
        let (segs, q) = split_request("/api/health");
        assert_eq!(segs, vec!["api", "health"]);
        assert!(q.is_empty());
    }

    #[test]
    fn percent_escapes_decode_and_bad_ones_pass_through() {
        // The apple the API spec quotes, encoded the way `encode_segment` does.
        assert_eq!(percent_decode("%F0%9F%8D%8E"), "🍎");
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("100%25"), "100%");
        // A lone or malformed escape is literal, not an error.
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        // `+` is a space in query values.
        assert_eq!(percent_decode("hello+world"), "hello world");
    }

    #[test]
    fn an_encoded_segment_round_trips_through_the_client_encoder() {
        // The router decodes what `encode_segment` encoded; the pair must
        // agree or emoticon codes with reserved characters break.
        for original in ["🍎", "a/b", "100%", "a b", "msg_1749_4cfe.x~"] {
            assert_eq!(
                percent_decode(&crate::api::encode_segment(original)),
                original
            );
        }
    }

    // --- anti-drift: every local row must decode through the wire types ----

    const OWNER: &str = "0x742d35cc6634c0532925a3b8d31ce5bb1c6e6b22";

    fn room_id() -> String {
        format!("room_note_{OWNER}")
    }

    fn body(content: &str) -> Value {
        json!({
            "content": content,
            "msgHash": "a".repeat(64),
            "isEncrypted": false,
            "encVer": 1,
            "keyVersion": 1,
        })
    }

    #[test]
    fn a_local_message_row_is_a_wire_message() {
        let row = message_row(
            &body("hello"),
            "msg_1_00aabbcc",
            &room_id(),
            OWNER,
            7,
            1_700_000_000_000,
        );
        let msg: crate::api::Message = serde_json::from_value(row).expect("wire Message shape");
        assert_eq!(msg.content, "hello");
        assert_eq!(msg.msg_serial, 7);
        assert_eq!(msg.msg_type, "add");
        assert!(!msg.is_deleted);
        assert_eq!(msg.sender_address.as_str(), OWNER);
    }

    #[test]
    fn an_agent_body_lands_in_the_same_row_shape() {
        // AgentReplyBody carries `text` where MessageBody carries `content`.
        let agent_body = json!({ "text": "the answer", "msgHash": "b".repeat(64) });
        let row = message_row(&agent_body, "msg_2_00aabbcc", &room_id(), OWNER, 8, 0);
        let msg: crate::api::Message = serde_json::from_value(row).unwrap();
        assert_eq!(msg.content, "the answer");
    }

    #[test]
    fn edits_keep_identity_and_bump_only_the_serial() {
        let mut row = message_row(&body("v1"), "msg_3_00aabbcc", &room_id(), OWNER, 1, 1000);
        apply_edit(&mut row, &body("v2"), 5, 2000);
        let msg: crate::api::Message = serde_json::from_value(row).unwrap();
        assert_eq!(msg.content, "v2");
        assert_eq!(msg.msg_serial, 5);
        assert_eq!(
            msg.message_timestamp, 1000,
            "an edit must not move the message"
        );
        assert!(msg.is_edited());
        assert_eq!(msg.kind(), crate::api::MsgKind::Edit);
    }

    #[test]
    fn a_tombstone_clears_content_and_folds_as_a_delete() {
        let mut row = message_row(
            &body("secret"),
            "msg_4_00aabbcc",
            &room_id(),
            OWNER,
            1,
            1000,
        );
        apply_tombstone(&mut row, 9);
        let msg: crate::api::Message = serde_json::from_value(row).unwrap();
        assert!(msg.is_deleted);
        assert!(
            msg.content.is_empty(),
            "a deleted row must not keep its text"
        );
        assert_eq!(msg.kind(), crate::api::MsgKind::Delete);
        assert_eq!(msg.msg_serial, 9);
    }

    #[test]
    fn reaction_and_purge_rows_are_wire_messages_too() {
        let react = reaction_row(
            true,
            "msg_5_00aabbcc",
            &room_id(),
            OWNER,
            "msg_1_00aabbcc",
            "🍎",
            3,
            0,
        );
        let msg: crate::api::Message = serde_json::from_value(react).unwrap();
        assert_eq!(msg.kind(), crate::api::MsgKind::EmoticonAdd);
        assert_eq!(msg.emoticon_code.as_deref(), Some("🍎"));

        let purge = purge_marker("msg_6_00aabbcc", &room_id(), OWNER, 4, 0);
        let msg: crate::api::Message = serde_json::from_value(purge).unwrap();
        assert_eq!(msg.kind(), crate::api::MsgKind::DeleteAll);
    }

    #[test]
    fn synthesized_rooms_are_wire_rooms_with_members() {
        let user = json!({ "walletAddress": OWNER, "username": "alice" });
        let agent = format!("0xa9e5{}", &OWNER[6..]); // shape only — any address
        let rooms = static_rooms(
            OWNER,
            &agent,
            &user,
            RoomExtras::default(),
            RoomExtras::default(),
        );
        let rooms: Vec<crate::api::RoomWithMembers> =
            serde_json::from_value(rooms).expect("RoomWithMembers shapes");
        assert_eq!(rooms.len(), 2);
        assert_eq!(rooms[0].room.kind, "note");
        assert_eq!(rooms[1].room.kind, "jarvis");
        assert_eq!(rooms[0].room.id.as_str(), room_id());
        assert_eq!(rooms[0].members.len(), 1);
        assert_eq!(
            rooms[1].members.len(),
            2,
            "Jarvis holds the owner and the agent"
        );
        // The pinning logic must recognise them as the viewer's own.
        let viewer = pocketskynet_core::WalletAddress::new(OWNER).unwrap();
        assert!(crate::rooms::mine(&rooms[0], &viewer).is_some());
    }

    #[test]
    fn minted_ids_pass_the_wire_grammar() {
        let id = mint_message_id(1_700_000_000_000, [0x4c, 0xfe, 0x1c, 0x4c]);
        assert!(pocketskynet_core::MessageId::new(&id).is_ok(), "{id}");
        assert_eq!(id, "msg_1700000000000_4cfe1c4c");
    }

    #[test]
    fn serial_keys_sort_like_serials() {
        // String order must equal numeric order, or paging breaks silently.
        let keys: Vec<String> = [1, 9, 10, 99, 100, 5000]
            .iter()
            .map(|s| message_key("room_x", *s))
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
        let (from, to) = room_range_after("room_x", 9);
        assert!(from > message_key("room_x", 9));
        assert!(from <= message_key("room_x", 10));
        assert!(to > message_key("room_x", 999_999));
    }

    #[test]
    fn iso8601_matches_known_instants() {
        assert_eq!(iso8601_ms(0), "1970-01-01T00:00:00.000Z");
        // 2024-03-01T12:30:45.678Z
        assert_eq!(iso8601_ms(1_709_296_245_678), "2024-03-01T12:30:45.678Z");
        // A leap day.
        assert_eq!(iso8601_ms(1_709_164_800_000), "2024-02-29T00:00:00.000Z");
        // And the client's own parser reads it back to the millisecond.
        assert_eq!(
            crate::format::parse_iso8601_ms(&iso8601_ms(1_709_296_245_678)),
            Some(1_709_296_245_678)
        );
    }

    #[test]
    fn history_hides_what_the_server_hides() {
        let room = room_id();
        let mut rows = vec![
            message_row(&body("root"), "m1", &room, OWNER, 1, 100),
            message_row(&body("dead"), "m2", &room, OWNER, 2, 200),
            reaction_row(true, "m3", &room, OWNER, "m1", "🍎", 3, 300),
            message_row(&body("reply"), "m4", &room, OWNER, 4, 400),
            message_row(&body("newer"), "m5", &room, OWNER, 5, 500),
        ];
        apply_tombstone(&mut rows[1], 6);
        rows[3]["parentMessageId"] = json!("m1");

        let page = history_page(&rows, None, 50);
        let ids: Vec<&str> = page.iter().filter_map(|r| r["id"].as_str()).collect();
        assert_eq!(
            ids,
            vec!["m1", "m5"],
            "tombstones, events and replies stay out"
        );
        // The hidden reply is summarised on its root.
        assert_eq!(page[0]["replyCount"], json!(1));
        assert_eq!(page[0]["lastReplyAt"], json!(400));

        // `before` pages on the timestamp, `limit` keeps the newest.
        let older = history_page(&rows, Some(500), 50);
        assert_eq!(older.len(), 1);
        assert_eq!(older[0]["id"], json!("m1"));
        let capped = history_page(&rows, None, 1);
        assert_eq!(capped[0]["id"], json!("m5"), "the newest survives a cap");
    }

    #[test]
    fn editing_an_old_message_does_not_strand_newer_ones() {
        let room = room_id();
        let mut rows: Vec<Value> = (1..=3)
            .map(|s| {
                message_row(
                    &body(&format!("m{s}")),
                    &format!("m{s}"),
                    &room,
                    OWNER,
                    s,
                    s * 100,
                )
            })
            .collect();
        // Edit the *oldest*: it gets a fresh serial (4) but keeps ts 100, so
        // in serial-key order it now sits after the genuinely newer rows.
        apply_edit(&mut rows[0], &body("m1-edited"), 4, 999);
        rows.rotate_left(1); // the store's key order after the edit: m2, m3, m1

        // A capped first page must keep the chronologically newest rows, not
        // the highest serials — or m3 would silently vanish from the room.
        let page = history_page(&rows, None, 2);
        let ids: Vec<&str> = page.iter().filter_map(|r| r["id"].as_str()).collect();
        assert_eq!(ids, vec!["m2", "m3"]);

        // And the timestamp cursor still reaches the edited old message.
        let older = history_page(&rows, Some(200), 2);
        let ids: Vec<&str> = older.iter().filter_map(|r| r["id"].as_str()).collect();
        assert_eq!(ids, vec!["m1"]);
    }

    #[test]
    fn a_thread_answers_from_the_root_or_any_reply() {
        let room = room_id();
        let mut rows = vec![
            message_row(&body("root"), "m1", &room, OWNER, 1, 100),
            message_row(&body("reply"), "m2", &room, OWNER, 2, 200),
            message_row(&body("other"), "m3", &room, OWNER, 3, 300),
        ];
        rows[1]["parentMessageId"] = json!("m1");
        for id in ["m1", "m2"] {
            let thread = thread_of(&rows, id);
            let ids: Vec<&str> = thread.iter().filter_map(|r| r["id"].as_str()).collect();
            assert_eq!(ids, vec!["m1", "m2"], "asked via {id}");
        }
        assert!(thread_of(&rows, "m9").is_empty());
    }

    #[test]
    fn reactions_fold_adds_and_removes_in_order() {
        let room = room_id();
        let rows = vec![
            reaction_row(true, "e1", &room, OWNER, "m1", "🍎", 1, 0),
            reaction_row(
                true,
                "e2",
                &room,
                "0xbbbb35cc6634c0532925a3b8d31ce5bb1c6e6b22",
                "m1",
                "🍎",
                2,
                0,
            ),
            reaction_row(false, "e3", &room, OWNER, "m1", "🍎", 3, 0),
            reaction_row(true, "e4", &room, OWNER, "m2", "🔥", 4, 0),
        ];
        let aggs = emoticon_aggregates(&rows, "m1");
        assert_eq!(aggs.len(), 1);
        assert_eq!(aggs[0]["emoticonCode"], json!("🍎"));
        assert_eq!(aggs[0]["count"], json!(1), "one add was taken back");
        // And the shape is the wire shape.
        let _: Vec<crate::api::EmoticonAggregation> =
            serde_json::from_value(Value::Array(aggs)).unwrap();
    }

    #[test]
    fn sync_pages_cap_and_report_more() {
        let room = room_id();
        let rows: Vec<Value> = (1..=(SYNC_PAGE as i64 + 5))
            .map(|s| message_row(&body("x"), &format!("m{s}"), &room, OWNER, s, s))
            .collect();
        let (page, has_more) = sync_page(rows.clone());
        assert_eq!(page.len(), SYNC_PAGE);
        assert!(has_more);
        let (page, has_more) = sync_page(rows[..10].to_vec());
        assert_eq!(page.len(), 10);
        assert!(!has_more);
    }

    #[test]
    fn tags_and_terms_come_apart_and_score_sensibly() {
        let (tags, terms) = parse_query("#home spare key #Doors");
        assert_eq!(tags, vec!["home", "doors"]);
        assert_eq!(terms, vec!["spare", "key"]);

        assert_eq!(
            extract_tags("the key hangs by the #door, #door again #Home"),
            vec!["door", "home"]
        );

        let terms = vec!["key".to_owned()];
        assert!(search_score("the key by the door", &terms) > 0.0);
        assert_eq!(search_score("nothing here", &terms), 0.0);
        assert!(
            search_score("key key key", &terms) > search_score("key", &terms),
            "more hits, higher score"
        );
        // No terms means browse: everything matches flat.
        assert_eq!(search_score("anything", &[]), 1.0);
    }
}
