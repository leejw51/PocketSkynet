//! Self-signed HTTPS, so a phone or a tablet can join over the network.
//!
//! The problem this solves is not eavesdropping on a home LAN — it is that a
//! browser treats a plain-HTTP origin on a LAN address as insecure and takes
//! things away: no `crypto.subtle`, no clipboard write, no notifications, no
//! service worker, and a permanent "Not Secure" badge next to a page that asks
//! people to sign with a wallet key. HTTPS gives all of it back.
//!
//! ## Why a CA and a leaf, rather than one self-signed certificate
//!
//! The certificate has to name the addresses it is reached on, and those
//! change: a laptop moves between networks, a VPN comes and goes, DHCP hands
//! out a different lease. A single self-signed certificate would therefore have
//! to be regenerated whenever the address set changed — and every regeneration
//! would invalidate the trust the user had already installed on their tablet.
//!
//! So there are two:
//!
//! - **`ca.crt`** — long-lived, addressless, generated once. This is the file
//!   you install on the iPad; nothing routine ever replaces it.
//! - **`server.crt`** — short-lived, names every address the host currently
//!   holds, signed by the CA and regenerated freely.
//!
//! Move to a different network and the leaf is reissued; the tablet keeps
//! trusting it, because what it trusts is the issuer.
//!
//! ## Constraints that are not negotiable
//!
//! Apple platforms refuse a TLS certificate that misses any of these, and the
//! failure is a bare "connection is not private" with nothing naming the cause:
//!
//! - the hostname must be in a **subjectAltName**; the common name is ignored,
//! - **ECC P-256 or better** (RSA must be ≥ 2048),
//! - **`extendedKeyUsage` must include `serverAuth`**,
//! - a leaf must be valid for **≤ 398 days**,
//! - SHA-1 anywhere in the chain is fatal.
//!
//! [`LEAF_VALID_DAYS`] and the parameters below satisfy all of them.

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};
use time::{Duration, OffsetDateTime};

/// Leaf lifetime. Apple platforms reject anything over 398 days outright, so
/// this stays comfortably under it rather than at the edge.
const LEAF_VALID_DAYS: i64 = 397;

/// Reissue a leaf once it is this close to expiring, rather than at the moment
/// it dies — a long-running server should never serve an expired certificate.
const LEAF_RENEW_DAYS: i64 = 30;

/// CA lifetime. Long, deliberately: this is the certificate a user installs by
/// hand on their devices, and asking them to redo it is the one cost worth
/// engineering away.
const CA_VALID_DAYS: i64 = 3650;

/// Where the material for one host lives.
#[derive(Debug, Clone)]
pub struct Materials {
    /// PEM chain: leaf first, then the CA that signed it.
    pub chain: PathBuf,
    /// PKCS#8 private key for the leaf.
    pub key: PathBuf,
    /// The CA certificate on its own — the file installed on other devices.
    pub ca: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("creating {0}: {1}")]
    Dir(PathBuf, #[source] std::io::Error),
    #[error("writing {0}: {1}")]
    Write(PathBuf, #[source] std::io::Error),
    #[error("generating a certificate: {0}")]
    Generate(#[from] rcgen::Error),
    #[error("loading the certificate from {0}: {1}")]
    Load(PathBuf, #[source] std::io::Error),
}

/// Every name the certificate should be valid for: loopback, `localhost`, and
/// each non-loopback IPv4 the host currently holds.
///
/// Sorted and deduplicated so that "the address set changed" is a plain string
/// comparison rather than a set operation — interface enumeration order is not
/// stable across calls, and an unstable order would reissue the leaf on every
/// single startup.
pub fn local_names() -> Vec<String> {
    let mut names = vec!["localhost".to_string(), "127.0.0.1".to_string()];

    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for iface in interfaces {
            match iface.addr.ip() {
                IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local() => {
                    names.push(v4.to_string());
                }
                _ => {}
            }
        }
    }

    names.sort();
    names.dedup();
    names
}

/// Return usable certificate material, generating whatever is missing.
///
/// Idempotent and cheap on the common path: an unchanged address set with a
/// leaf that is not near expiry reuses what is on disk untouched.
pub fn ensure(dir: &Path, names: &[String]) -> Result<Materials, TlsError> {
    std::fs::create_dir_all(dir).map_err(|e| TlsError::Dir(dir.to_path_buf(), e))?;

    let materials = Materials {
        chain: dir.join("server.crt"),
        key: dir.join("server.key"),
        ca: dir.join("ca.crt"),
    };
    let ca_key_path = dir.join("ca.key");
    let names_path = dir.join("server.names");

    // The CA is generated once and then left alone for a decade. Losing it
    // would mean every device that trusts this server has to be visited again.
    let (ca_pem, ca_key_pem) = match (
        std::fs::read_to_string(&materials.ca),
        std::fs::read_to_string(&ca_key_path),
    ) {
        (Ok(cert), Ok(key)) if !cert.is_empty() && !key.is_empty() => (cert, key),
        _ => {
            let (cert, key) = generate_ca()?;
            write_secret(&ca_key_path, &key)?;
            write(&materials.ca, &cert)?;
            tracing::info!(path = ?materials.ca, "generated a local certificate authority");
            (cert, key)
        }
    };

    let wanted = names.join("\n");
    let current = std::fs::read_to_string(&names_path).unwrap_or_default();
    let reason = if !materials.chain.is_file() || !materials.key.is_file() {
        Some("no certificate yet")
    } else if current != wanted {
        Some("the set of addresses this host answers on changed")
    } else if expiring_soon(&materials.chain) {
        Some("the certificate is close to expiring")
    } else {
        None
    };

    if let Some(reason) = reason {
        let ca_key = KeyPair::from_pem(&ca_key_pem)?;
        let ca_params = CertificateParams::from_ca_cert_pem(&ca_pem)?;
        let ca = ca_params.self_signed(&ca_key)?;

        let (leaf_pem, leaf_key_pem) = generate_leaf(names, &ca, &ca_key)?;

        write_secret(&materials.key, &leaf_key_pem)?;
        // Leaf first, issuer second: the order a TLS chain is defined in, and
        // shipping the CA alongside is what lets a client that already trusts
        // it validate without a second fetch.
        write(&materials.chain, &format!("{leaf_pem}{ca_pem}"))?;
        std::fs::write(&names_path, &wanted).map_err(|e| TlsError::Write(names_path.clone(), e))?;

        tracing::info!(reason, ?names, "issued a new TLS certificate");
    }

    Ok(materials)
}

/// Load a chain and key into a server config.
///
/// Also installs the process-wide crypto provider. Doing it here rather than in
/// `main` means the desktop app, the tests and the binary cannot each forget
/// it in their own way; `install_default` is idempotent enough to call twice
/// because a second call simply reports that one is already set.
pub async fn server_config(
    chain: &Path,
    key: &Path,
) -> Result<axum_server::tls_rustls::RustlsConfig, TlsError> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    axum_server::tls_rustls::RustlsConfig::from_pem_file(chain, key)
        .await
        .map_err(|e| TlsError::Load(chain.to_path_buf(), e))
}

/// Serve the CA certificate for download.
///
/// Mounted on **both** listeners, for two different readers:
///
/// * the plain-HTTP redirect port, for a device that cannot get past the warning
///   at all — some in-app browsers (MetaMask's among them) will not offer a
///   bypass, so the file has to be reachable over a connection with nothing to
///   warn about;
/// * the HTTPS port, so the app itself can link to `/ca.crt` on its own origin.
///   Without that the client would need to be told the redirect port, and a
///   relative link is one fewer thing that can be configured wrongly.
///
/// A CA certificate is a public key. There is nothing here to protect.
pub async fn serve_ca(path: Option<PathBuf>) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;

    let Some(path) = path else {
        return (StatusCode::NOT_FOUND, "no generated certificate here").into_response();
    };
    match std::fs::read(&path) {
        Ok(bytes) => (
            // The iOS-specific type: it is what makes Safari offer to install
            // the certificate as a profile rather than showing it as text or
            // dropping it into Files.
            [
                (header::CONTENT_TYPE, "application/x-x509-ca-cert"),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"pocketskynet-ca.crt\"",
                ),
            ],
            bytes,
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(?path, error = %e, "could not read the CA certificate");
            (StatusCode::NOT_FOUND, "no certificate available").into_response()
        }
    }
}

/// The plain-HTTP companion to the TLS port: hand out the CA, redirect
/// everything else.
///
/// Two jobs, both about the first thirty seconds of using this from a tablet:
///
/// 1. A browser given a bare `host:port` tries **HTTP** first. Without this,
///    the most likely thing anybody types fails with a protocol error rather
///    than arriving at the site.
/// 2. Installing the CA is what removes the security warning permanently, and
///    fetching it over the very connection the warning is about is a poor
///    experience — this way the file comes over a connection with nothing to
///    warn about, since a public certificate is not a secret.
pub fn redirect_router(tls_port: u16, ca: Option<PathBuf>) -> axum::Router {
    use axum::extract::State;
    use axum::http::{header, HeaderMap, StatusCode, Uri};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;

    #[derive(Clone)]
    struct Ctx {
        tls_port: u16,
        ca: Option<PathBuf>,
    }

    async fn certificate(State(ctx): State<Ctx>) -> Response {
        serve_ca(ctx.ca).await
    }

    async fn upgrade(State(ctx): State<Ctx>, headers: HeaderMap, uri: Uri) -> Response {
        // The host as the client typed it, minus any port: it may have reached
        // us on a LAN address, a VPN address or `localhost`, and redirecting to
        // anything other than the one they used would either fail to resolve or
        // trip the certificate's name check.
        let host = headers
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .map(|h| h.rsplit_once(':').map_or(h, |(name, _)| name))
            .unwrap_or("localhost")
            .to_string();
        let path = uri
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/")
            .to_string();

        // 307, not 301: a permanent redirect would be cached by the browser,
        // and a cached "always use HTTPS on this host" outlives the flag that
        // turned TLS on.
        (
            StatusCode::TEMPORARY_REDIRECT,
            [(
                header::LOCATION,
                format!("https://{host}:{}{path}", ctx.tls_port),
            )],
        )
            .into_response()
    }

    axum::Router::new()
        .route("/ca.crt", get(certificate))
        // An alias, because half of everyone types the other one.
        .route("/cert.crt", get(certificate))
        .fallback(upgrade)
        .with_state(Ctx { tls_port, ca })
}

/// The addressless issuer. No SANs and no `serverAuth`: it is never presented
/// as a server, only as the thing that vouches for one.
fn generate_ca() -> Result<(String, String), TlsError> {
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    params
        .distinguished_name
        .push(DnType::CommonName, "PocketSkynet local CA");
    params
        .distinguished_name
        .push(DnType::OrganizationName, "PocketSkynet");

    // `Constrained(0)`: this CA may sign end-entity certificates and nothing
    // else. If the key ever leaks it cannot be used to mint further CAs.
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    params.not_before = OffsetDateTime::now_utc() - Duration::hours(1);
    params.not_after = OffsetDateTime::now_utc() + Duration::days(CA_VALID_DAYS);

    let key = KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// The certificate actually presented on the wire, naming every address this
/// host answers on.
fn generate_leaf(
    names: &[String],
    ca: &rcgen::Certificate,
    ca_key: &KeyPair,
) -> Result<(String, String), TlsError> {
    // `CertificateParams::new` reads each entry as an IP SAN when it parses as
    // an address and a DNS SAN otherwise — which is exactly the distinction
    // that matters, since a certificate naming `192.168.1.5` as a *DNS* name is
    // rejected when reached at that address.
    let mut params = CertificateParams::new(names.to_vec())?;
    params
        .distinguished_name
        .push(DnType::CommonName, "PocketSkynet");
    params.is_ca = IsCa::NoCa;
    params.use_authority_key_identifier_extension = true;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    // Without `serverAuth` an Apple client rejects the certificate even when
    // the CA is fully trusted.
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    // An hour of leeway: a device whose clock runs slightly behind would
    // otherwise see a certificate from the future and refuse it.
    params.not_before = OffsetDateTime::now_utc() - Duration::hours(1);
    params.not_after = OffsetDateTime::now_utc() + Duration::days(LEAF_VALID_DAYS);

    let key = KeyPair::generate()?;
    let cert = params.signed_by(&key, ca, ca_key)?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// Whether the certificate on disk is within [`LEAF_RENEW_DAYS`] of expiry.
///
/// Unreadable or unparseable counts as expiring: reissuing costs milliseconds,
/// and the alternative is serving something no client will accept.
fn expiring_soon(path: &Path) -> bool {
    let Ok(pem) = std::fs::read_to_string(path) else {
        return true;
    };
    let Ok(params) = CertificateParams::from_ca_cert_pem(&pem) else {
        return true;
    };
    params.not_after < OffsetDateTime::now_utc() + Duration::days(LEAF_RENEW_DAYS)
}

fn write(path: &Path, contents: &str) -> Result<(), TlsError> {
    std::fs::write(path, contents).map_err(|e| TlsError::Write(path.to_path_buf(), e))
}

/// Write a private key, readable only by its owner.
fn write_secret(path: &Path, contents: &str) -> Result<(), TlsError> {
    write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Best effort, as with the JWT secret: a failed chmod is worth a
        // warning, not a refusal to start.
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!(?path, error = %e, "could not restrict private key permissions");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        let mut buf = [0u8; 8];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut buf);
        let dir = std::env::temp_dir().join(format!("ps-tls-{}", hex::encode(buf)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn generates_then_reuses() {
        let dir = tempdir();
        let names = vec!["localhost".into(), "127.0.0.1".into(), "10.0.0.7".into()];

        let first = ensure(&dir, &names).unwrap();
        let chain = std::fs::read_to_string(&first.chain).unwrap();
        let ca = std::fs::read_to_string(&first.ca).unwrap();

        // The chain is leaf + issuer, so two certificates.
        assert_eq!(chain.matches("BEGIN CERTIFICATE").count(), 2);
        assert!(chain.ends_with(&ca), "the CA must be appended to the leaf");

        ensure(&dir, &names).unwrap();
        assert_eq!(
            std::fs::read_to_string(&first.chain).unwrap(),
            chain,
            "an unchanged address set must not reissue: it would churn the \
             certificate on every restart"
        );
    }

    #[test]
    fn a_new_address_reissues_the_leaf_but_keeps_the_ca() {
        let dir = tempdir();
        let materials = ensure(&dir, &["127.0.0.1".to_string()]).unwrap();
        let ca_before = std::fs::read_to_string(&materials.ca).unwrap();
        let leaf_before = std::fs::read_to_string(&materials.chain).unwrap();

        // Joining a network, or a VPN coming up.
        ensure(&dir, &["127.0.0.1".to_string(), "100.64.0.2".to_string()]).unwrap();

        assert_ne!(
            std::fs::read_to_string(&materials.chain).unwrap(),
            leaf_before,
            "the leaf must name the new address"
        );
        assert_eq!(
            std::fs::read_to_string(&materials.ca).unwrap(),
            ca_before,
            "reissuing the leaf must not invalidate trust already installed on \
             somebody's phone — that is the entire reason the CA is separate"
        );
    }

    #[test]
    fn the_leaf_satisfies_apple_platform_rules() {
        let dir = tempdir();
        let materials = ensure(&dir, &["127.0.0.1".to_string(), "10.1.2.3".to_string()]).unwrap();

        let chain = std::fs::read_to_string(&materials.chain).unwrap();
        let leaf = CertificateParams::from_ca_cert_pem(&chain).unwrap();

        assert!(
            leaf.extended_key_usages
                .contains(&ExtendedKeyUsagePurpose::ServerAuth),
            "iOS rejects a certificate without serverAuth even from a trusted CA"
        );
        assert_eq!(
            leaf.subject_alt_names.len(),
            2,
            "the address must be a SAN; the common name is ignored"
        );
        let span = leaf.not_after - leaf.not_before;
        assert!(
            span.whole_days() <= 398,
            "iOS caps leaf validity at 398 days, got {}",
            span.whole_days()
        );
    }

    #[test]
    fn local_names_always_cover_loopback() {
        let names = local_names();
        assert!(names.contains(&"127.0.0.1".to_string()));
        assert!(names.contains(&"localhost".to_string()));

        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(
            names, sorted,
            "an unstable order would look like a changed address set and \
             reissue the certificate on every startup"
        );
    }
}
