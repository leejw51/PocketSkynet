//! TSS (m-of-n threshold) wallet — the main-thread side.
//!
//! Every ceremony runs **in the browser** (docs/CRYPTO.md §15.3): this
//! module spawns the dedicated worker (`src/bin/tss_worker.rs`), feeds it
//! one request, relays its progress, and returns its terminal event. Share
//! files and the passphrase cross a same-origin `postMessage` boundary and
//! nothing else — no server sees them, in any mode, and nothing here is
//! ever written to `localStorage`.
//!
//! This module also owns the **quorum prompt**: a TSS session retains no
//! shares in memory, so every transaction signature starts with
//! [`request_quorum`], which raises the app-wide dialog asking the user to
//! present `t` share files and the passphrase for just that signing.

use std::cell::RefCell;

use futures::StreamExt;
use gloo_worker::Spawnable;
use pocketskynet_core::WalletAddress;
use serde::Deserialize;
use serde_json::Value;
use yew::Callback;

use crate::tss_proto::{TssEvent, TssPhase, TssRequest, TssWorker};
pub use crate::tss_proto::{TssHashSignature, TssSignBundle, MIN_PASSPHRASE};

/// The worker's loader shim, emitted un-hashed by trunk
/// (`data-loader-shim` on the worker's `<link data-trunk rel="rust">`).
const WORKER_LOADER: &str = "/tss_worker_loader.js";

// ---------------------------------------------------------------- header --

/// A share file's cleartext header, read locally to name a picked file and
/// to validate a quorum before any passphrase work. The file itself stays
/// an opaque [`Value`] on this side of the worker boundary.
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
    /// The largest `n` a share file may claim — mirrors
    /// `pocketskynet_tss::MAX_PARTIES`.
    pub const MAX_PARTIES: u16 = 5;

    /// The share-file format this build understands — version 3, the
    /// browser-ceremony DKLs23 format. Version-1 (server-side cggmp21) and
    /// version-2 (browser cggmp24) files belonged to retired stacks and
    /// are refused with a named message.
    pub const VERSION: u32 = 3;

    /// Parse a candidate file's header, `None` if it is not a share file.
    ///
    /// The header drives client-side quorum gating and slot rendering, so a
    /// garbled or hand-edited file must not pass: `2 ≤ threshold ≤ parties ≤
    /// MAX_PARTIES` is enforced here, the same bounds the keygen holds a
    /// wallet to — a threshold of 0 would show "ready" instantly, and an
    /// absurd `parties` would render that many shard slots.
    pub fn of(file: &Value) -> Option<Self> {
        let header: Self = serde_json::from_value(file.clone()).ok()?;
        (header.file_type == "pocketskynet-tss-share"
            && header.version == Self::VERSION
            && header.threshold >= 2
            && header.threshold <= header.parties
            && header.parties <= Self::MAX_PARTIES
            && header.party_index < header.parties)
            .then_some(header)
    }

    /// "2-of-3" — the shape, as the UI names it.
    pub fn shape(&self) -> String {
        format!("{}-of-{}", self.threshold, self.parties)
    }
}

/// The share-file version of a file this build cannot use: `Some(v)` when
/// the file really is one of ours but sealed by a retired ceremony stack
/// (v1 cggmp21, v2 cggmp24), `None` for anything else.
///
/// Such a file stays **out** of the quorum — its key share speaks a
/// protocol these ceremonies do not run, so counting it toward the
/// threshold would arm a sign-in that can only fail deep in the worker.
/// But "not an MPC share file" is the wrong thing to tell someone holding
/// their own wallet's backup, so the picker names it for what it is: the
/// same migration verdict `store::open_shares` gives, said early.
pub fn legacy_share_version(file: &Value) -> Option<u32> {
    let is_share = file.get("type").and_then(Value::as_str) == Some("pocketskynet-tss-share");
    let version = file.get("version").and_then(Value::as_u64)? as u32;
    (is_share && version < TssShareHeader::VERSION).then_some(version)
}

// ------------------------------------------------------- picked-file pool --

/// One share file picked in a file input: its file name for display, its
/// parsed JSON, and — when it is one — the share header.
#[derive(Debug, Clone, PartialEq)]
pub struct TssLoadedFile {
    pub name: String,
    pub value: Value,
    pub header: Option<TssShareHeader>,
}

/// Read a batch of picked files into the pool. Reading is async
/// (`File::text` under the hood); files that are not share files are kept
/// and *named* rather than dropped — a silently shrinking selection reads
/// as the picker losing files. Re-picking a share this pool already holds
/// replaces it, so a wrong pick is corrected without hunting for Clear.
pub async fn read_share_files(
    mut pool: Vec<TssLoadedFile>,
    picked: Vec<web_sys::File>,
) -> Vec<TssLoadedFile> {
    for file in picked {
        let name = file.name();
        // A file whose bytes cannot even be read still lands in the list
        // (header `None`, so it is named as not-a-share) — the same
        // promise the parse path keeps.
        let text = wasm_bindgen_futures::JsFuture::from(file.text())
            .await
            .ok()
            .and_then(|t| t.as_string());
        let value: Value = text
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);
        let header = TssShareHeader::of(&value);
        if let Some(h) = &header {
            pool.retain(|f| {
                f.header
                    .as_ref()
                    .map(|o| (o.address.clone(), o.party_index))
                    != Some((h.address.clone(), h.party_index))
            });
        }
        pool.push(TssLoadedFile {
            name,
            value,
            header,
        });
    }
    pool
}

/// One verdict on the picked files, computed one way for every consumer.
/// A status line renders it and the submit gate acts on it; deriving the
/// two answers separately is how a screen ends up saying "ready to sign
/// in" over a button that refuses.
pub enum TssQuorumCheck {
    /// No parsed share file yet.
    Empty,
    /// The headers disagree — files from different wallets mixed together.
    Mismatch,
    /// One wallet, but this many more *distinct* parties are needed.
    NeedMore(usize),
    /// A signable quorum.
    Ready {
        address: String,
        have: usize,
        need: usize,
    },
}

pub fn tss_quorum_check(files: &[TssLoadedFile]) -> TssQuorumCheck {
    let headers: Vec<&TssShareHeader> = files.iter().filter_map(|f| f.header.as_ref()).collect();
    let Some(first) = headers.first() else {
        return TssQuorumCheck::Empty;
    };
    if headers.iter().any(|h| {
        h.address != first.address || h.threshold != first.threshold || h.parties != first.parties
    }) {
        return TssQuorumCheck::Mismatch;
    }
    let mut parties: Vec<u16> = headers.iter().map(|h| h.party_index).collect();
    parties.sort_unstable();
    parties.dedup();
    let have = parties.len();
    let need = usize::from(first.threshold);
    if have < need {
        TssQuorumCheck::NeedMore(need - have)
    } else {
        TssQuorumCheck::Ready {
            address: first.address.clone(),
            have,
            need,
        }
    }
}

/// The picked files' consensus: `Some((address, share JSONs))` when every
/// parsed share belongs to one wallet and at least its threshold of
/// *distinct* parties is present. Anything else — mixed wallets, too few
/// shares, no valid share at all — is `None`, and the status line says
/// which.
pub fn tss_quorum(files: &[TssLoadedFile]) -> Option<(WalletAddress, Vec<Value>)> {
    let TssQuorumCheck::Ready { address, .. } = tss_quorum_check(files) else {
        return None;
    };
    let address = WalletAddress::new(&address).ok()?;
    let values = files
        .iter()
        .filter(|f| f.header.is_some())
        .map(|f| f.value.clone())
        .collect();
    Some((address, values))
}

// ------------------------------------------------------------- ceremonies --

/// A finished keygen, with the sealed share files parsed back to [`Value`]
/// so the download buttons and the sign-in-now path handle them exactly
/// like files picked from disk.
#[derive(Debug, Clone, PartialEq)]
pub struct TssCreated {
    pub address: String,
    pub threshold: u16,
    pub parties: u16,
    pub shares: Vec<Value>,
}

/// Spawn the worker, run one request, relay progress, return the terminal
/// event. The bridge — and with it the worker and everything it unsealed —
/// dies when this function returns.
async fn run_in_worker(
    req: TssRequest,
    mut on_phase: impl FnMut(TssPhase),
) -> Result<TssEvent, String> {
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let bridge = TssWorker::spawner()
        .callback(move |event| {
            let _ = tx.unbounded_send(event);
        })
        // Trunk's `data-loader-shim` emits a classic-worker script
        // (`importScripts` + the no-modules bindgen shim), so the loader
        // must be spawned as a classic worker — a module worker would
        // refuse `importScripts` and the ceremony would never start.
        .with_loader(true)
        .as_module(false)
        .spawn(WORKER_LOADER);
    bridge.send(req);
    while let Some(event) = rx.next().await {
        match event {
            TssEvent::Phase(phase) => on_phase(phase),
            terminal => {
                drop(bridge);
                return Ok(terminal);
            }
        }
    }
    Err("the ceremony worker went away".into())
}

/// Mint a fresh t-of-n wallet in the browser: DKG, the E2EE identity and
/// its binding, `n` sealed share files. The whole ceremony is seconds of
/// work, but it still runs in the dedicated worker so the UI thread never
/// competes with it.
pub async fn keygen(
    threshold: u16,
    parties: u16,
    passphrase: &str,
    mut on_phase: impl FnMut(TssPhase),
) -> Result<TssCreated, String> {
    let event = run_in_worker(
        TssRequest::Keygen {
            threshold,
            parties,
            passphrase: passphrase.to_owned(),
        },
        &mut on_phase,
    )
    .await?;
    match event {
        TssEvent::Created(created) => {
            let shares = created
                .shares
                .iter()
                .map(|s| serde_json::from_str(s))
                .collect::<Result<_, _>>()
                .map_err(|e| format!("worker returned an unreadable share file: {e}"))?;
            Ok(TssCreated {
                address: created.address,
                threshold: created.threshold,
                parties: created.parties,
                shares,
            })
        }
        TssEvent::Failed(e) => Err(e),
        _ => Err("unexpected ceremony result".into()),
    }
}

fn share_strings(shares: &[Value]) -> Vec<String> {
    shares.iter().map(|v| v.to_string()).collect()
}

/// EIP-191 `personal_sign` by ceremony over a presented quorum, plus the
/// E2EE identity the same quorum unseals — the login call.
pub async fn sign_message(
    shares: &[Value],
    passphrase: &str,
    message: &str,
) -> Result<TssSignBundle, String> {
    let event = run_in_worker(
        TssRequest::SignMessage {
            shares: share_strings(shares),
            passphrase: passphrase.to_owned(),
            message: message.to_owned(),
        },
        |_| {},
    )
    .await?;
    match event {
        TssEvent::SignedMessage(bundle) => Ok(bundle),
        TssEvent::Failed(e) => Err(e),
        _ => Err("unexpected ceremony result".into()),
    }
}

/// Threshold-sign a 32-byte digest — the transaction signer.
pub async fn sign_hash(
    shares: &[Value],
    passphrase: &str,
    hash: &[u8; 32],
) -> Result<TssHashSignature, String> {
    let event = run_in_worker(
        TssRequest::SignHash {
            shares: share_strings(shares),
            passphrase: passphrase.to_owned(),
            hash: *hash,
        },
        |_| {},
    )
    .await?;
    match event {
        TssEvent::SignedHash(sig) => Ok(sig),
        TssEvent::Failed(e) => Err(e),
        _ => Err("unexpected ceremony result".into()),
    }
}

// ----------------------------------------------------------- quorum prompt --

/// What a signing operation needs from the user: a quorum of share files
/// and the passphrase that seals them. Lives for one signature and is
/// dropped with it — never a field of the session, never storage.
#[derive(Debug, Clone)]
pub struct TssQuorum {
    pub shares: Vec<Value>,
    pub passphrase: String,
}

/// A pending "present your share files" request, held by the app-wide
/// prompt dialog while the user picks files.
pub struct QuorumRequest {
    /// The signing wallet — presented files must belong to it.
    pub address: WalletAddress,
    respond: futures::channel::oneshot::Sender<Option<TssQuorum>>,
}

impl QuorumRequest {
    /// Resolve the prompt: `Some` signs, `None` cancels.
    pub fn resolve(self, quorum: Option<TssQuorum>) {
        let _ = self.respond.send(quorum);
    }
}

struct PromptSlot {
    /// The mounted prompt host's "something is pending" poke.
    host: Option<Callback<()>>,
    pending: Option<QuorumRequest>,
}

thread_local! {
    static PROMPT: RefCell<PromptSlot> = const {
        RefCell::new(PromptSlot { host: None, pending: None })
    };
}

/// Called by the prompt host component on mount/unmount. One host at a
/// time — the app renders exactly one, next to its other global dialogs.
pub fn register_prompt_host(cb: Option<Callback<()>>) {
    PROMPT.with(|p| p.borrow_mut().host = cb);
}

/// The host collects the request it was poked about.
pub fn take_pending_request() -> Option<QuorumRequest> {
    PROMPT.with(|p| p.borrow_mut().pending.take())
}

/// Ask the user for a signing quorum. Resolves `None` when they cancel —
/// or immediately when no prompt host is mounted (nothing to ask with) or
/// another request is already on screen.
pub async fn request_quorum(address: WalletAddress) -> Option<TssQuorum> {
    let (tx, rx) = futures::channel::oneshot::channel();
    let host = PROMPT.with(|p| {
        let mut slot = p.borrow_mut();
        if slot.host.is_none() || slot.pending.is_some() {
            return None;
        }
        slot.pending = Some(QuorumRequest {
            address,
            respond: tx,
        });
        slot.host.clone()
    })?;
    host.emit(());
    rx.await.ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn share(threshold: u16, parties: u16, party_index: u16) -> Value {
        json!({
            "type": "pocketskynet-tss-share",
            "version": 3,
            "address": "0x00112233445566778899aabbccddeeff00112233",
            "threshold": threshold,
            "parties": parties,
            "partyIndex": party_index,
        })
    }

    #[test]
    fn a_well_formed_header_parses() {
        let h = TssShareHeader::of(&share(2, 3, 2)).unwrap();
        assert_eq!((h.threshold, h.parties, h.party_index), (2, 3, 2));
        assert_eq!(h.shape(), "2-of-3");
    }

    #[test]
    fn out_of_bounds_headers_are_not_share_files() {
        // The header gates the client-side quorum: a threshold of 0 or 1
        // would show "ready" with too few files, a threshold above `parties`
        // could never be met, and a huge `parties` would render that many
        // shard slots. All are refused here, mirroring the tss crate's
        // `2 ≤ t ≤ n ≤ MAX_PARTIES`.
        assert!(TssShareHeader::of(&share(0, 3, 0)).is_none());
        assert!(TssShareHeader::of(&share(1, 3, 0)).is_none());
        assert!(TssShareHeader::of(&share(4, 3, 0)).is_none());
        assert!(TssShareHeader::of(&share(2, TssShareHeader::MAX_PARTIES + 1, 0)).is_none());
        assert!(TssShareHeader::of(&share(2, u16::MAX, 0)).is_none());
        // The party index names one of the `n` files, `0..n`.
        assert!(TssShareHeader::of(&share(2, 3, 3)).is_none());
        // And the widest legal shape still parses.
        assert!(TssShareHeader::of(&share(2, TssShareHeader::MAX_PARTIES, 4)).is_some());
    }

    #[test]
    fn foreign_json_is_not_a_share_file() {
        assert!(TssShareHeader::of(&Value::Null).is_none());
        assert!(TssShareHeader::of(&json!({"type": "something-else"})).is_none());
        // Version 1 files sealed cggmp21 (server-side) shares and version
        // 2 sealed cggmp24 (browser) shares; this build's ceremonies
        // cannot use either, so the picker must not present one as
        // loadable.
        for old in [1, 2] {
            let mut stale = share(2, 3, 0);
            stale["version"] = json!(old);
            assert!(TssShareHeader::of(&stale).is_none());
        }
    }

    #[test]
    fn an_older_share_file_is_named_as_one_rather_than_as_junk() {
        // It must not count toward a quorum — its key share speaks a
        // retired protocol — but the holder of a v1/v2 wallet backup is
        // owed the migration verdict, not "this isn't a share file".
        for old in [1, 2] {
            let mut stale = share(2, 3, 0);
            stale["version"] = json!(old);
            assert!(
                TssShareHeader::of(&stale).is_none(),
                "stays out of the quorum"
            );
            assert_eq!(legacy_share_version(&stale), Some(old));
        }
        // The current version is not legacy, and neither is a stranger.
        assert_eq!(legacy_share_version(&share(2, 3, 0)), None);
        assert_eq!(
            legacy_share_version(&json!({"type": "something-else"})),
            None
        );
        assert_eq!(legacy_share_version(&Value::Null), None);
    }

    #[test]
    fn a_quorum_needs_threshold_distinct_parties_of_one_wallet() {
        let load = |v: Value, name: &str| TssLoadedFile {
            name: name.into(),
            header: TssShareHeader::of(&v),
            value: v,
        };
        // Two copies of one share are one party, not a quorum.
        let dup = vec![load(share(2, 3, 0), "a"), load(share(2, 3, 0), "b")];
        assert!(tss_quorum(&dup).is_none());
        assert!(matches!(
            tss_quorum_check(&dup),
            TssQuorumCheck::NeedMore(1)
        ));

        // Distinct parties of one wallet sign.
        let quorum = vec![load(share(2, 3, 0), "a"), load(share(2, 3, 2), "c")];
        let (address, files) = tss_quorum(&quorum).expect("a signable quorum");
        assert_eq!(
            address.as_str(),
            "0x00112233445566778899aabbccddeeff00112233"
        );
        assert_eq!(files.len(), 2);

        // A different wallet's share poisons the set — named, not ignored.
        let mut foreign = share(2, 3, 1);
        foreign["address"] = json!("0xffffffffffffffffffffffffffffffffffffffff");
        let mixed = vec![load(share(2, 3, 0), "a"), load(foreign, "x")];
        assert!(tss_quorum(&mixed).is_none());
        assert!(matches!(tss_quorum_check(&mixed), TssQuorumCheck::Mismatch));
    }
}
