//! `/api/tss/*` — the m-of-n threshold wallet (docs/CRYPTO.md §15).
//!
//! Every ceremony runs inside this process (§15.3: the audited CGGMP21
//! stack cannot build for wasm32), but custody is the **user's**: keygen
//! hands back `n` passphrase-sealed share files and this server persists
//! nothing. Signing requests present any `t` of the files, so losing up to
//! `n − t` of them loses nothing — that is the m-of-n promise, end to end.
//!
//! The routes are deliberately **unauthenticated**: a TSS wallet must sign
//! its login challenge before it has a JWT. What keeps that honest — the
//! files are the credential (sealed under the passphrase, 600k PBKDF2 per
//! request), a keygen's output is collectable exactly once by the caller
//! holding its random `keygenId`, and the general per-IP rate limit meters
//! the door.

use axum::extract::{DefaultBodyLimit, State};
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
/// only thing between a found share file and its key material, so a trivial
/// one defeats the whole custody model.
const MIN_PASSPHRASE: usize = 8;

/// A login presents up to five ~10 KB sealed share files in one JSON body,
/// which brushes the general 100 KB cap; give this router the same private
/// budget treatment as uploads.
const MAX_TSS_BODY_BYTES: usize = 1024 * 1024;

/// One DKG at a time, observable while it runs, its output parked for a
/// single collection.
///
/// Sync locks, not async ones: every touch is a short read-or-write from a
/// handler or the keygen task's phase callback (which is not async).
#[derive(Debug, Default)]
pub struct TssState {
    keygen: std::sync::RwLock<KeygenStatus>,
    /// The finished ceremony's sealed share files, waiting for the one
    /// `collect` call holding the matching `keygenId`. In memory only —
    /// gone on restart, wiped on the next keygen, never on disk.
    pending: std::sync::Mutex<Option<PendingShares>>,
}

#[derive(Debug)]
struct PendingShares {
    keygen_id: String,
    address: String,
    threshold: u16,
    parties: u16,
    files: Vec<store::ShareFile>,
    /// When the ceremony parked these — a fresh parking means the
    /// requester's 1.5 s poll loop is about to collect, and a new keygen
    /// must not evict the slot out from under that handover.
    parked_at: std::time::Instant,
}

/// How long a finished ceremony's shares hold the slot against a new
/// keygen. The collector polls every 1.5 s, so a minute covers any
/// realistic network hiccup; past it, the requester has walked away, and
/// holding key material in memory for them indefinitely is worse than
/// making them rerun.
const COLLECT_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum KeygenStatus {
    #[default]
    Idle,
    GeneratingPrimes,
    RunningProtocol,
    /// DKG done; minting the E2EE keypair, ceremony-signing its binding,
    /// and sealing the share files.
    Binding,
    /// Shares are sealed and waiting for `collect`.
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
        .route("/tss/keygen", post(keygen))
        .route("/tss/keygen/status", get(keygen_status))
        .route("/tss/keygen/collect", post(keygen_collect))
        .route("/tss/sign", post(sign_message))
        .route("/tss/sign-hash", post(sign_hash))
        .layer(DefaultBodyLimit::max(MAX_TSS_BODY_BYTES))
}

/// Map a TSS failure onto the API error envelope. The passphrase case is a
/// 401 — it is an authentication failure in every sense that matters.
fn api_error(e: TssError) -> ApiError {
    match e {
        TssError::BadPassphrase => ApiError::unauthorized("Wrong passphrase"),
        TssError::WalletNotFound => ApiError::not_found("No TSS wallet with that address"),
        TssError::WalletExists => ApiError::conflict("A TSS wallet with that address exists"),
        TssError::InvalidParams { .. } | TssError::InvalidSigners(_) | TssError::Store(_) => {
            ApiError::bad_request(e.to_string())
        }
        other => ApiError::Internal(anyhow::anyhow!(other)),
    }
}

fn parse_passphrase(raw: Option<&str>) -> ApiResult<String> {
    match raw {
        Some(p) if !p.is_empty() => Ok(p.to_owned()),
        _ => Err(ApiError::bad_request("passphrase is required")),
    }
}

/// Open a quorum of presented share files off the async runtime — PBKDF2
/// alone is ~a third of a second of pure CPU, which must not run on a
/// worker the realtime hub shares.
async fn open_shares(
    files: Vec<store::ShareFile>,
    passphrase: String,
) -> ApiResult<store::OpenedWallet> {
    tokio::task::spawn_blocking(move || store::open_shares(&files, &passphrase))
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("open task panicked: {e}")))?
        .map_err(api_error)
}

/// The opened quorum's shares, parsed into real `cggmp21` key shares.
fn typed_signers(opened: &store::OpenedWallet) -> ApiResult<Vec<(u16, Share)>> {
    opened
        .signers
        .iter()
        .map(|(i, raw)| {
            let share: Share = serde_json::from_value(raw.clone()).map_err(|_| {
                // A file that unseals but does not parse as a key share was
                // built by something other than this server's keygen.
                ApiError::bad_request(format!("share {i} is not a valid key share"))
            })?;
            Ok((*i, share))
        })
        .collect()
}

// ── keygen ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeygenBody {
    threshold: Option<u16>,
    parties: Option<u16>,
    passphrase: Option<String>,
}

/// `POST /api/tss/keygen` — start a t-of-n DKG in the background and
/// return the `keygenId` capability that will collect its output. Poll
/// `GET /api/tss/keygen/status` until `done`, then `POST
/// /api/tss/keygen/collect`. Each run mints a fresh wallet; nothing is
/// stored server-side, so nothing can be overwritten either.
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

    // Everything fallible happens *before* the slot is claimed: a `?` after
    // the claim would leave the status stuck in-progress forever, wedging
    // every future keygen until a restart.
    let keygen_id = crate::auth::random_hex_32()?;

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
        // A freshly finished ceremony keeps the slot while its requester's
        // poll loop collects it; past the grace they have walked away, and
        // the new ceremony evicts the abandoned shares. Check *and* evict
        // under one hold of the mutex, before the claim — a poisoned lock
        // is an error like the keygen lock above, never a silent skip that
        // would bypass the grace check or leave stale shares collectable.
        let mut pending = state
            .tss
            .pending
            .lock()
            .map_err(|_| ApiError::Internal(anyhow::anyhow!("tss pending lock poisoned")))?;
        if let Some(p) = pending.as_ref() {
            if p.parked_at.elapsed() < COLLECT_GRACE {
                return Err(ApiError::conflict(
                    "The previous key generation is still being collected — try again shortly",
                ));
            }
        }
        *pending = None;
        *keygen = KeygenStatus::GeneratingPrimes;
    }

    let app = state.clone();
    let task_id = keygen_id.clone();
    tokio::spawn(async move {
        let outcome = run_keygen(&app, t, n, &passphrase, &task_id).await;
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

    Ok(Json(json!({ "started": true, "keygenId": keygen_id })))
}

/// The whole creation ceremony: DKG, the E2EE identity (§15.2 — an
/// independent keypair plus one ceremony signature over its binding), then
/// sealing the `n` share files into the pending slot. Failing anywhere
/// leaves nothing behind.
async fn run_keygen(
    state: &AppState,
    t: u16,
    n: u16,
    passphrase: &str,
    keygen_id: &str,
) -> Result<WalletAddress, ApiError> {
    let eid = pocketskynet_tss::fresh_eid().map_err(api_error)?;
    let phase_state = state.clone();
    let shares = dkg::run_dkg(t, n, eid, move |phase| {
        let status = match phase {
            dkg::DkgPhase::GeneratingPrimes => KeygenStatus::GeneratingPrimes,
            dkg::DkgPhase::RunningProtocol => KeygenStatus::RunningProtocol,
            // The binding ceremony and the sealing below still have to run;
            // `Done` is published only once `collect` would succeed.
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
    // or the wallet would be born broken and only discovered at wrap time.
    keys::verify_key_binding(
        &address,
        Some(encryption.public_key_hex()),
        Some(&binding_sig),
    )
    .map_err(|e| ApiError::Internal(anyhow::anyhow!("binding self-check failed: {e}")))?;

    // Seal the n share files (PBKDF2-heavy → blocking thread).
    let raw_shares: Vec<serde_json::Value> = shares
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<_, _>>()
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("encoding shares: {e}")))?;
    let seal_address = address.clone();
    let enc_priv = encryption.private_key_hex();
    let seal_pass = passphrase.to_owned();
    let seal_binding = binding_sig.clone();
    let files = tokio::task::spawn_blocking(move || {
        store::seal_shares(
            &seal_address,
            t,
            n,
            &raw_shares,
            &enc_priv,
            &seal_binding,
            &seal_pass,
        )
    })
    .await
    .map_err(|e| ApiError::Internal(anyhow::anyhow!("seal task panicked: {e}")))?
    .map_err(api_error)?;

    let mut pending = state
        .tss
        .pending
        .lock()
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("tss pending lock poisoned")))?;
    *pending = Some(PendingShares {
        keygen_id: keygen_id.to_owned(),
        address: address.as_str().to_owned(),
        threshold: t,
        parties: n,
        files,
        parked_at: std::time::Instant::now(),
    });
    Ok(address)
}

/// `GET /api/tss/keygen/status` — phases only; the shares come from
/// `collect`, gated on the `keygenId` only their requester holds.
async fn keygen_status(State(state): State<AppState>) -> ApiResult<Json<KeygenStatus>> {
    let status = state
        .tss
        .keygen
        .read()
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("tss keygen lock poisoned")))?
        .clone();
    Ok(Json(status))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollectBody {
    keygen_id: Option<String>,
}

/// `POST /api/tss/keygen/collect` — hand over the sealed share files,
/// exactly once. A second call — or one with the wrong id — finds nothing,
/// so a bystander who merely polled the status endpoint gets nothing.
async fn keygen_collect(
    State(state): State<AppState>,
    ValidJson(body): ValidJson<CollectBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let keygen_id = body
        .keygen_id
        .filter(|id| !id.is_empty())
        .ok_or_else(|| ApiError::bad_request("keygenId is required"))?;
    let taken = {
        let mut pending = state
            .tss
            .pending
            .lock()
            .map_err(|_| ApiError::Internal(anyhow::anyhow!("tss pending lock poisoned")))?;
        match pending.as_ref() {
            Some(p) if p.keygen_id == keygen_id => pending.take(),
            _ => None,
        }
    };
    let Some(p) = taken else {
        return Err(ApiError::not_found(
            "Nothing to collect — wrong keygenId, already collected, or the server restarted",
        ));
    };
    // The handover is done: return the status to idle so the new wallet's
    // address stops being served from an unauthenticated endpoint, and so
    // a stale `done` cannot lure another tab into a doomed collect.
    if let Ok(mut keygen) = state.tss.keygen.write() {
        *keygen = KeygenStatus::Idle;
    }
    Ok(Json(json!({
        "address": p.address,
        "threshold": p.threshold,
        "parties": p.parties,
        "shares": p.files,
    })))
}

// ── signing ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignBody {
    shares: Option<Vec<store::ShareFile>>,
    passphrase: Option<String>,
    message: Option<String>,
}

fn parse_files(raw: Option<Vec<store::ShareFile>>) -> ApiResult<Vec<store::ShareFile>> {
    match raw {
        Some(files) if !files.is_empty() => Ok(files),
        _ => Err(ApiError::bad_request("shares are required")),
    }
}

/// `POST /api/tss/sign` — EIP-191 `personal_sign` by ceremony, plus the
/// E2EE identity the same quorum unseals. One endpoint on purpose: this is
/// the login call, and everything login needs comes from the same opened
/// files — a second passphrase round trip would just be a second PBKDF2.
async fn sign_message(ValidJson(body): ValidJson<SignBody>) -> ApiResult<Json<serde_json::Value>> {
    let files = parse_files(body.shares)?;
    let passphrase = parse_passphrase(body.passphrase.as_deref())?;
    let message = match body.message.as_deref() {
        Some(m) if !m.is_empty() => m.to_owned(),
        _ => return Err(ApiError::bad_request("message is required")),
    };

    let opened = open_shares(files, passphrase).await?;
    let signers = typed_signers(&opened)?;
    let prehash = eip191::eip191_digest(&message);
    let eid = pocketskynet_tss::fresh_eid().map_err(api_error)?;
    let sig = sign::sign_prehash(&signers, prehash, eid)
        .await
        .map_err(api_error)?;

    let signature = sig.to_hex();
    // Self-check through the very verifier the login endpoint runs. A
    // signature that fails here must never leave the process.
    if !eip191::verify_signature(&message, &signature, &opened.address) {
        return Err(ApiError::Internal(anyhow::anyhow!(
            "ceremony signature failed local verification"
        )));
    }

    let keypair = keys::EncryptionKeypair::from_private_key_hex(&opened.enc_priv_hex)
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("sealed E2EE key invalid: {e}")))?;
    Ok(Json(json!({
        "address": opened.address,
        "threshold": opened.threshold,
        "parties": opened.parties,
        "signature": signature,
        "encryptionKey": opened.enc_priv_hex,
        "publicKey": keypair.public_key_hex(),
        "bindingSig": opened.binding_sig,
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignHashBody {
    shares: Option<Vec<store::ShareFile>>,
    passphrase: Option<String>,
    /// The 32-byte digest to sign, hex, `0x` optional — for transactions
    /// this is `LegacyTransaction::sighash()`.
    hash: Option<String>,
}

/// `POST /api/tss/sign-hash` — threshold-sign a caller-supplied 32-byte
/// digest, returning `{r, s, v}` with `v` the y-parity in {0, 1}. The
/// client assembles the transaction with
/// `LegacyTransaction::sign_with_signature`.
async fn sign_hash(ValidJson(body): ValidJson<SignHashBody>) -> ApiResult<Json<serde_json::Value>> {
    let files = parse_files(body.shares)?;
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

    let opened = open_shares(files, passphrase).await?;
    let signers = typed_signers(&opened)?;
    let eid = pocketskynet_tss::fresh_eid().map_err(api_error)?;
    let sig = sign::sign_prehash(&signers, prehash, eid)
        .await
        .map_err(api_error)?;

    Ok(Json(json!({
        "address": opened.address,
        "r": format!("0x{}", hex::encode(sig.r)),
        "s": format!("0x{}", hex::encode(sig.s)),
        "v": sig.v,
    })))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use pocketskynet_core::WalletAddress;
    use pocketskynet_tss::store;

    use crate::test_support::{send, state};

    // The full DKG → collect → login → send path takes minutes of Paillier
    // arithmetic and lives in `server/tests/tss.rs`; what belongs here is
    // the fast contract: validation, status wiring, collect semantics, and
    // the failure envelope.

    fn sealed_files(t: u16, n: u16, passphrase: &str) -> Vec<store::ShareFile> {
        // Dummy share payloads: sealing is transport and does not care.
        // Anything that gets far enough to *parse* one as a key share is
        // covered by the e2e suite.
        let shares: Vec<serde_json::Value> =
            (0..n).map(|i| serde_json::json!({ "i": i })).collect();
        store::seal_shares(
            &WalletAddress::new("0x00112233445566778899aabbccddeeff00112233").unwrap(),
            t,
            n,
            &shares,
            &format!("0x{}", "ab".repeat(32)),
            &format!("0x{}", "cd".repeat(65)),
            passphrase,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn a_fresh_server_is_idle_with_nothing_to_collect() {
        let app = crate::routes::build(state("tss-empty"));
        let res = send(&app, "GET", "/api/tss/keygen/status", None, None).await;
        assert_eq!(res.status, StatusCode::OK);
        assert_eq!(res.json()["state"], "idle");

        let body = serde_json::json!({ "keygenId": "a".repeat(64) });
        let res = send(&app, "POST", "/api/tss/keygen/collect", None, Some(body)).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND);
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
    async fn signing_validates_the_quorum_before_any_ceremony() {
        let app = crate::routes::build(state("tss-quorum"));
        let files = sealed_files(2, 3, "a fine passphrase");

        // No shares at all.
        let body = serde_json::json!({
            "shares": [], "passphrase": "a fine passphrase", "message": "challenge",
        });
        let res = send(&app, "POST", "/api/tss/sign", None, Some(body)).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST);

        // One share of a 2-of-3 wallet — below the threshold.
        let body = serde_json::json!({
            "shares": [files[0]], "passphrase": "a fine passphrase", "message": "challenge",
        });
        let res = send(&app, "POST", "/api/tss/sign", None, Some(body)).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST);

        // Two wallets' files mixed together.
        let other = sealed_files(2, 3, "a fine passphrase");
        let body = serde_json::json!({
            "shares": [files[0], other[1]],
            "passphrase": "a fine passphrase",
            "message": "challenge",
        });
        let res = send(&app, "POST", "/api/tss/sign", None, Some(body)).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST);

        // The wrong passphrase is a 401, not a 400: the quorum was fine.
        let body = serde_json::json!({
            "shares": [files[0], files[2]], "passphrase": "not it", "message": "challenge",
        });
        let res = send(&app, "POST", "/api/tss/sign", None, Some(body)).await;
        assert_eq!(res.status, StatusCode::UNAUTHORIZED);

        // A good quorum with dummy payloads dies at the key-share parse —
        // a named 400, not a hang or a 500.
        let body = serde_json::json!({
            "shares": [files[0], files[2]],
            "passphrase": "a fine passphrase",
            "message": "challenge",
        });
        let res = send(&app, "POST", "/api/tss/sign", None, Some(body)).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn collect_hands_over_once_and_returns_the_status_to_idle() {
        let st = state("tss-collect");
        {
            *st.tss.keygen.write().unwrap() = super::KeygenStatus::Done {
                address: "0x00112233445566778899aabbccddeeff00112233".into(),
            };
            *st.tss.pending.lock().unwrap() = Some(super::PendingShares {
                keygen_id: "a".repeat(64),
                address: "0x00112233445566778899aabbccddeeff00112233".into(),
                threshold: 2,
                parties: 3,
                files: sealed_files(2, 3, "a fine passphrase"),
                parked_at: std::time::Instant::now(),
            });
        }
        let app = crate::routes::build(st);
        let body = serde_json::json!({ "keygenId": "a".repeat(64) });
        let res = send(&app, "POST", "/api/tss/keygen/collect", None, Some(body)).await;
        assert_eq!(res.status, StatusCode::OK);
        assert_eq!(res.json()["shares"].as_array().unwrap().len(), 3);

        // The collected wallet's address must not linger on the
        // unauthenticated status endpoint.
        let res = send(&app, "GET", "/api/tss/keygen/status", None, None).await;
        assert_eq!(res.json()["state"], "idle");

        // And the handover was exactly once.
        let body = serde_json::json!({ "keygenId": "a".repeat(64) });
        let res = send(&app, "POST", "/api/tss/keygen/collect", None, Some(body)).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_new_keygen_cannot_evict_shares_still_being_collected() {
        let st = state("tss-grace");
        {
            *st.tss.keygen.write().unwrap() = super::KeygenStatus::Done {
                address: "0x00112233445566778899aabbccddeeff00112233".into(),
            };
            *st.tss.pending.lock().unwrap() = Some(super::PendingShares {
                keygen_id: "a".repeat(64),
                address: "0x00112233445566778899aabbccddeeff00112233".into(),
                threshold: 2,
                parties: 3,
                files: sealed_files(2, 3, "a fine passphrase"),
                parked_at: std::time::Instant::now(),
            });
        }
        let app = crate::routes::build(st);
        // Valid params, honest passphrase — refused anyway: the previous
        // ceremony's requester is inside its collection window.
        let body = serde_json::json!({ "threshold": 2, "parties": 3, "passphrase": "long enough" });
        let res = send(&app, "POST", "/api/tss/keygen", None, Some(body)).await;
        assert_eq!(res.status, StatusCode::CONFLICT);

        // The parked shares survived the attempt.
        let body = serde_json::json!({ "keygenId": "a".repeat(64) });
        let res = send(&app, "POST", "/api/tss/keygen/collect", None, Some(body)).await;
        assert_eq!(res.status, StatusCode::OK);
    }

    #[tokio::test]
    async fn sign_hash_validates_its_digest_shape() {
        let app = crate::routes::build(state("tss-hash"));
        let files = sealed_files(2, 2, "a fine passphrase");
        for bad in ["", "0x1234", "zz", &"ab".repeat(33)] {
            let body = serde_json::json!({
                "shares": files,
                "passphrase": "a fine passphrase",
                "hash": bad,
            });
            let res = send(&app, "POST", "/api/tss/sign-hash", None, Some(body)).await;
            assert_eq!(res.status, StatusCode::BAD_REQUEST, "{bad:?}");
        }
    }
}
