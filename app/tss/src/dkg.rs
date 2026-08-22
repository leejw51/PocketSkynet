//! Distributed key generation: DKLs23 threshold keygen.
//!
//! Synchronous on purpose: [`crate::relay::run_parties`] drives every
//! party's future in one loop on the calling thread, so the same function
//! runs on a native test thread and inside a web worker — no async runtime
//! anywhere. The whole ceremony is OT and curve arithmetic, milliseconds
//! of work; the phases exist so a progress UI has something honest to say.

use rand_core::{OsRng, RngCore};
use sl_dkls23::setup::{keygen::SetupMessage, NoSigningKey, NoVerifyingKey};
use sl_mpc_mate::message::InstanceId;

use crate::relay::LocalCoordinator;
use crate::{Share, TssError};

/// Progress phases reported to the caller during DKG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DkgPhase {
    RunningProtocol,
    Done,
}

/// Runs a full `t`-of-`n` DKG and returns the `n` shares, indexed by party.
///
/// `eid_seed` must be unique per keygen ceremony (it becomes the protocol
/// instance id — [`crate::fresh_eid`]). `on_phase` is invoked as the
/// ceremony advances.
pub fn run_dkg(
    t: u16,
    n: u16,
    eid_seed: [u8; 32],
    mut on_phase: impl FnMut(DkgPhase),
) -> Result<Vec<Share>, TssError> {
    crate::validate_params(t, n)?;
    on_phase(DkgPhase::RunningProtocol);

    // All parties equal: no hierarchical ranks.
    let ranks = vec![0u8; usize::from(n)];
    // Local ceremony, one process: message authenticity between the
    // parties is the process's own memory safety, so the no-op signer
    // stands in for per-party network keys.
    let party_vk: Vec<NoVerifyingKey> = (0..usize::from(n)).map(NoVerifyingKey::new).collect();

    let coordinator = LocalCoordinator::new();
    let futures: Vec<_> = (0..usize::from(n))
        .map(|i| {
            let setup = SetupMessage::new(
                InstanceId::new(eid_seed),
                NoSigningKey,
                i,
                party_vk.clone(),
                &ranks,
                usize::from(t),
            );
            let mut seed = [0u8; 32];
            OsRng.fill_bytes(&mut seed);
            sl_dkls23::keygen::run(setup, seed, coordinator.connect())
        })
        .collect();

    let mut shares = Vec::with_capacity(usize::from(n));
    for (i, result) in crate::relay::run_parties(futures).into_iter().enumerate() {
        let share = result.map_err(|e| TssError::Ceremony(format!("keygen, party {i}: {e:?}")))?;
        shares.push(Share::new(share));
    }

    // Every party must have derived the same shared public key; anything
    // else is a broken ceremony and must never be sealed into files.
    let pk = shares[0].public_key();
    if shares.iter().any(|s| s.public_key() != pk) {
        return Err(TssError::Ceremony(
            "parties disagree about the shared public key".into(),
        ));
    }

    on_phase(DkgPhase::Done);
    Ok(shares)
}
