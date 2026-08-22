//! Distributed key generation: CGGMP24 threshold keygen + aux-info generation.
//!
//! Synchronous on purpose: `round_based::sim` drives every party's state
//! machine in one loop on the calling thread, so the same function runs on a
//! native test thread and inside a web worker — no async runtime anywhere.
//! Callers own the threading question; in the browser that means "not on the
//! main thread", because safe-prime generation blocks for minutes.

use cggmp24::{key_refresh::PregeneratedPrimes, ExecutionId, KeyShare};
use rand_core::OsRng;

use crate::{Curve, SecLevel, Share, TssError};

/// Progress phases reported to the caller during DKG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DkgPhase {
    /// Safe-prime generation — the dominant cost, one set per party.
    /// Reported before each set starts, `done` sets out of `total` finished.
    GeneratingPrimes {
        done: u16,
        total: u16,
    },
    RunningProtocol,
    Done,
}

/// One party's pregenerated safe-prime set, at this crate's security level.
pub type Primes = PregeneratedPrimes<SecLevel>;

/// Generate one party's safe-prime set — the unit of the DKG's dominant
/// cost, exposed separately so a browser can farm the `n` independent
/// sets out to `n` parallel workers and hand them to
/// [`run_dkg_with_primes`].
pub fn generate_prime_set() -> Primes {
    PregeneratedPrimes::generate(&mut OsRng)
}

/// Runs a full `t`-of-`n` DKG and returns the `n` shares, indexed by party.
///
/// `eid_seed` must be unique per keygen ceremony (it seeds the protocol
/// execution id — [`crate::fresh_eid`]). `on_phase` is invoked as the
/// ceremony advances; safe-prime generation dominates the wall clock.
pub fn run_dkg(
    t: u16,
    n: u16,
    eid_seed: [u8; 32],
    mut on_phase: impl FnMut(DkgPhase),
) -> Result<Vec<Share>, TssError> {
    crate::validate_params(t, n)?;

    // Paillier safe-prime generation dominates DKG wall-clock time. On a
    // native build the sets are generated on parallel threads; in the
    // browser's single-threaded wasm they run one after another, which is
    // why the phase carries a done/total counter for the progress UI.
    let primes = generate_primes(n, &mut on_phase)?;
    run_dkg_with_primes(t, n, eid_seed, primes, on_phase)
}

/// [`run_dkg`], with the safe-prime sets supplied by the caller — the web
/// client generates them in parallel workers ([`generate_prime_set`]) and
/// runs only the interactive protocol here.
pub fn run_dkg_with_primes(
    t: u16,
    n: u16,
    eid_seed: [u8; 32],
    primes: Vec<Primes>,
    mut on_phase: impl FnMut(DkgPhase),
) -> Result<Vec<Share>, TssError> {
    crate::validate_params(t, n)?;
    if primes.len() != usize::from(n) {
        return Err(TssError::Ceremony(format!(
            "expected {n} pregenerated prime sets, got {}",
            primes.len()
        )));
    }

    on_phase(DkgPhase::RunningProtocol);

    let eid = ExecutionId::new(&eid_seed);

    // `.set_threshold(t)` is what makes the shares VSS/threshold shares
    // rather than additive n-of-n ones.
    let incomplete = round_based::sim::run(n, |i, party| async move {
        let mut rng = OsRng;
        cggmp24::keygen::<Curve>(eid, i, n)
            .set_threshold(t)
            .start(&mut rng, party)
            .await
    })
    .map_err(|e| TssError::Ceremony(format!("keygen simulation: {e}")))?
    .0;

    let mut incomplete_shares = Vec::new();
    for (i, r) in incomplete.into_iter().enumerate() {
        incomplete_shares
            .push(r.map_err(|e| TssError::Ceremony(format!("keygen, party {i}: {e}")))?);
    }

    let aux = round_based::sim::run_with_setup(primes, |i, party, party_primes| async move {
        let mut rng = OsRng;
        cggmp24::aux_info_gen::<SecLevel>(eid, i, n, party_primes)
            .start(&mut rng, party)
            .await
    })
    .map_err(|e| TssError::Ceremony(format!("aux info simulation: {e}")))?
    .0;

    let mut shares = Vec::new();
    for (i, (core, aux)) in incomplete_shares.into_iter().zip(aux).enumerate() {
        let aux = aux.map_err(|e| TssError::Ceremony(format!("aux info gen, party {i}: {e}")))?;
        let share = KeyShare::from_parts((core, aux))
            .map_err(|e| TssError::Ceremony(format!("combining key share, party {i}: {e}")))?;
        shares.push(share);
    }

    on_phase(DkgPhase::Done);
    Ok(shares)
}

/// One pregenerated safe-prime set per party — threaded natively,
/// sequential (with progress callbacks) on wasm.
#[cfg(not(target_arch = "wasm32"))]
fn generate_primes(
    n: u16,
    on_phase: &mut impl FnMut(DkgPhase),
) -> Result<Vec<PregeneratedPrimes<SecLevel>>, TssError> {
    on_phase(DkgPhase::GeneratingPrimes { done: 0, total: n });
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..n)
            .map(|_| scope.spawn(|| PregeneratedPrimes::<SecLevel>::generate(&mut OsRng)))
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .map_err(|_| TssError::Ceremony("prime generation thread panicked".into()))
            })
            .collect()
    })
}

#[cfg(target_arch = "wasm32")]
fn generate_primes(
    n: u16,
    on_phase: &mut impl FnMut(DkgPhase),
) -> Result<Vec<PregeneratedPrimes<SecLevel>>, TssError> {
    let mut primes = Vec::with_capacity(usize::from(n));
    for done in 0..n {
        on_phase(DkgPhase::GeneratingPrimes { done, total: n });
        primes.push(PregeneratedPrimes::<SecLevel>::generate(&mut OsRng));
    }
    Ok(primes)
}
