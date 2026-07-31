//! Authentication: challenge, login, logout, profile, encryption salt, key
//! binding, and JWT acceptance/rejection. Spec: `docs/API.md` §1.3, §6.1, §6.2,
//! §7; `docs/CRYPTO.md` §2–§4.

mod common;

use common::*;
use serde_json::{json, Value};

// --- system ---------------------------------------------------------------

#[tokio::test]
async fn health_reports_status_ok_and_uptime() {
    let server = TestServer::start().await;
    let api = Api::anonymous(&server.base_url);

    let body = api.get("/api/health").await.expect_ok();

    // Note the body key is `status`, not `message` (§6.1.1).
    assert_eq!(s(&body, "status"), "ok");
    assert!(
        body.get("uptime").and_then(Value::as_i64).is_some(),
        "health must report whole-second uptime: {body}"
    );
}

#[tokio::test]
async fn health_needs_no_authentication() {
    let server = TestServer::start().await;
    Api::anonymous(&server.base_url)
        .get_with_auth("/api/health", None)
        .await
        .expect_status(200);
}

#[tokio::test]
async fn blockchain_info_is_public_and_fully_populated() {
    let server = TestServer::start().await;

    let body = Api::anonymous(&server.base_url)
        .get("/api/blockchain/info")
        .await
        .expect_ok();

    // §6.1.2: every value is a string; unset variables become `""` so a client
    // indexing these fields never hits an undefined.
    for key in [
        "chainId",
        "chainRpc",
        "chainName",
        "chainExplorer",
        "fruitnationHashCro",
        "fruitnationWallet",
    ] {
        assert!(
            body.get(key).and_then(Value::as_str).is_some(),
            "`{key}` must be a string: {body}"
        );
    }
}

#[tokio::test]
async fn security_headers_are_set_on_every_response() {
    let server = TestServer::start().await;
    let resp = Api::anonymous(&server.base_url).get("/api/health").await;

    for (header, want) in [
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        ("referrer-policy", "strict-origin-when-cross-origin"),
    ] {
        assert_eq!(
            resp.header(header).as_deref(),
            Some(want),
            "missing or wrong `{header}`; headers: {:?}",
            resp.headers
        );
    }
}

// --- challenge ------------------------------------------------------------

#[tokio::test]
async fn challenge_returns_id_message_and_expiry() {
    let server = TestServer::start().await;
    let api = Api::anonymous(&server.base_url);
    let signer = Signer::random();

    let body = api
        .post(
            "/api/auth/challenge",
            json!({ "walletAddress": signer.address() }),
        )
        .await
        .expect_ok();

    expect_keys(&body, &["challengeId", "message", "expiresAt"]);
    assert!(!s(&body, "challengeId").is_empty());
    assert!(
        s(&body, "expiresAt").ends_with('Z'),
        "expiresAt must be an ISO-8601 UTC string: {body}"
    );
}

#[tokio::test]
async fn challenge_message_matches_the_specified_bytes() {
    let server = TestServer::start().await;
    let api = Api::anonymous(&server.base_url);
    let signer = Signer::random();

    let (_, message) = request_challenge(&api, signer.address()).await;

    let nonce = nonce_of(&message);
    assert_eq!(nonce.len(), 64, "nonce must be 32 bytes of hex: {nonce}");
    assert!(
        nonce.chars().all(|c| c.is_ascii_hexdigit()),
        "nonce must be hex: {nonce}"
    );
    assert_eq!(
        message,
        crypto::expected_challenge_message(signer.address(), &nonce),
        "challenge message diverges from API.md §6.2.1"
    );
    assert!(
        !message.ends_with('\n'),
        "the challenge message has no trailing newline"
    );
}

#[tokio::test]
async fn challenge_lowercases_the_wallet_address() {
    let server = TestServer::start().await;
    let api = Api::anonymous(&server.base_url);
    let signer = Signer::random();
    let mixed = format!("0x{}", signer.address()[2..].to_uppercase());

    let (_, message) = request_challenge(&api, &mixed).await;

    assert!(
        message.contains(signer.address()),
        "the challenge must embed the lowercased address: {message}"
    );
    assert!(
        !message.contains(&mixed[2..]),
        "uppercase address leaked into the message"
    );
}

#[tokio::test]
async fn challenge_rejects_a_malformed_wallet_address() {
    let server = TestServer::start().await;
    let api = Api::anonymous(&server.base_url);

    for bad in [
        "not-an-address",
        "0x123",
        "",
        "0xZZZZ35cc6634c0532925a3b8d31ce5bb1c6e6b22",
    ] {
        api.post("/api/auth/challenge", json!({ "walletAddress": bad }))
            .await
            .expect_validation_failed();
    }
}

#[tokio::test]
async fn challenge_rejects_a_missing_wallet_address() {
    let server = TestServer::start().await;
    Api::anonymous(&server.base_url)
        .post("/api/auth/challenge", json!({}))
        .await
        .expect_validation_failed();
}

#[tokio::test]
async fn each_challenge_carries_a_fresh_nonce() {
    let server = TestServer::start().await;
    let api = Api::anonymous(&server.base_url);
    let signer = Signer::random();

    let (_, first) = request_challenge(&api, signer.address()).await;
    let (_, second) = request_challenge(&api, signer.address()).await;

    assert_ne!(
        nonce_of(&first),
        nonce_of(&second),
        "nonces must not repeat"
    );
}

// --- login ----------------------------------------------------------------

#[tokio::test]
async fn login_returns_user_token_wallet_and_salt() {
    let server = TestServer::start().await;
    let api = Api::anonymous(&server.base_url);
    let signer = Signer::random();
    let (challenge_id, message) = request_challenge(&api, signer.address()).await;

    let body = api
        .post(
            "/api/auth/login",
            json!({
                "walletAddress": signer.address(),
                "username": "alice",
                "challengeId": challenge_id,
                "signature": signer.sign(&message),
            }),
        )
        .await
        .expect_ok();

    expect_keys(&body, &["user", "token", "encryptionSalt"]);
    expect_user_shape(&body["user"]);
    assert_eq!(s(&body["user"], "username"), "alice");
    assert_eq!(s(&body["user"], "walletAddress"), signer.address());

    let salt = s(&body, "encryptionSalt");
    assert_eq!(salt.len(), 64, "the salt is 32 bytes of hex: {salt}");
    assert!(salt
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
}

#[tokio::test]
async fn login_normalises_a_mixed_case_address_to_lowercase() {
    let server = TestServer::start().await;
    let api = Api::anonymous(&server.base_url);
    let signer = Signer::random();
    let mixed = format!("0x{}", signer.address()[2..].to_uppercase());
    let (challenge_id, message) = request_challenge(&api, &mixed).await;

    let body = api
        .post(
            "/api/auth/login",
            json!({
                "walletAddress": mixed,
                "username": "mixedcase",
                "challengeId": challenge_id,
                "signature": signer.sign(&message),
            }),
        )
        .await
        .expect_ok();

    assert_eq!(s(&body["user"], "walletAddress"), signer.address());
}

#[tokio::test]
async fn a_challenge_is_burned_by_a_successful_login() {
    let server = TestServer::start().await;
    let api = Api::anonymous(&server.base_url);
    let signer = Signer::random();
    let (challenge_id, message) = request_challenge(&api, signer.address()).await;
    let signature = signer.sign(&message);

    let payload = json!({
        "walletAddress": signer.address(),
        "username": "burner",
        "challengeId": challenge_id,
        "signature": signature,
    });
    api.post("/api/auth/login", payload.clone())
        .await
        .expect_status(200);

    api.post("/api/auth/login", payload)
        .await
        .expect_error(400, "Invalid or expired challenge");
}

#[tokio::test]
async fn a_challenge_is_burned_even_by_a_failed_login() {
    let server = TestServer::start().await;
    let api = Api::anonymous(&server.base_url);
    let signer = Signer::random();
    let (challenge_id, message) = request_challenge(&api, signer.address()).await;

    // Wrong signature: signs a different message.
    api.post(
        "/api/auth/login",
        json!({
            "walletAddress": signer.address(),
            "username": "burner",
            "challengeId": challenge_id,
            "signature": signer.sign("some other message"),
        }),
    )
    .await
    .expect_status(401);

    // The challenge is consumed with DELETE … RETURNING *before* validation.
    api.post(
        "/api/auth/login",
        json!({
            "walletAddress": signer.address(),
            "username": "burner",
            "challengeId": challenge_id,
            "signature": signer.sign(&message),
        }),
    )
    .await
    .expect_error(400, "Invalid or expired challenge");
}

#[tokio::test]
async fn login_rejects_an_unknown_challenge_id() {
    let server = TestServer::start().await;
    let api = Api::anonymous(&server.base_url);
    let signer = Signer::random();

    api.post(
        "/api/auth/login",
        json!({
            "walletAddress": signer.address(),
            "username": "ghost",
            "challengeId": "00000000-0000-4000-8000-000000000000",
            "signature": signer.sign("anything"),
        }),
    )
    .await
    .expect_error(400, "Invalid or expired challenge");
}

#[tokio::test]
async fn login_rejects_a_signature_from_a_different_wallet() {
    let server = TestServer::start().await;
    let api = Api::anonymous(&server.base_url);
    let signer = Signer::random();
    let impostor = Signer::random();
    let (challenge_id, message) = request_challenge(&api, signer.address()).await;

    api.post(
        "/api/auth/login",
        json!({
            "walletAddress": signer.address(),
            "username": "victim",
            "challengeId": challenge_id,
            "signature": impostor.sign(&message),
        }),
    )
    .await
    .expect_error(401, "Invalid signature");
}

#[tokio::test]
async fn login_rejects_a_malformed_signature() {
    let server = TestServer::start().await;
    let api = Api::anonymous(&server.base_url);
    let signer = Signer::random();
    let (challenge_id, _) = request_challenge(&api, signer.address()).await;

    let resp = api
        .post(
            "/api/auth/login",
            json!({
                "walletAddress": signer.address(),
                "username": "victim",
                "challengeId": challenge_id,
                "signature": "0xdeadbeef",
            }),
        )
        .await;

    assert!(
        resp.code() == 400 || resp.code() == 401,
        "a malformed signature is a 400 (schema) or 401 (verify); got {} / {}",
        resp.code(),
        resp.text
    );
}

#[tokio::test]
async fn login_rejects_a_wallet_address_that_differs_from_the_challenge() {
    let server = TestServer::start().await;
    let api = Api::anonymous(&server.base_url);
    let alice = Signer::random();
    let bob = Signer::random();
    let (challenge_id, message) = request_challenge(&api, alice.address()).await;

    api.post(
        "/api/auth/login",
        json!({
            "walletAddress": bob.address(),
            "username": "bob",
            "challengeId": challenge_id,
            "signature": bob.sign(&message),
        }),
    )
    .await
    .expect_error(400, "Wallet address mismatch");
}

#[tokio::test]
async fn first_login_requires_a_username() {
    let server = TestServer::start().await;
    let api = Api::anonymous(&server.base_url);
    let signer = Signer::random();
    let (challenge_id, message) = request_challenge(&api, signer.address()).await;

    api.post(
        "/api/auth/login",
        json!({
            "walletAddress": signer.address(),
            "username": "",
            "challengeId": challenge_id,
            "signature": signer.sign(&message),
        }),
    )
    .await
    .expect_error(400, "Username is required for first-time login");
}

#[tokio::test]
async fn a_later_login_may_omit_the_username_and_reuses_the_stored_one() {
    let server = TestServer::start().await;
    let api = Api::anonymous(&server.base_url);
    let user = new_user(&server, "returning").await;

    let (challenge_id, message) = request_challenge(&api, user.address.as_str()).await;
    let body = api
        .post(
            "/api/auth/login",
            json!({
                "walletAddress": user.address,
                "challengeId": challenge_id,
                "signature": user.signer.sign(&message),
            }),
        )
        .await
        .expect_ok();

    assert_eq!(s(&body["user"], "username"), "returning");
}

#[tokio::test]
async fn login_rejects_a_username_with_forbidden_characters() {
    let server = TestServer::start().await;
    let api = Api::anonymous(&server.base_url);

    for bad in [
        "ab",
        "with<angle>",
        "quote\"name",
        "semi;colon",
        "back\\slash",
    ] {
        let signer = Signer::random();
        let (challenge_id, message) = request_challenge(&api, signer.address()).await;
        let resp = api
            .post(
                "/api/auth/login",
                json!({
                    "walletAddress": signer.address(),
                    "username": bad,
                    "challengeId": challenge_id,
                    "signature": signer.sign(&message),
                }),
            )
            .await;
        resp.expect_status(400);
        assert!(
            !resp.message().is_empty(),
            "a rejected username must explain itself: {}",
            resp.text
        );
    }
}

#[tokio::test]
async fn login_accepts_unicode_usernames() {
    let server = TestServer::start().await;
    for name in ["한글이름", "日本語の名前", "emoji🍎name"] {
        let user = new_user(&server, name).await;
        let body = user.api.get("/api/auth/profile").await.expect_ok();
        assert_eq!(s(&body, "username"), name);
    }
}

#[tokio::test]
async fn the_login_salt_matches_the_encryption_salt_endpoint() {
    let server = TestServer::start().await;
    let user = new_user(&server, "salty").await;

    let body = user.api.get("/api/auth/encryption-salt").await.expect_ok();

    assert_eq!(s(&body, "salt"), user.encryption_salt);
}

#[tokio::test]
async fn the_encryption_salt_is_stable_across_logins() {
    let server = TestServer::start().await;
    let first = new_user(&server, "stable").await;
    let second = login(&server, first.signer.clone(), "stable").await;

    assert_eq!(
        first.encryption_salt, second.encryption_salt,
        "a changing salt would change the derived E2EE key and lock the user out of every room"
    );
}

#[tokio::test]
async fn the_encryption_salt_requires_authentication() {
    let server = TestServer::start().await;
    Api::anonymous(&server.base_url)
        .get("/api/auth/encryption-salt")
        .await
        .expect_error(401, "No token provided");
}

#[tokio::test]
async fn two_accounts_get_different_encryption_salts() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;

    assert_ne!(alice.encryption_salt, bob.encryption_salt);
}

#[tokio::test]
async fn logout_always_succeeds_even_without_a_token() {
    let server = TestServer::start().await;
    Api::anonymous(&server.base_url)
        .post_empty("/api/auth/logout")
        .await
        .expect_message("Logged out successfully");
}

// --- key binding ----------------------------------------------------------

#[tokio::test]
async fn publishing_an_encryption_key_returns_only_address_and_public_key() {
    let server = TestServer::start().await;
    let user = new_user(&server, "publisher").await;
    let identity = crypto::derive_encryption_identity(&user.signer, &user.encryption_salt);
    let signature = crypto::key_binding_signature(&user.signer, &identity.public_key);

    let body = user
        .api
        .put(
            "/api/auth/encryption-key",
            json!({ "publicKey": identity.public_key, "publicKeySig": signature }),
        )
        .await
        .expect_ok();

    // §6.2.5: exactly these two fields.
    assert_eq!(s(&body, "walletAddress"), user.address);
    assert_eq!(s(&body, "publicKey"), identity.public_key);
    assert_eq!(
        body.as_object().map(|o| o.len()),
        Some(2),
        "the response carries only walletAddress and publicKey: {body}"
    );
}

#[tokio::test]
async fn a_published_key_appears_on_the_profile_and_is_verifiable() {
    let server = TestServer::start().await;
    let user = new_user(&server, "bound").await;
    let identity = user.publish_encryption_key().await;

    let profile = user.api.get("/api/auth/profile").await.expect_ok();

    assert_eq!(s(&profile, "publicKey"), identity.public_key);
    assert_eq!(
        s(&profile, "publicKey").len(),
        130,
        "uncompressed secp256k1, no 0x prefix"
    );
    let sig = s(&profile, "publicKeySig");
    assert!(
        crypto::verify_key_binding(&user.address, &identity.public_key, &sig),
        "the stored publicKeySig must recover to the owner (CRYPTO §4.3)"
    );
}

#[tokio::test]
async fn a_mismatched_key_binding_signature_is_rejected() {
    let server = TestServer::start().await;
    let user = new_user(&server, "victim").await;
    let attacker = Signer::random();
    let identity = crypto::derive_encryption_identity(&user.signer, &user.encryption_salt);

    // The attacker signs the binding for the victim's key with their own wallet.
    let forged = crypto::key_binding_signature(&attacker, &identity.public_key);

    user.api
        .put(
            "/api/auth/encryption-key",
            json!({ "publicKey": identity.public_key, "publicKeySig": forged }),
        )
        .await
        .expect_error(400, "Invalid public key binding signature");
}

#[tokio::test]
async fn a_binding_signature_over_a_different_public_key_is_rejected() {
    let server = TestServer::start().await;
    let user = new_user(&server, "swapper").await;
    let mine = crypto::derive_encryption_identity(&user.signer, &user.encryption_salt);
    let other = crypto::derive_encryption_identity(&Signer::random(), &user.encryption_salt);

    // Correct signer, wrong subject: the signature covers `other`, not `mine`.
    let signature = crypto::key_binding_signature(&user.signer, &other.public_key);

    user.api
        .put(
            "/api/auth/encryption-key",
            json!({ "publicKey": mine.public_key, "publicKeySig": signature }),
        )
        .await
        .expect_error(400, "Invalid public key binding signature");
}

#[tokio::test]
async fn publishing_a_non_hex_public_key_is_a_validation_error() {
    let server = TestServer::start().await;
    let user = new_user(&server, "badkey").await;

    user.api
        .put(
            "/api/auth/encryption-key",
            json!({ "publicKey": "0xnot-hex", "publicKeySig": "0xabcdef" }),
        )
        .await
        .expect_validation_failed();
}

#[tokio::test]
async fn publishing_an_encryption_key_requires_both_fields() {
    let server = TestServer::start().await;
    let user = new_user(&server, "halfkey").await;
    let identity = crypto::derive_encryption_identity(&user.signer, &user.encryption_salt);

    user.api
        .put(
            "/api/auth/encryption-key",
            json!({ "publicKey": identity.public_key }),
        )
        .await
        .expect_validation_failed();
}

#[tokio::test]
async fn login_can_carry_the_key_binding_inline() {
    let server = TestServer::start().await;
    let anon = Api::anonymous(&server.base_url);
    let user = new_user(&server, "inline").await;
    let identity = crypto::derive_encryption_identity(&user.signer, &user.encryption_salt);
    let signature = crypto::key_binding_signature(&user.signer, &identity.public_key);

    let (challenge_id, message) = request_challenge(&anon, &user.address).await;
    let body = anon
        .post(
            "/api/auth/login",
            json!({
                "walletAddress": user.address,
                "username": "inline",
                "challengeId": challenge_id,
                "signature": user.signer.sign(&message),
                "publicKey": identity.public_key,
                "publicKeySig": signature,
            }),
        )
        .await
        .expect_ok();

    assert_eq!(s(&body["user"], "publicKey"), identity.public_key);
    assert_eq!(s(&body["user"], "publicKeySig"), signature);
}

#[tokio::test]
async fn login_rejects_an_inline_binding_that_does_not_verify() {
    let server = TestServer::start().await;
    let anon = Api::anonymous(&server.base_url);
    let user = new_user(&server, "forger").await;
    let identity = crypto::derive_encryption_identity(&user.signer, &user.encryption_salt);
    let forged = crypto::key_binding_signature(&Signer::random(), &identity.public_key);

    let (challenge_id, message) = request_challenge(&anon, &user.address).await;
    anon.post(
        "/api/auth/login",
        json!({
            "walletAddress": user.address,
            "username": "forger",
            "challengeId": challenge_id,
            "signature": user.signer.sign(&message),
            "publicKey": identity.public_key,
            "publicKeySig": forged,
        }),
    )
    .await
    .expect_error(400, "Invalid public key binding signature");
}

#[tokio::test]
async fn a_plain_login_leaves_a_published_key_binding_intact() {
    // API.md §15 #3: the reference wipes `publicKeySig` on every login that
    // omits `publicKey`, silently un-binding the key. PocketSkynet must not.
    let server = TestServer::start().await;
    let user = new_user(&server, "keeper").await;
    let identity = user.publish_encryption_key().await;

    let relogged = login(&server, user.signer.clone(), "keeper").await;
    let profile = relogged.api.get("/api/auth/profile").await.expect_ok();

    assert_eq!(s(&profile, "publicKey"), identity.public_key);
    assert!(
        profile
            .get("publicKeySig")
            .and_then(Value::as_str)
            .is_some(),
        "publicKeySig must survive a login that omits publicKey: {profile}"
    );
}

// --- profile --------------------------------------------------------------

#[tokio::test]
async fn profile_returns_the_calling_user() {
    let server = TestServer::start().await;
    let user = new_user(&server, "self").await;

    let body = user.api.get("/api/auth/profile").await.expect_ok();

    expect_user_shape(&body);
    assert_eq!(s(&body, "walletAddress"), user.address);
    assert_eq!(s(&body, "username"), "self");
    assert!(body["publicKey"].is_null(), "no key published yet: {body}");
    assert!(body["publicKeySig"].is_null());
}

#[tokio::test]
async fn profile_update_changes_the_username() {
    let server = TestServer::start().await;
    let user = new_user(&server, "before").await;

    let body = user
        .api
        .put("/api/auth/profile", json!({ "username": "after" }))
        .await
        .expect_ok();

    expect_user_shape(&body);
    assert_eq!(s(&body, "username"), "after");
    let reread = user.api.get("/api/auth/profile").await.expect_ok();
    assert_eq!(s(&reread, "username"), "after");
}

#[tokio::test]
async fn profile_update_rejects_an_invalid_username() {
    let server = TestServer::start().await;
    let user = new_user(&server, "keeper").await;

    user.api
        .put("/api/auth/profile", json!({ "username": "no<angles>" }))
        .await
        .expect_validation_failed();
    user.api
        .put("/api/auth/profile", json!({ "username": "ab" }))
        .await
        .expect_validation_failed();
    user.api
        .put("/api/auth/profile", json!({}))
        .await
        .expect_validation_failed();
}

#[tokio::test]
async fn profile_update_cannot_change_the_wallet_address() {
    let server = TestServer::start().await;
    let user = new_user(&server, "fixed").await;
    let other = Signer::random();

    let body = user
        .api
        .put(
            "/api/auth/profile",
            json!({ "username": "fixed2", "walletAddress": other.address() }),
        )
        .await
        .expect_ok();

    assert_eq!(s(&body, "walletAddress"), user.address);
}

// --- JWT handling (§1.3) --------------------------------------------------

#[tokio::test]
async fn a_missing_token_is_rejected_with_no_token_provided() {
    let server = TestServer::start().await;
    let api = Api::anonymous(&server.base_url);

    api.get_with_auth("/api/auth/profile", None)
        .await
        .expect_error(401, "No token provided");
    api.get_with_auth("/api/auth/profile", Some(""))
        .await
        .expect_error(401, "No token provided");
    api.get_with_auth("/api/auth/profile", Some("   "))
        .await
        .expect_error(401, "No token provided");
}

#[tokio::test]
async fn a_bearer_scheme_with_no_token_is_rejected() {
    // §1.3: "empty after strip" is a missing token, not an invalid one — and
    // `Bearer` alone is a scheme with no credential, never a bare token that
    // happens to spell "Bearer".
    let server = TestServer::start().await;
    let api = Api::anonymous(&server.base_url);

    for header in ["Bearer ", "Bearer", "bearer   "] {
        api.get_with_auth("/api/auth/profile", Some(header))
            .await
            .expect_error(401, "No token provided");
    }
}

#[tokio::test]
async fn a_non_bearer_authorization_scheme_is_rejected() {
    let server = TestServer::start().await;
    let api = Api::anonymous(&server.base_url);

    for header in [
        "Basic dXNlcjpwYXNz",
        "Digest username=\"x\"",
        "Token abc123",
    ] {
        api.get_with_auth("/api/auth/profile", Some(header))
            .await
            .expect_error(401, "No token provided");
    }
}

#[tokio::test]
async fn a_malformed_token_is_rejected_with_invalid_token() {
    let server = TestServer::start().await;
    let api = Api::anonymous(&server.base_url);

    for token in [
        "garbage",
        "a.b.c",
        "Bearer.Bearer.Bearer",
        "..",
        "eyJhbGciOiJIUzI1NiJ9",
    ] {
        api.get_with_auth("/api/auth/profile", Some(&format!("Bearer {token}")))
            .await
            .expect_error(401, "Invalid token");
    }
}

#[tokio::test]
async fn a_token_signed_with_the_wrong_secret_is_rejected() {
    let server = TestServer::start().await;
    let user = new_user(&server, "target").await;
    let forged = mint_token_with_wrong_secret(&user.address);

    user.api
        .with_raw_token(&forged)
        .get("/api/auth/profile")
        .await
        .expect_error(401, "Invalid token");
}

#[tokio::test]
async fn an_expired_token_is_rejected() {
    let server = TestServer::start().await;
    let user = new_user(&server, "expired").await;
    let stale = mint_token(&user.address, now_secs() - 7200, now_secs() - 3600);

    user.api
        .with_raw_token(&stale)
        .get("/api/auth/profile")
        .await
        .expect_error(401, "Invalid token");
}

#[tokio::test]
async fn an_alg_none_token_is_rejected() {
    // The HS256 pin exists precisely to defeat this substitution.
    let server = TestServer::start().await;
    let user = new_user(&server, "algnone").await;
    let forged = mint_alg_none_token(&user.address);

    user.api
        .with_raw_token(&forged)
        .get("/api/auth/profile")
        .await
        .expect_error(401, "Invalid token");
}

#[tokio::test]
async fn a_token_with_a_stripped_signature_is_rejected() {
    let server = TestServer::start().await;
    let user = new_user(&server, "stripped").await;
    let mut parts: Vec<&str> = user.api.token().split('.').collect();
    parts.pop();
    let stripped = format!("{}.", parts.join("."));

    user.api
        .with_raw_token(&stripped)
        .get("/api/auth/profile")
        .await
        .expect_error(401, "Invalid token");
}

#[tokio::test]
async fn a_bare_token_without_the_bearer_prefix_is_accepted() {
    // §15 #16: bare-token support is retained for the native CLIs.
    let server = TestServer::start().await;
    let user = new_user(&server, "bare").await;

    let body = user
        .api
        .get_with_auth("/api/auth/profile", Some(user.api.token()))
        .await
        .expect_ok();

    assert_eq!(s(&body, "walletAddress"), user.address);
}

#[tokio::test]
async fn the_bearer_scheme_is_matched_case_insensitively() {
    // §1.3 recommendation for the Rust port.
    let server = TestServer::start().await;
    let user = new_user(&server, "caseless").await;

    for header in ["bearer", "BEARER", "BeArEr"] {
        let body = user
            .api
            .get_with_auth(
                "/api/auth/profile",
                Some(&format!("{header} {}", user.api.token())),
            )
            .await
            .expect_ok();
        assert_eq!(s(&body, "walletAddress"), user.address);
    }
}

#[tokio::test]
async fn a_token_for_a_deleted_identity_still_identifies_its_wallet() {
    // A token minted for an address that never logged in authenticates, but
    // `GET /api/auth/profile` has no row to return.
    let server = TestServer::start().await;
    let stranger = Signer::random();
    let token = mint_token(stranger.address(), now_secs(), now_secs() + 3600);

    Api::with_token(&server.base_url, &token)
        .get("/api/auth/profile")
        .await
        .expect_error(404, "User not found");
}
