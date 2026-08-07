//! Shared harness for the PocketSkynet end-to-end integration suite.
//!
//! Each `tests/*.rs` file is its own crate and pulls this module in with
//! `mod common;`, so any given binary uses only part of it — hence the
//! blanket `dead_code` allowance.

// Each test binary uses only part of the harness, so the unused halves are
// expected rather than a smell.
#![allow(dead_code, unused_imports)]

pub mod api;
pub mod crypto;
pub mod harness;

use serde_json::{json, Value};

pub use api::{
    b, expect_keys, expect_message_shape, expect_no_keys, expect_room_key_shape, expect_room_shape,
    expect_user_shape, i, s, Api, Resp,
};
pub use crypto::{
    new_entry_id, open_secret, seal_secret, sealed_from_json, sealed_to_json, vault_key, Identity,
    Signer,
};
pub use harness::{TestServer, JWT_SECRET};

// --- authentication -------------------------------------------------------

/// A logged-in test identity: the wallet that signs, plus a client that carries
/// the resulting JWT.
pub struct User {
    pub signer: Signer,
    pub api: Api,
    pub address: String,
    pub username: String,
    /// The per-account E2EE derivation salt handed back by login.
    pub encryption_salt: String,
}

impl std::ops::Deref for User {
    type Target = Api;
    fn deref(&self) -> &Api {
        &self.api
    }
}

impl User {
    /// Derive this user's E2EE identity and publish the wallet binding, so
    /// other members can wrap room keys to it (§7.3 steps 3–5).
    pub async fn publish_encryption_key(&self) -> Identity {
        let identity = crypto::derive_encryption_identity(&self.signer, &self.encryption_salt);
        let signature = crypto::key_binding_signature(&self.signer, &identity.public_key);
        self.api
            .put(
                "/api/auth/encryption-key",
                json!({ "publicKey": identity.public_key, "publicKeySig": signature }),
            )
            .await
            .expect_status(200);
        identity
    }
}

/// Full challenge → EIP-191 sign → login flow for a brand-new random wallet.
pub async fn new_user(server: &TestServer, username: &str) -> User {
    let signer = Signer::random();
    login(server, signer, username).await
}

/// Log an existing wallet in (again). Reuses the caller's `Signer` so a second
/// login for the same address exercises the upsert path.
pub async fn login(server: &TestServer, signer: Signer, username: &str) -> User {
    // `None` for a plain-HTTP server, so its clients are built exactly as they
    // always were; `Some` only for the TLS suite.
    let ca = server.ca_pem();
    let anon = Api::anonymous_trusting(&server.base_url, ca.as_deref());
    let (challenge_id, message) = request_challenge(&anon, signer.address()).await;
    let signature = signer.sign(&message);

    let resp = anon
        .post(
            "/api/auth/login",
            json!({
                "walletAddress": signer.address(),
                "username": username,
                "challengeId": challenge_id,
                "signature": signature,
            }),
        )
        .await;
    let body = resp.expect_ok();

    let token = s(&body, "token");
    let user = body
        .get("user")
        .unwrap_or_else(|| panic!("login response has no `user`: {body}"));
    let address = s(user, "walletAddress");
    let encryption_salt = s(&body, "encryptionSalt");

    let mut api = Api::with_token_trusting(&server.base_url, &token, ca.as_deref());
    api.address = address.clone();
    api.username = username.to_string();

    User {
        signer,
        api,
        address,
        username: username.to_string(),
        encryption_salt,
    }
}

/// `POST /api/auth/challenge`, returning `(challengeId, message)`.
pub async fn request_challenge(anon: &Api, address: &str) -> (String, String) {
    let resp = anon
        .post("/api/auth/challenge", json!({ "walletAddress": address }))
        .await;
    let body = resp.expect_ok();
    (s(&body, "challengeId"), s(&body, "message"))
}

/// The 64-hex nonce embedded in a challenge message.
pub fn nonce_of(message: &str) -> String {
    message
        .rsplit("Nonce:\n")
        .next()
        .expect("challenge message has a Nonce section")
        .trim()
        .to_string()
}

// --- JWT forging (for the §1.3 negative tests) ----------------------------

/// Mint a token with arbitrary claims, signed with the server's real secret.
pub fn mint_token(address: &str, issued_at: i64, expires_at: i64) -> String {
    let claims = json!({ "walletAddress": address, "iat": issued_at, "exp": expires_at });
    let key = jsonwebtoken::EncodingKey::from_secret(JWT_SECRET.as_bytes());
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
    jsonwebtoken::encode(&header, &claims, &key).expect("encode JWT")
}

/// Same claims, signed with a *different* secret — must never be accepted.
pub fn mint_token_with_wrong_secret(address: &str) -> String {
    let claims = json!({ "walletAddress": address, "iat": now_secs(), "exp": now_secs() + 3600 });
    let key = jsonwebtoken::EncodingKey::from_secret(b"an-entirely-different-signing-secret-value");
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
    jsonwebtoken::encode(&header, &claims, &key).expect("encode JWT")
}

/// An `alg: none` token with an empty signature — the classic substitution
/// attack that pinning HS256 must defeat.
pub fn mint_alg_none_token(address: &str) -> String {
    use base64::Engine;
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = engine.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let claims = json!({ "walletAddress": address, "iat": now_secs(), "exp": now_secs() + 3600 });
    let payload = engine.encode(serde_json::to_vec(&claims).expect("serialize claims"));
    format!("{header}.{payload}.")
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_secs() as i64
}

pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_millis() as i64
}

// --- room / message fixtures ----------------------------------------------

/// Create a room and return its id. The caller becomes its sole member+admin.
pub async fn create_room(api: &Api, name: &str) -> String {
    let body = api
        .post("/api/rooms", json!({ "name": name }))
        .await
        .expect_ok();
    s(&body, "id")
}

/// Invite `guest` to `room` and have them accept — the only way to gain
/// membership other than creating the room (§6.5.1).
pub async fn add_member(admin: &Api, guest: &User, room: &str) {
    admin
        .post(
            &format!("/api/rooms/{room}/invite"),
            json!({ "userAddress": guest.address }),
        )
        .await
        .expect_status(200);
    guest
        .api
        .post_empty(&format!("/api/invitations/{room}/accept"))
        .await
        .expect_status(200);
}

/// Send a plaintext message, computing `msgHash` the way a client must.
pub async fn send_message(api: &Api, room: &str, content: &str) -> Value {
    let resp = api
        .post(
            &format!("/api/rooms/{room}/messages"),
            json!({
                "content": content,
                "msgHash": crypto::sha256_hex(content.trim().as_bytes()),
            }),
        )
        .await;
    resp.expect_ok()
}

/// `POST …/messages` without asserting success, for the failure-mode tests.
pub async fn try_send_message(api: &Api, room: &str, content: &str) -> Resp {
    api.post(
        &format!("/api/rooms/{room}/messages"),
        json!({
            "content": content,
            "msgHash": crypto::sha256_hex(content.trim().as_bytes()),
        }),
    )
    .await
}

/// `GET …/sync?since=`, returning `(events, hasMore)` from the `X-Has-More`
/// header (§6.12.1).
pub async fn sync(api: &Api, room: &str, since: i64) -> (Vec<Value>, bool) {
    let resp = api
        .get(&format!("/api/rooms/{room}/sync?since={since}"))
        .await;
    resp.expect_status(200);
    let has_more = resp
        .header("x-has-more")
        .unwrap_or_else(|| panic!("/sync must set X-Has-More; headers: {:?}", resp.headers));
    assert!(
        has_more == "true" || has_more == "false",
        "X-Has-More must be exactly `true` or `false`, got `{has_more}`"
    );
    (resp.array(), has_more == "true")
}

/// Drain `/sync` to the end, following `X-Has-More` (§8.2).
pub async fn drain_sync(api: &Api, room: &str) -> Vec<Value> {
    let mut cursor = 0i64;
    let mut all = Vec::new();
    loop {
        let (batch, has_more) = sync(api, room, cursor).await;
        for event in &batch {
            cursor = cursor.max(i(event, "msgSerial"));
        }
        let empty = batch.is_empty();
        all.extend(batch);
        if !has_more || empty {
            break;
        }
    }
    all
}

pub async fn latest_serial(api: &Api, room: &str) -> i64 {
    let body = api
        .get(&format!("/api/rooms/{room}/latest-serial"))
        .await
        .expect_ok();
    i(&body, "serial")
}

/// Find a room in `GET /api/rooms` by id.
pub async fn room_in_list(api: &Api, room: &str) -> Option<Value> {
    let rooms = api.get("/api/rooms").await;
    rooms.expect_status(200);
    rooms.array().into_iter().find(|r| s(r, "id") == room)
}

// --- the §9 message-event fold -------------------------------------------

/// Client-side state produced by folding a `/sync` stream, per §9.
#[derive(Default, Debug)]
pub struct FoldState {
    pub messages: std::collections::BTreeMap<String, Value>,
    pub reactions:
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, Vec<String>>>,
    pub cursor: i64,
}

/// The reference fold from §9, including the two corrections API.md calls out:
/// `edit` upserts (never "only if present"), and `delete_all` clears state at
/// its own point in serial order rather than at the end of the batch.
pub fn fold(state: &mut FoldState, event: &Value) {
    let msg_type = s(event, "msgType");
    let id = s(event, "id");
    match msg_type.as_str() {
        "add" | "message" | "edit" => {
            state.messages.insert(id, event.clone());
        }
        "delete" => {
            state.messages.remove(&id);
            state.reactions.remove(&id);
        }
        "delete_all" => {
            state.messages.clear();
            state.reactions.clear();
        }
        "emoticon_add" => {
            let target = s(event, "targetMessageId");
            let code = s(event, "emoticonCode");
            let sender = s(event, "senderAddress");
            let set = state
                .reactions
                .entry(target)
                .or_default()
                .entry(code)
                .or_default();
            if !set.contains(&sender) {
                set.push(sender);
            }
        }
        "emoticon_remove" => {
            let target = s(event, "targetMessageId");
            let code = s(event, "emoticonCode");
            let sender = s(event, "senderAddress");
            if let Some(codes) = state.reactions.get_mut(&target) {
                if let Some(set) = codes.get_mut(&code) {
                    set.retain(|a| a != &sender);
                    if set.is_empty() {
                        codes.remove(&code);
                    }
                }
            }
        }
        // Forward compatibility: an unknown type must never abort the batch.
        _ => {}
    }
    state.cursor = state.cursor.max(i(event, "msgSerial"));
}

pub fn fold_all(events: &[Value]) -> FoldState {
    let mut state = FoldState::default();
    for event in events {
        fold(&mut state, event);
    }
    state
}
