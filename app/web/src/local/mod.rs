//! The simulated local backend — "web local mode".
//!
//! In local mode there is no server at all: the same requests every screen
//! already makes through [`crate::api::Client`] are answered here, from
//! IndexedDB, in this tab. The client's send funnel calls [`dispatch`]; the
//! router translates `(method, path)` into storage operations; the responses
//! are `serde_json::Value`s that decode through the exact same
//! `crate::api::types` shapes a network response does, so the two backends
//! cannot drift apart silently.
//!
//! Layout:
//!
//! * [`router`] — one `match` over the supported routes. **This is the
//!   maintainer's map**: adding a local endpoint is adding one arm there.
//! * [`logic`] — pure request/response shaping: path parsing, id and serial
//!   rules, paging, search scoring. Host-tested; no browser APIs.
//! * [`db`] — the thin async IndexedDB wrapper. `wasm32`-only; a stub on the
//!   host so `cargo test` compiles the crate.
//!
//! What is deliberately *not* here: multi-user anything. Presence, mentions,
//! invitations, blocks and friends answer benign empties so `refresh_all`
//! stays quiet; every route with no local meaning answers 404 with a message
//! that names itself — its UI is hidden in local mode, so reaching one is a
//! bug report, not a user experience.

pub mod db;
pub mod logic;
mod router;

use std::cell::RefCell;

use gloo_net::http::Method;
use pocketskynet_core::WalletAddress;
use serde_json::Value;

use crate::api::error::StatusError;
use crate::api::{ApiError, ApiResult};

pub use router::sync_page;

thread_local! {
    /// Which account's database requests answer from. Installed at local
    /// sign-in/unlock, and on boot when a locked local session is restored.
    static OWNER: RefCell<Option<WalletAddress>> = const { RefCell::new(None) };
    /// The at-rest sealing key (`SessionKeys::local_store_key`). Present only
    /// while unlocked — a locked session lists sealed rows, exactly as the
    /// rest of the app treats encrypted content.
    static STORE_KEY: RefCell<Option<[u8; 32]>> = const { RefCell::new(None) };
}

/// Point the local backend at this account's database.
pub fn install_owner(owner: WalletAddress) {
    let changed = OWNER.with(|o| {
        let mut o = o.borrow_mut();
        let changed = o.as_ref() != Some(&owner);
        *o = Some(owner);
        changed
    });
    if changed {
        db::close();
    }
}

/// Arm the at-rest sealer — called at unlock with the session's key.
pub fn install_store_key(key: [u8; 32]) {
    STORE_KEY.with(|k| *k.borrow_mut() = Some(key));
}

/// Drop the sealing key (sign-out). The owner and the database survive: the
/// data at rest is ciphertext, and the next unlock re-derives the same key.
pub fn clear_store_key() {
    STORE_KEY.with(|k| *k.borrow_mut() = None);
}

/// Forget everything in-memory (account switch). The database on disk is
/// untouched; `db::delete_database` is the destructive path.
pub fn clear_session() {
    clear_store_key();
    OWNER.with(|o| *o.borrow_mut() = None);
    db::close();
}

pub(crate) fn owner() -> ApiResult<WalletAddress> {
    OWNER.with(|o| o.borrow().clone()).ok_or_else(|| {
        ApiError::Status(StatusError {
            status: 401,
            message: "No local session".into(),
            code: None,
            current_key_version: None,
            errors: Vec::new(),
            missing: Vec::new(),
        })
    })
}

pub(crate) fn store_key() -> Option<[u8; 32]> {
    STORE_KEY.with(|k| *k.borrow())
}

/// Seal a string for at-rest storage, keyed to a domain string (a pseudo
/// room id — `local:knowledge:<id>`), reusing the message-encryption
/// primitives so no new crypto exists here. Returns a self-contained JSON
/// string `{content, iv, hmac}`.
pub(crate) fn seal(domain: &str, plaintext: &str) -> ApiResult<String> {
    let Some(key) = store_key() else {
        return Err(locked());
    };
    let sealed = pocketskynet_core::crypto::encrypt_message_v2(plaintext, &key, domain)
        .map_err(|e| ApiError::Network(format!("Couldn't seal for local storage: {e}")))?;
    Ok(serde_json::json!({
        "content": sealed.content,
        "iv": sealed.iv,
        "hmac": sealed.hmac,
    })
    .to_string())
}

/// Open a string [`seal`] produced. `None` when locked, tampered, or keyed
/// to another account — one answer for all three, deliberately.
pub(crate) fn open(domain: &str, sealed_json: &str) -> Option<String> {
    let key = store_key()?;
    let sealed: Value = serde_json::from_str(sealed_json).ok()?;
    pocketskynet_core::crypto::decrypt_message_by_version(
        Some(2),
        sealed["content"].as_str()?,
        sealed["iv"].as_str()?,
        sealed["hmac"].as_str()?,
        &key,
        domain,
    )
    .ok()
}

/// Ask the browser to protect this origin's storage from eviction.
///
/// Best-effort and fire-and-forget: browsers grant it silently (or not) based
/// on engagement, and a refusal changes nothing we can act on. It matters
/// most on Safari, whose seven-day inactivity eviction would otherwise be the
/// biggest real risk to local-mode data.
#[cfg(target_arch = "wasm32")]
pub fn request_persistence() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let storage = window.navigator().storage();
    if let Ok(promise) = storage.persist() {
        wasm_bindgen_futures::spawn_local(async move {
            let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
        });
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn request_persistence() {}

/// Read — or mint once — the per-account local encryption salt.
///
/// It lives in IndexedDB, not `localStorage`: sign-out and "erase local
/// data" clear `localStorage` wholesale, and losing the salt would orphan
/// every sealed row forever. In server mode the salt is the server's secret;
/// here the device is the trust boundary, so the device holds it.
pub async fn get_or_create_salt() -> Result<String, String> {
    let owner = owner().map_err(|e| e.user_message())?;
    let d = db::open(owner.as_str()).await?;
    if let Some(salt) = d.get(db::META, "salt").await? {
        if salt.len() == 64 {
            return Ok(salt);
        }
    }
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| format!("CSPRNG refused: {e}"))?;
    let salt = hex::encode(bytes);
    d.put(db::META, "salt", &salt).await?;
    Ok(salt)
}

/// Read — or create on first sign-in — the local profile row.
///
/// A returning user keeps the stored name, mirroring the server's behaviour
/// of ignoring the login username once an account exists.
pub async fn get_or_create_user(username: &str) -> Result<crate::api::User, String> {
    let owner = owner().map_err(|e| e.user_message())?;
    let d = db::open(owner.as_str()).await?;
    if let Some(stored) = d.get(db::META, "user").await? {
        if let Ok(user) = serde_json::from_str::<crate::api::User>(&stored) {
            return Ok(user);
        }
    }
    let user = serde_json::json!({
        "walletAddress": owner.as_str(),
        "username": username,
        "createdAt": logic::iso8601_ms(crate::format::now_ms()),
    });
    d.put(db::META, "user", &user.to_string()).await?;
    serde_json::from_value(user).map_err(|e| e.to_string())
}

/// Store an AI generation's bytes, sealed, and answer with the exact URL a
/// server would have answered with — `/api/images/<sha256>.<ext>` — so the
/// message content, `media::hosted_names` and every render site stay
/// mode-blind.
pub async fn store_media(mime: &str, bytes: &[u8]) -> Result<String, String> {
    let owner = owner().map_err(|e| e.user_message())?;
    let d = db::open(owner.as_str()).await?;
    let ext = match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        other => return Err(format!("Unsupported media type: {other}")),
    };
    let name = format!("{}.{ext}", pocketskynet_core::hash::sha256_hex(bytes));
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    let sealed = seal(&format!("local:media:{name}"), &b64).map_err(|e| e.user_message())?;
    let row = serde_json::json!({ "mime": mime, "sealed": sealed }).to_string();
    d.put(db::MEDIA, &name, &row).await?;
    Ok(format!("/api/images/{name}"))
}

#[cfg(target_arch = "wasm32")]
thread_local! {
    /// name → object URL, for the life of the tab. Deliberate: the plaintext
    /// bytes live only in memory, and revoking on unmount would break the
    /// lightbox holding the same URL.
    static MEDIA_URLS: RefCell<std::collections::HashMap<String, String>> =
        RefCell::new(std::collections::HashMap::new());
}

/// Resolve a hosted-media URL (`/api/images/<name>`, or anything ending in
/// one) to a blob object URL backed by the sealed bytes in IndexedDB.
/// `None` while locked, for a name this store does not hold — including the
/// `<name>/thumbnail` variant, which local mode has no smaller copy for —
/// or off the browser.
#[cfg(target_arch = "wasm32")]
pub async fn media_url(url: &str) -> Option<String> {
    let name = url.trim_end_matches('/').rsplit('/').next()?.to_owned();
    if let Some(hit) = MEDIA_URLS.with(|m| m.borrow().get(&name).cloned()) {
        return Some(hit);
    }
    let owner = owner().ok()?;
    let d = db::open(owner.as_str()).await.ok()?;
    let row = d.get(db::MEDIA, &name).await.ok()??;
    let row: Value = serde_json::from_str(&row).ok()?;
    let mime = row["mime"].as_str().unwrap_or("application/octet-stream");
    let b64 = open(&format!("local:media:{name}"), row["sealed"].as_str()?)?;
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;

    let array = js_sys::Uint8Array::from(bytes.as_slice());
    let parts = js_sys::Array::new();
    parts.push(&array.buffer());
    let options = web_sys::BlobPropertyBag::new();
    options.set_type(mime);
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options).ok()?;
    let object_url = web_sys::Url::create_object_url_with_blob(&blob).ok()?;
    MEDIA_URLS.with(|m| m.borrow_mut().insert(name, object_url.clone()));
    Some(object_url)
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn media_url(_url: &str) -> Option<String> {
    None
}

pub(crate) fn locked() -> ApiError {
    ApiError::Status(StatusError {
        status: 403,
        message: "Unlock this session to use local mode".into(),
        code: None,
        current_key_version: None,
        errors: Vec::new(),
        missing: Vec::new(),
    })
}

/// Answer one API request from the local backend.
///
/// `body` is the request's JSON body, already serialised by the client's send
/// funnel; the returned `Value` is decoded by the same funnel through the
/// typed response shapes.
pub async fn dispatch(method: Method, path: &str, body: Option<Value>) -> ApiResult<Value> {
    router::route(method.as_str(), path, body).await
}

/// The error every unimplemented route answers. It names the path because the
/// only way to see it is a bug: routes without local support have their UI
/// hidden in local mode.
pub(crate) fn not_available(path: &str) -> ApiError {
    ApiError::Status(StatusError {
        status: 404,
        message: format!("Not available in local mode: {path}"),
        code: None,
        current_key_version: None,
        errors: Vec::new(),
        missing: Vec::new(),
    })
}
