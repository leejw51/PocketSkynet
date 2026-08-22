//! The TSS ceremony worker's protocol — and the ceremonies themselves.
//!
//! This file is compiled into **two** binaries: the main app (`mod
//! tss_proto` in `main.rs`, which only spawns and talks to the worker) and
//! the worker (`src/bin/tss_worker.rs`, which registers [`TssWorker`] and
//! actually runs the ceremonies). It must therefore stay self-contained:
//! external crates only, no `crate::` paths into the rest of the app.
//!
//! Everything here executes **in the browser** (docs/CRYPTO.md §15.3). The
//! share files and the passphrase cross a `postMessage` boundary between two
//! contexts of the same origin — never a network. Share files travel as the
//! exact JSON strings the user's files hold, because the default worker
//! codec (bincode) cannot carry a `serde_json::Value`.

use gloo_worker::{HandlerId, Worker, WorkerScope};
use pocketskynet_core::{eip191, keys};
use pocketskynet_tss::{dkg, eth, sign, store, Share};
use serde::{Deserialize, Serialize};

/// Shortest passphrase a wallet may be sealed under — the passphrase is the
/// only thing between a found share file and its key material.
pub const MIN_PASSPHRASE: usize = 8;

/// What the app asks the worker to do. One request per spawned worker; the
/// bridge is dropped (terminating the worker and everything it unsealed)
/// as soon as the terminal event arrives.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TssRequest {
    /// Generate one party's safe-prime set — the DKG's dominant cost,
    /// farmed out to `n` of these workers **in parallel** so wallet
    /// creation costs one set's wall clock, not the sum of `n`.
    GeneratePrimes,
    /// Mint a fresh t-of-n wallet: DKG, the E2EE identity and its binding
    /// signature, then `n` sealed share files. `pregenerated` carries the
    /// `n` prime sets from the parallel `GeneratePrimes` fan-out (JSON, one
    /// per party); an empty vec makes this worker generate them itself,
    /// sequentially.
    Keygen {
        threshold: u16,
        parties: u16,
        passphrase: String,
        pregenerated: Vec<String>,
    },
    /// EIP-191 `personal_sign` by ceremony, plus the E2EE identity the same
    /// quorum unseals — everything a login needs from one passphrase entry.
    SignMessage {
        shares: Vec<String>,
        passphrase: String,
        message: String,
    },
    /// Threshold-sign a 32-byte digest (a transaction sighash).
    SignHash {
        shares: Vec<String>,
        passphrase: String,
        hash: [u8; 32],
    },
}

/// Keygen progress, forwarded to the creation wizard's step list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TssPhase {
    /// Safe-prime generation — the dominant cost; `done` of `total` party
    /// sets finished so a minutes-long step can show movement.
    Primes {
        done: u16,
        total: u16,
    },
    Protocol,
    /// Minting the E2EE identity, ceremony-signing its binding, sealing.
    Sealing,
}

/// A finished keygen: the wallet and its `n` sealed share files, each the
/// exact JSON text the user downloads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TssCreated {
    pub address: String,
    pub threshold: u16,
    pub parties: u16,
    pub shares: Vec<String>,
}

/// A ceremony signature over a message, plus the unsealed E2EE identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TssSignBundle {
    pub address: String,
    /// EIP-191 wire form, `0x` + 130 hex.
    pub signature: String,
    /// E2EE private key, `0x` + 64 hex. Held in memory only.
    pub encryption_key: String,
    /// Uncompressed public key, 130 hex chars, no `0x`.
    pub public_key: String,
    /// The wallet's ceremony signature over the key-binding message.
    pub binding_sig: String,
}

/// A recoverable signature over a 32-byte digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TssHashSignature {
    pub r: [u8; 32],
    pub s: [u8; 32],
    /// y-parity, 0 or 1 — the `recovery_id` for
    /// `LegacyTransaction::sign_with_signature`.
    pub v: u8,
}

impl TssHashSignature {
    /// The `r ‖ s` halves as the 64-byte array transaction assembly takes.
    /// Used by the main binary's transaction assembly; the worker binary
    /// compiles this file too and never calls it.
    #[allow(dead_code)]
    pub fn rs_bytes(&self) -> [u8; 64] {
        let mut rs = [0u8; 64];
        rs[..32].copy_from_slice(&self.r);
        rs[32..].copy_from_slice(&self.s);
        rs
    }
}

/// What the worker sends back: progress, then exactly one terminal event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TssEvent {
    Phase(TssPhase),
    /// One serialized safe-prime set — `GeneratePrimes`' terminal event.
    Primes(String),
    Created(TssCreated),
    SignedMessage(TssSignBundle),
    SignedHash(TssHashSignature),
    Failed(String),
}

/// The worker: one request in, progress + one terminal event out. All the
/// CPU-heavy Paillier arithmetic happens inside `received`, which is fine —
/// blocking is what a dedicated worker is *for*.
pub struct TssWorker;

impl Worker for TssWorker {
    type Message = ();
    type Input = TssRequest;
    type Output = TssEvent;

    fn create(_scope: &WorkerScope<Self>) -> Self {
        Self
    }

    fn update(&mut self, _scope: &WorkerScope<Self>, _msg: ()) {}

    fn received(&mut self, scope: &WorkerScope<Self>, msg: TssRequest, id: HandlerId) {
        run(msg, &mut |event| scope.respond(id, event));
    }
}

/// Executes one request, emitting progress and the terminal event.
pub fn run(req: TssRequest, emit: &mut dyn FnMut(TssEvent)) {
    let terminal = match req {
        TssRequest::GeneratePrimes => serde_json::to_string(&dkg::generate_prime_set())
            .map(TssEvent::Primes)
            .unwrap_or_else(|e| TssEvent::Failed(format!("encoding prime set: {e}"))),
        TssRequest::Keygen {
            threshold,
            parties,
            passphrase,
            pregenerated,
        } => keygen(threshold, parties, &passphrase, &pregenerated, emit)
            .map(TssEvent::Created)
            .unwrap_or_else(TssEvent::Failed),
        TssRequest::SignMessage {
            shares,
            passphrase,
            message,
        } => sign_message(&shares, &passphrase, &message)
            .map(TssEvent::SignedMessage)
            .unwrap_or_else(TssEvent::Failed),
        TssRequest::SignHash {
            shares,
            passphrase,
            hash,
        } => sign_hash(&shares, &passphrase, hash)
            .map(TssEvent::SignedHash)
            .unwrap_or_else(TssEvent::Failed),
    };
    emit(terminal);
}

/// The whole creation ceremony: DKG, the E2EE identity (CRYPTO.md §15.2 —
/// an independent keypair plus one ceremony signature over its binding),
/// then sealing the `n` share files. Failing anywhere leaves nothing behind.
fn keygen(
    t: u16,
    n: u16,
    passphrase: &str,
    pregenerated: &[String],
    emit: &mut dyn FnMut(TssEvent),
) -> Result<TssCreated, String> {
    pocketskynet_tss::validate_params(t, n).map_err(|e| e.to_string())?;
    if passphrase.chars().count() < MIN_PASSPHRASE {
        return Err(format!(
            "passphrase must be at least {MIN_PASSPHRASE} characters"
        ));
    }

    let eid = pocketskynet_tss::fresh_eid().map_err(|e| e.to_string())?;
    let mut on_phase = |phase: dkg::DkgPhase| {
        let phase = match phase {
            dkg::DkgPhase::GeneratingPrimes { done, total } => TssPhase::Primes { done, total },
            dkg::DkgPhase::RunningProtocol => TssPhase::Protocol,
            // The binding ceremony and the sealing below still have to run;
            // success is announced only by the terminal `Created` event.
            dkg::DkgPhase::Done => TssPhase::Sealing,
        };
        emit(TssEvent::Phase(phase));
    };
    let shares = if pregenerated.is_empty() {
        dkg::run_dkg(t, n, eid, on_phase).map_err(|e| e.to_string())?
    } else {
        let primes: Vec<dkg::Primes> = pregenerated
            .iter()
            .map(|p| serde_json::from_str(p))
            .collect::<Result<_, _>>()
            .map_err(|e| format!("decoding pregenerated primes: {e}"))?;
        dkg::run_dkg_with_primes(t, n, eid, primes, &mut on_phase).map_err(|e| e.to_string())?
    };

    let address = eth::eth_address(&shares[0]).map_err(|e| e.to_string())?;

    // The E2EE identity: minted from the CSPRNG, never derived (§15.2).
    let encryption =
        keys::EncryptionKeypair::generate().map_err(|e| format!("generating E2EE keypair: {e}"))?;
    let binding_message = keys::build_key_binding_message(&address, encryption.public_key_hex());
    let prehash = eip191::eip191_digest(&binding_message);

    let signers: Vec<(u16, Share)> = shares
        .iter()
        .take(usize::from(t))
        .enumerate()
        .map(|(i, s)| (i as u16, s.clone()))
        .collect();
    let sig_eid = pocketskynet_tss::fresh_eid().map_err(|e| e.to_string())?;
    let binding_sig = sign::sign_prehash(&signers, prehash, sig_eid)
        .map_err(|e| e.to_string())?
        .to_hex();

    // The binding must verify through the same door every client uses, or
    // the wallet would be born broken and only discovered at wrap time.
    keys::verify_key_binding(
        &address,
        Some(encryption.public_key_hex()),
        Some(&binding_sig),
    )
    .map_err(|e| format!("binding self-check failed: {e}"))?;

    let raw_shares: Vec<serde_json::Value> = shares
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("encoding shares: {e}"))?;
    let files = store::seal_shares(
        &address,
        t,
        n,
        &raw_shares,
        &encryption.private_key_hex(),
        &binding_sig,
        passphrase,
    )
    .map_err(|e| e.to_string())?;

    let shares = files
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("encoding share files: {e}"))?;

    Ok(TssCreated {
        address: address.as_str().to_owned(),
        threshold: t,
        parties: n,
        shares,
    })
}

/// Open a presented quorum and parse its shares into signer form.
fn open(
    files: &[String],
    passphrase: &str,
) -> Result<(store::OpenedWallet, Vec<(u16, Share)>), String> {
    let files: Vec<store::ShareFile> = files
        .iter()
        .map(|f| serde_json::from_str(f))
        .collect::<Result<_, _>>()
        .map_err(|_| "not a share file".to_owned())?;
    let opened = store::open_shares(&files, passphrase).map_err(|e| e.to_string())?;
    let signers = opened
        .signers
        .iter()
        .map(|(i, raw)| {
            let share: Share = serde_json::from_value(raw.clone())
                .map_err(|_| format!("share {i} is not a valid key share"))?;
            Ok((*i, share))
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok((opened, signers))
}

fn sign_message(
    files: &[String],
    passphrase: &str,
    message: &str,
) -> Result<TssSignBundle, String> {
    let (opened, signers) = open(files, passphrase)?;
    let prehash = eip191::eip191_digest(message);
    let eid = pocketskynet_tss::fresh_eid().map_err(|e| e.to_string())?;
    let sig = sign::sign_prehash(&signers, prehash, eid).map_err(|e| e.to_string())?;

    let signature = sig.to_hex();
    // Self-check through the very verifier the login endpoint runs. A
    // signature that fails here must never leave the worker.
    if !eip191::verify_signature(message, &signature, &opened.address) {
        return Err("ceremony signature failed local verification".into());
    }

    let keypair = keys::EncryptionKeypair::from_private_key_hex(&opened.enc_priv_hex)
        .map_err(|e| format!("sealed E2EE key invalid: {e}"))?;
    Ok(TssSignBundle {
        address: opened.address.as_str().to_owned(),
        signature,
        public_key: keypair.public_key_hex().to_owned(),
        encryption_key: opened.enc_priv_hex.clone(),
        binding_sig: opened.binding_sig.clone(),
    })
}

fn sign_hash(
    files: &[String],
    passphrase: &str,
    hash: [u8; 32],
) -> Result<TssHashSignature, String> {
    let (_opened, signers) = open(files, passphrase)?;
    let eid = pocketskynet_tss::fresh_eid().map_err(|e| e.to_string())?;
    let sig = sign::sign_prehash(&signers, hash, eid).map_err(|e| e.to_string())?;
    Ok(TssHashSignature {
        r: sig.r,
        s: sig.s,
        v: sig.v,
    })
}
