//! The login flow against a live server: challenge → EIP-191 sign → JWT,
//! plus every way a credential can be wrong.

use http::Method;
use pocketskynet_client::{Client, ClientError, Transport, Wallet};
use serde_json::json;

use crate::common::{self, TestServer, JWT_SECRET};

#[tokio::test]
async fn login_returns_a_usable_jwt_and_the_wallet_identity() {
    let server = TestServer::start().await;
    let wallet = Wallet::random().unwrap();
    let mut client = common::h1(&server);

    let session = client.login(&wallet, Some("alice")).await.unwrap();
    assert_eq!(session.user.wallet_address, wallet.address().as_str());
    assert_eq!(session.user.username, "alice");
    assert!(!session.token.is_empty());
    assert_eq!(client.token(), Some(session.token.as_str()));
    // The salt is owner-only material served on login (API.md §6.2.2).
    assert_eq!(
        session.encryption_salt.as_deref().map(str::len),
        Some(64),
        "the encryption salt is 32 bytes hex"
    );

    // And the token is honoured on an authenticated endpoint.
    client.rooms().await.expect("the JWT must be accepted");
}

#[tokio::test]
async fn a_first_login_without_a_username_falls_back_to_the_deterministic_one() {
    // Client::login retries with a *fresh* challenge (a failed login burns
    // the old one) and the protocol's deterministic username.
    let server = TestServer::start().await;
    let wallet = Wallet::random().unwrap();
    let mut client = common::h1(&server);

    let session = client.login(&wallet, None).await.unwrap();
    assert_eq!(
        session.user.username,
        pocketskynet_core::deterministic_username(wallet.address()),
    );
}

#[tokio::test]
async fn a_repeat_login_without_a_username_reuses_the_stored_one() {
    let server = TestServer::start().await;
    let wallet = Wallet::random().unwrap();

    let mut first = common::h1(&server);
    first.login(&wallet, Some("keeper")).await.unwrap();

    // No username this time — the server reuses the stored one, and the
    // client must not overwrite it with the deterministic fallback.
    let mut second = common::h1(&server);
    let session = second.login(&wallet, None).await.unwrap();
    assert_eq!(session.user.username, "keeper");
}

#[tokio::test]
async fn a_signature_from_the_wrong_wallet_is_rejected_with_401() {
    let server = TestServer::start().await;
    let wallet = Wallet::random().unwrap();
    let intruder = Wallet::random().unwrap();

    // Drive the wire by hand: a correct challenge for `wallet`, signed by
    // somebody else entirely.
    let client = common::h1(&server);
    let challenge = client.challenge(wallet.address().as_str()).await.unwrap();
    let forged = intruder.personal_sign(&challenge.message).unwrap();

    let raw = Transport::http1(&server.base_url, &common::insecure()).unwrap();
    let response = raw
        .request(
            Method::POST,
            "/api/auth/login",
            None,
            Some(&json!({
                "walletAddress": wallet.address().as_str(),
                "challengeId": challenge.challenge_id,
                "signature": forged,
                "username": "mallory",
            })),
        )
        .await
        .unwrap();

    assert_eq!(response.status, 401);
    let body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(body["message"], "Invalid signature");
}

#[tokio::test]
async fn a_challenge_is_single_use() {
    let server = TestServer::start().await;
    let wallet = Wallet::random().unwrap();

    let client = common::h1(&server);
    let challenge = client.challenge(wallet.address().as_str()).await.unwrap();
    let signature = wallet.personal_sign(&challenge.message).unwrap();
    let body = json!({
        "walletAddress": wallet.address().as_str(),
        "challengeId": challenge.challenge_id,
        "signature": signature,
        "username": "onceonly",
    });

    let raw = Transport::http1(&server.base_url, &common::insecure()).unwrap();
    let first = raw
        .request(Method::POST, "/api/auth/login", None, Some(&body))
        .await
        .unwrap();
    assert_eq!(first.status, 200, "the first use must succeed");

    // The exact same, perfectly valid signature again: the challenge was
    // consumed atomically, so a replay has nothing to verify against.
    let replay = raw
        .request(Method::POST, "/api/auth/login", None, Some(&body))
        .await
        .unwrap();
    assert_eq!(replay.status, 400);
    let envelope: serde_json::Value = serde_json::from_slice(&replay.body).unwrap();
    assert_eq!(envelope["message"], "Invalid or expired challenge");
}

#[tokio::test]
async fn a_tampered_jwt_is_rejected_on_an_authed_endpoint() {
    let server = TestServer::start().await;
    let wallet = Wallet::random().unwrap();
    let mut client = common::h1(&server);
    client.login(&wallet, Some("victim")).await.unwrap();

    // Flip the last character of the signature segment.
    let mut token = client.token().unwrap().to_owned();
    let flipped = if token.ends_with('A') { 'B' } else { 'A' };
    token.pop();
    token.push(flipped);
    client.set_token(token);

    match client.rooms().await {
        Err(ClientError::Api {
            status: 401,
            message,
            ..
        }) => assert_eq!(message, "Invalid token"),
        other => panic!("a tampered token must be a 401, got {other:?}"),
    }
}

#[tokio::test]
async fn an_expired_jwt_is_rejected() {
    // The harness pins --jwt-secret, so the test can mint a token that is
    // correctly signed and already dead.
    let server = TestServer::start().await;
    let wallet = Wallet::random().unwrap();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let expired = jsonwebtoken::encode(
        &jsonwebtoken::Header::default(), // HS256
        &json!({
            "walletAddress": wallet.address().as_str(),
            "iat": now - 10_000,
            "exp": now - 5_000,
        }),
        &jsonwebtoken::EncodingKey::from_secret(JWT_SECRET.as_bytes()),
    )
    .unwrap();

    let mut client = common::h1(&server);
    client.set_token(expired);
    match client.rooms().await {
        Err(ClientError::Api { status: 401, .. }) => {}
        other => panic!("an expired token must be a 401, got {other:?}"),
    }
}

#[tokio::test]
async fn garbage_bearer_tokens_are_401_and_no_token_fails_locally() {
    let server = TestServer::start().await;

    // Without login the client refuses locally, before any request is made.
    let client: Client = common::h1(&server);
    assert!(matches!(
        client.rooms().await,
        Err(ClientError::NotLoggedIn)
    ));

    let mut garbage = common::h1(&server);
    garbage.set_token("not-a-jwt".into());
    match garbage.rooms().await {
        Err(ClientError::Api {
            status: 401,
            message,
            ..
        }) => assert_eq!(message, "Invalid token"),
        other => panic!("garbage token must be a 401, got {other:?}"),
    }
}
