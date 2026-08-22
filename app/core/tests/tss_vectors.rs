//! The TSS indistinguishability vector (PROTOCOL.md "TSS wallets";
//! docs/CRYPTO.md §15.1; issue #85's deliverable).
//!
//! `vectors/tss-v1.json` records one real 2-of-3 DKLs23 ceremony (parties
//! {0, 2} signing). This suite verifies it **through this crate's ordinary
//! single-key code paths only** — `eip191::recover_address`, low-s parsing,
//! `LegacyTransaction::sign_with_signature` — none of which know MPC exists.
//! That is the requirement: the chain and the server must not be able to
//! tell a ceremony signature from a single-key one.
//!
//! Regenerate (a fresh, equally valid ceremony — the nonce is jointly
//! random, so the bytes will differ) with:
//! `cargo run -p pocketskynet-tss --example vector --release`.

use pocketskynet_core::{eip191, LegacyTransaction, WalletAddress};
use serde_json::Value;

fn vector() -> Value {
    serde_json::from_str(include_str!("vectors/tss-v1.json")).expect("valid vector JSON")
}

fn s(v: &Value, key: &str) -> String {
    v[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key}"))
        .to_owned()
}

#[test]
fn the_ceremony_message_signature_verifies_like_any_personal_sign() {
    let v = vector();
    let address = WalletAddress::new(&s(&v, "address")).unwrap();
    let message = s(&v, "message");
    let signature = s(&v, "messageSignature");

    // The exact calls the server's login endpoint makes.
    assert_eq!(
        eip191::recover_address(&message, &signature).unwrap(),
        address
    );
    assert!(eip191::verify_signature(&message, &signature, &address));

    // Wire shape: 0x + 65 bytes, v ∈ {27, 28} — and low-s, which
    // `recover_address` already enforced by rejecting high-s signatures.
    assert_eq!(signature.len(), 132);
    let v_byte = u8::from_str_radix(&signature[130..], 16).unwrap();
    assert!(v_byte == 27 || v_byte == 28);
}

#[test]
fn the_ceremony_transaction_signature_assembles_the_recorded_raw_bytes() {
    let v = vector();
    let tx_v = &v["tx"];
    let tx = LegacyTransaction {
        nonce: tx_v["nonce"].as_u64().unwrap().into(),
        gas_price: s(tx_v, "gasPrice").parse().unwrap(),
        gas_limit: tx_v["gasLimit"].as_u64().unwrap().into(),
        to: Some(WalletAddress::new(&s(tx_v, "to")).unwrap()),
        value: s(tx_v, "value").parse().unwrap(),
        data: vec![],
        chain_id: tx_v["chainId"].as_u64().unwrap(),
    };

    // The recorded sighash is reproducible — it has no signature in it.
    assert_eq!(
        format!("0x{}", hex::encode(tx.sighash())),
        s(tx_v, "sighash")
    );

    // Assembling from the ceremony's (r, s, recoveryId) must produce the
    // recorded raw transaction and hash, byte for byte.
    let r = hex::decode(s(tx_v, "r").trim_start_matches("0x")).unwrap();
    let s_half = hex::decode(s(tx_v, "s").trim_start_matches("0x")).unwrap();
    let mut rs = [0u8; 64];
    rs[..32].copy_from_slice(&r);
    rs[32..].copy_from_slice(&s_half);
    let recovery_id = tx_v["recoveryId"].as_u64().unwrap() as u8;

    let signed = tx.sign_with_signature(&rs, recovery_id);
    assert_eq!(signed.raw_hex(), s(tx_v, "raw"));
    assert_eq!(signed.hash_hex(), s(tx_v, "hash"));

    // And the raw transaction's signature recovers to the wallet — i.e. the
    // chain would attribute this transaction to the TSS address.
    use pocketskynet_core::k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
    let k_sig = Signature::from_slice(&rs).unwrap();
    let rec = RecoveryId::try_from(recovery_id).unwrap();
    let recovered = VerifyingKey::recover_from_prehash(&tx.sighash(), &k_sig, rec).unwrap();
    let recovered_addr =
        eip191::address_from_public_key(&pocketskynet_core::k256::PublicKey::from(&recovered));
    assert_eq!(recovered_addr.as_str(), s(&v, "address"));
}
