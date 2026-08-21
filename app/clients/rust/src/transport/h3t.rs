//! HTTP/3 over QUIC — `quinn` 0.11 + `h3` + `h3-quinn`, the same stack the
//! server's listener uses (`app/server/src/http3.rs`), so both ends agree on
//! every framing detail by construction.
//!
//! One QUIC connection is opened at construction and kept for the life of
//! the transport, the way a real client behaves: requests are independent
//! streams on it, so a slow response never queues behind another request.

use std::sync::Arc;

use bytes::{Buf, Bytes};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

use crate::error::ClientError;
use crate::transport::{RawResponse, TransportOptions};

/// The registered ALPN token for HTTP/3. Draft tokens (`h3-29`, …) negotiate
/// with nothing current; the server rejects anything but `h3` during the
/// handshake.
const ALPN_H3: &[u8] = b"h3";

pub struct H3Transport {
    /// h3 hands out one send handle per connection; requests need `&mut`, so
    /// it lives behind an async mutex. Contention is per-request setup only —
    /// the stream, once opened, proceeds independently.
    send: tokio::sync::Mutex<h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>>,
    /// Nothing progresses unless the driver is polled; dropping it ends the
    /// connection, so the handle is held even though nothing reads it.
    _driver: tokio::task::JoinHandle<()>,
    /// Kept so the endpoint is not dropped (which would kill the connection).
    _endpoint: quinn::Endpoint,
    /// `host:port` for the `:authority` pseudo-header — HTTP/3 has no `Host`
    /// header, the authority travels in the URI.
    authority: String,
}

impl H3Transport {
    pub async fn connect(
        base_url: &str,
        options: &TransportOptions,
        port_override: Option<u16>,
    ) -> Result<Self, ClientError> {
        let (host, port) = crate::transport::h3_target(base_url, port_override)?;

        let _ = rustls::crypto::ring::default_provider().install_default();

        // QUIC mandates TLS 1.3; unlike the TCP listener there is nothing to
        // negotiate down to.
        let builder =
            rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13]);
        let mut tls = if options.insecure {
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(InsecureVerifier::new()))
                .with_no_client_auth()
        } else {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            builder.with_root_certificates(roots).with_no_client_auth()
        };
        // Without this the handshake completes and the server then closes with
        // "no application protocol" — the single most common way an h3 client
        // fails to work at all.
        tls.alpn_protocols = vec![ALPN_H3.to_vec()];

        let quic_tls = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
            .map_err(|e| ClientError::Transport(format!("QUIC TLS config: {e}")))?;

        let addr = tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|e| ClientError::Transport(format!("resolving {host}:{port}: {e}")))?
            .next()
            .ok_or_else(|| {
                ClientError::Transport(format!("{host}:{port} resolved to no addresses"))
            })?;

        let bind = if addr.is_ipv6() {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        };
        let mut endpoint =
            quinn::Endpoint::client(bind.parse().expect("a constant address parses"))
                .map_err(|e| ClientError::Transport(format!("binding a client UDP socket: {e}")))?;
        endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic_tls)));

        let connection = endpoint
            .connect(addr, &host)
            .map_err(|e| ClientError::Transport(format!("starting the QUIC handshake: {e}")))?
            .await
            .map_err(|e| ClientError::Transport(format!("QUIC handshake with {addr}: {e}")))?;

        let (mut driver, send) = h3::client::new(h3_quinn::Connection::new(connection))
            .await
            .map_err(|e| ClientError::Transport(format!("opening the HTTP/3 session: {e}")))?;

        // h3 splits the connection into a driver and a request handle; the
        // driver has to be polled for anything to move.
        let driver = tokio::spawn(async move {
            let _ = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
        });

        Ok(Self {
            send: tokio::sync::Mutex::new(send),
            _driver: driver,
            _endpoint: endpoint,
            authority: format!("{host}:{port}"),
        })
    }

    pub async fn request(
        &self,
        method: http::Method,
        path: &str,
        token: Option<&str>,
        body: Option<&serde_json::Value>,
    ) -> Result<RawResponse, ClientError> {
        let mut builder = http::Request::builder()
            .method(method)
            .uri(format!("https://{}{path}", self.authority));
        if let Some(token) = token {
            builder = builder.header(http::header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let payload = body
            .map(|v| serde_json::to_vec(v).map(Bytes::from))
            .transpose()?;
        if payload.is_some() {
            builder = builder.header(http::header::CONTENT_TYPE, "application/json");
        }
        let request = builder
            .body(())
            .map_err(|e| ClientError::Transport(format!("building the request: {e}")))?;

        let mut stream = {
            let mut send = self.send.lock().await;
            send.send_request(request)
                .await
                .map_err(|e| ClientError::Transport(format!("sending request headers: {e}")))?
        };

        if let Some(payload) = payload {
            stream
                .send_data(payload)
                .await
                .map_err(|e| ClientError::Transport(format!("sending the body: {e}")))?;
        }
        stream
            .finish()
            .await
            .map_err(|e| ClientError::Transport(format!("finishing the request stream: {e}")))?;

        let response = stream
            .recv_response()
            .await
            .map_err(|e| ClientError::Transport(format!("receiving response headers: {e}")))?;
        let status = response.status().as_u16();

        let mut body = Vec::new();
        while let Some(mut chunk) = stream
            .recv_data()
            .await
            .map_err(|e| ClientError::Transport(format!("receiving the response body: {e}")))?
        {
            while chunk.has_remaining() {
                let piece = chunk.chunk().to_vec();
                chunk.advance(piece.len());
                body.extend_from_slice(&piece);
            }
        }

        Ok(RawResponse { status, body })
    }
}

/// Accept any certificate. Exists solely for the server's self-signed
/// development certificates behind an explicit `--insecure`; signatures are
/// still verified so a garbled handshake fails rather than "succeeding".
#[derive(Debug)]
struct InsecureVerifier {
    provider: rustls::crypto::CryptoProvider,
}

impl InsecureVerifier {
    fn new() -> Self {
        Self {
            provider: rustls::crypto::ring::default_provider(),
        }
    }
}

impl rustls::client::danger::ServerCertVerifier for InsecureVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
