//! Passphrase-sealed **share files** (CRYPTO.md §15.4).
//!
//! Custody is the user's, not the server's: key generation hands back `n`
//! sealed files — one per party — and the server keeps **nothing** on disk.
//! Any `t` of the files sign; fewer than `t` can do nothing; losing up to
//! `n − t` of them loses nothing. Each file carries the whole E2EE identity
//! (§15.2) so any quorum also recovers messaging.
//!
//! The seal, per file:
//!
//! ```text
//! okm    = PBKDF2-HMAC-SHA256(passphrase, salt, 600_000) → 64 bytes
//! encKey = okm[0..32]   macKey = okm[32..64]
//! ct     = AES-256-CBC(encKey, iv) over the secrets JSON (PKCS-7)
//! mac    = HMAC-SHA256(macKey, iv ‖ ct)      — verified before decryption
//! ```
//!
//! One wallet's files share a KDF salt (so opening a quorum costs one key
//! derivation) but never an IV. The header — address, `t`, `n`, party index
//! — is cleartext, so a login screen can name a file without a passphrase.

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use base64::Engine;
use hmac::{Hmac, Mac};
use pocketskynet_core::WalletAddress;
use sha2::Sha256;

use crate::TssError;

/// Current share-file format version; bump when the layout changes.
pub const FORMAT_VERSION: u32 = 1;

/// The `type` tag every share file carries, mirroring the wallet-backup
/// convention (`pocketskynet-wallet-backup`) so a file manager full of JSON
/// stays legible.
pub const FILE_TYPE: &str = "pocketskynet-tss-share";

/// PBKDF2 work factor. The passphrase is the only thing between a found
/// share file and its key material; OWASP's 2023 floor for
/// PBKDF2-HMAC-SHA256 is 600k and that is what ships.
pub const KDF_ITERATIONS: u32 = 600_000;

/// One party's sealed share — the unit the user downloads, stores, and
/// later presents `t` of.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareFile {
    /// Always [`FILE_TYPE`]; checked on open.
    #[serde(rename = "type")]
    pub file_type: String,
    pub version: u32,
    /// Lowercase `0x…` — which wallet this share belongs to.
    pub address: String,
    /// `t`: how many shares a signing ceremony needs.
    pub threshold: u16,
    /// `n`: how many shares exist.
    pub parties: u16,
    /// This share's index at keygen, `0..n`.
    pub party_index: u16,
    /// Unix seconds at creation.
    pub created_at: u64,
    /// PBKDF2 salt, 16 bytes hex — identical across one wallet's files.
    pub kdf_salt: String,
    pub kdf_iterations: u32,
    /// AES-CBC IV, 16 bytes hex — unique per file.
    pub iv: String,
    /// The sealed [`ShareSecrets`] JSON, base64.
    pub ciphertext: String,
    /// HMAC-SHA256 over `iv ‖ ciphertext`, 32 bytes hex.
    pub mac: String,
}

/// What lives under one file's seal.
///
/// The share itself is kept as raw JSON rather than a typed
/// `cggmp21::KeyShare`: sealing and unsealing are transport, and only the
/// signing layer needs (and validates) the real type.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareSecrets {
    /// The cggmp21 key share, verbatim.
    pub share: serde_json::Value,
    /// E2EE private key, `0x` + 64 hex (§15.2 — stored, never derived).
    pub enc_priv_hex: String,
    /// The wallet's ceremony signature over the key-binding message.
    pub binding_sig: String,
}

/// Everything a quorum of opened files yields.
pub struct OpenedWallet {
    pub address: WalletAddress,
    pub threshold: u16,
    pub parties: u16,
    /// Exactly `threshold` `(party_index, share_json)` pairs, ascending.
    pub signers: Vec<(u16, serde_json::Value)>,
    pub enc_priv_hex: String,
    pub binding_sig: String,
}

/// Seal one wallet's `n` shares into `n` files under one passphrase.
///
/// `shares[i]` must be party `i`'s share. All files reuse one KDF salt (one
/// derivation to open a quorum) and each gets its own IV.
pub fn seal_shares(
    address: &WalletAddress,
    threshold: u16,
    parties: u16,
    shares: &[serde_json::Value],
    enc_priv_hex: &str,
    binding_sig: &str,
    passphrase: &str,
) -> Result<Vec<ShareFile>, TssError> {
    if shares.len() != usize::from(parties) {
        return Err(TssError::Store(format!(
            "expected {parties} shares to seal, got {}",
            shares.len()
        )));
    }
    let salt = pocketskynet_core::random::bytes::<16>().map_err(|_| TssError::Entropy)?;
    let (enc_key, mac_key) = derive_keys(passphrase, &salt, KDF_ITERATIONS);
    let created_at = now_secs();

    let mut files = Vec::with_capacity(shares.len());
    for (i, share) in shares.iter().enumerate() {
        let secrets = ShareSecrets {
            share: share.clone(),
            enc_priv_hex: enc_priv_hex.to_owned(),
            binding_sig: binding_sig.to_owned(),
        };
        let plaintext = serde_json::to_vec(&secrets)
            .map_err(|e| TssError::Store(format!("encoding share {i}: {e}")))?;
        let iv = pocketskynet_core::random::bytes::<16>().map_err(|_| TssError::Entropy)?;
        let ct = cbc::Encryptor::<aes::Aes256>::new(&enc_key.into(), &iv.into())
            .encrypt_padded_vec_mut::<Pkcs7>(&plaintext);
        let mac = seal_mac(&mac_key, &iv, &ct);
        files.push(ShareFile {
            file_type: FILE_TYPE.to_owned(),
            version: FORMAT_VERSION,
            address: address.as_str().to_owned(),
            threshold,
            parties,
            party_index: i as u16,
            created_at,
            kdf_salt: hex::encode(salt),
            kdf_iterations: KDF_ITERATIONS,
            iv: hex::encode(iv),
            ciphertext: base64::engine::general_purpose::STANDARD.encode(&ct),
            mac: hex::encode(mac),
        });
    }
    Ok(files)
}

/// Open a quorum of share files with the wallet passphrase.
///
/// Accepts any number of files **at or above** the threshold — presenting a
/// spare costs nothing, and "bring what you have" is friendlier than "bring
/// exactly t". Duplicated party indexes collapse to one; the lowest `t`
/// distinct parties sign. Every file must belong to the same wallet.
///
/// The MAC is verified (constant-time) before decryption, and bad MAC / bad
/// padding / bad JSON all collapse into [`TssError::BadPassphrase`] —
/// distinguishing them helps only an attacker with a corruption oracle.
pub fn open_shares(files: &[ShareFile], passphrase: &str) -> Result<OpenedWallet, TssError> {
    let first = files
        .first()
        .ok_or_else(|| TssError::InvalidSigners("no share files provided".into()))?;
    if first.file_type != FILE_TYPE || first.version != FORMAT_VERSION {
        return Err(TssError::Store(format!(
            "not a version-{FORMAT_VERSION} {FILE_TYPE} file"
        )));
    }
    let address = WalletAddress::new(&first.address)
        .map_err(|e| TssError::Store(format!("bad address in share file: {e}")))?;
    crate::validate_params(first.threshold, first.parties)?;

    // One wallet, one seal family: any header disagreement is two different
    // wallets' files mixed together, named before any passphrase work.
    for f in files {
        if f.file_type != FILE_TYPE || f.version != FORMAT_VERSION {
            return Err(TssError::Store(
                "mixed or unsupported share-file versions".into(),
            ));
        }
        if f.address != first.address
            || f.threshold != first.threshold
            || f.parties != first.parties
            || f.kdf_salt != first.kdf_salt
            || f.kdf_iterations != first.kdf_iterations
        {
            return Err(TssError::InvalidSigners(
                "these share files belong to different wallets".into(),
            ));
        }
        if f.party_index >= f.parties {
            return Err(TssError::Store(
                "share file names an impossible party".into(),
            ));
        }
    }

    // Distinct parties, lowest first; a duplicate file is harmless.
    let mut chosen: Vec<&ShareFile> = Vec::new();
    let mut sorted: Vec<&ShareFile> = files.iter().collect();
    sorted.sort_by_key(|f| f.party_index);
    for f in sorted {
        if chosen.last().map(|c| c.party_index) != Some(f.party_index) {
            chosen.push(f);
        }
    }
    if chosen.len() < usize::from(first.threshold) {
        return Err(TssError::InvalidSigners(format!(
            "this wallet needs {} distinct shares to sign, got {}",
            first.threshold,
            chosen.len()
        )));
    }
    chosen.truncate(usize::from(first.threshold));

    let salt = hex::decode(&first.kdf_salt).map_err(|_| TssError::BadPassphrase)?;
    let (enc_key, mac_key) = derive_keys(passphrase, &salt, first.kdf_iterations);

    let mut signers = Vec::with_capacity(chosen.len());
    let mut identity: Option<(String, String)> = None;
    for f in chosen {
        let iv_bytes = hex::decode(&f.iv).map_err(|_| TssError::BadPassphrase)?;
        let iv: [u8; 16] = iv_bytes
            .as_slice()
            .try_into()
            .map_err(|_| TssError::BadPassphrase)?;
        let ct = base64::engine::general_purpose::STANDARD
            .decode(&f.ciphertext)
            .map_err(|_| TssError::BadPassphrase)?;
        let mac = hex::decode(&f.mac).map_err(|_| TssError::BadPassphrase)?;

        let mut verifier = <Hmac<Sha256> as Mac>::new_from_slice(&mac_key).expect("any key length");
        verifier.update(&iv);
        verifier.update(&ct);
        verifier
            .verify_slice(&mac)
            .map_err(|_| TssError::BadPassphrase)?;

        let plaintext = cbc::Decryptor::<aes::Aes256>::new(&enc_key.into(), &iv.into())
            .decrypt_padded_vec_mut::<Pkcs7>(&ct)
            .map_err(|_| TssError::BadPassphrase)?;
        let secrets: ShareSecrets =
            serde_json::from_slice(&plaintext).map_err(|_| TssError::BadPassphrase)?;
        identity.get_or_insert((secrets.enc_priv_hex, secrets.binding_sig));
        signers.push((f.party_index, secrets.share));
    }
    let (enc_priv_hex, binding_sig) = identity.expect("threshold ≥ 2 files opened");

    Ok(OpenedWallet {
        address,
        threshold: first.threshold,
        parties: first.parties,
        signers,
        enc_priv_hex,
        binding_sig,
    })
}

fn derive_keys(passphrase: &str, salt: &[u8], iterations: u32) -> ([u8; 32], [u8; 32]) {
    let mut okm = [0u8; 64];
    pbkdf2::pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), salt, iterations, &mut okm);
    let mut enc = [0u8; 32];
    let mut mac = [0u8; 32];
    enc.copy_from_slice(&okm[..32]);
    mac.copy_from_slice(&okm[32..]);
    (enc, mac)
}

fn seal_mac(mac_key: &[u8; 32], iv: &[u8; 16], ct: &[u8]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(mac_key).expect("any key length");
    mac.update(iv);
    mac.update(ct);
    mac.finalize().into_bytes().into()
}

/// Unix seconds now — the `created_at` stamp.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDR: &str = "0x00112233445566778899aabbccddeeff00112233";

    fn address() -> WalletAddress {
        WalletAddress::new(ADDR).unwrap()
    }

    /// Dummy share payloads: the seal is over JSON and does not care — a
    /// real `cggmp21` share only exists after a multi-minute DKG, and that
    /// round-trip is the integration test's job.
    fn dummy_shares(n: usize) -> Vec<serde_json::Value> {
        (0..n)
            .map(|i| serde_json::json!({ "core": { "i": i }, "aux": {} }))
            .collect()
    }

    fn sealed(t: u16, n: u16, passphrase: &str) -> Vec<ShareFile> {
        seal_shares(
            &address(),
            t,
            n,
            &dummy_shares(usize::from(n)),
            &format!("0x{}", "ab".repeat(32)),
            &format!("0x{}", "cd".repeat(65)),
            passphrase,
        )
        .unwrap()
    }

    #[test]
    fn any_quorum_of_files_opens_the_wallet() {
        let files = sealed(2, 3, "correct horse");

        // The full set works, a strict subset works, and — the lost-share
        // case — a *different* strict subset works too.
        for subset in [vec![0usize, 1, 2], vec![0, 1], vec![1, 2], vec![0, 2]] {
            let quorum: Vec<ShareFile> = subset.iter().map(|&i| files[i].clone()).collect();
            let opened = open_shares(&quorum, "correct horse").unwrap();
            assert_eq!(opened.address, address());
            assert_eq!(opened.signers.len(), 2, "exactly t sign");
            // Chosen indexes are the two lowest distinct parties presented.
            let idx: Vec<u16> = opened.signers.iter().map(|(i, _)| *i).collect();
            assert_eq!(
                idx,
                subset[..2].iter().map(|&i| i as u16).collect::<Vec<_>>()
            );
            assert_eq!(opened.enc_priv_hex, format!("0x{}", "ab".repeat(32)));
            assert_eq!(opened.binding_sig, format!("0x{}", "cd".repeat(65)));
        }
    }

    #[test]
    fn below_the_threshold_nothing_opens() {
        let files = sealed(2, 3, "pass pass pass");
        // One file — and one file presented twice, which must not count as
        // two shares.
        for quorum in [
            vec![files[1].clone()],
            vec![files[1].clone(), files[1].clone()],
        ] {
            assert!(matches!(
                open_shares(&quorum, "pass pass pass"),
                Err(TssError::InvalidSigners(_))
            ));
        }
    }

    #[test]
    fn the_wrong_passphrase_is_one_indistinguishable_error() {
        let files = sealed(2, 2, "right");
        assert!(matches!(
            open_shares(&files, "wrong"),
            Err(TssError::BadPassphrase)
        ));
    }

    #[test]
    fn a_tampered_ciphertext_fails_the_mac_not_the_padding() {
        let mut files = sealed(2, 2, "pass pass");
        let mut ct = base64::engine::general_purpose::STANDARD
            .decode(&files[0].ciphertext)
            .unwrap();
        ct[0] ^= 0x01;
        files[0].ciphertext = base64::engine::general_purpose::STANDARD.encode(&ct);
        assert!(matches!(
            open_shares(&files, "pass pass"),
            Err(TssError::BadPassphrase)
        ));
    }

    #[test]
    fn files_from_two_wallets_never_mix() {
        let a = sealed(2, 2, "same passphrase");
        let b = sealed(2, 2, "same passphrase");
        // Same shape, same passphrase — but different wallets (fresh salt),
        // and the header check names it before any KDF work.
        let mixed = vec![a[0].clone(), b[1].clone()];
        assert!(matches!(
            open_shares(&mixed, "same passphrase"),
            Err(TssError::InvalidSigners(_))
        ));
    }

    #[test]
    fn the_cleartext_header_is_exactly_the_public_facts() {
        let files = sealed(2, 3, "a passphrase");
        let json = serde_json::to_value(&files[0]).unwrap();
        let mut keys: Vec<&str> = json.as_object().unwrap().keys().map(|s| &**s).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "address",
                "ciphertext",
                "createdAt",
                "iv",
                "kdfIterations",
                "kdfSalt",
                "mac",
                "parties",
                "partyIndex",
                "threshold",
                "type",
                "version",
            ]
        );
        assert_eq!(json["type"], FILE_TYPE);
        // And the round trip is lossless.
        let back: ShareFile = serde_json::from_value(json).unwrap();
        assert_eq!(back, files[0]);
    }
}
