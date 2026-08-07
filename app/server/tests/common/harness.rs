//! Boots a real `pocketskynet` process per test.
//!
//! Every test gets its own process, its own ephemeral port and its own temp
//! data directory, so the whole suite runs in parallel with no shared SQLite
//! file, no shared JSONL log and no cross-test event bleed. Teardown happens in
//! `Drop`, which runs even when a test panics — a leaked server would hold a
//! port and a temp directory for the rest of the run.
//!
//! Boots are serialised (see [`BOOT_LOCK`]) because "ask the OS for a free
//! port, close it, then hand the number to a child" has a window in which two
//! tests can be handed the same port. Only one child wins the bind; the loser
//! exits, and — this is the part that bites — its `/api/health` probe is
//! happily answered by the *winner*, so the losing test proceeds to drive
//! somebody else's server. It fails minutes later, in a different file, on an
//! assertion that has nothing to do with the cause.

use std::fs::File;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Handed to the server with `--jwt-secret` so tests can mint and tamper with
/// tokens themselves (expiry, wrong signature, `alg:none`).
pub const JWT_SECRET: &str = "pocketskynet-integration-test-secret-0123456789abcdef";

/// How long to wait for `/api/health` before declaring the boot failed.
const BOOT_TIMEOUT: Duration = Duration::from_secs(30);

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Held from picking a port until that port answers `/api/health`, so no two
/// children of this process can be aimed at the same one. A boot takes tens of
/// milliseconds, so the tests still run wide open after the handoff.
static BOOT_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

pub struct TestServer {
    child: Child,
    pub port: u16,
    /// The plain-HTTP port beside an HTTPS server, when there is one.
    pub redirect_port: Option<u16>,
    /// The UDP port serving HTTP/3, when this server was started with one.
    pub http3_port: Option<u16>,
    pub data_dir: PathBuf,
    pub base_url: String,
}

impl TestServer {
    /// Start a server and block until `/api/health` answers 200.
    pub async fn start() -> Self {
        Self::start_with_args(&[]).await
    }

    /// Start a server with extra CLI flags (e.g. `--sse-token-query`).
    pub async fn start_with_args(extra: &[&str]) -> Self {
        Self::start_with(extra, &[]).await
    }

    /// Start a server with extra environment, for the settings that are not
    /// CLI flags (`VITE_FRUITNATION_WALLET` and the other chain metadata).
    pub async fn start_with_env(env: &[(&str, &str)]) -> Self {
        Self::start_with(&[], env).await
    }

    /// Start an HTTPS server with a freshly generated certificate, plus the
    /// plain-HTTP redirect listener beside it.
    ///
    /// The redirect port is allocated here rather than left to default to
    /// `port + 1`: the suite runs wide open, and `port + 1` belongs to whoever
    /// `free_port` handed it to.
    pub async fn start_tls() -> Self {
        let mut last_err = String::new();
        for _ in 0..5 {
            let redirect = free_port();
            let redirect_s = redirect.to_string();
            match Self::try_start(&["--tls", "--http-redirect-port", &redirect_s], &[]).await {
                Ok(mut s) => {
                    s.redirect_port = Some(redirect);
                    return s;
                }
                Err(e) => last_err = e,
            }
        }
        panic!("could not start pocketskynet over TLS after 5 attempts: {last_err}");
    }

    /// Start a server with both listeners live: plain HTTP on the TCP port,
    /// and HTTP/3 on a UDP port of its own.
    ///
    /// Deliberately *without* `--tls`, because that is the configuration worth
    /// pinning: QUIC mandates TLS, so the server has to generate certificate
    /// material for the UDP listener even though the TCP one is unencrypted.
    /// The two ports then answer the same API over different transports and
    /// different trust models, which is exactly the thing that could silently
    /// break.
    pub async fn start_http3() -> Self {
        Self::start_http3_with(&[]).await
    }

    /// [`start_http3`](Self::start_http3), plus extra flags — `--tls` for the
    /// both-encrypted case.
    pub async fn start_http3_with(extra: &[&str]) -> Self {
        let mut last_err = String::new();
        for _ in 0..5 {
            let quic = free_udp_port();
            let quic_s = quic.to_string();
            let mut args = vec!["--http3", "--http3-port", quic_s.as_str()];
            args.extend_from_slice(extra);
            match Self::try_start(&args, &[]).await {
                Ok(mut s) => {
                    s.http3_port = Some(quic);
                    // The CA is written during startup but `/api/health`
                    // answering does not prove the file is flushed, and a QUIC
                    // client has nothing to trust without it.
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !s.ca_path().is_file() && Instant::now() < deadline {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    return s;
                }
                Err(e) => last_err = e,
            }
        }
        panic!("could not start pocketskynet with HTTP/3 after 5 attempts: {last_err}");
    }

    /// The QUIC endpoint's address. Panics unless the server was started with
    /// [`start_http3`](Self::start_http3).
    pub fn http3_addr(&self) -> std::net::SocketAddr {
        let port = self.http3_port.expect("this server has no HTTP/3 listener");
        std::net::SocketAddr::from(([127, 0, 0, 1], port))
    }

    /// The CA this server generated, whichever listener needed it.
    ///
    /// [`ca_pem`](Self::ca_pem) is scheme-driven and returns `None` for a
    /// plain-HTTP server; with HTTP/3 on, a plain-HTTP server has a CA all the
    /// same, because QUIC could not have started without one.
    pub fn generated_ca(&self) -> Vec<u8> {
        std::fs::read(self.ca_path()).expect("the server must have written its CA")
    }

    pub async fn start_with(extra: &[&str], env: &[(&str, &str)]) -> Self {
        // Reserving the port and immediately releasing it leaves a tiny race
        // window, so retry the whole boot rather than failing the test on a
        // collision that has nothing to do with the code under test.
        let mut last_err = String::new();
        for _ in 0..5 {
            match Self::try_start(extra, env).await {
                Ok(s) => return s,
                Err(e) => last_err = e,
            }
        }
        panic!("could not start pocketskynet after 5 attempts: {last_err}");
    }

    async fn try_start(extra: &[&str], env: &[(&str, &str)]) -> Result<Self, String> {
        let _boot = BOOT_LOCK.lock().await;
        let port = free_port();
        let data_dir = unique_dir();
        std::fs::create_dir_all(&data_dir).map_err(|e| format!("mkdir {data_dir:?}: {e}"))?;
        // An empty directory of its own, so a traversal test can never reach
        // the database or the JWT secret through the static file service.
        let static_dir = data_dir.join("static");
        std::fs::create_dir_all(&static_dir).map_err(|e| format!("mkdir {static_dir:?}: {e}"))?;

        // Piped stdio would deadlock the child once the pipe buffer filled, so
        // logs go to a file we can quote back in a failure message instead.
        let log_path = data_dir.join("server.log");
        let log = File::create(&log_path).map_err(|e| format!("create log: {e}"))?;
        let log_err = log.try_clone().map_err(|e| format!("clone log: {e}"))?;

        let port_s = port.to_string();
        let dir_s = data_dir.to_string_lossy().to_string();
        let static_s = static_dir.to_string_lossy().to_string();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pocketskynet"));
        cmd.args([
            "--host",
            "127.0.0.1",
            "--port",
            &port_s,
            "--data-dir",
            &dir_s,
            "--static-dir",
            &static_s,
            "--jwt-secret",
            JWT_SECRET,
            "--no-rate-limit",
            // The suite runs offline; the payment verifier's RPC half has its
            // own unit tests against a mock chain (`payment.rs`).
            "--no-payment-verify",
            // Advertising would spawn a `dns-sd` child per server on macOS,
            // and the SIGKILL in `Drop` below would orphan every one of them.
            "--no-mdns",
            "--log",
            "warn",
        ])
        .args(extra)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));

        // Inherited PS_* variables (and the data-root override) would silently
        // override the flags above and make a developer's shell decide what the
        // suite tests.
        for key in [
            "PS_HOST",
            "PS_PORT",
            "POCKETSKYNET_PATH",
            "PS_STATIC_DIR",
            "PS_JWT_SECRET",
            "PS_JWT_TTL_HOURS",
            "PS_CORS_ORIGIN",
            "PS_SSE_TOKEN_QUERY",
            "PS_NO_RATE_LIMIT",
            "PS_NO_PAYMENT_VERIFY",
            "PS_NO_MDNS",
            "PS_SHOUT_PRICE_CRO",
            "PS_PUBLISH_PRICE_CRO",
            "PS_TLS",
            "PS_TLS_CERT",
            "PS_TLS_KEY",
            "PS_HTTP_REDIRECT_PORT",
            "PS_HTTP3",
            "PS_HTTP3_PORT",
            "PS_LOG",
        ] {
            cmd.env_remove(key);
        }
        // The chain metadata is environment-only, and a developer's shell must
        // not decide what the suite sees — so clear it, then apply whatever
        // this test asked for.
        for key in [
            "VITE_FRUITNATION_WALLET",
            "VITE_FRUITNATION_HASH_CRO",
            // Doubly important: leaving a developer's own address in here
            // would give one wallet in the suite silent admin powers, and the
            // tests that assert an action is refused would pass on CI and fail
            // on that developer's machine — or worse, the reverse.
            "VITE_FRUITNATION_ADMIN",
            "VITE_CHAIN_ID",
            "VITE_CHAIN_RPC",
            "VITE_CHAIN_NAME",
            "VITE_CHAIN_EXPLORER",
        ] {
            cmd.env_remove(key);
        }
        // Clearing the environment is only half of it: `make build` bakes these
        // same values in with `option_env!`, and no amount of `env_remove`
        // reaches a string that is already compiled into the binary. Without
        // this, a developer who had run `make build` — which is everyone who
        // has run the server — tested against a wallet the suite thought it had
        // taken away.
        cmd.env("PS_IGNORE_BAKED_ENV", "1");
        for (key, value) in env {
            cmd.env(key, value);
        }

        let child = cmd.spawn().map_err(|e| format!("spawn server: {e}"))?;

        let scheme = if extra.contains(&"--tls") {
            "https"
        } else {
            "http"
        };
        let mut server = TestServer {
            child,
            port,
            redirect_port: None,
            http3_port: None,
            data_dir,
            base_url: format!("{scheme}://127.0.0.1:{port}"),
        };

        match server.await_health().await {
            Ok(()) => Ok(server),
            Err(e) => {
                let logs = std::fs::read_to_string(&log_path).unwrap_or_default();
                let tail: String = logs.lines().rev().take(30).collect::<Vec<_>>().join("\n");
                Err(format!("{e}\n--- server log (tail) ---\n{tail}"))
            }
        }
    }

    async fn await_health(&mut self) -> Result<(), String> {
        let url = format!("{}/api/health", self.base_url);
        let deadline = Instant::now() + BOOT_TIMEOUT;

        // Over TLS the client has to trust the CA the server just wrote, and
        // that file does not exist until the server has generated it — so wait
        // for it before there is anything to probe with.
        let http = if self.is_tls() {
            while !self.ca_path().is_file() && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            self.try_client()?
        } else {
            reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .map_err(|e| e.to_string())?
        };

        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(format!("server exited during boot with {status}"));
            }
            if let Ok(resp) = http.get(&url).send().await {
                if resp.status().is_success() {
                    // Somebody answered — make sure it was us. If our child
                    // lost a bind race it has already exited, and this 200 came
                    // from the winner.
                    return match self.child.try_wait() {
                        Ok(None) => Ok(()),
                        Ok(Some(status)) => Err(format!(
                            "another process owns port {}; our child exited with {status}",
                            self.port
                        )),
                        Err(e) => Err(format!("could not check on the server process: {e}")),
                    };
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Err(format!(
            "/api/health never became ready on port {}",
            self.port
        ))
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// A URL on the plain-HTTP redirect listener. Panics unless the server was
    /// started with [`TestServer::start_tls`].
    pub fn redirect_url(&self, path: &str) -> String {
        let port = self
            .redirect_port
            .expect("this server has no HTTP redirect listener");
        format!("http://127.0.0.1:{port}{path}")
    }

    pub fn ws_url(&self, path: &str) -> String {
        let scheme = if self.is_tls() { "wss" } else { "ws" };
        format!("{scheme}://127.0.0.1:{}{}", self.port, path)
    }

    pub fn is_tls(&self) -> bool {
        self.base_url.starts_with("https://")
    }

    /// The CA this server generated for itself.
    pub fn ca_path(&self) -> PathBuf {
        self.data_dir.join("tls").join("ca.crt")
    }

    /// The CA in PEM, or `None` for a plain-HTTP server.
    pub fn ca_pem(&self) -> Option<Vec<u8>> {
        self.is_tls()
            .then(|| std::fs::read(self.ca_path()).expect("the server must have written its CA"))
    }

    /// An HTTP client that trusts this server's CA and *only* its CA.
    ///
    /// Deliberately not `danger_accept_invalid_certs`: a test that skips
    /// verification would pass against a certificate no real client would
    /// accept, which is the one thing worth asserting here.
    pub fn client(&self) -> reqwest::Client {
        self.try_client().expect("build a client")
    }

    fn try_client(&self) -> Result<reqwest::Client, String> {
        let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(5));
        if self.is_tls() {
            let pem = std::fs::read(self.ca_path())
                .map_err(|e| format!("read {:?}: {e}", self.ca_path()))?;
            let ca = reqwest::Certificate::from_pem(&pem).map_err(|e| e.to_string())?;
            builder = builder
                .add_root_certificate(ca)
                .tls_built_in_root_certs(false);
        }
        builder.build().map_err(|e| e.to_string())
    }

    pub fn events_dir(&self) -> PathBuf {
        self.data_dir.join("events")
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("pocketskynet.db")
    }

    /// The server's own log, for diagnosing a failure from inside a test.
    pub fn server_log(&self) -> String {
        std::fs::read_to_string(self.data_dir.join("server.log")).unwrap_or_default()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    port
}

/// A free UDP port. Separate from [`free_port`] on purpose: TCP and UDP port
/// numbers live in different namespaces, so probing one says nothing about the
/// other and a QUIC listener handed a "free" TCP port can still collide.
fn free_udp_port() -> u16 {
    let socket = std::net::UdpSocket::bind(("127.0.0.1", 0)).expect("bind ephemeral UDP port");
    let port = socket.local_addr().expect("local_addr").port();
    drop(socket);
    port
}

fn unique_dir() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("ps-it-{}-{nanos}-{n}", std::process::id()))
}
