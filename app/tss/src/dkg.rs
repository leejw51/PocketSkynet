//! Distributed key generation: CGGMP21 threshold keygen + aux-info generation.

use cggmp21::{key_refresh::PregeneratedPrimes, ExecutionId, KeyShare};
use rand_core::OsRng;

use crate::{sim_capacity, Curve, SecLevel, Share, TssError};

/// Progress phases reported to the caller during DKG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DkgPhase {
    GeneratingPrimes,
    RunningProtocol,
    Done,
}

/// Runs a full `t`-of-`n` DKG and returns the `n` shares, indexed by party.
///
/// `eid_seed` must be unique per keygen ceremony (it seeds the protocol
/// execution id — [`crate::fresh_eid`]). `on_phase` is invoked as the
/// ceremony advances; safe-prime generation dominates the wall clock.
pub async fn run_dkg(
    t: u16,
    n: u16,
    eid_seed: [u8; 32],
    mut on_phase: impl FnMut(DkgPhase) + Send,
) -> Result<Vec<Share>, TssError> {
    crate::validate_params(t, n)?;

    // Paillier safe-prime generation dominates DKG wall-clock time; run one
    // generation per party on blocking threads, in parallel.
    on_phase(DkgPhase::GeneratingPrimes);
    let mut prime_tasks = Vec::new();
    for _ in 0..n {
        prime_tasks.push(tokio::task::spawn_blocking(|| {
            PregeneratedPrimes::<SecLevel>::generate(&mut OsRng)
        }));
    }
    let mut primes = Vec::new();
    for task in prime_tasks {
        primes.push(
            task.await
                .map_err(|e| TssError::Ceremony(format!("prime generation task panicked: {e}")))?,
        );
    }

    on_phase(DkgPhase::RunningProtocol);

    // The whole ceremony is CPU-bound; move it off the async runtime. The
    // nested current-thread runtime is how an async simulated protocol runs
    // on a blocking thread.
    let capacity = sim_capacity(n);
    let shares = tokio::task::spawn_blocking(move || -> Result<Vec<Share>, TssError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .map_err(|e| TssError::Ceremony(format!("building ceremony runtime: {e}")))?;
        rt.block_on(async move {
            let eid = ExecutionId::new(&eid_seed);

            // `.set_threshold(t)` is what makes the shares VSS/threshold
            // shares rather than additive n-of-n ones — the whole point of
            // this port over the 2-of-2 reference.
            let incomplete = round_based::sim::async_env::run_with_capacity(
                capacity,
                n,
                |i, party| async move {
                    let mut rng = OsRng;
                    cggmp21::keygen::<Curve>(eid, i, n)
                        .set_threshold(t)
                        .start(&mut rng, party)
                        .await
                },
            )
            .await
            .into_vec();

            let mut incomplete_shares = Vec::new();
            for (i, r) in incomplete.into_iter().enumerate() {
                incomplete_shares
                    .push(r.map_err(|e| TssError::Ceremony(format!("keygen, party {i}: {e}")))?);
            }

            let aux = round_based::sim::async_env::run_with_capacity_and_setup(
                capacity,
                primes.into_iter(),
                |i, party, party_primes| async move {
                    let mut rng = OsRng;
                    cggmp21::aux_info_gen::<SecLevel>(eid, i, n, party_primes)
                        .start(&mut rng, party)
                        .await
                },
            )
            .await
            .into_vec();

            let mut shares = Vec::new();
            for (i, (core, aux)) in incomplete_shares.into_iter().zip(aux).enumerate() {
                let aux =
                    aux.map_err(|e| TssError::Ceremony(format!("aux info gen, party {i}: {e}")))?;
                let share = KeyShare::from_parts((core, aux)).map_err(|e| {
                    TssError::Ceremony(format!("combining key share, party {i}: {e}"))
                })?;
                shares.push(share);
            }
            Ok(shares)
        })
    })
    .await
    .map_err(|e| TssError::Ceremony(format!("DKG task panicked: {e}")))??;

    on_phase(DkgPhase::Done);
    Ok(shares)
}
