//! t-of-n threshold signing over a 32-byte prehash.
//!
//! Synchronous like [`crate::dkg`]: the ceremony is a `round_based::sim`
//! loop on the calling thread — a second or two of Paillier arithmetic, so
//! browser callers run it in a worker.

use cggmp24::key_share::AnyKeyShare;
use cggmp24::signing::PrehashedDataToSign;
use cggmp24::{generic_ec::Scalar, ExecutionId};
use rand_core::OsRng;

use crate::{eth, Curve, Share, TssError};

/// Signs `prehash` with a chosen subset of exactly `t` shares and returns a
/// recoverable, low-s Ethereum signature.
///
/// `signers` pairs each share with its **party index at keygen** — CGGMP24
/// needs the mapping to compute the Lagrange coefficients that turn VSS
/// shares into additive ones. Any subset of size `t` works; which `t` is the
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
    let t = first.1.min_signers();
    let n = first.1.n();
    if signers.len() != usize::from(t) {
        return Err(TssError::InvalidSigners(format!(
            "this wallet needs exactly {t} of its {n} shares to sign, got {}",
            signers.len()
        )));
    }

    // Sort by keygen index for a deterministic signer list, and reject a
    // subset that repeats a party or names one the wallet does not have.
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

    // The digest was computed by this crate's callers (EIP-191, EIP-155
    // sighash), so the preimage is known to *us* — but the type-safe
    // `DataToSign` door wants the preimage bytes themselves, and threading
    // them down here would tie this signer to two specific message formats.
    // `PrehashedDataToSign` is explicitly supported by `sign` and the
    // protocol is secure with it (its caveat is about APIs that must prove
    // preimage knowledge, which ECDSA signing does not).
    let data = PrehashedDataToSign::from_scalar(Scalar::<Curve>::from_be_bytes_mod_order(prehash));

    let eid = ExecutionId::new(&eid_seed);
    let reference = signers[0].1.clone();
    let indexes_ref = &indexes;
    let signers_ref = &signers;
    let results = round_based::sim::run(t, |i, party| async move {
        let mut rng = OsRng;
        cggmp24::signing(eid, i, indexes_ref, &signers_ref[usize::from(i)].1)
            .sign(&mut rng, party, &data)
            .await
    })
    .map_err(|e| TssError::Ceremony(format!("signing simulation: {e}")))?
    .0;

    let mut out = None;
    for (i, r) in results.into_iter().enumerate() {
        let s = r.map_err(|e| TssError::Ceremony(format!("signing, party {i}: {e}")))?;
        out.get_or_insert(s);
    }
    let sig = out.ok_or_else(|| TssError::Ceremony("no signature produced".into()))?;

    eth::to_eth_signature(&reference, sig, prehash)
}
