//! t-of-n threshold signing over a 32-byte prehash.
//!
//! Synchronous like [`crate::dkg`]: the ceremony is a
//! [`crate::relay::run_parties`] loop on the calling thread — milliseconds
//! of OT and curve arithmetic; browser callers still run it in a worker so
//! the UI never competes with a ceremony.

use rand_core::{OsRng, RngCore};
use sl_dkls23::setup::{sign::SetupMessage, NoSigningKey, NoVerifyingKey};
use sl_mpc_mate::message::InstanceId;

use crate::relay::LocalCoordinator;
use crate::{eth, Share, TssError};

/// Signs `prehash` with a chosen subset of exactly `t` shares and returns a
/// recoverable, low-s Ethereum signature.
///
/// `signers` pairs each share with its **party index at keygen** — the
/// pairing the sealed files record, revalidated here against the id the
/// share itself carries. Any subset of size `t` works; which `t` is the
/// caller's choice. `eid_seed` must be unique per signing ceremony
/// ([`crate::fresh_eid`]).
pub fn sign_prehash(
    signers: &[(u16, Share)],
    prehash: [u8; 32],
    eid_seed: [u8; 32],
) -> Result<eth::TssSignature, TssError> {
    let first = signers
        .first()
        .ok_or_else(|| TssError::InvalidSigners("empty signer set".into()))?;
    let t = u16::from(first.1.threshold);
    let n = u16::from(first.1.total_parties);
    if signers.len() != usize::from(t) {
        return Err(TssError::InvalidSigners(format!(
            "this wallet needs exactly {t} of its {n} shares to sign, got {}",
            signers.len()
        )));
    }

    // Sort by keygen index for a deterministic signer list, and reject a
    // subset that repeats a party, names one the wallet does not have, or
    // pairs a share with an index that is not its own.
    let mut signers: Vec<(u16, Share)> = signers.to_vec();
    signers.sort_by_key(|(i, _)| *i);
    let indexes: Vec<u16> = signers.iter().map(|(i, _)| *i).collect();
    if indexes.windows(2).any(|w| w[0] == w[1]) {
        return Err(TssError::InvalidSigners(
            "duplicate party in signer set".into(),
        ));
    }
    if indexes.iter().any(|i| *i >= n) {
        return Err(TssError::InvalidSigners(format!(
            "party index out of range for a {n}-party wallet"
        )));
    }
    for (i, share) in &signers {
        if u16::from(share.party_id) != *i {
            return Err(TssError::InvalidSigners(format!(
                "share labeled party {i} was minted for party {}",
                share.party_id
            )));
        }
        if u16::from(share.threshold) != t || u16::from(share.total_parties) != n {
            return Err(TssError::InvalidSigners(
                "shares disagree about the wallet's shape".into(),
            ));
        }
    }

    let reference = signers[0].1.clone();
    // The signing setup names only the participating quorum; each party is
    // addressed by its position in that list, while the keyshare itself
    // carries its original keygen id.
    let party_vk: Vec<NoVerifyingKey> = signers
        .iter()
        .map(|(_, share)| NoVerifyingKey::new(usize::from(share.party_id)))
        .collect();

    let coordinator = LocalCoordinator::new();
    let futures: Vec<_> = signers
        .iter()
        .enumerate()
        .map(|(idx, (_, share))| {
            // Default chain path "m": sign under the wallet's root key —
            // the address derivation and this signer must agree.
            let setup = SetupMessage::new(
                InstanceId::new(eid_seed),
                NoSigningKey,
                idx,
                party_vk.clone(),
                share.clone(),
            )
            .with_hash(prehash);
            let mut seed = [0u8; 32];
            OsRng.fill_bytes(&mut seed);
            sl_dkls23::sign::run(setup, seed, coordinator.connect())
        })
        .collect();

    let mut out = None;
    for (i, result) in crate::relay::run_parties(futures).into_iter().enumerate() {
        let sig = result.map_err(|e| TssError::Ceremony(format!("signing, party {i}: {e:?}")))?;
        out.get_or_insert(sig);
    }
    let (signature, recovery_id) =
        out.ok_or_else(|| TssError::Ceremony("no signature produced".into()))?;

    eth::to_eth_signature(&reference, signature, recovery_id, prehash)
}
