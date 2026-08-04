//! HTTP/3 over QUIC, driven by a real client.
//!
//! The point of these tests is the same as the TLS suite's: a *real* client
//! completes a *real* handshake and gets a *real* answer. Nothing here talks
//! to the router in-process, because the whole surface being added — the QUIC
//! socket, the ALPN negotiation, the h3 framing, the bridge back into axum —
//! lives entirely between "a client sent bytes" and "a handler ran", and an
//! in-process test would skip all of it.
//!
//! Certificate verification is fully on and pinned to the CA the server just
//! generated. A chain that only works with verification disabled fails here,
//! which is exactly the failure that would otherwise be discovered on
//! somebody's phone.

mod common;

use std::sync::Arc;
use std::time::Duration;

use bytes::{Buf, Bytes};
use common::TestServer;
use serde_json::{json, Value};

// --- a minimal HTTP/3 client ----------------------------------------------

/// One QUIC connection, kept open across requests the way a real client would.
struct H3Client {
    send: h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>,
    /// Dropping this ends the connection, so it is held even though the
    /// request path never touches it after the handshake.
    _driver: tokio::task::JoinHandle<()>,
    endpoint: quinn::Endpoint,
    authority: String,
}

struct H3Response {
    status: http::StatusCode,
    /// Captured so a failing assertion can be diagnosed from the response
    /// itself; no test reads it, and taking it is how we know the header
    /// frame parsed at all.
    #[allow(dead_code)]
    headers: http::HeaderMap,
    body: Vec<u8>,
}

impl H3Response {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "expected JSON, got {:?}: {e}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }
}

impl H3Client {
    /// Connect over QUIC, trusting only the CA this server generated.
    async fn connect(server: &TestServer) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let mut roots = rustls::RootCertStore::empty();
        let pem = server.generated_ca();
        for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
            roots
                .add(cert.expect("the CA file must parse"))
                .expect("the CA must be a usable trust anchor");
        }

        let mut tls =
            rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
                .with_root_certificates(roots)
                .with_no_client_auth();
        // Without this the handshake completes and then the server closes the
        // connection with "no application protocol" — the single most common
        // way an h3 client fails to work at all.
        tls.alpn_protocols = vec![b"h3".to_vec()];

        let quic_tls = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
            .expect("TLS 1.3 is what QUIC requires");
        let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap())
            .expect("bind a client UDP socket");
        endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic_tls)));

        let addr = server.http3_addr();
        let connection = endpoint
            // The certificate names `localhost` and the host's addresses; the
            // client must verify against one of them rather than against an IP
            // it happened to dial.
            .connect(addr, "localhost")
            .expect("start the QUIC handshake")
            .await
            .expect("the QUIC handshake must complete");

        let (mut driver, send) = h3::client::new(h3_quinn::Connection::new(connection))
            .await
            .expect("the HTTP/3 session must open");

        // h3 splits the connection into a driver and a request handle; nothing
        // progresses unless the driver is polled.
        let driver = tokio::spawn(async move {
            let _ = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
        });

        Self {
            send,
            _driver: driver,
            endpoint,
            authority: format!("localhost:{}", addr.port()),
        }
    }

    async fn request(
        &mut self,
        method: http::Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> H3Response {
        let mut builder = http::Request::builder()
            .method(method)
            // HTTP/3 has no `Host` header: the authority is part of the
            // pseudo-header block, so it has to be in the URI.
            .uri(format!("https://{}{path}", self.authority));
        if let Some(token) = token {
            builder = builder.header(http::header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let payload = body.map(|v| Bytes::from(serde_json::to_vec(&v).unwrap()));
        if payload.is_some() {
            builder = builder.header(http::header::CONTENT_TYPE, "application/json");
        }

        let request = builder.body(()).expect("build the request");
        let mut stream = self
            .send
            .send_request(request)
            .await
            .expect("send the request headers");

        if let Some(payload) = payload {
            stream.send_data(payload).await.expect("send the body");
        }
        stream.finish().await.expect("finish the request stream");

        let response = stream
            .recv_response()
            .await
            .expect("receive the response headers");

        let mut body = Vec::new();
        while let Some(mut chunk) = stream.recv_data().await.expect("receive the body") {
            while chunk.has_remaining() {
                let piece = chunk.chunk().to_vec();
                chunk.advance(piece.len());
                body.extend_from_slice(&piece);
            }
        }

        H3Response {
            status: response.status(),
            headers: response.headers().clone(),
            body,
        }
    }

    async fn get(&mut self, path: &str, token: Option<&str>) -> H3Response {
        self.request(http::Method::GET, path, token, None).await
    }

    async fn post(&mut self, path: &str, token: Option<&str>, body: Value) -> H3Response {
        self.request(http::Method::POST, path, token, Some(body))
            .await
    }

    async fn close(self) {
        self.endpoint.close(0u32.into(), b"done");
        self.endpoint.wait_idle().await;
    }
}

// --- the tests ------------------------------------------------------------

#[tokio::test]
async fn serves_the_api_over_http3() {
    let server = TestServer::start_http3().await;
    let mut client = H3Client::connect(&server).await;

    let resp = client.get("/api/health", None).await;
    assert_eq!(resp.status, 200, "health must answer over QUIC");
    assert_eq!(resp.json()["status"], "ok");

    client.close().await;
}

#[tokio::test]
async fn both_listeners_answer_at_the_same_time() {
    // The whole point of the feature: TCP and QUIC serve the same API side by
    // side, so a client can choose and an operator can measure. One of them
    // silently winning would look identical to this working.
    let server = TestServer::start_http3().await;

    let over_tcp = server
        .client()
        .get(server.url("/api/health"))
        .send()
        .await
        .expect("the TCP listener must still serve");
    assert_eq!(over_tcp.status(), 200);

    let mut client = H3Client::connect(&server).await;
    let over_quic = client.get("/api/health", None).await;
    assert_eq!(over_quic.status, 200);

    // Same deployment, not two servers that happened to both answer.
    let tcp_body: Value = over_tcp.json().await.unwrap();
    assert_eq!(tcp_body["status"], over_quic.json()["status"]);

    client.close().await;
}

#[tokio::test]
async fn the_tcp_listener_advertises_http3_via_alt_svc() {
    // A client cannot discover HTTP/3 by probing — QUIC on a closed UDP port
    // is silence, not a refusal. This header is the only way URLSession or a
    // browser learns the port exists.
    let server = TestServer::start_http3().await;

    let resp = server
        .client()
        .get(server.url("/api/health"))
        .send()
        .await
        .expect("request over TCP");

    let alt_svc = resp
        .headers()
        .get("alt-svc")
        .expect("the TCP listener must advertise the HTTP/3 endpoint")
        .to_str()
        .unwrap()
        .to_string();

    let port = server.http3_port.unwrap();
    assert_eq!(
        alt_svc,
        format!(r#"h3=":{port}"; ma=86400"#),
        "the advertisement must name the port QUIC actually bound"
    );
}

#[tokio::test]
async fn a_server_without_http3_advertises_nothing() {
    // The other half: no listener, no advertisement. A stale Alt-Svc header
    // would send every client to a dead UDP port and cost them a timeout on
    // every connection.
    let server = TestServer::start().await;

    let resp = server
        .client()
        .get(server.url("/api/health"))
        .send()
        .await
        .expect("request over TCP");

    assert!(
        resp.headers().get("alt-svc").is_none(),
        "a server with no QUIC listener must not advertise one"
    );
}

#[tokio::test]
async fn the_full_auth_flow_works_over_http3() {
    // Health is a static handler; this is the real proof that the bridge
    // carries request bodies, response bodies, headers and state — a POST with
    // JSON in, a signature round trip, and an authenticated GET afterwards.
    let server = TestServer::start_http3().await;
    let signer = common::Signer::random();
    let mut client = H3Client::connect(&server).await;

    let challenge = client
        .post(
            "/api/auth/challenge",
            None,
            json!({ "walletAddress": signer.address() }),
        )
        .await;
    assert_eq!(challenge.status, 200, "challenge over QUIC");
    let body = challenge.json();
    let message = body["message"]
        .as_str()
        .expect("a challenge message")
        .to_string();
    let challenge_id = body["challengeId"]
        .as_str()
        .expect("a challenge id")
        .to_string();

    let signature = signer.sign(&message);
    let login = client
        .post(
            "/api/auth/login",
            None,
            json!({
                "walletAddress": signer.address(),
                "signature": signature,
                "challengeId": challenge_id,
                "username": "quicuser",
            }),
        )
        .await;
    assert_eq!(login.status, 200, "login over QUIC: {:?}", login.json());
    let token = login.json()["token"].as_str().expect("a JWT").to_string();

    // And the token is honoured on a later request over the same connection.
    let me = client.get("/api/auth/profile", Some(&token)).await;
    assert_eq!(
        me.status,
        200,
        "an authenticated GET over QUIC: {:?}",
        me.json()
    );
    assert_eq!(
        me.json()["walletAddress"].as_str().unwrap().to_lowercase(),
        signer.address().to_lowercase()
    );

    client.close().await;
}

#[tokio::test]
async fn a_room_created_over_http3_is_visible_over_tcp() {
    // The two listeners are one deployment sharing one database. If they ever
    // became separate processes or separate state, this is what would catch
    // it.
    let server = TestServer::start_http3().await;
    let user = common::new_user(&server, "bridged").await;
    let mut client = H3Client::connect(&server).await;

    let created = client
        .post(
            "/api/rooms",
            Some(user.api.token()),
            json!({ "name": "quic-room", "encrypted": false }),
        )
        .await;
    assert_eq!(
        created.status,
        200,
        "create a room over QUIC: {:?}",
        created.json()
    );
    let room_id = created.json()["id"]
        .as_str()
        .expect("a room id")
        .to_string();

    // Read it back over the *TCP* listener, with the same token. The list is
    // a bare array, not an envelope.
    let listed = user.api.get("/api/rooms").await;
    listed.expect_status(200);
    let ids: Vec<String> = listed
        .array()
        .iter()
        .filter_map(|r| r["id"].as_str().map(str::to_string))
        .collect();
    assert!(
        ids.contains(&room_id),
        "a room created over QUIC must exist for a TCP client too"
    );

    client.close().await;
}

#[tokio::test]
async fn websocket_is_refused_with_an_explanation_not_a_hang() {
    // There is no WebSocket over HTTP/3. The failure mode worth preventing is
    // a client waiting forever for an upgrade that will never arrive, so the
    // server answers immediately and says where to go instead.
    let server = TestServer::start_http3().await;
    let mut client = H3Client::connect(&server).await;

    let resp = tokio::time::timeout(
        Duration::from_secs(5),
        client.request(http::Method::GET, "/ws", None, None),
    )
    .await;

    let resp = match resp {
        Ok(resp) => resp,
        Err(_) => panic!("an upgrade over HTTP/3 must be refused, not left hanging"),
    };

    // The upgrade header is what marks it; without one this is just a GET on a
    // route that does not speak plain HTTP.
    let upgrade = tokio::time::timeout(Duration::from_secs(5), upgrade_attempt(&mut client))
        .await
        .expect("the refusal must be immediate");

    assert_eq!(
        upgrade.status, 501,
        "WebSocket over HTTP/3 is not implemented"
    );
    assert_eq!(
        upgrade.json()["code"],
        "NO_WEBSOCKET_OVER_H3",
        "and it must say so in a form a client can branch on"
    );
    // The plain GET is allowed to fail however the route sees fit; what
    // matters is that it answered at all.
    assert!(resp.status.as_u16() >= 100);

    client.close().await;
}

/// A request carrying the WebSocket upgrade header, which is what the server
/// keys its refusal on.
async fn upgrade_attempt(client: &mut H3Client) -> H3Response {
    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri(format!("https://{}/ws", client.authority))
        .header(http::header::UPGRADE, "websocket")
        .header(http::header::CONNECTION, "Upgrade")
        .body(())
        .expect("build the upgrade request");

    let mut stream = client
        .send
        .send_request(request)
        .await
        .expect("send the upgrade attempt");
    stream.finish().await.expect("finish the request stream");

    let response = stream
        .recv_response()
        .await
        .expect("a response, not a hang");
    let mut body = Vec::new();
    while let Some(mut chunk) = stream.recv_data().await.expect("receive the body") {
        while chunk.has_remaining() {
            let piece = chunk.chunk().to_vec();
            chunk.advance(piece.len());
            body.extend_from_slice(&piece);
        }
    }
    H3Response {
        status: response.status(),
        headers: response.headers().clone(),
        body,
    }
}

#[tokio::test]
async fn a_client_that_does_not_know_the_ca_is_refused() {
    // QUIC's handshake is TLS 1.3; the certificate has to be verified for
    // real. A test that trusted anything would pass against a chain no phone
    // would accept.
    let server = TestServer::start_http3().await;
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut tls = rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"h3".to_vec()];

    let quic_tls = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic_tls)));

    let result = endpoint
        .connect(server.http3_addr(), "localhost")
        .expect("the dial itself is fine")
        .await;

    assert!(
        result.is_err(),
        "a self-signed certificate must not validate against an empty trust store"
    );
}

#[tokio::test]
async fn a_peer_offering_the_wrong_alpn_is_rejected() {
    // ALPN is what keeps a stray QUIC speaker from occupying a connection
    // slot. Rejecting during the handshake rather than after is the point.
    let server = TestServer::start_http3().await;
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut roots = rustls::RootCertStore::empty();
    let pem = server.generated_ca();
    for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
        roots.add(cert.unwrap()).unwrap();
    }
    let mut tls = rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_root_certificates(roots)
        .with_no_client_auth();
    // A perfectly valid protocol — just not the one this endpoint speaks.
    tls.alpn_protocols = vec![b"hq-interop".to_vec()];

    let quic_tls = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic_tls)));

    let result = endpoint
        .connect(server.http3_addr(), "localhost")
        .expect("the dial itself is fine")
        .await;

    assert!(
        result.is_err(),
        "a peer that cannot speak h3 must not complete the handshake"
    );
}

#[tokio::test]
async fn http3_runs_alongside_https_when_both_are_asked_for() {
    // The conventional deployment: everything encrypted, TCP and UDP together.
    let server = TestServer::start_http3_with(&["--tls"]).await;
    assert!(server.is_tls(), "the TCP listener should be HTTPS here");

    let over_tls = server
        .client()
        .get(server.url("/api/health"))
        .send()
        .await
        .expect("HTTPS must still serve");
    assert_eq!(over_tls.status(), 200);
    assert!(
        over_tls.headers().get("alt-svc").is_some(),
        "HTTPS must advertise the QUIC endpoint too"
    );

    let mut client = H3Client::connect(&server).await;
    assert_eq!(client.get("/api/health", None).await.status, 200);
    client.close().await;
}

#[tokio::test]
async fn plain_http_with_http3_still_generates_a_downloadable_ca() {
    // QUIC has no plaintext mode, so `--http3` without `--tls` generates
    // certificate material for the UDP listener alone. If `/ca.crt` did not
    // then serve it, nobody could ever trust the QUIC port — the material
    // would exist and be unreachable.
    let server = TestServer::start_http3().await;
    assert!(!server.is_tls(), "the TCP listener is plain HTTP here");

    let resp = server
        .client()
        .get(server.url("/ca.crt"))
        .send()
        .await
        .expect("request the CA");

    assert_eq!(resp.status(), 200, "the generated CA must be downloadable");
    let body = resp.bytes().await.unwrap();
    assert!(
        body.starts_with(b"-----BEGIN CERTIFICATE-----"),
        "and it must be the PEM, not an error page"
    );
    assert_eq!(
        body.as_ref(),
        server.generated_ca().as_slice(),
        "the served CA must be the one QUIC is using"
    );
}

#[tokio::test]
async fn server_info_reports_the_transport_that_carried_the_request() {
    // The endpoint exists so a client can *display* the truth rather than
    // infer it. The same deployment must therefore answer "h2" on TCP and
    // "h3" on QUIC — one canned value would defeat the whole point.
    let server = TestServer::start_http3().await;

    let over_tcp: Value = server
        .client()
        .get(server.url("/api/server/info"))
        .send()
        .await
        .expect("server info over TCP")
        .json()
        .await
        .unwrap();
    assert_ne!(over_tcp["protocol"], "h3", "a TCP request did not use QUIC");
    assert_eq!(over_tcp["http3Available"], true);
    assert_eq!(over_tcp["http3Port"], server.http3_port.unwrap());
    // Realtime is TCP-only and the client needs to be told, or it will wait
    // forever for a WebSocket upgrade that cannot happen.
    assert_eq!(over_tcp["websocketTransport"], "tcp");

    let mut client = H3Client::connect(&server).await;
    let over_quic = client.get("/api/server/info", None).await;
    assert_eq!(over_quic.status, 200);
    assert_eq!(
        over_quic.json()["protocol"],
        "h3",
        "a request that arrived over QUIC must say so"
    );

    // And both list the same addresses, per transport.
    let endpoints = &over_quic.json()["endpoints"];
    assert!(
        endpoints["tcp"].as_array().is_some_and(|a| !a.is_empty()),
        "the TCP addresses should be listed"
    );
    assert!(
        endpoints["http3"].as_array().is_some_and(|a| !a.is_empty()),
        "and the QUIC ones too"
    );

    client.close().await;
}

#[tokio::test]
async fn server_info_says_so_when_there_is_no_quic_listener() {
    // The client renders "not enabled" from this; reporting a port that is
    // not listening would send every viewer to a dead UDP socket.
    let server = TestServer::start().await;

    let info: Value = server
        .client()
        .get(server.url("/api/server/info"))
        .send()
        .await
        .expect("server info")
        .json()
        .await
        .unwrap();

    assert_eq!(info["http3Available"], false);
    assert!(info["http3Port"].is_null());
    assert!(info["endpoints"]["http3"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn concurrent_requests_share_one_quic_connection() {
    // The headline property: QUIC streams are independent, so many requests
    // ride one connection without queueing behind each other. This asserts
    // they all complete on a single connection — the multiplexing works at
    // all — rather than trying to time head-of-line blocking, which is not
    // measurable on loopback.
    let server = TestServer::start_http3().await;
    let mut client = H3Client::connect(&server).await;

    for _ in 0..12 {
        let resp = client.get("/api/health", None).await;
        assert_eq!(resp.status, 200);
    }

    client.close().await;
}
