//! Per-IP fixed-window rate limiting (`docs/API.md` §2).
//!
//! Three cumulative limiters share one table. A login request consumes a slot
//! in both the login limiter and the general limiter, which is what makes the
//! tight auth limits meaningful — otherwise an attacker could spend their
//! general budget entirely on logins.
//!
//! The window is fixed rather than sliding. A fixed window admits up to twice
//! the nominal rate across a boundary, which for a 100/min budget is a
//! rounding error, and it costs one integer per key instead of a timestamp
//! ring. The reference used the same scheme, so the observable behaviour
//! matches.
//!
//! Keying is on the socket's peer address unless the operator opts in with
//! `--trust-proxy N`. `X-Forwarded-For` is client-controlled, so trusting it by
//! default would let anyone rotate the header and defeat the limiter entirely —
//! strictly worse than throttling a shared NAT. Behind a real proxy, though,
//! every request arrives from one address and the whole deployment shares a
//! single bucket, so the opt-in has to exist. See [`client_ip`].

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use dashmap::DashMap;

use crate::error::ApiError;
use crate::AppState;

const WINDOW: Duration = Duration::from_secs(60);

/// Above this many tracked keys, expired windows are swept. The sweep is O(n)
/// but runs only when the table has actually grown, so a normal workload never
/// pays for it.
const SWEEP_THRESHOLD: usize = 10_000;

/// Which budget a request draws from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// Every `/api` route except `/api/health` and the upload chunk endpoints.
    General,
    /// Serving stored media (`/api/files/{id}/raw`, `/api/images/{name}`).
    ///
    /// Its own budget for the mirror image of the upload reason: a `<video>`
    /// element *streams* by issuing many small `Range` requests, and Safari in
    /// particular probes an mp4 with dozens to hundreds of tiny ranges before
    /// it plays a frame. Under the general 100/min budget one film exhausted
    /// the caller's entire allowance — and because the budget is per IP, it
    /// then 429'd everything else from that device, including the login
    /// challenge after a refresh. A meter that counts requests is simply the
    /// wrong instrument for range streaming; the real costs (bytes on disk,
    /// membership) are checked per request regardless.
    Media,
    /// The resumable upload routes (`/api/uploads/…`).
    ///
    /// Its own budget because a chunked upload is *supposed* to make hundreds
    /// of requests: a 4 GB file is 512 chunks at the suggested size, and under
    /// the general 100/min budget it would 429 partway through and stall — which
    /// is exactly what happened to the first 888 MB film put through it.
    ///
    /// Counting requests is the wrong meter here anyway. What an upload costs
    /// is bytes and disk, and both are already bounded: 4 GB per file, eight
    /// open sessions per wallet, one chunk of server memory at a time. This
    /// number only has to stop a client spinning, so it is generous enough that
    /// a full-speed upload never notices it.
    Upload,
    /// `POST /api/auth/challenge`.
    Challenge,
    /// `POST /api/auth/login`.
    Login,
    /// `POST /api/webhooks/{token}` — the unauthenticated webhook post.
    ///
    /// Its own budget, applied *instead of* [`Scope::General`], for the same
    /// structural reason the auth endpoints have theirs: this is the one
    /// message-writing route with no wallet behind it, so the IP is the only
    /// identity there is to meter. One post a second sustained is more than
    /// any CI pipeline produces and little enough that a looping script
    /// cannot flood a room between someone noticing and revoking the token.
    Webhook,
}

impl Scope {
    pub fn max_per_minute(self) -> u32 {
        match self {
            Self::General => 100,
            // A two-hour film seeked aggressively is a few hundred ranges;
            // this only exists to stop a spinning client, not to meter
            // playback.
            Self::Media => 3_000,
            // 4 GB at 8 MB chunks is 512 requests; a fast LAN can push those in
            // well under a minute, so the budget has to clear that with room
            // for the status probes a resume makes.
            Self::Upload => 1_200,
            Self::Challenge => 10,
            Self::Login => 5,
            Self::Webhook => 60,
        }
    }

    /// The 429 body. Distinct per scope so a client can tell which budget it
    /// exhausted and back off the right thing.
    pub fn message(self) -> &'static str {
        match self {
            Self::General => "Too many requests, please try again later",
            Self::Media => "Too many media requests, please slow down",
            // Names the offset endpoint rather than the file, because the fix
            // is to slow the chunk loop and resume, not to restart the upload.
            Self::Upload => "Too many upload requests, please slow down and resume",
            Self::Challenge => "Too many challenge requests, please try again later",
            Self::Login => "Too many login attempts, please try again later",
            Self::Webhook => "Too many webhook posts, please slow down",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Window {
    start: Instant,
    count: u32,
}

/// The outcome of a check, in the form the `RateLimit-*` headers need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub limit: u32,
    pub remaining: u32,
    /// Seconds until the current window resets.
    pub reset: u64,
    pub allowed: bool,
}

pub struct RateLimiter {
    enabled: bool,
    windows: DashMap<(Scope, IpAddr), Window>,
    /// Cheap growth signal so the sweep does not have to check `len()` on a
    /// sharded map for every request.
    checks: AtomicU64,
}

impl std::fmt::Debug for RateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimiter")
            .field("enabled", &self.enabled)
            .field("tracked", &self.windows.len())
            .finish()
    }
}

impl RateLimiter {
    /// `enabled = false` disables every limiter — the switch tests use, since
    /// a suite driving dozens of wallets from one address would otherwise
    /// trip the per-IP budget and fail for reasons unrelated to the test.
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            windows: DashMap::new(),
            checks: AtomicU64::new(0),
        }
    }

    /// Consume one slot, at the instant `now`.
    ///
    /// Taking `now` as a parameter is what lets the tests exercise window
    /// expiry without sleeping for a minute.
    pub fn check_at(&self, scope: Scope, ip: IpAddr, now: Instant) -> Verdict {
        let limit = scope.max_per_minute();
        if !self.enabled {
            return Verdict {
                limit,
                remaining: limit,
                reset: WINDOW.as_secs(),
                allowed: true,
            };
        }

        if self.checks.fetch_add(1, Ordering::Relaxed) % 1024 == 0
            && self.windows.len() > SWEEP_THRESHOLD
        {
            self.windows
                .retain(|_, w| now.duration_since(w.start) < WINDOW);
        }

        let mut entry = self.windows.entry((scope, ip)).or_insert(Window {
            start: now,
            count: 0,
        });

        if now.duration_since(entry.start) >= WINDOW {
            entry.start = now;
            entry.count = 0;
        }

        // The counter increments even on a refusal: a client that keeps
        // hammering keeps the window pinned rather than sliding back into
        // budget one request at a time.
        entry.count += 1;
        let used = entry.count;
        let elapsed = now.duration_since(entry.start);

        Verdict {
            limit,
            remaining: limit.saturating_sub(used),
            reset: (WINDOW.saturating_sub(elapsed)).as_secs(),
            allowed: used <= limit,
        }
    }

    pub fn check(&self, scope: Scope, ip: IpAddr) -> Verdict {
        self.check_at(scope, ip, Instant::now())
    }
}

/// The peer address, or loopback when the router was built without connection
/// info (which only happens in tests that call the service directly).
fn peer_ip(req: &Request) -> IpAddr {
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

/// The address to bill this request to.
///
/// With `trust_proxy = 0` (the default) this is the socket's peer address, full
/// stop. `X-Forwarded-For` is client-controlled: honouring it unconditionally
/// lets anyone rotate the header and bypass the limiter entirely, which is far
/// worse than throttling a shared NAT.
///
/// When an operator sets `--trust-proxy N`, they are asserting that exactly `N`
/// proxies they control sit in front. Those proxies *append* to the header, so
/// the trustworthy entries are the rightmost ones and the real client is the
/// `N`th from the right. Counting from the left instead is the classic
/// spoofing hole — the leftmost entry is whatever the client sent.
fn client_ip(req: &Request, trust_proxy: u8) -> IpAddr {
    let peer = peer_ip(req);
    if trust_proxy == 0 {
        return peer;
    }

    let Some(forwarded) = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
    else {
        return peer;
    };

    let hops: Vec<&str> = forwarded
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    // `N` hops from the right. If the header is shorter than promised, the
    // chain is not what was configured, so fall back to the peer rather than
    // trusting an entry the proxies did not write.
    hops.len()
        .checked_sub(trust_proxy as usize)
        .and_then(|i| hops.get(i))
        .and_then(|s| s.parse::<IpAddr>().ok())
        .unwrap_or(peer)
}

fn apply_headers(response: &mut Response, verdict: Verdict) {
    let headers = response.headers_mut();
    // `standardHeaders` naming (RFC draft), not the legacy `X-RateLimit-*`.
    headers.insert("RateLimit-Limit", HeaderValue::from(verdict.limit));
    headers.insert("RateLimit-Remaining", HeaderValue::from(verdict.remaining));
    headers.insert("RateLimit-Reset", HeaderValue::from(verdict.reset));
}

async fn enforce(scope: Scope, state: AppState, req: Request, next: Next) -> Response {
    let ip = client_ip(&req, state.cfg.trust_proxy);
    let verdict = state.limiter.check(scope, ip);

    let mut response = if verdict.allowed {
        next.run(req).await
    } else {
        ApiError::TooManyRequests(scope.message().to_owned()).into_response()
    };

    apply_headers(&mut response, verdict);
    response
}

/// 100/min, applied to every `/api` route except `/api/health`.
pub async fn general(State(state): State<AppState>, req: Request, next: Next) -> Response {
    enforce(Scope::General, state, req, next).await
}

/// The media budget, applied *instead of* [`general`] — see [`Scope::Media`].
pub async fn media(State(state): State<AppState>, req: Request, next: Next) -> Response {
    enforce(Scope::Media, state, req, next).await
}

/// The upload budget, applied *instead of* [`general`] — see [`Scope::Upload`].
pub async fn upload(State(state): State<AppState>, req: Request, next: Next) -> Response {
    enforce(Scope::Upload, state, req, next).await
}

/// 10/min, on top of the general budget.
pub async fn challenge(State(state): State<AppState>, req: Request, next: Next) -> Response {
    enforce(Scope::Challenge, state, req, next).await
}

/// 5/min, on top of the general budget.
pub async fn login(State(state): State<AppState>, req: Request, next: Next) -> Response {
    enforce(Scope::Login, state, req, next).await
}

/// The webhook-post budget, applied *instead of* [`general`] — see
/// [`Scope::Webhook`].
pub async fn webhook(State(state): State<AppState>, req: Request, next: Next) -> Response {
    enforce(Scope::Webhook, state, req, next).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, last))
    }

    /// A request arriving from `peer`, optionally carrying `X-Forwarded-For`.
    fn req_from(peer: IpAddr, xff: Option<&str>) -> Request {
        let mut b = Request::builder().uri("/api/rooms");
        if let Some(v) = xff {
            b = b.header("x-forwarded-for", v);
        }
        let mut req = b.body(axum::body::Body::empty()).unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(peer, 40000)));
        req
    }

    #[test]
    fn without_trust_proxy_a_forwarded_header_is_ignored_entirely() {
        // The whole point: a client that could pick its own rate-limit key
        // would simply not be rate limited.
        let req = req_from(ip(9), Some("1.2.3.4"));
        assert_eq!(client_ip(&req, 0), ip(9));

        // Including the tricks — multiple entries, spoofed private ranges.
        let req = req_from(ip(9), Some("1.2.3.4, 5.6.7.8, 10.0.0.1"));
        assert_eq!(client_ip(&req, 0), ip(9));
    }

    #[test]
    fn with_one_trusted_proxy_the_client_is_the_last_entry() {
        // One proxy appends the peer it saw, so the rightmost entry is the one
        // our own infrastructure wrote.
        let req = req_from(ip(9), Some("203.0.113.7"));
        assert_eq!(client_ip(&req, 1), "203.0.113.7".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn a_client_cannot_forge_the_entry_that_gets_trusted() {
        // The client prepends a lie; the single trusted proxy appends the truth.
        // Reading from the right must land on the proxy's entry, not the lie.
        let req = req_from(ip(9), Some("6.6.6.6, 203.0.113.7"));
        assert_eq!(
            client_ip(&req, 1),
            "203.0.113.7".parse::<IpAddr>().unwrap(),
            "counting from the left is the classic spoofing hole"
        );

        // Two trusted proxies: the client is two from the right.
        let req = req_from(ip(9), Some("6.6.6.6, 203.0.113.7, 198.51.100.2"));
        assert_eq!(client_ip(&req, 2), "203.0.113.7".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn a_chain_shorter_than_configured_falls_back_to_the_peer() {
        // Configured for two proxies but only one entry present: the deployment
        // is not what was described, so trust nothing in the header.
        let req = req_from(ip(9), Some("6.6.6.6"));
        assert_eq!(client_ip(&req, 2), ip(9));

        let req = req_from(ip(9), None);
        assert_eq!(client_ip(&req, 1), ip(9));

        // A garbage entry where the client address should be.
        let req = req_from(ip(9), Some("not-an-ip"));
        assert_eq!(client_ip(&req, 1), ip(9));
    }

    #[test]
    fn forwarded_addresses_get_separate_budgets() {
        // Otherwise every user behind the proxy shares one bucket, which is the
        // reason this option exists.
        let limiter = RateLimiter::new(true);
        let now = Instant::now();
        let a = req_from(ip(9), Some("203.0.113.1"));
        let b = req_from(ip(9), Some("203.0.113.2"));

        for _ in 0..Scope::Login.max_per_minute() {
            assert!(
                limiter
                    .check_at(Scope::Login, client_ip(&a, 1), now)
                    .allowed
            );
        }
        assert!(
            !limiter
                .check_at(Scope::Login, client_ip(&a, 1), now)
                .allowed
        );
        assert!(
            limiter
                .check_at(Scope::Login, client_ip(&b, 1), now)
                .allowed,
            "a second client behind the same proxy must keep its own budget"
        );
    }

    #[test]
    fn a_budget_is_spent_then_refused() {
        let limiter = RateLimiter::new(true);
        let now = Instant::now();

        for i in 0..5 {
            let v = limiter.check_at(Scope::Login, ip(1), now);
            assert!(v.allowed, "request {i} should be inside the budget");
            assert_eq!(v.remaining, 4 - i);
        }

        let over = limiter.check_at(Scope::Login, ip(1), now);
        assert!(!over.allowed);
        assert_eq!(over.remaining, 0);
    }

    #[test]
    fn the_window_resets_after_a_minute() {
        let limiter = RateLimiter::new(true);
        let start = Instant::now();

        for _ in 0..5 {
            limiter.check_at(Scope::Login, ip(1), start);
        }
        assert!(!limiter.check_at(Scope::Login, ip(1), start).allowed);

        let later = start + WINDOW + Duration::from_millis(1);
        let after = limiter.check_at(Scope::Login, ip(1), later);
        assert!(after.allowed, "a fresh window must restore the budget");
        assert_eq!(after.remaining, 4);
    }

    #[test]
    fn a_refused_request_keeps_the_window_pinned() {
        let limiter = RateLimiter::new(true);
        let start = Instant::now();
        for _ in 0..20 {
            limiter.check_at(Scope::Login, ip(1), start);
        }
        // Still inside the window: hammering does not buy the caller anything.
        let mid = start + Duration::from_secs(30);
        assert!(!limiter.check_at(Scope::Login, ip(1), mid).allowed);
    }

    #[test]
    fn budgets_are_independent_per_scope_and_per_address() {
        let limiter = RateLimiter::new(true);
        let now = Instant::now();

        for _ in 0..5 {
            limiter.check_at(Scope::Login, ip(1), now);
        }
        assert!(!limiter.check_at(Scope::Login, ip(1), now).allowed);

        // A different scope from the same IP still has its own budget…
        assert!(limiter.check_at(Scope::Challenge, ip(1), now).allowed);
        // …and a different IP is entirely unaffected.
        assert!(limiter.check_at(Scope::Login, ip(2), now).allowed);
    }

    #[test]
    fn the_documented_limits_are_what_is_enforced() {
        assert_eq!(Scope::General.max_per_minute(), 100);
        assert_eq!(Scope::Challenge.max_per_minute(), 10);
        assert_eq!(Scope::Login.max_per_minute(), 5);
        assert_eq!(Scope::Webhook.max_per_minute(), 60);

        let limiter = RateLimiter::new(true);
        let now = Instant::now();
        for _ in 0..100 {
            assert!(limiter.check_at(Scope::General, ip(3), now).allowed);
        }
        assert!(!limiter.check_at(Scope::General, ip(3), now).allowed);
    }

    #[test]
    fn disabling_the_limiter_never_refuses() {
        let limiter = RateLimiter::new(false);
        let now = Instant::now();
        for _ in 0..1000 {
            let v = limiter.check_at(Scope::Login, ip(1), now);
            assert!(v.allowed);
            assert_eq!(v.remaining, v.limit, "headers stay honest when disabled");
        }
    }

    #[test]
    fn reset_counts_down_within_the_window() {
        let limiter = RateLimiter::new(true);
        let start = Instant::now();
        let first = limiter.check_at(Scope::General, ip(4), start);
        let later = limiter.check_at(Scope::General, ip(4), start + Duration::from_secs(20));

        assert_eq!(first.reset, 60);
        assert!(later.reset <= 40 && later.reset >= 39);
    }
}
