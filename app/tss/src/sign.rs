//! t-of-n threshold signing over a 32-byte prehash.

use cggmp21::key_share::AnyKeyShare;
use cggmp21::{generic_ec::Scalar, DataToSign, ExecutionId};
use rand_core::OsRng;

use crate::{eth, sim_capacity, Curve, Share, TssError};

/// Signs `prehash` with a chosen subset of exactly `t` shares and returns a
/// recoverable, low-s Ethereum signature.
///
/// `signers` pairs each share with its **party index at keygen** — CGGMP21
/// needs the mapping to compute the Lagrange coefficients that turn VSS
/// shares into additive ones. Any subset of size `t` works; which `t` is the
/// caller's choice. `eid_seed` must be unique per signing ceremony
/// ([`crate::fresh_eid`]).
pub async fn sign_prehash(
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

    let data = DataToSign::from_scalar(Scalar::<Curve>::from_be_bytes_mod_order(prehash));
    let capacity = sim_capacity(t);

    // Signing is CPU-bound (Paillier); run it on a blocking thread.
    let reference = signers[0].1.clone();
    let sig =
        tokio::task::spawn_blocking(move || -> Result<cggmp21::Signature<Curve>, TssError> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .map_err(|e| TssError::Ceremony(format!("building ceremony runtime: {e}")))?;
            rt.block_on(async move {
                let eid = ExecutionId::new(&eid_seed);
                let indexes = &indexes;
                let signers = &signers;
                let results = round_based::sim::async_env::run_with_capacity(
                    capacity,
                    t,
                    |i, party| async move {
                        let mut rng = OsRng;
                        cggmp21::signing(eid, i, indexes, &signers[usize::from(i)].1)
                            .sign(&mut rng, party, data)
                            .await
                    },
                )
                .await
                .into_vec();

                let mut out = None;
                for (i, r) in results.into_iter().enumerate() {
                    let s =
                        r.map_err(|e| TssError::Ceremony(format!("signing, party {i}: {e}")))?;
                    out.get_or_insert(s);
                }
                out.ok_or_else(|| TssError::Ceremony("no signature produced".into()))
            })
        })
        .await
        .map_err(|e| TssError::Ceremony(format!("signing task panicked: {e}")))??;

    eth::to_eth_signature(&reference, sig, prehash)
}
