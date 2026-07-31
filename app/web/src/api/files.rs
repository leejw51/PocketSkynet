//! Attachments (`docs/API.md` §14).
//!
//! Two things here are unlike every other endpoint group, both forced by the
//! server's decision that an attachment is as private as its room:
//!
//! * **Upload sends raw bytes**, not JSON, with the metadata in the query
//!   string — the same shape `upload_image` uses, because base64 in a JSON body
//!   costs a third of the payload for nothing.
//! * **Download cannot be an `href`.** `/api/files/{id}/raw` demands a bearer
//!   token, so the bytes are fetched here and handed to the page as an
//!   object URL. That is the price of attachments not being a public
//!   capability-URL space like `/api/images`.

use gloo_net::http::Method;

use super::{ApiError, ApiResult, Client};
use crate::api::types::FileMeta;

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

    /// Fetch an attachment's bytes with the caller's token.
    ///
    /// Returns the raw bytes rather than a URL because only the caller knows
    /// what it wants them for — a preview needs a typed object URL, a save
    /// needs an anchor click, and building both here would mean leaking one
    /// object URL per call with nobody to revoke it.
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
