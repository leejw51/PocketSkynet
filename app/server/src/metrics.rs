//! Transfer telemetry for the files dashboard — derived, in memory, gone at
//! restart.
//!
//! # Why counters and not a table
//!
//! The same reasoning presence went through (`docs/ROADMAP.md` §0a): a durable
//! record of every transfer is a log of who moved which bytes when, and
//! "how busy is this server right now?" does not need one. What the dashboard
//! wants is a handful of running totals and a feel for current throughput, and
//! both are answerable from memory. Nothing here survives a restart — which is
//! the point, and the dashboard says so rather than pretending otherwise.
//!
//! # What is measured, and where
//!
//! Two middlewares, applied in `routes/mod.rs` where the state is in scope:
//!
//! * [`track_uploads`] wraps the chunked-upload router and the single-shot
//!   attachment route. A chunk's bytes and wall time are recorded per request —
//!   the timer starts before the body is read, so the wire is inside it — and a
//!   *transfer* is counted when a session finishes (or a single-shot upload
//!   lands), so the count means "files that arrived", not "requests made".
//! * [`track_downloads`] wraps `GET /api/files/{id}/raw`. The response streams,
//!   so the bytes and the clock are carried by the body itself and recorded
//!   when the stream ends — however it ends. A partial transfer still moved
//!   bytes, and speed derived only from happy endings would flatter the server.
//!
//! Honesty note, worth keeping with the numbers: download time is the wire *as
//! this server observed it*, which includes the client's own backpressure. A
//! viewer letting a film buffer at its own pace reads as a slow transfer here.
//! Averages answer "what has this server been doing", not "what is the link
//! capable of" — the recent window is the better read on the latter.
//!
//! Bytes and milliseconds ride the wire as integers and the client divides,
//! so the API stays free of floating point and the arithmetic is testable.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use axum::extract::{Request, State};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::Response;
use futures_util::StreamExt;
use serde::Serialize;

use crate::AppState;

/// How far back "recent" reaches. Five minutes: long enough to smooth a gap
/// between chunks, short enough that the number still describes *now*.
const RECENT_WINDOW_MS: i64 = 5 * 60 * 1000;

/// The most samples the recent ring holds. At one sample per request this is
/// minutes of the busiest plausible traffic, and it bounds the memory the way
/// the window bounds the arithmetic.
const RECENT_CAP: usize = 512;

/// One finished measurement: some bytes moved in some milliseconds.
#[derive(Debug, Clone, Copy)]
struct Sample {
    at_ms: i64,
    bytes: u64,
    millis: u64,
    upload: bool,
}

/// Running totals for one direction.
#[derive(Debug, Default)]
struct Flow {
    /// Whole transfers: finished uploads, or download requests served.
    transfers: AtomicU64,
    bytes: AtomicU64,
    millis: AtomicU64,
}

impl Flow {
    fn view(&self) -> FlowView {
        FlowView {
            transfers: self.transfers.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            millis: self.millis.load(Ordering::Relaxed),
            recent_bytes: 0,
            recent_millis: 0,
        }
    }
}

/// One direction, on the wire. `bytes / millis` is the average since start;
/// `recentBytes / recentMillis` the same over the last five minutes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowView {
    pub transfers: u64,
    pub bytes: u64,
    pub millis: u64,
    pub recent_bytes: u64,
    pub recent_millis: u64,
}

/// Everything the dashboard's activity card shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityView {
    /// When these counters started counting — process start, not an epoch of
    /// any table. The client turns it into "since {time}".
    pub since_ms: i64,
    pub recent_window_ms: i64,
    pub uploads: FlowView,
    pub downloads: FlowView,
}

/// The counters. One per process, on [`AppState`].
#[derive(Debug)]
pub struct TransferMetrics {
    started_ms: i64,
    uploads: Flow,
    downloads: Flow,
    /// Completed measurements, newest at the back. A ring rather than a table:
    /// the window is what gives "recent" meaning, and anything older is
    /// already summed into the totals above.
    recent: Mutex<VecDeque<Sample>>,
}

impl Default for TransferMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl TransferMetrics {
    pub fn new() -> Self {
        Self {
            started_ms: crate::db::now_ms(),
            uploads: Flow::default(),
            downloads: Flow::default(),
            recent: Mutex::new(VecDeque::new()),
        }
    }

    /// Bytes received by an upload request. Does **not** count a transfer —
    /// a 4 GB file arrives as hundreds of chunks, and "uploads: 512" for one
    /// film would be a lie with a unit attached.
    pub fn record_upload_bytes(&self, bytes: u64, millis: u64) {
        self.record(crate::db::now_ms(), true, bytes, millis);
    }

    /// A whole upload landed: a finished session, or a single-shot post.
    pub fn upload_completed(&self) {
        self.uploads.transfers.fetch_add(1, Ordering::Relaxed);
    }

    /// One download request served to its end, along with what it moved.
    pub fn record_download(&self, bytes: u64, millis: u64) {
        self.downloads.transfers.fetch_add(1, Ordering::Relaxed);
        self.record(crate::db::now_ms(), false, bytes, millis);
    }

    fn record(&self, at_ms: i64, upload: bool, bytes: u64, millis: u64) {
        let flow = if upload {
            &self.uploads
        } else {
            &self.downloads
        };
        flow.bytes.fetch_add(bytes, Ordering::Relaxed);
        flow.millis.fetch_add(millis, Ordering::Relaxed);

        if let Ok(mut ring) = self.recent.lock() {
            if ring.len() == RECENT_CAP {
                ring.pop_front();
            }
            ring.push_back(Sample {
                at_ms,
                bytes,
                millis,
                upload,
            });
        }
    }

    pub fn snapshot(&self) -> ActivityView {
        self.snapshot_at(crate::db::now_ms())
    }

    fn snapshot_at(&self, now_ms: i64) -> ActivityView {
        let mut uploads = self.uploads.view();
        let mut downloads = self.downloads.view();

        if let Ok(mut ring) = self.recent.lock() {
            // Pruned at read time rather than on a timer: the window only has
            // to be true at the moment somebody looks.
            let horizon = now_ms - RECENT_WINDOW_MS;
            while ring.front().is_some_and(|s| s.at_ms < horizon) {
                ring.pop_front();
            }
            for sample in ring.iter() {
                let flow = if sample.upload {
                    &mut uploads
                } else {
                    &mut downloads
                };
                flow.recent_bytes += sample.bytes;
                flow.recent_millis += sample.millis;
            }
        }

        ActivityView {
            since_ms: self.started_ms,
            recent_window_ms: RECENT_WINDOW_MS,
            uploads,
            downloads,
        }
    }
}

// ------------------------------------------------------------ middleware ---

/// Meter the upload routes. Applied to the chunked-upload router and the
/// attachment router in `routes/mod.rs`; everything else those routers carry
/// (status polls, listings) falls through the `match` untouched.
///
/// The timer starts before `next` runs, and the body is read *inside* `next`
/// — that ordering is what puts the wire inside the measurement. The byte
/// count is the request's own `Content-Length`, which every chunk and every
/// single-shot upload carries; a request without one is not metered rather
/// than guessed at.
pub async fn track_uploads(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let declared = req
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);

    let start = Instant::now();
    let response = next.run(req).await;

    // Only what succeeded: a refused chunk moved nothing worth averaging.
    if response.status().is_success() {
        let elapsed = start.elapsed().as_millis() as u64;
        match method {
            // A chunk append (`PATCH /api/uploads/{id}`): bytes on the wire.
            Method::PATCH if declared > 0 => state.metrics.record_upload_bytes(declared, elapsed),
            // A finished session (`POST /api/uploads/{id}/finish`): one whole
            // transfer. Its bytes were already counted chunk by chunk.
            Method::POST if path.ends_with("/finish") => state.metrics.upload_completed(),
            // The single-shot route (`POST /api/rooms/{id}/files`): both at
            // once — the request is the whole file. The completion counts even
            // without a `Content-Length` to meter; a file arrived either way.
            Method::POST if path.ends_with("/files") => {
                if declared > 0 {
                    state.metrics.record_upload_bytes(declared, elapsed);
                }
                state.metrics.upload_completed();
            }
            _ => {}
        }
    }
    response
}

/// Meter attachment downloads. Applied to `files::media_router` only, and
/// within it only `GET …/raw` responses are wrapped.
///
/// A download's cost is paid while the body streams, long after this function
/// has returned — so the measurement rides the body: a guard moves into the
/// stream, counts every chunk it sees, and reports when it is dropped. Drop
/// is the one event every ending shares, so an aborted transfer still records
/// the bytes it managed.
pub async fn track_downloads(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let counts = req.method() == Method::GET && req.uri().path().ends_with("/raw");
    let start = Instant::now();
    let response = next.run(req).await;

    if !counts || !response.status().is_success() {
        return response;
    }

    let (parts, body) = response.into_parts();
    let mut guard = DownloadGuard {
        metrics: state.metrics.clone(),
        start,
        bytes: 0,
    };
    let counted = body.into_data_stream().inspect(move |chunk| {
        if let Ok(bytes) = chunk {
            // Through a method, deliberately. `guard.bytes += …` would let
            // the 2021 closure rules capture only the `bytes` *field* — a
            // `u64`, so a copy — and the guard itself would be dropped back
            // in the handler, recording zero before the first chunk moved.
            // A method call needs the whole receiver, so the whole guard
            // moves into the closure and lives exactly as long as the stream.
            guard.add(bytes.len() as u64);
        }
        // When the stream is dropped — fully served, or the client gone —
        // the guard's `Drop` files the numbers.
    });
    Response::from_parts(parts, axum::body::Body::from_stream(counted))
}

struct DownloadGuard {
    metrics: std::sync::Arc<TransferMetrics>,
    start: Instant,
    bytes: u64,
}

impl DownloadGuard {
    fn add(&mut self, n: u64) {
        self.bytes += n;
    }
}

impl Drop for DownloadGuard {
    fn drop(&mut self) {
        self.metrics
            .record_download(self.bytes, self.start.elapsed().as_millis() as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_accumulate_and_directions_stay_apart() {
        let m = TransferMetrics::new();
        m.record_upload_bytes(1_000, 10);
        m.record_upload_bytes(3_000, 30);
        m.upload_completed();
        m.record_download(500, 5);

        let view = m.snapshot();
        assert_eq!(view.uploads.bytes, 4_000);
        assert_eq!(view.uploads.millis, 40);
        // One finished file — not one per chunk.
        assert_eq!(view.uploads.transfers, 1);
        assert_eq!(view.downloads.transfers, 1);
        assert_eq!(view.downloads.bytes, 500);
    }

    #[test]
    fn recent_forgets_what_the_window_no_longer_covers() {
        let m = TransferMetrics::new();
        let now = crate::db::now_ms();
        // One sample well outside the window, one inside.
        m.record(now - RECENT_WINDOW_MS - 1_000, true, 9_000, 90);
        m.record(now - 1_000, true, 1_000, 10);

        let view = m.snapshot_at(now);
        // Totals keep everything…
        assert_eq!(view.uploads.bytes, 10_000);
        // …the recent window keeps only what just happened.
        assert_eq!(view.uploads.recent_bytes, 1_000);
        assert_eq!(view.uploads.recent_millis, 10);
    }

    #[test]
    fn the_ring_is_bounded() {
        let m = TransferMetrics::new();
        for _ in 0..(RECENT_CAP + 100) {
            m.record_upload_bytes(1, 1);
        }
        assert!(m.recent.lock().unwrap().len() <= RECENT_CAP);
        // Evicting a sample from the ring never rewrites the totals.
        assert_eq!(m.snapshot().uploads.bytes, (RECENT_CAP + 100) as u64);
    }

    #[test]
    fn an_empty_server_reports_zeros_not_errors() {
        let view = TransferMetrics::new().snapshot();
        assert_eq!(view.uploads, FlowView::default());
        assert_eq!(view.downloads, FlowView::default());
        assert!(view.since_ms > 0);
        assert_eq!(view.recent_window_ms, RECENT_WINDOW_MS);
    }
}
