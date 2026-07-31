//! Router assembly, cross-cutting middleware, and static file serving.
//!
//! Route precedence (`docs/API.md` §14.1) is worth stating because the
//! reference depended on Express's first-match semantics: `/api/users/search`,
//! `/api/users/blocked`, `/api/users/blocked-by` and `/api/rooms/hidden` must
//! not be swallowed by `/api/users/{address}` and `/api/rooms/{roomId}`.
//! `axum`'s matcher prefers static segments over parameters regardless of
//! registration order, so the collision cannot occur here — the tests at the
//! bottom of this file assert it rather than trusting the claim.

pub mod auth;
pub mod emoticons;
pub mod files;
pub mod images;
pub mod invitations;
pub mod keys;
pub mod messages;
pub mod misc;
pub mod realtime;
pub mod rooms;
pub mod search;
pub mod shout;
pub mod sites;
pub mod sync;
pub mod users;

use std::path::Path;
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Request};
use axum::http::header::{
    ACCEPT, AUTHORIZATION, CONTENT_TYPE, ORIGIN, REFERRER_POLICY, STRICT_TRANSPORT_SECURITY,
    X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Router;
use tower_http::compression::predicate::{NotForContentType, Predicate, SizeAbove};
use tower_http::compression::CompressionLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::config::Config;
use crate::validate::MAX_BODY_BYTES;
use crate::AppState;

/// `X-Has-More` is the only endpoint-specific header, set by `/sync` and
/// exposed to browsers through CORS.
pub fn has_more_header() -> HeaderName {
    HeaderName::from_static("x-has-more")
}

/// The generated CA on disk, if this server made one.
///
/// `None` for plain HTTP (nothing to trust) and for a supplied certificate (the
/// operator's own chain is already trusted by whatever issued it), so the
/// "trust this server" affordance only appears where it can actually help.
fn ca_certificate_path(state: &AppState) -> Option<std::path::PathBuf> {
    match state.cfg.tls {
        crate::config::Tls::SelfSigned => {
            let path = state.cfg.tls_dir().join("ca.crt");
            path.exists().then_some(path)
        }
        _ => None,
    }
}

/// Build the complete application.
pub fn build(state: AppState) -> Router {
    let api = api_router(&state).with_state(state.clone());

    // The CA certificate, on the HTTPS listener as well as the plain-HTTP
    // redirect port. Both are needed, for different readers:
    //
    // * the redirect port serves a device that cannot get past the warning at
    //   all — MetaMask's in-app browser offers no bypass, so the file has to
    //   arrive over a connection with nothing to warn about;
    // * this one lets the app link to `/ca.crt` on its own origin, so the
    //   client never has to be told the redirect port and a relative link
    //   cannot be misconfigured.
    //
    // 404s when there is no generated CA — a deployment with a real
    // certificate has nothing to hand out.
    let ca_path = ca_certificate_path(&state);

    let mut router = Router::new()
        .route(
            "/ca.crt",
            axum::routing::get(move || crate::tls::serve_ca(ca_path.clone())),
        )
        .nest("/api", api)
        // `/ws` deliberately sits outside `/api`: it is not rate limited (a
        // socket is one request), not CORS-checked (the WebSocket handshake
        // has its own origin rules), and not body-limited.
        .merge(realtime::ws_router().with_state(state.clone()))
        // Published sites are ordinary web pages at `/sites/{id}/` — outside
        // `/api` because their URLs are links people open, not API calls.
        // Sandboxed per response; see routes/sites.rs.
        .merge(sites::serve_router().with_state(state.clone()))
        .fallback_service(
            tower::ServiceBuilder::new()
                .layer(axum::middleware::from_fn(spa_fallback_guard))
                .layer(axum::middleware::from_fn_with_state(
                    preload_header(&state.cfg.static_dir),
                    cache_control,
                ))
                .service(static_service(&state.cfg)),
        )
        .layer(SetResponseHeaderLayer::overriding(
            X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            REFERRER_POLICY,
            HeaderValue::from_static("strict-origin-when-cross-origin"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("permissions-policy"),
            HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("x-dns-prefetch-control"),
            HeaderValue::from_static("off"),
        ))
        .layer(compression_layer());

    if let Some(hsts) = hsts_layer() {
        router = router.layer(hsts);
    }
    router
}

/// Everything under `/api`.
fn api_router(state: &AppState) -> Router<AppState> {
    // `/api/health` is registered outside the general limiter so a
    // load-balancer probe can never be throttled into reporting an outage.
    let unlimited = misc::health_router();

    let limited = Router::new()
        .merge(misc::router())
        .merge(auth::router(state))
        .merge(users::router())
        .merge(rooms::router())
        .merge(invitations::router())
        .merge(keys::router())
        .merge(messages::router())
        .merge(files::router())
        .merge(images::router())
        .merge(search::router())
        .merge(shout::router())
        .merge(sites::router())
        .merge(realtime::sse_router())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::ratelimit::general,
        ));

    unlimited
        .merge(limited)
        .fallback(unknown_route)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(cors_layer(&state.cfg))
        // Outermost, so it also normalises the 405s the method router emits.
        .layer(axum::middleware::from_fn(json_error_bodies))
}

async fn unknown_route() -> Response {
    crate::error::ApiError::not_found("Not found").into_response()
}

/// Guarantee that every error leaving `/api` is a JSON object with a
/// `message`, even the ones axum produces before a handler runs (405 from the
/// method router, 415 from a content-type mismatch).
///
/// Clients parse `message` unconditionally; a bare text body would make them
/// report "unexpected token" instead of the actual failure.
async fn json_error_bodies(req: Request, next: Next) -> Response {
    let response = next.run(req).await;
    let status = response.status();

    if status.is_success() || status.is_redirection() {
        return response;
    }
    let is_json = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if is_json {
        return response;
    }

    let message = status.canonical_reason().unwrap_or("Request failed");
    let body = serde_json::json!({ "message": message });
    let mut rebuilt = (status, axum::Json(body)).into_response();
    // Preserve anything the inner layers set (rate-limit counters, CORS).
    for (name, value) in response.headers() {
        if name != CONTENT_TYPE && name != axum::http::header::CONTENT_LENGTH {
            rebuilt.headers_mut().insert(name, value.clone());
        }
    }
    rebuilt
}

/// HSTS, added only when the deployment says it is behind TLS.
///
/// Sending it from a plain-HTTP dev server would pin `localhost` to HTTPS in
/// the browser's HSTS store and break every other local project on that host —
/// a genuinely painful, hard-to-diagnose side effect.
pub fn hsts_layer() -> Option<SetResponseHeaderLayer<HeaderValue>> {
    crate::config::is_production().then(|| {
        SetResponseHeaderLayer::overriding(
            STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000; includeSubDomains"),
        )
    })
}

/// CORS for `/api`.
///
/// Never a wildcard: credentials are allowed, and `Access-Control-Allow-Origin: *`
/// with credentials is both rejected by browsers and a mistake worth making
/// impossible. The origin serving the client is same-origin and needs no entry.
fn cors_layer(cfg: &Config) -> CorsLayer {
    let extra: Arc<Vec<String>> = Arc::new(cfg.cors_origin.clone());

    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |origin: &HeaderValue, _| {
            origin
                .to_str()
                .map(|value| is_allowed_origin(value, &extra))
                .unwrap_or(false)
        }))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::PATCH,
            Method::OPTIONS,
        ])
        .allow_headers([
            ORIGIN,
            HeaderName::from_static("x-requested-with"),
            CONTENT_TYPE,
            ACCEPT,
            AUTHORIZATION,
        ])
        .allow_credentials(true)
        .expose_headers([has_more_header()])
}

/// Any loopback origin on any port, the two Tauri schemes, plus whatever the
/// operator configured. Loopback is trusted because anything that can bind a
/// port on the user's own machine can already read the browser's storage.
fn is_allowed_origin(origin: &str, extra: &[String]) -> bool {
    if extra.iter().any(|allowed| allowed == origin) {
        return true;
    }
    if origin == "tauri://localhost" || origin == "https://tauri.localhost" {
        return true;
    }

    let Some(rest) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    let (host, port) = match rest.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (rest, None),
    };
    if host != "localhost" && host != "127.0.0.1" {
        return false;
    }
    match port {
        None => true,
        Some(port) => !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()),
    }
}

/// The `Link:` preload header for the HTML document.
///
/// Loading is otherwise a three-step waterfall: the browser parses the HTML,
/// the module script fetches the JS, and only then does the JS request the
/// `.wasm` — which is by far the largest file. A preload hint on the document
/// lets the WASM fetch start immediately, in parallel with the JS, removing a
/// round trip from first paint.
///
/// This lives on the server rather than in the bundler because the filename is
/// content-hashed: only something reading the built directory knows it. Trunk's
/// own `pattern_preload` is accepted by its config parser but emits nothing.
///
/// Returns `None` when the bundle is missing, which is the normal case in tests
/// (the harness serves an empty directory) and must not be fatal.
fn preload_header(static_dir: &Path) -> Option<HeaderValue> {
    let mut wasm = None;
    let mut js = None;

    for entry in std::fs::read_dir(static_dir).ok()? {
        let name = entry.ok()?.file_name().to_string_lossy().into_owned();
        if name.ends_with("_bg.wasm") {
            wasm = Some(name);
        } else if name.ends_with(".js") {
            js = Some(name);
        }
    }

    let wasm = wasm?;
    // `as=fetch`, not `as=script`: the bundle is consumed by
    // `WebAssembly.instantiateStreaming`. `crossorigin` is required or the
    // preload sits in a different cache partition and the fetch happens twice —
    // which would make this slower, not faster.
    let mut value =
        format!("</{wasm}>; rel=preload; as=fetch; type=\"application/wasm\"; crossorigin");
    if let Some(js) = js {
        value.push_str(&format!(", </{js}>; rel=modulepreload"));
    }

    HeaderValue::from_str(&value).ok()
}

/// Brotli/gzip for everything worth compressing.
///
/// The WASM bundle dominates the payload — around 1.7 MB in release, and it is
/// highly compressible, so this is the difference between a fast first load and
/// a slow one. It costs CPU per response, which is why the exclusions matter:
///
/// - **`text/event-stream`** must never be compressed. The encoder buffers to
///   fill a block, and a buffered stream is not a stream — SSE events would
///   arrive in clumps or not until the connection closed.
/// - **Images** are already compressed; re-encoding them burns CPU to make them
///   very slightly larger.
/// - **Tiny bodies** cost more in framing than they save. Most API responses
///   here are a few hundred bytes.
fn compression_layer() -> CompressionLayer<impl Predicate + Send + Sync + 'static> {
    // The vendored Topcoat fonts are OTF, which is not pre-compressed, so they
    // are deliberately left in scope — they compress well.
    let predicate = SizeAbove::new(512)
        .and(NotForContentType::SSE)
        .and(NotForContentType::IMAGES);

    CompressionLayer::new()
        .br(true)
        .gzip(true)
        .compress_when(predicate)
}

/// Turn the SPA fallback back into a 404 for requests that wanted an asset.
///
/// `ServeDir`'s fallback answers *everything* it cannot find with `index.html`,
/// which is right for a client route like `/rooms/abc` and wrong for
/// `/app-abc123.js`. A browser holding a stale `index.html` after a redeploy
/// then asks for a bundle that no longer exists, receives HTML with a 200, and
/// reports:
///
/// ```text
/// Failed to load module script: Expected a JavaScript-or-Wasm module script
/// but the server responded with a MIME type of "text/html".
/// ```
///
/// which sends the reader looking for a MIME configuration problem that does
/// not exist. A plain 404 says what actually happened.
///
/// The test is the `Accept` header, not the file extension: room ids may
/// legitimately contain a dot (`docs/API.md` §3.1), so `/rooms/a.b` is a route,
/// not a file. Browsers send `text/html` when navigating and `*/*` for module
/// scripts and `fetch`, which is exactly the distinction needed.
async fn spa_fallback_guard(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let wants_document = req
        .headers()
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/html"));

    // Strictly `text/html` — **not** `*/*`. A browser sends `*/*` when fetching
    // a module script, so treating it as "wants a document" would let exactly
    // the case this guard exists for straight through.
    //
    // The path carries the real signal: `/` and `/rooms/abc` name no file and
    // are always documents, so `curl http://host:9099/` still works. Only a
    // path naming a file can 404 here.
    let looks_like_a_file = req
        .uri()
        .path()
        .rsplit('/')
        .next()
        .is_some_and(|last| last.contains('.'));

    let response = next.run(req).await;

    let served_the_shell = response.status() == StatusCode::OK
        && response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/html"));

    // Both conditions, not either: a path naming a file that the client will
    // not render as a document is an asset request, and answering it with the
    // app shell produces the misleading MIME error described above.
    if served_the_shell && looks_like_a_file && !wants_document {
        return (StatusCode::NOT_FOUND, "Not found").into_response();
    }
    response
}

/// Serve the built web client with an SPA fallback.
///
/// A request for a path the client routes internally (`/rooms/abc`) has no
/// file behind it, so anything unmatched returns `index.html` and lets the
/// client router take over. API routes are registered on the router proper and
/// therefore win over this fallback.
fn static_service(cfg: &Config) -> ServeDir<ServeFile> {
    let index = cfg.static_dir.join("index.html");
    ServeDir::new(&cfg.static_dir).fallback(ServeFile::new(index))
}

/// Tell browsers what they may cache, because otherwise they guess.
///
/// Without an explicit header a browser applies a *heuristic* freshness window
/// to anything carrying `Last-Modified` — which means an edited `app.css` or a
/// redeployed `index.html` can keep serving the old bytes with no request made
/// at all. That is the difference between "deployed" and "actually visible".
///
/// Two regimes:
///
/// - Trunk's bundle filenames embed a content hash, so a changed file is a
///   changed URL. Those are safe to cache forever, and `immutable` also stops
///   the revalidation request on reload.
/// - Everything else — `index.html`, `app.css`, images — keeps a stable URL, so
///   it must be revalidated. `no-cache` still allows a 304, so this costs one
///   conditional request, not a re-download.
async fn cache_control(
    axum::extract::State(preload): axum::extract::State<Option<HeaderValue>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let hashed = is_content_hashed(req.uri().path());
    let mut response = next.run(req).await;

    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        if hashed {
            HeaderValue::from_static("public, max-age=31536000, immutable")
        } else {
            HeaderValue::from_static("no-cache")
        },
    );

    // Only the HTML document benefits: preloading from within an asset response
    // would arrive too late to overlap anything.
    let is_html = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html"));

    if is_html {
        if let Some(link) = preload {
            response
                .headers_mut()
                .insert(axum::http::header::LINK, link);
        }
    }

    response
}

/// Whether a path carries a content hash, and so can never change meaning.
///
/// Trunk emits `<crate>-<hex>.js` and `<crate>-<hex>_bg.wasm`. Matching on the
/// hex run rather than the extension keeps this correct if the bundler starts
/// hashing something else, and conservative if it stops: an unrecognised name
/// falls through to `no-cache`, which is merely slower, never wrong.
fn is_content_hashed(path: &str) -> bool {
    let Some(file) = path.rsplit('/').next() else {
        return false;
    };
    let stem = file
        .strip_suffix("_bg.wasm")
        .or_else(|| file.strip_suffix(".js"))
        .or_else(|| file.strip_suffix(".wasm"));
    let Some(stem) = stem else { return false };

    // The hash is the trailing `-`-delimited segment.
    let Some((_, tail)) = stem.rsplit_once('-') else {
        return false;
    };
    tail.len() >= 8 && tail.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Attach `X-Has-More` to a `/sync` response.
///
/// The flag rides in a header rather than the body so the body stays a plain
/// JSON array — older CLIs parse it positionally and would break on an object.
pub fn with_has_more(body: impl IntoResponse, has_more: bool) -> Response {
    let mut response = body.into_response();
    response.headers_mut().insert(
        has_more_header(),
        if has_more {
            HeaderValue::from_static("true")
        } else {
            HeaderValue::from_static("false")
        },
    );
    response
}

/// A 200 carrying only a `message`, the shape most mutating endpoints return.
pub fn message(text: &str) -> Response {
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "message": text })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use tower::ServiceExt;

    use super::*;
    use crate::test_support::{register, send, state, wallet};

    #[tokio::test]
    async fn a_missing_asset_is_a_404_not_the_app_shell() {
        // A browser holding a stale `index.html` after a redeploy asks for a
        // bundle that no longer exists. Answering with HTML and a 200 produced
        // "Expected a JavaScript-or-Wasm module script but the server responded
        // with a MIME type of text/html" — which points at a MIME
        // misconfiguration that was never the problem.
        let state = state("spa-fallback");
        // The shell must actually exist, or `ServeDir` 404s on its own and the
        // test passes without ever exercising the guard — which is exactly how
        // an earlier version of this test stayed green while the live server
        // returned 200.
        let dir = state.cfg.static_dir.clone();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), "<!doctype html><title>app</title>").unwrap();

        let app = build(state);

        // A module script: names a file, and will not render a document.
        for path in [
            "/pocketskynet-web-deadbeef.js",
            "/nope_bg.wasm",
            "/static/missing.css",
        ] {
            let req = Request::builder()
                .uri(path)
                .header("accept", "*/*")
                .body(Body::empty())
                .unwrap();
            let res = app.clone().oneshot(req).await.unwrap();
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "{path} should 404");
        }
    }

    #[tokio::test]
    async fn the_app_root_is_served_even_to_a_client_that_asks_for_anything() {
        // `curl http://host:9099/` and most health checks send `*/*`. Refusing
        // them the app root would look like the server was broken.
        let state = state("spa-root");
        let dir = state.cfg.static_dir.clone();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), "<!doctype html><title>app</title>").unwrap();

        let app = build(state);
        let req = Request::builder()
            .uri("/")
            .header("accept", "*/*")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_client_route_still_gets_the_app_shell() {
        // The other half: deep links must keep working, including room ids that
        // contain a dot — which is why this keys on `Accept`, not on whether
        // the path looks like a filename.
        let state = state("spa-route");
        let dir = state.cfg.static_dir.clone();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), "<!doctype html><title>app</title>").unwrap();

        let app = build(state);
        for path in [
            "/rooms/room_0000000001",
            "/rooms/room_00000001.b",
            "/settings",
        ] {
            let nav = Request::builder()
                .uri(path)
                .header("accept", "text/html,application/xhtml+xml")
                .body(Body::empty())
                .unwrap();
            let res = app.clone().oneshot(nav).await.unwrap();
            assert_eq!(
                res.status(),
                StatusCode::OK,
                "deep link {path} should load the app"
            );
        }
    }

    #[test]
    fn the_preload_hint_names_the_bundle_and_uses_fetch_semantics() {
        let dir = std::env::temp_dir().join(format!("ps-preload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("app-deadbeef01234567_bg.wasm"), b"\0asm").unwrap();
        std::fs::write(dir.join("app-deadbeef01234567.js"), b"export{}").unwrap();

        let header = preload_header(&dir).expect("a bundle was present");
        let value = header.to_str().unwrap();

        assert!(value.contains("</app-deadbeef01234567_bg.wasm>; rel=preload"));
        // `as=fetch`, not `as=script`: the wrong one makes the browser fetch the
        // bundle twice, which is slower than not preloading at all.
        assert!(value.contains("as=fetch"), "got: {value}");
        assert!(value.contains("crossorigin"), "got: {value}");
        assert!(value.contains("rel=modulepreload"), "got: {value}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_bundle_yields_no_preload_rather_than_failing() {
        // The test harness and a fresh checkout both serve a directory with no
        // build in it; that must not be fatal.
        let dir = std::env::temp_dir().join(format!("ps-preload-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        assert!(preload_header(&dir).is_none());
        assert!(preload_header(Path::new("/nonexistent/path/xyz")).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn trunk_hashed_bundles_are_treated_as_immutable() {
        for path in [
            "/pocketskynet-web-8276bc825016b1f3_bg.wasm",
            "/pocketskynet-web-8276bc825016b1f3.js",
            "/nested/dir/app-deadbeefcafe1234.js",
        ] {
            assert!(is_content_hashed(path), "should be immutable: {path}");
        }
    }

    #[test]
    fn stable_urls_are_never_treated_as_immutable() {
        // Regression: `app.css` was being heuristically cached by the browser
        // because nothing told it not to, so an edited stylesheet kept serving
        // the previous bytes with no request made at all.
        for path in [
            "/",
            "/index.html",
            "/static/app.css",
            "/static/img/logo.png",
            "/rooms/room_123",
            // A name with a short or non-hex tail is not a content hash.
            "/app-v2.js",
            "/vendor-notahash.js",
            "/bundle-zzzzzzzzzzzz.js",
        ] {
            assert!(!is_content_hashed(path), "should revalidate: {path}");
        }
    }

    #[test]
    fn loopback_origins_are_allowed_on_any_port() {
        for origin in [
            "http://localhost:5173",
            "http://127.0.0.1:9081",
            "https://localhost",
            "http://localhost",
            "tauri://localhost",
            "https://tauri.localhost",
        ] {
            assert!(is_allowed_origin(origin, &[]), "{origin} should be allowed");
        }
    }

    #[test]
    fn everything_else_needs_explicit_configuration() {
        for origin in [
            "http://evil.example",
            "http://localhost.evil.example",
            "http://127.0.0.1.evil.example",
            "http://localhost:notaport",
            "ftp://localhost",
            "null",
        ] {
            assert!(!is_allowed_origin(origin, &[]), "{origin} must be refused");
        }
        assert!(is_allowed_origin(
            "https://chat.example",
            &["https://chat.example".to_string()]
        ));
    }

    #[tokio::test]
    async fn security_headers_are_on_every_response() {
        let state = state("headers");
        let router = build(state);
        let response = send(&router, "GET", "/api/health", None, None).await;

        assert_eq!(response.headers["x-content-type-options"], "nosniff");
        assert_eq!(response.headers["x-frame-options"], "DENY");
        assert_eq!(
            response.headers["referrer-policy"],
            "strict-origin-when-cross-origin"
        );
        assert!(response.headers.contains_key("permissions-policy"));
    }

    #[tokio::test]
    async fn unknown_api_routes_answer_with_the_json_envelope() {
        let router = build(state("notfound"));
        let response = send(&router, "GET", "/api/does-not-exist", None, None).await;

        assert_eq!(response.status, StatusCode::NOT_FOUND);
        assert!(response.json()["message"].is_string());
    }

    #[tokio::test]
    async fn a_wrong_method_still_answers_json() {
        let router = build(state("method"));
        let response = send(&router, "DELETE", "/api/health", None, None).await;

        assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED);
        assert!(response.json()["message"].is_string());
    }

    #[tokio::test]
    async fn static_segments_win_over_the_address_parameter() {
        // §14.1: these four literals must not be captured as `:address`
        // or `:roomId`. Express needed registration order for this; the
        // assertion is here so a future refactor cannot silently lose it.
        let state = state("precedence");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        let search = send(
            &router,
            "GET",
            "/api/users/search?q=ali",
            Some(&token),
            None,
        )
        .await;
        assert_eq!(search.status, StatusCode::OK);
        assert!(
            search.json().is_array(),
            "search must not hit /users/:address"
        );

        let blocked = send(&router, "GET", "/api/users/blocked", Some(&token), None).await;
        assert!(blocked.json().is_array());

        let blocked_by = send(&router, "GET", "/api/users/blocked-by", Some(&token), None).await;
        assert!(blocked_by.json().is_array());

        let hidden = send(&router, "GET", "/api/rooms/hidden", Some(&token), None).await;
        assert_eq!(hidden.status, StatusCode::OK);
        assert!(
            hidden.json().is_array(),
            "hidden must not hit /rooms/:roomId"
        );
    }

    #[tokio::test]
    async fn an_oversized_body_is_refused_before_it_is_parsed() {
        let state = state("bodylimit");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        let huge = serde_json::json!({ "name": "x".repeat(MAX_BODY_BYTES + 1024) });
        let response = send(&router, "POST", "/api/rooms", Some(&token), Some(huge)).await;

        assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(response.json()["message"].is_string());
    }

    #[tokio::test]
    async fn rate_limit_headers_accompany_api_responses() {
        let mut state = state("ratelimit-headers");
        state.limiter = Arc::new(crate::ratelimit::RateLimiter::new(true));
        let router = build(state);

        let response = send(&router, "GET", "/api/blockchain/info", None, None).await;
        assert_eq!(response.headers["ratelimit-limit"], "100");
        assert!(response.headers.contains_key("ratelimit-remaining"));
        assert!(response.headers.contains_key("ratelimit-reset"));
    }

    #[tokio::test]
    async fn health_is_exempt_from_rate_limiting() {
        let mut state = state("health-exempt");
        state.limiter = Arc::new(crate::ratelimit::RateLimiter::new(true));
        let router = build(state);

        // Well past the 100/min general budget.
        for _ in 0..150 {
            let response = send(&router, "GET", "/api/health", None, None).await;
            assert_eq!(response.status, StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn the_general_limiter_trips_at_a_hundred() {
        let mut state = state("ratelimit-trip");
        state.limiter = Arc::new(crate::ratelimit::RateLimiter::new(true));
        let router = build(state);

        let mut refused = 0;
        for _ in 0..105 {
            let response = send(&router, "GET", "/api/blockchain/info", None, None).await;
            if response.status == StatusCode::TOO_MANY_REQUESTS {
                refused += 1;
                assert!(response.json()["message"]
                    .as_str()
                    .unwrap()
                    .contains("Too many requests"));
            }
        }
        assert_eq!(refused, 5);
    }
}
