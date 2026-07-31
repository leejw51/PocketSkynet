//! Hosted images for the AI assistant.
//!
//! The AI image providers answer in two shapes: a hosted URL (Grok) or raw
//! base64 bytes (OpenAI, Gemini). Base64 cannot be pasted into a chat room —
//! messages are capped far below a megabyte of PNG — so the client uploads
//! the bytes here and posts the resulting URL instead. The reference client
//! calls exactly this endpoint (`POST /api/images`) but its server never
//! implemented it; this is the fix, not an invention.
//!
//! Storage is content-addressed: the filename is the SHA-256 of the bytes,
//! so re-uploading the same image is idempotent, the URL can be cached as
//! immutable forever, and there is nothing for two concurrent uploads to
//! race over — both write the same bytes to the same name.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use sha2::{Digest, Sha256};

use crate::auth::AuthUser;
use crate::error::{ApiError, ApiResult};
use crate::AppState;

/// Uploads above this are refused. Generous for a 1024×1024 PNG, small
/// enough that the endpoint is useless as free blob storage.
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/images", post(upload))
        .route("/images/{name}", get(serve))
        // Overrides the 100 KB API-wide default: this is the one endpoint
        // whose whole point is a body bigger than that. Innermost layer
        // wins, so the general limit still applies everywhere else.
        .layer(DefaultBodyLimit::max(MAX_IMAGE_BYTES))
}

/// The image types the AI providers actually emit. A server that stores
/// arbitrary content types is a server that hosts `text/html` for phishing
/// pages, so this is an allow-list, not a validation.
const ALLOWED: &[(&str, &str)] = &[
    ("image/png", "png"),
    ("image/jpeg", "jpg"),
    ("image/webp", "webp"),
    ("image/gif", "gif"),
];

fn extension_for(content_type: &str) -> Option<&'static str> {
    // `image/png;charset=...` never legitimately happens, but a lenient
    // parse here costs nothing and a strict one costs a confused user.
    let base = content_type.split(';').next().unwrap_or("").trim();
    ALLOWED
        .iter()
        .find(|(mime, _)| mime.eq_ignore_ascii_case(base))
        .map(|(_, ext)| *ext)
}

fn mime_for(extension: &str) -> Option<&'static str> {
    ALLOWED
        .iter()
        .find(|(_, ext)| *ext == extension)
        .map(|(mime, _)| *mime)
}

/// `POST /api/images` — store raw image bytes, return the hosted URL.
///
/// Auth required: hosting is a privilege of logged-in users. The GET side is
/// deliberately public (see [`serve`]).
async fn upload(
    State(state): State<AppState>,
    AuthUser(_caller): AuthUser,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let Some(ext) = extension_for(content_type) else {
        return Err(ApiError::bad_request(
            "Content-Type must be image/png, image/jpeg, image/webp, or image/gif",
        ));
    };
    if body.is_empty() {
        return Err(ApiError::bad_request("Empty image"));
    }

    let name = format!("{}.{ext}", hex::encode(Sha256::digest(&body)));
    let dir = state.cfg.images_dir();
    let path = dir.join(&name);

    // Content-addressed: if the file exists the bytes are identical, so a
    // second upload is a no-op rather than an error or a rewrite.
    if !path.exists() {
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| ApiError::Internal(e.into()))?;
        // Write-then-rename so a crash mid-write can never leave a corrupt
        // file behind the immutable cache header.
        let tmp = dir.join(format!(".{name}.tmp"));
        tokio::fs::write(&tmp, &body)
            .await
            .map_err(|e| ApiError::Internal(e.into()))?;
        tokio::fs::rename(&tmp, &path)
            .await
            .map_err(|e| ApiError::Internal(e.into()))?;
    }

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "url": format!("/api/images/{name}") })),
    )
        .into_response())
}

/// `GET /api/images/{name}` — serve a stored image.
///
/// No auth: image URLs are pasted into encrypted messages and loaded by
/// `<img>` tags, which cannot attach an `Authorization` header. The name is
/// a SHA-256 of the content — an unguessable capability — so possession of
/// the URL is the access control, the same model as every chat attachment
/// host. Immutable caching is sound for the same reason: the name *is* the
/// content.
async fn serve(State(state): State<AppState>, Path(name): Path<String>) -> ApiResult<Response> {
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return Err(ApiError::not_found("Image not found"));
    };
    let Some(mime) = mime_for(ext) else {
        return Err(ApiError::not_found("Image not found"));
    };
    // The stem must be exactly a SHA-256 hex digest. This is the traversal
    // guard: no separators, no dots, no way to name anything but a hash.
    if stem.len() != 64 || !stem.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ApiError::not_found("Image not found"));
    }

    let path = state.cfg.images_dir().join(&name);
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| ApiError::not_found("Image not found"))?;

    let mut response = (StatusCode::OK, bytes).into_response();
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(mime));
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;
    use crate::routes::build;
    use crate::test_support::{register, send, state, wallet};

    /// A 1×1 transparent PNG — the smallest real image there is.
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    async fn post_image(
        router: &axum::Router,
        token: Option<&str>,
        content_type: &str,
        bytes: &[u8],
    ) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder()
            .method("POST")
            .uri("/api/images")
            .header("content-type", content_type);
        if let Some(token) = token {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        let response = router
            .clone()
            .oneshot(req.body(Body::from(bytes.to_vec())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    #[tokio::test]
    async fn upload_stores_and_the_returned_url_serves_the_same_bytes() {
        let state = state("img-roundtrip");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        let (status, json) = post_image(&router, Some(&token), "image/png", PNG).await;
        assert_eq!(status, StatusCode::OK);
        let url = json["url"].as_str().expect("a url");
        assert!(url.starts_with("/api/images/"), "{url}");
        assert!(url.ends_with(".png"), "{url}");

        let req = Request::builder().uri(url).body(Body::empty()).unwrap();
        let response = router.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_TYPE], "image/png");
        assert_eq!(
            response.headers()[CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], PNG);
    }

    #[tokio::test]
    async fn re_uploading_the_same_bytes_yields_the_same_url() {
        let state = state("img-idempotent");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        let (_, first) = post_image(&router, Some(&token), "image/png", PNG).await;
        let (_, second) = post_image(&router, Some(&token), "image/png", PNG).await;
        assert_eq!(first["url"], second["url"]);
    }

    #[tokio::test]
    async fn uploads_need_a_token() {
        let router = build(state("img-auth"));
        let (status, _) = post_image(&router, None, "image/png", PNG).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn non_image_content_types_are_refused() {
        let state = state("img-mime");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        for bad in ["text/html", "application/json", "", "image/svg+xml"] {
            let (status, _) = post_image(&router, Some(&token), bad, PNG).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "accepted {bad:?}");
        }
    }

    #[tokio::test]
    async fn traversal_shaped_names_are_not_found_never_served() {
        let router = build(state("img-traversal"));
        for name in [
            "/api/images/..%2F..%2Fjwt.secret.png",
            "/api/images/notahash.png",
            "/api/images/aaaa.exe",
            &format!("/api/images/{}.png", "a".repeat(63)),
        ] {
            let response = send(&router, "GET", name, None, None).await;
            assert_eq!(response.status, StatusCode::NOT_FOUND, "{name}");
        }
    }

    #[tokio::test]
    async fn the_api_wide_body_limit_is_lifted_but_the_image_cap_holds() {
        let state = state("img-limit");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        // Bigger than the 100 KB API default, well under the image cap. The
        // bytes only need a valid content type, not a valid PNG decode.
        let medium = vec![0x42u8; 300 * 1024];
        let (status, _) = post_image(&router, Some(&token), "image/png", &medium).await;
        assert_eq!(status, StatusCode::OK);

        let huge = vec![0x42u8; MAX_IMAGE_BYTES + 1];
        let (status, _) = post_image(&router, Some(&token), "image/png", &huge).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }
}
