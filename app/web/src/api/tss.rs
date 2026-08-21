//! TSS (m-of-n threshold) wallet endpoints — `/api/tss/*`.
//!
//! Every ceremony runs on the server (docs/CRYPTO.md §15.3: the audited
//! CGGMP21 stack cannot build for wasm32), but custody is the user's: the
//! browser holds the passphrase-sealed **share files**, presents any `t` of
//! them per signing request, and the server persists nothing. None of these
//! calls exist in local mode — there is no server to run a ceremony.

use gloo_net::http::Method;
use serde::Deserialize;
use serde_json::Value;

use super::{ApiResult, Client};

/// A share file's cleartext header, read locally to name a picked file and
/// to validate a quorum before anything is sent. The file itself stays an
/// opaque [`Value`] — the browser never needs to understand the seal.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TssShareHeader {
    #[serde(rename = "type")]
    pub file_type: String,
    pub version: u32,
    pub address: String,
    /// `t`: shares a signing ceremony needs.
    pub threshold: u16,
    /// `n`: shares that exist.
    pub parties: u16,
    /// This file's party index, `0..n`.
    pub party_index: u16,
}

impl TssShareHeader {
    /// Parse a candidate file's header, `None` if it is not a share file.
    pub fn of(file: &Value) -> Option<Self> {
        let header: Self = serde_json::from_value(file.clone()).ok()?;
        (header.file_type == "pocketskynet-tss-share"
            && header.version == 1
            && header.party_index < header.parties)
            .then_some(header)
    }

    /// "2-of-3" — the shape, as the UI names it.
    pub fn shape(&self) -> String {
        format!("{}-of-{}", self.threshold, self.parties)
    }
}

/// `GET /api/tss/keygen/status` — a tagged progress record.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TssKeygenStatus {
    pub state: String,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// `POST /api/tss/keygen/collect` — the one-shot handover of the sealed
/// share files.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TssCollected {
    pub address: String,
    pub threshold: u16,
    pub parties: u16,
    /// The `n` sealed share files, opaque, ready to download verbatim.
    pub shares: Vec<Value>,
}

/// `POST /api/tss/sign` — a ceremony signature plus the E2EE identity the
/// same quorum unseals; everything a login needs in one round trip.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TssSignBundle {
    pub address: String,
    /// EIP-191 wire form, `0x` + 130 hex.
    pub signature: String,
    /// E2EE private key, `0x` + 64 hex. Held in memory only, like a pasted
    /// private key would be.
    pub encryption_key: String,
    /// Uncompressed public key, 130 hex chars, no `0x`.
    pub public_key: String,
    /// The wallet's ceremony signature over the key-binding message.
    pub binding_sig: String,
}

/// `POST /api/tss/sign-hash` — a recoverable signature over a 32-byte digest.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TssHashSignature {
    pub r: String,
    pub s: String,
    /// y-parity, 0 or 1 — the `recovery_id` for
    /// `LegacyTransaction::sign_with_signature`.
    pub v: u8,
}

impl TssHashSignature {
    /// The `r ‖ s` halves as the 64-byte array transaction assembly takes.
    pub fn rs_bytes(&self) -> Result<[u8; 64], String> {
        let r = hex::decode(self.r.trim_start_matches("0x")).map_err(|e| e.to_string())?;
        let s = hex::decode(self.s.trim_start_matches("0x")).map_err(|e| e.to_string())?;
        if r.len() != 32 || s.len() != 32 {
            return Err("r and s must be 32 bytes each".into());
        }
        let mut rs = [0u8; 64];
        rs[..32].copy_from_slice(&r);
        rs[32..].copy_from_slice(&s);
        Ok(rs)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeygenStarted {
    keygen_id: String,
}

impl Client {
    /// Start a t-of-n DKG; returns the `keygenId` capability that will
    /// collect its output. Poll [`Client::tss_keygen_status`] until `done`,
    /// then call [`Client::tss_keygen_collect`].
    pub async fn tss_keygen(
        &self,
        threshold: u16,
        parties: u16,
        passphrase: &str,
    ) -> ApiResult<String> {
        let started: KeygenStarted = self
            .send_json(
                Method::POST,
                "/api/tss/keygen",
                &serde_json::json!({
                    "threshold": threshold,
                    "parties": parties,
                    "passphrase": passphrase,
                }),
            )
            .await?;
        Ok(started.keygen_id)
    }

    pub async fn tss_keygen_status(&self) -> ApiResult<TssKeygenStatus> {
        self.send(Method::GET, "/api/tss/keygen/status").await
    }

    /// Collect the finished ceremony's sealed share files — exactly once;
    /// a second call finds nothing.
    pub async fn tss_keygen_collect(&self, keygen_id: &str) -> ApiResult<TssCollected> {
        self.send_json(
            Method::POST,
            "/api/tss/keygen/collect",
            &serde_json::json!({ "keygenId": keygen_id }),
        )
        .await
    }

    /// EIP-191 `personal_sign` by ceremony over any `t` share files, plus
    /// the E2EE identity — the login call.
    pub async fn tss_sign(
        &self,
        shares: &[Value],
        passphrase: &str,
        message: &str,
    ) -> ApiResult<TssSignBundle> {
        self.send_json(
            Method::POST,
            "/api/tss/sign",
            &serde_json::json!({
                "shares": shares,
                "passphrase": passphrase,
                "message": message,
            }),
        )
        .await
    }

    /// Threshold-sign a 32-byte digest — the transaction signer.
    pub async fn tss_sign_hash(
        &self,
        shares: &[Value],
        passphrase: &str,
        hash: &[u8; 32],
    ) -> ApiResult<TssHashSignature> {
        self.send_json(
            Method::POST,
            "/api/tss/sign-hash",
            &serde_json::json!({
                "shares": shares,
                "passphrase": passphrase,
                "hash": format!("0x{}", hex::encode(hash)),
            }),
        )
        .await
    }
}
