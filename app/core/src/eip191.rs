//! EIP-191 `personal_sign` — the signature scheme every FruitNation identity
//! operation runs through.
//!
//! Three places depend on it: the login challenge, the encryption-key
//! derivation message (§3), and the public-key binding message (§4). The middle
//! one is why determinism is load-bearing: the E2EE private key is
//! `keccak256(signature)`, so a non-deterministic nonce would mint a *different*
//! identity key on every device and silently lock the user out of their own
//! rooms. `k256` signs with RFC 6979, which makes the signature a pure function
//! of (key, message).

use elliptic_curve::sec1::ToEncodedPoint;
use k256::ecdsa::{RecoveryId, Signature, SigningKey, VerifyingKey};
use k256::{PublicKey, SecretKey};
use sha3::{Digest, Keccak256};

use crate::crypto::CryptoError;
use crate::ids::WalletAddress;

/// The literal EIP-191 personal-sign prefix. The leading byte is `0x19`, a
/// single control character — not the four ASCII characters `\`, `x`, `1`, `9`.
const PREFIX: &[u8] = b"\x19Ethereum Signed Message:\n";

/// The EIP-191 digest: `keccak256(0x19 ‖ "Ethereum Signed Message:\n" ‖ len ‖ msg)`.
///
/// `len` is the **UTF-8 byte length** in decimal ASCII, not the character
/// count. The two differ the moment a message contains a non-ASCII character,
/// and a wrong length produces a valid-looking signature that recovers to the
/// wrong address.
///
/// This is original Keccak (`0x01` padding), **not** NIST SHA3-256 (`0x06`).
/// `sha3::Keccak256`, never `sha3::Sha3_256`.
pub fn eip191_digest(message: &str) -> [u8; 32] {
    let bytes = message.as_bytes();
    let mut hasher = Keccak256::new();
    hasher.update(PREFIX);
    hasher.update(bytes.len().to_string().as_bytes());
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Sign `message` with `secret`, returning the 132-character `0x`-prefixed
/// `r ‖ s ‖ v` hex string that `ethers.verifyMessage` expects.
///
/// `s` is low-S normalised and the recovery bit flipped to match — `k256` does
/// this inside `sign_prehash_recoverable`, and ethers v6 *rejects* high-S
/// signatures, so emitting a raw high-S signature would be silently unverifiable
/// server-side.
///
/// `v` is `recovery_id + 27`. Raw recovery ids `0`/`1` are not the wire format;
/// every canonical vector and the server's zod schema use 27/28.
pub fn personal_sign(secret: &SecretKey, message: &str) -> Result<String, CryptoError> {
    let signing_key = SigningKey::from(secret);
    let digest = eip191_digest(message);
    let (signature, recovery_id) = signing_key
        .sign_prehash_recoverable(&digest)
        .map_err(|_| CryptoError::SigningFailed)?;

    let mut out = [0u8; 65];
    out[..64].copy_from_slice(&signature.to_bytes());
    out[64] = recovery_id.to_byte() + 27;
    Ok(format!("0x{}", hex::encode(out)))
}

/// A parsed 65-byte signature.
struct ParsedSignature {
    signature: Signature,
    recovery_id: RecoveryId,
}

/// Parse `0x`-prefixed `r ‖ s ‖ v`.
///
/// `v` of 27/28 is the wire format; 0/1 is also accepted because
/// `ethers.Signature` tolerates it on input, and a verifier that is *stricter*
/// than the server would reject signatures the server considers valid.
///
/// High-S is rejected outright (OQ-4): ethers v6 enforces canonicality, so
/// accepting a high-S signature here would mean accepting a key binding the
/// server would later refuse — and it would make signatures malleable, since
/// `(r, n-s)` is equally valid over the same digest.
fn parse_signature(signature_hex: &str) -> Result<ParsedSignature, CryptoError> {
    let stripped = signature_hex
        .strip_prefix("0x")
        .or_else(|| signature_hex.strip_prefix("0X"))
        .ok_or(CryptoError::InvalidSignature)?;
    if stripped.len() != 130 {
        return Err(CryptoError::InvalidSignature);
    }
    let bytes = hex::decode(stripped).map_err(|_| CryptoError::InvalidSignature)?;

    let v = match bytes[64] {
        27 | 28 => bytes[64] - 27,
        0 | 1 => bytes[64],
        _ => return Err(CryptoError::InvalidSignature),
    };
    let recovery_id = RecoveryId::from_byte(v).ok_or(CryptoError::InvalidSignature)?;

    // `from_slice` rejects r == 0 and s == 0.
    let signature =
        Signature::from_slice(&bytes[..64]).map_err(|_| CryptoError::InvalidSignature)?;
    if signature.normalize_s().is_some() {
        // `normalize_s` returns `Some` only when the input was high-S.
        return Err(CryptoError::InvalidSignature);
    }

    Ok(ParsedSignature {
        signature,
        recovery_id,
    })
}

/// Recover the public key that produced an EIP-191 signature over `message`.
pub fn recover_public_key(message: &str, signature_hex: &str) -> Result<PublicKey, CryptoError> {
    let parsed = parse_signature(signature_hex)?;
    let digest = eip191_digest(message);
    let verifying =
        VerifyingKey::recover_from_prehash(&digest, &parsed.signature, parsed.recovery_id)
            .map_err(|_| CryptoError::InvalidSignature)?;
    Ok(PublicKey::from(&verifying))
}

/// Recover the signer's wallet address from an EIP-191 signature.
///
/// The returned address is lowercase (a [`WalletAddress`] invariant), so
/// comparing it to a stored address never needs a case-folding step that
/// someone could forget.
pub fn recover_address(message: &str, signature_hex: &str) -> Result<WalletAddress, CryptoError> {
    Ok(address_from_public_key(&recover_public_key(
        message,
        signature_hex,
    )?))
}

/// Verify that `signature_hex` over `message` was produced by `expected`.
///
/// Returns `false` — never an error — for every failure mode, so call sites
/// cannot accidentally treat a malformed signature as "not verified yet" and
/// continue.
pub fn verify_signature(message: &str, signature_hex: &str, expected: &WalletAddress) -> bool {
    matches!(recover_address(message, signature_hex), Ok(a) if &a == expected)
}

/// `address = keccak256(X ‖ Y)[12..32]`, i.e. the **last** 20 bytes of the hash
/// of the 64-byte public key with its `0x04` SEC1 prefix dropped.
///
/// Hashing the 65-byte encoding *including* the prefix is the classic mistake:
/// it produces a well-formed address that belongs to nobody.
pub fn address_from_public_key(public_key: &PublicKey) -> WalletAddress {
    let encoded = public_key.to_encoded_point(false);
    let hash = Keccak256::digest(&encoded.as_bytes()[1..]);
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&hash[12..32]);
    WalletAddress::from_bytes(&addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(hex_str: &str) -> SecretKey {
        SecretKey::from_slice(&hex::decode(hex_str.trim_start_matches("0x")).unwrap()).unwrap()
    }

    const V1_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const V1_ADDR: &str = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266";
    const V1_MSG: &str = "FruitNation Encryption Key Derivation\n\nAddress: 0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266\nPurpose: End-to-end encryption only";
    const V1_SIG: &str = "0xe98d833febe631493b5589273c82b3a66df264a47ca2c02754c0f9dffaeceaa601306b1edf3328bfe7f46b747def4fd6b4f4b400dcc76955a6fb33a47ac6ba531b";

    #[test]
    fn digest_matches_the_worked_example() {
        assert_eq!(V1_MSG.len(), 126);
        assert_eq!(
            hex::encode(eip191_digest(V1_MSG)),
            "50da9eedfc742dd88128600e67d046124066add1cf9907a747837d823d9c7677"
        );
    }

    #[test]
    fn prefix_bytes_are_the_documented_ones() {
        let mut prefix = PREFIX.to_vec();
        prefix.extend_from_slice(b"126");
        assert_eq!(
            hex::encode(&prefix),
            "19457468657265756d205369676e6564204d6573736167653a0a313236"
        );
    }

    #[test]
    fn length_is_counted_in_utf8_bytes_not_characters() {
        // "🍓" is 4 UTF-8 bytes but 1 character; the digest must use 4.
        let msg = "🍓";
        let mut hasher = Keccak256::new();
        hasher.update(PREFIX);
        hasher.update(b"4");
        hasher.update(msg.as_bytes());
        let expected: [u8; 32] = hasher.finalize().into();
        assert_eq!(eip191_digest(msg), expected);
    }

    #[test]
    fn signature_matches_the_canonical_vector() {
        assert_eq!(personal_sign(&secret(V1_KEY), V1_MSG).unwrap(), V1_SIG);
    }

    #[test]
    fn signing_is_deterministic_across_calls() {
        let sk = secret(V1_KEY);
        assert_eq!(
            personal_sign(&sk, "anything at all").unwrap(),
            personal_sign(&sk, "anything at all").unwrap()
        );
    }

    #[test]
    fn signature_is_low_s_and_v_is_27_or_28() {
        let sk = secret(V1_KEY);
        for msg in ["a", "b", "c", "d", "e", "f", "g", "h"] {
            let sig = personal_sign(&sk, msg).unwrap();
            let bytes = hex::decode(&sig[2..]).unwrap();
            assert!(
                matches!(bytes[64], 27 | 28),
                "v must be 27/28, got {}",
                bytes[64]
            );
            let parsed = Signature::from_slice(&bytes[..64]).unwrap();
            assert!(parsed.normalize_s().is_none(), "s must already be low");
        }
    }

    #[test]
    fn recovery_round_trips_and_is_lowercase() {
        let expected = WalletAddress::new(V1_ADDR).unwrap();
        assert_eq!(recover_address(V1_MSG, V1_SIG).unwrap(), expected);
        assert!(verify_signature(V1_MSG, V1_SIG, &expected));
    }

    #[test]
    fn recovery_matches_the_second_legacy_vector() {
        let msg = "FruitNation Encryption Key Derivation\n\nAddress: 0xfcad0b19bb29d4674531d6f115237e16afce377c\nPurpose: End-to-end encryption only";
        let sig = "0x41d928b05cdef74a60021241437fb7697f814cd8d4401ce892182a0c2ef0adbe670f7276e6fad3e125111887d22d391b08ec542ad4069fafca16c9e58da57a401b";
        assert_eq!(
            recover_address(msg, sig).unwrap().as_str(),
            "0xfcad0b19bb29d4674531d6f115237e16afce377c"
        );
    }

    #[test]
    fn a_changed_message_recovers_to_a_different_address() {
        let expected = WalletAddress::new(V1_ADDR).unwrap();
        let tampered = V1_MSG.replace("Purpose", "purpose");
        assert!(!verify_signature(&tampered, V1_SIG, &expected));
    }

    #[test]
    fn malformed_signatures_are_rejected_not_ignored() {
        let bad_cases: Vec<String> = vec![
            String::new(),
            "0x".to_string(),
            V1_SIG.trim_start_matches("0x").to_string(), // missing prefix
            V1_SIG[..130].to_string(),                   // too short
            format!("{V1_SIG}00"),                       // too long
            format!("{}ff", &V1_SIG[..130]),             // v = 255
            format!("0x{}", "0".repeat(130)),            // r = s = 0
            format!("0x{}zz", &V1_SIG[2..128]),          // non-hex
        ];
        for bad in bad_cases {
            assert!(
                recover_address(V1_MSG, &bad).is_err(),
                "should have rejected {bad:?}"
            );
        }
    }

    #[test]
    fn raw_recovery_ids_are_tolerated_like_ethers_does() {
        let mut bytes = hex::decode(&V1_SIG[2..]).unwrap();
        bytes[64] -= 27; // 27 -> 0
        let sig = format!("0x{}", hex::encode(&bytes));
        assert_eq!(recover_address(V1_MSG, &sig).unwrap().as_str(), V1_ADDR);
    }

    #[test]
    fn high_s_signatures_are_rejected_to_match_ethers() {
        use elliptic_curve::PrimeField;
        use k256::{FieldBytes, Scalar};

        // (r, n - s) is an equally valid signature over the same digest; ethers
        // v6 refuses it, so we must too, or we would accept bindings the server
        // would later reject.
        let bytes = hex::decode(&V1_SIG[2..]).unwrap();
        let r = FieldBytes::clone_from_slice(&bytes[..32]);
        let s = Scalar::from_repr(FieldBytes::clone_from_slice(&bytes[32..64])).unwrap();

        let mut malleable = Vec::with_capacity(65);
        malleable.extend_from_slice(&r);
        malleable.extend_from_slice(&(-s).to_bytes());
        malleable.push(bytes[64] ^ 1); // flip the recovery bit to match

        assert_eq!(
            recover_address(V1_MSG, &format!("0x{}", hex::encode(&malleable))),
            Err(CryptoError::InvalidSignature)
        );
    }

    #[test]
    fn address_derivation_drops_the_sec1_prefix() {
        let sk = secret(V1_KEY);
        assert_eq!(address_from_public_key(&sk.public_key()).as_str(), V1_ADDR);

        // The wrong-but-plausible variant: hashing all 65 bytes including the
        // `0x04` prefix yields a well-formed address that belongs to nobody.
        let encoded = sk.public_key().to_encoded_point(false);
        let wrong = Keccak256::digest(encoded.as_bytes());
        assert_ne!(hex::encode(&wrong[12..32]), V1_ADDR[2..]);
    }
}
