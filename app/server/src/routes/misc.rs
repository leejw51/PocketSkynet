//! System endpoints: liveness, chain metadata, and how to reach this server.

use axum::extract::State;
use axum::http::{StatusCode, Version};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};

use crate::AppState;

/// `GET /api/health`, registered outside the rate limiter.
pub fn health_router() -> Router<AppState> {
    Router::new().route("/health", get(health))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/blockchain/info", get(blockchain_info))
        .route("/networks", get(networks))
        .route("/server/info", get(server_info))
}

/// Liveness, including a real database probe.
///
/// The probe matters: a process that is up but cannot reach its database
/// serves 500s for every useful request, and a health check that only proves
/// the HTTP stack is alive would keep it in the load-balancer pool.
///
/// The body key is `status`, not `message` — this endpoint predates the error
/// envelope and clients match on it.
async fn health(State(state): State<AppState>) -> Response {
    let reachable = state
        .db
        .call(|conn| {
            conn.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))?;
            Ok(())
        })
        .await
        .is_ok();

    if !reachable {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "status": "unavailable" })),
        )
            .into_response();
    }

    let uptime = state.started.elapsed().as_secs();
    (
        StatusCode::OK,
        Json(serde_json::json!({ "status": "ok", "uptime": uptime })),
    )
        .into_response()
}

/// `GET /api/server/info` — where this server is, and how you got here.
///
/// Exists because the transport is otherwise invisible from inside a client.
/// A browser will quietly move to HTTP/3 once it has seen `Alt-Svc`, and the
/// only honest way for the page to say which one it is on is to be told by the
/// end that actually terminated the connection.
///
/// Unauthenticated: everything here is already discoverable by connecting —
/// the addresses are the ones the server prints on startup, and the protocol
/// is a property of the request the caller just made.
async fn server_info(State(state): State<AppState>, request: axum::extract::Request) -> Response {
    // The version of *this* request. Over the QUIC listener the router is
    // driven directly rather than through hyper, so the extension carries what
    // the h3 bridge recorded; over TCP it is hyper's own.
    let protocol = match request.version() {
        Version::HTTP_3 => "h3",
        Version::HTTP_2 => "h2",
        Version::HTTP_11 => "http/1.1",
        Version::HTTP_10 => "http/1.0",
        _ => "unknown",
    };

    let scheme = if state.cfg.tls.is_on() {
        "https"
    } else {
        "http"
    };
    let http3_port = state.cfg.http3_port;

    Json(serde_json::json!({
        // What carried this very request.
        "protocol": protocol,
        // The TCP listener.
        "scheme": scheme,
        "port": state.cfg.port,
        // The QUIC listener, when there is one. `null` rather than absent so a
        // client can branch on it without a key check.
        "http3Port": http3_port,
        "http3Available": http3_port.is_some(),
        // Every address this deployment answers on, per transport, so a client
        // can show them without re-deriving the host list itself.
        "endpoints": endpoint_json(&state),
        // The one base URL to put in front of a relative path when the
        // resulting link is meant for *somebody else's* device: the mesh-VPN
        // (Tailscale) address when the host has one, else a LAN address,
        // `null` when this server is loopback-only or its port is ephemeral.
        // Without it a client can only offer its own origin, and a link
        // reading `http://127.0.0.1:9099/…` is useless to everyone but the
        // person who copied it. Same value `GET /api/sites` serves.
        "shareBase": crate::share_base(&state.cfg),
        "uptime": state.started.elapsed().as_secs(),
        // Realtime lives only on TCP: there is no WebSocket over HTTP/3, and a
        // client that assumed otherwise would wait forever for an upgrade.
        "websocketTransport": "tcp",
        // Whether there is a CA at `/ca.crt` to install. The client needs it
        // here as well as on `/api/blockchain/info` because it is the answer
        // to "HTTP/3 is offered, so why am I not on it?" — a browser will not
        // speak QUIC to a certificate it does not genuinely trust, and unlike
        // TLS-over-TCP there is no click-through.
        "caCertAvailable": ca_available(&state),
    }))
    .into_response()
}

/// The server's addresses, grouped by transport.
///
/// Empty when the configured port is `0` — the desktop app's ephemeral-port
/// fallback. The bound port is not knowable from the config, and a URL ending
/// in `:0` would be a lie; the client falls back to its own origin, exactly as
/// it does for `share_base`.
fn endpoint_json(state: &AppState) -> serde_json::Value {
    if state.cfg.port == 0 {
        return serde_json::json!({ "tcp": [], "http3": [] });
    }
    let addr = std::net::SocketAddr::new(state.cfg.host, state.cfg.port);
    let scheme = if state.cfg.tls.is_on() {
        crate::Scheme::Https
    } else {
        crate::Scheme::Http
    };
    let tcp = crate::connect_urls(addr, scheme);
    let quic = state
        .cfg
        .http3_port
        .map(|port| crate::http3_urls(&tcp, port))
        .unwrap_or_default();

    let render = |list: &[crate::Endpoint]| -> Vec<serde_json::Value> {
        list.iter()
            .map(|e| serde_json::json!({ "url": e.url, "reach": e.reach.label() }))
            .collect()
    };
    serde_json::json!({ "tcp": render(&tcp), "http3": render(&quic) })
}

/// Read a deployment setting: the runtime environment first, then the value
/// baked in when this binary was built.
///
/// Two delivery mechanisms, because there are two ways this server runs.
/// `make start` sources `.env` and execs the binary, so the setting arrives
/// through the environment. The desktop app embeds the server in its own
/// process and is launched by Finder from `/Applications` — there is no shell
/// to source anything, and a bundle carries no `.env`, so without a compiled-in
/// value every paid feature fails on an installed app with "no payment wallet
/// configured". The build supplies `baked` from the release environment
/// (`build.rs` re-runs whenever one of these changes).
///
/// Runtime wins, so one build can still be pointed at another deployment
/// without recompiling, and `.env` keeps meaning what it has always meant.
fn setting_opt(key: &str, baked: Option<&'static str>) -> Option<String> {
    // Blank counts as unconfigured on both sides: an exported-but-empty
    // variable is how a shell says "I have no opinion", and letting it mask the
    // compiled-in value would break the installed app in the one case the
    // compiled-in value exists to cover.
    fn present(v: &str) -> Option<String> {
        let v = v.trim();
        (!v.is_empty()).then(|| v.to_owned())
    }
    let runtime = std::env::var(key).ok();
    let baked = if ignore_baked() { None } else { baked };
    runtime
        .as_deref()
        .and_then(present)
        .or_else(|| baked.and_then(present))
}

/// Whether compiled-in defaults are off.
///
/// The integration harness clears the `VITE_*` variables before spawning a
/// server so that a developer's `.env` cannot decide what the suite sees. That
/// scrub reaches the *environment* only, and [`setting_opt`] then falls back to
/// values `option_env!` captured when the binary was compiled — which `make
/// build` populates from `.env` on purpose. So a machine that had ever built
/// normally ran the "no payment wallet" test against a server that did have
/// one, and it failed there and nowhere else.
///
/// This is the switch that makes the scrub total. Nothing in a real deployment
/// sets it; it exists so a test can say "this server is configured with
/// nothing" and be believed.
fn ignore_baked() -> bool {
    std::env::var("PS_IGNORE_BAKED_ENV").is_ok_and(|v| {
        let v = v.trim();
        !v.is_empty() && v != "0"
    })
}

/// The same lookup, flattened to an empty string.
///
/// Empty rather than absent because clients index these fields directly; a
/// missing key would be a `TypeError` in the browser while an empty string is
/// merely an unconfigured chain.
fn setting(key: &str, baked: Option<&'static str>) -> String {
    setting_opt(key, baked).unwrap_or_default()
}

/// The Privy app id, or empty when Privy sign-in is not configured.
///
/// Served rather than compiled in, because this client is a WASM binary: the
/// reference client bakes `VITE_PRIVY_APPID` at build time, which a shipped
/// `.wasm` cannot do per deployment. An empty value is the feature flag — the
/// client offers Privy only when there is an id to offer it with, exactly as
/// the reference does with `PRIVY_ENABLED = !!import.meta.env.VITE_PRIVY_APPID`.
///
/// Not a secret. A Privy app id is a public client-side identifier; the
/// server-side counterpart is the app *secret*, which is not here and is not
/// needed — this server never talks to Privy at all.
pub fn privy_app_id() -> String {
    setting("VITE_PRIVY_APPID", option_env!("VITE_PRIVY_APPID"))
}

/// The server wallet that on-chain message anchors must pay.
pub fn server_wallet() -> String {
    setting(
        "VITE_FRUITNATION_WALLET",
        option_env!("VITE_FRUITNATION_WALLET"),
    )
}

/// The CRO price of an on-chain anchor.
pub fn hash_price_cro() -> String {
    setting_opt(
        "VITE_FRUITNATION_HASH_CRO",
        option_env!("VITE_FRUITNATION_HASH_CRO"),
    )
    .unwrap_or_else(|| "1.2".to_owned())
}

/// The chain this deployment runs on.
///
/// **The server owns this, not the client.** A wallet that could be pointed at
/// a different chain than the one the server anchors to is a wallet that can
/// sign a transaction nobody will ever look for — so the choice belongs to
/// whoever configures the deployment, in one place, for everyone.
///
/// `VITE_CHAIN_ID` selects it out of the compiled registry; unset means Cronos
/// mainnet, and an id that matches nothing falls back the same way rather than
/// leaving the wallet with no RPC to talk to.
pub fn configured_network() -> pocketskynet_core::chain::Network {
    network_for(setting_opt("VITE_CHAIN_ID", option_env!("VITE_CHAIN_ID")).as_deref())
}

/// [`configured_network`] with the id supplied rather than read.
///
/// Split out so the fallback rules can be tested without touching process-wide
/// environment state — which is shared by every test thread and, now that a
/// value can also be compiled in, is not something a test can fully clear.
fn network_for(id: Option<&str>) -> pocketskynet_core::chain::Network {
    let nets = pocketskynet_core::chain::builtin_networks();
    let wanted: Option<u64> = id.and_then(|v| v.trim().parse().ok());
    wanted
        .and_then(|id| nets.iter().find(|n| n.chain_id == Some(id)).cloned())
        .unwrap_or_else(|| {
            nets.into_iter()
                .find(|n| n.id == "cronos-mainnet")
                .expect("the registry always carries cronos mainnet")
        })
}

/// `GET /api/blockchain/info` — everything a client needs to build a
/// publish transaction.
///
/// Every field derives from [`configured_network`], so the wallet, the ribbon,
/// the identity card and the anchor explorer link cannot disagree about which
/// chain is in play.
///
/// **The name and the explorer are deliberately not overridable.** They were,
/// and it was a trap: a deployment whose environment still carried
/// `VITE_CHAIN_NAME="CRONOS EVM TESTNET"` from an earlier configuration would
/// serve chain 25 wearing the word TESTNET, which is the worst possible lie to
/// tell next to a Send button. A label that can contradict the chain it labels
/// is not a feature.
///
/// The RPC is not overridable either, for the same reason and a sharper one.
/// A stale `VITE_CHAIN_RPC` left over from another chain does not merely
/// mislabel the wallet — it points signing at a node that knows nothing about
/// the chain the id claims, so balances read as zero and a send is broadcast
/// where nobody is listening. `VITE_CHAIN_ID` is the single knob; everything
/// else follows it, and a deployment that needs a private endpoint adds a
/// network to the registry rather than half-editing this one.
async fn blockchain_info(State(state): State<AppState>) -> Response {
    let hash_cro = hash_price_cro();
    let net = configured_network();

    Json(serde_json::json!({
        "chainId": net.chain_id.map(|c| c.to_string()).unwrap_or_default(),
        "chainRpc": net.rpc_url,
        "chainName": net.name.to_uppercase(),
        "chainExplorer": net.explorer_url,
        "fruitnationHashCro": hash_cro,
        "fruitnationWallet": server_wallet(),
        "privyAppId": privy_app_id(),
        // Paid-feature prices, in CRO, as decimal strings. Served rather than
        // compiled into the client so an operator can retune them
        // (`PS_SHOUT_PRICE_CRO`, `PS_PUBLISH_PRICE_CRO`) without a release —
        // and so the number on the pay button is the number the server will
        // actually enforce against the transaction.
        "shoutPriceCro": crate::payment::shout_price_cro(),
        "publishPriceCro": crate::payment::publish_price_cro(),
        // Whether this server generated its own CA and is offering it for
        // download. The client shows the "trust this server" flow only when
        // there is something to install: plain HTTP has nothing to trust, and a
        // supplied certificate is already trusted by whoever issued it.
        //
        // Named for what the client does with it rather than for the transport,
        // because this endpoint has quietly become the one unauthenticated
        // config channel the client reads at boot.
        "caCertAvailable": ca_available(&state),
    }))
    .into_response()
}

/// Whether a generated CA is on disk and downloadable at `/ca.crt`.
///
/// Read from the resolved config rather than from `POCKETSKYNET_PATH`: the data
/// directory can be set by flag as well as by environment, and a server started
/// with `--data-dir` would otherwise be asked about the wrong path and answer
/// confidently that there is no certificate.
fn ca_available(state: &AppState) -> bool {
    // `generates_own_ca()` rather than a `SelfSigned` match: with `--http3`
    // and no `--tls` the server still writes a CA, because QUIC has no
    // plaintext mode — and `/ca.crt` already serves it. Matching only on
    // `SelfSigned` reported "nothing to trust" for a deployment whose QUIC
    // listener could not be reached without trusting exactly that file.
    state.cfg.generates_own_ca() && state.cfg.tls_dir().join("ca.crt").exists()
}

/// `GET /api/networks` — the chain the wallet operates on.
///
/// Deliberately **one entry, not a menu**. This used to serve the whole
/// compiled registry so the client could offer a switcher, which made the
/// active chain a per-browser preference: two people on the same deployment
/// could be spending on different chains, and either could be on a chain the
/// server does not anchor to. Now the deployment decides
/// ([`configured_network`]) and the client reports what it is told.
///
/// Still an array, so the shape is unchanged for any client that iterates it,
/// and so a future deployment can offer several without a protocol change.
async fn networks() -> Response {
    Json(vec![configured_network()]).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::build;
    use crate::test_support::{send, state};

    #[tokio::test]
    async fn health_reports_ok_with_an_uptime() {
        let router = build(state("health"));
        let response = send(&router, "GET", "/api/health", None, None).await;

        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.json()["status"], "ok");
        assert!(response.json()["uptime"].is_number());
        assert!(
            response.json().get("message").is_none(),
            "the key is `status`, not `message`"
        );
    }

    #[tokio::test]
    async fn a_plain_http_server_offers_no_certificate_to_trust() {
        // The trust affordance must not appear where it cannot help. A test
        // server runs with TLS off, so there is nothing generated to install —
        // and a "trust this server" link that 404s is worse than none.
        let state = state("ca-none");
        let router = build(state);
        let response = send(&router, "GET", "/api/blockchain/info", None, None).await;
        assert_eq!(response.json()["caCertAvailable"], false);

        // And the endpoint itself says so rather than serving something odd.
        let response = send(&router, "GET", "/ca.crt", None, None).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn blockchain_info_always_has_every_key() {
        let router = build(state("chaininfo"));
        let response = send(&router, "GET", "/api/blockchain/info", None, None).await;

        assert_eq!(response.status, StatusCode::OK);
        for key in [
            "chainId",
            "chainRpc",
            "chainName",
            "chainExplorer",
            "fruitnationHashCro",
            "fruitnationWallet",
            // Empty when Privy is not configured, but always present: the
            // client treats a missing key and an empty one differently, and
            // only the latter means "not offered".
            "privyAppId",
            // The paid-feature price tags the client renders on its buttons.
            "shoutPriceCro",
            "publishPriceCro",
        ] {
            assert!(
                response.json()[key].is_string(),
                "{key} must be present as a string, even unconfigured"
            );
        }
    }

    #[tokio::test]
    // Holding the lock across the await is the point: the guard serialises
    // env-var access against the test that *sets* VITE_CHAIN_ID, and the
    // request it spans is in-process with no other awaiter of this mutex.
    #[allow(clippy::await_holding_lock)]
    async fn networks_serves_only_the_configured_chain() {
        // Held across the request: the variable is process-wide, so this must
        // not overlap the test that sets it to something else.
        let _guard = CHAIN_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("VITE_CHAIN_ID").ok();
        // Pinned rather than cleared. A build can now carry a compiled-in chain
        // id, so "unset" no longer means "mainnet" — that fallback is asserted
        // hermetically by `the_chain_id_selects_the_network_and_garbage_falls_back`.
        std::env::set_var("VITE_CHAIN_ID", "25");

        let router = build(state("networks"));
        let response = send(&router, "GET", "/api/networks", None, None).await;

        assert_eq!(response.status, StatusCode::OK);
        let nets = response.json();
        let nets = nets.as_array().expect("an array of networks");
        // Exactly one: the endpoint is a statement of fact, not a menu. If it
        // grows entries again the client regains a switcher it must not have.
        assert_eq!(nets.len(), 1, "the client must not be offered a choice");
        assert_eq!(nets[0]["id"], "cronos-mainnet");
        assert_eq!(nets[0]["testnet"], false);
        assert_eq!(nets[0]["chainId"], 25);
        assert_eq!(nets[0]["tokens"][0]["symbol"], "USDC");
        assert_eq!(nets[0]["tokens"][0]["decimals"], 6);

        restore("VITE_CHAIN_ID", prior);
    }

    /// Put a process-wide variable back exactly as it was, absent included.
    fn restore(key: &str, prior: Option<String>) {
        match prior {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    /// `VITE_CHAIN_ID` is process-wide, and cargo runs tests in threads of one
    /// process: without this, the test that *sets* it and the test that reads
    /// the default race, and the loser sees a chain it never asked for.
    static CHAIN_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn the_chain_id_selects_the_network_and_garbage_falls_back() {
        assert_eq!(network_for(Some("338")).id, "cronos-testnet");
        assert!(network_for(Some("338")).testnet);
        assert_eq!(network_for(Some("25")).id, "cronos-mainnet");

        // An id nobody serves, a value that is not a number at all, and no id
        // whatsoever all land on mainnet rather than leaving the wallet without
        // an RPC to talk to.
        assert_eq!(network_for(Some("999999")).id, "cronos-mainnet");
        assert_eq!(network_for(Some("not-a-number")).id, "cronos-mainnet");
        assert_eq!(network_for(None).id, "cronos-mainnet");
    }

    #[test]
    fn a_stale_label_never_survives_a_chain_change() {
        // The exact shape of a live bug: an environment still exporting the
        // testnet name while the id says mainnet. The name and the explorer
        // follow the id and are not overridable.
        let _guard = CHAIN_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("VITE_CHAIN_ID").ok();

        std::env::set_var("VITE_CHAIN_ID", "25");
        std::env::set_var("VITE_CHAIN_NAME", "CRONOS EVM TESTNET");
        std::env::set_var("VITE_CHAIN_EXPLORER", "https://explorer.cronos.org/testnet");
        let net = configured_network();
        assert_eq!(net.name.to_uppercase(), "CRONOS MAINNET");
        assert_eq!(net.explorer_url, "https://explorer.cronos.org");

        std::env::remove_var("VITE_CHAIN_NAME");
        std::env::remove_var("VITE_CHAIN_EXPLORER");
        restore("VITE_CHAIN_ID", prior);
    }

    #[test]
    fn a_compiled_in_default_applies_only_when_the_environment_is_silent() {
        // This is what makes the installed desktop app work: it is launched by
        // Finder with no shell to source `.env`, so the compiled-in value is
        // the only configuration it will ever see.
        let key = "PS_TEST_SETTING_PRECEDENCE";
        std::env::remove_var(key);
        assert_eq!(setting(key, Some("baked")), "baked");
        assert_eq!(setting(key, None), "", "nothing configured either way");

        // A real deployment setting still wins, so one build can be pointed at
        // another deployment without recompiling.
        std::env::set_var(key, "runtime");
        assert_eq!(setting(key, Some("baked")), "runtime");

        // An exported-but-blank variable is a shell with no opinion, not a
        // value — it must not mask the compiled-in default.
        std::env::set_var(key, "   ");
        assert_eq!(setting(key, Some("baked")), "baked");

        std::env::remove_var(key);
    }

    #[tokio::test]
    async fn neither_system_endpoint_needs_a_token() {
        let router = build(state("noauth"));
        assert_eq!(
            send(&router, "GET", "/api/health", None, None).await.status,
            StatusCode::OK
        );
        assert_eq!(
            send(&router, "GET", "/api/blockchain/info", None, None)
                .await
                .status,
            StatusCode::OK
        );
    }
}
