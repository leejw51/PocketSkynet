//! BIP-39 mnemonics, BIP-32 derivation, and the secp256k1 wallet identity.
//!
//! FruitNation authenticates with an Ethereum wallet, so "log in" means "prove
//! control of a secp256k1 key at `m/44'/60'/0'/0/{index}`". Everything in this
//! module exists to get from a phrase the user can write on paper to that key,
//! reproducibly, on both native and wasm targets.
//!
//! BIP-32 is implemented here by hand rather than pulled in as a crate. The
//! child-key derivation function is about forty lines over `hmac`, `sha2` and
//! `k256`, and the usual crates either link the libsecp256k1 C library (which
//! does not build cleanly for `wasm32-unknown-unknown`) or drag in a second,
//! duplicate copy of the curve arithmetic. Fewer dependencies on the path
//! between a seed phrase and a private key is a security property in itself.

use bip39::Mnemonic;
use elliptic_curve::sec1::ToEncodedPoint;
use elliptic_curve::PrimeField;
use hmac::{Hmac, Mac};
use k256::{FieldBytes, NonZeroScalar, PublicKey, Scalar, SecretKey};
use sha2::Sha512;

use crate::crypto::CryptoError;
use crate::eip191;
use crate::ids::WalletAddress;
use crate::random;

type HmacSha512 = Hmac<Sha512>;

/// BIP-32 marks hardened child indices by setting the high bit.
const HARDENED: u32 = 0x8000_0000;

/// The BIP-44 path FruitNation derives from: `m/44'/60'/0'/0/{index}`.
///
/// Coin type 60 is Ethereum. This is the same path MetaMask, Ledger and every
/// other Ethereum wallet uses, which is what makes a FruitNation mnemonic
/// importable elsewhere — deviating would produce a valid but orphaned address.
const ETH_PATH_PREFIX: [u32; 4] = [44 | HARDENED, 60 | HARDENED, HARDENED, 0];

/// Render the derivation path for display or logging.
pub fn derivation_path(index: u32) -> String {
    format!("m/44'/60'/0'/0/{index}")
}

/// How many words to generate.
///
/// 12 words is 128 bits of entropy, 24 words is 256. Both are far beyond
/// brute-force reach; 24 exists because some users (and some hardware wallets)
/// insist on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MnemonicLength {
    /// 12 words / 128 bits of entropy.
    Words12,
    /// 24 words / 256 bits of entropy.
    Words24,
}

impl MnemonicLength {
    /// Entropy size in bytes, per BIP-39's `ENT = MS * 32 / 3`.
    fn entropy_bytes(self) -> usize {
        match self {
            MnemonicLength::Words12 => 16,
            MnemonicLength::Words24 => 32,
        }
    }
}

/// Generate a fresh BIP-39 English mnemonic.
///
/// The entropy is drawn through [`crate::random`] — see that module for why the
/// crate has exactly one entropy source and why a draw that fails comes back as
/// an error rather than as twelve words anybody could guess — rather than via
/// `bip39`'s optional `rand` feature, which would pull `rand` back into the
/// dependency graph this consolidation removed it from.
///
/// The buffer is 32 bytes regardless of length and only the first `ENT/8` are
/// filled and used, which keeps the two cases one code path. `Mnemonic::from_entropy`
/// reads the slice it is given, so the unused tail never reaches the phrase.
pub fn generate_mnemonic(length: MnemonicLength) -> Result<String, CryptoError> {
    let mut entropy = [0u8; 32];
    let entropy = &mut entropy[..length.entropy_bytes()];
    random::fill(entropy)?;
    let mnemonic = Mnemonic::from_entropy(entropy).map_err(|_| CryptoError::InvalidMnemonic)?;
    Ok(mnemonic.to_string())
}

/// Parse and checksum-validate a BIP-39 English phrase.
///
/// The phrase is trimmed first, matching the reference client's
/// `Mnemonic.fromPhrase(phrase.trim())`. Users paste phrases with trailing
/// newlines constantly, and a whitespace-only difference must not be the
/// difference between "your wallet" and "invalid mnemonic".
pub fn parse_mnemonic(phrase: &str) -> Result<Mnemonic, CryptoError> {
    Mnemonic::parse(phrase.trim()).map_err(|_| CryptoError::InvalidMnemonic)
}

/// Whether a phrase is a valid BIP-39 English mnemonic (checksum included).
pub fn validate_mnemonic(phrase: &str) -> bool {
    parse_mnemonic(phrase).is_ok()
}

/// The BIP-39 seed: PBKDF2-HMAC-SHA512, 2048 iterations, 64 bytes out.
///
/// The passphrase is always the empty string — FruitNation never uses a BIP-39
/// passphrase, so a non-empty one would derive an address the server has never
/// seen. This is a deliberate protocol constraint, not an oversight.
pub fn seed_from_mnemonic(mnemonic: &Mnemonic) -> [u8; 64] {
    mnemonic.to_seed("")
}

/// A BIP-32 extended private key: the scalar plus its chain code.
///
/// Kept private to this module. The chain code is as sensitive as the key
/// itself — leaking it plus one child key exposes every sibling.
struct ExtendedPrivateKey {
    key: [u8; 32],
    chain_code: [u8; 32],
}

impl ExtendedPrivateKey {
    /// `I = HMAC-SHA512("Bitcoin seed", seed)`; left half is the key, right half
    /// the chain code. The odd-looking constant is BIP-32's, and it is shared
    /// across every coin — the Ethereum-ness lives entirely in the path.
    fn from_seed(seed: &[u8]) -> Result<Self, CryptoError> {
        let mut mac =
            <HmacSha512>::new_from_slice(b"Bitcoin seed").expect("HMAC accepts any key length");
        mac.update(seed);
        let i = mac.finalize().into_bytes();

        let mut key = [0u8; 32];
        let mut chain_code = [0u8; 32];
        key.copy_from_slice(&i[..32]);
        chain_code.copy_from_slice(&i[32..]);

        // Reject a master key outside [1, n-1) rather than let it silently
        // become something else downstream.
        SecretKey::from_slice(&key).map_err(|_| CryptoError::KeyDerivation)?;
        Ok(Self { key, chain_code })
    }

    /// BIP-32 CKDpriv.
    ///
    /// Hardened children hash `0x00 ‖ ser256(k_par) ‖ ser32(i)`; normal children
    /// hash the **compressed** public key instead. Using the uncompressed form
    /// here is the classic silent-divergence bug: derivation still "works", it
    /// just produces a different wallet than every other implementation.
    fn derive_child(&self, index: u32) -> Result<Self, CryptoError> {
        let parent = SecretKey::from_slice(&self.key).map_err(|_| CryptoError::KeyDerivation)?;

        let mut mac =
            <HmacSha512>::new_from_slice(&self.chain_code).expect("HMAC accepts any key length");
        if index >= HARDENED {
            mac.update(&[0x00]);
            mac.update(&self.key);
        } else {
            mac.update(parent.public_key().to_encoded_point(true).as_bytes());
        }
        mac.update(&index.to_be_bytes());
        let i = mac.finalize().into_bytes();

        // `from_repr` fails when IL >= n. BIP-32 says to skip to the next index
        // in that case; we surface the error instead, because silently moving
        // the path would hand the caller a different wallet than they asked for.
        let il = Option::<Scalar>::from(Scalar::from_repr(FieldBytes::clone_from_slice(&i[..32])))
            .ok_or(CryptoError::KeyDerivation)?;
        let child = il + *parent.to_nonzero_scalar().as_ref();
        let child = Option::<NonZeroScalar>::from(NonZeroScalar::new(child))
            .ok_or(CryptoError::KeyDerivation)?;

        let mut chain_code = [0u8; 32];
        chain_code.copy_from_slice(&i[32..]);
        Ok(Self {
            key: child.to_bytes().into(),
            chain_code,
        })
    }
}

/// A secp256k1 wallet identity: the signing key plus its Ethereum address.
///
/// The private key lives in a [`k256::SecretKey`], which zeroizes its scalar on
/// drop; the raw bytes are never held in a plain array on this struct so a
/// `Wallet` moved around in memory does not leave copies behind.
pub struct Wallet {
    secret: SecretKey,
    address: WalletAddress,
}

impl core::fmt::Debug for Wallet {
    /// Prints the address only. A `Debug` impl that could print key material
    /// eventually ends up in a log file.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Wallet")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl Wallet {
    /// Derive the wallet at `m/44'/60'/0'/0/{index}` from a mnemonic phrase.
    pub fn from_mnemonic(phrase: &str, index: u32) -> Result<Self, CryptoError> {
        let mnemonic = parse_mnemonic(phrase)?;
        Self::from_seed(&seed_from_mnemonic(&mnemonic), index)
    }

    /// Derive the wallet at `m/44'/60'/0'/0/{index}` from a BIP-39 seed.
    pub fn from_seed(seed: &[u8], index: u32) -> Result<Self, CryptoError> {
        if index >= HARDENED {
            // The address index is the *normal* (non-hardened) leg of the path.
            return Err(CryptoError::KeyDerivation);
        }
        let mut node = ExtendedPrivateKey::from_seed(seed)?;
        for step in ETH_PATH_PREFIX {
            node = node.derive_child(step)?;
        }
        node = node.derive_child(index)?;
        Self::from_private_key_bytes(&node.key)
    }

    /// Import a raw 32-byte private key.
    ///
    /// Used for MetaMask-style imports and for tests that pin a known address.
    /// Rejects `0` and anything `≥ n` — a scalar outside `[1, n)` is not a key,
    /// and accepting one would produce an address nobody can sign for.
    pub fn from_private_key_bytes(bytes: &[u8; 32]) -> Result<Self, CryptoError> {
        let secret = SecretKey::from_slice(bytes).map_err(|_| CryptoError::InvalidPrivateKey)?;
        Ok(Self::from_secret(secret))
    }

    /// Wrap an already-validated secret key: derive its address and pair them.
    ///
    /// The one constructor that takes a `SecretKey` rather than bytes, so the
    /// two callers that already hold a validated scalar — the byte import above,
    /// which parsed it, and [`Self::random`], which drew it — share the
    /// address-derivation step instead of copying it. `SecretKey` cannot hold a
    /// value outside `[1, n)`, so there is nothing left to reject here; that is
    /// exactly why this is infallible and its callers decide their own errors.
    fn from_secret(secret: SecretKey) -> Self {
        let address = eip191::address_from_public_key(&secret.public_key());
        Self { secret, address }
    }

    /// Import a private key from hex, with or without the `0x` prefix.
    pub fn from_private_key_hex(hex_str: &str) -> Result<Self, CryptoError> {
        let stripped = hex_str
            .trim()
            .trim_start_matches("0x")
            .trim_start_matches("0X");
        let mut bytes = [0u8; 32];
        hex::decode_to_slice(stripped, &mut bytes).map_err(|_| CryptoError::InvalidPrivateKey)?;
        Self::from_private_key_bytes(&bytes)
    }

    /// Generate a brand-new random wallet with no mnemonic behind it.
    ///
    /// Prefer [`generate_mnemonic`] for anything a human has to back up: a raw
    /// key that only exists in one place is a key that gets lost.
    ///
    /// [`random::secret_key`] rejection-samples, so unlike a draw fed straight
    /// into [`Self::from_private_key_bytes`] this cannot report
    /// [`CryptoError::InvalidPrivateKey`] for a scalar that merely landed
    /// outside `[1, n)`. The only error it can return is
    /// [`CryptoError::Randomness`], which means exactly one thing: the OS would
    /// not produce entropy.
    pub fn random() -> Result<Self, CryptoError> {
        Ok(Self::from_secret(random::secret_key()?))
    }

    /// The wallet's lowercase Ethereum address — the protocol's primary key.
    pub fn address(&self) -> &WalletAddress {
        &self.address
    }

    /// The underlying secp256k1 secret, for the signing and ECDH entry points.
    pub fn secret_key(&self) -> &SecretKey {
        &self.secret
    }

    /// The wallet's public key.
    pub fn public_key(&self) -> PublicKey {
        self.secret.public_key()
    }

    /// The private key as `0x`-prefixed lowercase hex.
    ///
    /// Exists for export/backup flows. Every call site is a place where key
    /// material escapes into a `String` that will not be zeroized — keep them
    /// countable.
    pub fn private_key_hex(&self) -> String {
        format!("0x{}", hex::encode(self.secret.to_bytes()))
    }

    /// EIP-191 `personal_sign` with this wallet's key.
    pub fn personal_sign(&self, message: &str) -> Result<String, CryptoError> {
        eip191::personal_sign(&self.secret, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical all-`abandon` BIP-39 test vector.
    const ABANDON: &str =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    #[test]
    fn bip39_seed_matches_the_reference_vector() {
        // Trezor's BIP-39 vector for 128 bits of zero entropy, empty passphrase.
        let mnemonic = parse_mnemonic(ABANDON).unwrap();
        assert_eq!(
            hex::encode(seed_from_mnemonic(&mnemonic)),
            "5eb00bbddcf069084889a8ab9155568165f5c453ccb85e70811aaed6f6da5fc19a5ac40b389cd370d086206dec8aa6c43daea6690f20ad3d8d48b2d2ce9e38e4"
        );
    }

    #[test]
    fn bip44_ethereum_path_matches_metamask() {
        // `m/44'/60'/0'/0/0` of the all-abandon phrase — the single most widely
        // reproduced Ethereum test address there is.
        let wallet = Wallet::from_mnemonic(ABANDON, 0).unwrap();
        assert_eq!(
            wallet.address().as_str(),
            "0x9858effd232b4033e47d90003d41ec34ecaeda94"
        );
        assert_eq!(
            wallet.private_key_hex(),
            "0x1ab42cc412b618bdea3a599e3c9bae199ebf030895b039e9db1e30dafb12b727"
        );
    }

    #[test]
    fn successive_indices_give_different_wallets() {
        let a = Wallet::from_mnemonic(ABANDON, 0).unwrap();
        let b = Wallet::from_mnemonic(ABANDON, 1).unwrap();
        assert_eq!(
            b.address().as_str(),
            "0x6fac4d18c912343bf86fa7049364dd4e424ab9c0"
        );
        assert_ne!(a.address(), b.address());
        assert_eq!(derivation_path(1), "m/44'/60'/0'/0/1");
    }

    #[test]
    fn derivation_is_deterministic() {
        let a = Wallet::from_mnemonic(ABANDON, 3).unwrap();
        let b = Wallet::from_mnemonic(ABANDON, 3).unwrap();
        assert_eq!(a.address(), b.address());
        assert_eq!(a.private_key_hex(), b.private_key_hex());
    }

    #[test]
    fn phrases_are_trimmed_before_parsing() {
        let padded = format!("  \n{ABANDON}\t ");
        assert_eq!(
            Wallet::from_mnemonic(&padded, 0).unwrap().address(),
            Wallet::from_mnemonic(ABANDON, 0).unwrap().address()
        );
    }

    #[test]
    fn invalid_mnemonics_are_rejected() {
        let bad_checksum = ABANDON.replace("about", "abandon");
        for phrase in [
            "",
            "not words at all",
            &bad_checksum,
            // 11 words: a valid-looking but wrong-length phrase.
            &ABANDON
                .split_whitespace()
                .take(11)
                .collect::<Vec<_>>()
                .join(" "),
        ] {
            assert!(
                !validate_mnemonic(phrase),
                "should have rejected {phrase:?}"
            );
            assert_eq!(
                Wallet::from_mnemonic(phrase, 0).err(),
                Some(CryptoError::InvalidMnemonic)
            );
        }
    }

    #[test]
    fn generated_mnemonics_have_the_requested_length_and_validate() {
        for (length, words) in [(MnemonicLength::Words12, 12), (MnemonicLength::Words24, 24)] {
            let phrase = generate_mnemonic(length).unwrap();
            assert_eq!(phrase.split_whitespace().count(), words);
            assert!(validate_mnemonic(&phrase));
        }
        // Two calls must not collide.
        assert_ne!(
            generate_mnemonic(MnemonicLength::Words12).unwrap(),
            generate_mnemonic(MnemonicLength::Words12).unwrap()
        );
    }

    #[test]
    fn raw_private_key_import_matches_the_documented_addresses() {
        for (key, addr) in [
            (
                "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
                "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
            ),
            (
                "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "0xfcad0b19bb29d4674531d6f115237e16afce377c",
            ),
        ] {
            let wallet = Wallet::from_private_key_hex(key).unwrap();
            assert_eq!(wallet.address().as_str(), addr);
            assert_eq!(wallet.private_key_hex(), key);
            // The prefix is optional on import.
            let bare = Wallet::from_private_key_hex(&key[2..]).unwrap();
            assert_eq!(bare.address().as_str(), addr);
        }
    }

    #[test]
    fn checksummed_display_form_matches_eip55() {
        let wallet = Wallet::from_private_key_hex(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        )
        .unwrap();
        assert_eq!(
            wallet.address().to_checksummed(),
            "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
        );
    }

    #[test]
    fn invalid_private_keys_are_rejected() {
        for bad in [
            "0x00",                           // too short
            &format!("0x{}", "0".repeat(64)), // zero scalar
            // secp256k1 group order n — the first value that is out of range.
            "0xfffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141",
            "0xzz00000000000000000000000000000000000000000000000000000000000000", // non-hex
        ] {
            assert_eq!(
                Wallet::from_private_key_hex(bad).err(),
                Some(CryptoError::InvalidPrivateKey),
                "should have rejected {bad}"
            );
        }
    }

    #[test]
    fn hardened_address_indices_are_refused() {
        let mnemonic = parse_mnemonic(ABANDON).unwrap();
        assert_eq!(
            Wallet::from_seed(&seed_from_mnemonic(&mnemonic), HARDENED).err(),
            Some(CryptoError::KeyDerivation)
        );
    }

    #[test]
    fn random_wallets_are_distinct() {
        assert_ne!(
            Wallet::random().unwrap().address(),
            Wallet::random().unwrap().address()
        );
    }

    /// A generated phrase has to be a phrase the rest of the world accepts, and
    /// it has to lead back to one address. Length and checksum are pinned
    /// above; what this adds is the part a user actually depends on — write the
    /// twelve words down, type them in again, and land on the same account.
    ///
    /// The re-derivation goes through the *public* entry point rather than
    /// reusing the entropy, so a generator that produced a phrase this crate
    /// alone could parse would fail here.
    #[test]
    fn a_generated_mnemonic_round_trips_to_its_address() {
        for _ in 0..4 {
            let phrase = generate_mnemonic(MnemonicLength::Words12).unwrap();
            let wallet = Wallet::from_mnemonic(&phrase, 0).unwrap();

            // Same phrase, same index, same address — twice, from scratch.
            assert_eq!(
                Wallet::from_mnemonic(&phrase, 0).unwrap().address(),
                wallet.address()
            );
            // And the BIP-39 → BIP-32 path agrees with the seed-level one, so
            // the phrase is not merely *parseable* but carries the entropy the
            // derivation actually used.
            let seed = seed_from_mnemonic(&parse_mnemonic(&phrase).unwrap());
            assert_eq!(
                Wallet::from_seed(&seed, 0).unwrap().address(),
                wallet.address()
            );

            // Index 1 is a different account off the same phrase. A generator
            // returning a constant would pass every assertion above; this is
            // the one that also needs the address to be a function of the
            // words rather than of nothing.
            assert_ne!(
                Wallet::from_mnemonic(&phrase, 1).unwrap().address(),
                wallet.address()
            );
        }
    }

    /// The failure that must never be silent. With the OS refusing entropy,
    /// both generators return `Randomness` — not a fixed phrase, not the
    /// all-`abandon` wallet that a zero-filled buffer would become, not a wallet
    /// at all. Returning `Err` *is* the "not a fallback phrase" guarantee:
    /// there is no `Ok` value to inspect, which is the whole point. (An earlier
    /// draft asserted the returned phrase was not `ABANDON` here — but under the
    /// armed guard the call is always `Err`, so `.ok()` was always `None` and
    /// the assertion could never fail. It proved nothing and is gone.)
    #[test]
    fn generation_refuses_rather_than_falling_back_when_entropy_fails() {
        let _guard = crate::random::FailureGuard::new();

        for length in [MnemonicLength::Words12, MnemonicLength::Words24] {
            assert_eq!(
                generate_mnemonic(length).err(),
                Some(CryptoError::Randomness)
            );
        }
        assert_eq!(Wallet::random().err(), Some(CryptoError::Randomness));
    }

    #[test]
    fn debug_never_prints_key_material() {
        let wallet = Wallet::from_private_key_hex(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        )
        .unwrap();
        let rendered = format!("{wallet:?}");
        assert!(!rendered.contains("ac0974"), "{rendered}");
        assert!(rendered.contains("0xf39fd6e5"));
    }
}
