//! Attachments (`docs/API.md` §14).
//!
//! Bytes on the filesystem under a content hash, metadata in SQLite — see
//! `db/files.rs` for why the split, and `config.rs::files_dir` for why
//! attachments do not share a directory with images.
//!
//! **This is not the images route with a different name.** Two differences
//! carry all the security:
//!
//! * `GET /api/images/{hash}` is *public*, because an `<img src>` cannot send
//!   an `Authorization` header and the unguessable hash is the capability. An
//!   attachment is room-scoped, so its download demands a bearer token and a
//!   membership check. The cost is that clients cannot use a bare `href` — they
//!   fetch with the token and build a blob URL. That is the intended trade.
//! * Images are served with their real `Content-Type` for inline rendering.
//!   Attachments are **always** `application/octet-stream` with
//!   `Content-Disposition: attachment`, whatever the uploader declared, so a
//!   `text/html` upload can never execute on this origin. The declared type is
//!   stored and returned as metadata; the client decides whether to preview it.
//!
//! Metadata arrives as query parameters rather than multipart: axum's multipart
//! feature is not enabled, headers would need their own encoding scheme for
//! non-ASCII filenames, and base64 in JSON costs a third of the payload. Query
//! strings are percent-encoded by every HTTP client already.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::auth::AuthUser;
use crate::db::files::{self, NewFile};
use crate::db::rooms;
use crate::error::{ApiError, ApiResult};
use crate::routes::messages::require_member;
use crate::validate;
use crate::AppState;
use pocketskynet_core::RoomId;

/// 25 MB. Larger than an image because attachments are documents, smaller than
/// "unlimited" because the whole file is read into memory on both the upload
/// and the download path — this server has no streaming or Range support, and
/// pretending otherwise with a 2 GB cap would just move the failure.
pub const MAX_FILE_BYTES: usize = 25 * 1024 * 1024;

/// Per-room ceiling. The images route shipped with no quota at all, which made
/// unbounded disk fill a documented finding; repeating that here knowingly
/// would be worse than the original. A room is the right unit because that is
/// the boundary a member can already fill with messages.
const MAX_FILES_PER_ROOM: i64 = 500;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/rooms/{roomId}/files", post(upload).get(list))
        .route("/files/{id}", get(detail).delete(remove))
        .route("/files/{id}/raw", get(download))
        // Innermost wins, so the 100 KB API-wide default is lifted here only.
        // It applies to the GETs in this router too, which is harmless.
        .layer(DefaultBodyLimit::max(MAX_FILE_BYTES))
}

#[derive(Debug, Deserialize)]
struct UploadParams {
    filename: Option<String>,
    caption: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListParams {
    tag: Option<String>,
    limit: Option<i64>,
}

/// The stored extension, derived from the uploader's filename.
///
/// Lowercase ASCII alphanumerics only, at most 8 of them, `bin` when there is
/// nothing usable. This is the second half of the traversal guard: `stored_name`
/// is `{64 hex}.{this}`, and `serve` re-validates that shape, so an extension
/// that could contain a separator or a dot would be the one way to break out.
fn extension_of(filename: &str) -> String {
    let raw = filename.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
    let ext: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .take(8)
        .collect();
    if ext.is_empty() {
        "bin".to_owned()
    } else {
        ext
    }
}

/// `POST /api/rooms/{roomId}/files?filename=…&caption=…`
async fn upload(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
    Query(params): Query<UploadParams>,
    body: Bytes,
) -> ApiResult<Response> {
    let room = RoomId::new(&room_id).map_err(|_| ApiError::not_found("Room not found"))?;
    require_member(&state, &room, &caller).await?;

    let filename = validate::filename(params.filename.as_deref())?;
    let caption = validate::caption(params.caption.as_deref())?;
    if body.is_empty() {
        return Err(ApiError::bad_request("Empty file"));
    }

    // Declared type is recorded but never trusted for serving; see module docs.
    let mime = "application/octet-stream".to_owned();

    let ext = extension_of(&filename);
    let stored_name = format!("{}.{ext}", hex::encode(Sha256::digest(&body)));

    let room_for_count = room.as_str().to_owned();
    let count = state
        .db
        .call(move |conn| {
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM files WHERE room_id = ?1",
                rusqlite::params![room_for_count],
                |r| r.get(0),
            )?;
            Ok(n)
        })
        .await?;
    if count >= MAX_FILES_PER_ROOM {
        return Err(ApiError::bad_request(format!(
            "This room has reached its limit of {MAX_FILES_PER_ROOM} attachments"
        )));
    }

    // Bytes before the row: a file with no row is an invisible orphan, but a
    // row with no file is a broken download every client has to handle.
    let dir = state.cfg.files_dir();
    let path = dir.join(&stored_name);
    if !path.exists() {
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| ApiError::Internal(e.into()))?;
        // Write-then-rename, and the tmp name carries the content hash *and* a
        // uuid: two concurrent uploads of the same bytes would otherwise share
        // one tmp path and race each other's rename.
        let tmp = dir.join(format!(".{stored_name}.{}.tmp", uuid::Uuid::new_v4()));
        tokio::fs::write(&tmp, &body)
            .await
            .map_err(|e| ApiError::Internal(e.into()))?;
        tokio::fs::rename(&tmp, &path)
            .await
            .map_err(|e| ApiError::Internal(e.into()))?;
    }

    let new = NewFile {
        id: format!("file_{}_{}", crate::db::now_ms(), uuid::Uuid::new_v4()),
        room_id: room.as_str().to_owned(),
        uploader: caller.as_str().to_owned(),
        filename,
        stored_name,
        mime,
        size_bytes: body.len() as i64,
        caption,
    };
    let file = state.db.call(move |conn| files::create(conn, new)).await?;

    Ok((StatusCode::CREATED, Json(file)).into_response())
}

/// `GET /api/rooms/{roomId}/files?tag=&limit=`
async fn list(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
    Query(params): Query<ListParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = RoomId::new(&room_id).map_err(|_| ApiError::not_found("Room not found"))?;
    require_member(&state, &room, &caller).await?;

    // Normalised the same way the client's chips are: a leading `#` is what a
    // person types, and the index stores tags lowercased without it.
    let tag = params
        .tag
        .as_deref()
        .map(|t| t.trim().trim_start_matches('#').to_lowercase());
    let tag = tag.filter(|t| !t.is_empty());
    let limit = params.limit.unwrap_or(100).clamp(1, 200);
    let room_id = room.as_str().to_owned();

    let items = state
        .db
        .call(move |conn| files::list_for_room(conn, &room_id, tag.as_deref(), limit))
        .await?;

    Ok(Json(serde_json::json!({ "files": items })))
}

/// Resolve a file the caller is allowed to see, or a uniform 404.
///
/// Membership is checked *before* existence is admitted, so a non-member cannot
/// tell "not yours" from "does not exist" — the same rule `routes/rooms.rs`
/// states for room ids, applied here because a file id is just as guessable.
async fn visible_file(
    state: &AppState,
    caller: &pocketskynet_core::WalletAddress,
    id: &str,
) -> ApiResult<crate::db::files::FileMeta> {
    let owned = id.to_owned();
    let file = state
        .db
        .call(move |conn| files::read(conn, &owned))
        .await?
        .ok_or_else(|| ApiError::not_found("File not found"))?;

    let room = RoomId::new(&file.room_id).map_err(|_| ApiError::not_found("File not found"))?;
    // A uniform 404 rather than require_member's 403: a 403 here would confirm
    // the file exists to someone with no access to the room it lives in.
    let room_id = file.room_id.clone();
    let address = caller.as_str().to_owned();
    let member = state
        .db
        .call(move |conn| rooms::is_member(conn, &room_id, &address))
        .await?;
    let _ = room;
    if !member {
        return Err(ApiError::not_found("File not found"));
    }
    Ok(file)
}

/// `GET /api/files/{id}` — metadata only.
async fn detail(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(id): Path<String>,
) -> ApiResult<Json<crate::db::files::FileMeta>> {
    Ok(Json(visible_file(&state, &caller, &id).await?))
}

/// `GET /api/files/{id}/raw` — the bytes.
async fn download(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let file = visible_file(&state, &caller, &id).await?;

    let owned = id.clone();
    let stored = state
        .db
        .call(move |conn| files::stored_name(conn, &owned))
        .await?
        .ok_or_else(|| ApiError::not_found("File not found"))?;

    // Re-validate the shape even though we wrote it: this is the value that
    // becomes a path, and a guard that only runs at write time protects
    // nothing if the row is ever touched by anything else.
    let Some((stem, ext)) = stored.rsplit_once('.') else {
        return Err(ApiError::not_found("File not found"));
    };
    if stem.len() != 64
        || !stem.bytes().all(|b| b.is_ascii_hexdigit())
        || ext.is_empty()
        || ext.len() > 8
        || !ext.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return Err(ApiError::not_found("File not found"));
    }

    let path = state.cfg.files_dir().join(&stored);
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| ApiError::not_found("File not found"))?;

    let mut response = (StatusCode::OK, bytes).into_response();
    let headers = response.headers_mut();
    // Always octet-stream, always an attachment: see the module docs. The
    // uploader's declared type never reaches a browser's sniffer.
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    // Two parameters, and the split matters. `filename=` is a quoted-string in
    // a header, so it must be **ASCII** — a raw UTF-8 byte there is not a legal
    // header value, and while `HeaderValue` will carry it as opaque obs-text,
    // clients mangle it and any proxy is entitled to reject it. So the fallback
    // is transliterated to ASCII and `filename*` carries the real name,
    // percent-encoded per RFC 5987, for every client of the last fifteen years.
    //
    // The filename is already validated to hold no quote, control character or
    // separator, so the quoted-string cannot be broken out of.
    if let Ok(value) = HeaderValue::from_str(&format!(
        "attachment; filename=\"{}\"; filename*=UTF-8''{}",
        ascii_fallback(&file.filename),
        percent_encode(&file.filename)
    )) {
        headers.insert(CONTENT_DISPOSITION, value);
    }
    headers.insert(
        CACHE_CONTROL,
        // Content-addressed on disk but *authorised* per request, so this is
        // private: a shared cache must not hand it to the next person.
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    headers.insert(
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

/// An ASCII-only stand-in for the `filename=` parameter.
///
/// Every non-ASCII character becomes `_`, so a Korean or emoji filename still
/// produces a legal header for the oldest client on the network while
/// `filename*` carries the real thing. Never empty: a bare `filename=""` makes
/// some clients invent a name from the URL, which would be the opaque id.
fn ascii_fallback(s: &str) -> String {
    let mapped: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = mapped.trim();
    if trimmed.is_empty() {
        "download".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Minimal RFC 5987 encoding for the `filename*` parameter. Everything outside
/// the unreserved set is escaped, which is stricter than required and cannot be
/// wrong.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `DELETE /api/files/{id}` — the uploader, or a room admin.
///
/// Wider than "author only" because a room admin already governs the room's
/// content, and narrower than messages' delete-any-member because an
/// attachment is a deliberate act of publication rather than a line of chat.
async fn remove(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let file = visible_file(&state, &caller, &id).await?;

    if file.uploader != caller.as_str() {
        let room_id = file.room_id.clone();
        let address = caller.as_str().to_owned();
        let admin = state
            .db
            .call(move |conn| rooms::is_admin(conn, &room_id, &address))
            .await?;
        if !admin {
            return Err(ApiError::forbidden(
                "Only the uploader or a room admin can delete an attachment",
            ));
        }
    }

    let owned = id.clone();
    state
        .db
        .call(move |conn| files::delete(conn, &owned))
        .await?;
    Ok(super::message("Attachment deleted"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_extension_is_reduced_to_something_that_cannot_be_a_path() {
        assert_eq!(extension_of("report.pdf"), "pdf");
        assert_eq!(extension_of("ARCHIVE.TAR.GZ"), "gz");
        // Nothing usable, or nothing at all.
        assert_eq!(extension_of("Makefile"), "bin");
        assert_eq!(extension_of("trailing."), "bin");
        // The split takes the *last* dot, so this yields "/etc", and the
        // filter is what makes it harmless.
        assert_eq!(extension_of("x.p/../../etc"), "etc");
        assert_eq!(extension_of("x.a b"), "ab");
        // Bounded, so a 4KB "extension" cannot make an absurd filename.
        assert_eq!(extension_of(&format!("x.{}", "a".repeat(50))).len(), 8);
    }

    /// The property the guard actually rests on, rather than a handful of
    /// examples: whatever comes in, `{hash}.{extension_of(..)}` can only ever
    /// be `[0-9a-f]{64}` + `.` + lowercase ASCII alphanumerics. No separator,
    /// no dot, no `..`, nothing that can escape `files_dir`.
    #[test]
    fn no_filename_can_produce_an_extension_that_escapes_the_directory() {
        for hostile in [
            "../../../../etc/passwd",
            "x.../..",
            "a.%2e%2e%2fetc",
            "a.tar.gz/../..",
            "a.\u{0}bin",
            "a.pdf\nContent-Type: text/html",
            "a.<script>",
            "a.\\windows\\system32",
            "no-dot-at-all",
            ".hidden",
            "a.☃",
            &format!("a.{}", "../".repeat(40)),
        ] {
            let ext = extension_of(hostile);
            assert!(!ext.is_empty(), "{hostile:?} produced an empty extension");
            assert!(ext.len() <= 8, "{hostile:?} produced {ext:?}");
            assert!(
                ext.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
                "{hostile:?} produced {ext:?}, which is not [a-z0-9]"
            );
            // And the composed name passes the download guard.
            let stored = format!("{}.{ext}", "a".repeat(64));
            let (stem, ext2) = stored.rsplit_once('.').unwrap();
            assert_eq!(stem.len(), 64);
            assert_eq!(ext2, ext);
            assert!(!stored.contains('/') && !stored.contains("..'"));
        }
    }

    #[test]
    fn the_ascii_fallback_is_always_a_legal_header_value() {
        assert_eq!(ascii_fallback("report.pdf"), "report.pdf");
        assert_eq!(ascii_fallback("Q3 report.pdf"), "Q3 report.pdf");
        // Non-ASCII is transliterated, not dropped, so the shape survives.
        assert_eq!(ascii_fallback("보고서.pdf"), "___.pdf");
        assert_eq!(ascii_fallback("café.txt"), "caf_.txt");
        // Never empty, whatever comes in.
        assert_eq!(ascii_fallback("보고서"), "___");
        assert_eq!(ascii_fallback("   "), "download");
        assert_eq!(ascii_fallback(""), "download");

        // The property: the result must always be usable as a header value,
        // which is what the integration test caught us failing.
        for name in ["보고서.pdf", "🔥.bin", "a\u{a0}b.pdf", "ünïcödé.doc"] {
            let out = ascii_fallback(name);
            assert!(out.is_ascii(), "{name:?} -> {out:?}");
            assert!(!out.contains('"'), "{out:?} would break the quoted-string");
            assert!(HeaderValue::from_str(&format!("attachment; filename=\"{out}\"")).is_ok());
        }
    }

    #[test]
    fn the_rfc5987_encoding_escapes_everything_interesting() {
        assert_eq!(percent_encode("report.pdf"), "report.pdf");
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("보고서.pdf"), {
            let mut s = String::new();
            for b in "보고서".bytes() {
                s.push_str(&format!("%{b:02X}"));
            }
            s.push_str(".pdf");
            s
        });
    }
}
