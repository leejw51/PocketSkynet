//! Transport parity: the same API over HTTP/1.1+TLS and HTTP/3, the
//! `--insecure` trust decision on both, and cross-transport consistency.

use pocketskynet_client::{Client, ClientError, Transport, TransportOptions, Wallet};

use crate::common::{self, TestServer};

#[tokio::test]
async fn health_answers_unauthenticated_over_plain_http() {
    let server = TestServer::start().await;
    let client = common::h1(&server);
    let health = client.health().await.unwrap();
    assert_eq!(health.status, "ok");
    assert!(
        health.uptime.is_some(),
        "uptime is whole seconds since boot"
    );
    assert_eq!(client.transport_name(), "http/1.1");
}

#[tokio::test]
async fn the_full_flow_works_over_https_with_insecure() {
    let server = TestServer::start_tls().await;
    assert!(server.is_tls());

    let wallet = Wallet::random().unwrap();
    let mut client = common::h1(&server); // insecure: trusts the dev cert
    client.login(&wallet, Some("tlsuser")).await.unwrap();

    let room = client.create_room("tls room", None).await.unwrap();
    client.send_message(&room.id, "over https").await.unwrap();
    let messages = client.messages(&room.id, 10).await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "over https");
}

#[tokio::test]
async fn a_self_signed_certificate_is_refused_without_insecure() {
    // The negative half of --insecure: with verification on (the default),
    // the dev server's self-signed chain must NOT validate — a client that
    // accepted it would accept anybody's.
    let server = TestServer::start_tls().await;

    let strict = TransportOptions { insecure: false };
    let client = Client::new(Transport::http1(&server.base_url, &strict).unwrap());
    match client.health().await {
        Err(ClientError::Transport(_)) => {}
        other => panic!("an unverifiable certificate must fail the transport, got {other:?}"),
    }
}

#[tokio::test]
async fn the_full_flow_works_over_http3() {
    let server = TestServer::start_http3().await;

    let wallet = Wallet::random().unwrap();
    let mut client = common::h3(&server).await;
    assert_eq!(client.transport_name(), "h3");

    assert_eq!(client.health().await.unwrap().status, "ok");

    let session = client.login(&wallet, Some("quicflow")).await.unwrap();
    assert_eq!(session.user.wallet_address, wallet.address().as_str());

    let room = client.create_room("quic room", None).await.unwrap();
    client.send_message(&room.id, "over quic").await.unwrap();
    let messages = client.messages(&room.id, 10).await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "over quic");
}

#[tokio::test]
async fn http3_without_insecure_fails_the_handshake() {
    // QUIC's TLS is verified for real by default; the self-signed listener
    // must not complete a handshake against the webpki roots.
    let server = TestServer::start_http3().await;
    let strict = TransportOptions { insecure: false };

    match Transport::http3(&server.base_url, &strict, server.http3_port).await {
        Err(ClientError::Transport(_)) => {}
        Ok(_) => panic!("a self-signed certificate must not validate against real roots"),
        Err(other) => panic!("expected a transport error, got {other:?}"),
    }
}

#[tokio::test]
async fn messages_cross_transports_in_both_directions() {
    // One deployment, two listeners: what one transport writes the other
    // reads, in order, because both land in the same database.
    let server = TestServer::start_http3().await;
    let wallet = Wallet::random().unwrap();

    let mut tcp = common::h1(&server);
    tcp.login(&wallet, Some("bridger")).await.unwrap();
    let mut quic = common::h3(&server).await;
    quic.login(&wallet, None).await.unwrap();

    let room = tcp.create_room("bridge", None).await.unwrap();
    tcp.send_message(&room.id, "sent over tcp").await.unwrap();
    quic.send_message(&room.id, "sent over quic").await.unwrap();

    // Read the whole conversation back over each transport.
    let over_quic = quic.messages(&room.id, 10).await.unwrap();
    let over_tcp = tcp.messages(&room.id, 10).await.unwrap();

    let expected = vec!["sent over tcp", "sent over quic"];
    let quic_contents: Vec<&str> = over_quic.iter().map(|m| m.content.as_str()).collect();
    let tcp_contents: Vec<&str> = over_tcp.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(quic_contents, expected, "QUIC sees the TCP write");
    assert_eq!(tcp_contents, expected, "TCP sees the QUIC write");

    // Identical rows, not merely identical text.
    let quic_ids: Vec<&str> = over_quic.iter().map(|m| m.id.as_str()).collect();
    let tcp_ids: Vec<&str> = over_tcp.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(quic_ids, tcp_ids, "one database behind both listeners");
}
