//! The transport abstraction: one enum, two wire protocols.
//!
//! The API layer ([`crate::Client`]) only ever sees [`Transport::request`] —
//! method, path, optional bearer token, optional JSON body in; status and
//! bytes out. Which listener carries it (HTTP/1.1 over TCP, or HTTP/3 over
//! QUIC) is decided once, at construction.
//!
//! An enum rather than a trait object: there are exactly two transports, both
//! known at compile time, and matching on an enum keeps the futures `Send`
//! without an `async_trait` indirection.

mod h1;
mod h3t;

pub use h1::H1Transport;
pub use h3t::H3Transport;

use crate::error::ClientError;

/// Options shared by both transports.
#[derive(Debug, Clone, Default)]
pub struct TransportOptions {
    /// Skip server-certificate verification. For the server's self-signed
    /// development certificates only; never use it against a real deployment.
    pub insecure: bool,
}

/// A raw response: status plus body bytes. The API layer does the JSON.
#[derive(Debug)]
pub struct RawResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// The one transport the client was built with.
pub enum Transport {
    H1(H1Transport),
    H3(H3Transport),
}

impl Transport {
    /// HTTP/1.1(+TLS) to `base_url` (e.g. `http://127.0.0.1:9099`).
    pub fn http1(base_url: &str, options: &TransportOptions) -> Result<Self, ClientError> {
        Ok(Transport::H1(H1Transport::new(base_url, options)?))
    }

    /// HTTP/3 over QUIC to the host and port of `base_url`. QUIC mandates
    /// TLS 1.3, so the scheme is treated as `https` regardless; the UDP port
    /// used is the URL's port (the server's `make start` serves QUIC on the
    /// same number as HTTPS), unless `port_override` says otherwise.
    pub async fn http3(
        base_url: &str,
        options: &TransportOptions,
        port_override: Option<u16>,
    ) -> Result<Self, ClientError> {
        Ok(Transport::H3(
            H3Transport::connect(base_url, options, port_override).await?,
        ))
    }

    /// Send one request and read the whole response.
    pub async fn request(
        &self,
        method: http::Method,
        path: &str,
        token: Option<&str>,
        body: Option<&serde_json::Value>,
    ) -> Result<RawResponse, ClientError> {
        match self {
            Transport::H1(t) => t.request(method, path, token, body).await,
            Transport::H3(t) => t.request(method, path, token, body).await,
        }
    }

    /// A human-readable name for logs and `--verbose` output.
    pub fn name(&self) -> &'static str {
        match self {
            Transport::H1(_) => "http/1.1",
            Transport::H3(_) => "h3",
        }
    }
}

/// The host and UDP port an HTTP/3 connection to `base_url` should dial.
///
/// QUIC mandates TLS, so the scheme only matters for the *default* port —
/// `http://host` still dials 80/udp when nothing better is known. An explicit
/// port in the URL wins over the scheme default, and `port_override` wins
/// over everything: the server's `make start` serves QUIC on the same number
/// as HTTPS, but the bare binary defaults to TCP port + 2, and the override
/// is how a client reaches that layout.
pub fn h3_target(base_url: &str, port_override: Option<u16>) -> Result<(String, u16), ClientError> {
    let parsed = url::Url::parse(base_url).map_err(|e| ClientError::InvalidUrl(e.to_string()))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| ClientError::InvalidUrl(format!("no host in {base_url:?}")))?
        .to_owned();
    let port = port_override.or_else(|| parsed.port()).unwrap_or({
        if parsed.scheme() == "http" {
            80
        } else {
            443
        }
    });
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_urls_own_port_is_the_default_quic_port() {
        // `make start` serves QUIC on the same number as HTTPS.
        assert_eq!(
            h3_target("https://127.0.0.1:9099", None).unwrap(),
            ("127.0.0.1".into(), 9099)
        );
    }

    #[test]
    fn an_override_beats_the_urls_port() {
        // The bare server binary defaults QUIC to TCP port + 2.
        assert_eq!(
            h3_target("https://127.0.0.1:9099", Some(9101)).unwrap(),
            ("127.0.0.1".into(), 9101)
        );
    }

    #[test]
    fn scheme_defaults_apply_only_without_an_explicit_port() {
        assert_eq!(
            h3_target("https://example.com", None).unwrap(),
            ("example.com".into(), 443)
        );
        assert_eq!(
            h3_target("http://example.com", None).unwrap(),
            ("example.com".into(), 80)
        );
    }

    #[test]
    fn a_hostname_is_kept_verbatim_for_sni() {
        assert_eq!(
            h3_target("https://localhost:19099", None).unwrap(),
            ("localhost".into(), 19099)
        );
    }

    #[test]
    fn unparseable_or_hostless_urls_are_refused() {
        assert!(matches!(
            h3_target("not a url", None),
            Err(ClientError::InvalidUrl(_))
        ));
        assert!(matches!(
            h3_target("data:text/plain,hello", None),
            Err(ClientError::InvalidUrl(_))
        ));
    }

    #[test]
    fn transport_construction_reports_its_name_and_rejects_bad_urls() {
        let options = TransportOptions::default();
        let transport = Transport::http1("http://127.0.0.1:1", &options).expect("h1 builds");
        assert_eq!(transport.name(), "http/1.1");

        assert!(matches!(
            Transport::http1("::not-a-url::", &options),
            Err(ClientError::InvalidUrl(_))
        ));
        assert!(matches!(
            Transport::http1("data:text/plain,x", &options),
            Err(ClientError::InvalidUrl(_))
        ));
    }
}
