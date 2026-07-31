//! Multi-chain network registry and EVM transaction signing.
//!
//! PocketSkynet's wallet is chain-agnostic at the model layer: a [`Network`]
//! describes *any* chain (its RPC endpoint, native token, explorer, and token
//! list) and a [`ChainKind`] says which signing family it belongs to. Today
//! only [`ChainKind::Evm`] can actually sign and send — Cronos mainnet and
//! testnet ship in the built-in registry, with USDC as an ERC-20 on mainnet —
//! but Solana and Cardano entries exist so the active-network switcher, the
//! wire format, and the UI never need a schema change when those land.
//!
//! The EVM half implements exactly what a legacy (pre-EIP-1559) transfer
//! needs: minimal RLP, EIP-155 replay-protected signing over `k256`, ERC-20
//! `transfer`/`balanceOf` calldata, and the Cronos/Ethermint intrinsic-gas
//! rule (40/10 gas per data byte — four times Ethereum's, so reusing a
//! mainnet-Ethereum estimate here would underprice every tx with data).
//!
//! Amounts are `u128` wei throughout. That caps a single value at ~3.4e20
//! whole 18-decimal tokens — ten billion times the total CRO supply — and in
//! exchange the crate needs no bigint dependency and stays wasm-clean.

use k256::ecdsa::SigningKey;
use k256::SecretKey;
use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};

use crate::ids::WalletAddress;

/// The signing family a network belongs to.
///
/// This is deliberately *not* per-network: everything the client must branch
/// on (how to sign, how to derive addresses, what an explorer link looks
/// like) follows from the family, so adding e.g. `Base` or `Polygon` is a
/// registry entry, not new code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChainKind {
    /// secp256k1 + RLP + JSON-RPC — Ethereum, Cronos, and every EVM chain.
    Evm,
    /// ed25519 — registry entry only; signing is not implemented yet.
    Solana,
    /// ed25519/CBOR — registry entry only; signing is not implemented yet.
    Cardano,
}

/// A token contract on a network (ERC-20 for EVM chains).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Token {
    /// Ticker shown in the UI, e.g. `USDC`.
    pub symbol: String,
    /// Full display name, e.g. `USD Coin`.
    pub name: String,
    /// The contract address, `0x`-prefixed lowercase hex.
    pub contract: String,
    /// On-chain decimals. USDC is 6, not 18 — formatting with the wrong
    /// value is off by a factor of a trillion, so it lives next to the
    /// contract address rather than being assumed anywhere.
    pub decimals: u8,
}

/// One selectable network in the wallet's active-network switcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Network {
    /// Stable slug used as the persistence key, e.g. `cronos-mainnet`.
    pub id: String,
    /// Which signing family this network uses.
    pub kind: ChainKind,
    /// Human-readable name for the switcher.
    pub name: String,
    /// The EVM numeric chain id (EIP-155). `None` for non-EVM chains.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain_id: Option<u64>,
    /// JSON-RPC endpoint the client talks to directly.
    pub rpc_url: String,
    /// Block-explorer base URL, no trailing slash.
    pub explorer_url: String,
    /// Native token ticker (`CRO`, `TCRO`, `SOL`, …).
    pub symbol: String,
    /// Native token decimals (18 for EVM, 9 for Solana, 6 for Cardano).
    pub decimals: u8,
    /// Whether this is a test network — drives the warning ribbon.
    pub testnet: bool,
    /// Token contracts the wallet shows balances for.
    pub tokens: Vec<Token>,
}

impl Network {
    /// Whether the wallet can actually sign and send on this network today.
    ///
    /// Non-EVM entries appear in the switcher (that is the point of the
    /// registry) but their send UI is disabled rather than hidden, so the
    /// roadmap is visible and nothing pretends to work that does not.
    pub fn supports_send(&self) -> bool {
        matches!(self.kind, ChainKind::Evm)
    }

    /// Explorer link for a transaction hash.
    pub fn tx_url(&self, tx_hash: &str) -> String {
        format!("{}/tx/{}", self.explorer_url, tx_hash)
    }

    /// Explorer link for an address.
    pub fn address_url(&self, address: &str) -> String {
        format!("{}/address/{}", self.explorer_url, address)
    }
}

/// USDC on Cronos mainnet — the canonical bridged contract.
pub const USDC_CRONOS_MAINNET: &str = "0xc21223249ca28397b4b6541dffaecc539bff0c59";

/// The built-in network registry, first entry is the default.
///
/// Cronos testnet leads deliberately: a fresh install should not be one
/// mis-click away from spending mainnet funds. The server re-serves this
/// list over `GET /api/networks` so future deployments can override it
/// without a client release.
pub fn builtin_networks() -> Vec<Network> {
    vec![
        // Mainnet first, and therefore the default: the client falls back to
        // the registry's first entry when nothing is persisted. Ordering is
        // the whole mechanism — there is no separate "default" flag to keep
        // in step with it.
        Network {
            id: "cronos-mainnet".into(),
            kind: ChainKind::Evm,
            name: "Cronos Mainnet".into(),
            chain_id: Some(25),
            rpc_url: "https://evm.cronos.org".into(),
            explorer_url: "https://explorer.cronos.org".into(),
            symbol: "CRO".into(),
            decimals: 18,
            testnet: false,
            tokens: vec![Token {
                symbol: "USDC".into(),
                name: "USD Coin".into(),
                contract: USDC_CRONOS_MAINNET.into(),
                decimals: 6,
            }],
        },
        Network {
            id: "cronos-testnet".into(),
            kind: ChainKind::Evm,
            name: "Cronos Testnet".into(),
            chain_id: Some(338),
            rpc_url: "https://evm-t3.cronos.org".into(),
            explorer_url: "https://explorer.cronos.org/testnet".into(),
            symbol: "TCRO".into(),
            decimals: 18,
            testnet: true,
            tokens: vec![],
        },
        Network {
            id: "solana-mainnet".into(),
            kind: ChainKind::Solana,
            name: "Solana Mainnet".into(),
            chain_id: None,
            rpc_url: "https://api.mainnet-beta.solana.com".into(),
            explorer_url: "https://explorer.solana.com".into(),
            symbol: "SOL".into(),
            decimals: 9,
            testnet: false,
            tokens: vec![],
        },
        Network {
            id: "cardano-mainnet".into(),
            kind: ChainKind::Cardano,
            name: "Cardano Mainnet".into(),
            chain_id: None,
            rpc_url: String::new(),
            explorer_url: "https://cardanoscan.io".into(),
            symbol: "ADA".into(),
            decimals: 6,
            testnet: false,
            tokens: vec![],
        },
    ]
}

/// Everything that can go wrong between a typed amount and a raw transaction.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChainError {
    /// The amount string is not a plain decimal number.
    #[error("invalid amount")]
    InvalidAmount,
    /// The amount has more fractional digits than the token has decimals.
    #[error("too many decimal places")]
    TooManyDecimals,
    /// The value does not fit in 128 bits.
    #[error("amount out of range")]
    Overflow,
    /// An RPC quantity was not `0x`-prefixed hex.
    #[error("invalid quantity")]
    InvalidQuantity,
    /// ECDSA signing failed (never expected with a valid key).
    #[error("signing failed")]
    SigningFailed,
    /// This session has no key on this device — it signed in with a browser
    /// wallet, which signs through its own provider rather than here.
    #[error("no signing key on this device")]
    NoSigningKey,
}

/// Parse a human decimal string (`"1.5"`, `"0.000001"`, `"42"`) into base
/// units at `decimals`.
///
/// Float parsing is deliberately absent: `0.1` CRO is not representable in
/// an `f64` of wei, and a wallet that rounds the user's amount is broken in
/// the worst possible way. This is pure string/integer arithmetic.
pub fn parse_amount(input: &str, decimals: u8) -> Result<u128, ChainError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(ChainError::InvalidAmount);
    }
    let (whole, frac) = match trimmed.split_once('.') {
        Some((w, f)) => (w, f),
        None => (trimmed, ""),
    };
    // "1." and ".5" are fine; "." and "1.2.3" are not.
    if (whole.is_empty() && frac.is_empty()) || frac.contains('.') {
        return Err(ChainError::InvalidAmount);
    }
    if !whole.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        return Err(ChainError::InvalidAmount);
    }
    if frac.len() > decimals as usize {
        // Truncating silently would send a different amount than displayed.
        return Err(ChainError::TooManyDecimals);
    }

    let scale = 10u128
        .checked_pow(decimals as u32)
        .ok_or(ChainError::Overflow)?;
    let whole: u128 = if whole.is_empty() {
        0
    } else {
        whole.parse().map_err(|_| ChainError::Overflow)?
    };
    let mut frac_units: u128 = if frac.is_empty() {
        0
    } else {
        frac.parse().map_err(|_| ChainError::Overflow)?
    };
    frac_units *= 10u128.pow((decimals as usize - frac.len()) as u32);

    whole
        .checked_mul(scale)
        .and_then(|w| w.checked_add(frac_units))
        .ok_or(ChainError::Overflow)
}

/// Format base units back into a trimmed decimal string.
///
/// The inverse of [`parse_amount`]: no scientific notation, no trailing
/// zeros, no locale. `1500000000000000000` at 18 decimals renders `"1.5"`.
pub fn format_amount(value: u128, decimals: u8) -> String {
    let scale = 10u128.pow(decimals as u32);
    let whole = value / scale;
    let frac = value % scale;
    if frac == 0 {
        return whole.to_string();
    }
    let frac = format!("{frac:0>width$}", width = decimals as usize);
    format!("{whole}.{}", frac.trim_end_matches('0'))
}

/// Parse a JSON-RPC `QUANTITY` (`"0x1b4"`) into a `u128`.
pub fn parse_hex_quantity(input: &str) -> Result<u128, ChainError> {
    let stripped = input
        .strip_prefix("0x")
        .or_else(|| input.strip_prefix("0X"))
        .ok_or(ChainError::InvalidQuantity)?;
    if stripped.is_empty() {
        return Err(ChainError::InvalidQuantity);
    }
    u128::from_str_radix(stripped, 16).map_err(|_| ChainError::InvalidQuantity)
}

/// Render a `u128` as a minimal JSON-RPC `QUANTITY` (`"0x0"`, `"0x1b4"`).
pub fn to_hex_quantity(value: u128) -> String {
    format!("0x{value:x}")
}

/// Decode a 32-byte ABI `uint256` return (e.g. `balanceOf`) into a `u128`.
///
/// Values above 2^128 surface as [`ChainError::Overflow`] instead of being
/// silently truncated — a balance that large is either a scam token or a
/// decoding bug, and both deserve an error over a plausible-looking number.
pub fn decode_abi_uint(output: &str) -> Result<u128, ChainError> {
    let stripped = output
        .strip_prefix("0x")
        .or_else(|| output.strip_prefix("0X"))
        .ok_or(ChainError::InvalidQuantity)?;
    // eth_call on a non-contract returns "0x" — treat as zero, matching what
    // a wallet should show for a token that is not deployed on this chain.
    if stripped.is_empty() {
        return Ok(0);
    }
    let bytes = hex::decode(stripped).map_err(|_| ChainError::InvalidQuantity)?;
    if bytes.len() != 32 {
        return Err(ChainError::InvalidQuantity);
    }
    if bytes[..16].iter().any(|&b| b != 0) {
        return Err(ChainError::Overflow);
    }
    let mut tail = [0u8; 16];
    tail.copy_from_slice(&bytes[16..]);
    Ok(u128::from_be_bytes(tail))
}

/// ERC-20 `transfer(address,uint256)` calldata.
///
/// Selector `a9059cbb` + the recipient left-padded to 32 bytes + the amount
/// as a 32-byte big-endian integer. This is the entire ABI surface the
/// wallet needs; a full ABI encoder would be 100× the code for the same tx.
pub fn erc20_transfer_data(to: &WalletAddress, amount: u128) -> Vec<u8> {
    let mut data = Vec::with_capacity(4 + 32 + 32);
    data.extend_from_slice(&[0xa9, 0x05, 0x9c, 0xbb]);
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(&to.to_bytes());
    data.extend_from_slice(&[0u8; 16]);
    data.extend_from_slice(&amount.to_be_bytes());
    data
}

/// ERC-20 `balanceOf(address)` calldata (selector `70a08231`).
pub fn erc20_balance_of_data(owner: &WalletAddress) -> Vec<u8> {
    let mut data = Vec::with_capacity(4 + 32);
    data.extend_from_slice(&[0x70, 0xa0, 0x82, 0x31]);
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(&owner.to_bytes());
    data
}

/// The Cronos/Ethermint intrinsic-gas rule with the reference client's 20%
/// safety margin: `21000 + ceil((40·nonzero + 10·zero) × 1.2)`.
///
/// Ethereum mainnet charges 16/4 per byte; Cronos charges 40/10. Using the
/// Ethereum figures produces transactions that are *accepted* by estimation
/// and then rejected at execution, which is why this constant lives in
/// tested code rather than in a UI default. Validated against the seven
/// vectors in the reference PROTOCOL.md.
pub fn intrinsic_gas(data: &[u8]) -> u64 {
    let data_gas: u64 = data.iter().map(|&b| if b == 0 { 10 } else { 40 }).sum();
    // ceil(data_gas × 1.2) in integer arithmetic: ceil(6·g / 5).
    21_000 + (data_gas * 6).div_ceil(5)
}

/// An unsigned legacy (pre-EIP-1559) EVM transaction.
///
/// Cronos accepts type-0 transactions with a plain `gasPrice`, and the
/// reference client never sends anything else, so neither do we — EIP-1559
/// fee fields would be dead weight on a chain that ignores them.
#[derive(Debug, Clone)]
pub struct LegacyTransaction {
    /// The sender's account nonce (`eth_getTransactionCount`, "pending").
    pub nonce: u128,
    /// Gas price in wei.
    pub gas_price: u128,
    /// Gas limit.
    pub gas_limit: u128,
    /// Recipient. `None` would deploy a contract — the wallet never does,
    /// but the RLP encoding is defined for it and the type says so.
    pub to: Option<WalletAddress>,
    /// Value in wei.
    pub value: u128,
    /// Calldata (empty for a plain transfer, ERC-20 calldata for tokens).
    pub data: Vec<u8>,
    /// EIP-155 chain id — 25 for Cronos mainnet, 338 for testnet.
    pub chain_id: u64,
}

/// A signed transaction ready for `eth_sendRawTransaction`.
#[derive(Debug, Clone)]
pub struct SignedTransaction {
    /// The full RLP-encoded signed transaction.
    pub raw: Vec<u8>,
    /// `keccak256(raw)` — the transaction hash the explorer will show.
    pub hash: [u8; 32],
}

impl SignedTransaction {
    /// The raw transaction as `0x`-prefixed hex, the wire form RPC expects.
    pub fn raw_hex(&self) -> String {
        format!("0x{}", hex::encode(&self.raw))
    }

    /// The transaction hash as `0x`-prefixed hex.
    pub fn hash_hex(&self) -> String {
        format!("0x{}", hex::encode(self.hash))
    }
}

impl LegacyTransaction {
    /// The EIP-155 signing digest:
    /// `keccak256(rlp([nonce, gasPrice, gasLimit, to, value, data, chainId, 0, 0]))`.
    ///
    /// The trailing `(chainId, 0, 0)` is what makes a Cronos signature
    /// meaningless on Ethereum and vice versa — omitting it (pre-155 style)
    /// would produce replayable transactions.
    pub fn sighash(&self) -> [u8; 32] {
        let payload = self.rlp_encode(Some((u128::from(self.chain_id), &[], &[])));
        Keccak256::digest(&payload).into()
    }

    /// Sign with `secret`, producing the raw transaction and its hash.
    ///
    /// `v = chainId·2 + 35 + recoveryId` per EIP-155. RFC 6979 makes the
    /// result deterministic, which is what lets the canonical EIP-155 test
    /// vector pin this whole pipeline byte-for-byte.
    pub fn sign(&self, secret: &SecretKey) -> Result<SignedTransaction, ChainError> {
        let signing_key = SigningKey::from(secret);
        let (signature, recovery_id) = signing_key
            .sign_prehash_recoverable(&self.sighash())
            .map_err(|_| ChainError::SigningFailed)?;

        let v = self.chain_id * 2 + 35 + u64::from(recovery_id.to_byte());
        let bytes = signature.to_bytes();
        let r = strip_leading_zeros(&bytes[..32]);
        let s = strip_leading_zeros(&bytes[32..]);

        let raw = self.rlp_encode(Some((u128::from(v), r, s)));
        let hash = Keccak256::digest(&raw).into();
        Ok(SignedTransaction { raw, hash })
    }

    /// RLP-encode the nine-field list. `tail` carries either the EIP-155
    /// placeholder `(chainId, "", "")` for the sighash or `(v, r, s)` for
    /// the final transaction; the six leading fields are identical in both.
    fn rlp_encode(&self, tail: Option<(u128, &[u8], &[u8])>) -> Vec<u8> {
        let mut items: Vec<Vec<u8>> = Vec::with_capacity(9);
        items.push(rlp_uint(self.nonce));
        items.push(rlp_uint(self.gas_price));
        items.push(rlp_uint(self.gas_limit));
        items.push(rlp_bytes(
            self.to
                .as_ref()
                .map(|a| a.to_bytes().to_vec())
                .as_deref()
                .unwrap_or(&[]),
        ));
        items.push(rlp_uint(self.value));
        items.push(rlp_bytes(&self.data));
        if let Some((v, r, s)) = tail {
            items.push(rlp_uint(v));
            items.push(rlp_bytes(r));
            items.push(rlp_bytes(s));
        }
        rlp_list(&items)
    }
}

/// Big-endian bytes of an integer with leading zeros removed; zero is the
/// empty string in RLP, not `0x00`.
fn strip_leading_zeros(bytes: &[u8]) -> &[u8] {
    let start = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
    &bytes[start..]
}

/// RLP-encode an unsigned integer (as its minimal big-endian bytes).
fn rlp_uint(value: u128) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    rlp_bytes(strip_leading_zeros(&bytes))
}

/// RLP-encode a byte string.
fn rlp_bytes(bytes: &[u8]) -> Vec<u8> {
    match bytes {
        // A single byte below 0x80 is its own encoding.
        [b] if *b < 0x80 => vec![*b],
        _ if bytes.len() <= 55 => {
            let mut out = Vec::with_capacity(1 + bytes.len());
            out.push(0x80 + bytes.len() as u8);
            out.extend_from_slice(bytes);
            out
        }
        _ => {
            let len_bytes = usize_be(bytes.len());
            let mut out = Vec::with_capacity(1 + len_bytes.len() + bytes.len());
            out.push(0xb7 + len_bytes.len() as u8);
            out.extend_from_slice(&len_bytes);
            out.extend_from_slice(bytes);
            out
        }
    }
}

/// RLP-encode a list of already-encoded items.
fn rlp_list(items: &[Vec<u8>]) -> Vec<u8> {
    let payload_len: usize = items.iter().map(Vec::len).sum();
    let mut out;
    if payload_len <= 55 {
        out = Vec::with_capacity(1 + payload_len);
        out.push(0xc0 + payload_len as u8);
    } else {
        let len_bytes = usize_be(payload_len);
        out = Vec::with_capacity(1 + len_bytes.len() + payload_len);
        out.push(0xf7 + len_bytes.len() as u8);
        out.extend_from_slice(&len_bytes);
    }
    for item in items {
        out.extend_from_slice(item);
    }
    out
}

/// Minimal big-endian encoding of a length.
fn usize_be(value: usize) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    strip_leading_zeros(&bytes).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- registry ----------------------------------------------------------

    #[test]
    fn builtin_registry_has_cronos_both_ways_and_usdc_on_mainnet() {
        let nets = builtin_networks();
        let testnet = nets.iter().find(|n| n.id == "cronos-testnet").unwrap();
        let mainnet = nets.iter().find(|n| n.id == "cronos-mainnet").unwrap();

        assert_eq!(testnet.chain_id, Some(338));
        assert!(testnet.testnet);
        assert_eq!(testnet.symbol, "TCRO");

        assert_eq!(mainnet.chain_id, Some(25));
        assert!(!mainnet.testnet);
        assert_eq!(mainnet.tokens[0].symbol, "USDC");
        assert_eq!(mainnet.tokens[0].contract, USDC_CRONOS_MAINNET);
        assert_eq!(mainnet.tokens[0].decimals, 6);
    }

    #[test]
    fn default_network_is_cronos_mainnet() {
        // Deliberate, and a reversal: this used to assert a testnet default so
        // a fresh install was never one click from real funds. The product
        // wants mainnet as the everyday chain, so the safety moved rather than
        // vanished — the client badges mainnet in the ribbon and the send flow
        // keeps its tiered confirmations. What must stay true is that the
        // first entry *is* the default, since nothing else marks one.
        let nets = builtin_networks();
        assert_eq!(nets[0].id, "cronos-mainnet");
        assert!(!nets[0].testnet);
        assert_eq!(nets[0].chain_id, Some(25));
        // The testnet has to remain reachable, or there is no safe place to
        // try a send before doing it with real money.
        assert!(nets.iter().any(|n| n.testnet && n.chain_id == Some(338)));
    }

    #[test]
    fn only_evm_networks_support_send() {
        for net in builtin_networks() {
            assert_eq!(net.supports_send(), matches!(net.kind, ChainKind::Evm));
        }
    }

    #[test]
    fn explorer_urls_compose_without_double_slashes() {
        // Pinned to the testnet entry by id rather than by position: the
        // pathful explorer URL is what makes double slashes possible, and it
        // is the testnet that has one.
        let nets = builtin_networks();
        let net = nets.iter().find(|n| n.id == "cronos-testnet").unwrap();
        assert_eq!(
            net.tx_url("0xabc"),
            "https://explorer.cronos.org/testnet/tx/0xabc"
        );
        assert_eq!(
            net.address_url("0xdef"),
            "https://explorer.cronos.org/testnet/address/0xdef"
        );
    }

    #[test]
    fn networks_serialize_camel_case_for_the_wire() {
        let nets = builtin_networks();
        let testnet = nets.iter().find(|n| n.id == "cronos-testnet").unwrap();
        let json = serde_json::to_value(testnet).unwrap();
        assert_eq!(json["chainId"], 338);
        assert_eq!(json["rpcUrl"], "https://evm-t3.cronos.org");
        assert_eq!(json["kind"], "evm");
        // Non-EVM entries omit chainId entirely rather than sending null.
        let sol = serde_json::to_value(
            builtin_networks()
                .into_iter()
                .find(|n| n.kind == ChainKind::Solana)
                .unwrap(),
        )
        .unwrap();
        assert!(sol.get("chainId").is_none());
    }

    // ---- amounts -----------------------------------------------------------

    #[test]
    fn parse_amount_handles_the_obvious_shapes() {
        assert_eq!(parse_amount("1", 18).unwrap(), 10u128.pow(18));
        assert_eq!(parse_amount("1.5", 18).unwrap(), 15 * 10u128.pow(17));
        assert_eq!(parse_amount("0.000001", 6).unwrap(), 1);
        assert_eq!(parse_amount(".5", 2).unwrap(), 50);
        assert_eq!(parse_amount("2.", 2).unwrap(), 200);
        assert_eq!(parse_amount(" 42 ", 0).unwrap(), 42);
    }

    #[test]
    fn parse_amount_rejects_garbage_and_precision_loss() {
        for bad in ["", ".", "1.2.3", "1e18", "-1", "0x10", "1,5"] {
            assert!(parse_amount(bad, 18).is_err(), "accepted {bad:?}");
        }
        // 7 fractional digits into a 6-decimal token would silently truncate.
        assert_eq!(
            parse_amount("0.0000001", 6).err(),
            Some(ChainError::TooManyDecimals)
        );
        assert_eq!(
            parse_amount(&format!("{}", u128::MAX), 18).err(),
            Some(ChainError::Overflow)
        );
    }

    #[test]
    fn format_amount_is_the_inverse_of_parse() {
        for (s, d) in [("1.5", 18u8), ("0.000001", 6), ("42", 0), ("0", 18)] {
            assert_eq!(format_amount(parse_amount(s, d).unwrap(), d), s);
        }
        // Trailing zeros are trimmed, whole numbers drop the point.
        assert_eq!(format_amount(1_500_000, 6), "1.5");
        assert_eq!(format_amount(10u128.pow(18), 18), "1");
    }

    #[test]
    fn hex_quantities_round_trip() {
        assert_eq!(parse_hex_quantity("0x1b4").unwrap(), 436);
        assert_eq!(parse_hex_quantity("0x0").unwrap(), 0);
        assert_eq!(to_hex_quantity(436), "0x1b4");
        assert_eq!(to_hex_quantity(0), "0x0");
        for bad in ["", "0x", "1b4", "0xzz"] {
            assert!(parse_hex_quantity(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn abi_uint_decoding_checks_width_and_overflow() {
        let small = format!("0x{}{:032x}", "0".repeat(32), 1_000_000u128);
        assert_eq!(decode_abi_uint(&small).unwrap(), 1_000_000);
        // eth_call against a missing contract yields "0x" — that is zero.
        assert_eq!(decode_abi_uint("0x").unwrap(), 0);
        // A value above 2^128 must error, not truncate.
        let huge = format!("0x{}{}", "01", "0".repeat(62));
        assert_eq!(decode_abi_uint(&huge).err(), Some(ChainError::Overflow));
        // Wrong length is a decoding bug, not a balance.
        assert!(decode_abi_uint("0x1234").is_err());
    }

    // ---- calldata ----------------------------------------------------------

    #[test]
    fn erc20_transfer_calldata_matches_the_abi_layout() {
        let to = WalletAddress::new("0x3535353535353535353535353535353535353535").unwrap();
        let data = erc20_transfer_data(&to, 1_000_000);
        assert_eq!(
            hex::encode(&data),
            "a9059cbb\
             0000000000000000000000003535353535353535353535353535353535353535\
             00000000000000000000000000000000000000000000000000000000000f4240"
                .replace(char::is_whitespace, "")
        );
    }

    #[test]
    fn erc20_balance_of_calldata_matches_the_abi_layout() {
        let owner = WalletAddress::new("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266").unwrap();
        let data = erc20_balance_of_data(&owner);
        assert_eq!(
            hex::encode(&data),
            "70a08231000000000000000000000000f39fd6e51aad88f6f4ce6ab8827279cfffb92266"
        );
    }

    // ---- intrinsic gas (PROTOCOL.md vectors 1–7) ---------------------------

    #[test]
    fn intrinsic_gas_matches_all_seven_reference_vectors() {
        assert_eq!(intrinsic_gas(b""), 21_000); // 1: empty
        assert_eq!(intrinsic_gas(b"ok"), 21_096); // 2
        assert_eq!(intrinsic_gas(b"hello"), 21_240); // 3
        assert_eq!(intrinsic_gas(&[0xde, 0xad, 0xbe, 0xef]), 21_192); // 4
        assert_eq!(intrinsic_gas(&[0x00, 0x61]), 21_060); // 5: mixed
        assert_eq!(intrinsic_gas("한".as_bytes()), 21_144); // 6: UTF-8
        assert_eq!(intrinsic_gas(&[0xab; 32]), 22_536); // 7: 32-byte hash
    }

    // ---- RLP + EIP-155 signing ---------------------------------------------

    /// The canonical EIP-155 example from the spec: the one transaction whose
    /// signed bytes every correct implementation must reproduce exactly.
    fn eip155_example() -> LegacyTransaction {
        LegacyTransaction {
            nonce: 9,
            gas_price: 20_000_000_000,
            gas_limit: 21_000,
            to: Some(WalletAddress::new("0x3535353535353535353535353535353535353535").unwrap()),
            value: 1_000_000_000_000_000_000,
            data: vec![],
            chain_id: 1,
        }
    }

    fn eip155_key() -> SecretKey {
        SecretKey::from_slice(&[0x46u8; 32]).unwrap()
    }

    #[test]
    fn sighash_matches_the_eip155_example() {
        assert_eq!(
            hex::encode(eip155_example().sighash()),
            "daf5a779ae972f972197303d7b574746c7ef83eadac0f2791ad23db92e4c8e53"
        );
    }

    #[test]
    fn signed_bytes_match_the_eip155_example() {
        let signed = eip155_example().sign(&eip155_key()).unwrap();
        assert_eq!(
            signed.raw_hex(),
            "0xf86c098504a817c800825208943535353535353535353535353535353535353535880de0b6b3a76400008025a028ef61340bd939bc2195fe537567866003e1a15d3c71ff63e1590620aa636276a067cbe9d8997f761aecb703304b3800ccf555c9f3dc64214b297fb1966a3b6d83"
        );
        // The hash is keccak of exactly those bytes.
        assert_eq!(
            signed.hash_hex(),
            format!("0x{}", hex::encode(Keccak256::digest(&signed.raw)))
        );
    }

    #[test]
    fn v_encodes_the_chain_id_per_eip155() {
        // On Cronos mainnet (25), v must be 85 or 86 — never 27/28.
        let mut tx = eip155_example();
        tx.chain_id = 25;
        let signed = tx.sign(&eip155_key()).unwrap();
        // v is the 7th RLP item; for a tx this small it is a single byte we
        // can find by re-signing on chain 1 and diffing, but asserting via
        // the raw bytes is simpler: 25*2+35 = 85 (0x55), +1 = 86 (0x56).
        let raw = hex::encode(&signed.raw);
        assert!(
            raw.contains("55a0") || raw.contains("56a0"),
            "v byte should be 0x55/0x56 followed by the 32-byte r marker, got {raw}"
        );
    }

    #[test]
    fn signing_is_deterministic() {
        let a = eip155_example().sign(&eip155_key()).unwrap();
        let b = eip155_example().sign(&eip155_key()).unwrap();
        assert_eq!(a.raw, b.raw);
        assert_eq!(a.hash, b.hash);
    }

    #[test]
    fn contract_creation_encodes_an_empty_to_field() {
        let mut tx = eip155_example();
        tx.to = None;
        // 0x80 (empty string) must appear where the address was; the encoding
        // must not panic or emit twenty zero bytes.
        let unsigned = tx.rlp_encode(Some((1, &[], &[])));
        assert!(unsigned.len() < eip155_example().rlp_encode(Some((1, &[], &[]))).len());
    }

    #[test]
    fn rlp_primitives_match_the_spec_examples() {
        assert_eq!(rlp_bytes(b""), vec![0x80]); // empty string
        assert_eq!(rlp_bytes(&[0x00]), vec![0x00]); // single zero byte...
        assert_eq!(rlp_bytes(&[0x7f]), vec![0x7f]); // single byte < 0x80
        assert_eq!(rlp_bytes(&[0x80]), vec![0x81, 0x80]); // single byte >= 0x80
        assert_eq!(rlp_bytes(b"dog"), vec![0x83, b'd', b'o', b'g']);
        assert_eq!(rlp_uint(0), vec![0x80]); // zero is the empty string
        assert_eq!(rlp_uint(15), vec![0x0f]);
        assert_eq!(rlp_uint(1024), vec![0x82, 0x04, 0x00]);
        assert_eq!(rlp_list(&[]), vec![0xc0]); // empty list
                                               // 56-byte string exercises the long-form header.
        let long = vec![b'a'; 56];
        let encoded = rlp_bytes(&long);
        assert_eq!(&encoded[..2], &[0xb8, 56]);
        assert_eq!(encoded.len(), 58);
    }
}
