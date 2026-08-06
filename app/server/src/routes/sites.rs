//! Web publishing (docs/API.md §16.2).
//!
//! Pay the publish price (default 1 CRO, `PS_PUBLISH_PRICE_CRO`) to the
//! operator's FruitNation wallet and this server hosts your page: a single
//! HTML document (uploaded or pasted) or a zip carrying `index.html` plus its
//! assets. The result is served publicly at `/sites/{id}/`, recorded in
//! SQLite, and indexed into search (kind `site`, global visibility like
//! knowledge). **Any signed-in user may delete any site** — the requirement,
//! verbatim: this is a shared wall, and the community can tear down what
//! offends it. The payment already happened, so deletion is not a refund.
//!
//! * `POST   /api/sites?title=…&txHash=…` — raw body: HTML bytes or a zip
//! * `GET    /api/sites`                  — newest first
//! * `DELETE /api/sites/{id}`             — any signed-in user
//! * `GET    /sites/{id}/{*path}`         — the hosting itself, public
//!
//! # Serving user HTML without handing over the app's origin
//!
//! This server's own rule (`routes/files.rs`) is that uploaded HTML must
//! never execute on this origin — the client keeps its recovery phrase in
//! `localStorage` when the user opts into staying signed in, and a hostile
//! published page could read it. Hosting is the one feature whose entire
//! point is executing uploaded HTML, so the escape hatch is CSP: every
//! `/sites/…` response carries `Content-Security-Policy: sandbox
//! allow-scripts …` *without* `allow-same-origin`, which makes the browser
//! run the document in an opaque origin. Scripts work; `localStorage`,
//! cookies and same-origin `fetch` credentials of the app do not exist
//! there. The global `X-Frame-Options: DENY` additionally keeps published
//! pages out of iframes entirely — they open as top-level tabs.

use std::collections::HashMap;
use std::io::Read;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::auth::AuthUser;
use crate::db::now_ms;
use crate::db::sites::{self, NewSite, Site};
use crate::error::{ApiError, ApiResult};
use crate::payment::{self, Purpose};
use crate::AppState;

/// Upload ceiling — the same 25 MB as attachments, for the same reason: the
/// body is held in memory.
pub const MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;

/// Unpacked ceiling. A zip is allowed to inflate, but 64 MB of hosting for
/// 1 CRO is where generosity stops being generosity.
const MAX_UNPACKED_BYTES: u64 = 64 * 1024 * 1024;

/// Files per site. Enough for any hand-made page and most exported ones.
const MAX_SITE_FILES: usize = 500;

/// Sites this server will host at once. The payment already bounds the
/// *rate*; this bounds the *disk*.
const MAX_SITES: i64 = 500;

const MAX_TITLE_CHARS: usize = 100;

/// How much of a site's front page feeds the search index.
const INDEX_TEXT_CHARS: usize = 2000;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/sites", post(publish).get(list_sites))
        .route("/sites/{id}", axum::routing::delete(remove))
        // Innermost wins: lift the 100 KB API default for the upload.
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
}

/// The public hosting router, mounted at the root — *not* under `/api`,
/// because these URLs are handed to browsers as ordinary links.
pub fn serve_router() -> Router<AppState> {
    Router::new()
        .route("/sites/{id}", get(serve_root_redirect))
        .route("/sites/{id}/", get(serve_index))
        .route("/sites/{id}/{*path}", get(serve_asset))
}

#[derive(Debug, Deserialize)]
struct PublishParams {
    title: Option<String>,
    #[serde(rename = "txHash")]
    tx_hash: Option<String>,
}

/// Site titles: 1–100 chars after trimming, no markup or control characters —
/// the same character policy as room names, because titles render in lists.
fn site_title(raw: Option<&str>) -> ApiResult<String> {
    let raw = raw.ok_or_else(|| crate::validate::required("title", "Title"))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ApiError::field("title", "Title is required"));
    }
    if trimmed.chars().count() > MAX_TITLE_CHARS {
        return Err(ApiError::field(
            "title",
            "Title must be at most 100 characters",
        ));
    }
    if trimmed.chars().any(|c| {
        matches!(c, '<' | '>' | '{' | '}' | ';' | '"' | '\'' | '`' | '\\')
            || (c as u32) <= 0x1f
            || c == '\u{7f}'
    }) {
        return Err(ApiError::field(
            "title",
            "Title contains invalid characters",
        ));
    }
    Ok(trimmed.to_owned())
}

/// `POST /api/sites?title=…&txHash=…`
///
/// Order matters: the upload is parsed and validated **before** the payment
/// is verified and burned, so a malformed zip costs nothing.
async fn publish(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Query(params): Query<PublishParams>,
    body: Bytes,
) -> ApiResult<Response> {
    let title = site_title(params.title.as_deref())?;
    let tx_hash = params
        .tx_hash
        .as_deref()
        .ok_or_else(|| crate::validate::required("txHash", "Transaction hash"))?;
    if body.is_empty() {
        return Err(ApiError::bad_request("Empty upload"));
    }

    let hosted = state.db.call(|conn| sites::count(conn)).await?;
    if hosted >= MAX_SITES {
        return Err(ApiError::bad_request(
            "This server is hosting its maximum number of sites",
        ));
    }

    // Parse first, pay later.
    let files = if body.starts_with(b"PK\x03\x04") {
        unpack_zip(std::io::Cursor::new(&body[..]))?
    } else {
        // A single document — pasted text or an uploaded .html file. Whatever
        // it is, the browser will render it; the sandbox CSP is the guard.
        vec![("index.html".to_owned(), body.to_vec())]
    };

    commit_site(&state, &caller, title, tx_hash, files).await
}

/// Pay for, store and index an already-unpacked site.
///
/// The half of publishing that does not care how the bytes arrived, so the
/// single-shot route and the chunked-session route cannot drift on the parts
/// that involve money and disk.
///
/// Order is deliberate and unchanged: the archive is parsed before the payment
/// is verified, so a corrupt upload is refused without taking anyone's money,
/// and the payment is verified before anything is written, so the disk is only
/// spent on a paid site.
async fn commit_site(
    state: &AppState,
    caller: &pocketskynet_core::WalletAddress,
    title: String,
    tx_hash: &str,
    files: Vec<(String, Vec<u8>)>,
) -> ApiResult<Response> {
    let price = payment::price_wei(&payment::publish_price_cro());
    let amount_wei =
        payment::verify_and_record(state, caller, tx_hash, price, Purpose::Site).await?;

    // Bytes onto disk, then the row — same order as attachments: an orphan
    // directory is invisible, a row serving 404s is a broken product.
    let id = uuid::Uuid::new_v4().simple().to_string();
    let dir = state.cfg.sites_dir().join(&id);
    let size_bytes: i64 = files.iter().map(|(_, b)| b.len() as i64).sum();
    let file_count = files.len() as i64;
    let index_text = files
        .iter()
        .find(|(name, _)| name == "index.html")
        .map(|(_, bytes)| readable_text(bytes, INDEX_TEXT_CHARS))
        .unwrap_or_default();

    for (name, bytes) in &files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ApiError::Internal(e.into()))?;
        }
        tokio::fs::write(&path, bytes)
            .await
            .map_err(|e| ApiError::Internal(e.into()))?;
    }

    let new = NewSite {
        id: id.clone(),
        owner_address: caller.as_str().to_owned(),
        title: title.clone(),
        tx_hash: payment::normalize_tx_hash(tx_hash)?,
        amount_wei,
        size_bytes,
        file_count,
        created_at: now_ms(),
    };
    let owner = caller.as_str().to_owned();
    let search_body = if index_text.is_empty() {
        title.clone()
    } else {
        format!("{title} {index_text}")
    };
    let site = state
        .db
        .call(move |conn| {
            let site = sites::create(conn, new)?;
            crate::search::store::index_site(
                conn,
                &site.id,
                &owner,
                &search_body,
                site.created_at,
            )?;
            Ok(site)
        })
        .await?;

    let _ = state.log.append_audit(
        "site_published",
        Some(caller),
        json!({ "siteId": site.id, "title": site.title, "txHash": site.tx_hash,
                "sizeBytes": site.size_bytes, "fileCount": site.file_count }),
    );
    Ok((StatusCode::CREATED, Json(site)).into_response())
}

/// Commit a finished upload session as a published site.
///
/// The archive is unpacked from the temp file in place — the reason
/// [`unpack_zip`] is generic — so publishing a large zip costs the 64 MB of
/// unpacked output rather than the size of the archive plus its contents.
///
/// The `txHash` rides in the session's `extra` field. It is checked here rather
/// than at `begin` on purpose: verifying a payment against an upload that may
/// never finish would take money for nothing, and the whole point of the
/// session protocol is that starting one is cheap and abandoning one is normal.
pub(crate) async fn finalize_upload(
    state: &AppState,
    caller: &pocketskynet_core::WalletAddress,
    session: &crate::db::uploads::Session,
    temp_path: &std::path::Path,
    _digest: &str,
) -> ApiResult<Response> {
    // A site's title arrives as the caption; fall back to the filename so a
    // client that only set one of the two still publishes.
    let raw_title = Some(session.caption.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or(session.filename.trim());
    let title = site_title(Some(raw_title))?;
    let tx_hash = session.extra.trim();
    if tx_hash.is_empty() {
        return Err(crate::validate::required("txHash", "Transaction hash"));
    }

    let hosted = state.db.call(|conn| sites::count(conn)).await?;
    if hosted >= MAX_SITES {
        return Err(ApiError::bad_request(
            "This server is hosting its maximum number of sites",
        ));
    }

    // `zip` is a blocking, seeking reader and this can be a 4 GB archive, so it
    // runs on the blocking pool rather than stalling a runtime worker for the
    // duration.
    let path = temp_path.to_owned();
    let files = tokio::task::spawn_blocking(move || -> ApiResult<Vec<(String, Vec<u8>)>> {
        let mut head = [0u8; 4];
        let mut file =
            std::fs::File::open(&path).map_err(|e| ApiError::Internal(anyhow::Error::new(e)))?;
        use std::io::{Read, Seek};
        let n = file
            .read(&mut head)
            .map_err(|e| ApiError::Internal(anyhow::Error::new(e)))?;
        file.rewind()
            .map_err(|e| ApiError::Internal(anyhow::Error::new(e)))?;

        if n == 4 && &head == b"PK\x03\x04" {
            unpack_zip(file)
        } else {
            // A single document, same as the raw path. Bounded by the unpacked
            // ceiling: a 4 GB "html file" is not a page.
            let mut bytes = Vec::new();
            (&mut file)
                .take(MAX_UNPACKED_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| ApiError::Internal(anyhow::Error::new(e)))?;
            if bytes.len() as u64 > MAX_UNPACKED_BYTES {
                return Err(ApiError::bad_request(
                    "The unpacked site is too large (max 64 MB)",
                ));
            }
            Ok(vec![("index.html".to_owned(), bytes)])
        }
    })
    .await
    .map_err(|e| ApiError::Internal(anyhow::Error::new(e)))??;

    commit_site(state, caller, title, tx_hash, files).await
}

#[derive(Debug, Deserialize)]
struct ListParams {
    limit: Option<i64>,
}

/// `GET /api/sites` — newest first. Full-text search goes through
/// `GET /api/search?kind=site`, where every site is indexed.
///
/// `shareBase` is the base URL *other devices* should use — Tailscale
/// address first, then LAN, `null` when the server is loopback-only (see
/// [`crate::share_base`]). The client prefixes it onto each site's relative
/// `url` so what a card shows is an address that works off this machine.
async fn list_sites(
    State(state): State<AppState>,
    AuthUser(_caller): AuthUser,
    Query(params): Query<ListParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let limit = params.limit.unwrap_or(100).clamp(1, 500) as usize;
    let sites = state.db.call(move |conn| sites::list(conn, limit)).await?;
    Ok(Json(json!({
        "sites": sites,
        "shareBase": crate::share_base(&state.cfg),
    })))
}

/// `DELETE /api/sites/{id}` — any signed-in user, deliberately.
async fn remove(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let checked = site_id(&id)?;

    let lookup = checked.clone();
    let site: Option<Site> = state
        .db
        .call(move |conn| sites::read(conn, &lookup))
        .await?;
    let Some(site) = site else {
        return Err(ApiError::not_found("Site not found"));
    };

    let delete_id = checked.clone();
    state
        .db
        .call(move |conn| sites::delete(conn, &delete_id))
        .await?;
    // Row first, files second: the moment the row is gone the serving path
    // 404s, so a failed directory removal leaks disk, never content.
    if let Err(e) = tokio::fs::remove_dir_all(state.cfg.sites_dir().join(&checked)).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(site = %checked, error = %e, "site directory could not be removed");
        }
    }

    let _ = state.log.append_audit(
        "site_removed",
        Some(&caller),
        json!({ "siteId": site.id, "title": site.title, "owner": site.owner_address }),
    );
    Ok(super::message("Site removed"))
}

// ---------------------------------------------------------------- serving --

async fn serve_root_redirect(Path(id): Path<String>) -> Response {
    match site_id(&id) {
        // Relative asset links inside the page need the trailing slash.
        Ok(id) => Redirect::permanent(&format!("/sites/{id}/")).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn serve_index(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    serve_file(&state, &id, "index.html").await
}

async fn serve_asset(
    State(state): State<AppState>,
    Path((id, path)): Path<(String, String)>,
) -> Response {
    serve_file(&state, &id, &path).await
}

/// Site ids are the exact shape this server mints: 32 lowercase hex chars.
/// Anything else never touches the filesystem.
fn site_id(raw: &str) -> ApiResult<String> {
    if raw.len() == 32
        && raw
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        Ok(raw.to_owned())
    } else {
        Err(ApiError::not_found("Site not found"))
    }
}

/// One hosted file. Public: hosting is the product, and these URLs are
/// pasted into chats and opened by people who are not signed in.
async fn serve_file(state: &AppState, raw_id: &str, raw_path: &str) -> Response {
    let Ok(id) = site_id(raw_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // The row is the authority: a deleted site stops serving even if its
    // directory somehow survived.
    let lookup = id.clone();
    let exists = state
        .db
        .call(move |conn| sites::read(conn, &lookup))
        .await
        .ok()
        .flatten()
        .is_some();
    if !exists {
        return StatusCode::NOT_FOUND.into_response();
    }

    let mut rel = raw_path.trim_start_matches('/').to_owned();
    if rel.is_empty() || rel.ends_with('/') {
        rel.push_str("index.html");
    }
    // Belt over the suspenders unpack_zip already provides: nothing that
    // could climb, nothing hidden, nothing with a byte a path API might
    // reinterpret.
    if rel.split('/').any(|seg| {
        seg.is_empty() || seg == "." || seg == ".." || seg.contains('\\') || seg.contains('\0')
    }) {
        return StatusCode::NOT_FOUND.into_response();
    }

    let path = state.cfg.sites_dir().join(&id).join(&rel);
    let Ok(bytes) = tokio::fs::read(&path).await else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let mut response = (StatusCode::OK, bytes).into_response();
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, content_type_for(&rel));
    // The load-bearing header — see the module docs. `allow-same-origin` is
    // the one grant that must never appear here.
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "sandbox allow-scripts allow-forms allow-popups allow-modals allow-downloads",
        ),
    );
    // Deletable at any moment by any user, so nothing may cache it long.
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

/// Content types for the handful of extensions a small site actually ships.
/// Everything unknown is octet-stream — download, never execute.
fn content_type_for(path: &str) -> HeaderValue {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    let mime = match ext.as_deref() {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("txt") => "text/plain; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("mp3") => "audio/mpeg",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    };
    HeaderValue::from_static(mime)
}

// -------------------------------------------------------------- unpacking --

/// Unpack a site zip into `(relative_path, bytes)` pairs, enforcing every
/// bound, and normalising the one shape people actually upload: an archive
/// whose content sits inside a single top-level folder.
/// Generic over the reader so a 4 GB archive can be unpacked straight from the
/// file it was uploaded into, rather than from a copy of it in memory. `zip`
/// needs `Seek` to read the central directory, which a `File` gives and a
/// stream would not — the archive is read in place, and only the *unpacked*
/// bytes are held, which `MAX_UNPACKED_BYTES` already bounds at 64 MB.
fn unpack_zip<R: std::io::Read + std::io::Seek>(reader: R) -> ApiResult<Vec<(String, Vec<u8>)>> {
    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|_| ApiError::bad_request("The upload is not a readable zip archive"))?;

    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut total: u64 = 0;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|_| ApiError::bad_request("The zip archive is corrupt"))?;
        if entry.is_dir() {
            continue;
        }
        // `enclosed_name` refuses absolute paths and `..` — the zip-slip
        // guard. Entries it refuses are hostile; fail the whole upload.
        let Some(name) = entry.enclosed_name() else {
            return Err(ApiError::bad_request(
                "The zip archive contains an unsafe path",
            ));
        };
        let rel = name
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        // macOS zips ship resource-fork noise; hidden files are dropped, not
        // fatal, so "compress" on a Mac just works.
        if rel
            .split('/')
            .any(|seg| seg.starts_with('.') || seg == "__MACOSX")
        {
            continue;
        }
        if rel.contains('\0') || rel.contains('\\') {
            return Err(ApiError::bad_request(
                "The zip archive contains an unsafe path",
            ));
        }

        if files.len() >= MAX_SITE_FILES {
            return Err(ApiError::bad_request(
                "The zip archive contains too many files (max 500)",
            ));
        }
        // The declared size first (cheap refusal), then a `take`-bounded read
        // of the actual bytes, so a lying header cannot smuggle a
        // decompression bomb past the check.
        if total.saturating_add(entry.size()) > MAX_UNPACKED_BYTES {
            return Err(ApiError::bad_request(
                "The unpacked site is too large (max 64 MB)",
            ));
        }
        let budget = MAX_UNPACKED_BYTES - total;
        let mut bytes = Vec::new();
        (&mut entry)
            .take(budget + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ApiError::bad_request("The zip archive is corrupt"))?;
        if bytes.len() as u64 > budget {
            return Err(ApiError::bad_request(
                "The unpacked site is too large (max 64 MB)",
            ));
        }
        total += bytes.len() as u64;
        files.push((rel, bytes));
    }

    if files.is_empty() {
        return Err(ApiError::bad_request("The zip archive is empty"));
    }

    // `site/index.html` → `index.html`: exported folders arrive wrapped.
    if !files.iter().any(|(n, _)| n == "index.html") {
        let prefix: Option<String> = files
            .first()
            .and_then(|(n, _)| n.split_once('/'))
            .map(|(head, _)| head.to_owned());
        if let Some(prefix) = prefix {
            let wrapped = files
                .iter()
                .all(|(n, _)| n.starts_with(&format!("{prefix}/")));
            if wrapped {
                for (name, _) in &mut files {
                    *name = name[prefix.len() + 1..].to_owned();
                }
            }
        }
    }

    if !files.iter().any(|(n, _)| n == "index.html") {
        return Err(ApiError::bad_request(
            "The zip archive has no index.html at its root",
        ));
    }

    // Two entries unpacking to one path would make serving depend on write
    // order. Refuse rather than pick.
    let mut seen = HashMap::new();
    for (name, _) in &files {
        if seen.insert(name.clone(), ()).is_some() {
            return Err(ApiError::bad_request(
                "The zip archive contains duplicate paths",
            ));
        }
    }

    Ok(files)
}

/// The readable text of an HTML document, for the search index: scripts and
/// styles dropped, tags stripped, whitespace collapsed, length capped.
///
/// Tag names are ASCII, so the case-insensitive probes work on raw bytes at
/// the char's byte offset — never on a `to_lowercase()` copy, whose byte
/// offsets can drift from the original on non-ASCII input.
fn readable_text(html: &[u8], cap: usize) -> String {
    let html = String::from_utf8_lossy(html);
    let bytes = html.as_bytes();
    let probe = |i: usize, needle: &[u8]| {
        bytes.len() >= i + needle.len() && bytes[i..i + needle.len()].eq_ignore_ascii_case(needle)
    };

    let mut out = String::new();
    let mut written = 0usize;
    let mut chars = html.char_indices();
    let mut skip_until: Option<&'static [u8]> = None;
    let mut in_tag = false;

    while let Some((i, c)) = chars.next() {
        if let Some(closer) = skip_until {
            if probe(i, closer) {
                skip_until = None;
                // The closer is pure ASCII, so chars == bytes inside it.
                for _ in 0..closer.len() - 1 {
                    chars.next();
                }
            }
            continue;
        }
        if c == '<' {
            if probe(i, b"<script") {
                skip_until = Some(b"</script>");
                continue;
            }
            if probe(i, b"<style") {
                skip_until = Some(b"</style>");
                continue;
            }
            in_tag = true;
            continue;
        }
        if c == '>' && in_tag {
            in_tag = false;
            // Tag boundaries separate words: `<p>a</p><p>b</p>` is "a b".
            if !out.ends_with(' ') && !out.is_empty() {
                out.push(' ');
            }
            continue;
        }
        if in_tag {
            continue;
        }
        if c.is_whitespace() {
            if !out.ends_with(' ') && !out.is_empty() {
                out.push(' ');
            }
        } else {
            out.push(c);
            written += 1;
        }
        if written >= cap {
            break;
        }
    }
    out.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    use crate::routes::build;
    use crate::test_support::{register, send, send_raw, state, wallet};

    fn tx(byte: char) -> String {
        format!("0x{}", byte.to_string().repeat(64))
    }

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(&mut buf);
        let options = zip::write::SimpleFileOptions::default();
        for (name, bytes) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
        buf.into_inner()
    }

    #[test]
    fn readable_text_strips_markup_scripts_and_styles() {
        let html = br#"<html><head><title>My Page</title>
            <style>body { color: red }</style>
            <script>alert("nope")</script></head>
            <body><h1>Hello</h1><p>the launch    code is #secret</p></body></html>"#;
        let text = readable_text(html, 2000);
        assert_eq!(text, "My Page Hello the launch code is #secret");
        assert!(!text.contains("alert"));
        assert!(!text.contains("color"));

        // The cap holds even against tag-free input.
        assert_eq!(readable_text(&[b'a'; 5000], 10).chars().count(), 10);
    }

    #[test]
    fn zips_unpack_with_wrapping_folder_stripped() {
        let z = zip_of(&[
            ("mysite/index.html", b"<h1>hi</h1>" as &[u8]),
            ("mysite/css/app.css", b"body{}"),
            ("__MACOSX/index.html", b"resource fork noise"),
            ("mysite/.DS_Store", b"junk"),
        ]);
        let files = unpack_zip(std::io::Cursor::new(&z)).unwrap();
        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["index.html", "css/app.css"]);
    }

    #[test]
    fn a_zip_without_an_index_is_refused() {
        let z = zip_of(&[("about.html", b"<h1>about</h1>" as &[u8])]);
        let err = unpack_zip(std::io::Cursor::new(&z)).unwrap_err();
        assert!(err.to_string().contains("no index.html"));
    }

    #[test]
    fn hostile_zip_paths_are_refused() {
        // `zip`'s writer will happily *create* a traversal entry name; the
        // reader's `enclosed_name` is what must refuse it on the way in.
        // Build a normal archive and rewrite its entry name in place —
        // same length, so every offset in the file stays valid.
        let benign = b"AAABBB/evil.html"; // 16 bytes
        let hostile = b"../../evil.html\0"; // 16 bytes; also carries a NUL
        let z = zip_of(&[(
            std::str::from_utf8(benign).unwrap(),
            b"<h1>evil</h1>" as &[u8],
        )]);
        let mut raw = z.clone();
        let mut replaced = 0;
        let mut i = 0;
        while i + benign.len() <= raw.len() {
            if &raw[i..i + benign.len()] == benign {
                raw[i..i + benign.len()].copy_from_slice(hostile);
                replaced += 1;
                i += benign.len();
            } else {
                i += 1;
            }
        }
        assert!(replaced >= 2, "local header and central directory");

        let result = unpack_zip(std::io::Cursor::new(&raw));
        assert!(
            result.is_err(),
            "a traversal entry must fail the upload, got {:?}",
            result.map(|f| f.into_iter().map(|(n, _)| n).collect::<Vec<_>>())
        );
    }

    #[test]
    fn site_ids_only_ever_have_the_minted_shape() {
        assert!(site_id(&"a".repeat(32)).is_ok());
        for bad in [
            "".to_owned(),
            "short".to_owned(),
            "A".repeat(32),                 // uppercase
            format!("{}/", "a".repeat(31)), // separator
            "..".repeat(16),                // dots
            "g".repeat(32),                 // not hex
        ] {
            assert!(site_id(&bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[tokio::test]
    async fn publish_serve_and_community_delete_round_trip() {
        let state = state("sites-roundtrip");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state);

        let html = b"<html><body><h1>Terminator fan page</h1></body></html>";
        let response = send_raw(
            &router,
            "POST",
            &format!("/api/sites?title=Fan%20page&txHash={}", tx('a')),
            Some(&alice_token),
            html.to_vec(),
            "text/html",
        )
        .await;
        assert_eq!(response.status, StatusCode::CREATED, "{:?}", response.body);
        assert_eq!(response.body["title"], "Fan page");
        assert_eq!(response.body["username"], "alice");
        assert_eq!(response.body["fileCount"], 1);
        let id = response.body["id"].as_str().unwrap().to_owned();
        let url = response.body["url"].as_str().unwrap().to_owned();
        assert_eq!(url, format!("/sites/{id}/"));

        // Served publicly — no token — with the sandbox CSP and real type.
        let response = send(&router, "GET", &url, None, None).await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(
            response.header("content-type").as_deref(),
            Some("text/html; charset=utf-8")
        );
        let csp = response.header("content-security-policy").unwrap();
        assert!(csp.contains("sandbox"), "{csp}");
        assert!(
            !csp.contains("allow-same-origin"),
            "allow-same-origin would hand a published page the app's localStorage: {csp}"
        );

        // Listed, and findable through search (kind=site, global).
        let response = send(&router, "GET", "/api/sites", Some(&bob_token), None).await;
        assert_eq!(response.body["sites"].as_array().unwrap().len(), 1);
        assert!(
            response.body.get("shareBase").is_some(),
            "the listing must always carry the key, null or not: {:?}",
            response.body
        );
        assert!(
            response.body["shareBase"].is_null(),
            "the test config binds loopback on port 0 — there is nothing to share"
        );
        let response = send(
            &router,
            "GET",
            "/api/search?q=terminator%20fan&kind=site",
            Some(&bob_token),
            None,
        )
        .await;
        let results = response.body["results"].as_array().unwrap();
        assert_eq!(results.len(), 1, "{:?}", response.body);
        assert_eq!(results[0]["refId"].as_str().unwrap(), id);

        // Bob — not the owner — removes it. Any user can; that is the spec.
        let response = send(
            &router,
            "DELETE",
            &format!("/api/sites/{id}"),
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);

        let response = send(&router, "GET", &url, None, None).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "deleted = gone");
        let response = send(
            &router,
            "GET",
            "/api/search?q=terminator%20fan&kind=site",
            Some(&bob_token),
            None,
        )
        .await;
        assert!(response.body["results"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_zip_site_serves_its_assets_with_their_own_types() {
        let state = state("sites-zip");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        let z = zip_of(&[
            (
                "site/index.html",
                b"<link rel=stylesheet href=css/app.css>" as &[u8],
            ),
            ("site/css/app.css", b"h1 { color: cyan }"),
        ]);
        let response = send_raw(
            &router,
            "POST",
            &format!("/api/sites?title=Zipped&txHash={}", tx('b')),
            Some(&token),
            z,
            "application/zip",
        )
        .await;
        assert_eq!(response.status, StatusCode::CREATED, "{:?}", response.body);
        assert_eq!(response.body["fileCount"], 2);
        let id = response.body["id"].as_str().unwrap().to_owned();

        let response = send(
            &router,
            "GET",
            &format!("/sites/{id}/css/app.css"),
            None,
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(
            response.header("content-type").as_deref(),
            Some("text/css; charset=utf-8")
        );

        // The no-slash form redirects so relative links resolve.
        let response = send(&router, "GET", &format!("/sites/{id}"), None, None).await;
        assert_eq!(response.status, StatusCode::PERMANENT_REDIRECT);

        // Climbing out of the site directory is a 404, not a file.
        for probe in ["../../jwt.secret", "..%2F..%2Fjwt.secret", "a/../../x"] {
            let response = send(&router, "GET", &format!("/sites/{id}/{probe}"), None, None).await;
            assert_eq!(response.status, StatusCode::NOT_FOUND, "{probe}");
        }
    }

    #[tokio::test]
    async fn publishing_needs_a_token_a_title_and_a_fresh_payment() {
        let state = state("sites-validate");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        // No token.
        let response = send_raw(
            &router,
            "POST",
            &format!("/api/sites?title=x&txHash={}", tx('c')),
            None,
            b"<h1>x</h1>".to_vec(),
            "text/html",
        )
        .await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED);

        // No title.
        let response = send_raw(
            &router,
            "POST",
            &format!("/api/sites?txHash={}", tx('c')),
            Some(&token),
            b"<h1>x</h1>".to_vec(),
            "text/html",
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);

        // Publish once…
        let response = send_raw(
            &router,
            "POST",
            &format!("/api/sites?title=First&txHash={}", tx('c')),
            Some(&token),
            b"<h1>x</h1>".to_vec(),
            "text/html",
        )
        .await;
        assert_eq!(response.status, StatusCode::CREATED, "{:?}", response.body);

        // …the same tx hash does not buy a second site.
        let response = send_raw(
            &router,
            "POST",
            &format!("/api/sites?title=Second&txHash={}", tx('c')),
            Some(&token),
            b"<h1>y</h1>".to_vec(),
            "text/html",
        )
        .await;
        assert_eq!(response.status, StatusCode::CONFLICT, "{:?}", response.body);

        // A shout's spent hash cannot be recycled into hosting either.
        let response = send(
            &router,
            "POST",
            "/api/shout",
            Some(&token),
            Some(json!({ "text": "spend it", "txHash": tx('d') })),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
        let response = send_raw(
            &router,
            "POST",
            &format!("/api/sites?title=Recycled&txHash={}", tx('d')),
            Some(&token),
            b"<h1>z</h1>".to_vec(),
            "text/html",
        )
        .await;
        assert_eq!(response.status, StatusCode::CONFLICT, "{:?}", response.body);
    }
}
