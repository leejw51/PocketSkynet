//! Runtime configuration, assembled from CLI flags and the environment.

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use clap::Parser;

/// PocketSkynet messenger server.
#[derive(Debug, Clone, Parser)]
#[command(name = "pocketskynet", version, about)]
pub struct Cli {
    /// Address to bind. 0.0.0.0 makes the server reachable from other machines.
    #[arg(long, env = "PS_HOST", default_value = "0.0.0.0")]
    pub host: IpAddr,

    #[arg(long, env = "PS_PORT", default_value_t = 9099)]
    pub port: u16,

    /// Directory holding the SQLite database and the JSONL event log.
    /// Defaults to `~/.pocketskynet`; `POCKETSKYNET_PATH` overrides the
    /// default, and this flag overrides both.
    #[arg(long, env = "POCKETSKYNET_PATH", default_value_os_t = default_data_dir())]
    pub data_dir: PathBuf,

    /// Directory holding the built web client (trunk's `dist`).
    #[arg(long, env = "PS_STATIC_DIR", default_value = "web/dist")]
    pub static_dir: PathBuf,

    /// Serve HTTPS with a self-signed certificate, generated on first use.
    ///
    /// Needed for phones and tablets: a browser only grants a page the secure
    /// platform (`crypto.subtle`, clipboard, notifications) over HTTPS, and
    /// marks a plain-HTTP LAN address as insecure.
    #[arg(long, env = "PS_TLS", default_value_t = false)]
    pub tls: bool,

    /// Use this certificate chain instead of the generated one. Requires
    /// `--tls-key`, and implies `--tls`.
    #[arg(long, env = "PS_TLS_CERT")]
    pub tls_cert: Option<PathBuf>,

    /// Private key for `--tls-cert`.
    #[arg(long, env = "PS_TLS_KEY")]
    pub tls_key: Option<PathBuf>,

    /// Plain-HTTP port that redirects to HTTPS and serves the CA certificate.
    /// `0` disables it. Only used when TLS is on; defaults to the HTTPS port
    /// plus one.
    ///
    /// It exists because a browser given a bare `host:port` tries HTTP first —
    /// without this, the most likely thing anyone types fails obscurely.
    #[arg(long, env = "PS_HTTP_REDIRECT_PORT")]
    pub http_redirect_port: Option<u16>,

    /// Also serve HTTP/3 over QUIC, on its own UDP port.
    ///
    /// The two listeners run together and speak the same API, so a client can
    /// pick either — HTTP/3 wins on lossy and mobile networks (no transport
    /// head-of-line blocking, one round trip to first byte, and a connection
    /// that survives Wi-Fi→cellular), and loses on a clean local link, where
    /// TCP's kernel and NIC offload is hard to beat from userspace.
    ///
    /// QUIC mandates TLS, so this implies certificate material: with `--tls`
    /// the listeners share it, and without it a self-signed certificate is
    /// generated for the QUIC port alone.
    #[arg(long, env = "PS_HTTP3", default_value_t = false)]
    pub http3: bool,

    /// UDP port for the HTTP/3 listener. `0` disables it. Defaults to the main
    /// port plus two, leaving the plus-one slot to the HTTPS redirect.
    ///
    /// Setting it equal to the HTTPS port is valid and is the conventional
    /// deployment — TCP and UDP port numbers are separate namespaces, and a
    /// browser expects `Alt-Svc` to point at the same number it already knows.
    #[arg(long, env = "PS_HTTP3_PORT")]
    pub http3_port: Option<u16>,

    /// Secret used to sign JWTs. Generated and persisted on first run if unset.
    #[arg(long, env = "PS_JWT_SECRET")]
    pub jwt_secret: Option<String>,

    /// How long an issued JWT stays valid.
    #[arg(long, env = "PS_JWT_TTL_HOURS", default_value_t = 24)]
    pub jwt_ttl_hours: i64,

    /// Extra origins allowed to call the API. The origin serving the client is
    /// same-origin and never needs listing.
    #[arg(long, env = "PS_CORS_ORIGIN", value_delimiter = ',')]
    pub cors_origin: Vec<String>,

    /// Accept the SSE `?token=<jwt>` fallback. Off by default: the token would
    /// land in access logs, proxy history, and `Referer` headers. The ticket
    /// flow (`POST /api/events/ticket`) is the supported path.
    #[arg(long, env = "PS_SSE_TOKEN_QUERY", default_value_t = false)]
    pub sse_token_query: bool,

    /// Disable rate limiting. Intended for tests, which would otherwise trip
    /// the per-IP limits while driving many wallets from one address.
    ///
    /// Refused when `PS_ENV=production` — see [`Cli::resolve`].
    #[arg(long, env = "PS_NO_RATE_LIMIT", default_value_t = false)]
    pub no_rate_limit: bool,

    /// Skip on-chain verification of paid features (Shout, web publishing)
    /// and accept any well-formed transaction hash.
    ///
    /// Intended for tests and offline development, where no RPC endpoint is
    /// reachable. Refused when `PS_ENV=production` — an unverified payment
    /// endpoint in production is a free-money bug, not a convenience.
    #[arg(long, env = "PS_NO_PAYMENT_VERIFY", default_value_t = false)]
    pub no_payment_verify: bool,

    /// Do not advertise this server over Bonjour/mDNS.
    ///
    /// Intended for tests, which start hundreds of short-lived servers: on
    /// macOS each advertisement is a spawned `dns-sd` child, and a test
    /// harness that kills its server outright (SIGKILL, so no `Drop` runs)
    /// orphans that child forever. A test server has no business announcing
    /// itself to the LAN anyway.
    #[arg(long, env = "PS_NO_MDNS", default_value_t = false)]
    pub no_mdns: bool,

    /// How many trusted reverse proxies sit in front of this server.
    ///
    /// `0` (the default) ignores `X-Forwarded-For` entirely and rate-limits on
    /// the socket's peer address. Set it only to the number of proxies you
    /// actually control: the value decides how far from the *right* of the
    /// header the client address is read, and overstating it lets a client
    /// forge the entry that gets trusted.
    #[arg(long, env = "PS_TRUST_PROXY", default_value_t = 0)]
    pub trust_proxy: u8,

    /// Log filter, e.g. `info`, `pocketskynet_server=debug,tower_http=debug`.
    #[arg(long, env = "PS_LOG", default_value = "info,tower_http=warn")]
    pub log: String,
}

/// The persistence root used when `--data-dir` is not given.
///
/// `POCKETSKYNET_PATH` from the environment wins, then `~/.pocketskynet`. The
/// home directory rather than the working directory, so where the server was
/// launched from does not decide where the deployment's identity lives. The
/// relative `.pocketskynet` fallback only fires when no home directory can be
/// determined at all.
///
/// Public because the desktop app resolves its data directory with exactly
/// this function — two resolvers would eventually disagree about where the
/// database is.
pub fn default_data_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("POCKETSKYNET_PATH") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }

    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");

    match home {
        Some(h) if !h.is_empty() => PathBuf::from(h).join(".pocketskynet"),
        _ => PathBuf::from(".pocketskynet"),
    }
}

/// How the socket is served.
#[derive(Debug, Clone)]
pub enum Tls {
    /// Plain HTTP.
    Off,
    /// HTTPS with a certificate generated into `<data-dir>/tls`.
    SelfSigned,
    /// HTTPS with a certificate the operator supplied.
    Supplied { cert: PathBuf, key: PathBuf },
}

impl Tls {
    pub fn is_on(&self) -> bool {
        !matches!(self, Tls::Off)
    }
}

/// Validated configuration shared by every handler.
#[derive(Debug, Clone)]
pub struct Config {
    pub host: IpAddr,
    pub port: u16,
    pub data_dir: PathBuf,
    pub static_dir: PathBuf,
    pub jwt_ttl_hours: i64,
    pub cors_origin: Vec<String>,
    pub sse_token_query: bool,
    pub rate_limit: bool,
    /// Whether paid-feature transaction hashes are verified against the
    /// configured chain's RPC before being honoured.
    pub verify_payments: bool,
    /// Whether the server announces itself over Bonjour/mDNS.
    pub advertise: bool,
    pub trust_proxy: u8,
    pub tls: Tls,
    /// Plain-HTTP port that redirects to HTTPS, when TLS is on.
    pub http_redirect_port: Option<u16>,
    /// UDP port for the HTTP/3 listener, when one was asked for.
    pub http3_port: Option<u16>,
}

impl Config {
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("pocketskynet.db")
    }

    /// Whether this deployment generates its own CA into `tls_dir`.
    ///
    /// True for self-signed HTTPS, and *also* for plain HTTP with HTTP/3 on:
    /// QUIC mandates TLS, so the QUIC listener gets generated material even
    /// when the TCP listener is unencrypted. Both cases have a CA worth
    /// serving from `/ca.crt`; a supplied certificate does not, because its
    /// issuer is already trusted and publishing an unrelated CA would be
    /// worse than publishing nothing.
    pub fn generates_own_ca(&self) -> bool {
        match self.tls {
            Tls::SelfSigned => true,
            Tls::Supplied { .. } => false,
            Tls::Off => self.http3_port.is_some(),
        }
    }

    /// Where generated certificate material lives. Under the data directory
    /// like everything else the deployment owns, so one backup captures it.
    pub fn tls_dir(&self) -> PathBuf {
        self.data_dir.join("tls")
    }

    pub fn events_dir(&self) -> PathBuf {
        self.data_dir.join("events")
    }

    /// Where AI-generated and user-uploaded media — images *and* generated
    /// video — are stored, one file per content hash. Lives under the data
    /// dir so a backup of `data/` captures everything the deployment owns,
    /// and so an AI generation outlives the provider's own expiring link.
    pub fn images_dir(&self) -> PathBuf {
        self.data_dir.join("images")
    }

    /// Where attachments are stored, one file per content hash. Separate from
    /// `images_dir` on purpose: images are a public capability-URL space served
    /// to bare `<img>` tags, attachments are room-scoped and authenticated, and
    /// a shared directory would make it a one-line mistake to serve one under
    /// the other's rules. Only the file *name* reaches SQLite.
    pub fn files_dir(&self) -> PathBuf {
        self.data_dir.join("files")
    }

    /// Where published sites live, one directory per site id
    /// (`sites/{id}/index.html` + assets). Under the data dir so a backup of
    /// `data/` captures the hosting a user paid for.
    pub fn sites_dir(&self) -> PathBuf {
        self.data_dir.join("sites")
    }

    /// Scratch space for in-flight resumable uploads, one `.part` file per
    /// session (`routes/uploads.rs`).
    ///
    /// Its own directory rather than a prefix inside `files_dir` for two
    /// reasons. A half-written file must never be reachable by the routes that
    /// serve finished ones — a `.part` in the attachment store is one careless
    /// `read_dir` away from being served — and finishing an upload is a
    /// `rename`, which is only atomic *within a filesystem*. Both directories
    /// hang off `data_dir`, so that holds by construction here and would stop
    /// holding the moment someone mounted one of them elsewhere. If you move
    /// this, move `files_dir` with it.
    ///
    /// Nothing here is worth backing up: an interrupted upload resumes from the
    /// client's copy, which is the only complete one until it finishes.
    pub fn uploads_dir(&self) -> PathBuf {
        self.data_dir.join("uploads")
    }

    fn secret_path(data_dir: &Path) -> PathBuf {
        data_dir.join("jwt.secret")
    }
}

/// Whether this process believes it is serving production traffic.
///
/// `PS_ENV` is the project's own switch; `NODE_ENV` is honoured too so a
/// deployment carried over from the reference server keeps its meaning.
pub fn is_production() -> bool {
    std::env::var("PS_ENV")
        .or_else(|_| std::env::var("NODE_ENV"))
        .map(|v| v == "production")
        .unwrap_or(false)
}

/// The signing secret, kept separate from [`Config`] so it is never caught up
/// in a `Debug` dump of the configuration.
pub struct Secret(pub Vec<u8>);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("creating data directory {0}: {1}")]
    DataDir(PathBuf, #[source] std::io::Error),
    #[error("reading JWT secret {0}: {1}")]
    ReadSecret(PathBuf, #[source] std::io::Error),
    #[error("writing JWT secret {0}: {1}")]
    WriteSecret(PathBuf, #[source] std::io::Error),
    #[error("PS_JWT_SECRET must be at least 32 characters")]
    WeakSecret,
    #[error(
        "--tls-cert and --tls-key go together: a certificate without its key, or \
         a key without its certificate, cannot serve HTTPS"
    )]
    HalfSuppliedTls,
    #[error(
        "refusing to start: rate limiting is disabled but PS_ENV=production. \
         --no-rate-limit exists for tests, which drive many wallets from one address; \
         in production it removes the only defence against credential stuffing."
    )]
    RateLimitDisabledInProduction,
    #[error(
        "refusing to start: payment verification is disabled but PS_ENV=production. \
         --no-payment-verify exists for tests and offline development; in production \
         it lets anyone claim a paid shout or a hosted site with a made-up hash."
    )]
    PaymentVerifyDisabledInProduction,
}

impl Cli {
    /// Split the parsed flags into shareable config plus the signing secret,
    /// creating the data directory and, if needed, a fresh secret.
    pub fn resolve(self) -> Result<(Config, Secret), ConfigError> {
        self.resolve_as(is_production())
    }

    /// [`Cli::resolve`] with the production decision supplied rather than read
    /// from the environment.
    ///
    /// Taking it as a parameter is what keeps the tests honest: the alternative
    /// is mutating a process-global env var, which races every other test in the
    /// binary and makes the suite order-dependent.
    pub fn resolve_as(self, production: bool) -> Result<(Config, Secret), ConfigError> {
        std::fs::create_dir_all(&self.data_dir)
            .map_err(|e| ConfigError::DataDir(self.data_dir.clone(), e))?;

        // Fail loudly rather than run unprotected. The reference server does the
        // same, and the failure mode it prevents — an unlimited login endpoint
        // on a public host — is not one you notice until it is exploited.
        if self.no_rate_limit && production {
            return Err(ConfigError::RateLimitDisabledInProduction);
        }
        if self.no_payment_verify && production {
            return Err(ConfigError::PaymentVerifyDisabledInProduction);
        }

        let secret = match self.jwt_secret {
            Some(s) => {
                // A short secret makes HS256 brute-forceable offline; refuse
                // rather than quietly accept a weak key.
                if s.len() < 32 {
                    return Err(ConfigError::WeakSecret);
                }
                Secret(s.into_bytes())
            }
            None => load_or_create_secret(&Config::secret_path(&self.data_dir))?,
        };

        // Supplying a certificate is itself a request for HTTPS; making the
        // operator also pass `--tls` would only ever produce a confusing
        // silent fallback to plain HTTP.
        let tls = match (self.tls, self.tls_cert, self.tls_key) {
            (_, Some(cert), Some(key)) => Tls::Supplied { cert, key },
            (_, Some(_), None) | (_, None, Some(_)) => return Err(ConfigError::HalfSuppliedTls),
            (true, None, None) => Tls::SelfSigned,
            (false, None, None) => Tls::Off,
        };

        // Default the redirect listener to one above the HTTPS port. `0` is an
        // explicit "don't", not an ephemeral port: a redirect target nobody can
        // predict would be useless.
        let http_redirect_port = match (tls.is_on(), self.http_redirect_port) {
            (false, _) => None,
            (true, Some(0)) => None,
            (true, Some(port)) => Some(port),
            (true, None) => self.port.checked_add(1),
        };

        // Same rule as the redirect port: `0` is an explicit "don't". The
        // default sits at plus-two so that `--tls --http3` on the default port
        // lays out predictably as 9099/9100/9101 rather than colliding.
        //
        // A port is only reserved when HTTP/3 was actually asked for; passing
        // `--http3-port` alone is a request for HTTP/3 too, because the only
        // reason to name that port is to serve on it.
        let http3_port = match (self.http3, self.http3_port) {
            (_, Some(0)) => None,
            (_, Some(port)) => Some(port),
            (true, None) => self.port.checked_add(2),
            (false, None) => None,
        };

        let cfg = Config {
            host: self.host,
            port: self.port,
            data_dir: self.data_dir,
            static_dir: self.static_dir,
            jwt_ttl_hours: self.jwt_ttl_hours,
            cors_origin: self.cors_origin,
            sse_token_query: self.sse_token_query,
            rate_limit: !self.no_rate_limit,
            verify_payments: !self.no_payment_verify,
            advertise: !self.no_mdns,
            trust_proxy: self.trust_proxy,
            tls,
            http_redirect_port,
            http3_port,
        };
        Ok((cfg, secret))
    }
}

/// Reuse the persisted secret when there is one, so restarting the server does
/// not invalidate everybody's session. Generate one otherwise.
///
/// Public because the desktop app needs exactly this behaviour and must not
/// reimplement it: a second, subtly weaker generator for the key that signs
/// every session is not a mistake worth risking.
pub fn load_or_create_secret(path: &Path) -> Result<Secret, ConfigError> {
    match std::fs::read(path) {
        Ok(bytes) if bytes.len() >= 32 => return Ok(Secret(bytes)),
        Ok(_) => {
            tracing::warn!(?path, "stored JWT secret is too short; regenerating");
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(ConfigError::ReadSecret(path.to_path_buf(), e)),
    }

    let mut buf = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut buf);
    let hex = hex::encode(buf);

    std::fs::write(path, &hex).map_err(|e| ConfigError::WriteSecret(path.to_path_buf(), e))?;
    restrict_permissions(path);
    tracing::info!(?path, "generated a new JWT signing secret");

    Ok(Secret(hex.into_bytes()))
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    // Best effort: the secret is still usable if the chmod fails, and refusing
    // to start over file modes would be worse than logging it.
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!(?path, error = %e, "could not restrict JWT secret permissions");
    }
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_is_persisted_and_reused() {
        let dir = tempdir();
        let path = dir.join("jwt.secret");

        let first = load_or_create_secret(&path).unwrap();
        let second = load_or_create_secret(&path).unwrap();

        assert_eq!(first.0, second.0, "restart must not invalidate sessions");
        assert!(first.0.len() >= 32);
    }

    #[test]
    fn short_stored_secret_is_replaced() {
        let dir = tempdir();
        let path = dir.join("jwt.secret");
        std::fs::write(&path, b"tooshort").unwrap();

        let secret = load_or_create_secret(&path).unwrap();
        assert!(secret.0.len() >= 32);
    }

    fn cli_at(dir: &Path) -> Cli {
        Cli {
            host: "127.0.0.1".parse().unwrap(),
            port: 0,
            data_dir: dir.to_path_buf(),
            static_dir: dir.to_path_buf(),
            http3: false,
            http3_port: None,
            jwt_secret: None,
            jwt_ttl_hours: 1,
            cors_origin: vec![],
            sse_token_query: false,
            no_rate_limit: false,
            no_payment_verify: false,
            no_mdns: false,
            trust_proxy: 0,
            tls: false,
            tls_cert: None,
            tls_key: None,
            http_redirect_port: None,
            log: "off".into(),
        }
    }

    #[test]
    fn short_supplied_secret_is_rejected() {
        let dir = tempdir();
        let cli = Cli {
            jwt_secret: Some("short".into()),
            no_rate_limit: true,
            ..cli_at(&dir)
        };
        assert!(matches!(cli.resolve(), Err(ConfigError::WeakSecret)));
    }

    #[test]
    fn disabling_rate_limits_is_refused_in_production() {
        // `--no-rate-limit` exists so the test suite can drive many wallets from
        // one address. On a public host it removes the only thing standing
        // between an attacker and unlimited login attempts, and the failure is
        // invisible until it is exploited — so refuse to start rather than warn.
        let dir = tempdir();

        let refused = Cli {
            no_rate_limit: true,
            ..cli_at(&dir)
        }
        .resolve_as(true);
        assert!(
            matches!(refused, Err(ConfigError::RateLimitDisabledInProduction)),
            "production + --no-rate-limit must refuse to start"
        );

        // Outside production the same configuration is exactly what tests use.
        let allowed = Cli {
            no_rate_limit: true,
            ..cli_at(&dir)
        }
        .resolve_as(false);
        assert!(allowed.is_ok(), "development must still allow it");

        // And production alone is fine, so long as limiting stays on.
        let ok = cli_at(&dir).resolve_as(true);
        assert!(ok.is_ok());
        assert!(ok.unwrap().0.rate_limit);
    }

    #[test]
    fn disabling_payment_verification_is_refused_in_production() {
        // Same shape as the rate-limit refusal, same reason: an unverified
        // payment endpoint on a public host is a silently exploitable hole.
        let dir = tempdir();

        let refused = Cli {
            no_payment_verify: true,
            ..cli_at(&dir)
        }
        .resolve_as(true);
        assert!(matches!(
            refused,
            Err(ConfigError::PaymentVerifyDisabledInProduction)
        ));

        let allowed = Cli {
            no_payment_verify: true,
            ..cli_at(&dir)
        }
        .resolve_as(false);
        assert!(allowed.is_ok(), "tests and offline development need it");
        assert!(!allowed.unwrap().0.verify_payments);

        let default = cli_at(&dir).resolve_as(false).unwrap();
        assert!(default.0.verify_payments, "verification is on by default");
    }

    #[test]
    fn trust_proxy_defaults_to_ignoring_forwarded_headers() {
        let dir = tempdir();
        let (cfg, _) = cli_at(&dir).resolve_as(false).unwrap();
        assert_eq!(
            cfg.trust_proxy, 0,
            "trusting X-Forwarded-For by default would let any client pick its own \
             rate-limit key"
        );
        assert!(cfg.rate_limit, "rate limiting is on unless asked otherwise");
    }

    #[test]
    fn tls_is_off_unless_asked_for() {
        let dir = tempdir();
        let (cfg, _) = cli_at(&dir).resolve_as(false).unwrap();
        assert!(!cfg.tls.is_on());
        assert_eq!(
            cfg.http_redirect_port, None,
            "a plain-HTTP server has nothing to redirect to"
        );
    }

    #[test]
    fn tls_defaults_the_redirect_port_to_one_above() {
        let dir = tempdir();
        let (cfg, _) = Cli {
            port: 9099,
            tls: true,
            ..cli_at(&dir)
        }
        .resolve_as(false)
        .unwrap();

        assert!(matches!(cfg.tls, Tls::SelfSigned));
        assert_eq!(cfg.http_redirect_port, Some(9100));
    }

    #[test]
    fn a_supplied_certificate_turns_tls_on_by_itself() {
        // Passing `--tls-cert` and then serving plain HTTP because `--tls` was
        // missing is the kind of silent downgrade nobody notices until the
        // wrong thing is on the wire.
        let dir = tempdir();
        let (cfg, _) = Cli {
            tls_cert: Some(dir.join("cert.pem")),
            tls_key: Some(dir.join("key.pem")),
            ..cli_at(&dir)
        }
        .resolve_as(false)
        .unwrap();
        assert!(matches!(cfg.tls, Tls::Supplied { .. }));
    }

    #[test]
    fn half_a_certificate_is_refused() {
        let dir = tempdir();
        let refused = Cli {
            tls_cert: Some(dir.join("cert.pem")),
            ..cli_at(&dir)
        }
        .resolve_as(false);
        assert!(matches!(refused, Err(ConfigError::HalfSuppliedTls)));
    }

    #[test]
    fn the_redirect_listener_can_be_turned_off() {
        let dir = tempdir();
        let (cfg, _) = Cli {
            tls: true,
            http_redirect_port: Some(0),
            ..cli_at(&dir)
        }
        .resolve_as(false)
        .unwrap();
        assert_eq!(cfg.http_redirect_port, None, "0 means off, not ephemeral");
    }

    // --- HTTP/3 -----------------------------------------------------------

    #[test]
    fn http3_is_off_unless_asked_for() {
        // QUIC opens a UDP socket and generates a certificate; neither should
        // happen because someone ran the server with no flags.
        let dir = tempdir();
        let (cfg, _) = cli_at(&dir).resolve_as(false).unwrap();
        assert_eq!(cfg.http3_port, None);
        assert!(!cfg.generates_own_ca(), "and no certificate is written");
    }

    #[test]
    fn http3_defaults_to_two_above_the_main_port() {
        // Plus-two, not plus-one: the HTTPS redirect already owns plus-one, so
        // `--tls --http3` on the default port lays out as 9099/9100/9101
        // rather than having two features collide on the same number.
        let dir = tempdir();
        let (cfg, _) = Cli {
            port: 9099,
            http3: true,
            ..cli_at(&dir)
        }
        .resolve_as(false)
        .unwrap();
        assert_eq!(cfg.http3_port, Some(9101));

        let (with_tls, _) = Cli {
            port: 9099,
            tls: true,
            http3: true,
            ..cli_at(&dir)
        }
        .resolve_as(false)
        .unwrap();
        assert_eq!(with_tls.http_redirect_port, Some(9100));
        assert_eq!(with_tls.http3_port, Some(9101), "no collision");
    }

    #[test]
    fn naming_the_http3_port_is_itself_a_request_for_http3() {
        // The only reason to pass `--http3-port` is to serve on it. Requiring
        // `--http3` as well would make the flag silently do nothing, which is
        // the worst possible outcome for a networking option.
        let dir = tempdir();
        let (cfg, _) = Cli {
            http3_port: Some(4433),
            ..cli_at(&dir)
        }
        .resolve_as(false)
        .unwrap();
        assert_eq!(cfg.http3_port, Some(4433));
    }

    #[test]
    fn the_http3_listener_can_be_turned_off_explicitly() {
        let dir = tempdir();
        let (cfg, _) = Cli {
            http3: true,
            http3_port: Some(0),
            ..cli_at(&dir)
        }
        .resolve_as(false)
        .unwrap();
        assert_eq!(cfg.http3_port, None, "0 means off, not ephemeral");
    }

    #[test]
    fn http3_may_share_the_https_port_number() {
        // TCP and UDP port numbers are separate namespaces, and pointing
        // Alt-Svc at the number the client already knows is the conventional
        // deployment — so this must be allowed, not "corrected".
        let dir = tempdir();
        let (cfg, _) = Cli {
            port: 9099,
            tls: true,
            http3: true,
            http3_port: Some(9099),
            ..cli_at(&dir)
        }
        .resolve_as(false)
        .unwrap();
        assert_eq!(cfg.port, 9099);
        assert_eq!(cfg.http3_port, Some(9099));
    }

    #[test]
    fn plain_http_with_http3_still_generates_a_ca() {
        // QUIC has no plaintext mode, so the UDP listener needs certificate
        // material even when the TCP one is unencrypted — and that CA has to
        // be downloadable, or nothing can ever trust the QUIC port.
        let dir = tempdir();
        let (cfg, _) = Cli {
            http3: true,
            ..cli_at(&dir)
        }
        .resolve_as(false)
        .unwrap();
        assert!(!cfg.tls.is_on(), "the TCP listener stays plain HTTP");
        assert!(cfg.generates_own_ca());
    }

    #[test]
    fn a_supplied_certificate_publishes_no_ca_even_with_http3() {
        // The operator's chain is already trusted by whatever issued it;
        // publishing an unrelated CA would be worse than publishing nothing.
        let dir = tempdir();
        let (cfg, _) = Cli {
            tls_cert: Some(dir.join("cert.pem")),
            tls_key: Some(dir.join("key.pem")),
            http3: true,
            ..cli_at(&dir)
        }
        .resolve_as(false)
        .unwrap();
        assert!(cfg.http3_port.is_some());
        assert!(!cfg.generates_own_ca());
    }

    #[test]
    fn the_default_data_dir_is_under_home() {
        // When the operator has POCKETSKYNET_PATH exported the override is the
        // answer, and asserting on it would mean mutating process-global env —
        // the order-dependence resolve_as() exists to avoid.
        if std::env::var_os("POCKETSKYNET_PATH").is_some() {
            return;
        }
        let dir = default_data_dir();
        assert!(
            dir.ends_with(".pocketskynet"),
            "expected ~/.pocketskynet, got {dir:?}"
        );
    }

    #[test]
    fn debug_does_not_leak_the_secret() {
        let rendered = format!("{:?}", Secret(b"super-secret-value".to_vec()));
        assert!(!rendered.contains("super-secret"));
    }

    fn tempdir() -> PathBuf {
        let mut buf = [0u8; 8];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut buf);
        let dir = std::env::temp_dir().join(format!("ps-cfg-{}", hex::encode(buf)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
