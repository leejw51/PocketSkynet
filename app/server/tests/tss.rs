//! TSS wallet end-to-end, user-held-share edition: DKG over the API, a
//! one-shot collect of the n sealed share files, login with a **strict
//! subset** of them (the lost-share case), transaction signing, and the
//! quorum guards. Spec: `docs/CRYPTO.md` §15; issue #85.
//!
//! This runs a real 2-of-3 CGGMP21 ceremony, so it takes a minute or two;
//! the wider quorum matrix is covered by the `pocketskynet-tss` crate's own
//! integration test.

mod common;

use common::*;
use pocketskynet_core::k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use pocketskynet_core::{eip191, LegacyTransaction, WalletAddress};
use serde_json::{json, Value};

const PASSPHRASE: &str = "correct horse battery staple";

#[tokio::test]
async fn a_tss_wallet_survives_losing_a_share_and_still_signs_everything() {
    let server = TestServer::start().await;
    let anon = Api::anonymous(&server.base_url);

    // --- creation: POST /api/tss/keygen, poll, collect ------------------
    let started = anon
        .post(
            "/api/tss/keygen",
            json!({ "threshold": 2, "parties": 3, "passphrase": PASSPHRASE }),
        )
        .await
        .expect_ok();
    let keygen_id = s(&started, "keygenId");
    assert_eq!(keygen_id.len(), 64, "a hex-32 capability");

    // While the ceremony runs, a second keygen must be refused, not queued.
    anon.post(
        "/api/tss/keygen",
        json!({ "threshold": 2, "parties": 3, "passphrase": PASSPHRASE }),
    )
    .await
    .expect_status(409);

    let address = wait_for_keygen(&anon).await;

    // The wrong capability collects nothing — the polled status endpoint
    // alone must never be enough to walk away with key material.
    anon.post(
        "/api/tss/keygen/collect",
        json!({ "keygenId": "f".repeat(64) }),
    )
    .await
    .expect_status(404);

    let collected = anon
        .post("/api/tss/keygen/collect", json!({ "keygenId": keygen_id }))
        .await
        .expect_ok();
    assert_eq!(s(&collected, "address"), address);
    assert_eq!(collected["threshold"], 2);
    assert_eq!(collected["parties"], 3);
    let shares = collected["shares"].as_array().expect("share files").clone();
    assert_eq!(shares.len(), 3, "one sealed file per party");
    for (i, f) in shares.iter().enumerate() {
        assert_eq!(f["type"], "pocketskynet-tss-share");
        assert_eq!(f["partyIndex"], i as u64);
        assert_eq!(s(f, "address"), address);
    }

    // Collect is one-shot: the server now holds nothing.
    anon.post("/api/tss/keygen/collect", json!({ "keygenId": keygen_id }))
        .await
        .expect_status(404);

    // --- the lost-share case: party 1's file is gone --------------------
    // Any t of the n files must be a complete wallet. Everything below
    // uses only files {0, 2}.
    let quorum = vec![shares[0].clone(), shares[2].clone()];

    // Below the threshold, nothing signs.
    anon.post(
        "/api/tss/sign",
        json!({ "shares": [shares[0]], "passphrase": PASSPHRASE, "message": "no" }),
    )
    .await
    .expect_status(400);

    // The passphrase still gates the quorum.
    anon.post(
        "/api/tss/sign",
        json!({ "shares": quorum, "passphrase": "not the passphrase", "message": "no" }),
    )
    .await
    .expect_status(401);

    // --- login: the server-side verifier cannot tell this is TSS -------
    let (challenge_id, message) = request_challenge(&anon, &address).await;
    let signed = anon
        .post(
            "/api/tss/sign",
            json!({ "shares": quorum, "passphrase": PASSPHRASE, "message": message }),
        )
        .await
        .expect_ok();
    let signature = s(&signed, "signature");
    assert_eq!(signature.len(), 132, "0x + 65 bytes of hex");
    let public_key = s(&signed, "publicKey");
    let binding_sig = s(&signed, "bindingSig");
    assert_eq!(public_key.len(), 130, "uncompressed SEC1 hex, no 0x");
    assert!(s(&signed, "encryptionKey").starts_with("0x"));

    let login = anon
        .post(
            "/api/auth/login",
            json!({
                "walletAddress": address,
                "username": "ThresholdTester",
                "challengeId": challenge_id,
                "signature": signature,
                "publicKey": public_key,
                "publicKeySig": binding_sig,
            }),
        )
        .await
        .expect_ok();
    let token = s(&login, "token");
    assert_eq!(s(&login["user"], "walletAddress"), address);

    // The JWT is a normal JWT: an authenticated read works.
    let api = Api::with_token(&server.base_url, &token);
    let profile = api.get("/api/auth/profile").await.expect_ok();
    assert_eq!(s(&profile, "walletAddress"), address);

    // And the published binding is the sealed one, verifiable by any peer.
    let wallet_address = WalletAddress::new(&address).unwrap();
    pocketskynet_core::verify_key_binding(&wallet_address, Some(&public_key), Some(&binding_sig))
        .expect("the published binding must verify");

    // --- transaction signing through /api/tss/sign-hash -----------------
    let tx = LegacyTransaction {
        nonce: 0,
        gas_price: 5_000_000_000_000,
        gas_limit: 21_000,
        to: Some(WalletAddress::new("0x3535353535353535353535353535353535353535").unwrap()),
        value: 1_000_000_000_000_000_000,
        data: vec![],
        chain_id: 338,
    };
    let sighash = tx.sighash();
    let sig = anon
        .post(
            "/api/tss/sign-hash",
            json!({
                "shares": quorum,
                "passphrase": PASSPHRASE,
                "hash": format!("0x{}", hex::encode(sighash)),
            }),
        )
        .await
        .expect_ok();

    let r = hex::decode(s(&sig, "r").trim_start_matches("0x")).unwrap();
    let s_bytes = hex::decode(s(&sig, "s").trim_start_matches("0x")).unwrap();
    let v = sig["v"].as_u64().expect("v is 0 or 1") as u8;
    let mut rs = [0u8; 64];
    rs[..32].copy_from_slice(&r);
    rs[32..].copy_from_slice(&s_bytes);

    // Assemble exactly as the web client will, then prove the raw bytes
    // recover to the TSS address — i.e. the chain would accept this as an
    // ordinary EIP-155 transaction from that account.
    let signed_tx = tx.sign_with_signature(&rs, v);
    assert!(!signed_tx.raw.is_empty());
    let k_sig = Signature::from_slice(&rs).unwrap();
    let rec = RecoveryId::try_from(v).unwrap();
    let recovered = VerifyingKey::recover_from_prehash(&sighash, &k_sig, rec).unwrap();
    let recovered_addr =
        eip191::address_from_public_key(&pocketskynet_core::k256::PublicKey::from(&recovered));
    assert_eq!(recovered_addr.as_str(), address);
}

/// Poll `GET /api/tss/keygen/status` until `done`, returning the address.
///
/// Generous ceiling: safe-prime generation is randomized, and a slow CI
/// runner drawing unlucky primes is a fact of life, not a failure.
async fn wait_for_keygen(anon: &Api) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
    loop {
        let body: Value = anon.get("/api/tss/keygen/status").await.expect_ok();
        match s(&body, "state").as_str() {
            "done" => return s(&body, "address"),
            "error" => panic!("keygen failed: {body}"),
            _ => {}
        }
        assert!(
            std::time::Instant::now() < deadline,
            "keygen did not finish within 10 minutes: {body}"
        );
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}
