//! The paid features, end to end (`docs/API.md` §16): Shout and web
//! publishing over a real server, real wallets, real HTTP.
//!
//! The suite runs with `--no-payment-verify` (the harness passes it), so the
//! RPC half of the payment path is *not* exercised here — it has its own
//! unit tests against a mock chain in `payment.rs`. What only this file can
//! prove is everything downstream of verification: the single-use ledger
//! shared across features, the wire shapes, the public hosting path with its
//! sandbox CSP, and the any-user delete.

mod common;

use common::*;

/// The operator's wallet for this suite. The harness scrubs `VITE_*` from the
/// child's environment, so a server started without this has no payment
/// wallet and refuses every paid action — which is its own test, below.
const OPERATOR: &str = "0x2222222222222222222222222222222222222222";

async fn paid_server() -> TestServer {
    TestServer::start_with_env(&[("VITE_FRUITNATION_WALLET", OPERATOR)]).await
}

fn tx(byte: char) -> String {
    format!("0x{}", byte.to_string().repeat(64))
}

fn q(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Publish raw bytes as `user`, returning the response as JSON + status.
async fn publish(
    server: &TestServer,
    user: &User,
    title: &str,
    tx_hash: &str,
    bytes: Vec<u8>,
) -> (u16, serde_json::Value) {
    let url = server.url(&format!(
        "/api/sites?title={}&txHash={}",
        q(title),
        q(tx_hash)
    ));
    let response = user
        .api
        .http
        .post(url)
        .header("Authorization", format!("Bearer {}", user.api.token()))
        .body(bytes)
        .send()
        .await
        .expect("publish request");
    let status = response.status().as_u16();
    let body: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
    (status, body)
}

#[tokio::test]
async fn a_shout_reaches_the_whole_server_and_burns_out() {
    let server = paid_server().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;

    let created = alice
        .api
        .post(
            "/api/shout",
            serde_json::json!({
                "text": "  Judgment day is postponed 🎉  ",
                "txHash": tx('a'),
                "durationSecs": 60
            }),
        )
        .await
        .expect_ok();
    assert_eq!(created["text"], "Judgment day is postponed 🎉");
    assert_eq!(created["username"], "alice");
    let expires = created["expiresAt"].as_i64().unwrap();
    let created_at = created["createdAt"].as_i64().unwrap();
    assert_eq!(expires - created_at, 60_000, "the minute, exactly");

    // Bob — no shared room with alice anywhere — still sees it: being
    // connected to the server is the audience.
    let active = bob.api.get("/api/shout/active").await.expect_ok();
    let shouts = active["shouts"].as_array().unwrap();
    assert_eq!(shouts.len(), 1);
    assert_eq!(shouts[0]["id"], created["id"]);
    assert_eq!(shouts[0]["username"], "alice");

    // Anonymous clients see nothing: the audience is signed-in users.
    alice
        .api
        .without_token()
        .get("/api/shout/active")
        .await
        .expect_status(401);
}

#[tokio::test]
async fn one_payment_buys_one_action_across_both_features() {
    let server = paid_server().await;
    let alice = new_user(&server, "alice").await;

    // The hash pays for a shout…
    alice
        .api
        .post(
            "/api/shout",
            serde_json::json!({ "text": "paid once", "txHash": tx('b') }),
        )
        .await
        .expect_ok();

    // …and is then worthless everywhere: another shout, any case games, and
    // the *other* feature.
    alice
        .api
        .post(
            "/api/shout",
            serde_json::json!({ "text": "again?", "txHash": tx('b') }),
        )
        .await
        .expect_status(409);
    alice
        .api
        .post(
            "/api/shout",
            serde_json::json!({ "text": "AGAIN?", "txHash": tx('b').to_uppercase().replace("0X", "0x") }),
        )
        .await
        .expect_status(409);
    let (status, body) = publish(
        &server,
        &alice,
        "Recycled",
        &tx('b'),
        b"<h1>free hosting?</h1>".to_vec(),
    )
    .await;
    assert_eq!(status, 409, "{body}");
}

#[tokio::test]
async fn a_published_zip_is_hosted_publicly_sandboxed_and_owner_deletable() {
    use std::io::Write;

    let server = paid_server().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;

    // A wrapped zip, the way "Compress" produces one.
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut cursor);
        let options = zip::write::SimpleFileOptions::default();
        writer.start_file("fanpage/index.html", options).unwrap();
        writer
            .write_all(b"<html><body><h1>Skynet appreciation page</h1><script>console.log(localStorage)</script></body></html>")
            .unwrap();
        writer.start_file("fanpage/css/app.css", options).unwrap();
        writer.write_all(b"h1 { color: cyan }").unwrap();
        writer.finish().unwrap();
    }

    let (status, site) = publish(&server, &alice, "Fan page", &tx('c'), cursor.into_inner()).await;
    assert_eq!(status, 201, "{site}");
    assert_eq!(
        site["fileCount"], 2,
        "wrapper folder stripped, dotfiles none"
    );
    let url = site["url"].as_str().unwrap().to_owned();

    // Served to the world: no token, real content type, and the sandbox CSP
    // that keeps that script's `localStorage` probe inside an opaque origin.
    let anon = server.client();
    let page = anon.get(server.url(&url)).send().await.unwrap();
    assert_eq!(page.status().as_u16(), 200);
    let csp = page
        .headers()
        .get("content-security-policy")
        .and_then(|v| v.to_str().ok())
        .expect("published pages must carry a CSP")
        .to_owned();
    assert!(csp.contains("sandbox"), "{csp}");
    assert!(!csp.contains("allow-same-origin"), "{csp}");
    let html = page.text().await.unwrap();
    assert!(html.contains("Skynet appreciation page"));

    let css = anon
        .get(server.url(&format!("{url}css/app.css")))
        .send()
        .await
        .unwrap();
    assert_eq!(css.status().as_u16(), 200);
    assert_eq!(
        css.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/css; charset=utf-8")
    );

    // Findable by anyone signed in, through the same search as knowledge.
    let hits = bob
        .api
        .get("/api/search?q=appreciation&kind=site")
        .await
        .expect_ok();
    assert_eq!(hits["results"].as_array().unwrap().len(), 1);

    // Bob is not the owner and this server has no admins configured (the
    // harness scrubs VITE_FRUITNATION_ADMIN), so he cannot spend the payment
    // Alice made. Publishing costs real money; deletion is not a refund.
    let id = site["id"].as_str().unwrap();
    bob.api
        .delete(&format!("/api/sites/{id}"))
        .await
        .expect_status(403);
    alice
        .api
        .delete(&format!("/api/sites/{id}"))
        .await
        .expect_ok();
    let gone = anon.get(server.url(&url)).send().await.unwrap();
    assert_eq!(gone.status().as_u16(), 404, "deleted means unreachable");
    let hits = bob
        .api
        .get("/api/search?q=appreciation&kind=site")
        .await
        .expect_ok();
    assert!(hits["results"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn a_server_with_no_operator_wallet_sells_nothing() {
    let server = TestServer::start().await; // VITE_FRUITNATION_WALLET scrubbed
    let alice = new_user(&server, "alice").await;
    let resp = alice
        .api
        .post(
            "/api/shout",
            serde_json::json!({ "text": "free?", "txHash": tx('f') }),
        )
        .await;
    resp.expect_status(400);
    assert!(
        resp.message().contains("no payment wallet"),
        "the operator must be told what to configure: {}",
        resp.message()
    );
}

#[tokio::test]
async fn the_boot_config_names_both_prices() {
    let server = TestServer::start().await;
    let anon = Api::anonymous(&server.url(""));
    let info = anon.get("/api/blockchain/info").await.expect_ok();
    assert_eq!(info["shoutPriceCro"], "10");
    assert_eq!(info["publishPriceCro"], "1");
}
