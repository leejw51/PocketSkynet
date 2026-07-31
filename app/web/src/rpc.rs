//! Direct EVM JSON-RPC client for the wallet (balances, gas, send).
//!
//! The browser talks to the chain's public RPC endpoint itself — the same
//! architecture as the reference client and MetaMask. The server never sees a
//! private key, a nonce, or a raw transaction; its only blockchain role is
//! serving the network registry and recording published hashes.
//!
//! Only the request building and response parsing are testable on the host
//! (and they are, below); the actual `fetch` is wasm-only, mirroring how
//! `realtime.rs` splits transport from logic.

use pocketskynet_core::chain::{parse_hex_quantity, ChainError};
use pocketskynet_core::WalletAddress;
use serde_json::{json, Value};

/// A JSON-RPC failure, kept as a display string: the wallet UI can only ever
/// show it, and RPC error codes are not stable across providers anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError(pub String);

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<ChainError> for RpcError {
    fn from(e: ChainError) -> Self {
        RpcError(e.to_string())
    }
}

/// A transaction receipt, reduced to what the success screen shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    /// `true` when the tx executed successfully (`status == 0x1`).
    pub ok: bool,
    pub gas_used: u128,
    /// The created contract's address, on deployment receipts. The node is
    /// the authority here — reading it beats re-deriving CREATE addresses.
    pub contract_address: Option<String>,
}

/// Build the JSON-RPC 2.0 request envelope.
fn request_body(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })
}

/// Extract `result` from a JSON-RPC response, surfacing `error.message`.
fn parse_response(body: &Value) -> Result<Value, RpcError> {
    if let Some(err) = body.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("RPC error");
        return Err(RpcError(msg.to_owned()));
    }
    body.get("result")
        .cloned()
        .ok_or_else(|| RpcError("malformed RPC response: neither result nor error".to_owned()))
}

fn result_str(result: Value) -> Result<String, RpcError> {
    result
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| RpcError("RPC result was not a string".to_owned()))
}

/// A handle to one EVM JSON-RPC endpoint.
#[derive(Clone, PartialEq)]
pub struct EvmRpc {
    url: String,
}

impl EvmRpc {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_owned(),
        }
    }

    #[cfg(target_arch = "wasm32")]
    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let resp = gloo_net::http::Request::post(&self.url)
            .json(&request_body(method, params))
            .map_err(|e| RpcError(e.to_string()))?
            .send()
            .await
            .map_err(|e| RpcError(format!("network: {e}")))?;
        let body: Value = resp
            .json()
            .await
            .map_err(|e| RpcError(format!("bad RPC response: {e}")))?;
        parse_response(&body)
    }

    /// Host builds never reach the network; the stub keeps the crate's host
    /// test target linking, same pattern as `realtime.rs`.
    #[cfg(not(target_arch = "wasm32"))]
    async fn call(&self, _method: &str, _params: Value) -> Result<Value, RpcError> {
        Err(RpcError("RPC is wasm-only".to_owned()))
    }

    /// `eth_chainId` — used to verify the endpoint really is the network the
    /// registry claims before signing anything against its chain id.
    pub async fn chain_id(&self) -> Result<u128, RpcError> {
        let result = self.call("eth_chainId", json!([])).await?;
        Ok(parse_hex_quantity(&result_str(result)?)?)
    }

    /// Native balance in wei.
    pub async fn balance(&self, address: &WalletAddress) -> Result<u128, RpcError> {
        let result = self
            .call("eth_getBalance", json!([address.as_str(), "latest"]))
            .await?;
        Ok(parse_hex_quantity(&result_str(result)?)?)
    }

    /// Current gas price in wei.
    pub async fn gas_price(&self) -> Result<u128, RpcError> {
        let result = self.call("eth_gasPrice", json!([])).await?;
        Ok(parse_hex_quantity(&result_str(result)?)?)
    }

    /// The account nonce, including pending transactions — using `"latest"`
    /// here would silently replace an in-flight tx instead of queueing.
    pub async fn nonce(&self, address: &WalletAddress) -> Result<u128, RpcError> {
        let result = self
            .call(
                "eth_getTransactionCount",
                json!([address.as_str(), "pending"]),
            )
            .await?;
        Ok(parse_hex_quantity(&result_str(result)?)?)
    }

    /// Read-only contract call (`eth_call`), returns the raw hex output.
    pub async fn eth_call(&self, to: &str, data_hex: &str) -> Result<String, RpcError> {
        let result = self
            .call(
                "eth_call",
                json!([{ "to": to, "data": data_hex }, "latest"]),
            )
            .await?;
        result_str(result)
    }

    /// Ask the node what a call would cost. Used for ERC-20 transfers, where
    /// intrinsic gas is not the whole story (contract execution is on top).
    pub async fn estimate_gas(
        &self,
        from: &WalletAddress,
        to: &str,
        value: u128,
        data_hex: &str,
    ) -> Result<u128, RpcError> {
        let result = self
            .call(
                "eth_estimateGas",
                json!([{
                    "from": from.as_str(),
                    "to": to,
                    "value": pocketskynet_core::chain::to_hex_quantity(value),
                    "data": data_hex,
                }]),
            )
            .await?;
        Ok(parse_hex_quantity(&result_str(result)?)?)
    }

    /// Broadcast a signed transaction; returns the tx hash.
    pub async fn send_raw_transaction(&self, raw_hex: &str) -> Result<String, RpcError> {
        let result = self
            .call("eth_sendRawTransaction", json!([raw_hex]))
            .await?;
        result_str(result)
    }

    /// The receipt, or `None` while the tx is still pending.
    pub async fn receipt(&self, tx_hash: &str) -> Result<Option<Receipt>, RpcError> {
        let result = self
            .call("eth_getTransactionReceipt", json!([tx_hash]))
            .await?;
        if result.is_null() {
            return Ok(None);
        }
        let ok = result
            .get("status")
            .and_then(Value::as_str)
            .map(|s| parse_hex_quantity(s).map(|v| v == 1).unwrap_or(false))
            // Pre-Byzantium receipts have no status; treat presence as success.
            .unwrap_or(true);
        let gas_used = result
            .get("gasUsed")
            .and_then(Value::as_str)
            .and_then(|s| parse_hex_quantity(s).ok())
            .unwrap_or(0);
        let contract_address = result
            .get("contractAddress")
            .and_then(Value::as_str)
            .map(str::to_lowercase);
        Ok(Some(Receipt {
            ok,
            gas_used,
            contract_address,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_envelope_is_json_rpc_2() {
        let body = request_body("eth_chainId", json!([]));
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["method"], "eth_chainId");
        assert!(body["params"].as_array().unwrap().is_empty());
        assert!(body["id"].is_number());
    }

    #[test]
    fn a_result_is_extracted_and_an_error_is_surfaced() {
        let ok = json!({ "jsonrpc": "2.0", "id": 1, "result": "0x152" });
        assert_eq!(parse_response(&ok).unwrap(), json!("0x152"));

        let err = json!({ "jsonrpc": "2.0", "id": 1,
                          "error": { "code": -32000, "message": "insufficient funds" } });
        assert_eq!(
            parse_response(&err).unwrap_err(),
            RpcError("insufficient funds".to_owned())
        );

        // Neither key: a proxy mangled the response. Must be an error, not a
        // silent null that formats as a zero balance.
        assert!(parse_response(&json!({ "jsonrpc": "2.0", "id": 1 })).is_err());
    }

    #[test]
    fn error_without_a_message_still_reads_as_an_rpc_error() {
        let err = json!({ "error": { "code": -32000 } });
        assert_eq!(parse_response(&err).unwrap_err().0, "RPC error");
    }
}
