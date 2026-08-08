//! Driving the resumable upload protocol (`docs/API.md` §14.2).
//!
//! # The file is never in memory
//!
//! That is the whole point of this module, and it is easy to undo by accident.
//! A `web_sys::File` is a *handle*, not bytes — the browser keeps the data on
//! disk. `Blob::slice` makes another handle to part of it, still without
//! reading anything, and only `array_buffer()` on a slice actually pulls bytes
//! in. So the checksum pass holds one chunk at a time, and the upload pass
//! holds **nothing**: each chunk's body is the `Blob` slice itself, which the
//! browser's network process reads directly.
//!
//! The old path did the opposite in one line: `blob.array_buffer()` over the
//! whole file, then `Uint8Array::from(&vec)` to send it — the file twice, in an
//! address space that is 4 GB in total. A 700 MB attachment was already
//! hopeless; the 25 MB cap was hiding it.
//!
//! # The chunk body must be a `Blob`, not an `ArrayBuffer`
//!
//! Not a style preference — a measured 4x. A `fetch` whose body is an
//! `ArrayBuffer` makes the renderer copy the whole buffer to the network
//! process *on the main thread*: ~250 ms per 8 MB chunk, during which the UI
//! is frozen, capping uploads near 30 MB/s however fast the link is. A `Blob`
//! body is a handle the network process reads by itself — same loop measured
//! at over 100 MB/s with the main thread idle. (Localhost, Chromium; the
//! per-chunk stall is the same order everywhere, so on a fast LAN it is the
//! difference between the link's speed and a third of it.)
//!
//! # Two passes, on purpose
//!
//! The digest is computed in its own pass before uploading rather than
//! alongside it, because the server wants it *declared up front* — that is what
//! lets a mismatch be caught at `finish` against something the client committed
//! to before it knew what would arrive. Reading a local file twice is cheap
//! (the OS cache has it) next to sending it once over a network.
//!
//! Callers get progress for both passes, distinguished by [`Phase`], because a
//! silent minute of hashing before the bar starts moving reads as a hang.

use gloo_net::http::Method;
use pocketskynet_core::hash::Sha256Stream;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use super::{ApiError, ApiResult, Client};

/// Which pass a progress report belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Reading the file to compute its checksum. Local, fast, no network.
    Checksum,
    /// Sending it.
    Upload,
}

/// What the caller is told after every chunk.
#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub phase: Phase,
    pub done: f64,
    pub total: f64,
}

impl Progress {
    /// 0.0–1.0. A zero-length file is complete rather than a division by zero.
    pub fn fraction(&self) -> f64 {
        if self.total <= 0.0 {
            1.0
        } else {
            (self.done / self.total).clamp(0.0, 1.0)
        }
    }
}

/// What is being uploaded, which decides what `finish` returns.
#[derive(Debug, Clone)]
pub enum Target {
    /// A room attachment.
    File { room_id: String, caption: String },
    /// An image or video, stored by content hash and served publicly.
    Image { mime: String },
    /// A published site: an HTML document or a zip, paid for by `tx_hash`.
    Site { title: String, tx_hash: String },
}

/// The ceiling, mirrored from `routes/uploads.rs::MAX_UPLOAD_BYTES`.
///
/// Checked on the device so a file that was never going to work fails
/// immediately rather than after however long it takes to hash and send it.
/// 4 GiB minus one byte, for the same reason as the server: it is the largest
/// count a `u32` can hold, and exactly 4 GiB would be one byte past it.
pub const MAX_UPLOAD_BYTES: f64 = 4.0 * 1024.0 * 1024.0 * 1024.0 - 1.0;

/// Fallback when the server does not name one. The server always does; this
/// exists so a malformed response degrades to a working upload rather than a
/// division by zero.
const FALLBACK_CHUNK: f64 = 8.0 * 1024.0 * 1024.0;

/// How many times one chunk is retried before the upload gives up.
///
/// A transfer that is minutes long will meet a blip, and throwing away the
/// whole thing for one failed request is the behaviour this protocol exists to
/// avoid. On a 409 the retry is not blind: the server's message carries where
/// it actually is, and the loop re-reads the offset and continues from there.
const CHUNK_ATTEMPTS: u32 = 4;

/// How long `begin` and `upload_status` may run before they are treated as
/// failed and retried. `append` gets this *plus* time for its body — see
/// [`append_timeout_ms`].
///
/// The gap this closes: a dead mobile connection often stops answering rather
/// than erroring, and a `fetch` in that state resolves neither `Ok` nor `Err`
/// — it just never settles. Without a bound on it, `recover`'s retry loop
/// never engages, because there is never an `Err` to trigger it, and the
/// transfer sits exactly where the transfer rail's own stall timer
/// (`components/transfers.rs::STALL_AFTER_MS`) finds it: stuck at one byte
/// count, forever, with cancel-and-reattach as the only way out. Seen in the
/// field as a phone stalled at exactly one chunk boundary while the server's
/// session row said the next chunk had fully landed — the chunk arrived, the
/// *response* was lost, and the request that would never settle was the only
/// thing the client was waiting on.
///
/// `finish` is deliberately not raced against any clock: the server re-hashes
/// the whole file there, and a multi-gigabyte upload can legitimately hold it
/// well past any bound that would still catch a dead control request promptly.
const REQUEST_TIMEOUT_MS: u32 = 45_000;

/// The floor transfer rate an `append` is budgeted against, in bytes per
/// millisecond (128 KB/s).
///
/// A chunk's timeout is [`REQUEST_TIMEOUT_MS`] plus its size at this rate, so
/// an 8 MB chunk gets ~109 s before it is called dead. Deliberately far below
/// any usable link — even a relayed Tailscale path clears it easily — because
/// this only exists to separate "slow" from "never", and a link genuinely
/// under it still converges: every timed-out chunk halves the next attempt
/// (see [`MIN_CHUNK`]), so the budget shrinks toward one a struggling link can
/// meet.
const FLOOR_BYTES_PER_MS: f64 = 131.0;

/// Where the halving stops. Small enough that a link this side of unusable can
/// finish one inside its budget; large enough that progress is still real.
const MIN_CHUNK: f64 = 512.0 * 1024.0;

/// The message every timed-out request carries, and the marker
/// [`upload_in_chunks`] checks to tell "the link is slow" from an ordinary
/// refusal — only the former should shrink the next chunk.
const TIMED_OUT: &str = "The request timed out";

/// The clock an `append` of `bytes` races: the control-request bound plus the
/// body itself at the floor rate.
fn append_timeout_ms(bytes: f64) -> u32 {
    REQUEST_TIMEOUT_MS.saturating_add((bytes / FLOOR_BYTES_PER_MS) as u32)
}

/// Race a request that already carries `controller`'s abort signal against
/// `timeout_ms`, aborting it on the clock rather than leaving it to hang. The
/// controller must be the one whose signal was attached when the request was
/// built — aborting any other one does nothing to this request.
async fn send_with_timeout(
    req: gloo_net::http::Request,
    controller: web_sys::AbortController,
    timeout_ms: u32,
) -> ApiResult<gloo_net::http::Response> {
    use futures::future::{select, Either};
    match select(
        Box::pin(req.send()),
        Box::pin(gloo_timers::future::TimeoutFuture::new(timeout_ms)),
    )
    .await
    {
        Either::Left((result, _)) => result.map_err(|e| ApiError::Network(e.to_string())),
        Either::Right(_) => {
            // Stops the browser doing the work for a response nothing is
            // waiting for any more, and — for `append` specifically — is what
            // lets the *next* attempt at this same offset land cleanly rather
            // than racing a chunk this attempt is still, unknown to it, in the
            // middle of sending.
            controller.abort();
            Err(ApiError::Network(TIMED_OUT.to_owned()))
        }
    }
}

/// A fresh [`web_sys::AbortController`], as the one map-error site for the
/// constructor every timed request needs.
fn abort_controller() -> ApiResult<web_sys::AbortController> {
    web_sys::AbortController::new()
        .map_err(|_| ApiError::Network("Could not prepare the request".to_owned()))
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionView {
    id: String,
    offset: f64,
    #[allow(dead_code)]
    size: f64,
    chunk_size: f64,
}

impl Client {
    /// Upload a `File` in chunks, reporting progress, and return whatever the
    /// server made of it.
    ///
    /// `on_progress` is called after every chunk of both passes. It must be
    /// cheap — it runs inside the transfer loop.
    pub async fn upload_in_chunks<F>(
        &self,
        file: &web_sys::File,
        target: Target,
        mut on_progress: F,
    ) -> ApiResult<serde_json::Value>
    where
        F: FnMut(Progress),
    {
        let size = file.size();
        if size <= 0.0 {
            return Err(ApiError::Network("The file is empty".to_owned()));
        }
        if size > MAX_UPLOAD_BYTES {
            // `+ 1.0` because the ceiling is 4 GiB minus a byte, and "3 GB
            // limit" would be a lie in the other direction.
            return Err(ApiError::Network(format!(
                "The file is larger than the {:.0} GB limit",
                (MAX_UPLOAD_BYTES + 1.0) / (1024.0 * 1024.0 * 1024.0)
            )));
        }
        let filename = file.name();
        let blob: web_sys::Blob = file.clone().into();

        // Pass one: the checksum, over slices, so this does not become the
        // thing that reads the whole file into memory.
        let digest = checksum(&blob, size, &mut on_progress).await?;

        // Pass two: the transfer — resuming the last attempt at this exact
        // file if there is one still standing.
        let key = resume_key(&target, &digest, size);
        let session = match self.resume(&key, size).await {
            Some(existing) => existing,
            None => {
                let started = self.begin(&filename, size, &digest, &target).await?;
                remember_session(&key, &started.id);
                started
            }
        };
        let mut chunk = if session.chunk_size > 0.0 {
            session.chunk_size
        } else {
            FALLBACK_CHUNK
        };

        let mut offset = session.offset;
        on_progress(Progress {
            phase: Phase::Upload,
            done: offset,
            total: size,
        });

        while offset < size {
            let end = (offset + chunk).min(size);
            // A handle, not bytes — see the module docs for why sending the
            // slice itself (rather than reading it first) is load-bearing.
            let part = slice_blob(&blob, offset, end)?;

            match self.append(&session.id, offset, &part).await {
                Ok(next) => offset = next,
                Err(e) => {
                    // A timeout means the link could not move this much inside
                    // its budget, so the next attempt asks less of it. The
                    // server does not care that chunks stop being uniform, and
                    // without this a link slower than the budget's floor rate
                    // would retry the same too-big chunk forever.
                    if matches!(&e, ApiError::Network(m) if m == TIMED_OUT) {
                        chunk = (chunk / 2.0).max(MIN_CHUNK);
                    }
                    // Ask the server where it really is and carry on from
                    // there. This covers the common case — the chunk landed
                    // and the response was lost — and it covers a genuine
                    // failure by looping until the attempts run out.
                    offset = self.recover(&session.id, offset, e).await?;
                }
            }
            on_progress(Progress {
                phase: Phase::Upload,
                done: offset,
                total: size,
            });
        }

        let done = self.finish(&session.id).await;
        // The session is over either way once `finish` has spoken: on success
        // it no longer exists, and on a *commit* failure (a full room, an
        // unpayable site) the server keeps the bytes but retrying is a fresh
        // decision. Only a transfer that never reached `finish` is worth
        // resuming, and that is exactly the case this does not clear.
        if done.is_ok() {
            forget_session(&key);
        }
        done
    }

    /// Pick up an upload of this exact file that a previous attempt left open.
    ///
    /// Returns `None` for anything unclear rather than guessing: no stored id,
    /// a session the server has forgotten, or one whose declared size does not
    /// match. Resuming into the wrong session would append this file's bytes to
    /// a different one, and the digest check at the end would be the first
    /// anyone heard of it.
    async fn resume(&self, key: &str, size: f64) -> Option<SessionView> {
        let id = stored_session(key)?;
        match self.upload_status(&id).await {
            Ok(s) if s.size == size => Some(s),
            // Gone, or not the file we think it is. Start over.
            _ => {
                forget_session(key);
                None
            }
        }
    }

    async fn begin(
        &self,
        filename: &str,
        size: f64,
        digest: &str,
        target: &Target,
    ) -> ApiResult<SessionView> {
        // Every size on this path is an `f64`, because that is what `Blob::size`
        // returns and because `usize` is 32 bits here. But `serde_json`
        // serialises an `f64` as a JSON *float* — `125829120.0` — and the
        // server's `size: u64` refuses it with a 422 before a byte is sent.
        // A file size is always a whole number, so it goes on the wire as one.
        let size = size as u64;
        let body = match target {
            Target::File { room_id, caption } => serde_json::json!({
                "kind": "file",
                "roomId": room_id,
                "filename": filename,
                "caption": caption,
                "size": size,
                "sha256": digest,
            }),
            Target::Image { mime } => serde_json::json!({
                "kind": "image",
                "filename": filename,
                "mime": mime,
                "size": size,
                "sha256": digest,
            }),
            Target::Site { title, tx_hash } => serde_json::json!({
                "kind": "site",
                "filename": filename,
                // The site's title rides in `caption` and its payment in
                // `extra`; the server unpacks both.
                "caption": title,
                "extra": tx_hash,
                "size": size,
                "sha256": digest,
            }),
        };
        let controller = abort_controller()?;
        let req = self
            .build(Method::POST, "/api/uploads")
            .abort_signal(Some(&controller.signal()))
            .json(&body)
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let resp = send_with_timeout(req, controller, REQUEST_TIMEOUT_MS).await?;
        super::decode(resp).await
    }

    /// Send one chunk — as a `Blob` slice, never as bytes the wasm side has
    /// read. Returns the offset the server is now at.
    async fn append(&self, id: &str, offset: f64, part: &web_sys::Blob) -> ApiResult<f64> {
        #[derive(serde::Deserialize)]
        struct Ack {
            offset: f64,
        }
        // `as u64` for the same reason `begin` casts: an `f64` renders without
        // a fraction today, but the server parses this as an integer and a
        // formatting change would be a 400 rather than a compile error.
        let path = format!("/api/uploads/{id}?offset={}", offset as u64);
        let controller = abort_controller()?;
        // The explicit header wins over the Blob's own (empty) type, so the
        // server sees the same request it always did.
        let req = self
            .build(Method::PATCH, &path)
            .header("Content-Type", "application/octet-stream")
            .abort_signal(Some(&controller.signal()))
            .body(part)
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let resp = send_with_timeout(req, controller, append_timeout_ms(part.size())).await?;
        let ack: Ack = super::decode(resp).await?;
        Ok(ack.offset)
    }

    /// Work out where to resume after a failed chunk, or give up.
    ///
    /// Takes the error rather than swallowing it so that a genuine refusal —
    /// the room filled up, the session was reaped — surfaces as itself instead
    /// of as an endless retry loop.
    async fn recover(&self, id: &str, attempted: f64, err: ApiError) -> ApiResult<f64> {
        for _ in 0..CHUNK_ATTEMPTS {
            match self.upload_status(id).await {
                Ok(s) => return Ok(s.offset),
                // The session is gone. Nothing to resume, and retrying cannot
                // bring it back.
                Err(ApiError::Status(s)) if s.status == 404 => break,
                Err(_) => continue,
            }
        }
        let _ = attempted;
        Err(err)
    }

    async fn upload_status(&self, id: &str) -> ApiResult<SessionView> {
        let controller = abort_controller()?;
        let req = self
            .build(Method::GET, &format!("/api/uploads/{id}"))
            .abort_signal(Some(&controller.signal()))
            .build()
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let resp = send_with_timeout(req, controller, REQUEST_TIMEOUT_MS).await?;
        super::decode(resp).await
    }

    async fn finish(&self, id: &str) -> ApiResult<serde_json::Value> {
        self.send_json(
            Method::POST,
            &format!("/api/uploads/{id}/finish"),
            &serde_json::json!({}),
        )
        .await
    }

    /// Abandon a session and let the server reclaim its disk now rather than
    /// at the next sweep.
    pub async fn abort_upload(&self, id: &str) -> ApiResult<()> {
        self.send_ok_empty(Method::DELETE, &format!("/api/uploads/{id}"))
            .await
    }
}

// ------------------------------------------------------- resuming a retry --
//
// An upload that fails halfway should cost the half that failed, not the whole
// file. The protocol already supports that — the server keeps the partial file
// and will say where it got to — but only if the *next attempt* knows which
// session to ask about, and a fresh `attach` is a fresh function call with no
// memory of the last one. So the id outlives the attempt, in local storage.

const RESUME_PREFIX: &str = "ps.upload.resume.";

/// The identity of an upload, for the purpose of resuming it.
///
/// The **content digest** does the work here, not the filename: renaming a file
/// between attempts must not orphan its half-uploaded session, and two
/// different files that happen to share a name must not be mistaken for each
/// other. Destination and size are in the key because the same bytes going to a
/// different room is a different upload, and because a size mismatch is the
/// cheapest possible sanity check before appending to something.
fn resume_key(target: &Target, digest: &str, size: f64) -> String {
    let dest = match target {
        Target::File { room_id, .. } => format!("file:{room_id}"),
        Target::Image { .. } => "image".to_owned(),
        // Not keyed by `tx_hash`: a retry after a failed publish reuses the
        // same payment, and keying on it would strand the partial upload.
        Target::Site { .. } => "site".to_owned(),
    };
    format!("{RESUME_PREFIX}{dest}:{digest}:{size}")
}

#[cfg(target_arch = "wasm32")]
fn remember_session(key: &str, id: &str) {
    use gloo_storage::Storage;
    let _ = gloo_storage::LocalStorage::set(key, id);
}

#[cfg(target_arch = "wasm32")]
fn stored_session(key: &str) -> Option<String> {
    use gloo_storage::Storage;
    gloo_storage::LocalStorage::get::<String>(key).ok()
}

#[cfg(target_arch = "wasm32")]
fn forget_session(key: &str) {
    use gloo_storage::Storage;
    gloo_storage::LocalStorage::delete(key);
}

#[cfg(not(target_arch = "wasm32"))]
fn remember_session(_key: &str, _id: &str) {}
#[cfg(not(target_arch = "wasm32"))]
fn stored_session(_key: &str) -> Option<String> {
    None
}
#[cfg(not(target_arch = "wasm32"))]
fn forget_session(_key: &str) {}

/// SHA-256 of a blob, read in slices.
pub async fn checksum<F>(blob: &web_sys::Blob, size: f64, on_progress: &mut F) -> ApiResult<String>
where
    F: FnMut(Progress),
{
    // Bigger than the network chunk: this pass never leaves the machine, so
    // the only cost is one allocation of this size at a time.
    const READ: f64 = 16.0 * 1024.0 * 1024.0;

    let mut hasher = Sha256Stream::new();
    let mut at = 0.0f64;
    while at < size {
        let end = (at + READ).min(size);
        let piece = read_slice(blob, at, end).await?;
        hasher.update(&piece.to_vec());
        at = end;
        on_progress(Progress {
            phase: Phase::Checksum,
            done: at,
            total: size,
        });
    }
    Ok(hasher.finish())
}

/// `[start, end)` of a blob, as another handle — nothing is read.
///
/// `slice_with_f64_and_f64` rather than the `i32` overload: an `i32` offset
/// overflows at 2 GB, which would silently corrupt exactly the uploads this
/// work exists to support — the second half of a 3 GB file would be read from a
/// negative offset.
fn slice_blob(blob: &web_sys::Blob, start: f64, end: f64) -> ApiResult<web_sys::Blob> {
    blob.slice_with_f64_and_f64(start, end)
        .map_err(|_| ApiError::Network("Could not read the file".to_owned()))
}

/// Read `[start, end)` of a blob into wasm-visible bytes.
///
/// For the checksum pass only — hashing genuinely needs the bytes. The upload
/// pass must never come back to this; it sends handles (see the module docs).
async fn read_slice(blob: &web_sys::Blob, start: f64, end: f64) -> ApiResult<js_sys::Uint8Array> {
    let part = slice_blob(blob, start, end)?;
    let buffer = JsFuture::from(part.array_buffer())
        .await
        .map_err(|_| ApiError::Network("Could not read the file".to_owned()))?;
    let array = buffer
        .dyn_into::<js_sys::ArrayBuffer>()
        .map_err(|_| ApiError::Network("Could not read the file".to_owned()))?;
    Ok(js_sys::Uint8Array::new(&array))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fraction_is_bounded_and_survives_an_empty_file() {
        let p = |done, total| Progress {
            phase: Phase::Upload,
            done,
            total,
        };
        assert_eq!(p(0.0, 100.0).fraction(), 0.0);
        assert_eq!(p(50.0, 100.0).fraction(), 0.5);
        assert_eq!(p(100.0, 100.0).fraction(), 1.0);
        // A zero-length file is done, not a NaN — and NaN would render as an
        // empty bar forever rather than as an error.
        assert_eq!(p(0.0, 0.0).fraction(), 1.0);
        // A server that over-reports cannot push the bar past the end.
        assert_eq!(p(150.0, 100.0).fraction(), 1.0);
    }

    #[test]
    fn a_resume_key_identifies_content_rather_than_a_name() {
        let room = || Target::File {
            room_id: "room_1".to_owned(),
            caption: "anything".to_owned(),
        };
        let a = resume_key(&room(), "aa".repeat(32).as_str(), 100.0);

        // The caption is not part of the identity: editing it between attempts
        // must not strand a half-uploaded file.
        let with_other_caption = resume_key(
            &Target::File {
                room_id: "room_1".to_owned(),
                caption: "different".to_owned(),
            },
            "aa".repeat(32).as_str(),
            100.0,
        );
        assert_eq!(a, with_other_caption);

        // Different content, different destination, or a different size are
        // each a different upload — the last one because appending this file's
        // bytes to a session opened for another size is how a resume corrupts.
        assert_ne!(a, resume_key(&room(), "bb".repeat(32).as_str(), 100.0));
        assert_ne!(a, resume_key(&room(), "aa".repeat(32).as_str(), 101.0));
        assert_ne!(
            a,
            resume_key(
                &Target::File {
                    room_id: "room_2".to_owned(),
                    caption: String::new(),
                },
                "aa".repeat(32).as_str(),
                100.0
            )
        );

        // Kinds do not collide with one another.
        let img = resume_key(
            &Target::Image {
                mime: "image/png".to_owned(),
            },
            "aa".repeat(32).as_str(),
            100.0,
        );
        assert_ne!(a, img);

        // A site retry reuses its payment, so `tx_hash` must not be part of
        // the key or every retry would start from zero.
        let site = |tx: &str| {
            resume_key(
                &Target::Site {
                    title: "t".to_owned(),
                    tx_hash: tx.to_owned(),
                },
                "aa".repeat(32).as_str(),
                100.0,
            )
        };
        assert_eq!(site("0xdead"), site("0xbeef"));
    }

    #[test]
    fn a_chunks_clock_scales_with_its_size() {
        // The control bound alone for an empty body…
        assert_eq!(append_timeout_ms(0.0), REQUEST_TIMEOUT_MS);
        // …an 8 MB chunk gets over a minute on top of it…
        let eight_mb = append_timeout_ms(8.0 * 1024.0 * 1024.0);
        assert!(eight_mb > REQUEST_TIMEOUT_MS + 60_000, "{eight_mb}");
        // …and the floor rate is far below any usable link, so a live
        // connection clears the budget with room to spare: even at ten times
        // the floor, an 8 MB chunk finishes inside a fifth of its clock.
        let at_ten_times_floor = (8.0 * 1024.0 * 1024.0 / (FLOOR_BYTES_PER_MS * 10.0)) as u32;
        assert!(at_ten_times_floor < eight_mb / 5, "{at_ten_times_floor}");
        // The halving floor stays under the fallback chunk, so shrinking is
        // real: a first retry already sends less than the first attempt did.
        let halved = (FALLBACK_CHUNK / 2.0).max(MIN_CHUNK);
        assert!(halved < FALLBACK_CHUNK);
    }

    #[test]
    fn the_client_ceiling_matches_the_servers() {
        // u32::MAX — one byte under 4 GiB, exactly what the server allows. If
        // these drift, a file is refused by whichever is smaller and the
        // other's error message is a lie.
        assert_eq!(MAX_UPLOAD_BYTES, 4_294_967_295.0);
        // And the message still calls it a 4 GB limit, not a 3 GB one.
        assert_eq!((MAX_UPLOAD_BYTES + 1.0) / (1024.0 * 1024.0 * 1024.0), 4.0);
    }
}
