//! PocketSkynet server: REST + WebSocket + SSE over SQLite and a JSONL log.
//!
//! The crate is a library so the binary is a thin `main` and the whole surface
//! is reachable from tests. [`AppState`] is the single piece of shared state;
//! everything in it is an `Arc` or an `Arc`-backed handle, so cloning it per
//! request is free and no handler needs a lock to reach the database, the
//! hub, or the configuration.

#![forbid(unsafe_code)]

pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod http3;
pub mod hub;
pub mod jsonl;
pub mod payment;
pub mod ratelimit;
pub mod routes;
pub mod search;
pub mod tls;
pub mod validate;

use std::sync::Arc;
use std::time::Instant;

use crate::auth::JwtKeys;
use crate::config::{Config, Secret};
use crate::db::Db;
use crate::hub::Hub;
use crate::jsonl::JsonlLog;
use crate::ratelimit::RateLimiter;
use crate::routes::realtime::TicketStore;

/// Everything a handler can reach. Cheap to clone.
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub hub: Arc<Hub>,
    pub log: Arc<JsonlLog>,
    pub jwt: Arc<JwtKeys>,
    pub tickets: Arc<TicketStore>,
    pub cfg: Arc<Config>,
    pub limiter: Arc<RateLimiter>,
    /// Process start, for `GET /api/health`'s uptime.
    pub started: Instant,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("db", &self.db)
            .field("hub", &self.hub)
            .field("limiter", &self.limiter)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error(transparent)]
    Db(#[from] db::DbError),
    #[error(transparent)]
    Log(#[from] jsonl::LogError),
}

impl AppState {
    /// Open the database and the event log, and wire up the hub.
    pub fn build(cfg: Config, secret: Secret) -> Result<Self, StartupError> {
        let db = Db::open(&cfg.db_path())?;
        let log = Arc::new(JsonlLog::open(cfg.events_dir())?);
        let hub = Hub::new(log.clone(), db.clone());
        let jwt = Arc::new(JwtKeys::new(&secret.0, cfg.jwt_ttl_hours));
        let limiter = Arc::new(RateLimiter::new(cfg.rate_limit));

        Ok(Self {
            db,
            hub,
            log,
            jwt,
            tickets: Arc::new(TicketStore::new()),
            cfg: Arc::new(cfg),
            limiter,
            started: Instant::now(),
        })
    }
}

/// Which scheme the server answers on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }
}

/// How far away a client has to be to use a given address.
///
/// Worth distinguishing because the answer to "which one do I put in my
/// tablet?" depends entirely on where the tablet is, and the addresses look
/// alike on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reach {
    /// Loopback: this machine only.
    Local,
    /// A physical network the host is attached to — same wifi, same office.
    Lan,
    /// The carrier-grade NAT range `100.64.0.0/10`, which on a personal machine
    /// is essentially always a mesh VPN (Tailscale and its like). Singled out
    /// because it is the address that keeps working from anywhere, which makes
    /// it the right one to hand out for genuinely remote access.
    Vpn,
}

impl Reach {
    pub fn label(self) -> &'static str {
        match self {
            Reach::Local => "local",
            Reach::Lan => "network",
            Reach::Vpn => "vpn",
        }
    }

    fn of(ip: std::net::Ipv4Addr) -> Self {
        if ip.is_loopback() {
            Reach::Local
        } else if ip.octets()[0] == 100 && (64..128).contains(&ip.octets()[1]) {
            Reach::Vpn
        } else {
            Reach::Lan
        }
    }
}

/// One address the server can be opened at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub url: String,
    pub reach: Reach,
}

/// Every URL this server is reachable on, loopback first.
///
/// Lives here rather than in a launch script so the packaged app, the desktop
/// app and `make start` all report the same thing — the URL you hand to someone
/// else is the single most important line the server prints, and three
/// implementations of it would eventually disagree.
///
/// When the bind address is a specific interface there is exactly one answer.
/// When it is the wildcard, every non-loopback IPv4 the host holds is a real
/// answer, so they are all listed.
pub fn connect_urls(addr: std::net::SocketAddr, scheme: Scheme) -> Vec<Endpoint> {
    let port = addr.port();
    let s = scheme.as_str();
    let mut urls = vec![Endpoint {
        url: format!("{s}://127.0.0.1:{port}"),
        reach: Reach::Local,
    }];

    if !addr.ip().is_unspecified() {
        if let std::net::IpAddr::V4(v4) = addr.ip() {
            if !v4.is_loopback() {
                urls.push(Endpoint {
                    url: format!("{s}://{addr}"),
                    reach: Reach::of(v4),
                });
            }
        }
        return urls;
    }

    let Ok(interfaces) = if_addrs::get_if_addrs() else {
        return urls;
    };
    let mut reachable: Vec<Endpoint> = interfaces
        .into_iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.addr.ip() {
            // IPv4 only: an IPv6 URL needs brackets and is rarely what someone
            // types off a screen.
            std::net::IpAddr::V4(v4) if !v4.is_link_local() => Some(Endpoint {
                url: format!("{s}://{v4}:{port}"),
                reach: Reach::of(v4),
            }),
            _ => None,
        })
        .collect();
    // Grouped by how far away a client has to be, so the list reads as
    // categories rather than as sorted numbers.
    reachable.sort_by(|a, b| a.reach.cmp(&b.reach).then_with(|| a.url.cmp(&b.url)));
    reachable.dedup();
    urls.extend(reachable);
    urls
}

/// The same endpoints, addressed at the QUIC port.
///
/// Always `https://`, whatever the TCP listener is doing: QUIC has no
/// plaintext mode, so a `http://` HTTP/3 URL would be a URL nothing can open.
pub fn http3_urls(endpoints: &[Endpoint], port: u16) -> Vec<Endpoint> {
    endpoints
        .iter()
        .filter_map(|endpoint| {
            // Rebuilt from the host rather than string-replacing the port: a
            // replace would also rewrite a matching digit run inside an
            // address, and `connect_urls` only ever emits `scheme://host:port`.
            let (_, rest) = endpoint.url.split_once("://")?;
            let host = rest.rsplit_once(':')?.0;
            Some(Endpoint {
                url: format!("https://{host}:{port}"),
                reach: endpoint.reach,
            })
        })
        .collect()
}

/// The one URL to hand to another device, or `None` when the server is bound
/// to loopback and there is nothing to hand out.
///
/// A VPN address wins over a LAN address: both work from the next room, only
/// one still works from a train.
pub fn share_url(endpoints: &[Endpoint]) -> Option<&Endpoint> {
    endpoints
        .iter()
        .find(|e| e.reach == Reach::Vpn)
        .or_else(|| endpoints.iter().find(|e| e.reach == Reach::Lan))
}

/// The base URL another device should use to reach this server — the same
/// preference order as the startup banner: the VPN (Tailscale) address when
/// the host has one, else a LAN address, else `None`.
///
/// Served through the API (`GET /api/sites`) so a published site's shareable
/// URL names an address that works *off this machine*, whatever address the
/// viewer themselves happened to type. `None` when the server is bound to
/// loopback, or when the configured port is `0` (the desktop app's
/// ephemeral-port fallback — the bound port is not knowable from the config,
/// and a URL ending in `:0` would be a lie); the client then falls back to
/// its own origin.
pub fn share_base(cfg: &config::Config) -> Option<String> {
    if cfg.port == 0 {
        return None;
    }
    let scheme = if cfg.tls.is_on() {
        Scheme::Https
    } else {
        Scheme::Http
    };
    let endpoints = connect_urls(std::net::SocketAddr::new(cfg.host, cfg.port), scheme);
    share_url(&endpoints).map(|e| e.url.clone())
}

/// The banner shown on startup: where to connect, and who can.
pub fn connect_banner(
    addr: std::net::SocketAddr,
    scheme: Scheme,
    redirect_port: Option<u16>,
) -> String {
    connect_banner_with_http3(addr, scheme, redirect_port, None)
}

/// [`connect_banner`], plus the HTTP/3 endpoint when one is listening.
///
/// Separate entry point rather than a fourth argument on the original: the
/// three-argument form is what the desktop app and a pile of tests call, and
/// HTTP/3 is opt-in enough that most deployments have nothing to report.
pub fn connect_banner_with_http3(
    addr: std::net::SocketAddr,
    scheme: Scheme,
    redirect_port: Option<u16>,
    http3_port: Option<u16>,
) -> String {
    let endpoints = connect_urls(addr, scheme);
    let mut out = String::from("\n  PocketSkynet is running.\n\n");

    // The TCP listener, headed so the two transports read as two lists rather
    // than one list with a footnote.
    out.push_str(&format!(
        "  {} · tcp/{}\n\n",
        match scheme {
            Scheme::Http => "HTTP",
            Scheme::Https => "HTTPS",
        },
        addr.port()
    ));
    for endpoint in &endpoints {
        out.push_str(&format!(
            "    {:<9} {}\n",
            endpoint.reach.label(),
            endpoint.url
        ));
    }

    if let Some(port) = http3_port {
        // The same addresses again, with the QUIC port substituted — a URL
        // that can be copied, rather than a port number the reader has to
        // assemble one themselves. They are `https://` whatever the TCP
        // listener is doing, because QUIC has no plaintext mode.
        //
        // "udp/" is spelled out because the single most confusing thing about
        // running both is that `lsof -i :9101` shows nothing when you forget
        // QUIC is not TCP.
        out.push_str(&format!("\n  HTTP/3 · QUIC · udp/{port}\n\n"));
        for endpoint in http3_urls(&endpoints, port) {
            out.push_str(&format!(
                "    {:<9} {}\n",
                endpoint.reach.label(),
                endpoint.url
            ));
        }
        out.push_str(
            "\n    Advertised to clients as Alt-Svc, so a browser or the iOS app moves\n    \
             there on its own. The TCP listener above still serves everything, and\n    \
             WebSocket exists only there.\n",
        );
    }

    if let Some(share) = share_url(&endpoints) {
        out.push_str(&join_section(&share.url, scheme, redirect_port));
    }

    if addr.ip().is_unspecified() {
        out.push_str(
            "\n  Anyone who can reach a network address above can open it and sign in\n  \
             with their own wallet. Bind to 127.0.0.1 to keep it to this machine.\n",
        );
    } else if addr.ip().is_loopback() {
        out.push_str("\n  Reachable from this machine only.\n");
    }
    out
}

/// Where this run keeps everything it owns.
///
/// Printed beside the connect banner because "where did my upload go?" is a
/// question with a real answer that was previously only discoverable by reading
/// `config.rs`. Attachments in particular are *not* in the database — the bytes
/// are on disk under their own hash and only the name is a row — so an operator
/// backing up `pocketskynet.db` alone would silently lose every file.
///
/// Kept out of [`connect_banner`] rather than folded into it: that function is
/// about reachability and is pinned by tests that assert on its exact lines,
/// and paths have nothing to do with which URL to open.
pub fn storage_banner(cfg: &config::Config) -> String {
    // Always absolute. `data` is what the flag defaults to, and a relative path
    // in a banner is only meaningful if you already know which directory the
    // server was started from — which is the thing being asked.
    //
    // `canonicalize` alone is not enough: it fails on a path that does not
    // exist yet, and most of these are created lazily on first use. Falling
    // back to it would print `data/images` next to four absolute paths, which
    // reads as a different kind of location rather than as the same directory.
    let show = |p: std::path::PathBuf| {
        let absolute = if p.is_absolute() {
            p
        } else {
            std::env::current_dir().map(|cwd| cwd.join(&p)).unwrap_or(p)
        };
        // Resolve symlinks when the path is already there; keep the plain
        // absolute form when it is not.
        std::fs::canonicalize(&absolute)
            .unwrap_or(absolute)
            .display()
            .to_string()
    };

    let mut out = String::from("\n  Stored on this machine:\n\n");
    out.push_str(&format!(
        "    {:<11} {}\n",
        "data",
        show(cfg.data_dir.clone())
    ));
    out.push_str(&format!("    {:<11} {}\n", "database", show(cfg.db_path())));
    out.push_str(&format!("    {:<11} {}\n", "files", show(cfg.files_dir())));
    out.push_str(&format!(
        "    {:<11} {}\n",
        "images",
        show(cfg.images_dir())
    ));
    out.push_str(&format!(
        "    {:<11} {}\n",
        "events",
        show(cfg.events_dir())
    ));
    out.push_str(
        "\n  Attachments are files on disk named by their own SHA-256; the database\n  \
         holds only the name. Back up the whole data directory, not just the .db.\n",
    );
    out
}

/// The copy-and-paste block: the URL on its own line, the same URL as a QR
/// code, and — over HTTPS — what the certificate warning will look like and how
/// to be rid of it.
///
/// Typing `https://100.64.0.2:9099` into a tablet by hand is the single most
/// tedious step in joining a self-hosted server, and getting one digit wrong
/// looks exactly like the server being down. Hence both a line to copy and a
/// code to point a camera at.
fn join_section(url: &str, scheme: Scheme, redirect_port: Option<u16>) -> String {
    let mut out = String::from("\n  Open on a phone or tablet — copy this, or scan below:\n\n");
    out.push_str(&format!("      {url}\n\n"));

    for line in qr_lines(url) {
        out.push_str(&format!("      {line}\n"));
    }

    if scheme == Scheme::Https {
        let host = url
            .trim_start_matches("https://")
            .split(':')
            .next()
            .unwrap_or(url);
        out.push_str(
            "\n  The certificate is self-signed, so the browser objects once:\n  \
             tap \"Show Details\" and then \"visit this website\".\n",
        );
        if let Some(port) = redirect_port {
            out.push_str(&format!(
                "\n  To silence it for good, open  http://{host}:{port}/ca.crt  on the\n  \
                 device and install the profile — on iOS, Settings → General → VPN &\n  \
                 Device Management, then switch it on under Settings → General →\n  \
                 About → Certificate Trust Settings.\n\n  \
                 Plain HTTP on port {port} redirects here, so a typed address works too.\n"
            ));
        }
    }
    out
}

/// Render a URL as a QR code sized for a terminal.
///
/// Polarity is deliberately inverted — light modules drawn, dark modules left
/// as background — because a terminal's background is dark and a scanner needs
/// the modules to contrast the way they would on paper.
fn qr_lines(url: &str) -> Vec<String> {
    use qrcode::render::unicode;

    let Ok(code) = qrcode::QrCode::new(url.as_bytes()) else {
        return Vec::new();
    };
    code.render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Light)
        .light_color(unicode::Dense1x2::Dark)
        .quiet_zone(true)
        .build()
        .lines()
        .map(str::to_string)
        .collect()
}

/// A bound-but-not-yet-serving instance.
///
/// Binding and serving are separate steps so a caller can learn the port before
/// traffic starts. That matters for the desktop app, which asks the OS for an
/// ephemeral port and cannot open its window until it knows which one it got —
/// and it matters for tests, which would otherwise race the server's startup.
pub struct Bound {
    /// The address actually bound, with port 0 already resolved.
    pub addr: std::net::SocketAddr,
    /// What clients must speak to it.
    pub scheme: Scheme,
    /// The plain-HTTP port that redirects here, if one came up.
    pub redirect_port: Option<u16>,
    /// The UDP port serving HTTP/3, if one came up.
    pub http3_port: Option<u16>,
    transport: Transport,
    redirect: Option<(std::net::TcpListener, axum::Router)>,
    http3: Option<(http3::Http3Listener, axum::Router)>,
    router: axum::Router,
    log: Arc<JsonlLog>,
    /// Keeps the Bonjour advertisement registered for the server's lifetime.
    mdns: Option<Advertiser>,
}

/// The listening socket, and whether it is wrapped in TLS.
///
/// Two different servers drive them — `axum::serve` for plain HTTP,
/// `axum_server` for TLS — because a TLS handshake must happen inside the
/// connection's own task. Running it in the accept loop, which is what the
/// simpler `axum::serve` shape would require, lets one client that opens a
/// connection and then falls silent stall every other client's connect.
enum Transport {
    Plain(tokio::net::TcpListener),
    Tls {
        listener: std::net::TcpListener,
        config: axum_server::tls_rustls::RustlsConfig,
    },
}

impl Bound {
    /// Serve until `shutdown` resolves, then flush the event log.
    pub async fn serve<F>(self, shutdown: F) -> std::io::Result<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        // One shutdown future, two listeners: fan it out through a watch
        // channel so the redirect listener goes down with the main server
        // rather than outliving it and answering for a port nothing serves.
        let (tx, rx) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            shutdown.await;
            let _ = tx.send(true);
        });
        // `changed()` fails only when the sender is dropped, which happens on
        // the same path as the signal itself — either way, time to stop.
        let signalled = |mut rx: tokio::sync::watch::Receiver<bool>| async move {
            let _ = rx.changed().await;
        };

        // HTTP/3 runs beside the TCP listener, not instead of it: they serve
        // the same router on different transports, so a client can use either
        // and the operator can measure both.
        if let Some((http3, router)) = self.http3 {
            let stop = signalled(rx.clone());
            tokio::spawn(async move {
                http3.serve(router, stop).await;
                tracing::info!("the HTTP/3 listener stopped");
            });
        }

        if let Some((listener, router)) = self.redirect {
            let stop = signalled(rx.clone());
            tokio::spawn(async move {
                let handle = axum_server::Handle::new();
                let closing = handle.clone();
                tokio::spawn(async move {
                    stop.await;
                    closing.graceful_shutdown(Some(GRACE));
                });
                if let Err(e) = axum_server::from_tcp(listener)
                    .handle(handle)
                    .serve(router.into_make_service())
                    .await
                {
                    tracing::warn!(error = %e, "the HTTP redirect listener stopped");
                }
            });
        }

        // `into_make_service_with_connect_info` is what puts the peer address
        // into the request extensions. Without it the rate limiter would key
        // every request on the same fallback address and throttle the whole
        // world together.
        let service = self
            .router
            .into_make_service_with_connect_info::<std::net::SocketAddr>();

        let result = match self.transport {
            Transport::Plain(listener) => {
                axum::serve(listener, service)
                    .with_graceful_shutdown(signalled(rx))
                    .await
            }
            Transport::Tls { listener, config } => {
                let handle = axum_server::Handle::new();
                let closing = handle.clone();
                let stop = signalled(rx);
                tokio::spawn(async move {
                    stop.await;
                    closing.graceful_shutdown(Some(GRACE));
                });
                axum_server::from_tcp_rustls(listener, config)
                    .handle(handle)
                    .serve(service)
                    .await
            }
        };

        // The log buffers between fsync batches; a clean exit must not be the
        // reason a delivered event is missing from it.
        if let Err(e) = self.log.flush() {
            tracing::error!(error = %e, "could not flush the event log on shutdown");
        }
        result
    }
}

/// How long an in-flight request may take to finish once shutdown starts.
/// Generous enough for a normal request, short enough that a held-open
/// WebSocket cannot keep the process alive indefinitely.
const GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Advertise the server over Bonjour/mDNS as `_pocketskynet._tcp` so phones
/// on the same network can discover it without typing an address.  Best
/// effort: a machine with no multicast route still serves fine, so every
/// failure is a warning, never a startup error.  (VPN-only reachability —
/// e.g. Tailscale — does not carry multicast; those clients still enter the
/// address by hand.)
/// The live advertisement.  On macOS the system's own mDNSResponder holds
/// port 5353 exclusively enough that a userspace responder's announcements
/// are not reliably delivered even on the local network — so there the
/// registration goes through `dns-sd -R` (mDNSResponder's front door), and
/// the child process holds it.  Elsewhere the in-process responder serves.
pub enum Advertiser {
    System(std::process::Child),
    InProcess(mdns_sd::ServiceDaemon),
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        if let Advertiser::System(child) = self {
            let _ = child.kill();
        }
    }
}

fn advertise_mdns(addr: std::net::SocketAddr, scheme: Scheme) -> Option<Advertiser> {
    let scheme_prop = match scheme {
        Scheme::Http => "scheme=http",
        Scheme::Https => "scheme=https",
    };
    let instance = format!("PocketSkynet on {}", hostname());

    // Multicast does not cross a mesh VPN, so a phone can only discover this
    // server while it shares the LAN.  Publishing the VPN URL in the TXT
    // record means that one discovery is enough: the client keeps the
    // address that still works from anywhere.
    let endpoints = connect_urls(addr, scheme);
    let vpn_prop = endpoints
        .iter()
        .find(|e| e.reach == Reach::Vpn)
        .map(|e| format!("vpn={}", e.url));
    let lan_prop = endpoints
        .iter()
        .find(|e| e.reach == Reach::Lan)
        .map(|e| format!("lan={}", e.url));
    if cfg!(target_os = "macos") {
        let mut args = vec![
            "-R".to_string(), instance.clone(), "_pocketskynet._tcp".to_string(),
            ".".to_string(), addr.port().to_string(), scheme_prop.to_string(),
        ];
        args.extend(vpn_prop.clone());
        args.extend(lan_prop.clone());
        match std::process::Command::new("dns-sd")
            .args(&args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => {
                tracing::info!(instance, port = addr.port(), "advertising over Bonjour (mDNSResponder)");
                return Some(Advertiser::System(child));
            }
            Err(e) => tracing::warn!(error = %e, "dns-sd unavailable, falling back to in-process mDNS"),
        }
    }
    let mut props = vec![scheme_prop.to_string()];
    props.extend(vpn_prop);
    props.extend(lan_prop);
    advertise_in_process(addr, &props, &instance).map(Advertiser::InProcess)
}

fn advertise_in_process(
    addr: std::net::SocketAddr,
    props: &[String],
    instance: &str,
) -> Option<mdns_sd::ServiceDaemon> {
    let daemon = match mdns_sd::ServiceDaemon::new() {
        Ok(daemon) => daemon,
        Err(e) => {
            tracing::warn!(error = %e, "mDNS advertising unavailable");
            return None;
        }
    };
    let host = hostname();
    let pairs: Vec<(&str, &str)> = props
        .iter()
        .filter_map(|p| p.split_once('='))
        .collect();
    let info = match mdns_sd::ServiceInfo::new(
        "_pocketskynet._tcp.local.",
        instance,
        &format!("{host}.local."),
        (),
        addr.port(),
        &pairs[..],
    ) {
        Ok(info) => info.enable_addr_auto(),
        Err(e) => {
            tracing::warn!(error = %e, "mDNS service info rejected");
            return None;
        }
    };
    match daemon.register(info) {
        Ok(()) => {
            tracing::info!(instance, port = addr.port(), "advertising over Bonjour");
            Some(daemon)
        }
        Err(e) => {
            tracing::warn!(error = %e, "mDNS registration failed");
            None
        }
    }
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().replace(' ', "-"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "pocketskynet".into())
}

#[derive(Debug, thiserror::Error)]
pub enum BindError {
    #[error(transparent)]
    Startup(#[from] StartupError),
    #[error("binding {addr}: {source}")]
    Bind {
        addr: std::net::SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Tls(#[from] tls::TlsError),
    #[error(transparent)]
    Http3(#[from] http3::Http3Error),
}

/// Open the stores and bind the socket, without serving yet.
pub async fn bind(cfg: Config, secret: Secret) -> Result<Bound, BindError> {
    let addr = std::net::SocketAddr::new(cfg.host, cfg.port);
    let static_dir = cfg.static_dir.clone();
    let rate_limited = cfg.rate_limit;

    // Certificates are prepared before the socket exists: material the server
    // cannot produce should stop it starting, rather than surface later as
    // handshake failures against a port that is already accepting.
    //
    // `ca` is `Some` only for the generated certificate — a supplied one is
    // already trusted by whatever issued it, and publishing an unrelated CA
    // file would be worse than publishing nothing.
    // QUIC has no plaintext mode, so asking for HTTP/3 is asking for a
    // certificate — even when the TCP listener is plain HTTP. In that case the
    // material is generated for the QUIC port alone and the TCP listener stays
    // exactly as unencrypted as it was asked to be.
    let wants_http3 = cfg.http3_port.is_some();
    let (tls_config, ca, pem) = match &cfg.tls {
        config::Tls::Off if !wants_http3 => (None, None, None),
        config::Tls::Off => {
            let materials = tls::ensure(&cfg.tls_dir(), &tls::local_names())?;
            (
                None,
                Some(materials.ca),
                Some((materials.chain, materials.key)),
            )
        }
        config::Tls::SelfSigned => {
            let materials = tls::ensure(&cfg.tls_dir(), &tls::local_names())?;
            let config = tls::server_config(&materials.chain, &materials.key).await?;
            (
                Some(config),
                Some(materials.ca),
                Some((materials.chain, materials.key)),
            )
        }
        config::Tls::Supplied { cert, key } => (
            Some(tls::server_config(cert, key).await?),
            None,
            Some((cert.clone(), key.clone())),
        ),
    };

    let redirect_port = cfg.http_redirect_port;
    let http3_port = cfg.http3_port;
    let host = cfg.host;

    let state = AppState::build(cfg, secret)?;
    let log = state.log.clone();
    let base_router = routes::build(state);

    let (bound_addr, transport, scheme) = match tls_config {
        None => {
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .map_err(|source| BindError::Bind { addr, source })?;
            let bound = listener
                .local_addr()
                .map_err(|source| BindError::Bind { addr, source })?;
            (bound, Transport::Plain(listener), Scheme::Http)
        }
        Some(config) => {
            // A std listener, because that is what `axum_server` adopts; it
            // switches the socket to non-blocking itself.
            let listener = std::net::TcpListener::bind(addr)
                .map_err(|source| BindError::Bind { addr, source })?;
            let bound = listener
                .local_addr()
                .map_err(|source| BindError::Bind { addr, source })?;
            (bound, Transport::Tls { listener, config }, Scheme::Https)
        }
    };

    // The redirect listener is a convenience, not a requirement: if its port is
    // taken, say so and carry on rather than refusing to serve HTTPS at all.
    let redirect = redirect_port
        .filter(|_| scheme == Scheme::Https)
        .and_then(|port| {
            let redirect_addr = std::net::SocketAddr::new(host, port);
            match std::net::TcpListener::bind(redirect_addr) {
                Ok(listener) => Some((
                    listener,
                    tls::redirect_router(bound_addr.port(), ca.clone()),
                )),
                Err(e) => {
                    tracing::warn!(%redirect_addr, error = %e, "no HTTP redirect listener");
                    None
                }
            }
        });

    // HTTP/3, on its own UDP socket. Unlike the redirect listener this is a
    // hard failure: it was explicitly asked for, and silently serving only TCP
    // would look exactly like a working deployment right up until someone
    // measured it.
    let http3 = match (http3_port, &pem) {
        (Some(port), Some((chain, key))) => {
            let addr = std::net::SocketAddr::new(host, port);
            let config = http3::quic_config(chain, key)?;
            Some(http3::Http3Listener::bind(addr, config)?)
        }
        // `pem` is `None` only when nothing asked for a certificate, which is
        // the same condition as `http3_port` being `None`.
        _ => None,
    };
    let http3_port = http3.as_ref().map(|l| l.addr().port());

    // A client cannot discover HTTP/3 by trying it — QUIC on a closed UDP port
    // is silence, not a refusal — so the only way it learns the port is being
    // told over the connection it already has. URLSession and every browser
    // upgrade on this header alone.
    let router = match http3_port {
        Some(port) => {
            let value = http3::alt_svc_value(port);
            match axum::http::HeaderValue::from_str(&value) {
                Ok(value) => base_router.clone().layer(
                    tower_http::set_header::SetResponseHeaderLayer::overriding(
                        axum::http::header::ALT_SVC,
                        value,
                    ),
                ),
                Err(e) => {
                    tracing::warn!(%value, error = %e, "could not advertise HTTP/3 via Alt-Svc");
                    base_router.clone()
                }
            }
        }
        None => base_router.clone(),
    };

    tracing::info!(
        bound = %bound_addr,
        scheme = scheme.as_str(),
        ?static_dir,
        rate_limited,
        "pocketskynet listening"
    );
    if !static_dir.exists() {
        tracing::warn!(
            ?static_dir,
            "static directory is missing; the API works but the web client will 404"
        );
    }

    let mdns = advertise_mdns(bound_addr, scheme);

    Ok(Bound {
        addr: bound_addr,
        scheme,
        // Reported only when a listener actually came up, so the banner never
        // advertises a port that is not answering.
        redirect_port: redirect
            .as_ref()
            .and_then(|(l, _)| l.local_addr().ok())
            .map(|a| a.port()),
        http3_port,
        transport,
        redirect,
        // HTTP/3 serves the router *without* the Alt-Svc layer: advertising
        // an alternative service to a client already using it is noise.
        http3: http3.map(|listener| (listener, base_router)),
        router,
        log,
        mdns,
    })
}

#[cfg(test)]
mod banner_tests {
    use super::*;
    use std::net::SocketAddr;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn the_scheme_follows_the_transport() {
        let urls = connect_urls(addr("127.0.0.1:9099"), Scheme::Https);
        assert_eq!(urls[0].url, "https://127.0.0.1:9099");
        assert_eq!(
            connect_urls(addr("127.0.0.1:9099"), Scheme::Http)[0].url,
            "http://127.0.0.1:9099"
        );
    }

    #[test]
    fn a_bound_interface_is_the_only_answer() {
        let urls = connect_urls(addr("192.168.1.24:9099"), Scheme::Https);
        assert_eq!(urls.len(), 2, "loopback plus the interface itself");
        assert_eq!(urls[1].url, "https://192.168.1.24:9099");
        assert_eq!(urls[1].reach, Reach::Lan);
    }

    #[test]
    fn the_vpn_range_is_told_apart_from_the_lan() {
        // 100.64.0.0/10 is the carrier-grade NAT range a mesh VPN hands out,
        // and it is the address that keeps working from somewhere else.
        assert_eq!(Reach::of("100.120.4.113".parse().unwrap()), Reach::Vpn);
        assert_eq!(Reach::of("100.63.255.255".parse().unwrap()), Reach::Lan);
        assert_eq!(Reach::of("100.128.0.1".parse().unwrap()), Reach::Lan);
        assert_eq!(Reach::of("192.168.1.24".parse().unwrap()), Reach::Lan);
        assert_eq!(Reach::of("127.0.0.1".parse().unwrap()), Reach::Local);
    }

    #[test]
    fn the_shared_url_prefers_the_one_that_works_from_anywhere() {
        let endpoints = vec![
            Endpoint {
                url: "https://127.0.0.1:9099".into(),
                reach: Reach::Local,
            },
            Endpoint {
                url: "https://172.30.1.58:9099".into(),
                reach: Reach::Lan,
            },
            Endpoint {
                url: "https://100.120.4.113:9099".into(),
                reach: Reach::Vpn,
            },
        ];
        assert_eq!(
            share_url(&endpoints).unwrap().url,
            "https://100.120.4.113:9099",
            "a LAN address is useless the moment the tablet leaves the building"
        );

        assert_eq!(
            share_url(&endpoints[..1]),
            None,
            "loopback is not something to hand to anyone"
        );
    }

    #[test]
    fn the_share_base_is_never_a_loopback_or_a_port_zero_lie() {
        let cfg = |host: &str, port: u16| crate::config::Config {
            host: host.parse().unwrap(),
            port,
            data_dir: std::path::PathBuf::from("/tmp/ps-share-base"),
            static_dir: std::path::PathBuf::from("/tmp/ps-share-base/static"),
            jwt_ttl_hours: 24,
            cors_origin: vec![],
            sse_token_query: false,
            rate_limit: false,
            verify_payments: false,
            trust_proxy: 0,
            tls: crate::config::Tls::Off,
            http_redirect_port: None,
            http3_port: None,
        };

        assert_eq!(
            share_base(&cfg("127.0.0.1", 9099)),
            None,
            "loopback-bound means nothing to hand out"
        );
        assert_eq!(
            share_base(&cfg("0.0.0.0", 0)),
            None,
            "the ephemeral-port fallback cannot know its own port"
        );

        // A specific non-loopback bind names exactly itself.
        assert_eq!(
            share_base(&cfg("100.120.4.113", 9099)).as_deref(),
            Some("http://100.120.4.113:9099")
        );
        assert_eq!(
            share_base(&crate::config::Config {
                tls: crate::config::Tls::SelfSigned,
                ..cfg("192.168.1.24", 9099)
            })
            .as_deref(),
            Some("https://192.168.1.24:9099"),
            "the scheme must match how the server actually listens"
        );
    }

    #[test]
    fn the_banner_carries_a_url_to_copy_and_a_code_to_scan() {
        let banner = connect_banner(addr("192.168.1.24:9099"), Scheme::Https, Some(9100));

        assert!(banner.contains("https://192.168.1.24:9099"));
        assert!(
            banner.contains("http://192.168.1.24:9100/ca.crt"),
            "the way out of the certificate warning must be in the banner:\n{banner}"
        );
        assert!(
            banner.lines().filter(|l| l.contains('█')).count() > 10,
            "a QR code should have been rendered:\n{banner}"
        );
    }

    #[test]
    fn the_storage_banner_names_every_directory_a_backup_must_include() {
        let cfg = crate::config::Config {
            host: "127.0.0.1".parse().unwrap(),
            port: 0,
            data_dir: std::path::PathBuf::from("/tmp/ps-banner-test"),
            static_dir: std::path::PathBuf::from("/tmp/ps-banner-test/static"),
            jwt_ttl_hours: 24,
            cors_origin: vec![],
            sse_token_query: false,
            rate_limit: false,
            verify_payments: false,
            trust_proxy: 0,
            tls: crate::config::Tls::Off,
            http_redirect_port: None,
            http3_port: None,
        };
        let banner = storage_banner(&cfg);

        // The four places bytes actually live. `files` is the one that prompted
        // this: attachments are on disk, not in the database, and an operator
        // who backs up only the .db loses all of them silently.
        for expected in ["files", "images", "database", "events", "data"] {
            assert!(banner.contains(expected), "missing {expected}:\n{banner}");
        }
        assert!(
            banner.contains("/tmp/ps-banner-test/files"),
            "the files path must be spelled out, not implied:\n{banner}"
        );
        assert!(
            banner.contains("pocketskynet.db"),
            "the database file, by name:\n{banner}"
        );
        // The warning is the point of the block, not decoration.
        assert!(
            banner.contains("SHA-256") && banner.contains("not just the .db"),
            "the backup caveat must be stated:\n{banner}"
        );

        // Every path absolute, including the ones that do not exist yet — none
        // of these directories is created until something is written to it, and
        // one relative row among four absolute ones reads as a different kind
        // of location rather than the same directory.
        for line in banner.lines().filter(|l| l.contains("/tmp/ps-banner-test")) {
            let path = line.split_whitespace().last().unwrap();
            assert!(
                std::path::Path::new(path).is_absolute(),
                "relative path in the banner: {line:?}"
            );
        }
    }

    #[test]
    fn a_loopback_banner_offers_nothing_to_share() {
        let banner = connect_banner(addr("127.0.0.1:9099"), Scheme::Http, None);
        assert!(banner.contains("Reachable from this machine only"));
        assert!(
            !banner.contains("scan"),
            "there is nobody to scan it:\n{banner}"
        );
    }

    #[test]
    fn a_plain_http_banner_says_nothing_about_certificates() {
        let banner = connect_banner(addr("192.168.1.24:9099"), Scheme::Http, None);
        assert!(banner.contains("http://192.168.1.24:9099"));
        assert!(!banner.contains("certificate"));
    }
}

#[cfg(test)]
mod test_support {
    //! Shared fixtures. Kept in the crate root so route tests, hub tests and
    //! storage tests all build their world the same way — a divergence there
    //! would make failures hard to compare.

    use std::sync::Arc;
    use std::time::Instant;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::Router;
    use pocketskynet_core::WalletAddress;
    use tower::ServiceExt;

    use crate::auth::JwtKeys;
    use crate::config::Config;
    use crate::db::Db;
    use crate::hub::Hub;
    use crate::jsonl::JsonlLog;
    use crate::ratelimit::RateLimiter;
    use crate::routes::realtime::TicketStore;
    use crate::AppState;

    pub fn tempdir(tag: &str) -> std::path::PathBuf {
        let mut buf = [0u8; 8];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut buf);
        let dir = std::env::temp_dir().join(format!("ps-{tag}-{}", hex::encode(buf)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A state with rate limiting **off**: the suite drives many wallets from
    /// one address and would otherwise trip the per-IP budget.
    pub fn state(tag: &str) -> AppState {
        // The paid routes refuse every request without an operator wallet, and
        // this is environment-only configuration — a developer's shell (or a
        // bare CI runner) must not decide whether those tests pass. The
        // missing-wallet path is covered by the integration suite, whose
        // harness scrubs the child server's environment.
        std::env::set_var(
            "VITE_FRUITNATION_WALLET",
            "0x2222222222222222222222222222222222222222",
        );
        let dir = tempdir(tag);
        let cfg = Config {
            host: "127.0.0.1".parse().unwrap(),
            port: 0,
            data_dir: dir.clone(),
            static_dir: dir.join("static"),
            jwt_ttl_hours: 24,
            cors_origin: vec![],
            sse_token_query: true,
            rate_limit: false,
            // Tests run offline; the payment path's RPC half is exercised by
            // its own unit tests against a mock endpoint.
            verify_payments: false,
            trust_proxy: 0,
            tls: crate::config::Tls::Off,
            http_redirect_port: None,
            http3_port: None,
        };
        let db = Db::open_temp().unwrap();
        let log = Arc::new(JsonlLog::open(dir.join("events")).unwrap());

        AppState {
            hub: Hub::new(log.clone(), db.clone()),
            db,
            log,
            jwt: Arc::new(JwtKeys::new(b"test-secret-at-least-32-bytes-long!!", 24)),
            tickets: Arc::new(TicketStore::new()),
            cfg: Arc::new(cfg),
            limiter: Arc::new(RateLimiter::new(false)),
            started: Instant::now(),
        }
    }

    pub fn wallet(tag: &str) -> WalletAddress {
        // A distinct, valid address per tag without hand-writing 40 hex chars.
        let mut hex = String::new();
        for byte in tag.bytes().cycle().take(20) {
            hex.push_str(&format!("{byte:02x}"));
        }
        WalletAddress::new(&format!("0x{hex}")).unwrap()
    }

    /// Register a user directly, skipping the challenge/signature dance that
    /// the auth tests exercise separately.
    pub fn register(state: &AppState, wallet: &WalletAddress, username: &str) -> String {
        let address = wallet.as_str().to_owned();
        let username = username.to_owned();
        state
            .db
            .call_blocking(move |conn| {
                crate::db::users::upsert_user(conn, &address, &username, None, None)?;
                Ok(())
            })
            .unwrap();
        state.jwt.issue(wallet).unwrap()
    }

    pub struct Response {
        pub status: StatusCode,
        pub headers: axum::http::HeaderMap,
        pub body: serde_json::Value,
    }

    impl Response {
        pub fn json(&self) -> &serde_json::Value {
            &self.body
        }

        pub fn header(&self, name: &str) -> Option<String> {
            self.headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        }
    }

    /// Send a request and inspect only the head.
    ///
    /// Required for SSE: the response body is an open stream that will not
    /// end for thirty minutes, so reading it to completion would hang.
    pub async fn send_head(
        router: &Router,
        method: &str,
        uri: &str,
        token: Option<&str>,
    ) -> (StatusCode, axum::http::HeaderMap) {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let response = router
            .clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        (response.status(), response.headers().clone())
    }

    /// Send raw bytes with an explicit content type — the shape of the
    /// attachment, image, and site-publishing uploads.
    pub async fn send_raw(
        router: &Router,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Vec<u8>,
        content_type: &str,
    ) -> Response {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", content_type);
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let response = router
            .clone()
            .oneshot(builder.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);

        Response {
            status,
            headers,
            body,
        }
    }

    /// Send a request through the full router, including every layer.
    pub async fn send(
        router: &Router,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> Response {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let request = match body {
            Some(value) => builder
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&value).unwrap()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };

        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);

        Response {
            status,
            headers,
            body,
        }
    }
}
