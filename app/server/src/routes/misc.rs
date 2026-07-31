//! System endpoints: liveness and chain metadata.

use axum::extract::State;
use axum::http::StatusCode;
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

/// Read a chain setting, falling back to an empty string.
///
/// Empty rather than absent because clients index these fields directly; a
/// missing key would be a `TypeError` in the browser while an empty string is
/// merely an unconfigured chain.
fn env_or_empty(key: &str) -> String {
    std::env::var(key).unwrap_or_default()
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
    env_or_empty("VITE_PRIVY_APPID")
}

/// The server wallet that on-chain message anchors must pay.
pub fn server_wallet() -> String {
    env_or_empty("VITE_FRUITNATION_WALLET")
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
    let nets = pocketskynet_core::chain::builtin_networks();
    let wanted: Option<u64> = std::env::var("VITE_CHAIN_ID")
        .ok()
        .and_then(|v| v.trim().parse().ok());
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
    let hash_cro = std::env::var("VITE_FRUITNATION_HASH_CRO").unwrap_or_else(|_| "1.2".to_owned());
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
    matches!(state.cfg.tls, crate::config::Tls::SelfSigned)
        && state.cfg.tls_dir().join("ca.crt").exists()
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
    async fn networks_serves_only_the_configured_chain_defaulting_to_mainnet() {
        // Held across the request: this asserts the *unset* default, so it must
        // not overlap the test that sets the variable.
        let _guard = CHAIN_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("VITE_CHAIN_ID").ok();
        std::env::remove_var("VITE_CHAIN_ID");

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

        if let Some(v) = prior {
            std::env::set_var("VITE_CHAIN_ID", v);
        }
    }

    /// `VITE_CHAIN_ID` is process-wide, and cargo runs tests in threads of one
    /// process: without this, the test that *sets* it and the test that reads
    /// the default race, and the loser sees a chain it never asked for.
    static CHAIN_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn the_chain_env_var_selects_the_network_and_garbage_falls_back() {
        let _guard = CHAIN_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("VITE_CHAIN_ID").ok();

        std::env::set_var("VITE_CHAIN_ID", "338");
        assert_eq!(configured_network().id, "cronos-testnet");
        assert!(configured_network().testnet);

        std::env::set_var("VITE_CHAIN_ID", "25");
        assert_eq!(configured_network().id, "cronos-mainnet");

        // An id nobody serves, and a value that is not a number at all, both
        // land on mainnet rather than leaving the wallet without an RPC.
        std::env::set_var("VITE_CHAIN_ID", "999999");
        assert_eq!(configured_network().id, "cronos-mainnet");
        std::env::set_var("VITE_CHAIN_ID", "not-a-number");
        assert_eq!(configured_network().id, "cronos-mainnet");

        std::env::remove_var("VITE_CHAIN_ID");
        assert_eq!(configured_network().id, "cronos-mainnet");

        // A stale label from an earlier configuration must never survive a
        // chain change. This is the exact shape of a live bug: an environment
        // still exporting the testnet name while the id says mainnet.
        std::env::set_var("VITE_CHAIN_ID", "25");
        std::env::set_var("VITE_CHAIN_NAME", "CRONOS EVM TESTNET");
        std::env::set_var("VITE_CHAIN_EXPLORER", "https://explorer.cronos.org/testnet");
        let net = configured_network();
        assert_eq!(net.name.to_uppercase(), "CRONOS MAINNET");
        assert_eq!(net.explorer_url, "https://explorer.cronos.org");
        std::env::remove_var("VITE_CHAIN_NAME");
        std::env::remove_var("VITE_CHAIN_EXPLORER");

        std::env::remove_var("VITE_CHAIN_ID");
        if let Some(v) = prior {
            std::env::set_var("VITE_CHAIN_ID", v);
        }
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
