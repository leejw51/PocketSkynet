//! On-chain payment verification for paid features.
//!
//! Shout (≥ 10 CRO) and web publishing (≥ 1 CRO) are paid to the operator's
//! FruitNation wallet (`VITE_FRUITNATION_WALLET`). The payment is the business
//! model — it funds the deployment — and the spam gate: broadcasting to every
//! connected screen or parking bytes on the operator's disk costs real money,
//! so flooding either is expensive by construction.
//!
//! The client pays first (signing in the browser, as always — the server never
//! sees a key), then presents the transaction hash. This module is the part
//! that refuses to take the client's word for it:
//!
//! 1. `eth_getTransactionByHash` — the transaction exists, is mined, pays the
//!    operator's wallet, was sent by the *authenticated caller*, and carries
//!    at least the feature's price.
//! 2. `eth_getTransactionReceipt` — it succeeded (`status == 0x1`), because a
//!    reverted transfer moved nothing.
//! 3. The `payments` table — the hash has not been used before. One payment,
//!    one action; a receipt is not a season ticket.
//!
//! `--no-payment-verify` skips step 1–2 for tests and offline development
//! (format and single-use checks still run), and is refused in production —
//! see `config.rs`.

use pocketskynet_core::chain::{parse_amount, parse_hex_quantity};
use pocketskynet_core::WalletAddress;
use serde_json::json;

use crate::error::{ApiError, ApiResult};
use crate::AppState;

/// What a payment bought. Stored in the `payments` row and echoed in audit
/// records, so the revenue ledger says what each transfer was for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Shout,
    Site,
}

impl Purpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shout => "shout",
            Self::Site => "site",
        }
    }
}

/// Price of a shout in CRO. `PS_SHOUT_PRICE_CRO` overrides; garbage falls back
/// to the default rather than making the feature free or unbuyable.
pub fn shout_price_cro() -> String {
    price_from_env("PS_SHOUT_PRICE_CRO", "10")
}

/// Price of hosting a published site, in CRO. `PS_PUBLISH_PRICE_CRO` overrides.
pub fn publish_price_cro() -> String {
    price_from_env("PS_PUBLISH_PRICE_CRO", "1")
}

fn price_from_env(key: &str, default: &str) -> String {
    match std::env::var(key) {
        Ok(v) if parse_amount(v.trim(), 18).is_ok() => v.trim().to_owned(),
        Ok(v) => {
            tracing::warn!(%key, value = %v, %default, "unparseable price override; using default");
            default.to_owned()
        }
        Err(_) => default.to_owned(),
    }
}

/// A price in wei. Callers pass the string form so the same value the client
/// was shown is the value enforced.
pub fn price_wei(cro: &str) -> u128 {
    // Both defaults parse, and `price_from_env` refuses overrides that do not.
    parse_amount(cro, 18).unwrap_or(u128::MAX)
}

/// `0x` + 64 hex, normalised to lowercase — the shape every explorer and RPC
/// agrees on. Anything else never reaches the RPC.
pub fn normalize_tx_hash(raw: &str) -> ApiResult<String> {
    let trimmed = raw.trim();
    let hex_part = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .ok_or_else(|| ApiError::field("txHash", "Invalid transaction hash format"))?;
    if hex_part.len() != 64 || !hex_part.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ApiError::field("txHash", "Invalid transaction hash format"));
    }
    Ok(format!("0x{}", hex_part.to_lowercase()))
}

/// Verify a payment and burn its hash, returning the paid amount in wei
/// (decimal string, for the ledger row the caller writes).
///
/// The single-use insert happens *after* verification but is what makes the
/// whole path race-safe: two requests presenting the same hash both verify,
/// and exactly one wins the `INSERT OR IGNORE`.
pub async fn verify_and_record(
    state: &AppState,
    caller: &WalletAddress,
    raw_tx_hash: &str,
    min_wei: u128,
    purpose: Purpose,
) -> ApiResult<String> {
    let tx_hash = normalize_tx_hash(raw_tx_hash)?;

    let wallet = crate::routes::misc::server_wallet();
    if wallet.trim().is_empty() {
        // Operator misconfiguration, not a client mistake — but the client is
        // the one who must stop retrying, so say what is actually wrong.
        return Err(ApiError::bad_request(
            "This server has no payment wallet configured (VITE_FRUITNATION_WALLET)",
        ));
    }

    let amount_wei = if state.cfg.verify_payments {
        let rpc = crate::routes::misc::configured_network().rpc_url;
        verify_on_chain(&rpc, &tx_hash, caller.as_str(), &wallet, min_wei).await?
    } else {
        // Offline mode: trust the format, still burn the hash below.
        min_wei
    };

    let hash_for_row = tx_hash.clone();
    let payer = caller.as_str().to_owned();
    let amount_str = amount_wei.to_string();
    let amount_for_row = amount_str.clone();
    let inserted = state
        .db
        .call(move |conn| {
            let changed = conn.execute(
                "INSERT OR IGNORE INTO payments (tx_hash, payer_address, amount_wei, purpose, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    hash_for_row,
                    payer,
                    amount_for_row,
                    purpose.as_str(),
                    crate::db::now_ms()
                ],
            )?;
            Ok(changed == 1)
        })
        .await?;
    if !inserted {
        return Err(ApiError::conflict(
            "This transaction has already been used for a paid feature",
        ));
    }

    let _ = state.log.append_audit(
        "payment_accepted",
        Some(caller),
        json!({ "txHash": tx_hash, "amountWei": amount_str, "purpose": purpose.as_str() }),
    );
    Ok(amount_str)
}

/// The two RPC calls, with every check spelled out. Takes the endpoint as a
/// parameter so the tests can point it at a mock chain.
pub async fn verify_on_chain(
    rpc_url: &str,
    tx_hash: &str,
    expected_from: &str,
    expected_to: &str,
    min_wei: u128,
) -> ApiResult<u128> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| ApiError::Internal(e.into()))?;

    let tx = rpc_call(&client, rpc_url, "eth_getTransactionByHash", tx_hash).await?;
    if tx.is_null() {
        return Err(ApiError::bad_request(
            "Transaction not found on chain — wait for it to propagate and try again",
        ));
    }

    let to = tx["to"].as_str().unwrap_or_default().to_lowercase();
    if to != expected_to.to_lowercase() {
        return Err(ApiError::bad_request(
            "This transaction does not pay the server's wallet",
        ));
    }
    let from = tx["from"].as_str().unwrap_or_default().to_lowercase();
    if from != expected_from.to_lowercase() {
        // The payment must come from the authenticated wallet: otherwise
        // anyone watching the chain could claim a stranger's transfer faster
        // than the stranger does.
        return Err(ApiError::bad_request(
            "This transaction was not sent by your wallet",
        ));
    }
    let value = tx["value"]
        .as_str()
        .and_then(|v| parse_hex_quantity(v).ok())
        .unwrap_or(0);
    if value < min_wei {
        return Err(ApiError::bad_request(
            "This transaction pays less than the required amount",
        ));
    }
    if tx["blockNumber"].is_null() {
        return Err(ApiError::bad_request(
            "Transaction is not yet mined — try again in a few seconds",
        ));
    }

    // A mined transfer can still have reverted; only the receipt says.
    let receipt = rpc_call(&client, rpc_url, "eth_getTransactionReceipt", tx_hash).await?;
    if receipt.is_null() {
        return Err(ApiError::bad_request(
            "Transaction receipt not available yet — try again in a few seconds",
        ));
    }
    if receipt["status"].as_str() != Some("0x1") {
        return Err(ApiError::bad_request("This transaction failed on chain"));
    }

    Ok(value)
}

async fn rpc_call(
    client: &reqwest::Client,
    rpc_url: &str,
    method: &str,
    tx_hash: &str,
) -> ApiResult<serde_json::Value> {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": [tx_hash] });
    let response = client
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            tracing::warn!(%rpc_url, %method, error = %e, "payment RPC unreachable");
            ApiError::bad_request("Could not reach the chain RPC to verify the payment — try again")
        })?
        .json::<serde_json::Value>()
        .await
        .map_err(|e| {
            tracing::warn!(%rpc_url, %method, error = %e, "payment RPC returned non-JSON");
            ApiError::bad_request("Chain RPC returned an unreadable reply — try again")
        })?;

    if let Some(err) = response.get("error") {
        tracing::warn!(%rpc_url, %method, %err, "payment RPC error");
        return Err(ApiError::bad_request(
            "Chain RPC rejected the verification request — try again",
        ));
    }
    Ok(response
        .get("result")
        .cloned()
        .unwrap_or(serde_json::Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_hashes_are_normalised_and_shape_checked() {
        let upper = format!("0x{}", "AB".repeat(32));
        assert_eq!(
            normalize_tx_hash(&upper).unwrap(),
            format!("0x{}", "ab".repeat(32))
        );
        assert_eq!(
            normalize_tx_hash(&format!("  0x{}  ", "1".repeat(64))).unwrap(),
            format!("0x{}", "1".repeat(64))
        );

        for bad in [
            "",
            "0x",
            &"a".repeat(66),                    // no 0x prefix
            &format!("0x{}", "a".repeat(63)),   // short
            &format!("0x{}", "a".repeat(65)),   // long
            &format!("0x{}zz", "a".repeat(62)), // not hex
        ] {
            assert!(normalize_tx_hash(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn prices_parse_to_wei() {
        assert_eq!(price_wei("10"), 10_000_000_000_000_000_000);
        assert_eq!(price_wei("1"), 1_000_000_000_000_000_000);
        assert_eq!(price_wei("0.5"), 500_000_000_000_000_000);
    }

    /// A tiny chain: answers `eth_getTransactionByHash` and
    /// `eth_getTransactionReceipt` with the values the test wires in.
    async fn mock_rpc(tx: serde_json::Value, receipt: serde_json::Value) -> String {
        use axum::routing::post;
        let app = axum::Router::new().route(
            "/",
            post(move |axum::Json(req): axum::Json<serde_json::Value>| {
                let tx = tx.clone();
                let receipt = receipt.clone();
                async move {
                    let result = match req["method"].as_str() {
                        Some("eth_getTransactionByHash") => tx,
                        Some("eth_getTransactionReceipt") => receipt,
                        _ => serde_json::Value::Null,
                    };
                    axum::Json(serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": result }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/")
    }

    const PAYER: &str = "0x1111111111111111111111111111111111111111";
    const OPERATOR: &str = "0x2222222222222222222222222222222222222222";

    fn good_tx() -> serde_json::Value {
        serde_json::json!({
            "from": PAYER,
            "to": OPERATOR,
            // 10 CRO
            "value": "0x8ac7230489e80000",
            "blockNumber": "0x10"
        })
    }

    fn good_receipt() -> serde_json::Value {
        serde_json::json!({ "status": "0x1" })
    }

    fn hash() -> String {
        format!("0x{}", "ab".repeat(32))
    }

    #[tokio::test]
    async fn a_confirmed_sufficient_payment_passes() {
        let rpc = mock_rpc(good_tx(), good_receipt()).await;
        let value = verify_on_chain(&rpc, &hash(), PAYER, OPERATOR, price_wei("10"))
            .await
            .unwrap();
        assert_eq!(value, price_wei("10"));
    }

    #[tokio::test]
    async fn the_operator_wallet_comparison_is_case_insensitive() {
        let rpc = mock_rpc(good_tx(), good_receipt()).await;
        let mixed = OPERATOR.to_uppercase().replace("0X", "0x");
        assert!(
            verify_on_chain(&rpc, &hash(), PAYER, &mixed, price_wei("10"))
                .await
                .is_ok(),
            "a checksummed VITE_FRUITNATION_WALLET must not break verification"
        );
    }

    #[tokio::test]
    async fn underpayment_wrong_recipient_and_wrong_sender_are_rejected() {
        let rpc = mock_rpc(good_tx(), good_receipt()).await;

        let err = verify_on_chain(&rpc, &hash(), PAYER, OPERATOR, price_wei("10") + 1)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("less than the required amount"));

        let err = verify_on_chain(&rpc, &hash(), PAYER, PAYER, price_wei("10"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not pay the server"));

        let err = verify_on_chain(&rpc, &hash(), OPERATOR, OPERATOR, price_wei("10"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not sent by your wallet"));
    }

    #[tokio::test]
    async fn unmined_missing_and_reverted_transactions_are_rejected() {
        let rpc = mock_rpc(serde_json::Value::Null, good_receipt()).await;
        let err = verify_on_chain(&rpc, &hash(), PAYER, OPERATOR, 1)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found on chain"));

        let mut pending = good_tx();
        pending["blockNumber"] = serde_json::Value::Null;
        let rpc = mock_rpc(pending, good_receipt()).await;
        let err = verify_on_chain(&rpc, &hash(), PAYER, OPERATOR, 1)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not yet mined"));

        let rpc = mock_rpc(good_tx(), serde_json::json!({ "status": "0x0" })).await;
        let err = verify_on_chain(&rpc, &hash(), PAYER, OPERATOR, 1)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("failed on chain"));
    }

    #[tokio::test]
    async fn an_unreachable_rpc_is_a_client_visible_retryable_error() {
        // Nothing listens here; the error must name the RPC, not be a 500.
        let err = verify_on_chain("http://127.0.0.1:1/", &hash(), PAYER, OPERATOR, 1)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Could not reach the chain RPC"));
    }
}
