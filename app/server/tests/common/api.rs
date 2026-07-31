//! A thin, assertion-friendly HTTP client.
//!
//! Every call returns a [`Resp`] carrying the status, the headers and the raw
//! body text — never a decoded struct. Integration tests must assert on the
//! *wire* shape (exact camelCase keys, exact error strings from `docs/API.md`),
//! and a typed struct would silently paper over a renamed or missing field.

use std::time::Duration;

use reqwest::header::HeaderMap;
use reqwest::{Method, StatusCode};
use serde_json::Value;

#[derive(Clone)]
pub struct Api {
    pub base: String,
    pub http: reqwest::Client,
    pub token: Option<String>,
    /// Lowercase wallet address of the logged-in identity, `""` when anonymous.
    pub address: String,
    pub username: String,
}

impl Api {
    pub fn anonymous(base: &str) -> Self {
        Self::anonymous_trusting(base, None)
    }

    /// An anonymous client that additionally trusts `ca_pem`, for the servers
    /// the TLS suite starts. The plain-HTTP path is left exactly as it was:
    /// two hundred tests depend on that client's timeout and proxy settings.
    pub fn anonymous_trusting(base: &str, ca_pem: Option<&[u8]>) -> Self {
        Api {
            base: base.to_string(),
            http: client(ca_pem),
            token: None,
            address: String::new(),
            username: String::new(),
        }
    }

    pub fn with_token(base: &str, token: &str) -> Self {
        Self::with_token_trusting(base, token, None)
    }

    pub fn with_token_trusting(base: &str, token: &str, ca_pem: Option<&[u8]>) -> Self {
        let mut api = Self::anonymous_trusting(base, ca_pem);
        api.token = Some(token.to_string());
        api
    }

    pub fn token(&self) -> &str {
        self.token.as_deref().unwrap_or("")
    }

    /// Same identity, but the request carries no `Authorization` header.
    pub fn without_token(&self) -> Self {
        let mut c = self.clone();
        c.token = None;
        c
    }

    /// Same identity, but with a caller-supplied token (used by the JWT tests).
    pub fn with_raw_token(&self, token: &str) -> Self {
        let mut c = self.clone();
        c.token = Some(token.to_string());
        c
    }

    pub async fn get(&self, path: &str) -> Resp {
        self.send(Method::GET, path, None).await
    }

    pub async fn post(&self, path: &str, body: Value) -> Resp {
        self.send(Method::POST, path, Some(body)).await
    }

    /// POST with no body at all — `leave`, `hide`, `accept`, `decline`.
    pub async fn post_empty(&self, path: &str) -> Resp {
        self.send(Method::POST, path, None).await
    }

    pub async fn put(&self, path: &str, body: Value) -> Resp {
        self.send(Method::PUT, path, Some(body)).await
    }

    pub async fn patch(&self, path: &str, body: Value) -> Resp {
        self.send(Method::PATCH, path, Some(body)).await
    }

    pub async fn delete(&self, path: &str) -> Resp {
        self.send(Method::DELETE, path, None).await
    }

    async fn send(&self, method: Method, path: &str, body: Option<Value>) -> Resp {
        let mut req = self.http.request(method, format!("{}{}", self.base, path));
        if let Some(t) = &self.token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        Resp::of(req).await
    }

    /// Send with a hand-written `Authorization` header (or none at all), for the
    /// §1.3 header-parsing tests.
    pub async fn get_with_auth(&self, path: &str, header: Option<&str>) -> Resp {
        let mut req = self.http.get(format!("{}{}", self.base, path));
        if let Some(h) = header {
            req = req.header("Authorization", h);
        }
        Resp::of(req).await
    }

    /// POST a body verbatim, bypassing JSON serialization — used for the
    /// oversized-body (413) and malformed-JSON checks.
    pub async fn post_raw(&self, path: &str, body: String) -> Resp {
        let mut req = self
            .http
            .post(format!("{}{}", self.base, path))
            .header("Content-Type", "application/json")
            .body(body);
        if let Some(t) = &self.token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        Resp::of(req).await
    }

    /// Issue a request with an `Origin` header, for the CORS checks.
    pub async fn get_with_origin(&self, path: &str, origin: &str) -> Resp {
        let mut req = self
            .http
            .get(format!("{}{}", self.base, path))
            .header("Origin", origin);
        if let Some(t) = &self.token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        Resp::of(req).await
    }

    pub async fn options(&self, path: &str, origin: &str) -> Resp {
        let req = self
            .http
            .request(Method::OPTIONS, format!("{}{}", self.base, path))
            .header("Origin", origin)
            .header("Access-Control-Request-Method", "POST");
        Resp::of(req).await
    }
}

fn client(ca_pem: Option<&[u8]>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        // A developer's HTTP proxy must not intercept loopback test traffic.
        .no_proxy();

    if let Some(pem) = ca_pem {
        // Trusting *only* this CA, never `danger_accept_invalid_certs`: a test
        // that skips verification would pass against a certificate no browser
        // would accept, which is the whole thing worth asserting.
        let ca = reqwest::Certificate::from_pem(pem).expect("parse the server's CA");
        builder = builder
            .add_root_certificate(ca)
            .tls_built_in_root_certs(false);
    }

    builder.build().expect("build reqwest client")
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub text: String,
}

impl Resp {
    async fn of(req: reqwest::RequestBuilder) -> Resp {
        let resp = req
            .send()
            .await
            .expect("request failed at the transport level");
        let status = resp.status();
        let headers = resp.headers().clone();
        let text = resp.text().await.unwrap_or_default();
        Resp {
            status,
            headers,
            text,
        }
    }

    pub fn code(&self) -> u16 {
        self.status.as_u16()
    }

    /// Body parsed as JSON. Panics with the raw body when it is not JSON, which
    /// is far more useful than a bare serde error.
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or_else(|e| {
            panic!(
                "body is not JSON ({e}); status {}, body: {}",
                self.status, self.text
            )
        })
    }

    pub fn array(&self) -> Vec<Value> {
        match self.json() {
            Value::Array(a) => a,
            other => panic!("expected a JSON array, got: {other}"),
        }
    }

    /// The `message` field of an error envelope.
    pub fn message(&self) -> String {
        self.json()
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("no `message` field in body: {}", self.text))
            .to_string()
    }

    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }

    // --- assertions -------------------------------------------------------

    pub fn expect_status(&self, want: u16) -> &Self {
        assert_eq!(
            self.code(),
            want,
            "expected HTTP {want}, got {}; body: {}",
            self.code(),
            self.text
        );
        self
    }

    pub fn expect_ok(&self) -> Value {
        self.expect_status(200);
        self.json()
    }

    /// A 200 whose body is exactly `{"message": …}` — several endpoints
    /// acknowledge with a message rather than a resource.
    pub fn expect_message(&self, message: &str) -> &Self {
        self.expect_status(200);
        assert_eq!(
            self.message(),
            message,
            "wrong acknowledgement; body: {}",
            self.text
        );
        self
    }

    /// Status plus the exact `{"message": …}` string from `docs/API.md`.
    pub fn expect_error(&self, want: u16, message: &str) -> &Self {
        self.expect_status(want);
        assert_eq!(
            self.message(),
            message,
            "wrong error message; body: {}",
            self.text
        );
        self
    }

    /// The §1.5 `Validation failed` envelope: 400, plus a non-empty `errors`
    /// array of `"field: reason"` strings.
    pub fn expect_validation_failed(&self) -> &Self {
        self.expect_status(400);
        let body = self.json();
        assert_eq!(
            body.get("message").and_then(Value::as_str),
            Some("Validation failed"),
            "expected the Validation failed envelope; body: {}",
            self.text
        );
        let errors = body
            .get("errors")
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("no `errors` array; body: {}", self.text));
        assert!(
            !errors.is_empty(),
            "`errors` must not be empty: {}",
            self.text
        );
        for e in errors {
            assert!(e.is_string(), "each error entry must be a string: {e}");
        }
        self
    }

    /// One of the two machine-readable 409s from `POST …/messages`.
    pub fn expect_conflict_code(&self, code: &str) -> Value {
        self.expect_status(409);
        let body = self.json();
        assert_eq!(
            body.get("code").and_then(Value::as_str),
            Some(code),
            "wrong conflict code; body: {}",
            self.text
        );
        assert!(
            body.get("message").and_then(Value::as_str).is_some(),
            "409 must carry a message; body: {}",
            self.text
        );
        assert!(
            body.get("currentKeyVersion")
                .and_then(Value::as_i64)
                .is_some(),
            "409 must carry currentKeyVersion; body: {}",
            self.text
        );
        body
    }
}

// --- shape helpers --------------------------------------------------------

/// Assert every listed key exists on the object (missing keys are the most
/// common port bug; extra keys are tolerated).
pub fn expect_keys(value: &Value, keys: &[&str]) {
    let obj = value
        .as_object()
        .unwrap_or_else(|| panic!("expected a JSON object, got: {value}"));
    for k in keys {
        assert!(obj.contains_key(*k), "missing key `{k}` in object: {value}");
    }
}

/// Assert none of the listed keys exist (used where API.md says a field is
/// *absent*, e.g. `unreadCount` on `GET /api/rooms/:roomId`).
pub fn expect_no_keys(value: &Value, keys: &[&str]) {
    let obj = value
        .as_object()
        .unwrap_or_else(|| panic!("expected a JSON object, got: {value}"));
    for k in keys {
        assert!(
            !obj.contains_key(*k),
            "key `{k}` must be absent from: {value}"
        );
    }
}

pub fn s(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("`{key}` is missing or not a string in: {value}"))
        .to_string()
}

pub fn i(value: &Value, key: &str) -> i64 {
    value
        .get(key)
        .and_then(Value::as_i64)
        .unwrap_or_else(|| panic!("`{key}` is missing or not an integer in: {value}"))
}

pub fn b(value: &Value, key: &str) -> bool {
    value
        .get(key)
        .and_then(Value::as_bool)
        .unwrap_or_else(|| panic!("`{key}` is missing or not a boolean in: {value}"))
}

/// The canonical `User` shape from §5.1.
pub fn expect_user_shape(user: &Value) {
    expect_keys(
        user,
        &[
            "walletAddress",
            "username",
            "publicKey",
            "publicKeySig",
            "createdAt",
            "updatedAt",
        ],
    );
    let addr = s(user, "walletAddress");
    assert_eq!(
        addr,
        addr.to_lowercase(),
        "walletAddress must always be lowercase: {addr}"
    );
}

/// The canonical `Message` shape from §5.5.
pub fn expect_message_shape(msg: &Value) {
    expect_keys(
        msg,
        &[
            "id",
            "roomId",
            "senderAddress",
            "content",
            "msgHash",
            "messageTimestamp",
            "msgType",
            "msgSerial",
            "isDeleted",
            "editedAt",
            "createdAt",
            "isEncrypted",
            "iv",
            "hmac",
            "encVer",
            "keyVersion",
            "txHash",
            "targetMessageId",
            "emoticonCode",
        ],
    );
}

/// The canonical `Room` shape from §5.2.
pub fn expect_room_shape(room: &Value) {
    expect_keys(
        room,
        &[
            "id",
            "name",
            "description",
            "currentKeyVersion",
            "keyRotationPending",
            "createdAt",
        ],
    );
}

/// The canonical `RoomKey` shape from §5.6.
pub fn expect_room_key_shape(key: &Value) {
    expect_keys(
        key,
        &[
            "roomId",
            "userAddress",
            "encryptedSymmetricKey",
            "ephemeralPublicKey",
            "encryptionIV",
            "hmac",
            "encVer",
            "keyVersion",
        ],
    );
}
