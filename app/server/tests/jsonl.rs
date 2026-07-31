//! The JSONL event log. Spec: `docs/REALTIME.md` §10.
//!
//! The log is the third view of the same events (after the WebSocket frame and
//! the SSE `data:` payload), and §10.4 makes its relationship to SQLite a
//! checkable invariant: replaying `kind:"realtime"` `new_message` records must
//! reproduce the per-room `max(msgSerial)` the database holds.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::*;
use serde_json::{json, Value};

/// Read every `.jsonl` file in the events directory, oldest file first, and
/// return one parsed record per line.
///
/// Parsing each line standalone is the point: a consumer tails this file with
/// `jq` or a log shipper and never sees the surrounding lines.
fn read_log(server: &TestServer) -> Vec<Value> {
    let dir = server.events_dir();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("the events directory {dir:?} must exist: {e}"))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    files.sort();

    let mut records = Vec::new();
    for file in files {
        let contents =
            std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("reading {file:?}: {e}"));
        for (n, line) in contents.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let record: Value = serde_json::from_str(line).unwrap_or_else(|e| {
                panic!(
                    "{file:?} line {}: every line must parse standalone ({e}): {line}",
                    n + 1
                )
            });
            assert!(
                record.is_object(),
                "{file:?} line {}: a record is a JSON object, got: {line}",
                n + 1
            );
            records.push(record);
        }
    }
    records
}

/// Poll the log until `ready` accepts it, or panic after `timeout`.
///
/// The writer flushes asynchronously, so a condition-poll is both faster than a
/// fixed sleep and immune to a slow machine.
async fn await_log<F>(
    server: &TestServer,
    timeout: Duration,
    label: &str,
    mut ready: F,
) -> Vec<Value>
where
    F: FnMut(&[Value]) -> bool,
{
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let records = read_log(server);
        if ready(&records) {
            return records;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "timed out after {timeout:?} waiting for {label}; the log holds {} records",
                records.len()
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Wait until the log holds at least `count` records.
async fn await_records(server: &TestServer, count: usize) -> Vec<Value> {
    await_log(
        server,
        Duration::from_secs(10),
        &format!("at least {count} JSONL records"),
        |records| records.len() >= count,
    )
    .await
}

fn realtime_records(records: &[Value]) -> Vec<&Value> {
    records
        .iter()
        .filter(|r| r.get("kind").and_then(Value::as_str) == Some("realtime"))
        .collect()
}

fn event_type(record: &Value) -> String {
    record
        .get("event")
        .and_then(|e| e.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

// --- structure ------------------------------------------------------------

#[tokio::test]
async fn the_event_log_exists_after_any_activity() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "logged").await;
    send_message(&alice.api, &room, "written to the log").await;

    let records = await_records(&server, 1).await;

    assert!(!records.is_empty());
    assert!(
        server.events_dir().is_dir(),
        "REALTIME §10.1: data/events/ holds the daily-rotated log"
    );
}

#[tokio::test]
async fn every_record_carries_the_specified_fields() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "shapes").await;
    add_member(&alice.api, &bob, &room).await;
    send_message(&alice.api, &room, "one").await;

    let records = await_records(&server, 1).await;

    for record in &records {
        expect_keys(record, &["seq", "ts", "at_ms", "kind", "event"]);
        assert!(record["seq"].as_u64().is_some(), "seq is a u64: {record}");
        assert!(
            record["at_ms"]
                .as_i64()
                .is_some_and(|ms| ms > 1_600_000_000_000),
            "at_ms is epoch millis: {record}"
        );
        let ts = record["ts"].as_str().unwrap_or_default();
        assert!(
            ts.ends_with('Z') && ts.contains('T'),
            "ts is RFC 3339 UTC: {record}"
        );
        let kind = record["kind"].as_str().unwrap_or_default();
        assert!(
            ["realtime", "audit", "system"].contains(&kind),
            "unknown kind `{kind}`: {record}"
        );
        assert!(
            record["event"]
                .get("type")
                .and_then(Value::as_str)
                .is_some(),
            "`event` is the exact tagged ServerEvent JSON: {record}"
        );
    }
}

#[tokio::test]
async fn realtime_records_name_their_target_and_origin() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "targets").await;
    add_member(&alice.api, &bob, &room).await;
    send_message(&alice.api, &room, "from alice").await;

    let records = await_records(&server, 1).await;
    let realtime = realtime_records(&records);
    assert!(
        !realtime.is_empty(),
        "sending a message publishes a realtime event"
    );

    let new_message = realtime
        .iter()
        .find(|r| event_type(r) == "new_message")
        .unwrap_or_else(|| panic!("no new_message record in: {realtime:?}"));

    let target = new_message
        .get("target")
        .unwrap_or_else(|| panic!("a realtime record names its target: {new_message}"));
    let tag = target.get("t").and_then(Value::as_str).unwrap_or_default();
    assert!(
        ["room", "user", "room_except"].contains(&tag),
        "unknown target tag `{tag}`: {target}"
    );

    // `origin` is what lets replay drop events from blocked senders.
    let origin = new_message.get("origin").and_then(Value::as_str);
    assert_eq!(
        origin,
        Some(alice.address.as_str()),
        "the originating wallet must be recorded, lowercase: {new_message}"
    );
}

#[tokio::test]
async fn the_logged_event_is_byte_identical_to_the_wire_form() {
    // One type, three encodings: the JSONL `event` object must deserialize as
    // the same `ServerEvent` the socket sends.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "wire form").await;
    let sent = send_message(&alice.api, &room, "identical").await;

    let records = await_records(&server, 1).await;
    let logged = records
        .iter()
        .find(|r| event_type(r) == "new_message")
        .unwrap_or_else(|| panic!("no new_message record: {records:?}"));

    assert_eq!(
        logged["event"],
        json!({
            "type": "new_message",
            "roomId": sent["roomId"],
            "msgSerial": sent["msgSerial"],
        }),
        "the logged event must match the documented camelCase wire shape"
    );
}

// --- ordering -------------------------------------------------------------

#[tokio::test]
async fn the_sequence_is_gapless_and_strictly_increasing() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "sequence").await;
    add_member(&alice.api, &bob, &room).await;
    for n in 0..10 {
        send_message(&alice.api, &room, &format!("message {n}")).await;
    }

    let records = await_records(&server, 10).await;

    let seqs: Vec<u64> = records
        .iter()
        .map(|r| {
            r["seq"]
                .as_u64()
                .unwrap_or_else(|| panic!("seq missing: {r}"))
        })
        .collect();
    assert!(!seqs.is_empty());
    for pair in seqs.windows(2) {
        assert_eq!(
            pair[1],
            pair[0] + 1,
            "§10.3: `seq` is gapless by construction; saw {} then {}",
            pair[0],
            pair[1]
        );
    }
}

#[tokio::test]
async fn a_sequence_number_is_never_reused() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "unique").await;
    let mut tasks = Vec::new();
    for n in 0..15 {
        let api = alice.api.clone();
        let room = room.clone();
        tasks.push(tokio::spawn(async move {
            send_message(&api, &room, &format!("concurrent {n}")).await;
        }));
    }
    for task in tasks {
        task.await.expect("send task");
    }

    let records = await_records(&server, 15).await;

    let seqs: Vec<u64> = records.iter().filter_map(|r| r["seq"].as_u64()).collect();
    let unique: std::collections::BTreeSet<u64> = seqs.iter().copied().collect();
    assert_eq!(
        unique.len(),
        seqs.len(),
        "concurrent publishes must not collide on a seq: {seqs:?}"
    );
}

#[tokio::test]
async fn records_are_appended_in_timestamp_order() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ordering").await;
    for n in 0..6 {
        send_message(&alice.api, &room, &format!("message {n}")).await;
    }

    let records = await_records(&server, 6).await;

    let times: Vec<i64> = records.iter().filter_map(|r| r["at_ms"].as_i64()).collect();
    let mut sorted = times.clone();
    sorted.sort_unstable();
    assert_eq!(
        times, sorted,
        "seq order must not disagree with at_ms order"
    );
}

// --- the §10.4 consistency check -----------------------------------------

#[tokio::test]
async fn replaying_new_message_records_reproduces_the_per_room_max_serial() {
    // The production consistency check from REALTIME §10.4, run as an
    // integration assertion: the log and the database must agree.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;

    let mut rooms = Vec::new();
    for n in 0..3 {
        let room = create_room(&alice.api, &format!("room {n}")).await;
        add_member(&alice.api, &bob, &room).await;
        rooms.push(room);
    }

    // A mixed workload so serials advance for several different reasons.
    for (n, room) in rooms.iter().enumerate() {
        for m in 0..(n + 2) {
            send_message(&alice.api, room, &format!("message {m}")).await;
        }
        let msg = send_message(&bob.api, room, "to be edited").await;
        alice
            .api
            .post(
                &format!("/api/messages/{}/emoticons", s(&msg, "id")),
                json!({ "emoticonCode": "🍎" }),
            )
            .await
            .expect_status(200);
        bob.api
            .patch(
                &format!("/api/messages/{}", s(&msg, "id")),
                json!({ "content": "edited", "msgHash": crypto::sha256_hex(b"edited") }),
            )
            .await
            .expect_status(200);
        alice
            .api
            .delete(&format!("/api/messages/{}", s(&msg, "id")))
            .await
            .expect_status(200);
    }

    // The authoritative per-room state, straight from SQLite via the API.
    let mut expected = std::collections::BTreeMap::new();
    for room in &rooms {
        expected.insert(room.clone(), latest_serial(&alice.api, room).await);
    }

    // Fold the log the way an SSE replay would.
    let records = await_log(
        &server,
        Duration::from_secs(10),
        "the log to catch up with the database",
        |records| {
            let replayed = replay_max_serials(records);
            expected
                .iter()
                .all(|(room, serial)| replayed.get(room) == Some(serial))
        },
    )
    .await;

    let replayed = replay_max_serials(&records);
    for (room, serial) in &expected {
        assert_eq!(
            replayed.get(room),
            Some(serial),
            "room {room}: the log says {:?} but SQLite says {serial}",
            replayed.get(room)
        );
    }
}

#[tokio::test]
async fn the_log_is_a_superset_of_what_was_delivered() {
    // §10.3: publish writes the JSONL line *before* broadcasting, so an event
    // can exist in the log with nobody listening — never the reverse.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "unlistened").await;
    let sent = send_message(&alice.api, &room, "nobody is connected").await;

    let records = await_records(&server, 1).await;

    let logged = records
        .iter()
        .filter(|r| event_type(r) == "new_message")
        .filter_map(|r| r["event"]["msgSerial"].as_i64())
        .collect::<Vec<_>>();
    assert!(
        logged.contains(&i(&sent, "msgSerial")),
        "the event is logged even with zero subscribers: {logged:?}"
    );
}

#[tokio::test]
async fn a_fanout_of_zero_is_recorded_rather_than_dropped() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "no listeners").await;
    send_message(&alice.api, &room, "into the void").await;

    let records = await_records(&server, 1).await;

    let new_message = records
        .iter()
        .find(|r| event_type(r) == "new_message")
        .unwrap_or_else(|| panic!("no new_message record: {records:?}"));
    if let Some(fanout) = new_message.get("fanout") {
        assert_eq!(
            fanout.as_u64(),
            Some(0),
            "nobody was connected, so fanout is the ops signal 0: {new_message}"
        );
    }
}

#[tokio::test]
async fn membership_events_reach_the_log_too() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "membership").await;

    alice
        .api
        .post(
            &format!("/api/rooms/{room}/invite"),
            json!({ "userAddress": bob.address }),
        )
        .await
        .expect_status(200);
    bob.api
        .post_empty(&format!("/api/invitations/{room}/accept"))
        .await
        .expect_status(200);

    let records = await_log(
        &server,
        Duration::from_secs(10),
        "an invitation_received record",
        |records| {
            records
                .iter()
                .any(|r| event_type(r) == "invitation_received")
        },
    )
    .await;

    let types: Vec<String> = records.iter().map(event_type).collect();
    assert!(
        types.contains(&"invitation_received".to_string()),
        "{types:?}"
    );
    assert!(
        types.contains(&"rooms_updated".to_string())
            || types.contains(&"member_removed".to_string()),
        "accepting an invitation refreshes the roster: {types:?}"
    );
}

#[tokio::test]
async fn each_line_is_compact_single_line_json() {
    // §10.2: no pretty-printing — a record must occupy exactly one line, or
    // `tail -f | jq` and the replay loader both break.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "compact").await;
    send_message(&alice.api, &room, "line\nwith\nnewlines").await;
    await_records(&server, 1).await;

    let dir = server.events_dir();
    for entry in std::fs::read_dir(&dir).expect("events dir").flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "jsonl") {
            let contents = std::fs::read_to_string(&path).expect("read log");
            for line in contents.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                assert!(
                    !line.contains("\n  ") && !line.starts_with(' '),
                    "{path:?}: records must be compact, not pretty-printed: {line}"
                );
                serde_json::from_str::<Value>(line)
                    .unwrap_or_else(|e| panic!("{path:?}: unparseable line ({e}): {line}"));
            }
        }
    }
}

/// Fold `kind:"realtime"` `new_message` records into `roomId -> max(msgSerial)`.
fn replay_max_serials(records: &[Value]) -> std::collections::BTreeMap<String, i64> {
    let mut max = std::collections::BTreeMap::new();
    for record in realtime_records(records) {
        if event_type(record) != "new_message" {
            continue;
        }
        let event = &record["event"];
        let (Some(room), Some(serial)) = (
            event.get("roomId").and_then(Value::as_str),
            event.get("msgSerial").and_then(Value::as_i64),
        ) else {
            continue;
        };
        let entry = max.entry(room.to_string()).or_insert(serial);
        *entry = (*entry).max(serial);
    }
    max
}
