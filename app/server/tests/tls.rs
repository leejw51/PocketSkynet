//! HTTPS with a self-signed certificate.
//!
//! The point of these tests is that a *real* client accepts what the server
//! presents. Every request here goes through a client that trusts the
//! generated CA and nothing else, with certificate verification fully on — so
//! a chain that only works with verification disabled fails the suite, which is
//! exactly the failure that would otherwise be discovered on somebody's tablet.

mod common;

use common::TestServer;
use futures_util::{SinkExt, StreamExt};

#[tokio::test]
async fn serves_https_with_a_certificate_a_real_client_verifies() {
    let server = TestServer::start_tls().await;

    let resp = server
        .client()
        .get(server.url("/api/health"))
        .send()
        .await
        .expect("a client trusting the generated CA must complete the handshake");

    assert_eq!(resp.status(), 200);
    assert!(server.base_url.starts_with("https://"));
}

#[tokio::test]
async fn the_certificate_is_rejected_by_a_client_that_does_not_know_the_ca() {
    // The other half of the previous test: the handshake succeeds because of
    // the CA, not because the certificate would be accepted by anybody.
    let server = TestServer::start_tls().await;

    let stranger = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();

    let result = stranger.get(server.url("/api/health")).send().await;
    assert!(
        result.is_err(),
        "a self-signed certificate must not validate against the public roots"
    );
}

#[tokio::test]
async fn plain_http_redirects_to_https_on_the_host_the_client_used() {
    let server = TestServer::start_tls().await;

    // Redirects unfollowed: the response itself is what is under test.
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();

    let resp = http
        .get(server.redirect_url("/rooms?a=1"))
        .send()
        .await
        .expect("the redirect listener must answer plain HTTP");

    assert_eq!(
        resp.status(),
        307,
        "a cached permanent redirect would outlive --tls"
    );
    assert_eq!(
        resp.headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default(),
        format!("https://127.0.0.1:{}/rooms?a=1", server.port),
        "the path, the query and the host the client used must all survive"
    );
}

#[tokio::test]
async fn the_ca_is_downloadable_over_plain_http_for_installing_on_a_device() {
    let server = TestServer::start_tls().await;

    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();

    for path in ["/ca.crt", "/cert.crt"] {
        let resp = http.get(server.redirect_url(path)).send().await.unwrap();
        assert_eq!(resp.status(), 200, "{path} must not be redirected to HTTPS");
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default(),
            // Anything else and iOS shows the certificate as text instead of
            // offering to install it.
            "application/x-x509-ca-cert",
            "{path} must be served as an installable certificate"
        );

        let body = resp.text().await.unwrap();
        assert_eq!(
            body,
            std::fs::read_to_string(server.ca_path()).unwrap(),
            "{path} must serve the CA the server is actually using"
        );
        assert!(body.starts_with("-----BEGIN CERTIFICATE-----"));
    }
}

#[tokio::test]
async fn realtime_works_over_wss() {
    // The messenger is only usable if realtime survives the switch to TLS, and
    // this is the one part of the stack where it could plausibly not: a
    // WebSocket needs an HTTP/1.1 upgrade, and an HTTPS server that negotiated
    // HTTP/2 for everything else has to still offer `http/1.1` over ALPN for
    // the handshake to have anywhere to happen.
    let server = TestServer::start_tls().await;
    let user = common::new_user(&server, "wss-tester").await;

    let mut request =
        tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
            server.ws_url("/ws"),
        )
        .expect("a valid wss request");
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        format!("fnauth, {}", user.token())
            .parse()
            .expect("header value"),
    );

    let (mut ws, response) = tokio_tungstenite::connect_async_tls_with_config(
        request,
        None,
        false,
        Some(tls_connector(&server)),
    )
    .await
    .expect("the WebSocket handshake must complete over TLS");

    assert_eq!(response.status(), 101, "the upgrade must be accepted");

    // And the socket has to actually carry traffic, not merely open.
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::json!({ "type": "ping" }).to_string(),
    ))
    .await
    .expect("send a frame");

    let reply = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await
        .expect("the server must answer within five seconds")
        .expect("the stream must not be closed")
        .expect("the frame must not be an error");

    assert!(
        reply.into_text().unwrap_or_default().contains("pong"),
        "a ping over wss must come back as a pong"
    );
}

/// A WebSocket TLS connector that trusts this server's CA and nothing else.
fn tls_connector(server: &TestServer) -> tokio_tungstenite::Connector {
    let mut roots = rustls::RootCertStore::empty();
    let pem = server.ca_pem().expect("a TLS server has a CA");
    for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
        roots
            .add(cert.expect("a certificate in the CA file"))
            .expect("trust the CA");
    }

    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(config))
}

#[tokio::test]
async fn the_leaf_is_valid_for_the_addresses_this_host_answers_on() {
    let server = TestServer::start_tls().await;

    // Loopback is the one address every host has, and the one this suite can
    // connect to — reaching `/api/health` through a verifying client above
    // already proves the SAN covers it. What is asserted here is that the
    // certificate is not *only* about loopback: every non-loopback IPv4 the
    // host holds must be named too, or joining from a tablet fails with a
    // hostname mismatch that no amount of trusting the CA repairs.
    let names = pocketskynet_server::tls::local_names();
    let chain = std::fs::read_to_string(server.data_dir.join("tls").join("server.crt")).unwrap();
    let leaf = rcgen::CertificateParams::from_ca_cert_pem(&chain).unwrap();

    assert_eq!(
        leaf.subject_alt_names.len(),
        names.len(),
        "every local address must be a SAN: {names:?}"
    );
}
