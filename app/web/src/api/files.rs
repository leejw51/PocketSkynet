//! Attachments (`docs/API.md` §14).
//!
//! Two things here are unlike every other endpoint group, both forced by the
//! server's decision that an attachment is as private as its room:
//!
//! * **Upload sends raw bytes**, not JSON, with the metadata in the query
//!   string — though the app itself now uploads through `api/uploads.rs`,
//!   which chunks; the raw route here is the legacy single-shot path.
//! * **Download is a capability URL.** `/api/files/{id}/raw` demands a bearer
//!   token, an `<img src>` cannot send one, and a 4 GB body cannot pass
//!   through the page — so [`Client::download_link`] mints a short-lived
//!   single-file token and everything (previews, players, saves) points at
//!   the URL carrying it. The server streams and honours `Range`.

use gloo_net::http::Method;

use super::{ApiError, ApiResult, Client};
use crate::api::types::FileMeta;

/// What `POST /api/files/{id}/download-token` answers.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadLink {
    /// Relative, and carrying the capability. Prefix with the client's base.
    pub url: String,
    /// Lowercase hex sha-256 of the file, so what landed on disk can be
    /// checked against what the server holds.
    pub sha256: String,
    pub size_bytes: f64,
    pub filename: String,
    /// The thumbnail, carrying the same capability — present only when one
    /// exists, so a bubble never renders an `<img>` it knows will 404.
    /// `default` keeps an older server (which omits the field) deserializing.
    #[serde(default)]
    pub thumb_url: Option<String>,
}

/// One tile of a room's gallery — `GET /api/rooms/{roomId}/media`.
///
/// Two stores merged by time (see the server's `routes/gallery.rs`), so half
/// the fields are per-source: an attachment has `id` and `filename`, hosted
/// media has `name` and `message_id`. `url` and `thumb_url` are ready to use
/// as handed over — attachment URLs already carry their `?dl=` capability, so
/// the grid renders with no further API calls.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GalleryItem {
    /// `"image"` or `"video"`.
    pub kind: String,
    /// `"attachment"` or `"hosted"`.
    pub source: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub caption: Option<String>,
    pub sender: String,
    pub url: String,
    #[serde(default)]
    pub thumb_url: Option<String>,
    pub created_at: String,
    pub created_at_ms: f64,
}

impl GalleryItem {
    pub fn is_video(&self) -> bool {
        self.kind == "video"
    }

    /// A stable identity for keyed lists, across both sources.
    pub fn key(&self) -> String {
        match (&self.id, &self.message_id, &self.name) {
            (Some(id), _, _) => id.clone(),
            (None, Some(mid), Some(name)) => format!("{mid}:{name}"),
            _ => format!("{}:{}", self.created_at_ms, self.url),
        }
    }
}

/// One page of a room's gallery.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GalleryPage {
    pub items: Vec<GalleryItem>,
    pub has_more: bool,
}

impl Client {
    /// Upload `bytes` to a room. `caption` may be empty; its `#hashtags` are
    /// what make the attachment findable, and the server extracts them.
    pub async fn upload_file(
        &self,
        room_id: &str,
        filename: &str,
        caption: &str,
        bytes: Vec<u8>,
    ) -> ApiResult<FileMeta> {
        let path = format!(
            "/api/rooms/{}/files?filename={}&caption={}",
            encode(room_id),
            encode(filename),
            encode(caption),
        );
        let req = self
            .build(Method::POST, &path)
            // Declared but not trusted: the server stores every attachment as
            // octet-stream and serves it as a download regardless.
            .header("Content-Type", "application/octet-stream")
            .body(js_sys::Uint8Array::from(bytes.as_slice()))
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let resp = req
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        super::decode(resp).await
    }

    /// One attachment's metadata. Used by the message embed, which knows only
    /// the id it found in a message body.
    pub async fn file(&self, id: &str) -> ApiResult<FileMeta> {
        self.send(Method::GET, &format!("/api/files/{}", encode(id)))
            .await
    }

    /// A room's attachments, newest first. `tag` filters on one exact hashtag.
    pub async fn list_files(&self, room_id: &str, tag: Option<&str>) -> ApiResult<Vec<FileMeta>> {
        #[derive(serde::Deserialize)]
        struct Listing {
            files: Vec<FileMeta>,
        }
        let mut path = format!("/api/rooms/{}/files", encode(room_id));
        if let Some(tag) = tag.map(str::trim).filter(|t| !t.is_empty()) {
            path.push_str(&format!("?tag={}", encode(tag)));
        }
        let listing: Listing = self.send(Method::GET, &path).await?;
        Ok(listing.files)
    }

    pub async fn delete_file(&self, id: &str) -> ApiResult<()> {
        self.send_ok_empty(Method::DELETE, &format!("/api/files/{}", encode(id)))
            .await
    }

    /// Post a captured poster frame for a **video** attachment the caller
    /// uploaded. The server re-encodes it, so the only contract is "a
    /// decodable image"; JPEG is what `capture.rs` produces.
    pub async fn upload_thumbnail(&self, id: &str, jpeg: Vec<u8>) -> ApiResult<()> {
        let path = format!("/api/files/{}/thumbnail", encode(id));
        let req = self
            .build(Method::POST, &path)
            .header("Content-Type", "image/jpeg")
            .body(js_sys::Uint8Array::from(jpeg.as_slice()))
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let resp = req
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let status = resp.status();
        if (200..300).contains(&status) {
            Ok(())
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(ApiError::from_response(status, &body))
        }
    }

    /// One page of the room's photo gallery, newest first. `before` is the
    /// smallest `created_at_ms` already shown — the "load more" cursor.
    pub async fn room_media(
        &self,
        room_id: &str,
        before: Option<f64>,
        limit: Option<u32>,
    ) -> ApiResult<GalleryPage> {
        let mut path = format!("/api/rooms/{}/media", encode(room_id));
        let mut sep = '?';
        if let Some(limit) = limit {
            path.push_str(&format!("{sep}limit={limit}"));
            sep = '&';
        }
        if let Some(before) = before {
            path.push_str(&format!("{sep}before={}", before as i64));
        }
        self.send(Method::GET, &path).await
    }

    /// Ask for a short-lived URL a browser can download directly, plus the
    /// digest to check what it saved.
    ///
    /// This is how a large attachment is saved. [`download_file`] below pulls
    /// the bytes through the page, which needs the whole file in memory and
    /// stops being possible somewhere well under a gigabyte; the browser
    /// writing the response straight to disk has no such ceiling and gets
    /// resume, pause and its own progress for free.
    pub async fn download_link(&self, id: &str) -> ApiResult<DownloadLink> {
        // GET, not POST, and with no body. A POST carrying `{}` is what this
        // was, and it fails outright from iOS Safari over HTTP/3 — which is
        // how a video lost its thumbnail, its playback and its download button
        // all at once, with only "Can't reach the server" to show for it.
        self.send(
            Method::GET,
            &format!("/api/files/{}/download-token", encode(id)),
        )
        .await
    }

    /// Fetch an attachment's whole body into memory with the caller's token.
    ///
    /// **Nothing in the app calls this any more.** Previews and saves moved to
    /// capability URLs (`download_link`) when the size ceiling moved to 4 GB,
    /// because this path buffers the entire file in the wasm heap. It stays
    /// for the same reason the rest of the unused protocol surface does (see
    /// `api/mod.rs`), and because a future small-file consumer — hashing a
    /// kilobyte attachment inline, say — is legitimate. Do not point anything
    /// large at it.
    pub async fn download_file(&self, id: &str) -> ApiResult<Vec<u8>> {
        let path = format!("/api/files/{}/raw", encode(id));
        let resp = self
            .build(Method::GET, &path)
            .build()
            .map_err(|e| ApiError::Network(e.to_string()))?
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let status = resp.status();
        if !(200..300).contains(&status) {
            // The error envelope is JSON even when the success body is bytes.
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::from_response(status, &body));
        }
        resp.binary()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))
    }
}

/// Percent-encode one query value or path segment.
///
/// Hand-rolled because the alternative is pulling in a crate to escape four
/// characters, and because a filename is the one value here that is entirely
/// attacker-chosen: everything outside the unreserved set is escaped, which is
/// stricter than required and cannot be wrong.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_escapes_everything_that_could_change_the_request() {
        assert_eq!(encode("report.pdf"), "report.pdf");
        assert_eq!(encode("Q3 report.pdf"), "Q3%20report.pdf");
        // The characters that would otherwise add a parameter or end the value.
        assert_eq!(encode("a&caption=x"), "a%26caption%3Dx");
        assert_eq!(encode("a?b#c"), "a%3Fb%23c");
        // A path separator cannot survive into a path segment.
        assert_eq!(encode("../../etc"), "..%2F..%2Fetc");
        // Non-ASCII becomes UTF-8 bytes, which is what the server decodes.
        assert_eq!(encode("보고서"), "%EB%B3%B4%EA%B3%A0%EC%84%9C");
    }
}
