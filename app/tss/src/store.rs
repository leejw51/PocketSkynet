//! Passphrase-sealed wallet persistence (CRYPTO.md §15.4).
//!
//! One wallet = one file `<dir>/<address>.wallet.json`, mode 0600. The
//! header (address, t, n, KDF parameters) is cleartext so the login screen
//! can list wallets without a passphrase; the shares, the E2EE private key
//! and the binding signature live under an encrypt-then-MAC seal:
//!
//! ```text
//! okm    = PBKDF2-HMAC-SHA256(passphrase, salt, 600_000) → 64 bytes
//! encKey = okm[0..32]   macKey = okm[32..64]
//! ct     = AES-256-CBC(encKey, iv) over the secrets JSON (PKCS-7)
//! mac    = HMAC-SHA256(macKey, iv ‖ ct)      — verified before decryption
//! ```

use std::io::Write;
use std::path::{Path, PathBuf};

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use base64::Engine;
use hmac::{Hmac, Mac};
use pocketskynet_core::WalletAddress;
use sha2::Sha256;

use crate::{Share, TssError};

/// Current on-disk format version; bump when the layout changes.
pub const FORMAT_VERSION: u32 = 1;

/// PBKDF2 work factor. The passphrase gates every ceremony, so this is the
/// entire cost of a stolen-file guess; OWASP's 2023 floor for
/// PBKDF2-HMAC-SHA256 is 600k and that is what ships.
pub const KDF_ITERATIONS: u32 = 600_000;

const SUFFIX: &str = ".wallet.json";

/// The cleartext header — everything the UI may know before a passphrase.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletInfo {
    /// Lowercase `0x…` — the wallet's identity.
    pub address: String,
    /// `t`: how many shares a signing ceremony needs.
    pub threshold: u16,
    /// `n`: how many shares exist.
    pub parties: u16,
    /// Unix seconds at creation.
    pub created_at: u64,
}

/// The full on-disk file: header + sealed blob.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletFile {
    pub version: u32,
    #[serde(flatten)]
    pub info: WalletInfo,
    /// PBKDF2 salt, 16 bytes hex.
    pub kdf_salt: String,
    pub kdf_iterations: u32,
    /// AES-CBC IV, 16 bytes hex.
    pub iv: String,
    /// The sealed secrets JSON, base64.
    pub ciphertext: String,
    /// HMAC-SHA256 over `iv ‖ ciphertext`, 32 bytes hex.
    pub mac: String,
}

/// One party's share, tagged with its keygen index so the file format
/// already supports distributing shares across holders later.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct StoredShare {
    pub party_index: u16,
    pub share: Share,
}

/// What lives under the seal (§15.2: the E2EE keypair is independent of any
/// signature and shares the shares' custody).
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletSecrets {
    pub shares: Vec<StoredShare>,
    /// E2EE private key, `0x` + 64 hex.
    pub enc_priv_hex: String,
    /// The wallet's ceremony signature over the key-binding message.
    pub binding_sig: String,
}

fn wallet_path(dir: &Path, address: &WalletAddress) -> PathBuf {
    // `WalletAddress` is validated `0x` + 40 lowercase hex — safe as a
    // filename by construction.
    dir.join(format!("{}{SUFFIX}", address.as_str()))
}

pub fn wallet_exists(dir: &Path, address: &WalletAddress) -> bool {
    wallet_path(dir, address).exists()
}

/// Seal and write a wallet. Refuses to overwrite unless `force` — the old
/// shares would be gone for good.
pub fn save_wallet(
    dir: &Path,
    info: &WalletInfo,
    secrets: &WalletSecrets,
    passphrase: &str,
    force: bool,
) -> Result<PathBuf, TssError> {
    let address = WalletAddress::new(&info.address)
        .map_err(|e| TssError::Store(format!("bad address in wallet info: {e}")))?;
    let path = wallet_path(dir, &address);
    if path.exists() && !force {
        return Err(TssError::WalletExists);
    }
    std::fs::create_dir_all(dir).map_err(|e| TssError::Store(format!("creating {dir:?}: {e}")))?;
    restrict_dir(dir);

    let plaintext = serde_json::to_vec(secrets)
        .map_err(|e| TssError::Store(format!("encoding secrets: {e}")))?;

    let salt = pocketskynet_core::random::bytes::<16>().map_err(|_| TssError::Entropy)?;
    let iv = pocketskynet_core::random::bytes::<16>().map_err(|_| TssError::Entropy)?;
    let (enc_key, mac_key) = derive_keys(passphrase, &salt, KDF_ITERATIONS);

    let ct = cbc::Encryptor::<aes::Aes256>::new(&enc_key.into(), &iv.into())
        .encrypt_padded_vec_mut::<Pkcs7>(&plaintext);
    let mac = seal_mac(&mac_key, &iv, &ct);

    let file = WalletFile {
        version: FORMAT_VERSION,
        info: info.clone(),
        kdf_salt: hex::encode(salt),
        kdf_iterations: KDF_ITERATIONS,
        iv: hex::encode(iv),
        ciphertext: base64::engine::general_purpose::STANDARD.encode(&ct),
        mac: hex::encode(mac),
    };
    let json = serde_json::to_vec_pretty(&file)
        .map_err(|e| TssError::Store(format!("encoding wallet file: {e}")))?;
    write_owner_only(&path, &json)
        .map_err(|e| TssError::Store(format!("writing {path:?}: {e}")))?;
    Ok(path)
}

/// Every wallet in `dir`, headers only, newest first. Unreadable files are
/// skipped rather than failing the listing — one corrupt file must not make
/// the login screen claim there are no wallets at all.
pub fn list_wallets(dir: &Path) -> Vec<WalletInfo> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut wallets: Vec<WalletInfo> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(SUFFIX))
        .filter_map(|e| {
            let bytes = std::fs::read(e.path()).ok()?;
            let file: WalletFile = serde_json::from_slice(&bytes).ok()?;
            (file.version == FORMAT_VERSION).then_some(file.info)
        })
        .collect();
    wallets.sort_by_key(|w| std::cmp::Reverse(w.created_at));
    wallets
}

fn read_file(dir: &Path, address: &WalletAddress) -> Result<WalletFile, TssError> {
    let path = wallet_path(dir, address);
    if !path.exists() {
        return Err(TssError::WalletNotFound);
    }
    let bytes =
        std::fs::read(&path).map_err(|e| TssError::Store(format!("reading {path:?}: {e}")))?;
    let file: WalletFile = serde_json::from_slice(&bytes)
        .map_err(|e| TssError::Store(format!("parsing {path:?}: {e}")))?;
    if file.version != FORMAT_VERSION {
        return Err(TssError::Store(format!(
            "unsupported wallet format version {} in {path:?}",
            file.version
        )));
    }
    Ok(file)
}

/// Unseal a wallet with its passphrase. The MAC is verified (constant-time)
/// before any decryption is attempted, and every failure mode from there —
/// bad MAC, bad padding, bad JSON — collapses into [`TssError::BadPassphrase`]
/// on purpose: distinguishing them helps only an attacker with a corrupted
/// file oracle.
pub fn open_wallet(
    dir: &Path,
    address: &WalletAddress,
    passphrase: &str,
) -> Result<(WalletInfo, WalletSecrets), TssError> {
    let file = read_file(dir, address)?;

    let salt = hex::decode(&file.kdf_salt).map_err(|_| TssError::BadPassphrase)?;
    let iv_bytes = hex::decode(&file.iv).map_err(|_| TssError::BadPassphrase)?;
    let iv: [u8; 16] = iv_bytes
        .as_slice()
        .try_into()
        .map_err(|_| TssError::BadPassphrase)?;
    let ct = base64::engine::general_purpose::STANDARD
        .decode(&file.ciphertext)
        .map_err(|_| TssError::BadPassphrase)?;
    let mac = hex::decode(&file.mac).map_err(|_| TssError::BadPassphrase)?;

    let (enc_key, mac_key) = derive_keys(passphrase, &salt, file.kdf_iterations);

    let mut verifier = <Hmac<Sha256> as Mac>::new_from_slice(&mac_key).expect("any key length");
    verifier.update(&iv);
    verifier.update(&ct);
    verifier
        .verify_slice(&mac)
        .map_err(|_| TssError::BadPassphrase)?;

    let plaintext = cbc::Decryptor::<aes::Aes256>::new(&enc_key.into(), &iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(&ct)
        .map_err(|_| TssError::BadPassphrase)?;
    let secrets: WalletSecrets =
        serde_json::from_slice(&plaintext).map_err(|_| TssError::BadPassphrase)?;
    Ok((file.info, secrets))
}

/// Delete a wallet — but only for a caller who can open it. Deletion without
/// the passphrase would let anyone who can reach the API destroy a wallet
/// they cannot use.
pub fn delete_wallet(
    dir: &Path,
    address: &WalletAddress,
    passphrase: &str,
) -> Result<(), TssError> {
    open_wallet(dir, address, passphrase)?;
    let path = wallet_path(dir, address);
    std::fs::remove_file(&path).map_err(|e| TssError::Store(format!("removing {path:?}: {e}")))
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

/// Writes `bytes` with the file created 0600 from the start — a
/// chmod-after-write would leave a window where the seal is world-readable.
fn write_owner_only(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    // `mode` only applies on creation; tighten a pre-existing file too.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(bytes)?;
    f.sync_all()
}

/// Owner-only on the directory as well, so a later file created by any path
/// is not exposed by a permissive parent. Best-effort — the files themselves
/// are 0600 regardless.
fn restrict_dir(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
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

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ps-tss-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn info(addr: &str) -> WalletInfo {
        WalletInfo {
            address: addr.into(),
            threshold: 2,
            parties: 3,
            created_at: 1_755_000_000,
        }
    }

    /// Secrets with no shares: the seal is over JSON and does not care, and
    /// a real `Share` only exists after a multi-minute DKG — that round-trip
    /// is the integration test's job.
    fn secrets() -> WalletSecrets {
        WalletSecrets {
            shares: Vec::new(),
            enc_priv_hex: format!("0x{}", "ab".repeat(32)),
            binding_sig: format!("0x{}", "cd".repeat(65)),
        }
    }

    const ADDR: &str = "0x00112233445566778899aabbccddeeff00112233";

    #[test]
    fn a_wallet_round_trips_through_its_passphrase() {
        let dir = tempdir("roundtrip");
        let address = WalletAddress::new(ADDR).unwrap();
        save_wallet(&dir, &info(ADDR), &secrets(), "correct horse", false).unwrap();

        let (got_info, got_secrets) = open_wallet(&dir, &address, "correct horse").unwrap();
        assert_eq!(got_info, info(ADDR));
        assert_eq!(got_secrets.enc_priv_hex, secrets().enc_priv_hex);
        assert_eq!(got_secrets.binding_sig, secrets().binding_sig);

        // And the header is listable without any passphrase.
        let listed = list_wallets(&dir);
        assert_eq!(listed, vec![info(ADDR)]);
    }

    #[test]
    fn the_wrong_passphrase_is_one_indistinguishable_error() {
        let dir = tempdir("wrongpass");
        let address = WalletAddress::new(ADDR).unwrap();
        save_wallet(&dir, &info(ADDR), &secrets(), "right", false).unwrap();
        assert!(matches!(
            open_wallet(&dir, &address, "wrong"),
            Err(TssError::BadPassphrase)
        ));
    }

    #[test]
    fn a_tampered_ciphertext_fails_the_mac_not_the_padding() {
        // Encrypt-then-MAC: flipping a ciphertext bit must land on the same
        // BadPassphrase as a wrong passphrase, never a padding oracle.
        let dir = tempdir("tamper");
        let address = WalletAddress::new(ADDR).unwrap();
        let path = save_wallet(&dir, &info(ADDR), &secrets(), "pass", false).unwrap();

        let mut file: WalletFile = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut ct = base64::engine::general_purpose::STANDARD
            .decode(&file.ciphertext)
            .unwrap();
        ct[0] ^= 0x01;
        file.ciphertext = base64::engine::general_purpose::STANDARD.encode(&ct);
        std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();

        assert!(matches!(
            open_wallet(&dir, &address, "pass"),
            Err(TssError::BadPassphrase)
        ));
    }

    #[test]
    fn overwriting_requires_force_and_deleting_requires_the_passphrase() {
        let dir = tempdir("guards");
        let address = WalletAddress::new(ADDR).unwrap();
        save_wallet(&dir, &info(ADDR), &secrets(), "pass", false).unwrap();

        assert!(matches!(
            save_wallet(&dir, &info(ADDR), &secrets(), "pass", false),
            Err(TssError::WalletExists)
        ));
        save_wallet(&dir, &info(ADDR), &secrets(), "pass2", true).unwrap();

        assert!(matches!(
            delete_wallet(&dir, &address, "pass"),
            Err(TssError::BadPassphrase)
        ));
        delete_wallet(&dir, &address, "pass2").unwrap();
        assert!(matches!(
            open_wallet(&dir, &address, "pass2"),
            Err(TssError::WalletNotFound)
        ));
        assert!(list_wallets(&dir).is_empty());
    }

    #[test]
    fn a_missing_wallet_is_not_found_not_a_passphrase_failure() {
        let dir = tempdir("missing");
        let address = WalletAddress::new(ADDR).unwrap();
        // The wallet's existence is public (it is listed); only the seal is
        // secret. So this error is allowed to be specific.
        assert!(matches!(
            open_wallet(&dir, &address, "anything"),
            Err(TssError::WalletNotFound)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn the_wallet_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir("perms");
        let path = save_wallet(&dir, &info(ADDR), &secrets(), "pass", false).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "share file must be 0600");
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(dir_mode & 0o777, 0o700, "share dir must be 0700");
    }
}
