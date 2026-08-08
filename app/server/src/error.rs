//! The API error type and its wire form.
//!
//! Every error the API emits is an object with a `message` string. Two
//! optional extensions exist, and only those two — see `docs/API.md` §1.5:
//!
//! - `errors: [..]` accompanies a 400 produced by input validation.
//! - `code` plus `currentKeyVersion` accompanies the two 409s that a client is
//!   expected to *act* on rather than merely display.
//!
//! Internal failures never reach the client as detail. A database error is
//! logged with its cause and answered with a flat "Internal Server Error",
//! because the alternative is leaking schema and file paths to anyone who can
//! provoke a panic.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

/// A machine-readable code, emitted only where a client must branch on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ErrorCode {
    /// The room's key epoch advanced; re-wrap and retry under the new version.
    #[serde(rename = "KEY_ROTATION_REQUIRED")]
    KeyRotationRequired,
    /// The submitted `keyVersion` is behind the room's current epoch.
    #[serde(rename = "STALE_KEY_VERSION")]
    StaleKeyVersion,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),

    /// Input validation failure: one entry per offending field.
    #[error("Validation failed")]
    Validation(Vec<String>),

    #[error("{0}")]
    Unauthorized(String),

    #[error("{0}")]
    Forbidden(String),

    #[error("{0}")]
    NotFound(String),

    #[error("{message}")]
    Conflict {
        code: Option<ErrorCode>,
        message: String,
        current_key_version: Option<i64>,
    },

    #[error("{0}")]
    PayloadTooLarge(String),

    #[error("{0}")]
    TooManyRequests(String),

    /// Anything the client cannot act on. The cause is logged, never sent.
    #[error("internal error: {0}")]
    Internal(#[from] anyhow::Error),
}

impl ApiError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::BadRequest(msg.into())
    }

    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self::Unauthorized(msg.into())
    }

    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::Forbidden(msg.into())
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::NotFound(msg.into())
    }

    pub fn conflict(msg: impl Into<String>) -> Self {
        Self::Conflict {
            code: None,
            message: msg.into(),
            current_key_version: None,
        }
    }

    /// A 409 the client is expected to handle by re-wrapping the room key.
    pub fn key_conflict(code: ErrorCode, msg: impl Into<String>, current_version: i64) -> Self {
        Self::Conflict {
            code: Some(code),
            message: msg.into(),
            current_key_version: Some(current_version),
        }
    }

    /// A single-field validation failure, formatted `field: reason`.
    pub fn field(field: &str, reason: &str) -> Self {
        Self::Validation(vec![format!("{field}: {reason}")])
    }

    /// The generic message used wherever revealing *which* check failed would
    /// tell an unauthenticated caller something they should not learn.
    pub fn access_denied() -> Self {
        Self::Forbidden("Access denied".into())
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) | Self::Validation(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict { .. } => StatusCode::CONFLICT,
            Self::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::TooManyRequests(_) => StatusCode::TOO_MANY_REQUESTS,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// The serialised error envelope. Absent fields are omitted rather than sent
/// as `null`, matching the reference server's `JSON.stringify` behaviour.
#[derive(Debug, Serialize)]
struct ErrorBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<ErrorCode>,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    errors: Option<&'a [String]>,
    #[serde(rename = "currentKeyVersion", skip_serializing_if = "Option::is_none")]
    current_key_version: Option<i64>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();

        // Log the cause exactly once, here, so handlers can use `?` freely and
        // no internal detail has to travel in the response body.
        if let Self::Internal(cause) = &self {
            tracing::error!(error = ?cause, "request failed");
        }

        let message = match &self {
            Self::Internal(_) => "Internal Server Error".to_string(),
            other => other.to_string(),
        };

        let body = ErrorBody {
            code: match &self {
                Self::Conflict { code, .. } => *code,
                _ => None,
            },
            message: &message,
            errors: match &self {
                Self::Validation(errors) => Some(errors.as_slice()),
                _ => None,
            },
            current_key_version: match &self {
                Self::Conflict {
                    current_key_version,
                    ..
                } => *current_key_version,
                _ => None,
            },
        };

        (status, Json(body)).into_response()
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Internal(anyhow::Error::new(e).context("database"))
    }
}

impl From<crate::jsonl::LogError> for ApiError {
    fn from(e: crate::jsonl::LogError) -> Self {
        Self::Internal(anyhow::Error::new(e).context("event log"))
    }
}

impl From<pocketskynet_core::IdError> for ApiError {
    fn from(e: pocketskynet_core::IdError) -> Self {
        Self::BadRequest(e.to_string())
    }
}

/// A refusal from the OS CSPRNG (`pocketskynet_core::random`) is never the
/// caller's fault and never actionable by them, so it collapses to the same
/// opaque 500 as a database error — cause logged, nothing leaked. It exists so
/// entropy-drawing routes can `?` instead of hand-rolling the wrap, which kept
/// them from all agreeing on the context string.
impl From<pocketskynet_core::CryptoError> for ApiError {
    fn from(e: pocketskynet_core::CryptoError) -> Self {
        Self::Internal(anyhow::Error::new(e).context("entropy"))
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn body_of(err: ApiError) -> (StatusCode, serde_json::Value) {
        let response = err.into_response();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn plain_errors_carry_only_a_message() {
        let (status, body) = body_of(ApiError::access_denied()).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["message"], "Access denied");
        assert!(
            body.get("errors").is_none(),
            "absent fields must be omitted"
        );
        assert!(body.get("code").is_none());
        assert!(body.get("currentKeyVersion").is_none());
    }

    #[tokio::test]
    async fn validation_errors_list_each_offending_field() {
        let err = ApiError::Validation(vec![
            "roomId: Room ID contains invalid characters".into(),
            "content: Message content is required".into(),
        ]);
        let (status, body) = body_of(err).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["message"], "Validation failed");
        assert_eq!(body["errors"].as_array().unwrap().len(), 2);
        assert_eq!(
            body["errors"][0],
            "roomId: Room ID contains invalid characters"
        );
    }

    #[tokio::test]
    async fn key_conflicts_are_machine_readable() {
        let err = ApiError::key_conflict(
            ErrorCode::StaleKeyVersion,
            "Message encrypted under an old key version",
            3,
        );
        let (status, body) = body_of(err).await;

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "STALE_KEY_VERSION");
        assert_eq!(body["currentKeyVersion"], 3);
        assert_eq!(
            body["message"],
            "Message encrypted under an old key version"
        );
    }

    #[tokio::test]
    async fn internal_errors_never_leak_their_cause() {
        let err = ApiError::Internal(anyhow::anyhow!(
            "no such column: users.secret_column in /var/db/pocketskynet.db"
        ));
        let (status, body) = body_of(err).await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["message"], "Internal Server Error");

        let rendered = body.to_string();
        assert!(!rendered.contains("secret_column"));
        assert!(!rendered.contains("/var/db"));
    }

    #[tokio::test]
    async fn database_errors_become_opaque_internal_errors() {
        let err: ApiError = rusqlite::Error::QueryReturnedNoRows.into();
        let (status, body) = body_of(err).await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["message"], "Internal Server Error");
    }

    #[test]
    fn field_helper_uses_the_documented_format() {
        let ApiError::Validation(errors) = ApiError::field("roomId", "Room ID is required") else {
            panic!("expected a validation error");
        };
        assert_eq!(errors, vec!["roomId: Room ID is required"]);
    }
}
