//! Generator and conformance test for `tests/vectors/protocol-v1.json` — the
//! cross-language porting vectors referenced by the repo-root `PROTOCOL.md`.
//!
//! Every value in the vendored file is produced by this crate's public API (or,
//! for the legacy v1 ciphertexts, by the same primitives the crate uses), never
//! typed by hand. The test rebuilds the whole document and compares it to the
//! vendored copy, so the file cannot drift from the implementation.
//!
//! To regenerate after an intentional protocol change:
//!
//! ```sh
//! UPDATE_PROTOCOL_VECTORS=1 cargo test -p pocketskynet-core --test protocol_vectors
//! ```
//!
//! The E2EE v2 message/wrap vectors live in `crypto-v2.json` (canonical,
//! synced from FruitNation) and are deliberately not duplicated here.

use std::path::PathBuf;

use aes::Aes256;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;

use pocketskynet_core::abi::{self, Arg};
use pocketskynet_core::bank;
use pocketskynet_core::chain::{
    self, erc20_balance_of_data, erc20_transfer_data, format_amount, intrinsic_gas, parse_amount,
    to_hex_quantity, LegacyTransaction,
};
use pocketskynet_core::crypto::{
    decrypt_message_v1, decrypt_message_v2, encrypt_message_v2_with_iv,
    uncompressed_public_key_hex, unwrap_room_key_v1, unwrap_room_key_v2, wrap_room_key_v2_with,
    WrappedRoomKey,
};
use pocketskynet_core::eip191;
use pocketskynet_core::hash::{
    msg_hash_emoticon, msg_hash_encrypted, msg_hash_plaintext, EmoticonAction,
};
use pocketskynet_core::k256::ecdh::diffie_hellman;
use pocketskynet_core::k256::SecretKey;
use pocketskynet_core::keys;
use pocketskynet_core::username::{deterministic_username, room_name_from_entropy};
use pocketskynet_core::wallet::{derivation_path, parse_mnemonic, seed_from_mnemonic, Wallet};
use pocketskynet_core::{ClientMessage, MessageId, ResyncReason, RoomId, ServerEvent, Target};

type Aes256CbcEnc = cbc::Encryptor<Aes256>;
type HmacSha256 = Hmac<Sha256>;

// ---------------------------------------------------------------- fixtures --

/// Hardhat account #0 — the canonical worked-example key of docs/CRYPTO.md.
const WALLET_A_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
/// The second worked-example key of docs/CRYPTO.md.
const WALLET_B_KEY: &str = "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
/// Hardhat account #1 — the rotation scenario's removed member.
const WALLET_C_KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const SALT: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
/// Per-account salts for the rotation scenario's other members — pattern
/// constants, like every other fixed input in this file.
const SALT_B: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const SALT_C: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

const ABANDON: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
const TEST_JUNK: &str = "test test test test test test test test test test test junk";

/// K1 of the crypto vectors: sha256("test").
const K1_HEX: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
const K2_HEX: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

fn key32(hex_str: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    hex::decode_to_slice(hex_str, &mut out).expect("32-byte hex");
    out
}

fn iv16(hex_str: &str) -> [u8; 16] {
    let mut out = [0u8; 16];
    hex::decode_to_slice(hex_str, &mut out).expect("16-byte hex");
    out
}

fn secret(hex_str: &str) -> SecretKey {
    SecretKey::from_slice(&hex::decode(hex_str.trim_start_matches("0x")).expect("hex"))
        .expect("valid scalar")
}

fn wallet(key: &str) -> Wallet {
    Wallet::from_private_key_hex(key).expect("valid key")
}

// -------------------------------------------------- legacy v1 construction --

/// v1 message encryption, reproduced from docs/CRYPTO.md §8.1: AES key is the
/// 32 raw key bytes, HMAC key is the 64 ASCII bytes of the lowercase hex
/// string, MAC input is the base64 ciphertext alone.
fn encrypt_message_v1(key: &[u8; 32], iv: &[u8; 16], plaintext: &str) -> (String, String) {
    let ct = Aes256CbcEnc::new(key.into(), iv.into())
        .encrypt_padded_vec_mut::<Pkcs7>(plaintext.as_bytes());
    let ct_b64 = BASE64.encode(&ct);
    let hmac = hmac_hex(hex::encode(key).as_bytes(), ct_b64.as_bytes());
    (ct_b64, hmac)
}

/// v1 room-key wrap, per docs/CRYPTO.md §8.2: the raw ECDH X coordinate is the
/// AES key directly (no KDF); the HMAC key is its ASCII hex string.
fn wrap_room_key_v1(
    room_key: &[u8; 32],
    recipient: &SecretKey,
    ephemeral: &SecretKey,
    iv: &[u8; 16],
) -> WrappedRoomKey {
    let shared_x = ecdh_shared_x(ephemeral, &recipient.public_key());
    let plaintext = hex::encode(room_key);
    let ct = Aes256CbcEnc::new((&shared_x).into(), iv.into())
        .encrypt_padded_vec_mut::<Pkcs7>(plaintext.as_bytes());
    let ct_b64 = BASE64.encode(&ct);
    let hmac = hmac_hex(hex::encode(shared_x).as_bytes(), ct_b64.as_bytes());
    WrappedRoomKey {
        encrypted_symmetric_key: ct_b64,
        ephemeral_public_key: uncompressed_public_key_hex(&ephemeral.public_key()),
        encryption_iv: hex::encode(iv),
        hmac,
    }
}

fn hmac_hex(key: &[u8], msg: &[u8]) -> String {
    let mut mac = <HmacSha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(msg);
    hex::encode(mac.finalize().into_bytes())
}

fn ecdh_shared_x(sk: &SecretKey, pk: &pocketskynet_core::k256::PublicKey) -> [u8; 32] {
    let shared = diffie_hellman(sk.to_nonzero_scalar(), pk.as_affine());
    (*shared.raw_secret_bytes()).into()
}

// --------------------------------------------------------- local RLP mirror --

/// RLP byte-string encoding, mirrored from the (private) implementation in
/// `chain.rs` so the primitive vectors can be emitted. The transaction vectors
/// pin the crate's own RLP; these pin the spec examples for porters.
fn rlp_bytes(bytes: &[u8]) -> Vec<u8> {
    match bytes {
        [b] if *b < 0x80 => vec![*b],
        _ if bytes.len() <= 55 => {
            let mut out = vec![0x80 + bytes.len() as u8];
            out.extend_from_slice(bytes);
            out
        }
        _ => {
            let len = bytes.len();
            let len_be: Vec<u8> = len
                .to_be_bytes()
                .iter()
                .copied()
                .skip_while(|&b| b == 0)
                .collect();
            let mut out = vec![0xb7 + len_be.len() as u8];
            out.extend_from_slice(&len_be);
            out.extend_from_slice(bytes);
            out
        }
    }
}

fn rlp_uint(value: u128) -> Vec<u8> {
    let be: Vec<u8> = value
        .to_be_bytes()
        .iter()
        .copied()
        .skip_while(|&b| b == 0)
        .collect();
    rlp_bytes(&be)
}

// ------------------------------------------------------------ the document --

fn eip191_vector(name: &str, key: &str, message: &str) -> Value {
    let w = wallet(key);
    let signature = w.personal_sign(message).unwrap();
    assert_eq!(
        eip191::recover_address(message, &signature).unwrap(),
        *w.address(),
        "[{name}] signature must recover to the signer"
    );
    json!({
        "name": name,
        "privateKeyHex": key,
        "address": w.address().as_str(),
        "message": message,
        "messageUtf8Len": message.len(),
        "digestHex": hex::encode(eip191::eip191_digest(message)),
        "signatureHex": signature,
    })
}

fn account_vector(phrase: &str, index: u32) -> Value {
    let w = Wallet::from_mnemonic(phrase, index).unwrap();
    json!({
        "phrase": phrase,
        "index": index,
        "path": derivation_path(index),
        "privateKeyHex": w.private_key_hex(),
        "address": w.address().as_str(),
        "addressChecksummed": w.address().to_checksummed(),
    })
}

fn tx_vector(name: &str, tx: &LegacyTransaction, key: &SecretKey) -> Value {
    let signed = tx.sign(key).unwrap();
    json!({
        "name": name,
        "chainId": tx.chain_id,
        "nonce": tx.nonce.to_string(),
        "gasPriceWei": tx.gas_price.to_string(),
        "gasLimit": tx.gas_limit.to_string(),
        "to": tx.to.as_ref().map(|a| a.as_str().to_owned()),
        "valueWei": tx.value.to_string(),
        "dataHex": hex::encode(&tx.data),
        "privateKeyHex": format!("0x{}", hex::encode(key.to_bytes())),
        "sighashHex": hex::encode(tx.sighash()),
        "rawHex": signed.raw_hex(),
        "txHashHex": signed.hash_hex(),
    })
}

fn build() -> Value {
    let wallet_a = wallet(WALLET_A_KEY);
    let wallet_b = wallet(WALLET_B_KEY);
    let k1 = key32(K1_HEX);
    let scalar_one = format!("0x{}01", "00".repeat(31));

    // --- EIP-191 messages -------------------------------------------------
    let legacy_msg_a = keys::build_legacy_encryption_message(wallet_a.address());
    let legacy_msg_b = keys::build_legacy_encryption_message(wallet_b.address());
    let salted_msg_a = keys::build_salted_encryption_message(wallet_a.address(), SALT).unwrap();
    let salted_msg_b = keys::build_salted_encryption_message(wallet_b.address(), SALT).unwrap();
    let challenge_nonce = K2_HEX; // any 64 hex chars; fixed so the vector is stable
    let challenge = format!(
        "Welcome to FruitNation!\n\nClick to sign in and accept the FruitNation Terms of Service.\n\nThis request will not trigger a blockchain transaction or cost any gas fees.\n\nWallet address:\n{}\n\nNonce:\n{}",
        wallet_a.address(),
        challenge_nonce
    );

    // --- encryption key derivation ---------------------------------------
    let enc_v2 = |w: &Wallet, salt: &str| {
        let message = keys::build_salted_encryption_message(w.address(), salt).unwrap();
        let signature = w.personal_sign(&message).unwrap();
        let kp = keys::derive_encryption_keys_from_signature(&signature).unwrap();
        json!({
            "walletPrivateKeyHex": w.private_key_hex(),
            "address": w.address().as_str(),
            "saltHex": salt,
            "message": message,
            "signatureHex": signature,
            "encryptionPrivateKeyHex": kp.private_key_hex(),
            "encryptionPublicKeyHex": kp.public_key_hex(),
        })
    };
    let enc_v1 = |w: &Wallet| {
        let message = keys::build_legacy_encryption_message(w.address());
        let signature = w.personal_sign(&message).unwrap();
        let kp = keys::derive_encryption_keys_from_signature(&signature).unwrap();
        json!({
            "walletPrivateKeyHex": w.private_key_hex(),
            "address": w.address().as_str(),
            "message": message,
            "signatureHex": signature,
            "encryptionPrivateKeyHex": kp.private_key_hex(),
            "encryptionPublicKeyHex": kp.public_key_hex(),
        })
    };
    let binding = |w: &Wallet, salt: &str| {
        let kp = keys::derive_encryption_keys_v2(w, salt).unwrap();
        let message = keys::build_key_binding_message(w.address(), kp.public_key_hex());
        let signature = keys::sign_key_binding(w, kp.public_key_hex()).unwrap();
        keys::verify_key_binding(w.address(), Some(kp.public_key_hex()), Some(&signature))
            .expect("binding must verify");
        json!({
            "address": w.address().as_str(),
            "encryptionPublicKeyHex": kp.public_key_hex(),
            "message": message,
            "signatureHex": signature,
        })
    };

    // --- legacy v1 E2EE ---------------------------------------------------
    let v1_message = |key_hex: &str, iv_hex: &str, plaintext: &str| {
        let key = key32(key_hex);
        let (ct_b64, hmac) = encrypt_message_v1(&key, &iv16(iv_hex), plaintext);
        assert_eq!(
            decrypt_message_v1(&ct_b64, iv_hex, &hmac, &key).unwrap(),
            plaintext,
            "v1 message vector must round-trip through the crate"
        );
        // Pin the §8.1 trap: an HMAC keyed by the raw 32 bytes must NOT verify.
        let wrong = hmac_hex(&key, ct_b64.as_bytes());
        assert!(decrypt_message_v1(&ct_b64, iv_hex, &wrong, &key).is_err());
        json!({
            "symmetricKeyHex": key_hex,
            "ivHex": iv_hex,
            "plaintextUtf8": plaintext,
            "ciphertextBase64": ct_b64,
            "hmacHex": hmac,
            "rejectedHmacRawKeyHex": wrong,
        })
    };
    let recipient = secret(WALLET_B_KEY); // 0123…ef, same scalar as the crypto-v2 wrap
    let ephemeral = secret(&"22".repeat(32));
    let v1_wrap = {
        let wrap = wrap_room_key_v1(
            &k1,
            &recipient,
            &ephemeral,
            &iv16("505152535455565758595a5b5c5d5e5f"),
        );
        assert_eq!(
            unwrap_room_key_v1(&wrap, &recipient).unwrap(),
            k1,
            "v1 wrap vector must round-trip through the crate"
        );
        let shared_x = ecdh_shared_x(&ephemeral, &recipient.public_key());
        json!({
            "roomSymmetricKeyHex": K1_HEX,
            "recipientPrivateKeyHex": hex::encode(recipient.to_bytes()),
            "recipientPublicKeyHex": uncompressed_public_key_hex(&recipient.public_key()),
            "ephemeralPrivateKeyHex": hex::encode(ephemeral.to_bytes()),
            "ephemeralPublicKeyHex": wrap.ephemeral_public_key,
            "sharedXHex": hex::encode(shared_x),
            "ivHex": wrap.encryption_iv,
            "encryptedSymmetricKeyBase64": wrap.encrypted_symmetric_key,
            "hmacHex": wrap.hmac,
        })
    };

    // --- msgHash ----------------------------------------------------------
    let emoticon = |code: &str, action: EmoticonAction, action_str: &str| {
        let id = MessageId::new("msg_1749652739650_304e0eaf").unwrap();
        let ts: i64 = 1_749_652_739_650;
        let hash = msg_hash_emoticon(&id, code, action, wallet_a.address(), ts);
        json!({
            "messageId": id.as_str(),
            "emoticonCode": code,
            "action": action_str,
            "senderAddress": wallet_a.address().as_str(),
            "timestampMs": ts,
            "eventData": format!("{}:{}:{}:{}:{}", id, code, action_str, wallet_a.address(), ts),
            "msgHashHex": hash,
        })
    };

    // --- amounts / gas ----------------------------------------------------
    let amount = |input: &str, decimals: u8| {
        json!({
            "input": input,
            "decimals": decimals,
            "baseUnits": parse_amount(input, decimals).unwrap().to_string(),
        })
    };
    let formatted = |units: u128, decimals: u8| {
        json!({
            "baseUnits": units.to_string(),
            "decimals": decimals,
            "formatted": format_amount(units, decimals),
        })
    };
    let gas = |data: &[u8]| json!({ "dataHex": hex::encode(data), "gas": intrinsic_gas(data) });

    // --- ABI --------------------------------------------------------------
    let selectors: Vec<Value> = [
        "transfer(address,uint256)",
        "balanceOf(address)",
        "approve(address,uint256)",
        "allowance(address,address)",
        "decimals()",
        "symbol()",
        "name()",
        "totalSupply()",
        "owner()",
        "greet()",
        "setGreeting(string)",
        "deposit()",
        "withdraw(uint256)",
        "getAmountsOut(uint256,address[])",
        "swapExactETHForTokens(uint256,address[],address,uint256)",
        "swapExactTokensForETH(uint256,uint256,address[],address,uint256)",
        "swapExactTokensForTokens(uint256,uint256,address[],address,uint256)",
    ]
    .iter()
    .map(|sig| json!({ "signature": sig, "selectorHex": hex::encode(abi::selector(sig)) }))
    .collect();

    let addr = |s: &str| pocketskynet_core::WalletAddress::new(s).unwrap();
    let to_3535 = addr("0x3535353535353535353535353535353535353535");
    let wcro = addr(bank::WCRO_CRONOS_MAINNET);
    let usdc = addr(chain::USDC_CRONOS_MAINNET);
    let router = addr(bank::VVS_ROUTER_CRONOS_MAINNET);
    let path = [wcro.clone(), usdc.clone()];
    let deadline: u64 = 1_750_000_000;

    let call = |name: &str, data: Vec<u8>| json!({ "name": name, "dataHex": hex::encode(data) });
    let abi_calls = vec![
        call("erc20Transfer(to=0x3535…, amount=1000000)", erc20_transfer_data(&to_3535, 1_000_000)),
        call(
            &format!("erc20BalanceOf(owner={})", wallet_a.address()),
            erc20_balance_of_data(wallet_a.address()),
        ),
        call(
            &format!("erc20Approve(spender={router}, amount=10^18)"),
            bank::erc20_approve_data(&router, 1_000_000_000_000_000_000),
        ),
        call(
            &format!("erc20Allowance(owner={}, spender={router})", wallet_a.address()),
            bank::erc20_allowance_data(wallet_a.address(), &router),
        ),
        call(
            "getAmountsOut(amountIn=1000000, path=[WCRO,USDC])",
            bank::get_amounts_out_data(1_000_000, &path),
        ),
        call(
            &format!("swapExactETHForTokens(min=9950, path=[WCRO,USDC], to={}, deadline={deadline})", wallet_b.address()),
            bank::swap_exact_eth_for_tokens_data(9_950, &path, wallet_b.address(), deadline),
        ),
        call(
            &format!("swapExactTokensForETH(in=1000000, min=9950, path=[USDC,WCRO], to={}, deadline={deadline})", wallet_b.address()),
            bank::swap_exact_tokens_for_eth_data(
                1_000_000,
                9_950,
                &[usdc.clone(), wcro.clone()],
                wallet_b.address(),
                deadline,
            ),
        ),
        call(
            &format!("swapExactTokensForTokens(in=1000000, min=9950, path=[USDC,WCRO], to={}, deadline={deadline})", wallet_b.address()),
            bank::swap_exact_tokens_for_tokens_data(
                1_000_000,
                9_950,
                &[usdc, wcro],
                wallet_b.address(),
                deadline,
            ),
        ),
        call("greet()", bank::greet_data()),
        call("owner()", bank::greeter_owner_data()),
        call("setGreeting(\"hello\")", bank::set_greeting_data("hello")),
        call("setGreeting(\"안녕하세요\")", bank::set_greeting_data("안녕하세요")),
        call(
            "erc20ConstructorArgs(\"My Token\", \"MTK\", 18, 1000·10^18)",
            abi::encode_args(&[
                Arg::Str("My Token".into()),
                Arg::Str("MTK".into()),
                Arg::Uint(18),
                Arg::Uint(1_000_000_000_000_000_000_000),
            ]),
        ),
        call(
            "greeterConstructorArgs(\"hello\")",
            abi::encode_args(&[Arg::Str("hello".into())]),
        ),
    ];

    // --- RLP primitives ---------------------------------------------------
    let rlp_vecs = vec![
        json!({ "kind": "bytes", "inputHex": "", "encodedHex": hex::encode(rlp_bytes(b"")) }),
        json!({ "kind": "bytes", "inputHex": "00", "encodedHex": hex::encode(rlp_bytes(&[0x00])) }),
        json!({ "kind": "bytes", "inputHex": "7f", "encodedHex": hex::encode(rlp_bytes(&[0x7f])) }),
        json!({ "kind": "bytes", "inputHex": "80", "encodedHex": hex::encode(rlp_bytes(&[0x80])) }),
        json!({ "kind": "bytes", "inputHex": hex::encode(b"dog"), "encodedHex": hex::encode(rlp_bytes(b"dog")) }),
        json!({ "kind": "bytes", "inputHex": "61".repeat(56), "encodedHex": hex::encode(rlp_bytes(&[b'a'; 56])) }),
        json!({ "kind": "uint", "value": "0", "encodedHex": hex::encode(rlp_uint(0)) }),
        json!({ "kind": "uint", "value": "15", "encodedHex": hex::encode(rlp_uint(15)) }),
        json!({ "kind": "uint", "value": "1024", "encodedHex": hex::encode(rlp_uint(1024)) }),
        json!({ "kind": "emptyList", "encodedHex": "c0" }),
    ];

    // --- transactions -----------------------------------------------------
    let eip155_key = SecretKey::from_slice(&[0x46u8; 32]).unwrap();
    let eip155_tx = LegacyTransaction {
        nonce: 9,
        gas_price: 20_000_000_000,
        gas_limit: 21_000,
        to: Some(to_3535.clone()),
        value: 1_000_000_000_000_000_000,
        data: vec![],
        chain_id: 1,
    };
    let cronos_tx = LegacyTransaction {
        chain_id: 25,
        ..eip155_tx.clone()
    };
    let usdc_transfer = LegacyTransaction {
        nonce: 7,
        gas_price: 5_000_000_000_000,
        gas_limit: 100_000,
        to: Some(addr(chain::USDC_CRONOS_MAINNET)),
        value: 0,
        data: erc20_transfer_data(wallet_b.address(), 1_000_000),
        chain_id: 338,
    };

    // --- realtime events --------------------------------------------------
    let room = RoomId::new("room_1749652739650_304e0eaf").unwrap();
    let server_events: Vec<Value> = [
        ServerEvent::NewMessage { room_id: room.clone(), msg_serial: 42 },
        ServerEvent::RoomsUpdated,
        ServerEvent::MemberRemoved { room_id: room.clone() },
        ServerEvent::InvitationReceived { room_id: room.clone() },
        ServerEvent::Typing { room_id: room.clone(), from: wallet_a.address().clone() },
        ServerEvent::ResyncRequired { reason: ResyncReason::Lagged, from_seq: 10, to_seq: 99 },
        ServerEvent::SessionExpired { reason: "token expired".into() },
        ServerEvent::Shout { shout_id: "shout_1".into() },
        ServerEvent::Pong,
    ]
    .iter()
    .map(|ev| json!({ "name": ev.name(), "replayable": ev.is_replayable(), "json": serde_json::to_value(ev).unwrap() }))
    .collect();
    let client_messages: Vec<Value> = [
        ClientMessage::Ping,
        ClientMessage::Typing {
            room_id: room.clone(),
        },
    ]
    .iter()
    .map(|m| serde_json::to_value(m).unwrap())
    .collect();
    let targets: Vec<Value> = [
        Target::Room {
            room_id: room.clone(),
        },
        Target::User {
            wallet: wallet_a.address().clone(),
        },
        Target::RoomExcept {
            room_id: room,
            except: wallet_a.address().clone(),
        },
        Target::All,
    ]
    .iter()
    .map(|t| serde_json::to_value(t).unwrap())
    .collect();

    // --- E2EE group flow + key rotation scenario --------------------------
    //
    // The full lifecycle a port must reproduce: three members derive their v2
    // encryption keys and bindings, epoch 1 wraps the room key to everyone and
    // carries two messages, then carol is removed, epoch 2 re-keys for the
    // survivors only, and a stranded epoch-1 v1 wrap is healed to v2. Every
    // wrap and message below is asserted to round-trip through the crate, and
    // the negative expectations (carol locked out of epoch 2, no cross-epoch
    // decryption) are asserted here too — a port should mirror them as tests.
    let scenario_room = "room-rotation-0001";
    let wallet_c = wallet(WALLET_C_KEY);
    let k2 = key32(K2_HEX);
    let alice_kp = keys::derive_encryption_keys_v2(&wallet_a, SALT).unwrap();
    let bob_kp = keys::derive_encryption_keys_v2(&wallet_b, SALT_B).unwrap();
    let carol_kp = keys::derive_encryption_keys_v2(&wallet_c, SALT_C).unwrap();

    let member_json = |name: &str, w: &Wallet, salt: &str, kp: &keys::EncryptionKeypair| {
        json!({
            "name": name,
            "walletPrivateKeyHex": w.private_key_hex(),
            "address": w.address().as_str(),
            "saltHex": salt,
            "encryptionPrivateKeyHex": kp.private_key_hex(),
            "encryptionPublicKeyHex": kp.public_key_hex(),
            "keyBindingSignatureHex": keys::sign_key_binding(w, kp.public_key_hex()).unwrap(),
        })
    };

    let scenario_wrap = |room_key: &[u8; 32],
                         kp: &keys::EncryptionKeypair,
                         recipient_name: &str,
                         eph_hex: &str,
                         iv_hex: &str,
                         key_version: u32| {
        let wrap = wrap_room_key_v2_with(
            room_key,
            &kp.public_key(),
            scenario_room,
            &secret(eph_hex),
            &iv16(iv_hex),
        );
        assert_eq!(
            unwrap_room_key_v2(&wrap, kp.secret_key(), scenario_room).unwrap(),
            *room_key,
            "scenario wrap for {recipient_name} (epoch {key_version}) must unwrap"
        );
        (
            wrap.clone(),
            json!({
                "recipient": recipient_name,
                "keyVersion": key_version,
                "ephemeralPrivateKeyHex": eph_hex,
                "ephemeralPublicKey": wrap.ephemeral_public_key,
                "encryptionIV": wrap.encryption_iv,
                "encryptedSymmetricKey": wrap.encrypted_symmetric_key,
                "hmac": wrap.hmac,
            }),
        )
    };

    let scenario_msg =
        |room_key: &[u8; 32], sender: &str, plaintext: &str, iv_hex: &str, key_version: u32| {
            let enc = encrypt_message_v2_with_iv(plaintext, room_key, scenario_room, &iv16(iv_hex))
                .unwrap();
            assert_eq!(
                decrypt_message_v2(&enc.content, &enc.iv, &enc.hmac, room_key, scenario_room)
                    .unwrap(),
                plaintext,
                "scenario message (epoch {key_version}) must decrypt"
            );
            (
                enc.clone(),
                json!({
                    "sender": sender,
                    "keyVersion": key_version,
                    "encVer": 2,
                    "isEncrypted": true,
                    "plaintextUtf8": plaintext,
                    "content": enc.content,
                    "iv": enc.iv,
                    "hmac": enc.hmac,
                    "msgHash": msg_hash_encrypted(&enc.content),
                }),
            )
        };

    // Epoch 1: everyone gets a wrap of K1; two messages are sent under it.
    let (_, e1_alice) = scenario_wrap(
        &k1,
        &alice_kp,
        "alice",
        &"11".repeat(32),
        "606162636465666768696a6b6c6d6e6f",
        1,
    );
    let (_, e1_bob) = scenario_wrap(
        &k1,
        &bob_kp,
        "bob",
        &"22".repeat(32),
        "707172737475767778797a7b7c7d7e7f",
        1,
    );
    let (e1_carol_wrap, e1_carol) = scenario_wrap(
        &k1,
        &carol_kp,
        "carol",
        &"33".repeat(32),
        "808182838485868788898a8b8c8d8e8f",
        1,
    );
    let (e1_msg1_enc, e1_msg1) = scenario_msg(
        &k1,
        "alice",
        "hello room — epoch one",
        "d0d1d2d3d4d5d6d7d8d9dadbdcdddedf",
        1,
    );
    let (_, e1_msg2) = scenario_msg(
        &k1,
        "bob",
        "한글 second message 🍓",
        "e0e1e2e3e4e5e6e7e8e9eaebecedeeef",
        1,
    );

    // Rotation: carol removed → epoch 2 wraps K2 to the survivors only.
    let (e2_alice_wrap, e2_alice) = scenario_wrap(
        &k2,
        &alice_kp,
        "alice",
        &"44".repeat(32),
        "909192939495969798999a9b9c9d9e9f",
        2,
    );
    let (_, e2_bob) = scenario_wrap(
        &k2,
        &bob_kp,
        "bob",
        &"55".repeat(32),
        "a0a1a2a3a4a5a6a7a8a9aaabacadaeaf",
        2,
    );
    let (e2_msg_enc, e2_msg) = scenario_msg(
        &k2,
        "alice",
        "fresh epoch after rotation",
        "f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff",
        2,
    );

    // Negative expectations — asserted here, to be mirrored by a port's tests.
    // Carol's key must not open an epoch-2 wrap (hers or a survivor's)…
    assert!(
        unwrap_room_key_v2(&e2_alice_wrap, carol_kp.secret_key(), scenario_room).is_err(),
        "removed member must not unwrap an epoch-2 wrap"
    );
    // …epoch-1 wraps must not open with the wrong member's key…
    assert!(
        unwrap_room_key_v2(&e1_carol_wrap, alice_kp.secret_key(), scenario_room).is_err(),
        "a wrap targets exactly one recipient"
    );
    // …and messages must not decrypt across epochs in either direction.
    assert!(
        decrypt_message_v2(
            &e2_msg_enc.content,
            &e2_msg_enc.iv,
            &e2_msg_enc.hmac,
            &k1,
            scenario_room
        )
        .is_err(),
        "epoch-2 message must not decrypt under the epoch-1 key"
    );
    assert!(
        decrypt_message_v2(
            &e1_msg1_enc.content,
            &e1_msg1_enc.iv,
            &e1_msg1_enc.hmac,
            &k2,
            scenario_room
        )
        .is_err(),
        "epoch-1 message must not decrypt under the epoch-2 key"
    );

    // Healing: an old v1 wrap of the epoch-1 key targets alice's LEGACY
    // (unsalted) keypair; the client unwraps it with the legacy key and
    // immediately re-wraps to her v2 key at the same keyVersion.
    let alice_legacy = keys::derive_legacy_encryption_keys(&wallet_a).unwrap();
    let legacy_wrap = wrap_room_key_v1(
        &k1,
        alice_legacy.secret_key(),
        &secret(&"77".repeat(32)),
        &iv16("c0c1c2c3c4c5c6c7c8c9cacbcccdcecf"),
    );
    assert_eq!(
        unwrap_room_key_v1(&legacy_wrap, alice_legacy.secret_key()).unwrap(),
        k1,
        "legacy wrap must unwrap with the legacy key"
    );
    let (_, healed) = scenario_wrap(
        &k1,
        &alice_kp,
        "alice",
        &"66".repeat(32),
        "b0b1b2b3b4b5b6b7b8b9babbbcbdbebf",
        1,
    );

    let rotation_scenario = json!({
        "roomId": scenario_room,
        "members": [
            member_json("alice", &wallet_a, SALT, &alice_kp),
            member_json("bob", &wallet_b, SALT_B, &bob_kp),
            member_json("carol", &wallet_c, SALT_C, &carol_kp),
        ],
        "epochs": [
            {
                "keyVersion": 1,
                "roomKeyHex": K1_HEX,
                "wraps": [e1_alice, e1_bob, e1_carol],
                "messages": [e1_msg1, e1_msg2],
            },
            {
                "keyVersion": 2,
                "roomKeyHex": K2_HEX,
                "rotationReason": "carol removed; server set keyRotationPending and any member re-keys (§11)",
                "wraps": [e2_alice, e2_bob],
                "messages": [e2_msg],
            },
        ],
        "healing": {
            "note": "epoch-1 key stranded in a v1 wrap to alice's legacy (unsalted) keypair; unwrap with the legacy key, re-wrap to her v2 key at the SAME keyVersion",
            "legacyKeypair": {
                "encryptionPrivateKeyHex": alice_legacy.private_key_hex(),
                "encryptionPublicKeyHex": alice_legacy.public_key_hex(),
            },
            "v1Wrap": {
                "keyVersion": 1,
                "ephemeralPrivateKeyHex": "77".repeat(32),
                "ephemeralPublicKey": legacy_wrap.ephemeral_public_key,
                "encryptionIV": legacy_wrap.encryption_iv,
                "encryptedSymmetricKey": legacy_wrap.encrypted_symmetric_key,
                "hmac": legacy_wrap.hmac,
            },
            "healedV2Wrap": healed,
        },
        "expectations": [
            "every wrap unwraps to its epoch's roomKeyHex with that recipient's encryption private key, and with no other key",
            "carol's encryption private key must fail to unwrap every keyVersion-2 wrap (forward secrecy on removal)",
            "keyVersion-1 messages decrypt only under the keyVersion-1 room key; keyVersion-2 only under keyVersion-2",
            "encrypting always uses the highest epoch held and stamps its keyVersion; decrypting selects the key by the message's keyVersion (missing = 1)",
            "before wrapping, each recipient's keyBindingSignatureHex must verify against templates.keyBinding rebuilt from their address and public key",
            "the v1 wrap unwraps only via the LEGACY keypair (raw sharedX as AES key, ASCII-hex HMAC key); the healed wrap must unwrap via the v2 keypair",
        ],
    });

    // Pre-built lists: `json!` cannot parse method calls on array literals.
    let bip39_seeds: Vec<Value> = [ABANDON, TEST_JUNK]
        .iter()
        .map(|phrase| {
            json!({
                "phrase": phrase,
                "passphrase": "",
                "seedHex": hex::encode(seed_from_mnemonic(&parse_mnemonic(phrase).unwrap())),
            })
        })
        .collect();
    let private_key_imports: Vec<Value> = [WALLET_A_KEY, WALLET_B_KEY, scalar_one.as_str()]
        .iter()
        .map(|key| {
            let w = wallet(key);
            json!({
                "privateKeyHex": key,
                "address": w.address().as_str(),
                "addressChecksummed": w.address().to_checksummed(),
                "publicKeyUncompressedHex": uncompressed_public_key_hex(&w.public_key()),
            })
        })
        .collect();
    let eip55_vecs: Vec<Value> = [
        "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed",
        "0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359",
        "0xdbF03B407c01E7cD3CBea99509d93f8DDDC8C6FB",
        "0xD1220A0cf47c7B9Be7A2E6BA89F429762e7b9aDb",
        "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
        "0xFCAd0B19bB29D4674531d6f115237E16AfCE377c",
    ]
    .iter()
    .map(|checksummed| json!({ "lower": checksummed.to_lowercase(), "checksummed": checksummed }))
    .collect();
    let encrypted_hashes: Vec<Value> = [
        "3nP4XMnquk7mpaDFxNxnZA==",
        "jLykKspwGTDA6abyS7HrIsSbcL6kRO4RixQVgE+VlJk=",
        "AeHMd1L87BW8NOlkHslfgN7D7U3yQPnhvrm9X20aeh8=",
    ]
    .iter()
    .map(|ct| json!({ "ciphertextBase64": ct, "msgHashHex": msg_hash_encrypted(ct) }))
    .collect();
    let username_vecs: Vec<Value> = [
        "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
        "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
        "0x9858effd232b4033e47d90003d41ec34ecaeda94",
        "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf",
    ]
    .iter()
    .map(|a| {
        let address = addr(a);
        json!({ "address": address.as_str(), "username": deterministic_username(&address) })
    })
    .collect();
    let room_name_vecs: Vec<Value> = [
        hex::encode([0u8; 16]),
        hex::encode([1u8; 16]),
        "deadbeef".to_string(),
    ]
    .iter()
    .map(|entropy_hex| {
        json!({
            "entropyHex": entropy_hex,
            "name": room_name_from_entropy(&hex::decode(entropy_hex).unwrap()),
        })
    })
    .collect();

    json!({
        "formatVersion": 1,
        "description": "PocketSkynet cross-language protocol vectors. Generated by core/tests/protocol_vectors.rs — never edit by hand. Spec: PROTOCOL.md (repo root). E2EE v2 message/room-key vectors live in crypto-v2.json.",
        "templates": {
            "loginChallenge": "Welcome to FruitNation!\n\nClick to sign in and accept the FruitNation Terms of Service.\n\nThis request will not trigger a blockchain transaction or cost any gas fees.\n\nWallet address:\n{walletAddressLowercase}\n\nNonce:\n{nonce64hex}",
            "encryptionKeyV2": "FruitNation Encryption Key Derivation v2\n\nAddress: {walletAddressLowercase}\nSalt: {saltHex}\nPurpose: End-to-end encryption only",
            "encryptionKeyV1Legacy": "FruitNation Encryption Key Derivation\n\nAddress: {walletAddressLowercase}\nPurpose: End-to-end encryption only",
            "keyBinding": "FruitNation Public Key Binding\n\nAddress: {walletAddressLowercase}\nEncryption Public Key: {encPubHex}",
            "messageMacInput": "FNv2|message|{roomId}|{ivHex}|{ciphertextBase64}",
            "roomKeyMacInput": "FNv2|roomkey|{roomId}|{ephemeralPublicKeyHex}|{ivHex}|{ciphertextBase64}",
            "emoticonEventData": "{messageId}:{emoticonCode}:{add|remove}:{senderWalletAddress}:{timestampMs}",
            "subkeyLabels": {
                "messageEnc": pocketskynet_core::crypto::MSG_ENC_LABEL,
                "messageMac": pocketskynet_core::crypto::MSG_MAC_LABEL,
                "roomKeyEnc": pocketskynet_core::crypto::WRAP_ENC_LABEL,
                "roomKeyMac": pocketskynet_core::crypto::WRAP_MAC_LABEL,
            },
        },
        "wallet": {
            "bip39Seeds": bip39_seeds,
            "accounts": [
                account_vector(ABANDON, 0),
                account_vector(ABANDON, 1),
                account_vector(TEST_JUNK, 0),
                account_vector(TEST_JUNK, 1),
            ],
            "privateKeyImports": private_key_imports,
            "eip55": eip55_vecs,
        },
        "eip191": [
            eip191_vector("simple", WALLET_A_KEY, "hello world"),
            eip191_vector("unicode-length-is-bytes", WALLET_A_KEY, "🍓 strawberry"),
            eip191_vector("legacy-derivation-a", WALLET_A_KEY, &legacy_msg_a),
            eip191_vector("legacy-derivation-b", WALLET_B_KEY, &legacy_msg_b),
            eip191_vector("salted-derivation-a", WALLET_A_KEY, &salted_msg_a),
            eip191_vector("salted-derivation-b", WALLET_B_KEY, &salted_msg_b),
            eip191_vector("login-challenge", WALLET_A_KEY, &challenge),
        ],
        "encryptionKeyDerivation": {
            "v2": [enc_v2(&wallet_a, SALT), enc_v2(&wallet_b, SALT)],
            "v1Legacy": [enc_v1(&wallet_a), enc_v1(&wallet_b)],
        },
        "keyBindings": [binding(&wallet_a, SALT), binding(&wallet_b, SALT)],
        "e2eeRotationScenario": rotation_scenario,
        "ecdh": [{
            "privateKeyHex": hex::encode(recipient.to_bytes()),
            "peerPublicKeyHex": uncompressed_public_key_hex(&ephemeral.public_key()),
            "sharedXHex": hex::encode(ecdh_shared_x(&recipient, &ephemeral.public_key())),
        }],
        "legacyV1": {
            "note": "Decrypt-only. AES key = raw 32 key bytes; HMAC key = 64 ASCII bytes of the lowercase hex string; MAC input = base64 ciphertext alone. Never write encVer=1.",
            "messages": [
                v1_message(K1_HEX, "000102030405060708090a0b0c0d0e0f", "attack at dawn"),
                v1_message(K2_HEX, "404142434445464748494a4b4c4d4e4f", "legacy unicode 한글 🍊"),
            ],
            "roomKeyWraps": [v1_wrap],
        },
        "msgHash": {
            "plaintext": [
                json!({ "content": "abc", "msgHashHex": msg_hash_plaintext("abc") }),
                json!({ "content": "  hello \n", "trimmedTo": "hello", "msgHashHex": msg_hash_plaintext("  hello \n") }),
                json!({ "content": "한글 메시지 🍓🍊", "msgHashHex": msg_hash_plaintext("한글 메시지 🍓🍊") }),
            ],
            "encrypted": encrypted_hashes,
            "emoticon": [
                emoticon("🍓", EmoticonAction::Add, "add"),
                emoticon("🍓", EmoticonAction::Remove, "remove"),
            ],
            "delete": { "msgHash": "", "note": "server force-sets msgHash to the empty string; nothing is hashed" },
        },
        "usernames": username_vecs,
        "roomNames": room_name_vecs,
        "amounts": {
            "parse": [
                amount("1", 18),
                amount("1.5", 18),
                amount("0.000001", 6),
                amount(".5", 2),
                amount("2.", 2),
                amount("42", 0),
            ],
            "format": [
                formatted(1_500_000_000_000_000_000, 18),
                formatted(1_500_000, 6),
                formatted(1_000_000_000_000_000_000, 18),
                formatted(1, 6),
                formatted(0, 18),
            ],
            "hexQuantities": [
                json!({ "decimal": "0", "hex": to_hex_quantity(0) }),
                json!({ "decimal": "436", "hex": to_hex_quantity(436) }),
                json!({ "decimal": "1000000000000000000", "hex": to_hex_quantity(1_000_000_000_000_000_000) }),
            ],
        },
        "intrinsicGas": [
            gas(b""),
            gas(b"ok"),
            gas(b"hello"),
            gas(&[0xde, 0xad, 0xbe, 0xef]),
            gas(&[0x00, 0x61]),
            gas("한".as_bytes()),
            gas(&[0xab; 32]),
        ],
        "slippage": {
            "apply": [
                json!({ "amount": "10000", "bps": 50, "amountOutMin": bank::apply_slippage_bps(10_000, 50).to_string() }),
                json!({ "amount": "10000", "bps": 0, "amountOutMin": bank::apply_slippage_bps(10_000, 0).to_string() }),
                json!({ "amount": "10000", "bps": 10000, "amountOutMin": bank::apply_slippage_bps(10_000, 10_000).to_string() }),
            ],
            "parsePercent": [
                json!({ "input": "0.5", "bps": bank::slippage_bps("0.5") }),
                json!({ "input": "50", "bps": bank::slippage_bps("50") }),
                json!({ "input": "99", "bps": bank::slippage_bps("99") }),
                json!({ "input": "0", "bps": bank::slippage_bps("0") }),
                json!({ "input": "nonsense", "bps": bank::slippage_bps("nonsense") }),
            ],
        },
        "abi": { "selectors": selectors, "calls": abi_calls },
        "rlp": rlp_vecs,
        "transactions": [
            tx_vector("eip155-spec-example-chain-1", &eip155_tx, &eip155_key),
            tx_vector("same-tx-cronos-mainnet-25", &cronos_tx, &eip155_key),
            tx_vector("erc20-transfer-cronos-testnet-338", &usdc_transfer, &secret(WALLET_B_KEY)),
        ],
        "realtimeEvents": {
            "server": server_events,
            "client": client_messages,
            "targets": targets,
        },
    })
}

fn vectors_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/vectors/protocol-v1.json")
}

/// The vendored file must be exactly what the generator produces. Regenerate
/// with `UPDATE_PROTOCOL_VECTORS=1` after an intentional protocol change.
#[test]
fn vendored_protocol_vectors_match_the_generator() {
    let generated = build();
    let path = vectors_path();

    if std::env::var("UPDATE_PROTOCOL_VECTORS").is_ok() {
        let pretty = serde_json::to_string_pretty(&generated).unwrap();
        std::fs::write(&path, format!("{pretty}\n")).unwrap();
        return;
    }

    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "protocol vectors missing at {} ({e}). Regenerate with \
             UPDATE_PROTOCOL_VECTORS=1 cargo test -p pocketskynet-core --test protocol_vectors",
            path.display()
        )
    });
    let vendored: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        vendored, generated,
        "tests/vectors/protocol-v1.json is stale — regenerate with UPDATE_PROTOCOL_VECTORS=1"
    );
}
