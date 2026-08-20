//! m-of-n threshold (TSS/MPC) wallet — CGGMP21 DKG, threshold-ECDSA signing
//! with Ethereum signature recovery, and passphrase-sealed share persistence.
//!
//! The design record is `docs/CRYPTO.md §15`; the port derives from the
//! reviewed reference wallet built on the audited `cggmp21` crate. Every
//! ceremony runs in this process over `round_based`'s simulated network —
//! all `n` shares are held by the user's own self-hosted server, sealed under
//! a passphrase (§15.4). What production distribution adds later is a real
//! transport between share holders, not a different share format.
//!
//! Native-only: this crate cannot build for wasm32 (§15.3) and must never be
//! a dependency of the web workspace.

pub mod dkg;
pub mod eth;
pub mod sign;
pub mod store;

/// The curve every EVM chain uses.
pub type Curve = cggmp21::supported_curves::Secp256k1;
/// 128-bit security level (the production default of cggmp21).
pub type SecLevel = cggmp21::security_level::SecurityLevel128;
/// A complete key share (DKG output + aux info) for one party.
pub type Share = cggmp21::KeyShare<Curve, SecLevel>;

/// Most parties a wallet may have. The ceremony cost is O(n²) messages and
/// n safe-prime generations, so this is a UX bound, not a protocol one.
pub const MAX_PARTIES: u16 = 5;

#[derive(Debug, thiserror::Error)]
pub enum TssError {
    /// `t`/`n` outside `2 ≤ t ≤ n ≤ MAX_PARTIES`.
    #[error("invalid threshold parameters: want 2 ≤ t ≤ n ≤ {MAX_PARTIES}, got t={t}, n={n}")]
    InvalidParams { t: u16, n: u16 },
    /// The signer set handed to [`sign::sign_prehash`] does not match the
    /// wallet's threshold.
    #[error("{0}")]
    InvalidSigners(String),
    /// A protocol run failed. The message names the party and round.
    #[error("ceremony failed: {0}")]
    Ceremony(String),
    /// The produced signature does not recover to the wallet's public key.
    #[error("signature does not match the wallet public key")]
    Recovery,
    /// Wallet-file storage problems (I/O, format, versioning).
    #[error("wallet store: {0}")]
    Store(String),
    /// The passphrase failed to open the wallet file. Deliberately carries no
    /// detail: MAC failure and corrupt ciphertext are indistinguishable by
    /// construction, and saying more would help nobody but an attacker.
    #[error("wrong passphrase or corrupt wallet file")]
    BadPassphrase,
    /// A wallet for this address already exists and `force` was not given.
    #[error("a TSS wallet for this address already exists")]
    WalletExists,
    #[error("no TSS wallet for this address")]
    WalletNotFound,
    /// The OS entropy source failed — never expected, never unwrapped.
    #[error("system entropy unavailable")]
    Entropy,
}

/// Validate `t`-of-`n` parameters. One place, so the DKG, the store and the
/// API layer cannot disagree about what a legal wallet shape is.
pub fn validate_params(t: u16, n: u16) -> Result<(), TssError> {
    if t < 2 || t > n || n > MAX_PARTIES {
        return Err(TssError::InvalidParams { t, n });
    }
    Ok(())
}

/// A fresh random execution id for one protocol run.
///
/// CGGMP21 requires the execution id to be unique per ceremony; reusing one
/// across ceremonies voids the protocol's security proofs.
pub fn fresh_eid() -> Result<[u8; 32], TssError> {
    pocketskynet_core::random::bytes::<32>().map_err(|_| TssError::Entropy)
}

/// Message capacity for the simulated network. The reference wallet used a
/// flat 2000 for two parties; message volume grows roughly quadratically, so
/// scale with the party count rather than discovering the ceiling in a
/// 5-party ceremony.
pub(crate) fn sim_capacity(parties: u16) -> usize {
    2000 * usize::from(parties)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_accept_exactly_the_documented_range() {
        // 2 ≤ t ≤ n ≤ MAX_PARTIES, nothing else.
        for (t, n, ok) in [
            (2, 2, true),
            (2, 3, true),
            (3, 5, true),
            (5, 5, true),
            (1, 2, false), // t=1 is a single key wearing a costume
            (0, 0, false),
            (3, 2, false), // t > n
            (2, 6, false), // n > MAX_PARTIES
            (6, 6, false),
        ] {
            assert_eq!(validate_params(t, n).is_ok(), ok, "t={t} n={n}");
        }
    }

    #[test]
    fn execution_ids_are_unique_per_ceremony() {
        // The protocol's security argument needs fresh eids; two draws
        // colliding would mean the entropy source is broken.
        assert_ne!(fresh_eid().unwrap(), fresh_eid().unwrap());
    }

    #[test]
    fn sim_capacity_grows_with_the_party_count() {
        assert!(sim_capacity(5) > sim_capacity(2));
        // And never below what the audited 2-party reference shipped with.
        assert!(sim_capacity(2) >= 2000);
    }
}
