//! Resumable chunked uploads (`docs/API.md` §14.2).
//!
//! ```text
//! POST   /api/uploads              begin  → { id, offset, chunkSize }
//! PATCH  /api/uploads/{id}?offset= append → { offset, size }
//! GET    /api/uploads/{id}         status → { offset, size }
//! POST   /api/uploads/{id}/finish  commit → the created resource
//! DELETE /api/uploads/{id}         abort
//! ```
//!
//! # Why an upload is a conversation and not a request
//!
//! The cap used to be 25 MB, and the reason given was honest: the whole body
//! was held in memory on both sides. Raising that number alone moves the
//! failure rather than fixing it, and at 4 GB it is not a tuning question —
//! a wasm32 client's *entire address space* is 4 GB, and the old client path
//! held the file twice (a `Vec<u8>` and the `Uint8Array` copied from it), so
//! it died an order of magnitude below the limit it advertised.
//!
//! So the transfer is split. The client reads its file in slices it never
//! holds all of, and each chunk is an ordinary small request. What that buys,
//! beyond working at all:
//!
//! * **Bounded memory, on both ends.** The server's exposure is
//!   [`MAX_CHUNK_BYTES`] × concurrent uploads, not file size × concurrent
//!   uploads. That is the single most important property here, and it is why
//!   [`append`] takes `Bytes` rather than streaming the body — the body *is*
//!   one chunk, and a 16 MB ceiling enforced by a body limit is both simpler
//!   and stricter than hand-rolled stream accounting.
//! * **Resume.** A dropped connection costs one chunk, not 4 GB. The client
//!   asks where it got to and carries on.
//! * **Progress that is not a lie.** The client knows how many bytes the
//!   server has acknowledged, because the server says so after every chunk.
//!
//! # The offset is the server's, not the client's
//!
//! Every append names the offset it believes it is writing at, and is refused
//! with a 409 unless that matches what the row says. The check and the update
//! are one conditional statement (`db/uploads.rs::advance`), so two chunks
//! racing cannot both win. This is what makes a retried chunk — the normal
//! case on a flaky network, where the write landed but the response did not —
//! safe: the retry is refused, the client re-reads the offset, and nothing is
//! written twice. A protocol that trusted the client's offset would silently
//! produce a corrupt file, and the digest check at the end would be the first
//! anyone heard of it, after 4 GB.
//!
//! # What `finish` actually verifies
//!
//! The whole assembled file is re-hashed and compared to the digest the client
//! declared at `begin`. Not the chunks as they arrive: a per-chunk digest
//! proves each piece survived the wire, which is the easy half, and proves
//! nothing about whether they were assembled in the right order or whether the
//! disk kept them. One pass over the finished file covers transfer, ordering,
//! assembly and storage together, and it produces the content hash the store
//! is addressed by, which is needed regardless.
//!
//! Declaring the digest is optional — a client that cannot hash 4 GB before
//! sending it may omit it — but the file is hashed at `finish` either way,
//! because the storage layout requires it.

use std::path::{Path as FsPath, PathBuf};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

use crate::auth::AuthUser;
use crate::db::uploads::{self, NewSession, Session, UploadKind};
use crate::error::{ApiError, ApiResult};
use crate::AppState;

/// The largest file this server will assemble, for every kind.
///
/// 4 GB. The ceiling is not arbitrary and is not a disk question: it is one
/// byte under what a `u32` byte-count can express, which is the limit every
/// 32-bit boundary in the stack shares — wasm32's address space, `zip`'s
/// classic (non-Zip64) headers, and a great deal of client tooling. Going past
/// it means auditing all of those rather than editing this constant.
pub const MAX_UPLOAD_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// The largest single chunk, and therefore the server's per-upload memory.
///
/// The client is *told* [`SUGGESTED_CHUNK_BYTES`]; this is the hard ceiling a
/// body limit enforces, with room above the suggestion so a client that picks
/// its own size has somewhere to move. Total exposure is this times the number
/// of uploads appending at the same instant.
pub const MAX_CHUNK_BYTES: usize = 16 * 1024 * 1024;

/// What `begin` tells the client to use.
///
/// 8 MB is a compromise between per-request overhead (a 4 GB file is 512
/// requests at this size, 4096 at 1 MB) and how much work a dropped connection
/// throws away. It is advisory: the client may send less, and the protocol
/// does not care whether chunks are uniform.
pub const SUGGESTED_CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// How long a session may go untouched before the sweep reclaims its disk.
///
/// Generous, because the client holds the only complete copy and a person who
/// closed a laptop mid-upload should be able to resume it. Polling the status
/// endpoint counts as being alive, so a paused-but-attended upload never ages
/// out.
const SESSION_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// Open sessions one wallet may hold at once.
///
/// Sessions cost disk before they cost anything else, and an abandoned one
/// costs it until the sweep runs. This is the bound that stops a client — buggy
/// or otherwise — from opening thousands.
const MAX_OPEN_SESSIONS: i64 = 8;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/uploads", post(begin))
        .route(
            "/uploads/{id}",
            get(status).patch(append).delete(abort_session),
        )
        .route("/uploads/{id}/finish", post(finish))
        // The one limit that matters in this module. Innermost wins, so this
        // replaces the 100 KB API-wide default for chunk bodies — and, being a
        // *limit* rather than a suggestion, it is what actually bounds server
        // memory no matter what a client claims its chunk size is.
        .layer(DefaultBodyLimit::max(MAX_CHUNK_BYTES))
}

// ----------------------------------------------------------------- begin --

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BeginRequest {
    kind: String,
    /// Required for `kind: "file"`, ignored otherwise.
    room_id: Option<String>,
    filename: Option<String>,
    caption: Option<String>,
    /// The uploader's declared type. Recorded, never trusted for serving.
    mime: Option<String>,
    size: u64,
    /// Lowercase hex sha-256 of the whole file, if the client can compute it
    /// up front. Verified at `finish`.
    sha256: Option<String>,
    /// Kind-specific payload — the sites route's `txHash`, for instance.
    extra: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionView {
    id: String,
    offset: i64,
    size: i64,
    chunk_size: usize,
}

impl From<&Session> for SessionView {
    fn from(s: &Session) -> Self {
        Self {
            id: s.id.clone(),
            offset: s.received,
            size: s.declared_size,
            chunk_size: SUGGESTED_CHUNK_BYTES,
        }
    }
}

/// `POST /api/uploads`
async fn begin(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Json(req): Json<BeginRequest>,
) -> ApiResult<Response> {
    let kind =
        UploadKind::parse(&req.kind).ok_or_else(|| ApiError::bad_request("Unknown upload kind"))?;

    if req.size == 0 {
        return Err(ApiError::bad_request("Empty file"));
    }
    if req.size > MAX_UPLOAD_BYTES {
        // Refused before a byte is sent, which is the entire reason the size is
        // declared up front rather than discovered at the end.
        return Err(ApiError::bad_request(format!(
            "File is larger than the {} GB limit",
            MAX_UPLOAD_BYTES / (1024 * 1024 * 1024)
        )));
    }
    let sha256 = normalise_digest(req.sha256.as_deref())?;

    let filename = crate::validate::filename(req.filename.as_deref())?;
    let caption = crate::validate::caption(req.caption.as_deref())?;

    // Kind-specific authorisation, done here so an unauthorised upload is
    // refused before it occupies any disk at all.
    let room_id = match kind {
        UploadKind::File => {
            let raw = req
                .room_id
                .as_deref()
                .ok_or_else(|| ApiError::bad_request("roomId is required for a file upload"))?;
            let room = pocketskynet_core::RoomId::new(raw)
                .map_err(|_| ApiError::not_found("Room not found"))?;
            crate::routes::messages::require_member(&state, &room, &caller).await?;
            crate::routes::files::check_room_capacity(&state, &room).await?;
            Some(room.as_str().to_owned())
        }
        // Images and sites are per-user, not per-room. Sites additionally
        // require a payment, which is checked at `finish` rather than here:
        // the transaction hash is only meaningful against a finished artefact,
        // and charging for an upload that never completes would be worse.
        UploadKind::Image | UploadKind::Site => None,
    };

    let owner = caller.as_str().to_owned();
    let open = {
        let owner = owner.clone();
        state
            .db
            .call(move |conn| uploads::open_count(conn, &owner))
            .await?
    };
    if open >= MAX_OPEN_SESSIONS {
        return Err(ApiError::bad_request(format!(
            "You already have {MAX_OPEN_SESSIONS} uploads in progress. Finish or cancel one first."
        )));
    }

    let id = format!("up_{}_{}", crate::db::now_ms(), uuid::Uuid::new_v4());
    let temp_name = format!("{id}.part");

    // Create the file before the row. A row naming a file that does not exist
    // makes every append fail in a way that reads as corruption; a file with no
    // row is invisible and the sweep will not find it either — so the file is
    // removed again if the insert fails.
    let dir = state.cfg.uploads_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    let temp_path = dir.join(&temp_name);
    tokio::fs::File::create(&temp_path)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;

    let new = NewSession {
        id: id.clone(),
        owner,
        kind,
        room_id,
        filename,
        caption,
        mime: req.mime.unwrap_or_default(),
        declared_size: req.size as i64,
        sha256,
        temp_name,
        extra: req.extra.unwrap_or_default(),
    };
    let session = match state.db.call(move |conn| uploads::create(conn, new)).await {
        Ok(s) => s,
        Err(e) => {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(e);
        }
    };

    Ok((StatusCode::CREATED, Json(SessionView::from(&session))).into_response())
}

/// Lowercase and shape-check a client-declared digest.
///
/// Rejecting a malformed one here rather than at `finish` means a client that
/// is going to fail the comparison finds out before it uploads 4 GB.
fn normalise_digest(raw: Option<&str>) -> ApiResult<Option<String>> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let lower = raw.to_ascii_lowercase();
    if lower.len() != 64 || !lower.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ApiError::bad_request(
            "sha256 must be 64 hexadecimal characters",
        ));
    }
    Ok(Some(lower))
}

// ------------------------------------------------------------- the session --

/// Load a session and prove the caller owns it.
///
/// A uniform 404 for someone else's session, not a 403: session ids are
/// guessable-shaped, and confirming one exists tells a stranger that a
/// particular wallet is uploading something.
async fn owned_session(state: &AppState, caller: &str, id: &str) -> ApiResult<Session> {
    let owned = id.to_owned();
    let session = state
        .db
        .call(move |conn| uploads::read(conn, &owned))
        .await?
        .ok_or_else(|| ApiError::not_found("Upload not found"))?;
    if session.owner != caller {
        return Err(ApiError::not_found("Upload not found"));
    }
    Ok(session)
}

/// The temp file for a session, with the name re-validated.
///
/// `temp_name` is generated by this module and never leaves it, but it is the
/// value that becomes a path — and a guard that only runs where the value is
/// written protects nothing if anything else ever writes one.
fn temp_path(state: &AppState, session: &Session) -> ApiResult<PathBuf> {
    let name = &session.temp_name;
    let ok = name.len() > 5
        && name.ends_with(".part")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        && !name.contains("..");
    if !ok {
        return Err(ApiError::Internal(anyhow::anyhow!(
            "refusing to touch upload temp name {name:?}"
        )));
    }
    Ok(state.cfg.uploads_dir().join(name))
}

#[derive(Debug, Deserialize)]
struct AppendParams {
    offset: Option<u64>,
}

/// `PATCH /api/uploads/{id}?offset=N`
async fn append(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(id): Path<String>,
    Query(params): Query<AppendParams>,
    body: Bytes,
) -> ApiResult<Response> {
    let session = owned_session(&state, caller.as_str(), &id).await?;

    if body.is_empty() {
        return Err(ApiError::bad_request("Empty chunk"));
    }
    let offset = params
        .offset
        .ok_or_else(|| ApiError::bad_request("offset is required"))?;

    // The client's belief about where it is, checked against the server's
    // record *before* anything is written. The conditional update below is what
    // actually makes this safe under concurrency; this check exists to give the
    // common case — a retried chunk — a 409 with the real offset in it rather
    // than a write followed by a rollback.
    if offset != session.received as u64 {
        return Err(offset_conflict(&session));
    }

    let end = offset
        .checked_add(body.len() as u64)
        .ok_or_else(|| ApiError::bad_request("Chunk overflows the file"))?;
    if end > session.declared_size as u64 {
        return Err(ApiError::bad_request(
            "Chunk would write past the declared file size",
        ));
    }

    let path = temp_path(&state, &session)?;

    // Claim the offset *before* writing. If the write then fails the row is
    // ahead of the file, which the next append catches as a short file — worse
    // is the other order, where two writers both pass the check above and both
    // append, producing a file that is the right length and the wrong content.
    let claimed = {
        let id = id.clone();
        let written = body.len() as i64;
        let from = session.received;
        state
            .db
            .call(move |conn| uploads::advance(conn, &id, from, written))
            .await?
    };
    if !claimed {
        // Somebody else moved it between the read and here. Re-read so the
        // client is told the truth rather than what was true a moment ago.
        let fresh = owned_session(&state, caller.as_str(), &id).await?;
        return Err(offset_conflict(&fresh));
    }

    // Opened per chunk rather than held across requests: a `File` handle cannot
    // outlive a request without a registry of open handles keyed by session,
    // which is a second source of truth about the same thing. `append(true)`
    // rather than a seek so the write cannot land anywhere but the end.
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .await
        .map_err(|_| ApiError::not_found("Upload not found"))?;
    if let Err(e) = file.write_all(&body).await {
        // Put the offset back: the row promised bytes that are not there, and
        // leaving it advanced would corrupt the file at the next append.
        let id = id.clone();
        let back = -(body.len() as i64);
        let from = end as i64;
        let _ = state
            .db
            .call(move |conn| uploads::advance(conn, &id, from, back))
            .await;
        return Err(ApiError::Internal(e.into()));
    }
    file.flush()
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;

    Ok(Json(serde_json::json!({
        "offset": end,
        "size": session.declared_size,
    }))
    .into_response())
}

/// A 409 that carries the offset the client should resume from.
///
/// The number is the point. A bare "conflict" makes the client guess or
/// re-probe; this lets it seek and carry on within the same error path.
fn offset_conflict(session: &Session) -> ApiError {
    ApiError::conflict(format!(
        "Upload is at offset {}; resume from there",
        session.received
    ))
}

/// `GET /api/uploads/{id}` — where am I?
async fn status(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(id): Path<String>,
) -> ApiResult<Json<SessionView>> {
    let session = owned_session(&state, caller.as_str(), &id).await?;
    // Polling counts as attendance, so an upload a person is watching but not
    // feeding does not age out from under them.
    let touched = id.clone();
    state
        .db
        .call(move |conn| uploads::touch(conn, &touched))
        .await?;
    Ok(Json(SessionView::from(&session)))
}

/// `DELETE /api/uploads/{id}` — give up, and give the disk back.
async fn abort_session(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let session = owned_session(&state, caller.as_str(), &id).await?;
    discard(&state, &session).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Remove a session's bytes and its row, in that order.
///
/// File first: a row without a file is a session that fails politely, a file
/// without a row is disk nothing will ever reclaim.
async fn discard(state: &AppState, session: &Session) {
    if let Ok(path) = temp_path(state, session) {
        let _ = tokio::fs::remove_file(&path).await;
    }
    let id = session.id.clone();
    let _ = state.db.call(move |conn| uploads::delete(conn, &id)).await;
}

// ---------------------------------------------------------------- finish --

/// `POST /api/uploads/{id}/finish`
async fn finish(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let session = owned_session(&state, caller.as_str(), &id).await?;

    if session.received != session.declared_size {
        return Err(ApiError::bad_request(format!(
            "Upload is incomplete: {} of {} bytes received",
            session.received, session.declared_size
        )));
    }

    let path = temp_path(&state, &session)?;

    // One pass over the assembled file. This is the check that means anything:
    // it covers the wire, the ordering of the chunks, and the disk, and it
    // yields the content hash the stores are addressed by.
    let (digest, size) = hash_file(&path).await?;

    // A length mismatch here means the file on disk disagrees with the row that
    // has been counting it — a failed write whose offset rollback did not land,
    // or something outside this server touching the directory.
    if size != session.declared_size as u64 {
        discard(&state, &session).await;
        return Err(ApiError::Internal(anyhow::anyhow!(
            "assembled upload is {size} bytes, expected {}",
            session.declared_size
        )));
    }

    if let Some(expected) = session.sha256.as_deref() {
        if expected != digest {
            // The bytes that arrived are not the bytes the client has. Destroy
            // them: keeping a file known to be wrong, under a name derived from
            // its own contents, would quietly publish corruption as content.
            discard(&state, &session).await;
            return Err(ApiError::bad_request(
                "Uploaded data does not match the declared sha256 checksum",
            ));
        }
    }

    let response = match session.kind {
        UploadKind::File => {
            crate::routes::files::finalize_upload(&state, &caller, &session, &path, &digest).await
        }
        UploadKind::Image => {
            crate::routes::images::finalize_upload(&state, &session, &path, &digest).await
        }
        UploadKind::Site => {
            crate::routes::sites::finalize_upload(&state, &caller, &session, &path, &digest).await
        }
    };

    match response {
        Ok(resp) => {
            // The finaliser consumed the temp file by renaming it, or copied
            // what it needed; either way the session is over. Remove the row,
            // and the temp file if it somehow survived.
            discard(&state, &session).await;
            Ok(resp)
        }
        Err(e) => {
            // Leave the bytes alone. The upload succeeded — it is the
            // *commit* that failed (an unpayable site, a room that filled up
            // while the transfer ran), and those are worth retrying without
            // re-sending 4 GB. The sweep reclaims it if nobody does.
            Err(e)
        }
    }
}

/// sha-256 and length of a file, read in bounded pieces.
///
/// `tokio::fs::read` would be one line and would defeat the entire module: the
/// point of everything above is that no code path holds a whole upload.
pub(crate) async fn hash_file(path: &FsPath) -> ApiResult<(String, u64)> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    let mut hasher = Sha256::new();
    let mut total: u64 = 0;
    // 1 MB: large enough that the syscall overhead is noise on a 4 GB file,
    // small enough to be an unremarkable allocation.
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .await
            .map_err(|e| ApiError::Internal(e.into()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((hex::encode(hasher.finalize()), total))
}

// ----------------------------------------------------------------- sweep --

/// Reclaim the disk held by sessions nobody came back for.
///
/// Spawned once at startup by `lib.rs`. Deliberately conservative: it removes
/// only what has been silent for [`SESSION_TTL_MS`], and the status endpoint
/// resets that clock, so the only thing it collects is genuinely abandoned.
pub async fn sweep_abandoned(state: AppState) {
    let cutoff = crate::db::now_ms() - SESSION_TTL_MS;
    let stale = match state
        .db
        .call(move |conn| uploads::stale(conn, cutoff, 100))
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("upload sweep could not list sessions: {e}");
            return;
        }
    };
    if stale.is_empty() {
        return;
    }
    let n = stale.len();
    let mut bytes = 0i64;
    for session in &stale {
        bytes += session.received;
        discard(&state, session).await;
    }
    tracing::info!("upload sweep reclaimed {n} abandoned session(s), {bytes} bytes");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_declared_digest_is_normalised_or_refused() {
        assert_eq!(normalise_digest(None).unwrap(), None);
        assert_eq!(normalise_digest(Some("   ")).unwrap(), None);

        let sixty_four = "A".repeat(64);
        assert_eq!(
            normalise_digest(Some(&sixty_four)).unwrap().unwrap(),
            "a".repeat(64),
            "hex is compared lowercase, so it is stored lowercase"
        );

        // Too short, too long, and not hex — all refused at `begin`, which is
        // the only moment refusing them saves anybody a 4 GB upload.
        assert!(normalise_digest(Some(&"a".repeat(63))).is_err());
        assert!(normalise_digest(Some(&"a".repeat(65))).is_err());
        assert!(normalise_digest(Some(&"g".repeat(64))).is_err());
    }

    #[tokio::test]
    async fn hashing_a_file_matches_hashing_it_whole() {
        let dir = tempdir();
        let path = dir.join("sample.bin");
        // Deliberately not a multiple of the read buffer, so the final partial
        // read is exercised.
        let data: Vec<u8> = (0..(1024 * 1024 * 2 + 12345))
            .map(|i| (i % 251) as u8)
            .collect();
        tokio::fs::write(&path, &data).await.unwrap();

        let (digest, size) = hash_file(&path).await.unwrap();
        assert_eq!(size, data.len() as u64);
        assert_eq!(digest, hex::encode(Sha256::digest(&data)));
    }

    #[tokio::test]
    async fn an_empty_file_hashes_to_the_empty_digest() {
        let dir = tempdir();
        let path = dir.join("empty.bin");
        tokio::fs::write(&path, b"").await.unwrap();
        let (digest, size) = hash_file(&path).await.unwrap();
        assert_eq!(size, 0);
        assert_eq!(digest, hex::encode(Sha256::digest(b"")));
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ps-upload-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
