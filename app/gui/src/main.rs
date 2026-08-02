//! PocketSkynet desktop app.
//!
//! The server runs **inside** this process rather than beside it. That is the
//! whole design: the web client resolves its API against `window.location`, so
//! anything that serves the bundle from one origin and the API from another
//! would need a configurable base URL, CORS, and a second process to supervise.
//! Embedding removes all three — the window simply points at a loopback server
//! this app owns.
//!
//! Consequences worth knowing:
//!
//! - It binds **`0.0.0.0` by default**, so another machine on the same network
//!   can open the app's URL and sign in with their own wallet. That is a
//!   deliberate product decision, not an accident, and the app says so: the
//!   window title carries the address and the banner is printed on startup.
//!   `PS_HOST=127.0.0.1` restricts it to this machine.
//! - The port is **stable** (`PS_PORT`, default 9099), because a URL you cannot
//!   predict is a URL you cannot hand to anyone. If it is taken, the app falls
//!   back to an ephemeral port rather than refusing to start.
//! - Data lives in `~/.pocketskynet` (`POCKETSKYNET_PATH` overrides it), the
//!   same root the CLI server uses — so the app and `make start` share one
//!   database, and where you launched it from does not decide where your
//!   identity is stored.

// Release builds have no console on Windows; without this the app would flash a
// terminal behind the window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;

use pocketskynet_server::config::{load_or_create_secret, Config};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

/// The window surface behind the webview, matched to the client's dark
/// background so the frame and the page are the same colour from the first
/// frame drawn.
const WINDOW_BACKGROUND: tauri::window::Color = tauri::window::Color(10, 10, 12, 255);
use tracing_subscriber::EnvFilter;

/// Where the built web client lives.
///
/// Bundled as a Tauri resource in a packaged app; taken from the workspace in a
/// development run, so `make gui` works straight after `make build` without a
/// packaging step.
fn static_dir(app: &tauri::App) -> PathBuf {
    if let Ok(resources) = app.path().resource_dir() {
        let bundled = resources.join("web");
        if bundled.join("index.html").is_file() {
            return bundled;
        }
    }

    // Development: the crate sits next to `web/` in the workspace.
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join("web").join("dist"))
        .unwrap_or_default();

    if !workspace.join("index.html").is_file() {
        tracing::warn!(
            ?workspace,
            "no web bundle found — run `make build` first, or the window will be empty"
        );
    }
    workspace
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(
            std::env::var("PS_LOG").unwrap_or_else(|_| "info".into()),
        ))
        .with_target(false)
        .init();

    tauri::Builder::default()
        .setup(|app| {
            // The same resolver the CLI server uses — `POCKETSKYNET_PATH`,
            // else `~/.pocketskynet` — so the desktop app and `make start`
            // read and write the same database.
            let data_dir = pocketskynet_server::config::default_data_dir();
            std::fs::create_dir_all(&data_dir)?;

            let cfg = Config {
                // Reachable from other machines by default — see the module
                // docs. `PS_HOST=127.0.0.1` keeps it to this one.
                host: env_host(),
                port: env_port(),
                data_dir: data_dir.clone(),
                static_dir: static_dir(app),
                jwt_ttl_hours: 24 * 30,
                cors_origin: vec![],
                // The window is same-origin with the server, so the ticket flow
                // works and the query-string fallback is never needed.
                sse_token_query: false,
                // A single-user desktop app throttling itself would only ever
                // punish the person using it.
                rate_limit: false,
                // Paid features are real money even on a desktop: the shout
                // and publish endpoints verify their transactions here too.
                verify_payments: true,
                trust_proxy: 0,
                // Plain HTTP, deliberately. The window loads the server over
                // loopback, and a webview meets a self-signed certificate with
                // a hard failure and no way to accept it — the app would open
                // on an error page. `make start --tls` is the answer when the
                // server is meant to be reached from a phone or a tablet.
                tls: pocketskynet_server::config::Tls::Off,
                http_redirect_port: None,
                // No HTTP/3 either, for the same reason and one more: the
                // window talks to the server over loopback, where QUIC's
                // advantages (loss recovery, connection migration) do not
                // exist and its userspace packet handling is pure overhead.
                http3_port: None,
            };

            // Persisted next to the database: regenerating it on every launch
            // would sign the user out every time they open the app. This is the
            // server's own loader, deliberately — a JWT signing key is not
            // something to generate twice, two different ways.
            let secret = load_or_create_secret(&data_dir.join("jwt.secret"))?;

            // Rendered before the retry path can move `cfg`.
            let storage = pocketskynet_server::storage_banner(&cfg);

            // Bind before the window opens — the URL is not knowable until the
            // OS has assigned the port. A busy port is not a reason to fail:
            // fall back to an ephemeral one so the app always starts.
            let bound = match tauri::async_runtime::block_on(pocketskynet_server::bind(
                cfg.clone(),
                secret,
            )) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(error = %e, "preferred port unavailable; taking any free port");
                    let secret = load_or_create_secret(&data_dir.join("jwt.secret"))?;
                    let retry = Config { port: 0, ..cfg };
                    tauri::async_runtime::block_on(pocketskynet_server::bind(retry, secret))?
                }
            };

            let addr = bound.addr;
            let scheme = bound.scheme;
            let endpoints = pocketskynet_server::connect_urls(addr, scheme);
            // Printed as well as logged: this is the line someone copies to
            // another machine, and a log filter must not be able to hide it.
            println!(
                "{}",
                pocketskynet_server::connect_banner(addr, scheme, bound.redirect_port)
            );
            // The desktop app's data dir is inside the app sandbox, so "where
            // did my upload go?" is *harder* to answer here than for the CLI,
            // not easier.
            println!("{storage}");
            tracing::info!(?data_dir, "embedded server ready");

            // The window is always loaded over loopback even when the server is
            // reachable from the network — no reason to route our own traffic
            // through an external interface.
            let own_url = format!("http://127.0.0.1:{}", addr.port());

            // The shareable address lives in the title bar, because a desktop
            // app has nowhere else to put it and "what URL do I give them?" is
            // the first thing anyone asks.
            let share = pocketskynet_server::share_url(&endpoints)
                .map(|e| e.url.clone())
                .unwrap_or_else(|| own_url.clone());
            let title = if share == own_url {
                format!("PocketSkynet — {share}  (this machine only)")
            } else {
                format!("PocketSkynet — {share}")
            };

            tauri::async_runtime::spawn(async move {
                if let Err(e) = bound.serve(std::future::pending()).await {
                    tracing::error!(error = %e, "embedded server stopped");
                }
            });

            // Two details separate "a web page in a frame" from something that
            // feels like an application:
            //
            // 1. A webview's default surface is **white**. This app is
            //    near-black, so without a matching window background there is a
            //    white flash on every launch — the single most web-like tell
            //    there is. First paint measures ~44ms, which is plenty to see.
            // 2. The window is created only once the port is known. Showing it
            //    empty while the client boots is worse than not showing it, so
            //    it starts hidden and is revealed on first paint.
            let window = WebviewWindowBuilder::new(
                app,
                "main",
                WebviewUrl::External(own_url.parse().expect("a valid loopback URL")),
            )
            .title(title)
            .inner_size(1180.0, 820.0)
            .min_inner_size(360.0, 420.0)
            .center()
            .visible(false)
            .background_color(WINDOW_BACKGROUND)
            .on_page_load(|window, payload| {
                if matches!(payload.event(), tauri::webview::PageLoadEvent::Finished) {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            })
            .build()?;

            // Belt and braces: if the page never reports a load — a broken
            // bundle, an offline resource — the window must still appear rather
            // than leaving the user with a dock icon and nothing else.
            {
                let window = window.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                    if !window.is_visible().unwrap_or(true) {
                        tracing::warn!("page load never reported; showing the window anyway");
                        let _ = window.show();
                    }
                });
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to start PocketSkynet");
}

/// Bind address, `0.0.0.0` unless told otherwise.
///
/// Reachable-by-default is the product decision here; the app is explicit about
/// it in the window title and the startup banner rather than quiet about it.
fn env_host() -> IpAddr {
    std::env::var("PS_HOST")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}

/// Preferred port. Stable rather than ephemeral, because a URL nobody can
/// predict is a URL nobody can be given.
fn env_port() -> u16 {
    std::env::var("PS_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9099)
}
