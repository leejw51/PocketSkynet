//! Throwaway probe: hit the *running* server's QUIC listener the way a
//! browser would. Delete after use.

use std::sync::Arc;

use bytes::{Buf, Bytes};

#[tokio::test]
async fn probe_live_server() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let ca = std::env::var("PROBE_CA").unwrap_or_else(|_| {
        format!("{}/.pocketskynet/tls/ca.crt", std::env::var("HOME").unwrap())
    });
    let authority = std::env::var("PROBE_AUTHORITY").unwrap_or_else(|_| "localhost".into());
    let addr: std::net::SocketAddr = std::env::var("PROBE_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:9099".into())
        .parse()
        .unwrap();

    let pem = std::fs::read(&ca).expect("read the CA");
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
        roots.add(cert.unwrap()).unwrap();
    }

    let mut tls = rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"h3".to_vec()];

    let quic_tls = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic_tls)));

    eprintln!("dialling {addr} as {authority}");
    let connection = endpoint
        .connect(addr, &authority)
        .expect("start handshake")
        .await
        .expect("handshake must complete");
    eprintln!("QUIC handshake OK, rtt={:?}", connection.rtt());

    let (mut driver, mut send) = h3::client::new(h3_quinn::Connection::new(connection))
        .await
        .expect("h3 session");
    let _driver = tokio::spawn(async move {
        let _ = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
    });

    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri(format!("https://{authority}:{}/api/health", addr.port()))
        .body(())
        .unwrap();
    let mut stream = send.send_request(request).await.expect("send headers");
    stream.finish().await.expect("finish");

    let response = stream.recv_response().await.expect("response");
    eprintln!("status={} version={:?}", response.status(), response.version());

    let mut body = Vec::new();
    while let Some(mut chunk) = stream.recv_data().await.expect("body") {
        while chunk.has_remaining() {
            let piece = chunk.chunk().to_vec();
            chunk.advance(piece.len());
            body.extend_from_slice(&piece);
        }
    }
    eprintln!("body={}", String::from_utf8_lossy(&body));
    assert!(response.status().is_success());
}
