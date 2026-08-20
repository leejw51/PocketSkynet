//! TSS (m-of-n threshold) wallet endpoints — `/api/tss/*`.
//!
//! Every ceremony runs on the server (docs/CRYPTO.md §15.3: the audited
//! CGGMP21 stack cannot build for wasm32); the browser drives it and carries
//! the passphrase that authorizes it. None of these calls exist in local
//! mode — there is no server to hold a share.

use gloo_net::http::Method;
use pocketskynet_core::WalletAddress;
use serde::{Deserialize, Serialize};

use super::{ApiResult, Client};

/// One wallet's public header, listable without a passphrase.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TssWalletInfo {
    pub address: String,
    /// `t`: shares a signing ceremony needs.
    pub threshold: u16,
    /// `n`: shares that exist.
    pub parties: u16,
    pub created_at: u64,
}

impl TssWalletInfo {
    /// "2-of-3" — the shape, as the UI names it.
    pub fn shape(&self) -> String {
        format!("{}-of-{}", self.threshold, self.parties)
    }
}

#[derive(Deserialize)]
struct WalletList {
    wallets: Vec<TssWalletInfo>,
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

/// `POST /api/tss/session-keys` — the stored E2EE identity (§15.2).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TssSessionKeys {
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KeygenReq<'a> {
    threshold: u16,
    parties: u16,
    passphrase: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WalletReq<'a> {
    address: &'a str,
    passphrase: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SignReq<'a> {
    address: &'a str,
    passphrase: &'a str,
    message: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SignHashReq<'a> {
    address: &'a str,
    passphrase: &'a str,
    hash: String,
}

#[derive(Deserialize)]
struct SignResp {
    signature: String,
}

impl Client {
    /// The wallets this server holds — address and shape only.
    pub async fn tss_wallets(&self) -> ApiResult<Vec<TssWalletInfo>> {
        let list: WalletList = self.send(Method::GET, "/api/tss/wallets").await?;
        Ok(list.wallets)
    }

    /// Start a t-of-n DKG; poll [`Client::tss_keygen_status`] until `done`
    /// (carrying the new address) or `error`.
    pub async fn tss_keygen(
        &self,
        threshold: u16,
        parties: u16,
        passphrase: &str,
    ) -> ApiResult<()> {
        self.send_ok(
            Method::POST,
            "/api/tss/keygen",
            &KeygenReq {
                threshold,
                parties,
                passphrase,
            },
        )
        .await
    }

    pub async fn tss_keygen_status(&self) -> ApiResult<TssKeygenStatus> {
        self.send(Method::GET, "/api/tss/keygen/status").await
    }

    /// EIP-191 `personal_sign` by ceremony — the login-challenge signer.
    pub async fn tss_sign(
        &self,
        address: &WalletAddress,
        passphrase: &str,
        message: &str,
    ) -> ApiResult<String> {
        let resp: SignResp = self
            .send_json(
                Method::POST,
                "/api/tss/sign",
                &SignReq {
                    address: address.as_str(),
                    passphrase,
                    message,
                },
            )
            .await?;
        Ok(resp.signature)
    }

    /// Threshold-sign a 32-byte digest — the transaction signer.
    pub async fn tss_sign_hash(
        &self,
        address: &WalletAddress,
        passphrase: &str,
        hash: &[u8; 32],
    ) -> ApiResult<TssHashSignature> {
        self.send_json(
            Method::POST,
            "/api/tss/sign-hash",
            &SignHashReq {
                address: address.as_str(),
                passphrase,
                hash: format!("0x{}", hex::encode(hash)),
            },
        )
        .await
    }

    /// Release the stored E2EE identity against the passphrase.
    pub async fn tss_session_keys(
        &self,
        address: &WalletAddress,
        passphrase: &str,
    ) -> ApiResult<TssSessionKeys> {
        self.send_json(
            Method::POST,
            "/api/tss/session-keys",
            &WalletReq {
                address: address.as_str(),
                passphrase,
            },
        )
        .await
    }

    /// Destroy a wallet. Gated on the passphrase like every other touch.
    pub async fn tss_delete(&self, address: &WalletAddress, passphrase: &str) -> ApiResult<()> {
        self.send_ok(
            Method::POST,
            "/api/tss/delete",
            &WalletReq {
                address: address.as_str(),
                passphrase,
            },
        )
        .await
    }
}
