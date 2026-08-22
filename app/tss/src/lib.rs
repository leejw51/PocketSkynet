//! m-of-n threshold (MPC) wallet — DKLs23 DKG, threshold-ECDSA signing
//! with Ethereum signature recovery, and passphrase-sealed share
//! persistence.
//!
//! The design record is `docs/CRYPTO.md §15`. Every ceremony runs in one
//! process over an in-process relay ([`relay`]) — and since this crate
//! builds for wasm32, that process is the **user's browser**: keygen mints
//! `n` passphrase-sealed share files (`store`) client-side, signing opens
//! any `t` of them client-side, and no share, passphrase or key material
//! ever reaches a server — the server's part of an MPC login is verifying
//! an ordinary ECDSA signature it cannot tell from a single-key one.
//! Losing up to `n − t` files loses nothing.
//!
//! The protocol engine is Silence Labs' DKLs23 (`sl-dkls23`): OT-based
//! threshold ECDSA, no Paillier moduli, no safe-prime hunting — a full DKG
//! is milliseconds where the previous CGGMP24 engine took minutes.
//!
//! Native builds serve the test suite and the vector generator; the server
//! must never link this crate.

pub mod dkg;
pub mod eth;
pub mod relay;
pub mod sign;
pub mod store;

/// A complete key share for one party. `Arc` because the signing setup
/// message shares it without copying key material.
pub type Share = std::sync::Arc<sl_dkls23::keygen::Keyshare>;

/// Most parties a wallet may have. A UX bound (that many share files to
/// download and safekeep), not a protocol one.
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
    #[error("an MPC wallet for this address already exists")]
    WalletExists,
    #[error("no MPC wallet for this address")]
    WalletNotFound,
    /// The entropy source failed — never expected, never unwrapped.
    #[error("system entropy unavailable")]
    Entropy,
}

/// Validate `t`-of-`n` parameters. One place, so the DKG, the store and the
/// client cannot disagree about what a legal wallet shape is.
pub fn validate_params(t: u16, n: u16) -> Result<(), TssError> {
    if t < 2 || t > n || n > MAX_PARTIES {
        return Err(TssError::InvalidParams { t, n });
    }
    Ok(())
}

/// A fresh random execution id for one protocol run.
///
/// It becomes the ceremony's DKLs23 `InstanceId`, which must be unique per
/// ceremony — reusing one across ceremonies voids the protocol's security
/// arguments. (Each party additionally draws its own private RNG seed
/// inside the ceremony functions.)
pub fn fresh_eid() -> Result<[u8; 32], TssError> {
    pocketskynet_core::random::bytes::<32>().map_err(|_| TssError::Entropy)
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
        // The protocol's security argument needs fresh instance ids; two
        // draws colliding would mean the entropy source is broken.
        assert_ne!(fresh_eid().unwrap(), fresh_eid().unwrap());
    }
}
