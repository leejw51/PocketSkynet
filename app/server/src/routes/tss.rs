//! `/api/tss/*` — the m-of-n threshold wallet (docs/CRYPTO.md §15).
//!
//! Every ceremony runs inside this process; the browser cannot hold a share
//! (§15.3). The passphrase is the gate on every endpoint that touches key
//! material — these routes are deliberately **unauthenticated**, because a
//! TSS wallet must sign its login challenge *before* it has a JWT. What keeps
//! that honest: nothing here returns anything an attacker without the
//! passphrase could use (the wallet list is public identity, everything else
//! opens the seal first), every request re-derives the KEK from scratch
//! (600k PBKDF2 iterations — a built-in brute-force throttle), and the
//! general per-IP rate limit meters the door.

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use pocketskynet_core::{eip191, keys, WalletAddress};
use pocketskynet_tss::{dkg, eth, sign, store, Share, TssError};
use serde::Deserialize;
use serde_json::json;

use crate::error::{ApiError, ApiResult};
use crate::validate::ValidJson;
use crate::AppState;

/// Shortest passphrase a wallet may be sealed under. The passphrase is the
/// only thing between a stolen wallet file and the key shares, so a trivial
/// one defeats the whole custody model.
const MIN_PASSPHRASE: usize = 8;

/// One DKG at a time, observable while it runs.
///
/// A sync lock, not an async one: every touch is a short read-or-write from
/// a handler or the keygen task's phase callback (which is not async).
#[derive(Debug, Default)]
pub struct TssState {
    keygen: std::sync::RwLock<KeygenStatus>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum KeygenStatus {
    #[default]
    Idle,
    GeneratingPrimes,
    RunningProtocol,
    /// DKG done; minting the E2EE keypair and ceremony-signing its binding.
    Binding,
    Done {
        address: String,
    },
    Error {
        error: String,
    },
}

impl KeygenStatus {
    fn in_progress(&self) -> bool {
        matches!(
            self,
            KeygenStatus::GeneratingPrimes | KeygenStatus::RunningProtocol | KeygenStatus::Binding
        )
    }
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tss/wallets", get(list_wallets))
        .route("/tss/keygen", post(keygen))
        .route("/tss/keygen/status", get(keygen_status))
        .route("/tss/sign", post(sign_message))
        .route("/tss/sign-hash", post(sign_hash))
        .route("/tss/session-keys", post(session_keys))
        .route("/tss/delete", post(delete_wallet))
}

/// Map a TSS failure onto the API error envelope. The passphrase case is a
/// 401 — it is an authentication failure in every sense that matters.
fn api_error(e: TssError) -> ApiError {
    match e {
        TssError::BadPassphrase => ApiError::unauthorized("Wrong passphrase"),
        TssError::WalletNotFound => ApiError::not_found("No TSS wallet with that address"),
        TssError::WalletExists => ApiError::conflict("A TSS wallet with that address exists"),
        TssError::InvalidParams { .. } | TssError::InvalidSigners(_) => {
            ApiError::bad_request(e.to_string())
        }
        other => ApiError::Internal(anyhow::anyhow!(other)),
    }
}

fn parse_address(raw: Option<&str>) -> ApiResult<WalletAddress> {
    crate::validate::wallet_address("address", raw)
}

fn parse_passphrase(raw: Option<&str>) -> ApiResult<String> {
    match raw {
        Some(p) if !p.is_empty() => Ok(p.to_owned()),
        _ => Err(ApiError::bad_request("passphrase is required")),
    }
}

/// Open a wallet off the async runtime — PBKDF2 alone is ~a third of a
/// second of pure CPU, which is exactly what must not run on a worker the
/// realtime hub shares.
async fn open_wallet(
    state: &AppState,
    address: &WalletAddress,
    passphrase: &str,
) -> ApiResult<(store::WalletInfo, store::WalletSecrets)> {
    let dir = state.cfg.tss_dir();
    let address = address.clone();
    let passphrase = passphrase.to_owned();
    tokio::task::spawn_blocking(move || store::open_wallet(&dir, &address, &passphrase))
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("open task panicked: {e}")))?
        .map_err(api_error)
}

/// The first `t` shares as a signer set. Which quorum signs is invisible in
/// the output by construction — any `t` produce a signature for the same
/// address — so the server simply uses the lowest indexes.
fn quorum(info: &store::WalletInfo, secrets: &store::WalletSecrets) -> Vec<(u16, Share)> {
    let mut shares: Vec<(u16, Share)> = secrets
        .shares
        .iter()
        .map(|s| (s.party_index, s.share.clone()))
        .collect();
    shares.sort_by_key(|(i, _)| *i);
    shares.truncate(usize::from(info.threshold));
    shares
}

// ── listing ─────────────────────────────────────────────────────────────

/// `GET /api/tss/wallets` — headers only, no passphrase involved. The login
/// screen uses this to offer a wallet picker before any authentication.
async fn list_wallets(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let dir = state.cfg.tss_dir();
    let wallets = tokio::task::spawn_blocking(move || store::list_wallets(&dir))
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("list task panicked: {e}")))?;
    Ok(Json(json!({ "wallets": wallets })))
}

// ── keygen ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeygenBody {
    threshold: Option<u16>,
    parties: Option<u16>,
    passphrase: Option<String>,
}

/// `POST /api/tss/keygen` — start a t-of-n DKG in the background. Poll
/// `GET /api/tss/keygen/status` until `done` (carrying the new address) or
/// `error`. Each DKG mints a fresh wallet at a fresh address, so nothing is
/// ever overwritten.
async fn keygen(
    State(state): State<AppState>,
    ValidJson(body): ValidJson<KeygenBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let t = body.threshold.unwrap_or(0);
    let n = body.parties.unwrap_or(0);
    pocketskynet_tss::validate_params(t, n).map_err(api_error)?;
    let passphrase = parse_passphrase(body.passphrase.as_deref())?;
    if passphrase.chars().count() < MIN_PASSPHRASE {
        return Err(ApiError::bad_request(format!(
            "passphrase must be at least {MIN_PASSPHRASE} characters"
        )));
    }

    // One ceremony at a time. Take the write lock for the check *and* the
    // claim so two concurrent requests cannot both pass an in-progress test.
    {
        let mut keygen = state
            .tss
            .keygen
            .write()
            .map_err(|_| ApiError::Internal(anyhow::anyhow!("tss keygen lock poisoned")))?;
        if keygen.in_progress() {
            return Err(ApiError::conflict("A key generation is already running"));
        }
        *keygen = KeygenStatus::GeneratingPrimes;
    }

    let app = state.clone();
    tokio::spawn(async move {
        let outcome = run_keygen(&app, t, n, &passphrase).await;
        let mut keygen = match app.tss.keygen.write() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        *keygen = match outcome {
            Ok(address) => {
                tracing::info!(%address, t, n, "TSS keygen finished");
                KeygenStatus::Done {
                    address: address.as_str().to_owned(),
                }
            }
            Err(e) => {
                tracing::error!("TSS keygen failed: {e}");
                KeygenStatus::Error {
                    error: e.to_string(),
                }
            }
        };
    });

    Ok(Json(json!({ "started": true })))
}

/// The whole creation ceremony: DKG, then the E2EE identity (§15.2 — an
/// independent keypair plus one ceremony signature over its binding), then
/// the sealed save. Failing anywhere leaves no file behind.
async fn run_keygen(
    state: &AppState,
    t: u16,
    n: u16,
    passphrase: &str,
) -> Result<WalletAddress, ApiError> {
    let eid = pocketskynet_tss::fresh_eid().map_err(api_error)?;
    let phase_state = state.clone();
    let shares = dkg::run_dkg(t, n, eid, move |phase| {
        let status = match phase {
            dkg::DkgPhase::GeneratingPrimes => KeygenStatus::GeneratingPrimes,
            dkg::DkgPhase::RunningProtocol => KeygenStatus::RunningProtocol,
            // The binding ceremony below still has to run; `Done` is
            // published only after the wallet file is on disk.
            dkg::DkgPhase::Done => KeygenStatus::Binding,
        };
        if let Ok(mut keygen) = phase_state.tss.keygen.write() {
            *keygen = status;
        }
    })
    .await
    .map_err(api_error)?;

    let address = eth::eth_address(&shares[0]).map_err(api_error)?;

    // The E2EE identity: minted from the CSPRNG, never derived (§15.2).
    let encryption = keys::EncryptionKeypair::generate()
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("generating E2EE keypair: {e}")))?;
    let binding_message = keys::build_key_binding_message(&address, encryption.public_key_hex());
    let prehash = eip191::eip191_digest(&binding_message);

    let signers: Vec<(u16, Share)> = shares
        .iter()
        .take(usize::from(t))
        .enumerate()
        .map(|(i, s)| (i as u16, s.clone()))
        .collect();
    let sig_eid = pocketskynet_tss::fresh_eid().map_err(api_error)?;
    let binding_sig = sign::sign_prehash(&signers, prehash, sig_eid)
        .await
        .map_err(api_error)?
        .to_hex();

    // The binding must verify through the same door every other client uses,
    // or the wallet would be created broken and only discovered at wrap time.
    keys::verify_key_binding(
        &address,
        Some(encryption.public_key_hex()),
        Some(&binding_sig),
    )
    .map_err(|e| ApiError::Internal(anyhow::anyhow!("binding self-check failed: {e}")))?;

    let info = store::WalletInfo {
        address: address.as_str().to_owned(),
        threshold: t,
        parties: n,
        created_at: store::now_secs(),
    };
    let secrets = store::WalletSecrets {
        shares: shares
            .into_iter()
            .enumerate()
            .map(|(i, share)| store::StoredShare {
                party_index: i as u16,
                share,
            })
            .collect(),
        enc_priv_hex: encryption.private_key_hex(),
        binding_sig,
    };

    let dir = state.cfg.tss_dir();
    let passphrase = passphrase.to_owned();
    tokio::task::spawn_blocking(move || {
        store::save_wallet(&dir, &info, &secrets, &passphrase, false)
    })
    .await
    .map_err(|e| ApiError::Internal(anyhow::anyhow!("save task panicked: {e}")))?
    .map_err(api_error)?;

    Ok(address)
}

/// `GET /api/tss/keygen/status`.
async fn keygen_status(State(state): State<AppState>) -> ApiResult<Json<KeygenStatus>> {
    let status = state
        .tss
        .keygen
        .read()
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("tss keygen lock poisoned")))?
        .clone();
    Ok(Json(status))
}

// ── signing ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignBody {
    address: Option<String>,
    passphrase: Option<String>,
    message: Option<String>,
}

/// `POST /api/tss/sign` — EIP-191 `personal_sign` by ceremony. Returns the
/// same `0x` + 130-hex wire form a local wallet produces; the caller feeds
/// it straight into `/api/auth/login`.
async fn sign_message(
    State(state): State<AppState>,
    ValidJson(body): ValidJson<SignBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let address = parse_address(body.address.as_deref())?;
    let passphrase = parse_passphrase(body.passphrase.as_deref())?;
    let message = match body.message.as_deref() {
        Some(m) if !m.is_empty() => m.to_owned(),
        _ => return Err(ApiError::bad_request("message is required")),
    };

    let (info, secrets) = open_wallet(&state, &address, &passphrase).await?;
    let prehash = eip191::eip191_digest(&message);
    let eid = pocketskynet_tss::fresh_eid().map_err(api_error)?;
    let sig = sign::sign_prehash(&quorum(&info, &secrets), prehash, eid)
        .await
        .map_err(api_error)?;

    let signature = sig.to_hex();
    // Self-check through the very verifier the login endpoint runs. A
    // signature that fails here must never leave the process.
    if !eip191::verify_signature(&message, &signature, &address) {
        return Err(ApiError::Internal(anyhow::anyhow!(
            "ceremony signature failed local verification"
        )));
    }
    Ok(Json(json!({ "address": address, "signature": signature })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignHashBody {
    address: Option<String>,
    passphrase: Option<String>,
    /// The 32-byte digest to sign, hex, `0x` optional — for transactions
    /// this is `LegacyTransaction::sighash()`.
    hash: Option<String>,
}

/// `POST /api/tss/sign-hash` — threshold-sign a caller-supplied 32-byte
/// digest, returning `{r, s, v}` with `v` the y-parity in {0, 1}. The client
/// assembles the transaction with `LegacyTransaction::sign_with_signature`.
async fn sign_hash(
    State(state): State<AppState>,
    ValidJson(body): ValidJson<SignHashBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let address = parse_address(body.address.as_deref())?;
    let passphrase = parse_passphrase(body.passphrase.as_deref())?;
    let raw = body.hash.as_deref().unwrap_or("");
    let stripped = raw
        .strip_prefix("0x")
        .or_else(|| raw.strip_prefix("0X"))
        .unwrap_or(raw);
    let bytes =
        hex::decode(stripped).map_err(|_| ApiError::bad_request("hash must be 32 bytes of hex"))?;
    let prehash: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| ApiError::bad_request("hash must be 32 bytes of hex"))?;

    let (info, secrets) = open_wallet(&state, &address, &passphrase).await?;
    let eid = pocketskynet_tss::fresh_eid().map_err(api_error)?;
    let sig = sign::sign_prehash(&quorum(&info, &secrets), prehash, eid)
        .await
        .map_err(api_error)?;

    Ok(Json(json!({
        "address": address,
        "r": format!("0x{}", hex::encode(sig.r)),
        "s": format!("0x{}", hex::encode(sig.s)),
        "v": sig.v,
    })))
}

// ── session keys / deletion ─────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WalletBody {
    address: Option<String>,
    passphrase: Option<String>,
}

/// `POST /api/tss/session-keys` — release the stored E2EE identity (§15.2)
/// to a caller who holds the passphrase. This is the TSS replacement for the
/// salted-derivation signature: the private key travels once per login, over
/// the same channel the mnemonic itself would travel in a paste, from the
/// user's own server.
async fn session_keys(
    State(state): State<AppState>,
    ValidJson(body): ValidJson<WalletBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let address = parse_address(body.address.as_deref())?;
    let passphrase = parse_passphrase(body.passphrase.as_deref())?;
    let (info, secrets) = open_wallet(&state, &address, &passphrase).await?;

    let keypair = keys::EncryptionKeypair::from_private_key_hex(&secrets.enc_priv_hex)
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("stored E2EE key invalid: {e}")))?;
    Ok(Json(json!({
        "address": address,
        "threshold": info.threshold,
        "parties": info.parties,
        "encryptionKey": secrets.enc_priv_hex,
        "publicKey": keypair.public_key_hex(),
        "bindingSig": secrets.binding_sig,
    })))
}

/// `POST /api/tss/delete` — destroy a wallet. Passphrase-gated like every
/// other touch: without it, anyone who can reach the API could destroy a
/// wallet they cannot open.
async fn delete_wallet(
    State(state): State<AppState>,
    ValidJson(body): ValidJson<WalletBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let address = parse_address(body.address.as_deref())?;
    let passphrase = parse_passphrase(body.passphrase.as_deref())?;
    let dir = state.cfg.tss_dir();
    let addr = address.clone();
    tokio::task::spawn_blocking(move || store::delete_wallet(&dir, &addr, &passphrase))
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("delete task panicked: {e}")))?
        .map_err(api_error)?;
    Ok(Json(json!({ "message": "TSS wallet deleted" })))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use crate::test_support::{send, state};

    // The full DKG → login → send path takes minutes of Paillier arithmetic
    // and lives in `server/tests/tss.rs`; what belongs here is the fast
    // contract: validation, status wiring, and the failure envelope.

    #[tokio::test]
    async fn an_empty_server_lists_no_wallets_and_is_idle() {
        let app = crate::routes::build(state("tss-empty"));
        let res = send(&app, "GET", "/api/tss/wallets", None, None).await;
        assert_eq!(res.status, StatusCode::OK);
        assert_eq!(res.json()["wallets"], serde_json::json!([]));

        let res = send(&app, "GET", "/api/tss/keygen/status", None, None).await;
        assert_eq!(res.status, StatusCode::OK);
        assert_eq!(res.json()["state"], "idle");
    }

    #[tokio::test]
    async fn keygen_rejects_bad_shapes_before_burning_any_cpu() {
        let app = crate::routes::build(state("tss-params"));
        for (body, why) in [
            (serde_json::json!({}), "no params"),
            (
                serde_json::json!({"threshold": 1, "parties": 3, "passphrase": "long enough"}),
                "t < 2",
            ),
            (
                serde_json::json!({"threshold": 3, "parties": 2, "passphrase": "long enough"}),
                "t > n",
            ),
            (
                serde_json::json!({"threshold": 2, "parties": 9, "passphrase": "long enough"}),
                "n over the cap",
            ),
            (
                serde_json::json!({"threshold": 2, "parties": 3, "passphrase": "short"}),
                "trivial passphrase",
            ),
        ] {
            let res = send(&app, "POST", "/api/tss/keygen", None, Some(body)).await;
            assert_eq!(res.status, StatusCode::BAD_REQUEST, "{why}");
        }
        // Nothing above may have claimed the keygen slot.
        let res = send(&app, "GET", "/api/tss/keygen/status", None, None).await;
        assert_eq!(res.json()["state"], "idle");
    }

    #[tokio::test]
    async fn signing_against_a_missing_wallet_is_a_404_not_a_hang() {
        let app = crate::routes::build(state("tss-missing"));
        let body = serde_json::json!({
            "address": "0x00112233445566778899aabbccddeeff00112233",
            "passphrase": "whatever it is",
            "message": "challenge",
        });
        let res = send(&app, "POST", "/api/tss/sign", None, Some(body)).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND);

        let body = serde_json::json!({
            "address": "0x00112233445566778899aabbccddeeff00112233",
            "passphrase": "whatever it is",
        });
        for path in ["/api/tss/session-keys", "/api/tss/delete"] {
            let res = send(&app, "POST", path, None, Some(body.clone())).await;
            assert_eq!(res.status, StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn sign_hash_validates_its_digest_shape() {
        let app = crate::routes::build(state("tss-hash"));
        for bad in ["", "0x1234", "zz", &"ab".repeat(33)] {
            let body = serde_json::json!({
                "address": "0x00112233445566778899aabbccddeeff00112233",
                "passphrase": "whatever it is",
                "hash": bad,
            });
            let res = send(&app, "POST", "/api/tss/sign-hash", None, Some(body)).await;
            assert_eq!(res.status, StatusCode::BAD_REQUEST, "{bad:?}");
        }
    }
}
