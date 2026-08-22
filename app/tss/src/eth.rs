//! Ethereum-side helpers: address derivation and conversion of a CGGMP24
//! signature into a recoverable Ethereum signature.
//!
//! Deliberately built on `pocketskynet_core`'s primitives (`eip191`,
//! `WalletAddress`) rather than a second Ethereum library, so a TSS wallet's
//! address and signature encoding cannot drift from the mnemonic wallet's.

use cggmp24::key_share::AnyKeyShare;
use k256::ecdsa::{RecoveryId, VerifyingKey};
use pocketskynet_core::{eip191, WalletAddress};

use crate::{Curve, Share, TssError};

/// The shared (aggregate) public key as a k256 verifying key.
pub fn verifying_key(share: &Share) -> Result<VerifyingKey, TssError> {
    let bytes = share.shared_public_key().to_bytes(true);
    VerifyingKey::from_sec1_bytes(bytes.as_ref())
        .map_err(|e| TssError::Ceremony(format!("invalid shared public key: {e}")))
}

/// Ethereum address of the shared public key — the wallet's identity.
pub fn eth_address(share: &Share) -> Result<WalletAddress, TssError> {
    let vk = verifying_key(share)?;
    let public = k256::PublicKey::from(&vk);
    Ok(eip191::address_from_public_key(&public))
}

/// A recoverable, low-s Ethereum signature.
#[derive(Debug, Clone, Copy)]
pub struct TssSignature {
    pub r: [u8; 32],
    pub s: [u8; 32],
    /// y-parity: 0 or 1.
    pub v: u8,
}

impl TssSignature {
    /// 65-byte `r ‖ s ‖ v` with v in {27, 28} — the `personal_sign` wire form.
    pub fn to_rsv_bytes(&self) -> [u8; 65] {
        let mut out = [0u8; 65];
        out[..32].copy_from_slice(&self.r);
        out[32..64].copy_from_slice(&self.s);
        out[64] = 27 + self.v;
        out
    }

    /// `0x` + 130 hex chars, byte-compatible with `eip191::personal_sign`'s
    /// output and therefore with everything that parses it.
    pub fn to_hex(&self) -> String {
        format!("0x{}", hex::encode(self.to_rsv_bytes()))
    }

    /// The `r ‖ s` halves as one 64-byte array, for EIP-155 tx assembly.
    pub fn rs_bytes(&self) -> [u8; 64] {
        let mut out = [0u8; 64];
        out[..32].copy_from_slice(&self.r);
        out[32..].copy_from_slice(&self.s);
        out
    }
}

/// Converts a CGGMP24 signature into a recoverable Ethereum signature over
/// `prehash`, normalizing to low-s and recovering the parity bit `v` by
/// trial recovery against the wallet's public key.
///
/// cggmp24's `normalize_s` already yields low-s, and because `v` is recovered
/// *after* normalization the parity is consistent with the final `s`. Only
/// v ∈ {0, 1} is tried: r ≥ curve order has probability ≈ 2⁻¹²⁸ and a
/// signature landing there would fail verification everywhere else anyway.
pub fn to_eth_signature(
    share: &Share,
    sig: cggmp24::Signature<Curve>,
    prehash: [u8; 32],
) -> Result<TssSignature, TssError> {
    let sig = sig.normalize_s();
    let mut rs = [0u8; 64];
    sig.write_to_slice(&mut rs);
    let mut r = [0u8; 32];
    let mut s = [0u8; 32];
    r.copy_from_slice(&rs[..32]);
    s.copy_from_slice(&rs[32..]);

    let expected = verifying_key(share)?;
    let k_sig = k256::ecdsa::Signature::from_slice(&rs)
        .map_err(|e| TssError::Ceremony(format!("invalid (r,s) signature: {e}")))?;

    for v in 0u8..=1 {
        let rec_id = RecoveryId::try_from(v).expect("0 and 1 are valid recovery ids");
        if let Ok(recovered) = VerifyingKey::recover_from_prehash(&prehash, &k_sig, rec_id) {
            if recovered == expected {
                return Ok(TssSignature { r, s, v });
            }
        }
    }
    Err(TssError::Recovery)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_encoding_matches_personal_sign_exactly() {
        // 65 bytes, v ∈ {27, 28}, 0x + 130 lowercase hex — the shape
        // `eip191::parse_signature` accepts without translation.
        let sig = TssSignature {
            r: [0x11; 32],
            s: [0x22; 32],
            v: 1,
        };
        let bytes = sig.to_rsv_bytes();
        assert_eq!(bytes[64], 28);
        assert_eq!(&bytes[..32], &[0x11; 32]);
        assert_eq!(&bytes[32..64], &[0x22; 32]);
        let hex = sig.to_hex();
        assert_eq!(hex.len(), 132);
        assert!(hex.starts_with("0x"));
        assert!(hex.ends_with("1c"));
        assert_eq!(sig.rs_bytes()[..32], [0x11; 32]);
        assert_eq!(sig.rs_bytes()[32..], [0x22; 32]);
    }
}
