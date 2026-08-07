//! Hosted images and videos for the AI assistant.
//!
//! The AI providers answer in two shapes: raw base64 bytes (image generation
//! on Grok, OpenAI and Gemini) or a temporary URL on the provider's own CDN
//! (video generation, where there is no bytes option at all). Neither can be
//! pasted into a chat room as it stands: base64 blows past the message size
//! cap, and a provider URL stops resolving within about a day — a room full
//! of dead links. So both are stored *here* and the room carries a
//! same-origin URL this server will still serve next year.
//!
//! Two ways in, for those two shapes:
//!
//! * `POST /api/images` — the caller has the bytes (base64 decoded in the
//!   browser). The reference client called exactly this endpoint but its
//!   server never implemented it; this is the fix, not an invention.
//! * `POST /api/images/import` — the caller has only a provider URL. The
//!   fetch happens on this server rather than in the browser because the
//!   provider CDNs send no CORS headers, so the browser cannot read the
//!   bytes it is being shown. See [`import`] for the SSRF allow-list that
//!   keeps "the server will fetch a URL for you" from meaning "the server
//!   will fetch *any* URL for you".
//!
//! Storage is content-addressed: the filename is the SHA-256 of the bytes,
//! so re-uploading the same image is idempotent, the URL can be cached as
//! immutable forever, and there is nothing for two concurrent uploads to
//! race over — both write the same bytes to the same name.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::auth::AuthUser;
use crate::error::{ApiError, ApiResult};
use crate::AppState;

/// Image uploads above this are refused. Generous for a 1024×1024 PNG, small
/// enough that the endpoint is useless as free blob storage.
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// The ceiling for the **single-shot** `POST /api/images`, which still holds
/// its whole body in memory.
///
/// Stays at 25 MB for the same reason `files.rs::MAX_FILE_BYTES` does: raising
/// it buys a bigger memory spike per request and nothing else. Anything larger
/// goes through `routes/uploads.rs`, which never holds more than one chunk.
pub const MAX_VIDEO_BYTES: usize = 25 * 1024 * 1024;

/// What a video may reach when it arrives in chunks.
///
/// The full upload ceiling. A film is a film whether it is shared as a room
/// attachment or as media, and a cap that let one route carry it and not the
/// other would just be a maze — the memory argument that justified 25 MB does
/// not apply to a path that streams.
///
/// Images keep [`MAX_IMAGE_BYTES`] on both paths: a still that large is a
/// mistake rather than a preference, and the cap catches it before the disk
/// does.
pub const MAX_VIDEO_SESSION_BYTES: usize = crate::routes::uploads::MAX_UPLOAD_BYTES as usize;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/images", post(upload))
        .route("/images/import", post(import))
        // Overrides the 100 KB API-wide default: this is the one endpoint
        // whose whole point is a body bigger than that. Innermost layer
        // wins, so the general limit still applies everywhere else. The
        // per-kind caps below are what actually bound a stored file; this
        // only has to be the larger of the two.
        .layer(DefaultBodyLimit::max(MAX_VIDEO_BYTES))
}

/// The serving route, split out for the media rate-limit budget — same
/// reasoning as `files::media_router`: an AI-generated video hosted here is
/// played by a `<video>` element, and playback is many requests by design.
pub fn media_router() -> Router<AppState> {
    Router::new().route("/images/{name}", get(serve))
}

/// The media types the AI providers actually emit. A server that stores
/// arbitrary content types is a server that hosts `text/html` for phishing
/// pages, so this is an allow-list, not a validation.
///
/// Every entry is inert in a browser: an `<img>` or a `<video>` source
/// carries no script capability, which is what keeps serving these inline on
/// the app's own origin sound. Nothing that a sniffer could take for markup
/// belongs in this table.
const ALLOWED: &[(&str, &str)] = &[
    ("image/png", "png"),
    ("image/jpeg", "jpg"),
    ("image/webp", "webp"),
    ("image/gif", "gif"),
    ("video/mp4", "mp4"),
    ("video/webm", "webm"),
];

/// The ceiling for one stored file, by media type.
fn cap_for(extension: &str) -> usize {
    match extension {
        "mp4" | "webm" => MAX_VIDEO_BYTES,
        _ => MAX_IMAGE_BYTES,
    }
}

fn extension_for(content_type: &str) -> Option<&'static str> {
    // `image/png;charset=...` never legitimately happens, but a lenient
    // parse here costs nothing and a strict one costs a confused user.
    let base = content_type.split(';').next().unwrap_or("").trim();
    ALLOWED
        .iter()
        .find(|(mime, _)| mime.eq_ignore_ascii_case(base))
        .map(|(_, ext)| *ext)
}

/// Also the authority on what a *stored* name may end in: `db::media` asks
/// here rather than keeping a second copy of the allow-list, so a media type
/// this server cannot serve can never become a reference it would try to purge.
pub(crate) fn mime_for(extension: &str) -> Option<&'static str> {
    ALLOWED
        .iter()
        .find(|(_, ext)| *ext == extension)
        .map(|(mime, _)| *mime)
}

/// `POST /api/images` — store raw image or video bytes, return the hosted URL.
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
            "Content-Type must be image/png, image/jpeg, image/webp, image/gif, \
             video/mp4, or video/webm",
        ));
    };
    let url = store(&state, ext, &body).await?;
    Ok((StatusCode::OK, Json(serde_json::json!({ "url": url }))).into_response())
}

/// Write bytes under their content hash and return the hosted URL.
///
/// The one place a file is created, so the emptiness check, the size cap and
/// the atomic write cannot drift between the two ways in.
async fn store(state: &AppState, ext: &str, body: &[u8]) -> ApiResult<String> {
    if body.is_empty() {
        return Err(ApiError::bad_request("Empty file"));
    }
    let cap = cap_for(ext);
    if body.len() > cap {
        return Err(ApiError::bad_request(format!(
            "File is larger than the {} MB limit for this media type",
            cap / (1024 * 1024)
        )));
    }

    let name = format!("{}.{ext}", hex::encode(Sha256::digest(body)));
    let dir = state.cfg.images_dir();
    let path = dir.join(&name);

    // Content-addressed: if the file exists the bytes are identical, so a
    // second upload is a no-op rather than an error or a rewrite.
    if !path.exists() {
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| ApiError::Internal(e.into()))?;
        // Write-then-rename so a crash mid-write can never leave a corrupt
        // file behind the immutable cache header. The tmp name carries a uuid
        // as well as the hash: two concurrent uploads of the same bytes would
        // otherwise share one tmp path and race each other's rename.
        let tmp = dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
        tokio::fs::write(&tmp, body)
            .await
            .map_err(|e| ApiError::Internal(e.into()))?;
        tokio::fs::rename(&tmp, &path)
            .await
            .map_err(|e| ApiError::Internal(e.into()))?;
    }

    Ok(format!("/api/images/{name}"))
}

/// Commit a finished upload session as an image or video.
///
/// The counterpart to [`store`] for bytes that arrived in chunks. It cannot
/// call `store`: that takes `&[u8]`, which is the thing a 4 GB upload does not
/// have. So the shared parts are re-expressed against a path — the media type
/// comes from the session's declared `mime` rather than a `Content-Type`
/// header, the digest was computed by the uploads route while it verified the
/// assembly, and the bytes move by `rename` instead of being written again.
///
/// The per-type ceilings still apply and are still the real limit: chunking
/// makes a 4 GB *transfer* possible, it does not make a 4 GB profile picture
/// sensible.
pub(crate) async fn finalize_upload(
    state: &AppState,
    session: &crate::db::uploads::Session,
    temp_path: &std::path::Path,
    digest: &str,
) -> ApiResult<Response> {
    let Some(ext) = extension_for(&session.mime) else {
        return Err(ApiError::bad_request(
            "mime must be image/png, image/jpeg, image/webp, image/gif, \
             video/mp4, or video/webm",
        ));
    };
    // The *session* ceiling, not the single-shot one: these bytes arrived in
    // chunks and were never held whole by anything.
    let cap = match ext {
        "mp4" | "webm" => MAX_VIDEO_SESSION_BYTES,
        _ => MAX_IMAGE_BYTES,
    };
    if session.declared_size as usize > cap {
        return Err(ApiError::bad_request(format!(
            "File is larger than the {} MB limit for this media type",
            cap / (1024 * 1024)
        )));
    }

    let name = format!("{digest}.{ext}");
    let dir = state.cfg.images_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    let path = dir.join(&name);

    // Content-addressed, so an existing file is the same file.
    if path.exists() {
        let _ = tokio::fs::remove_file(temp_path).await;
    } else {
        tokio::fs::rename(temp_path, &path)
            .await
            .map_err(|e| ApiError::Internal(e.into()))?;
    }

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "url": format!("/api/images/{name}") })),
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
struct ImportRequest {
    url: String,
}

/// `POST /api/images/import` — fetch a provider's temporary media URL and
/// store the bytes here, returning the permanent same-origin URL.
///
/// # Why the server fetches instead of the browser
///
/// Video generation has no base64 mode: the provider answers with a URL on
/// its own CDN that expires, and that CDN sends no `Access-Control-Allow-Origin`,
/// so a browser can display the video but cannot read its bytes to re-upload
/// them. Without this endpoint the only thing a client could paste into a
/// room is the expiring link.
///
/// # Why this is not an open proxy
///
/// "Fetch this URL for me" is server-side request forgery unless the target
/// set is closed, so it is closed on every axis at once:
///
/// * **https only**, so no `file:`, `gopher:` or plain-HTTP intranet target;
/// * **host allow-list** — the AI providers' media CDNs and nothing else, so
///   the URL can never name a link-local metadata address or a service on the
///   host's own network;
/// * **no redirects followed**, because a 302 from an allowed host to
///   `169.254.169.254` would otherwise walk straight through the allow-list;
/// * **allow-listed response type and a size cap**, so the answer becomes a
///   file only if it is the kind of media this route already hosts.
///
/// The response body is never shown to the caller, only stored, so this
/// cannot be used to read an internal endpoint even if one were reachable.
async fn import(
    State(state): State<AppState>,
    AuthUser(_caller): AuthUser,
    Json(req): Json<ImportRequest>,
) -> ApiResult<Response> {
    if !is_allowed_source(&req.url) {
        return Err(ApiError::bad_request(
            "Only media URLs from a supported AI provider can be imported",
        ));
    }

    let client = reqwest::Client::builder()
        // A ten-second video is a large download over a slow link, and the
        // provider CDN is not on this machine.
        .timeout(std::time::Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| ApiError::Internal(e.into()))?;

    let response = client
        .get(&req.url)
        .send()
        .await
        .map_err(|e| ApiError::bad_request(format!("Could not fetch that URL: {e}")))?;
    if !response.status().is_success() {
        return Err(ApiError::bad_request(format!(
            "The provider answered HTTP {} for that URL — it may have expired",
            response.status().as_u16()
        )));
    }

    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let Some(ext) = extension_for(&content_type) else {
        return Err(ApiError::bad_request(
            "That URL does not answer with an image or a video",
        ));
    };

    // Refuse on the advertised length before spending the bandwidth; `store`
    // re-checks the real length, because `Content-Length` is a claim.
    if let Some(len) = response.content_length() {
        if len > cap_for(ext) as u64 {
            return Err(ApiError::bad_request("That file is too large to host"));
        }
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| ApiError::bad_request(format!("Could not read that URL: {e}")))?;

    let url = store(&state, ext, &bytes).await?;
    Ok((StatusCode::OK, Json(serde_json::json!({ "url": url }))).into_response())
}

/// The hosts whose media this server will fetch on a caller's behalf.
///
/// Matched on the *whole* host or a dot-prefixed suffix — never `contains`,
/// which would make `x.ai.evil.example` an allowed source.
const IMPORT_HOSTS: &[&str] = &[
    // xAI: `imgen.x.ai` serves generated stills, `vidgen.x.ai` generated
    // video. The parent suffix covers both and any sibling they add.
    "x.ai",
    // OpenAI's image responses when a URL is returned rather than base64.
    "oaiusercontent.com",
    // Gemini's file service, for the same case.
    "googleapis.com",
];

fn is_allowed_source(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let rest = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Userinfo is stripped by taking what follows the last `@`: without this
    // `https://x.ai@evil.example/` would read as the allowed host.
    let host = rest.rsplit('@').next().unwrap_or(rest);
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    IMPORT_HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

/// `GET /api/images/{name}` — serve a stored image or video.
///
/// No auth: these URLs are pasted into encrypted messages and loaded by
/// `<img>` and `<video>` tags, which cannot attach an `Authorization` header.
/// The name is a SHA-256 of the content — an unguessable capability — so
/// possession of the URL is the access control, the same model as every chat
/// attachment host. Immutable caching is sound for the same reason: the name
/// *is* the content.
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
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(mime));
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    // The type is from the allow-list, not from whoever uploaded — but the
    // extension is what chose it, and `nosniff` is what stops a browser
    // second-guessing that on content it finds surprising.
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
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

        // Past the *image* cap but inside the layer's video-sized limit, so
        // this is the handler's own check answering, not the body limit.
        let huge = vec![0x42u8; MAX_IMAGE_BYTES + 1];
        let (status, _) = post_image(&router, Some(&token), "image/png", &huge).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Past the largest thing this route hosts: refused by the layer.
        let enormous = vec![0x42u8; MAX_VIDEO_BYTES + 1];
        let (status, _) = post_image(&router, Some(&token), "video/mp4", &enormous).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn video_is_hosted_and_served_as_video() {
        let state = state("img-video");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        // A generated clip is bigger than any image, and must be accepted at
        // a size the image cap would refuse.
        let clip = vec![0x21u8; MAX_IMAGE_BYTES + 1];
        let (status, json) = post_image(&router, Some(&token), "video/mp4", &clip).await;
        assert_eq!(status, StatusCode::OK);
        let url = json["url"].as_str().expect("a url");
        assert!(url.ends_with(".mp4"), "{url}");

        let req = Request::builder().uri(url).body(Body::empty()).unwrap();
        let response = router.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_TYPE], "video/mp4");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    }

    #[tokio::test]
    async fn an_import_of_an_unlisted_host_never_leaves_the_process() {
        let state = state("img-import");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        for hostile in [
            // The classic cloud metadata address, and the loopback and
            // private ranges behind it.
            "http://169.254.169.254/latest/meta-data/",
            "https://169.254.169.254/latest/meta-data/",
            "https://127.0.0.1:8443/api/server/info",
            "https://192.168.1.1/",
            "file:///etc/passwd",
            // Shapes that only *look* like an allowed host.
            "https://x.ai.evil.example/clip.mp4",
            "https://x.ai@evil.example/clip.mp4",
            "https://notx.ai/clip.mp4",
            // Right host, wrong scheme.
            "http://vidgen.x.ai/clip.mp4",
            "",
        ] {
            let req = Request::builder()
                .method("POST")
                .uri("/api/images/import")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(
                    serde_json::json!({ "url": hostile }).to_string(),
                ))
                .unwrap();
            let response = router.clone().oneshot(req).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{hostile:?}");
        }
    }

    #[tokio::test]
    async fn imports_need_a_token() {
        let router = build(state("img-import-auth"));
        let req = Request::builder()
            .method("POST")
            .uri("/api/images/import")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({ "url": "https://vidgen.x.ai/a.mp4" }).to_string(),
            ))
            .unwrap();
        let response = router.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// The allow-list is the whole of the SSRF defence, so it gets a test of
    /// its own rather than only being exercised through the handler.
    #[test]
    fn only_provider_media_hosts_are_importable() {
        assert!(is_allowed_source("https://vidgen.x.ai/xai-video/abc.mp4"));
        assert!(is_allowed_source("https://imgen.x.ai/xai-imgen/abc.jpeg"));
        assert!(is_allowed_source("https://x.ai/a.png"));
        assert!(is_allowed_source(
            "https://videos.oaiusercontent.com/a.mp4?sig=1"
        ));

        // Suffix, not substring; userinfo cannot forge the host; https only.
        assert!(!is_allowed_source("https://x.ai.evil.example/a.mp4"));
        assert!(!is_allowed_source("https://evil.example/x.ai/a.mp4"));
        assert!(!is_allowed_source("https://x.ai@evil.example/a.mp4"));
        assert!(!is_allowed_source("https://notx.ai/a.mp4"));
        assert!(!is_allowed_source("http://vidgen.x.ai/a.mp4"));
        assert!(!is_allowed_source("file:///etc/passwd"));
        assert!(!is_allowed_source("https://"));
        assert!(!is_allowed_source(""));
    }
}
