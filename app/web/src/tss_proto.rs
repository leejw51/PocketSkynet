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
    /// Mint a fresh t-of-n wallet: DKG, the E2EE identity and its binding
    /// signature, then `n` sealed share files.
    Keygen {
        threshold: u16,
        parties: u16,
        passphrase: String,
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
    /// The key-generation ceremony itself — seconds, not minutes.
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
    Created(TssCreated),
    SignedMessage(TssSignBundle),
    SignedHash(TssHashSignature),
    Failed(String),
}

/// The worker: one request in, progress + one terminal event out. All the
/// ceremony arithmetic happens inside `received`, which is fine —
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
        TssRequest::Keygen {
            threshold,
            parties,
            passphrase,
        } => keygen(threshold, parties, &passphrase, emit)
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
    emit: &mut dyn FnMut(TssEvent),
) -> Result<TssCreated, String> {
    pocketskynet_tss::validate_params(t, n).map_err(|e| e.to_string())?;
    if passphrase.chars().count() < MIN_PASSPHRASE {
        return Err(format!(
            "passphrase must be at least {MIN_PASSPHRASE} characters"
        ));
    }

    let eid = pocketskynet_tss::fresh_eid().map_err(|e| e.to_string())?;
    let on_phase = |phase: dkg::DkgPhase| {
        let phase = match phase {
            dkg::DkgPhase::RunningProtocol => TssPhase::Protocol,
            // The binding ceremony and the sealing below still have to run;
            // success is announced only by the terminal `Created` event.
            dkg::DkgPhase::Done => TssPhase::Sealing,
        };
        emit(TssEvent::Phase(phase));
    };
    let shares = dkg::run_dkg(t, n, eid, on_phase).map_err(|e| e.to_string())?;

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

    let raw_shares: Vec<String> = shares.iter().map(store::encode_share).collect();
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
            let share = store::decode_share(raw)
                .map_err(|_| format!("share {i} is not a valid key share"))?;
            Ok((*i, share))
        })
        .collect::<Result<Vec<(u16, Share)>, String>>()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    const PASSPHRASE: &str = "worker protocol test passphrase";

    /// Drive one request through the worker's own `run`, collecting every
    /// event — the exact code path the browser ceremonies take, minus the
    /// postMessage transport.
    fn drive(req: TssRequest) -> Vec<TssEvent> {
        let mut events = Vec::new();
        run(req, &mut |e| events.push(e));
        events
    }

    fn create_wallet() -> TssCreated {
        let events = drive(TssRequest::Keygen {
            threshold: 2,
            parties: 3,
            passphrase: PASSPHRASE.into(),
        });
        // Progress first, then exactly one terminal event.
        assert!(
            matches!(events.first(), Some(TssEvent::Phase(TssPhase::Protocol))),
            "keygen must announce the protocol phase first"
        );
        match events.last() {
            Some(TssEvent::Created(c)) => c.clone(),
            other => panic!("keygen must end in Created, got {other:?}"),
        }
    }

    #[test]
    fn a_send_signature_runs_end_to_end_through_the_worker_protocol() {
        // The whole send path the wallet dialog drives, in one process:
        // mint a 2-of-3 wallet, then sign a transaction sighash with a
        // strict subset of the sealed files — parties {0, 2}, the
        // lost-share quorum.
        let created = create_wallet();
        assert_eq!(created.shares.len(), 3);
        let quorum = vec![created.shares[0].clone(), created.shares[2].clone()];

        let sighash = [0x5a; 32];
        let events = drive(TssRequest::SignHash {
            shares: quorum.clone(),
            passphrase: PASSPHRASE.into(),
            hash: sighash,
        });
        let sig = match events.last() {
            Some(TssEvent::SignedHash(s)) => *s,
            other => panic!("sign-hash must end in SignedHash, got {other:?}"),
        };
        // The recovery id feeds `LegacyTransaction::sign_with_signature`
        // directly; anything but a parity bit would assemble a
        // chain-rejected transaction.
        assert!(
            sig.v <= 1,
            "recovery id must be a parity bit, got {}",
            sig.v
        );
        assert_ne!(sig.r, [0u8; 32]);
        assert_ne!(sig.s, [0u8; 32]);
        assert_eq!(sig.rs_bytes()[..32], sig.r);
        assert_eq!(sig.rs_bytes()[32..], sig.s);

        // The login path over the same quorum: an EIP-191 ceremony whose
        // signature must verify through the MPC-blind verifier the server
        // runs — the indistinguishability requirement itself.
        let message = "sign-in challenge stand-in";
        let events = drive(TssRequest::SignMessage {
            shares: quorum,
            passphrase: PASSPHRASE.into(),
            message: message.into(),
        });
        let bundle = match events.last() {
            Some(TssEvent::SignedMessage(b)) => b.clone(),
            other => panic!("sign-message must end in SignedMessage, got {other:?}"),
        };
        assert_eq!(bundle.address, created.address);
        let address = pocketskynet_core::WalletAddress::new(&created.address).unwrap();
        assert!(eip191::verify_signature(
            message,
            &bundle.signature,
            &address
        ));
    }

    #[test]
    fn the_wrong_passphrase_fails_the_seal_not_the_ceremony() {
        let created = create_wallet();
        let events = drive(TssRequest::SignHash {
            shares: vec![created.shares[0].clone(), created.shares[1].clone()],
            passphrase: "not the passphrase".into(),
            hash: [1; 32],
        });
        match events.last() {
            Some(TssEvent::Failed(msg)) => {
                assert!(
                    msg.contains("passphrase"),
                    "the failure must name the passphrase, got: {msg}"
                );
            }
            other => panic!("a wrong passphrase must fail, got {other:?}"),
        }
    }
}
