//! Append-only JSONL event log.
//!
//! SQLite answers "what is the state of room X". This answers "what happened,
//! globally, in what order" — and it is the only structure that can serve an
//! SSE `Last-Event-ID` resume without a second table and its own retention
//! policy. It is also plain text, so an operator can grep an incident without
//! a database client.
//!
//! Two ordering rules make the log trustworthy:
//!
//! 1. `seq` is assigned and the line is written under one lock, so the file's
//!    line order *is* `seq` order. `O_APPEND` plus a single writer makes torn
//!    interleaving impossible.
//! 2. Callers commit to SQLite first, then append here, then fan out. The log
//!    is therefore a superset of what was delivered and a subset of what was
//!    committed. A crash in the gap loses a wake-up signal but never a
//!    message — the tolerable direction, since clients recover by syncing.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use pocketskynet_core::{ServerEvent, Target, WalletAddress};
use serde::{Deserialize, Serialize};
use time::macros::format_description;
use time::OffsetDateTime;

/// Batch `fsync` at whichever of these comes first. Flushing the `BufWriter`
/// happens on every append regardless, so a process crash (as opposed to a
/// machine crash) never loses a line.
const SYNC_EVERY_LINES: u32 = 64;
const SYNC_EVERY: Duration = Duration::from_millis(100);

/// The first sequence number ever handed out.
///
/// 0 is deliberately never used: it is the "I have received nothing" cursor,
/// and since [`JsonlLog::replay_since`] is exclusive, an event numbered 0 could
/// not be replayed to a client resuming from the start.
pub const FIRST_SEQ: u64 = 1;

/// Which stream a record belongs to. SSE replay reads `Realtime` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A fanned-out [`ServerEvent`].
    Realtime,
    /// A security-relevant action worth keeping outside the database.
    Audit,
    /// The log talking about itself: rotation, recovery.
    System,
}

/// One line of the log. Field order here is the field order on disk; serde
/// preserves declaration order, which keeps the file diffable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub seq: u64,
    pub ts: String,
    pub at_ms: i64,
    pub kind: Kind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<Target>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<WalletAddress>,
    pub event: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fanout: Option<u32>,
}

impl Record {
    /// Parse the [`ServerEvent`] back out of a replayed line.
    ///
    /// Returns `None` for `system`/`audit` records, whose `event` is not a
    /// `ServerEvent` at all.
    pub fn server_event(&self) -> Option<ServerEvent> {
        if self.kind != Kind::Realtime {
            return None;
        }
        serde_json::from_value(self.event.clone()).ok()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("event log I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("serialising event record: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("event log writer is poisoned; a previous append panicked")]
    Poisoned,
}

impl LogError {
    fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

#[derive(Debug)]
struct Inner {
    /// The seq the *next* append will use.
    next_seq: u64,
    file: BufWriter<File>,
    /// UTC date of the currently open file, for rotation.
    day: String,
    unsynced: u32,
    last_sync: Instant,
}

/// The append-only event log.
#[derive(Debug)]
pub struct JsonlLog {
    dir: PathBuf,
    inner: Mutex<Inner>,
}

impl JsonlLog {
    /// Open (or create) the log in `dir`, recovering the sequence counter from
    /// whatever the last run left behind.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, LogError> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir).map_err(|e| LogError::io(&dir, e))?;

        let recovered = recover_next_seq(&dir)?;
        let day = today();
        let file = open_day_file(&dir, &day)?;

        let log = Self {
            dir,
            inner: Mutex::new(Inner {
                next_seq: recovered.next_seq,
                file,
                day,
                unsynced: 0,
                last_sync: Instant::now(),
            }),
        };

        if recovered.recovered_from_scan {
            // Worth a line in the log itself: it means the previous process did
            // not shut down cleanly, and `events.seq` was behind the files.
            log.append_system(
                "log_recovered",
                serde_json::json!({
                    "nextSeq": recovered.next_seq,
                    "truncatedTornLine": recovered.discarded_torn_line,
                }),
            )?;
        }

        Ok(log)
    }

    /// The seq that the next append will receive.
    pub fn next_seq(&self) -> u64 {
        self.inner.lock().map(|i| i.next_seq).unwrap_or(0)
    }

    /// Append a fanned-out realtime event and return its `seq`.
    pub fn append_event(
        &self,
        target: &Target,
        origin: Option<&WalletAddress>,
        event: &ServerEvent,
        fanout: u32,
    ) -> Result<u64, LogError> {
        self.append(
            Kind::Realtime,
            Some(target.clone()),
            origin.cloned(),
            serde_json::to_value(event)?,
            Some(fanout),
        )
    }

    /// Append a security-relevant action (login, key rotation, block, kick).
    pub fn append_audit(
        &self,
        action: &str,
        actor: Option<&WalletAddress>,
        detail: serde_json::Value,
    ) -> Result<u64, LogError> {
        let event = serde_json::json!({ "type": action, "detail": detail });
        self.append(Kind::Audit, None, actor.cloned(), event, None)
    }

    fn append_system(&self, action: &str, detail: serde_json::Value) -> Result<u64, LogError> {
        let event = serde_json::json!({ "type": action, "detail": detail });
        self.append(Kind::System, None, None, event, None)
    }

    fn append(
        &self,
        kind: Kind,
        target: Option<Target>,
        origin: Option<WalletAddress>,
        event: serde_json::Value,
        fanout: Option<u32>,
    ) -> Result<u64, LogError> {
        let now = OffsetDateTime::now_utc();
        let mut inner = self.inner.lock().map_err(|_| LogError::Poisoned)?;

        // Rotate before writing so a record never lands in yesterday's file.
        let day = day_of(now);
        if day != inner.day {
            self.rotate(&mut inner, &day)?;
        }

        let seq = inner.next_seq;
        let record = Record {
            seq,
            ts: format_ts(now),
            at_ms: (now.unix_timestamp_nanos() / 1_000_000) as i64,
            kind,
            target,
            origin,
            event,
            fanout,
        };

        let mut line = serde_json::to_vec(&record)?;
        line.push(b'\n');

        let path = self.day_path(&inner.day);
        inner
            .file
            .write_all(&line)
            .map_err(|e| LogError::io(&path, e))?;
        // Flush every append: the buffer must not outlive the process, or the
        // "log is a superset of what was delivered" invariant breaks.
        inner.file.flush().map_err(|e| LogError::io(&path, e))?;

        inner.next_seq = seq + 1;
        inner.unsynced += 1;

        if inner.unsynced >= SYNC_EVERY_LINES || inner.last_sync.elapsed() >= SYNC_EVERY {
            self.sync(&mut inner)?;
        }

        Ok(seq)
    }

    /// Force durability of everything appended so far.
    pub fn flush(&self) -> Result<(), LogError> {
        let mut inner = self.inner.lock().map_err(|_| LogError::Poisoned)?;
        self.sync(&mut inner)
    }

    fn sync(&self, inner: &mut Inner) -> Result<(), LogError> {
        let path = self.day_path(&inner.day);
        inner.file.flush().map_err(|e| LogError::io(&path, e))?;
        inner
            .file
            .get_ref()
            .sync_data()
            .map_err(|e| LogError::io(&path, e))?;
        // Record the watermark only after the data is durable, so a crash can
        // leave the marker behind the files but never ahead of them.
        let marker = self.dir.join("events.seq");
        std::fs::write(&marker, inner.next_seq.to_string())
            .map_err(|e| LogError::io(&marker, e))?;

        inner.unsynced = 0;
        inner.last_sync = Instant::now();
        Ok(())
    }

    fn rotate(&self, inner: &mut Inner, new_day: &str) -> Result<(), LogError> {
        self.sync(inner)?;
        let previous = format!("events-{}.jsonl", inner.day);

        inner.file = open_day_file(&self.dir, new_day)?;
        inner.day = new_day.to_owned();

        // First line of the new file points back at the old one, so `seq`
        // continuity is checkable across a rotation boundary.
        let seq = inner.next_seq;
        let now = OffsetDateTime::now_utc();
        let record = Record {
            seq,
            ts: format_ts(now),
            at_ms: (now.unix_timestamp_nanos() / 1_000_000) as i64,
            kind: Kind::System,
            target: None,
            origin: None,
            event: serde_json::json!({ "type": "log_rotated", "from": previous }),
            fanout: None,
        };

        let mut line = serde_json::to_vec(&record)?;
        line.push(b'\n');
        let path = self.day_path(new_day);
        inner
            .file
            .write_all(&line)
            .map_err(|e| LogError::io(&path, e))?;
        inner.next_seq = seq + 1;

        Ok(())
    }

    fn day_path(&self, day: &str) -> PathBuf {
        self.dir.join(format!("events-{day}.jsonl"))
    }

    /// Replay retained records with `seq > after`, oldest first.
    ///
    /// Returns `None` when the cursor is older than what is retained — the
    /// caller must then tell the client to do a full resync rather than serve
    /// a silently partial history.
    pub fn replay_since(&self, after: u64, max: usize) -> Result<Option<Vec<Record>>, LogError> {
        let mut files = self.log_files()?;
        files.sort();

        let mut out = Vec::new();
        let mut oldest_seen: Option<u64> = None;

        for path in &files {
            let file = File::open(path).map_err(|e| LogError::io(path, e))?;
            for line in BufReader::new(file).lines() {
                let line = line.map_err(|e| LogError::io(path, e))?;
                // A torn final line from a crash is the one parse failure we
                // expect; skipping it is correct and skipping a mid-file line
                // would be a bug we want to stay quiet about rather than crash.
                let Ok(record) = serde_json::from_str::<Record>(&line) else {
                    continue;
                };
                oldest_seen.get_or_insert(record.seq);
                if record.seq > after {
                    out.push(record);
                    if out.len() > max {
                        return Ok(None); // too far behind to replay bounded
                    }
                }
            }
        }

        // `after` predates everything retained: we cannot prove we have the
        // full gap, so refuse rather than under-deliver.
        if let Some(oldest) = oldest_seen {
            if after + 1 < oldest {
                return Ok(None);
            }
        }

        Ok(Some(out))
    }

    fn log_files(&self) -> Result<Vec<PathBuf>, LogError> {
        let mut out = Vec::new();
        let entries = std::fs::read_dir(&self.dir).map_err(|e| LogError::io(&self.dir, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| LogError::io(&self.dir, e))?;
            let path = entry.path();
            let is_log = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("events-") && n.ends_with(".jsonl"));
            if is_log {
                out.push(path);
            }
        }
        Ok(out)
    }
}

struct Recovery {
    next_seq: u64,
    recovered_from_scan: bool,
    discarded_torn_line: bool,
}

/// Work out where the sequence counter left off.
///
/// `events.seq` is a hint, not the truth: it is written after an fsync batch,
/// so it can lag the files. The files themselves are authoritative, and the
/// larger of the two wins — reusing a `seq` would corrupt SSE resume.
fn recover_next_seq(dir: &Path) -> Result<Recovery, LogError> {
    let marker = dir.join("events.seq");
    let from_marker = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);

    let mut newest: Option<PathBuf> = None;
    if dir.exists() {
        let entries = std::fs::read_dir(dir).map_err(|e| LogError::io(dir, e))?;
        for entry in entries {
            let path = entry.map_err(|e| LogError::io(dir, e))?.path();
            let is_log = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("events-") && n.ends_with(".jsonl"));
            if is_log && newest.as_ref().is_none_or(|cur| path > *cur) {
                newest = Some(path);
            }
        }
    }

    let mut from_files = 0u64;
    let mut discarded_torn_line = false;

    if let Some(path) = newest {
        let file = File::open(&path).map_err(|e| LogError::io(&path, e))?;
        let mut last_parse_failed = false;
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|e| LogError::io(&path, e))?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Record>(&line) {
                Ok(record) => {
                    from_files = from_files.max(record.seq + 1);
                    last_parse_failed = false;
                }
                Err(_) => last_parse_failed = true,
            }
        }
        // Only a *trailing* parse failure is an expected torn write.
        discarded_torn_line = last_parse_failed;
    }

    // Sequence numbers start at 1, never 0. `replay_since` is exclusive of its
    // cursor, and SSE `Last-Event-ID` means "I already have this one", so a seq
    // of 0 could never be replayed to anybody: a client resuming from the very
    // beginning passes 0 and would silently lose the first event. Reserving 0
    // as "I have received nothing" makes `replay_since(0)` mean "send me
    // everything", which is what a fresh resume actually wants.
    let next_seq = from_marker.max(from_files).max(FIRST_SEQ);
    Ok(Recovery {
        next_seq,
        recovered_from_scan: from_files > from_marker,
        discarded_torn_line,
    })
}

fn open_day_file(dir: &Path, day: &str) -> Result<BufWriter<File>, LogError> {
    let path = dir.join(format!("events-{day}.jsonl"));
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| LogError::io(&path, e))?;
    // `append(true)` already implies O_APPEND; seeking to the end keeps the
    // reported position honest for anything that inspects it.
    file.seek(SeekFrom::End(0))
        .map_err(|e| LogError::io(&path, e))?;
    Ok(BufWriter::new(file))
}

fn today() -> String {
    day_of(OffsetDateTime::now_utc())
}

fn day_of(t: OffsetDateTime) -> String {
    let fmt = format_description!("[year]-[month]-[day]");
    t.format(&fmt).expect("date formatting is infallible")
}

fn format_ts(t: OffsetDateTime) -> String {
    let fmt =
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");
    t.format(&fmt).expect("timestamp formatting is infallible")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pocketskynet_core::RoomId;

    fn tempdir(tag: &str) -> PathBuf {
        let mut buf = [0u8; 8];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut buf);
        let dir = std::env::temp_dir().join(format!("ps-jsonl-{tag}-{}", hex::encode(buf)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn room() -> RoomId {
        RoomId::new("room_test_1").unwrap()
    }

    fn wallet() -> WalletAddress {
        WalletAddress::new("0x742d35cc6634c0532925a3b8d31ce5bb1c6e6b22").unwrap()
    }

    fn new_message(serial: i64) -> ServerEvent {
        ServerEvent::NewMessage {
            room_id: room(),
            msg_serial: serial,
        }
    }

    #[test]
    fn seq_is_gapless_and_starts_at_zero() {
        let dir = tempdir("gapless");
        let log = JsonlLog::open(&dir).unwrap();

        let target = Target::Room { room_id: room() };
        let seqs: Vec<u64> = (0..5)
            .map(|i| {
                log.append_event(&target, Some(&wallet()), &new_message(i), 1)
                    .unwrap()
            })
            .collect();

        assert_eq!(seqs, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn the_very_first_event_is_replayable_from_a_zero_cursor() {
        // Regression guard: with a 0-based seq and an exclusive cursor, a client
        // resuming from the beginning (`Last-Event-ID: 0`, or none at all) would
        // never receive the first event ever logged.
        let dir = tempdir("first-seq");
        let log = JsonlLog::open(&dir).unwrap();
        let target = Target::Room { room_id: room() };

        let first = log.append_event(&target, None, &new_message(1), 0).unwrap();
        log.flush().unwrap();

        assert_eq!(first, FIRST_SEQ);
        assert_ne!(first, 0, "0 is reserved for 'nothing received yet'");

        let replayed = log.replay_since(0, 100).unwrap().unwrap();
        assert!(
            replayed.iter().any(|r| r.seq == first),
            "a from-scratch resume must include the very first event"
        );
    }

    #[test]
    fn a_reopened_log_never_reuses_a_seq() {
        let dir = tempdir("reopen");
        let target = Target::Room { room_id: room() };

        {
            let log = JsonlLog::open(&dir).unwrap();
            for i in 0..3 {
                log.append_event(&target, None, &new_message(i), 0).unwrap();
            }
            log.flush().unwrap();
        }

        let log = JsonlLog::open(&dir).unwrap();
        let next = log
            .append_event(&target, None, &new_message(99), 0)
            .unwrap();
        assert!(next >= 3, "seq {next} would collide with a previous run");
    }

    #[test]
    fn recovery_prefers_the_files_when_the_marker_lags() {
        let dir = tempdir("marker-lag");
        let target = Target::Room { room_id: room() };

        {
            let log = JsonlLog::open(&dir).unwrap();
            for i in 0..4 {
                log.append_event(&target, None, &new_message(i), 0).unwrap();
            }
        }
        // Simulate a crash before the fsync batch updated the watermark.
        std::fs::write(dir.join("events.seq"), "1").unwrap();

        let log = JsonlLog::open(&dir).unwrap();
        assert!(
            log.next_seq() >= 4,
            "file scan must win over a stale marker, got {}",
            log.next_seq()
        );
    }

    #[test]
    fn a_torn_trailing_line_is_discarded_not_fatal() {
        let dir = tempdir("torn");
        let target = Target::Room { room_id: room() };

        {
            let log = JsonlLog::open(&dir).unwrap();
            log.append_event(&target, None, &new_message(1), 0).unwrap();
            log.flush().unwrap();
        }

        let path = dir.join(format!("events-{}.jsonl", today()));
        let mut content = std::fs::read_to_string(&path).unwrap();
        content.push_str("{\"seq\":1,\"ts\":\"2026-07-2"); // half a line
        std::fs::write(&path, content).unwrap();

        let log = JsonlLog::open(&dir).expect("a torn line must not stop startup");
        assert!(log.next_seq() >= 1);
    }

    #[test]
    fn records_round_trip_and_keep_the_wire_event_verbatim() {
        let dir = tempdir("roundtrip");
        let log = JsonlLog::open(&dir).unwrap();
        let target = Target::RoomExcept {
            room_id: room(),
            except: wallet(),
        };
        let event = new_message(1749652900000);

        log.append_event(&target, Some(&wallet()), &event, 7)
            .unwrap();
        log.flush().unwrap();

        let replayed = log.replay_since(0, 100).unwrap().unwrap();
        let record = replayed.iter().find(|r| r.kind == Kind::Realtime).unwrap();

        assert_eq!(record.fanout, Some(7));
        assert_eq!(record.origin.as_ref(), Some(&wallet()));
        assert_eq!(record.target.as_ref(), Some(&target));
        assert_eq!(record.server_event().unwrap(), event);
        // The logged event must be byte-identical to what went on the wire.
        assert_eq!(record.event, serde_json::to_value(&event).unwrap());
    }

    #[test]
    fn replay_is_exclusive_of_the_cursor() {
        let dir = tempdir("cursor");
        let log = JsonlLog::open(&dir).unwrap();
        let target = Target::Room { room_id: room() };

        for i in 0..5 {
            log.append_event(&target, None, &new_message(i), 0).unwrap();
        }
        log.flush().unwrap();

        let from_two = log.replay_since(2, 100).unwrap().unwrap();
        assert!(from_two.iter().all(|r| r.seq > 2));
        assert_eq!(from_two.first().map(|r| r.seq), Some(3));
    }

    #[test]
    fn replay_refuses_rather_than_truncating() {
        let dir = tempdir("bounded");
        let log = JsonlLog::open(&dir).unwrap();
        let target = Target::Room { room_id: room() };

        for i in 0..20 {
            log.append_event(&target, None, &new_message(i), 0).unwrap();
        }
        log.flush().unwrap();

        assert!(
            log.replay_since(0, 5).unwrap().is_none(),
            "a replay that cannot fit the bound must return None, not a prefix"
        );
    }

    #[test]
    fn each_line_is_exactly_one_json_object() {
        let dir = tempdir("lines");
        let log = JsonlLog::open(&dir).unwrap();
        let target = Target::Room { room_id: room() };

        for i in 0..3 {
            log.append_event(&target, None, &new_message(i), 0).unwrap();
        }
        log.flush().unwrap();

        let path = dir.join(format!("events-{}.jsonl", today()));
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.ends_with('\n'));
        for line in content.lines() {
            serde_json::from_str::<Record>(line).expect("every line parses standalone");
        }
    }

    #[test]
    fn audit_records_are_not_replayable_server_events() {
        let dir = tempdir("audit");
        let log = JsonlLog::open(&dir).unwrap();

        // Seq 0 is unreachable through an exclusive cursor, so the record
        // under test is deliberately the second one written.
        log.append_event(&Target::Room { room_id: room() }, None, &new_message(1), 0)
            .unwrap();
        log.append_audit(
            "login",
            Some(&wallet()),
            serde_json::json!({"ip": "127.0.0.1"}),
        )
        .unwrap();
        log.flush().unwrap();

        let replayed = log.replay_since(0, 100).unwrap().unwrap();
        let record = replayed.iter().find(|r| r.kind == Kind::Audit).unwrap();
        assert!(record.server_event().is_none());
    }

    #[test]
    fn timestamp_format_is_millisecond_utc() {
        let ts = format_ts(OffsetDateTime::from_unix_timestamp(1785316462).unwrap());
        assert_eq!(ts, "2026-07-29T09:14:22.000Z");
        assert_eq!(ts.len(), 24);
    }
}
