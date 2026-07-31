//! The typed error envelope (API.md §1.5).
//!
//! The server speaks three error shapes and exactly two endpoints emit a
//! machine-readable `code`. Decoding all of it into one type here means the rest
//! of the client can branch on *meaning* (`is_key_rotation_required`) rather
//! than on string matching, which is the thing that rots when server copy
//! changes.

use serde::Deserialize;

/// The raw JSON body of any non-2xx response.
///
/// Every field except `message` is optional because only some endpoints emit
/// them; `message` itself is optional too, because a proxy or a 502 from
/// somewhere upstream may return no JSON at all.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ErrorBody {
    message: Option<String>,
    /// Present only on `POST /rooms/:id/messages` 409s.
    code: Option<String>,
    /// Accompanies both 409 codes so the client can re-encrypt without a refetch.
    current_key_version: Option<i64>,
    /// One entry per Zod issue, `"field: message"`.
    #[serde(default)]
    errors: Vec<String>,
    /// Addresses a rotation failed to cover.
    #[serde(default)]
    missing: Vec<String>,
}

/// Anything that can go wrong on the way to, or back from, the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// The request never completed — DNS, TCP, CORS, or the device is offline.
    /// Distinguished from a server error because the UI reaction differs: a
    /// network failure means "retry when connectivity returns", not "the server
    /// rejected you".
    Network(String),
    /// The server answered with a non-2xx status.
    Status(StatusError),
    /// A 2xx body that did not match the type we expected. Always a bug — in
    /// the client or the server — and never something the user can fix, so it
    /// is kept separate from `Status` for triage.
    Decode(String),
}

/// A decoded non-2xx response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusError {
    pub status: u16,
    pub message: String,
    pub code: Option<String>,
    pub current_key_version: Option<i64>,
    pub errors: Vec<String>,
    pub missing: Vec<String>,
}

impl ApiError {
    /// Decode a non-2xx response body. Never fails: a body that is not JSON, or
    /// is JSON of an unexpected shape, degrades to the status line. Failing to
    /// parse an error must not itself become a different error — the user would
    /// then see "decode failed" instead of "you are not a member of this room".
    pub fn from_response(status: u16, body: &str) -> Self {
        let parsed: ErrorBody = serde_json::from_str(body).unwrap_or_default();
        let message = parsed
            .message
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| default_message(status));
        ApiError::Status(StatusError {
            status,
            message,
            code: parsed.code,
            current_key_version: parsed.current_key_version,
            errors: parsed.errors,
            missing: parsed.missing,
        })
    }

    /// The HTTP status, when there was one.
    pub fn status(&self) -> Option<u16> {
        match self {
            ApiError::Status(s) => Some(s.status),
            _ => None,
        }
    }

    /// The single line to show the user. Validation errors are folded in
    /// because "Validation failed" alone tells nobody anything.
    pub fn user_message(&self) -> String {
        match self {
            ApiError::Network(_) => {
                "Can't reach the server. Check your connection and try again.".to_owned()
            }
            ApiError::Decode(_) => {
                "The server sent something this client couldn't read.".to_owned()
            }
            ApiError::Status(s) => {
                if s.errors.is_empty() {
                    s.message.clone()
                } else {
                    format!("{}: {}", s.message, s.errors.join("; "))
                }
            }
        }
    }

    /// The token is missing, expired or invalid — the caller must sign in again.
    pub fn is_unauthorized(&self) -> bool {
        self.status() == Some(401)
    }

    /// The caller is not a member (or the room does not exist — the server does
    /// not distinguish, deliberately, so as not to be a room-existence oracle).
    pub fn is_forbidden(&self) -> bool {
        self.status() == Some(403)
    }

    pub fn is_not_found(&self) -> bool {
        self.status() == Some(404)
    }

    /// Rate limited. Worth its own predicate: the correct reaction is to back
    /// off, not to show a red error.
    pub fn is_rate_limited(&self) -> bool {
        self.status() == Some(429)
    }

    /// The room needs re-keying before any encrypted message can be posted
    /// (API.md §6.10.1). **Do not retry** — perform a rotation first.
    pub fn is_key_rotation_required(&self) -> bool {
        matches!(self, ApiError::Status(s) if s.code.as_deref() == Some("KEY_ROTATION_REQUIRED"))
    }

    /// The message was sealed under a superseded epoch. Refetch keys, re-encrypt
    /// under `current_key_version`, retry **once**.
    pub fn is_stale_key_version(&self) -> bool {
        matches!(self, ApiError::Status(s) if s.code.as_deref() == Some("STALE_KEY_VERSION"))
    }

    /// The epoch the server says is current, when it told us.
    pub fn current_key_version(&self) -> Option<i64> {
        match self {
            ApiError::Status(s) => s.current_key_version,
            _ => None,
        }
    }

    /// Whether a blind retry could plausibly succeed. Used by the send queue:
    /// retrying a 400 forever is a busy loop, retrying a 503 is correct.
    pub fn is_retryable(&self) -> bool {
        match self {
            ApiError::Network(_) => true,
            ApiError::Decode(_) => false,
            ApiError::Status(s) => matches!(s.status, 429 | 500 | 502 | 503 | 504),
        }
    }
}

/// A last-resort message for a status with no JSON body.
fn default_message(status: u16) -> String {
    match status {
        400 => "The server rejected that request.",
        401 => "Your session has expired. Sign in again.",
        403 => "You don't have access to that.",
        404 => "That doesn't exist.",
        409 => "That conflicts with the current state.",
        413 => "That's too large to send.",
        429 => "Too many requests. Wait a moment and try again.",
        500..=599 => "The server is having trouble. Try again shortly.",
        _ => "The request failed.",
    }
    .to_owned()
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.user_message())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_message_envelope() {
        let e = ApiError::from_response(403, r#"{"message":"Access denied"}"#);
        assert_eq!(e.user_message(), "Access denied");
        assert!(e.is_forbidden());
        assert!(!e.is_retryable());
    }

    #[test]
    fn validation_envelope_surfaces_the_field_errors() {
        let e = ApiError::from_response(
            400,
            r#"{"message":"Validation failed","errors":["roomId: Room ID contains invalid characters"]}"#,
        );
        assert_eq!(
            e.user_message(),
            "Validation failed: roomId: Room ID contains invalid characters"
        );
    }

    #[test]
    fn key_rotation_required_is_recognised_by_code_not_by_prose() {
        let e = ApiError::from_response(
            409,
            r#"{"code":"KEY_ROTATION_REQUIRED","message":"Room key rotation is pending","currentKeyVersion":3}"#,
        );
        assert!(e.is_key_rotation_required());
        assert!(!e.is_stale_key_version());
        assert_eq!(e.current_key_version(), Some(3));
        // Critically: never retry this one blindly.
        assert!(!e.is_retryable());
    }

    #[test]
    fn stale_key_version_carries_the_epoch_to_re_encrypt_under() {
        let e = ApiError::from_response(
            409,
            r#"{"code":"STALE_KEY_VERSION","message":"Message key version does not match","currentKeyVersion":4}"#,
        );
        assert!(e.is_stale_key_version());
        assert_eq!(e.current_key_version(), Some(4));
    }

    #[test]
    fn rotation_coverage_failure_lists_the_missing_members() {
        let e = ApiError::from_response(
            400,
            r#"{"message":"Rotation must include a key for every current member","missing":["0xaaa","0xbbb"]}"#,
        );
        match e {
            ApiError::Status(s) => assert_eq!(s.missing, vec!["0xaaa", "0xbbb"]),
            other => panic!("expected a status error, got {other:?}"),
        }
    }

    #[test]
    fn a_non_json_body_degrades_to_the_status_line_instead_of_erroring() {
        let e = ApiError::from_response(502, "<html>Bad Gateway</html>");
        assert_eq!(e.status(), Some(502));
        assert!(e.user_message().contains("trouble"));
        assert!(e.is_retryable());
    }

    #[test]
    fn an_empty_message_field_is_treated_as_absent() {
        let e = ApiError::from_response(404, r#"{"message":""}"#);
        assert_eq!(e.user_message(), "That doesn't exist.");
    }

    #[test]
    fn retryability_matches_the_documented_transient_statuses() {
        for (status, retryable) in [
            (400, false),
            (401, false),
            (403, false),
            (404, false),
            (409, false),
            (429, true),
            (500, true),
            (502, true),
            (503, true),
            (504, true),
        ] {
            let e = ApiError::from_response(status, "{}");
            assert_eq!(e.is_retryable(), retryable, "status {status}");
        }
        assert!(ApiError::Network("offline".into()).is_retryable());
        assert!(!ApiError::Decode("bad shape".into()).is_retryable());
    }

    #[test]
    fn rate_limiting_is_distinguishable_so_it_can_back_off_quietly() {
        let e = ApiError::from_response(
            429,
            r#"{"message":"Too many requests, please try again later"}"#,
        );
        assert!(e.is_rate_limited());
        assert!(e.is_retryable());
    }
}
