//! Hostile input: SQL injection, oversized bodies, path traversal, control
//! characters, and error bodies that must never leak internal detail.
//! Spec: `docs/API.md` §1.2, §1.5, §1.6, §1.7, §3.

mod common;

use common::*;
use serde_json::{json, Value};

/// Strings that would break a query built by concatenation. Drizzle and any
/// `rusqlite` bind-parameter port are immune; these assert the port did not
/// hand-roll SQL anywhere.
const INJECTIONS: &[&str] = &[
    "'; DROP TABLE users; --",
    "' OR '1'='1",
    "\" OR \"\"=\"",
    "1; DELETE FROM messages WHERE 1=1; --",
    "admin'--",
    "' UNION SELECT * FROM users --",
    "%' OR 1=1 --",
    "\\'; DROP TABLE rooms; --",
];

/// Substrings that must never appear in any error body served to a client.
const LEAKY_TERMS: &[&str] = &[
    "sqlite",
    "rusqlite",
    "panic",
    "unwrap",
    "backtrace",
    "src/",
    ".rs:",
    "SELECT ",
    "INSERT INTO",
    "no such table",
    "thread '",
    "RUST_BACKTRACE",
];

fn assert_no_internal_detail(label: &str, body: &str) {
    let lowered = body.to_lowercase();
    for term in LEAKY_TERMS {
        assert!(
            !lowered.contains(&term.to_lowercase()),
            "{label}: error body leaks `{term}`: {body}"
        );
    }
}

/// The same check, minus any echo of the payload itself.
///
/// The injection payloads deliberately contain `SELECT`, `DROP TABLE` and so
/// on, and a validation error that quotes the offending input back is correct
/// behaviour — it is the server's *own* words that must not mention SQL.
fn assert_no_internal_detail_beyond_the_echo(label: &str, body: &str, payload: &str) {
    assert_no_internal_detail(label, &body.replace(payload, "<payload>"));
}

// --- SQL injection --------------------------------------------------------

#[tokio::test]
async fn injection_strings_in_a_username_never_execute() {
    let server = TestServer::start().await;

    for payload in INJECTIONS {
        let signer = Signer::random();
        let anon = Api::anonymous(&server.base_url);
        let (challenge_id, message) = request_challenge(&anon, signer.address()).await;
        let resp = anon
            .post(
                "/api/auth/login",
                json!({
                    "walletAddress": signer.address(),
                    "username": payload,
                    "challengeId": challenge_id,
                    "signature": signer.sign(&message),
                }),
            )
            .await;

        // Every one of these contains a character the username schema forbids,
        // so the only correct outcome is a rejection — never a 5xx.
        assert_eq!(
            resp.code(),
            400,
            "payload {payload:?} must be rejected, got {}: {}",
            resp.code(),
            resp.text
        );
        assert_no_internal_detail_beyond_the_echo("username injection", &resp.text, payload);
    }

    // The database is still there and still works.
    let survivor = new_user(&server, "survivor").await;
    survivor
        .api
        .get("/api/auth/profile")
        .await
        .expect_status(200);
}

#[tokio::test]
async fn injection_strings_in_message_content_are_stored_verbatim() {
    // Message content is deliberately *not* sanitized (§3.4): mangling
    // legitimate quotes and `%` broke real messages, and bind parameters make
    // sanitizing pointless. So these must round-trip byte-for-byte.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "injection").await;

    for payload in INJECTIONS {
        let msg = send_message(&alice.api, &room, payload).await;
        assert_eq!(
            s(&msg, "content"),
            *payload,
            "content must survive unmodified"
        );
    }

    let listed = alice
        .api
        .get(&format!("/api/rooms/{room}/messages"))
        .await
        .array();
    assert_eq!(listed.len(), INJECTIONS.len(), "every message was stored");
}

#[tokio::test]
async fn injection_strings_in_a_search_query_are_harmless() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    for payload in INJECTIONS {
        let encoded = urlencode(payload);
        let resp = alice
            .api
            .get(&format!("/api/users/search?q={encoded}"))
            .await;

        assert!(
            resp.code() == 200 || resp.code() == 400,
            "search with {payload:?} must be answered or rejected, got {}: {}",
            resp.code(),
            resp.text
        );
        if resp.code() == 200 {
            assert!(
                resp.array()
                    .iter()
                    .all(|u| !s(u, "walletAddress").is_empty()),
                "a tautology payload must not dump the user table"
            );
        }
        assert_no_internal_detail_beyond_the_echo("search injection", &resp.text, payload);
    }

    alice.api.get("/api/auth/profile").await.expect_status(200);
}

#[tokio::test]
async fn injection_strings_in_a_room_name_are_rejected_or_stored_literally() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    for payload in INJECTIONS {
        let resp = alice
            .api
            .post("/api/rooms", json!({ "name": payload }))
            .await;
        assert!(
            resp.code() == 200 || resp.code() == 400,
            "room name {payload:?} produced {}: {}",
            resp.code(),
            resp.text
        );
        if resp.code() == 200 {
            assert_eq!(s(&resp.json(), "name"), *payload);
        }
        assert_no_internal_detail_beyond_the_echo("room name injection", &resp.text, payload);
    }

    alice.api.get("/api/rooms").await.expect_status(200);
}

#[tokio::test]
async fn injection_strings_in_a_room_id_are_rejected() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    for payload in INJECTIONS {
        let encoded = urlencode(payload);
        let resp = alice.api.get(&format!("/api/rooms/{encoded}")).await;

        assert!(
            resp.code() == 400 || resp.code() == 403 || resp.code() == 404,
            "room id {payload:?} produced {}: {}",
            resp.code(),
            resp.text
        );
        assert_no_internal_detail_beyond_the_echo("room id injection", &resp.text, payload);
    }
}

#[tokio::test]
async fn injection_strings_in_an_address_path_are_rejected() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    for payload in INJECTIONS {
        let encoded = urlencode(payload);
        for path in [
            format!("/api/users/{encoded}"),
            format!("/api/users/{encoded}/is-blocked"),
            format!("/api/users/block/{encoded}"),
        ] {
            let resp = alice.api.get(&path).await;
            assert!(
                resp.code() < 500,
                "{path} produced {}: {}",
                resp.code(),
                resp.text
            );
            assert_no_internal_detail_beyond_the_echo("address injection", &resp.text, payload);
        }
    }
}

// --- body size ------------------------------------------------------------

#[tokio::test]
async fn a_body_larger_than_one_hundred_kilobytes_is_refused() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "oversized").await;
    let filler = "x".repeat(200 * 1024);
    let body = json!({ "content": filler, "msgHash": crypto::sha256_hex(b"x") }).to_string();
    assert!(body.len() > 100 * 1024);

    let resp = alice
        .api
        .post_raw(&format!("/api/rooms/{room}/messages"), body)
        .await;

    assert_eq!(
        resp.code(),
        413,
        "§1.2: the 100 KB limit must answer 413, not 400 or 500; got {}: {}",
        resp.code(),
        resp.text
    );
    assert_no_internal_detail("oversized body", &resp.text);
}

#[tokio::test]
async fn an_oversized_body_does_not_break_the_connection_for_later_requests() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "recovery").await;
    let huge = json!({ "content": "x".repeat(200 * 1024), "msgHash": crypto::sha256_hex(b"x") });

    alice
        .api
        .post_raw(&format!("/api/rooms/{room}/messages"), huge.to_string())
        .await;

    send_message(&alice.api, &room, "still working").await;
}

#[tokio::test]
async fn a_body_just_under_the_limit_is_accepted_by_the_transport() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "borderline").await;
    // Well under 100 KB but over the 5000-char content rule: the size limit
    // must not fire, so the response is the schema's 400, not a 413.
    let content = "x".repeat(90 * 1024);
    let body = json!({ "content": content, "msgHash": crypto::sha256_hex(b"x") }).to_string();
    assert!(body.len() < 100 * 1024);

    let resp = alice
        .api
        .post_raw(&format!("/api/rooms/{room}/messages"), body)
        .await;

    assert_eq!(
        resp.code(),
        400,
        "expected the content-length rule, got: {}",
        resp.text
    );
}

#[tokio::test]
async fn malformed_json_is_a_client_error_not_a_server_error() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    for body in ["{", "not json at all", "[1,2,3", "{\"name\": }", ""] {
        let resp = alice.api.post_raw("/api/rooms", body.to_string()).await;
        assert!(
            (400..500).contains(&resp.code()),
            "malformed JSON {body:?} produced {}: {}",
            resp.code(),
            resp.text
        );
        assert_no_internal_detail("malformed json", &resp.text);
    }
}

#[tokio::test]
async fn deeply_nested_json_does_not_exhaust_the_parser() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let nested = format!("{}{}", "[".repeat(2000), "]".repeat(2000));

    let resp = alice.api.post_raw("/api/rooms", nested).await;

    assert!(
        resp.code() < 500,
        "deep nesting produced {}: {}",
        resp.code(),
        resp.text
    );
}

// --- path traversal -------------------------------------------------------

#[tokio::test]
async fn path_traversal_in_a_room_id_never_succeeds() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    let payloads = [
        "../../../etc/passwd",
        "..%2f..%2f..%2fetc%2fpasswd",
        "%2e%2e%2f%2e%2e%2fetc%2fpasswd",
        "....//....//etc/passwd",
        "..\\..\\windows\\system32",
        "room_1234567890_../../secret",
    ];

    for payload in payloads {
        for path in [
            format!("/api/rooms/{payload}"),
            format!("/api/rooms/{payload}/messages"),
            format!("/api/rooms/{payload}/sync?since=0"),
            format!("/api/rooms/{payload}/keys"),
        ] {
            let resp = alice.api.get(&path).await;
            assert!(
                resp.code() != 200,
                "traversal payload must never return 200: {path} -> {}",
                resp.text
            );
            assert!(
                resp.code() < 500,
                "{path} produced {}: {}",
                resp.code(),
                resp.text
            );
            assert!(
                !resp.text.contains("root:"),
                "a file was served through a room id: {}",
                resp.text
            );
            assert_no_internal_detail("room traversal", &resp.text);
        }
    }
}

#[tokio::test]
async fn path_traversal_in_a_message_id_never_succeeds() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    for payload in ["../../../etc/passwd", "..%2f..%2fsecret", "msg_1_../../x"] {
        let resp = alice
            .api
            .get(&format!("/api/messages/{payload}/emoticons"))
            .await;
        assert!(resp.code() != 200, "{payload} returned 200: {}", resp.text);
        assert!(
            resp.code() < 500,
            "{payload} produced {}: {}",
            resp.code(),
            resp.text
        );
        assert_no_internal_detail("message traversal", &resp.text);
    }
}

#[tokio::test]
async fn a_message_id_may_not_contain_a_dot() {
    // §3.1: unlike roomId, the messageId charset excludes `.`.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    let resp = alice
        .api
        .get("/api/messages/msg_1749652746620_a.b/emoticons")
        .await;

    assert!(
        resp.code() == 400 || resp.code() == 404,
        "a dotted message id must not be accepted: {} / {}",
        resp.code(),
        resp.text
    );
}

// --- identifier bounds ----------------------------------------------------

#[tokio::test]
async fn identifiers_outside_their_length_bounds_are_rejected() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    alice
        .api
        .get("/api/rooms/short")
        .await
        .expect_validation_failed();
    alice
        .api
        .get(&format!("/api/rooms/{}", "a".repeat(101)))
        .await
        .expect_validation_failed();
    alice
        .api
        .get(&format!("/api/messages/{}/emoticons", "a".repeat(101)))
        .await
        .expect_status(400);
}

// --- unicode and control characters ---------------------------------------

#[tokio::test]
async fn control_characters_in_a_username_are_rejected() {
    let server = TestServer::start().await;

    for payload in [
        "null\u{0000}byte",
        "bell\u{0007}char",
        "esc\u{001b}[31m",
        "del\u{007f}char",
    ] {
        let signer = Signer::random();
        let anon = Api::anonymous(&server.base_url);
        let (challenge_id, message) = request_challenge(&anon, signer.address()).await;

        let resp = anon
            .post(
                "/api/auth/login",
                json!({
                    "walletAddress": signer.address(),
                    "username": payload,
                    "challengeId": challenge_id,
                    "signature": signer.sign(&message),
                }),
            )
            .await;

        assert_eq!(
            resp.code(),
            400,
            "control characters must be rejected ({payload:?}): {}",
            resp.text
        );
    }
}

#[tokio::test]
async fn a_null_byte_in_message_content_is_rejected_or_preserved_exactly() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "nulls").await;
    let payload = "before\u{0000}after";

    let resp = alice
        .api
        .post(
            &format!("/api/rooms/{room}/messages"),
            json!({ "content": payload, "msgHash": crypto::sha256_hex(payload.as_bytes()) }),
        )
        .await;

    assert!(
        resp.code() == 200 || resp.code() == 400,
        "got {}: {}",
        resp.code(),
        resp.text
    );
    if resp.code() == 200 {
        // Truncation at the NUL would mean a C-string crept into the path.
        assert_eq!(s(&resp.json(), "content"), payload);
    }
}

#[tokio::test]
async fn unicode_survives_every_text_field_intact() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    for text in [
        "한국어",
        "日本語",
        "Ελληνικά",
        "🍎🍇🍊",
        "e\u{0301}combining",
        "\u{200b}zero-width",
        "\u{202e}rtl-override",
    ] {
        let resp = alice.api.post("/api/rooms", json!({ "name": text })).await;
        assert!(
            resp.code() == 200 || resp.code() == 400,
            "{text:?}: {}",
            resp.text
        );
        if resp.code() == 200 {
            assert_eq!(
                s(&resp.json(), "name"),
                text,
                "the stored name must be byte-identical"
            );
        }
    }
}

#[tokio::test]
async fn a_multibyte_username_is_measured_in_characters_not_bytes() {
    // "한글" is 6 UTF-8 bytes but 2 characters, so it must fail the 3-char
    // minimum rather than pass a byte-length check.
    let server = TestServer::start().await;
    let anon = Api::anonymous(&server.base_url);
    let signer = Signer::random();
    let (challenge_id, message) = request_challenge(&anon, signer.address()).await;

    let resp = anon
        .post(
            "/api/auth/login",
            json!({
                "walletAddress": signer.address(),
                "username": "한글",
                "challengeId": challenge_id,
                "signature": signer.sign(&message),
            }),
        )
        .await;

    assert_eq!(
        resp.code(),
        400,
        "length is counted in characters: {}",
        resp.text
    );
}

// --- error hygiene --------------------------------------------------------

#[tokio::test]
async fn no_error_body_ever_leaks_internal_detail() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "errors").await;

    let mut bodies: Vec<(String, String)> = Vec::new();
    let requests: Vec<(&str, String, Option<Value>)> = vec![
        ("GET", "/api/rooms/bad!id".into(), None),
        ("GET", "/api/users/notanaddress".into(), None),
        ("GET", "/api/users/search".into(), None),
        ("GET", "/api/messages/x/emoticons".into(), None),
        ("GET", "/api/nonexistent-endpoint".into(), None),
        ("POST", "/api/rooms".into(), Some(json!({ "name": "" }))),
        (
            "POST",
            "/api/users/block".into(),
            Some(json!({ "address": 42 })),
        ),
        (
            "POST",
            format!("/api/rooms/{room}/messages"),
            Some(json!({ "content": 1234, "msgHash": true })),
        ),
        (
            "POST",
            format!("/api/rooms/{room}/read"),
            Some(json!({ "lastReadSerial": "abc" })),
        ),
        (
            "POST",
            format!("/api/rooms/{room}/rotate-key"),
            Some(json!({ "newVersion": "two", "keys": {} })),
        ),
        (
            "PUT",
            "/api/auth/encryption-key".into(),
            Some(json!({ "publicKey": [], "publicKeySig": 0 })),
        ),
        ("PATCH", "/api/messages/nope/x".into(), Some(json!({}))),
    ];

    for (method, path, body) in requests {
        let resp = match (method, body) {
            ("GET", _) => alice.api.get(&path).await,
            ("POST", Some(b)) => alice.api.post(&path, b).await,
            ("PUT", Some(b)) => alice.api.put(&path, b).await,
            ("PATCH", Some(b)) => alice.api.patch(&path, b).await,
            _ => unreachable!("every non-GET entry supplies a body"),
        };
        assert_no_internal_detail(&path, &resp.text);
        bodies.push((path, resp.text.clone()));
    }

    // Whatever the status, the envelope is always an object with `message`.
    for (path, body) in &bodies {
        if body.is_empty() {
            continue;
        }
        let parsed: Value = serde_json::from_str(body)
            .unwrap_or_else(|_| panic!("{path}: error body must be JSON, got: {body}"));
        assert!(
            parsed.get("message").and_then(Value::as_str).is_some(),
            "{path}: every error body carries a `message` string: {body}"
        );
    }
}

#[tokio::test]
async fn a_five_hundred_never_carries_a_specific_message() {
    // §1.5: status ≥ 500 always answers exactly `Internal Server Error`.
    // This test passes vacuously when nothing 500s, which is the goal.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    let probes = [
        "/api/rooms/%00%00%00%00%00%00%00%00%00%00",
        "/api/users/0x0000000000000000000000000000000000000000",
        "/api/rooms/hidden",
        "/api/invitations",
    ];

    for path in probes {
        let resp = alice.api.get(path).await;
        if resp.code() >= 500 {
            assert_eq!(
                resp.message(),
                "Internal Server Error",
                "{path} leaked detail in a 5xx: {}",
                resp.text
            );
        }
    }
}

// --- transport hardening --------------------------------------------------

#[tokio::test]
async fn cors_echoes_only_allowlisted_origins() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    let allowed = alice
        .api
        .get_with_origin("/api/auth/profile", "http://localhost:5173")
        .await;
    assert_eq!(
        allowed.header("access-control-allow-origin").as_deref(),
        Some("http://localhost:5173"),
        "a loopback origin must be echoed"
    );

    let denied = alice
        .api
        .get_with_origin("/api/auth/profile", "https://evil.example.com")
        .await;
    let echoed = denied.header("access-control-allow-origin");
    assert!(
        echoed.is_none() || echoed.as_deref() == Some("null"),
        "an unlisted origin must not be echoed, got {echoed:?}"
    );
}

#[tokio::test]
async fn cors_never_pairs_a_wildcard_with_credentials() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    let resp = alice
        .api
        .get_with_origin("/api/auth/profile", "http://127.0.0.1:5173")
        .await;

    if resp.header("access-control-allow-credentials").as_deref() == Some("true") {
        assert_ne!(
            resp.header("access-control-allow-origin").as_deref(),
            Some("*"),
            "`*` with credentials is rejected by every browser and unsafe besides"
        );
    }
}

#[tokio::test]
async fn a_preflight_request_short_circuits() {
    let server = TestServer::start().await;
    let api = Api::anonymous(&server.base_url);

    let resp = api.options("/api/rooms", "http://localhost:5173").await;

    assert!(
        resp.code() == 200 || resp.code() == 204,
        "OPTIONS must short-circuit, got {}: {}",
        resp.code(),
        resp.text
    );
}

#[tokio::test]
async fn security_headers_accompany_error_responses_too() {
    let server = TestServer::start().await;
    let api = Api::anonymous(&server.base_url);

    let resp = api.get("/api/auth/profile").await;

    resp.expect_status(401);
    assert_eq!(
        resp.header("x-content-type-options").as_deref(),
        Some("nosniff")
    );
    assert_eq!(resp.header("x-frame-options").as_deref(), Some("DENY"));
}

#[tokio::test]
async fn an_unknown_api_path_is_a_clean_404() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    let resp = alice.api.get("/api/there-is-no-such-endpoint").await;

    assert_eq!(resp.code(), 404);
    assert_no_internal_detail("unknown path", &resp.text);
}

// --- authorization can't be bypassed --------------------------------------

#[tokio::test]
async fn a_token_cannot_be_reused_to_act_as_another_wallet() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&bob.api, "bob only").await;

    // Alice knows the room id, has a valid token, and asks nicely.
    alice
        .api
        .get(&format!("/api/rooms/{room}"))
        .await
        .expect_status(403);
    try_send_message(&alice.api, &room, "sneaking in")
        .await
        .expect_status(403);
    alice
        .api
        .post(
            &format!("/api/rooms/{room}/invite"),
            json!({ "userAddress": alice.address }),
        )
        .await
        .expect_status(403);
}

#[tokio::test]
async fn a_swapped_wallet_claim_does_not_grant_access() {
    // The JWT's `walletAddress` is the only identity claim, so a token minted
    // for another address must be usable only with the right secret.
    let server = TestServer::start().await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&bob.api, "bob only").await;
    let forged = mint_token_with_wrong_secret(&bob.address);

    Api::with_token(&server.base_url, &forged)
        .get(&format!("/api/rooms/{room}"))
        .await
        .expect_error(401, "Invalid token");
}

fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}
