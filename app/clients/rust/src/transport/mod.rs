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
