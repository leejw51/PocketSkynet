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
use pocketskynet_core::k256::elliptic_curve::sec1::ToEncodedPoint;
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

#[test]
fn every_private_key_import_vector_reproduces_its_address_and_public_key() {
    // Wallet::from_private_key_hex is exactly how the CLI turns --key into a
    // signer, so all three canonical imports are replayed through it.
    let doc = protocol_vectors();
    let imports = doc["wallet"]["privateKeyImports"]
        .as_array()
        .expect("privateKeyImports vectors");
    assert_eq!(imports.len(), 3, "the canonical file carries three imports");

    for vector in imports {
        let key = vector["privateKeyHex"].as_str().unwrap();
        let wallet = Wallet::from_private_key_hex(key).expect("a canonical key imports");
        assert_eq!(
            wallet.address().as_str(),
            vector["address"].as_str().unwrap(),
            "address for {key}"
        );
        assert_eq!(
            hex::encode(wallet.public_key().to_encoded_point(false).as_bytes()),
            vector["publicKeyUncompressedHex"].as_str().unwrap(),
            "uncompressed public key for {key}"
        );
        // The private key round-trips with its 0x prefix.
        assert_eq!(wallet.private_key_hex(), key.to_lowercase());
    }
}

#[test]
fn every_mnemonic_account_vector_derives_the_same_wallet() {
    // The library also accepts mnemonics (Wallet::from_mnemonic); the
    // accounts vectors pin the BIP-39/44 path m/44'/60'/0'/0/{index}.
    let doc = protocol_vectors();
    let accounts = doc["wallet"]["accounts"]
        .as_array()
        .expect("accounts vectors");
    assert!(accounts.len() >= 4);

    for vector in accounts {
        let phrase = vector["phrase"].as_str().unwrap();
        let index = vector["index"].as_u64().unwrap() as u32;
        let wallet = Wallet::from_mnemonic(phrase, index).expect("a canonical phrase parses");
        assert_eq!(
            wallet.address().as_str(),
            vector["address"].as_str().unwrap(),
            "address for {phrase:?}#{index}"
        );
        assert_eq!(
            wallet.private_key_hex(),
            vector["privateKeyHex"].as_str().unwrap(),
            "derived key for {phrase:?}#{index}"
        );
    }
}

#[test]
fn bad_private_keys_are_rejected_not_mangled() {
    // The CLI surfaces these as "invalid private key"; none may panic and
    // none may silently produce a wallet.
    let cases: Vec<String> = vec![
        String::new(),
        "0x".into(),
        "0x1234".into(),                   // too short
        format!("0x{}", "0".repeat(64)),   // zero is not a valid scalar
        format!("0x{}", "f".repeat(64)),   // >= the curve order n
        format!("0x{}", "0".repeat(63)),   // odd length
        format!("0x{}z", "0".repeat(63)),  // non-hex
        format!("0x{}00", "a".repeat(64)), // too long
    ];
    for case in cases {
        assert!(
            Wallet::from_private_key_hex(&case).is_err(),
            "should have rejected {case:?}"
        );
    }
}

#[test]
fn msg_hash_matches_every_plaintext_vector() {
    // send_message computes msgHash via msg_hash_plaintext; the vectors pin
    // both the SHA-256 (not Keccak) choice and the server-side trim.
    let doc = protocol_vectors();
    let cases = doc["msgHash"]["plaintext"]
        .as_array()
        .expect("plaintext msgHash vectors");
    assert!(cases.len() >= 3);

    for case in cases {
        let content = case["content"].as_str().unwrap();
        assert_eq!(
            pocketskynet_core::msg_hash_plaintext(content),
            case["msgHashHex"].as_str().unwrap(),
            "msgHash of {content:?}"
        );
        if let Some(trimmed) = case["trimmedTo"].as_str() {
            // Hashing the pre-trim and the trimmed string agree, because the
            // hash is defined over the trimmed content.
            assert_eq!(
                pocketskynet_core::msg_hash_plaintext(trimmed),
                case["msgHashHex"].as_str().unwrap(),
            );
        }
    }
}

#[test]
fn deterministic_usernames_match_the_vectors() {
    // The first-login fallback (Client::login with no username) sends
    // deterministic_username(address); the vectors pin the word tables.
    let doc = protocol_vectors();
    for case in doc["usernames"].as_array().expect("username vectors") {
        let address =
            pocketskynet_core::WalletAddress::new(case["address"].as_str().unwrap()).unwrap();
        assert_eq!(
            pocketskynet_core::deterministic_username(&address),
            case["username"].as_str().unwrap(),
        );
    }
}
