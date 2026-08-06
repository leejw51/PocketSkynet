//! Typed HTTP client for the PocketSkynet API (API.md §6).
//!
//! One module per endpoint group, all hanging off a single [`Client`] that owns
//! the base URL and the bearer token. The client is `Clone` and cheap
//! (`Rc<str>` inside), so components hold their own copy rather than threading a
//! reference through every prop.
//!
//! Everything returns `Result<T, ApiError>` — there is no `unwrap` on a network
//! boundary anywhere in this crate.

// The endpoint surface here mirrors `docs/API.md` in full, including calls no
// screen makes *yet* (profile rename, on-chain publish, single-key fetch). They
// are kept because a partial protocol client is the thing that rots: the next
// screen that needs one would otherwise re-derive its request shape from prose
// rather than from the one place that is already tested against the spec.
#![allow(dead_code)]

pub mod error;
pub mod types;

/// Public for the admin console's view types.
pub mod admin;
mod auth;
/// Public for [`downloads::Support`], which the save action branches on.
pub mod downloads;
pub mod files;
mod invitations;
mod keys;
/// Public for the [`mentions::Mention`] type the inbox renders.
pub mod mentions;
pub mod messages;
/// Public because [`messages::MessageBody`] is the shape `crate::crypto` builds
/// and the composer sends; everything else in these modules is `impl Client`.
/// Public for the [`operators::OperatorFile`] type the ladder renders.
pub mod operators;
mod rooms;
/// Public for the hit/note/tag types the Knowledge page renders.
pub mod search;
/// Public for the [`shout::Shout`] type the banner layer renders.
pub mod shout;
/// Public for the [`sites::Site`] type the Publish page renders.
pub mod sites;
/// Public for [`uploads::Progress`] and [`uploads::Target`], which the actions
/// layer and the progress UI both name.
pub mod uploads;
mod users;

use std::rc::Rc;

pub use error::ApiError;
pub use types::*;

use gloo_net::http::{Method, RequestBuilder};
use serde::de::DeserializeOwned;
use serde::Serialize;

/// Result alias used throughout the API layer.
pub type ApiResult<T> = Result<T, ApiError>;

/// An authenticated (or not) handle to the server.
#[derive(Clone, PartialEq)]
pub struct Client {
    /// Empty means "same origin", which is the normal deployment: the server
    /// serves the `.wasm` bundle and the API from one host, so there is no CORS
    /// preflight and no third-party cookie question.
    base: Rc<str>,
    token: Option<Rc<str>>,
}

impl Default for Client {
    fn default() -> Self {
        Self::new("")
    }
}

impl Client {
    pub fn new(base: &str) -> Self {
        Self {
            base: Rc::from(base.trim_end_matches('/')),
            token: None,
        }
    }

    /// Returns a copy carrying the bearer token. Deliberately not a setter: an
    /// immutable client makes it impossible for one component to silently
    /// de-authenticate another's in-flight request.
    pub fn with_token(&self, token: Option<&str>) -> Self {
        Self {
            base: self.base.clone(),
            token: token.map(Rc::from),
        }
    }

    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// Absolute URL for a path, used by the realtime layer to build `ws://` and
    /// SSE URLs from the same origin the API lives on.
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn build(&self, method: Method, path: &str) -> RequestBuilder {
        let mut req = RequestBuilder::new(&self.url(path)).method(method);
        if let Some(t) = &self.token {
            // Standard `Bearer <token>`. The reference server also accepts a
            // bare token, but emitting the non-standard form would be a trap
            // for any proxy or gateway in front of it.
            req = req.header("Authorization", &format!("Bearer {t}"));
        }
        req
    }

    /// Send a request with no body and decode a JSON response.
    async fn send<T: DeserializeOwned>(&self, method: Method, path: &str) -> ApiResult<T> {
        let req = self
            .build(method, path)
            .build()
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let resp = req
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        decode(resp).await
    }

    /// Send a JSON body and decode a JSON response.
    async fn send_json<B: Serialize, T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: &B,
    ) -> ApiResult<T> {
        let req = self
            .build(method, path)
            .json(body)
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let resp = req
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        decode(resp).await
    }

    /// Fire a request whose response body we do not care about, but whose
    /// status we do. Used for the several endpoints that answer
    /// `{"message":"…"}` on success.
    async fn send_ok<B: Serialize>(&self, method: Method, path: &str, body: &B) -> ApiResult<()> {
        let _: serde_json::Value = self.send_json(method, path, body).await?;
        Ok(())
    }

    async fn send_ok_empty(&self, method: Method, path: &str) -> ApiResult<()> {
        let _: serde_json::Value = self.send(method, path).await?;
        Ok(())
    }

    /// Whether *this* server is answering.
    ///
    /// The distinction from `navigator.onLine` is the whole point. That flag
    /// reports whether the machine has a network route at all, which says
    /// nothing about a server on loopback, on the LAN, or on a mesh VPN — all
    /// three keep working with the Wi-Fi off, and all three are how this app is
    /// normally reached. Trusting the browser's flag locks people out of a
    /// server that is sitting there answering requests.
    ///
    /// `/api/health` is the right probe: it is exempt from the rate limiter, so
    /// polling it cannot lock the caller out, and it probes the database rather
    /// than only proving the HTTP stack is up.
    ///
    /// Any completed response counts, including a 5xx. This asks "is there a
    /// server on the other end", not "is it healthy" — a server returning 500
    /// is one whose errors the user should see, not one to hide behind a dead
    /// button.
    pub async fn reachable(&self) -> bool {
        let Ok(req) = self.build(Method::GET, "/api/health").build() else {
            return false;
        };
        req.send().await.is_ok()
    }
}

/// Turn a `gloo` response into `Result<T, ApiError>`.
///
/// Reads the body as text *first*, then decides: a non-2xx body has to go
/// through [`ApiError::from_response`], and a 2xx body that fails to parse must
/// report what it actually received rather than a bare "invalid type".
async fn decode<T: DeserializeOwned>(resp: gloo_net::http::Response) -> ApiResult<T> {
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| ApiError::Network(e.to_string()))?;

    if !(200..300).contains(&status) {
        return Err(ApiError::from_response(status, &body));
    }
    // A 200 with an empty body happens on some proxies; `()`-shaped callers go
    // through `send_ok`, which asks for a `Value`, so normalise empty to `null`.
    let text = if body.trim().is_empty() {
        "null"
    } else {
        &body
    };
    serde_json::from_str(text)
        .map_err(|e| ApiError::Decode(format!("{e} (body: {})", truncate(&body))))
}

/// Keep decode-failure diagnostics from pasting a 500 KB body into a toast.
fn truncate(s: &str) -> String {
    if s.len() <= 200 {
        s.to_owned()
    } else {
        format!("{}…", &s[..s.floor_boundary(200)])
    }
}

/// `str::floor_char_boundary` only stabilised after this crate's MSRV, so we
/// carry a two-line version under a different name (sharing the name would
/// silently switch implementations on a newer toolchain).
trait FloorBoundary {
    fn floor_boundary(&self, index: usize) -> usize;
}

impl FloorBoundary for str {
    fn floor_boundary(&self, index: usize) -> usize {
        let mut i = index.min(self.len());
        while i > 0 && !self.is_char_boundary(i) {
            i -= 1;
        }
        i
    }
}

/// Percent-encode one path segment.
///
/// Room and message ids are charset-restricted and need no escaping, but an
/// emoticon code is arbitrary Unicode and goes into a path segment
/// (`DELETE /messages/:id/emoticons/:code`), so it must be encoded — and encoded
/// **once**: the reference server double-decodes, which corrupts any code
/// containing a literal `%` (API.md quirk #14). We cannot fix the server from
/// here, but we can avoid contributing a second layer.
pub fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Percent-encode a query-string value.
pub fn encode_query(s: &str) -> String {
    encode_segment(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_normalisation_never_doubles_a_slash() {
        assert_eq!(Client::new("").url("/api/rooms"), "/api/rooms");
        assert_eq!(
            Client::new("http://127.0.0.1:9099").url("/api/rooms"),
            "http://127.0.0.1:9099/api/rooms"
        );
        assert_eq!(
            Client::new("http://127.0.0.1:9099/").url("/api/rooms"),
            "http://127.0.0.1:9099/api/rooms"
        );
    }

    #[test]
    fn with_token_produces_a_new_client_and_leaves_the_original_alone() {
        let anon = Client::new("");
        let authed = anon.with_token(Some("jwt"));
        assert!(!anon.has_token());
        assert_eq!(authed.token(), Some("jwt"));
        assert!(!authed.with_token(None).has_token());
    }

    #[test]
    fn segment_encoding_escapes_emoji_and_reserved_characters() {
        // 🍎 → the exact percent-encoding the API spec quotes.
        assert_eq!(encode_segment("🍎"), "%F0%9F%8D%8E");
        assert_eq!(encode_segment("a/b"), "a%2Fb");
        assert_eq!(encode_segment("100%"), "100%25");
        assert_eq!(encode_segment("a b"), "a%20b");
        // Unreserved characters survive untouched.
        assert_eq!(
            encode_segment("msg_1749_4cfe-1c4c.x~"),
            "msg_1749_4cfe-1c4c.x~"
        );
    }

    #[test]
    fn truncation_of_a_diagnostic_body_respects_utf8_boundaries() {
        let long = "한".repeat(300);
        let t = truncate(&long);
        // Must not panic, and must remain valid UTF-8 (guaranteed by String).
        assert!(t.ends_with('…'));
        assert!(t.len() <= 203);
        assert_eq!(truncate("short"), "short");
    }
}
