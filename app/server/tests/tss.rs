//! TSS wallet end-to-end: DKG over the API, the same challenge → sign →
//! login flow every wallet uses, transaction signing through
//! `/api/tss/sign-hash`, and passphrase-gated deletion.
//! Spec: `docs/CRYPTO.md` §15; issue #85.
//!
//! This runs a real CGGMP21 ceremony (2-of-2 — the cheapest honest one), so
//! it takes on the order of a minute; the m-of-n quorum matrix is covered by
//! the `pocketskynet-tss` crate's own integration test.

mod common;

use common::*;
use pocketskynet_core::k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use pocketskynet_core::{eip191, LegacyTransaction, WalletAddress};
use serde_json::json;

const PASSPHRASE: &str = "correct horse battery staple";

#[tokio::test]
async fn a_tss_wallet_is_born_signs_in_and_signs_transactions() {
    let server = TestServer::start().await;
    let anon = Api::anonymous(&server.base_url);

    // --- creation: POST /api/tss/keygen, then poll ---------------------
    anon.post(
        "/api/tss/keygen",
        json!({ "threshold": 2, "parties": 2, "passphrase": PASSPHRASE }),
    )
    .await
    .expect_ok();

    // While the ceremony runs, a second keygen must be refused, not queued.
    anon.post(
        "/api/tss/keygen",
        json!({ "threshold": 2, "parties": 2, "passphrase": PASSPHRASE }),
    )
    .await
    .expect_status(409);

    let address = wait_for_keygen(&anon).await;

    // The new wallet is listed, header only, without any passphrase.
    let body = anon.get("/api/tss/wallets").await.expect_ok();
    let wallets = body["wallets"].as_array().expect("a wallet list");
    assert_eq!(wallets.len(), 1);
    assert_eq!(s(&wallets[0], "address"), address);
    assert_eq!(wallets[0]["threshold"], 2);
    assert_eq!(wallets[0]["parties"], 2);

    // --- the passphrase is the gate ------------------------------------
    anon.post(
        "/api/tss/session-keys",
        json!({ "address": address, "passphrase": "not the passphrase" }),
    )
    .await
    .expect_status(401);

    let keys = anon
        .post(
            "/api/tss/session-keys",
            json!({ "address": address, "passphrase": PASSPHRASE }),
        )
        .await
        .expect_ok();
    let public_key = s(&keys, "publicKey");
    let binding_sig = s(&keys, "bindingSig");
    assert_eq!(public_key.len(), 130, "uncompressed SEC1 hex, no 0x");
    assert!(public_key.starts_with("04"));
    assert!(s(&keys, "encryptionKey").starts_with("0x"));

    // --- login: the server-side verifier cannot tell this is TSS -------
    let (challenge_id, message) = request_challenge(&anon, &address).await;
    let signed = anon
        .post(
            "/api/tss/sign",
            json!({ "address": address, "passphrase": PASSPHRASE, "message": message }),
        )
        .await
        .expect_ok();
    let signature = s(&signed, "signature");
    assert_eq!(signature.len(), 132, "0x + 65 bytes of hex");

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

    // And the published binding is the stored one, verifiable by any peer.
    let wallet_address = WalletAddress::new(&address).unwrap();
    pocketskynet_core::verify_key_binding(&wallet_address, Some(&public_key), Some(&binding_sig))
        .expect("the published binding must verify");

    // --- transaction signing through /api/tss/sign-hash ----------------
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
                "address": address,
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

    // --- deletion is passphrase-gated too ------------------------------
    anon.post(
        "/api/tss/delete",
        json!({ "address": address, "passphrase": "still not it" }),
    )
    .await
    .expect_status(401);
    anon.post(
        "/api/tss/delete",
        json!({ "address": address, "passphrase": PASSPHRASE }),
    )
    .await
    .expect_ok();
    let body = anon.get("/api/tss/wallets").await.expect_ok();
    assert_eq!(body["wallets"].as_array().unwrap().len(), 0);
}

/// Poll `GET /api/tss/keygen/status` until `done`, returning the address.
///
/// Generous ceiling: safe-prime generation is randomized, and a slow CI
/// runner drawing unlucky primes is a fact of life, not a failure.
async fn wait_for_keygen(anon: &Api) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
    loop {
        let body = anon.get("/api/tss/keygen/status").await.expect_ok();
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
