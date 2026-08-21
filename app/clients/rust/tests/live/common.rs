//! Boots a real `pocketskynet` server process per test, mirroring the
//! server's own integration harness (`app/server/tests/common/harness.rs`).
//!
//! Every test gets its own process, its own ephemeral port and its own temp
//! data directory, so the suite runs in parallel with no shared SQLite file.
//! Teardown happens in `Drop`, which runs even when a test panics — a leaked
//! server would hold a port and a temp directory for the rest of the run.
//!
//! Boots are serialised (see [`BOOT_LOCK`]) because "ask the OS for a free
//! port, close it, then hand the number to a child" has a window in which two
//! tests can be handed the same port. Only one child wins the bind; the loser
//! exits, and its `/api/health` probe is happily answered by the *winner* —
//! so after a 200 the child is checked to still be alive before the boot is
//! declared good.
//!
//! Unlike the server's harness, this crate has no `CARGO_BIN_EXE_pocketskynet`
//! — the binary belongs to a sibling crate — so [`server_bin`] locates a
//! prebuilt one (honouring `CARGO_TARGET_DIR` and `POCKETSKYNET_SERVER_BIN`)
//! and builds it once via `cargo build -p pocketskynet-server` when missing,
//! failing with a clear message rather than a mysterious spawn error.

use std::fs::File;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pocketskynet_client::{Client, Transport, TransportOptions, Wallet};

/// Handed to the server with `--jwt-secret` so tests can mint and tamper
/// with tokens themselves (expiry, wrong signature).
pub const JWT_SECRET: &str = "pocketskynet-client-integration-secret-0123456789abcdef";

/// How long to wait for `/api/health` before declaring the boot failed.
const BOOT_TIMEOUT: Duration = Duration::from_secs(30);

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Held from picking a port until that port answers `/api/health`, so no two
/// children of this process can be aimed at the same one.
static BOOT_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

/// The server binary under test: located, or built exactly once.
static SERVER_BIN: LazyLock<PathBuf> = LazyLock::new(|| {
    if let Some(overridden) = std::env::var_os("POCKETSKYNET_SERVER_BIN") {
        let bin = PathBuf::from(overridden);
        assert!(
            bin.is_file(),
            "POCKETSKYNET_SERVER_BIN points at {bin:?}, which does not exist"
        );
        return bin;
    }

    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("target"));
    let bin = target
        .join("debug")
        .join(format!("pocketskynet{}", std::env::consts::EXE_SUFFIX));

    if !bin.is_file() {
        // Build it once for the whole test binary. Cargo releases the target
        // directory lock once compilation of this suite finished, so a nested
        // build does not deadlock.
        let cargo = option_env!("CARGO").unwrap_or("cargo");
        let built = Command::new(cargo)
            .args(["build", "-p", "pocketskynet-server"])
            .current_dir(&workspace)
            .status();
        match built {
            Ok(status) if status.success() => {}
            other => panic!(
                "could not build the `pocketskynet` server binary for the integration tests \
                 ({other:?}). Run `cargo build -p pocketskynet-server` in {workspace:?} first, \
                 or point POCKETSKYNET_SERVER_BIN at an existing binary."
            ),
        }
    }

    assert!(
        bin.is_file(),
        "no server binary at {bin:?} even after building — set POCKETSKYNET_SERVER_BIN \
         or CARGO_TARGET_DIR to where `cargo build -p pocketskynet-server` puts it"
    );
    bin
});

pub struct TestServer {
    child: Child,
    pub port: u16,
    /// The UDP port serving HTTP/3, when this server was started with one.
    pub http3_port: Option<u16>,
    pub data_dir: PathBuf,
    pub base_url: String,
}

impl TestServer {
    /// Start a plain-HTTP server and block until `/api/health` answers 200.
    pub async fn start() -> Self {
        Self::start_with_args(&[]).await
    }

    pub async fn start_with_args(extra: &[&str]) -> Self {
        let mut last_err = String::new();
        for _ in 0..5 {
            match Self::try_start(extra).await {
                Ok(server) => return server,
                Err(e) => last_err = e,
            }
        }
        panic!("could not start pocketskynet after 5 attempts: {last_err}");
    }

    /// Start an HTTPS server with a freshly generated self-signed chain.
    ///
    /// The redirect port is allocated here rather than left to default to
    /// `port + 1`: the suite runs wide open, and `port + 1` belongs to
    /// whoever `free_port` handed it to.
    pub async fn start_tls() -> Self {
        let mut last_err = String::new();
        for _ in 0..5 {
            let redirect = free_port().to_string();
            match Self::try_start(&["--tls", "--http-redirect-port", &redirect]).await {
                Ok(server) => return server,
                Err(e) => last_err = e,
            }
        }
        panic!("could not start pocketskynet over TLS after 5 attempts: {last_err}");
    }

    /// Start with both listeners live: plain HTTP on the TCP port, HTTP/3 on
    /// a UDP port of its own — the cross-transport configuration worth
    /// pinning, since QUIC mandates TLS even when TCP goes without.
    pub async fn start_http3() -> Self {
        let mut last_err = String::new();
        for _ in 0..5 {
            let quic = free_udp_port();
            let quic_s = quic.to_string();
            match Self::try_start(&["--http3", "--http3-port", &quic_s]).await {
                Ok(mut server) => {
                    server.http3_port = Some(quic);
                    return server;
                }
                Err(e) => last_err = e,
            }
        }
        panic!("could not start pocketskynet with HTTP/3 after 5 attempts: {last_err}");
    }

    async fn try_start(extra: &[&str]) -> Result<Self, String> {
        let _boot = BOOT_LOCK.lock().await;
        let port = free_port();
        let data_dir = unique_dir();
        std::fs::create_dir_all(&data_dir).map_err(|e| format!("mkdir {data_dir:?}: {e}"))?;
        let static_dir = data_dir.join("static");
        std::fs::create_dir_all(&static_dir).map_err(|e| format!("mkdir {static_dir:?}: {e}"))?;

        // Piped stdio would deadlock the child once the pipe buffer filled,
        // so logs go to a file we can quote back in a failure message.
        let log_path = data_dir.join("server.log");
        let log = File::create(&log_path).map_err(|e| format!("create log: {e}"))?;
        let log_err = log.try_clone().map_err(|e| format!("clone log: {e}"))?;

        let port_s = port.to_string();
        let dir_s = data_dir.to_string_lossy().to_string();
        let static_s = static_dir.to_string_lossy().to_string();
        let mut cmd = Command::new(&*SERVER_BIN);
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
            // Every CLI command logs in afresh; the 5/min login limiter
            // would fail the suite on pacing, not correctness.
            "--no-rate-limit",
            "--no-payment-verify",
            // Advertising would spawn a `dns-sd` child per server on macOS,
            // and the SIGKILL in `Drop` would orphan every one of them.
            "--no-mdns",
            "--log",
            "warn",
        ])
        .args(extra)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));

        // A developer's shell must not decide what the suite tests.
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
            "PS_TLS",
            "PS_TLS_CERT",
            "PS_TLS_KEY",
            "PS_HTTP_REDIRECT_PORT",
            "PS_HTTP3",
            "PS_HTTP3_PORT",
            "PS_LOG",
            "VITE_FRUITNATION_WALLET",
            "VITE_FRUITNATION_HASH_CRO",
            "VITE_FRUITNATION_ADMIN",
            "VITE_CHAIN_ID",
            "VITE_CHAIN_RPC",
            "VITE_CHAIN_NAME",
            "VITE_CHAIN_EXPLORER",
        ] {
            cmd.env_remove(key);
        }
        // `make build` bakes chain metadata in with `option_env!`; without
        // this a developer who ran it would test against a wallet the suite
        // thought it had cleared away.
        cmd.env("PS_IGNORE_BAKED_ENV", "1");

        let child = cmd
            .spawn()
            .map_err(|e| format!("spawn the server binary {:?}: {e}", &*SERVER_BIN))?;

        let scheme = if extra.contains(&"--tls") {
            "https"
        } else {
            "http"
        };
        let mut server = TestServer {
            child,
            port,
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
        // The probe accepts the self-signed dev certificate, exactly like
        // the client's own --insecure mode does.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .danger_accept_invalid_certs(true)
            .build()
            .map_err(|e| e.to_string())?;

        let deadline = Instant::now() + BOOT_TIMEOUT;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(format!("server exited during boot with {status}"));
            }
            if let Ok(resp) = http.get(&url).send().await {
                if resp.status().is_success() {
                    // Somebody answered — make sure it was us. If our child
                    // lost a bind race it has already exited, and this 200
                    // came from the winner.
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

    pub fn is_tls(&self) -> bool {
        self.base_url.starts_with("https://")
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
/// numbers live in different namespaces, so probing one says nothing about
/// the other.
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
    std::env::temp_dir().join(format!("psc-it-{}-{nanos}-{n}", std::process::id()))
}

// --- client-side conveniences ----------------------------------------------

pub fn insecure() -> TransportOptions {
    TransportOptions { insecure: true }
}

/// An HTTP/1.1 client for this server, trusting its self-signed certificate
/// the way the CLI's `--insecure` does.
pub fn h1(server: &TestServer) -> Client {
    Client::new(Transport::http1(&server.base_url, &insecure()).expect("h1 transport builds"))
}

/// An HTTP/3 client for this server. Panics unless it was started with
/// [`TestServer::start_http3`]; passes the UDP port as an override, which is
/// also what exercises the `--http3-port` path.
pub async fn h3(server: &TestServer) -> Client {
    let quic = server
        .http3_port
        .expect("this server has no HTTP/3 listener");
    Client::new(
        Transport::http3(&server.base_url, &insecure(), Some(quic))
            .await
            .expect("the QUIC handshake must complete"),
    )
}

/// A fresh random wallet plus a logged-in HTTP/1.1 client for it.
pub async fn login_new_user(server: &TestServer, username: &str) -> (Client, Wallet) {
    let wallet = Wallet::random().expect("generate a wallet");
    let mut client = h1(server);
    client
        .login(&wallet, Some(username))
        .await
        .expect("login must succeed");
    (client, wallet)
}
