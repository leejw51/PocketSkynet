//! HTTP/3 over QUIC — a second listener speaking the same API.
//!
//! # Why
//!
//! Not "because it is newer". HTTP/3 buys this deployment three specific
//! things, and costs it one:
//!
//! * **No transport head-of-line blocking.** Over TCP every stream shares one
//!   ordered byte pipe, so a single lost segment stalls *every* concurrent
//!   request until it is retransmitted. QUIC streams are independent. On a
//!   clean LAN this is worth nothing; on hotel Wi-Fi or a train it is the
//!   difference between "slow" and "hung".
//! * **One round trip to first byte.** The TLS handshake is folded into the
//!   transport handshake instead of running on top of a completed one.
//! * **Connection migration.** A phone moving from Wi-Fi to cellular keeps the
//!   same QUIC connection — the connection ID survives the address change.
//!   For the iOS client this is the biggest of the three.
//!
//! What it costs: **there is no WebSocket over HTTP/3.** RFC 9220 defines
//! WebSocket-over-Extended-CONNECT, but no browser and no client this project
//! talks to implements it, so [`serve`] refuses upgrades with a plain 501
//! rather than hanging. The realtime ladder handles this already — SSE is an
//! ordinary streaming response body and works here unchanged, and the client
//! falls back to it when the socket is unavailable.
//!
//! And what it is *not*: faster on localhost. TCP is offloaded to the kernel
//! and often the NIC; QUIC does congestion control and packet assembly in
//! userspace. For a large upload over a good link, expect the TCP listener to
//! win. Both listeners run at once precisely so that either can be measured
//! and chosen, rather than one being asserted to be better.
//!
//! # 0-RTT is deliberately off
//!
//! QUIC can carry application data in the very first flight, which is the
//! headline latency number in every HTTP/3 benchmark. That data is replayable
//! by design: an observer can capture the flight and send it again, and the
//! server cannot tell the difference. On a server whose POSTs move CRO and
//! post messages, trading one round trip for a replay primitive is a bad
//! deal, so `max_early_data_size` stays at zero.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use bytes::{Buf, Bytes};
use futures_util::StreamExt;
use http::{Request, Response, StatusCode};
use tower::ServiceExt;

/// Largest request body accepted over HTTP/3.
///
/// The TCP listener's limits live in the individual routes; this is a
/// transport-level backstop so a peer cannot stream forever into memory
/// before any route gets a say. Generous enough for the largest thing the
/// API accepts (a published site zip).
const MAX_BODY: usize = 32 * 1024 * 1024;

/// How long a peer may take to complete the QUIC handshake.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Idle timeout. Long enough that an SSE stream with a slow event source is
/// not reaped mid-stream, short enough that a vanished phone's connection
/// state is released.
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Debug, thiserror::Error)]
pub enum Http3Error {
    #[error("reading {0}: {1}")]
    Read(std::path::PathBuf, #[source] std::io::Error),
    #[error("no certificate in {0}")]
    NoCertificate(std::path::PathBuf),
    #[error("no private key in {0}")]
    NoKey(std::path::PathBuf),
    #[error("building the QUIC TLS configuration: {0}")]
    Tls(#[from] rustls::Error),
    #[error("QUIC does not accept this TLS configuration: {0}")]
    NoQuicTls(#[from] quinn::crypto::rustls::NoInitialCipherSuite),
    #[error("binding the QUIC socket on {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
}

/// The ALPN token that identifies HTTP/3. A peer that does not offer it is
/// rejected during the handshake rather than after, which is what keeps a
/// stray QUIC speaker from occupying a connection slot.
const ALPN_H3: &[u8] = b"h3";

/// Build the QUIC server configuration from the same PEM material the TLS
/// listener uses.
///
/// QUIC mandates TLS 1.3, so — unlike the TCP listener, which still offers
/// 1.2 for older clients — there is nothing to negotiate down to here.
pub fn quic_config(chain: &Path, key: &Path) -> Result<quinn::ServerConfig, Http3Error> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let certs = load_certs(chain)?;
    let key = load_key(key)?;

    let mut tls = rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    tls.alpn_protocols = vec![ALPN_H3.to_vec()];
    // See the module docs: replayable early data is not worth one round trip
    // on a server that moves money.
    tls.max_early_data_size = 0;

    let quic_tls = quinn::crypto::rustls::QuicServerConfig::try_from(tls)?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(quic_tls));

    let transport = Arc::get_mut(&mut config.transport)
        .expect("the transport config is not shared before the endpoint exists");
    // Flow-control windows sized for the upload protocol, explicitly.
    //
    // quinn's defaults are tuned for request/response traffic and sit far
    // below one upload chunk (routes/uploads.rs suggests 8 MB and caps at
    // 16 MB). A sender pushing a body larger than the stream window must
    // pause mid-chunk and wait for MAX_STREAM_DATA credit — a dance our own
    // h3 test client performs correctly, and which shipped WebKit did not
    // survive on real devices: the first 8 MB PATCH from an iPhone or iPad
    // wedged, and with it the whole connection, which the browser then kept
    // reusing until a page refresh. A window comfortably larger than any
    // single chunk means Safari never waits for mid-body credit at all,
    // which sidesteps the class rather than the instance.
    //
    // The memory exposure is bounded and deliberate: only *unread* data is
    // buffered, our body loop reads eagerly, and this server is a LAN box
    // with a handful of peers — not a public edge balancing thousands of
    // connections.
    transport.stream_receive_window(
        quinn::VarInt::from_u32(32 * 1024 * 1024), // 2x the largest chunk
    );
    transport.receive_window(
        quinn::VarInt::from_u32(96 * 1024 * 1024), // several streams' worth
    );
    // The mirror direction: a 4 GB film streams *out* over this transport.
    transport.send_window(96 * 1024 * 1024);
    transport.max_idle_timeout(Some(
        IDLE_TIMEOUT
            .try_into()
            .expect("the idle timeout is inside QUIC's varint range"),
    ));
    // Keep-alives are the server's half of connection migration: a phone that
    // changes network mid-idle is only discovered to still be there if
    // something is being sent.
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(15)));
    config.max_incoming(256);

    Ok(config)
}

fn load_certs(path: &Path) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, Http3Error> {
    let pem = std::fs::read(path).map_err(|e| Http3Error::Read(path.to_path_buf(), e))?;
    let certs = rustls_pemfile::certs(&mut pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| Http3Error::Read(path.to_path_buf(), e))?;
    if certs.is_empty() {
        return Err(Http3Error::NoCertificate(path.to_path_buf()));
    }
    Ok(certs)
}

fn load_key(path: &Path) -> Result<rustls::pki_types::PrivateKeyDer<'static>, Http3Error> {
    let pem = std::fs::read(path).map_err(|e| Http3Error::Read(path.to_path_buf(), e))?;
    rustls_pemfile::private_key(&mut pem.as_slice())
        .map_err(|e| Http3Error::Read(path.to_path_buf(), e))?
        .ok_or_else(|| Http3Error::NoKey(path.to_path_buf()))
}

/// A bound QUIC socket, not yet serving.
///
/// Bound separately from serving for the same reason the TCP listener is: a
/// port that is already taken should fail startup with a clear message, not
/// surface later as connections that quietly go nowhere.
pub struct Http3Listener {
    endpoint: quinn::Endpoint,
    addr: SocketAddr,
}

impl Http3Listener {
    pub fn bind(addr: SocketAddr, config: quinn::ServerConfig) -> Result<Self, Http3Error> {
        let endpoint = quinn::Endpoint::server(config, addr)
            .map_err(|source| Http3Error::Bind { addr, source })?;
        let addr = endpoint
            .local_addr()
            .map_err(|source| Http3Error::Bind { addr, source })?;
        Ok(Self { endpoint, addr })
    }

    /// The address actually bound — resolves port 0 to what the OS chose.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Accept connections until `shutdown` resolves.
    pub async fn serve<F>(self, router: axum::Router, shutdown: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let endpoint = self.endpoint.clone();
        tokio::pin!(shutdown);

        loop {
            let incoming = tokio::select! {
                biased;
                _ = &mut shutdown => break,
                incoming = endpoint.accept() => match incoming {
                    Some(incoming) => incoming,
                    // `None` means the endpoint is closed; nothing more will
                    // ever arrive.
                    None => break,
                },
            };

            let router = router.clone();
            tokio::spawn(async move {
                // The handshake gets its own timeout: a peer that opens a
                // connection and then falls silent must not hold a task.
                let connecting = match tokio::time::timeout(HANDSHAKE_TIMEOUT, incoming).await {
                    Ok(Ok(connection)) => connection,
                    Ok(Err(e)) => {
                        tracing::debug!(error = %e, "QUIC handshake failed");
                        return;
                    }
                    Err(_) => {
                        tracing::debug!("QUIC handshake timed out");
                        return;
                    }
                };
                let peer = connecting.remote_address();
                if let Err(e) = drive_connection(connecting, router, peer).await {
                    tracing::debug!(%peer, error = %e, "HTTP/3 connection ended");
                }
            });
        }

        // Give in-flight streams a moment to finish, then tell peers why the
        // connection is going away rather than letting it time out.
        self.endpoint.close(0u32.into(), b"shutting down");
        self.endpoint.wait_idle().await;
    }
}

/// Run one QUIC connection's HTTP/3 session: accept requests until the peer
/// goes away, handling each on its own task so a slow response never blocks
/// the next request on the same connection.
async fn drive_connection(
    connection: quinn::Connection,
    router: axum::Router,
    peer: SocketAddr,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut h3 = h3::server::builder()
        .build::<h3_quinn::Connection, Bytes>(h3_quinn::Connection::new(connection))
        .await?;

    loop {
        match h3.accept().await {
            Ok(Some(resolver)) => {
                let router = router.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_request(resolver, router, peer).await {
                        tracing::debug!(%peer, error = %e, "HTTP/3 request failed");
                    }
                });
            }
            // A clean close.
            Ok(None) => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
}

/// Bridge one HTTP/3 request stream to the axum router.
///
/// The two halves are split so the request body can still be arriving while
/// the response is being written — the shape SSE needs, and the reason this
/// does not simply buffer everything.
async fn handle_request(
    resolver: h3::server::RequestResolver<h3_quinn::Connection, Bytes>,
    router: axum::Router,
    peer: SocketAddr,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (request, stream) = resolver.resolve_request().await?;
    let (mut send, mut recv) = stream.split();

    // WebSocket cannot ride HTTP/3 (see the module docs). Say so plainly
    // instead of letting the client wait for an upgrade that never comes.
    if is_websocket_upgrade(&request) {
        // Same flow-control hygiene as the 413 below: never answer early and
        // walk away from an unread body.
        recv.stop_sending(h3::error::Code::H3_REQUEST_REJECTED);
        let response = Response::builder()
            .status(StatusCode::NOT_IMPLEMENTED)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(())
            .expect("a constant response builds");
        send.send_response(response).await?;
        send.send_data(Bytes::from_static(
            br#"{"message":"WebSocket is not available over HTTP/3; use SSE at /api/events, or the HTTP/1.1 listener","code":"NO_WEBSOCKET_OVER_H3"}"#,
        ))
        .await?;
        send.finish().await?;
        return Ok(());
    }

    // The body is read into memory rather than streamed into the router.
    // Request bodies here are JSON and uploads with their own route-level
    // caps; streaming them would mean holding the recv half inside the
    // response future, and the added complexity buys nothing this API does.
    let mut body = Vec::new();
    while let Some(mut chunk) = recv.recv_data().await? {
        if body.len() + chunk.remaining() > MAX_BODY {
            // Tell the peer to stop sending before answering. Without this
            // the unread remainder of the body sits in the connection's
            // flow-control window forever — the stream is dead but its
            // credit is not returned, and once enough credit leaks, every
            // later request on the connection hangs with no error anywhere.
            // A refused body must be *refused*, not merely ignored.
            recv.stop_sending(h3::error::Code::H3_REQUEST_REJECTED);
            let response = Response::builder()
                .status(StatusCode::PAYLOAD_TOO_LARGE)
                .body(())
                .expect("a constant response builds");
            send.send_response(response).await?;
            send.finish().await?;
            return Ok(());
        }
        while chunk.has_remaining() {
            let piece = chunk.chunk().to_vec();
            chunk.advance(piece.len());
            body.extend_from_slice(&piece);
        }
    }

    let (mut parts, ()) = request.into_parts();
    // Without this the rate limiter keys every HTTP/3 request on the same
    // fallback address and throttles the whole world together — the TCP path
    // gets it from `into_make_service_with_connect_info`, and this is the
    // equivalent for a router driven directly.
    parts.extensions.insert(ConnectInfo(peer));
    // Say so explicitly. A handler asking `request.version()` is the only way
    // anything above this layer can tell which transport it is on, and h3 does
    // not stamp it — the default would claim HTTP/1.1 over QUIC.
    parts.version = http::Version::HTTP_3;
    let request = Request::from_parts(parts, Body::from(body));

    let response = router
        .oneshot(request)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;

    let (parts, body) = response.into_parts();
    send.send_response(Response::from_parts(parts, ())).await?;

    // Stream the body out frame by frame: an SSE response never completes, so
    // collecting it first would hang forever.
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => send.send_data(bytes).await?,
            Err(e) => {
                tracing::debug!(%peer, error = %e, "response body ended early");
                break;
            }
        }
    }
    send.finish().await?;
    Ok(())
}

/// Whether this request is trying to open a WebSocket.
///
/// Both spellings are checked: HTTP/1.1 clients send `Upgrade: websocket`,
/// and an HTTP/3 client attempting RFC 9220 sends `:protocol` as an extended
/// CONNECT — which arrives here as a CONNECT with that header.
fn is_websocket_upgrade<T>(request: &Request<T>) -> bool {
    let upgrade = request
        .headers()
        .get(http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let extended_connect = request.method() == http::Method::CONNECT;
    upgrade || extended_connect
}

/// The `Alt-Svc` value advertising this deployment's HTTP/3 endpoint.
///
/// Sent on the *TCP* listener's responses: a client cannot discover HTTP/3 by
/// trying it, so the only way it learns the port is being told over the
/// connection it already has.
pub fn alt_svc_value(port: u16) -> String {
    // `ma` is how long the advertisement may be cached. A day: long enough to
    // be worth having, short enough that moving the port is not a week-long
    // outage for clients that cached it.
    format!(r#"h3=":{port}"; ma=86400"#)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alt_svc_names_the_h3_port() {
        // The quoted form with a leading colon is what RFC 7838 requires for
        // "same host, different port"; dropping the quotes is the classic way
        // to make an advertisement silently ignored.
        assert_eq!(alt_svc_value(9101), r#"h3=":9101"; ma=86400"#);
    }

    #[test]
    fn websocket_upgrades_are_recognised_in_both_spellings() {
        let plain = Request::builder()
            .uri("/api/ws")
            .header(http::header::UPGRADE, "WebSocket")
            .body(())
            .unwrap();
        assert!(
            is_websocket_upgrade(&plain),
            "header match is case-insensitive"
        );

        let connect = Request::builder()
            .method(http::Method::CONNECT)
            .uri("/api/ws")
            .body(())
            .unwrap();
        assert!(is_websocket_upgrade(&connect), "extended CONNECT counts");

        let ordinary = Request::builder().uri("/api/health").body(()).unwrap();
        assert!(!is_websocket_upgrade(&ordinary));
    }

    #[test]
    fn the_alpn_token_is_the_registered_one() {
        // "h3-29" and friends were draft tokens; a server still offering one
        // negotiates with nothing current.
        assert_eq!(ALPN_H3, b"h3");
    }
}
