//! The client's signing path, pinned to the canonical protocol vectors.
//!
//! The login flow signs the server's challenge with EIP-191 `personal_sign`
//! through `pocketskynet-core` — the same call `Client::login` makes. These
//! tests replay every `eip191[]` vector from
//! `app/core/tests/vectors/protocol-v1.json` through that exact path, so a
//! regression in the client's signing dependency chain (a bumped `k256`, a
//! swapped hash) fails here before it fails against a server.

use pocketskynet_client::types::{LoginRequest, SendMessageRequest};
use pocketskynet_client::Wallet;
use serde_json::Value;

fn protocol_vectors() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../core/tests/vectors/protocol-v1.json"
    );
    let raw = std::fs::read_to_string(path).expect("the canonical vector file must exist");
    serde_json::from_str(&raw).expect("the vector file must be valid JSON")
}

fn eip191_vectors(doc: &Value) -> &Vec<Value> {
    doc["eip191"]
        .as_array()
        .expect("the vector file carries an eip191 array")
}

#[test]
fn signing_reproduces_every_eip191_vector() {
    let doc = protocol_vectors();
    let vectors = eip191_vectors(&doc);
    assert!(
        vectors.len() >= 5,
        "expected several vectors, found {}",
        vectors.len()
    );

    for vector in vectors {
        let name = vector["name"].as_str().unwrap_or("<unnamed>");
        let message = vector["message"].as_str().expect("a message");
        let key = vector["privateKeyHex"].as_str().expect("a private key");
        let expected_sig = vector["signatureHex"].as_str().expect("a signature");
        let expected_addr = vector["address"].as_str().expect("an address");

        // The exact path Client::login takes: Wallet::from_private_key_hex
        // then Wallet::personal_sign over the message verbatim.
        let wallet = Wallet::from_private_key_hex(key)
            .unwrap_or_else(|e| panic!("vector {name}: key must import: {e:?}"));
        assert_eq!(
            wallet.address().as_str(),
            expected_addr,
            "vector {name}: address derivation"
        );
        assert_eq!(
            wallet
                .personal_sign(message)
                .unwrap_or_else(|e| panic!("vector {name}: signing failed: {e:?}")),
            expected_sig,
            "vector {name}: signature bytes"
        );
    }
}

#[test]
fn the_digest_and_recovery_match_every_vector() {
    // Belt and braces around the same dependency chain: the digest is what a
    // wrong UTF-8 length corrupts, and recovery is what the server runs
    // against our signature at login.
    let doc = protocol_vectors();
    for vector in eip191_vectors(&doc) {
        let name = vector["name"].as_str().unwrap_or("<unnamed>");
        let message = vector["message"].as_str().unwrap();

        assert_eq!(
            message.len() as u64,
            vector["messageUtf8Len"].as_u64().expect("a byte length"),
            "vector {name}: UTF-8 byte length (bytes, not characters)"
        );
        assert_eq!(
            hex::encode(pocketskynet_core::eip191::eip191_digest(message)),
            vector["digestHex"].as_str().unwrap(),
            "vector {name}: digest"
        );
        assert_eq!(
            pocketskynet_core::recover_address(message, vector["signatureHex"].as_str().unwrap())
                .unwrap_or_else(|e| panic!("vector {name}: recovery failed: {e:?}"))
                .as_str(),
            vector["address"].as_str().unwrap(),
            "vector {name}: recovered address"
        );
    }
}

#[test]
fn a_challenge_shaped_message_signs_and_recovers() {
    // The login-challenge template from the vectors, instantiated the way the
    // server does it, signed the way the client does it — then recovered, the
    // way the server verifies it. Uses the well-known Hardhat #0 key.
    let doc = protocol_vectors();
    let template = doc["templates"]["loginChallenge"]
        .as_str()
        .expect("the loginChallenge template");

    let key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    let wallet = Wallet::from_private_key_hex(key).unwrap();
    let nonce = "ab".repeat(32);
    let message = template
        .replace("{walletAddressLowercase}", wallet.address().as_str())
        .replace("{nonce64hex}", &nonce);

    let signature = wallet.personal_sign(&message).unwrap();
    assert_eq!(
        pocketskynet_core::recover_address(&message, &signature)
            .unwrap()
            .as_str(),
        wallet.address().as_str(),
        "the server-side verification of a client-signed challenge"
    );
}

#[test]
fn request_bodies_serialize_with_camel_case_field_names() {
    // The wire is camelCase (API.md §1.4); a snake_case field would be
    // silently dropped by the server's serde into `None` and fail validation
    // with a misleading message, so the names are pinned here.
    let login = serde_json::to_value(LoginRequest {
        wallet_address: "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266".into(),
        username: Some("alice".into()),
        challenge_id: "6f1e2c30-0000-0000-0000-000000000000".into(),
        signature: "0xdeadbeef".into(),
    })
    .unwrap();
    assert_eq!(
        login["walletAddress"], "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
        "walletAddress must be camelCase"
    );
    assert_eq!(login["challengeId"], "6f1e2c30-0000-0000-0000-000000000000");
    assert_eq!(login["username"], "alice");
    assert_eq!(login["signature"], "0xdeadbeef");
    assert!(
        login.get("wallet_address").is_none() && login.get("challenge_id").is_none(),
        "no snake_case leakage"
    );

    // A repeat login omits username entirely rather than sending null — an
    // explicit value would overwrite the stored one.
    let repeat = serde_json::to_value(LoginRequest {
        wallet_address: "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266".into(),
        username: None,
        challenge_id: "id".into(),
        signature: "0x00".into(),
    })
    .unwrap();
    assert!(repeat.get("username").is_none(), "None username is omitted");

    let send = serde_json::to_value(SendMessageRequest {
        content: "hello".into(),
        msg_hash: pocketskynet_core::msg_hash_plaintext("hello"),
        is_encrypted: false,
    })
    .unwrap();
    assert_eq!(send["content"], "hello");
    assert_eq!(
        send["msgHash"], "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
        "msgHash is the lowercase-hex SHA-256 of the content"
    );
    assert_eq!(send["isEncrypted"], false);
    assert!(send.get("msg_hash").is_none() && send.get("is_encrypted").is_none());
}
