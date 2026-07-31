//! Response compression.
//!
//! The WASM bundle dominates what this server ships, and it compresses roughly
//! 4.5:1 — so compression is not a micro-optimisation here, it is most of the
//! first-load time. These tests guard the two halves of getting it right:
//! that it is actually on for the big static assets, and that it is off for
//! the one response type it would break.

mod common;

use common::*;

/// Fetch a path with a given `Accept-Encoding` and report what came back.
async fn fetch(
    url: &str,
    encoding: Option<&str>,
    auth: Option<&str>,
) -> (u16, Option<String>, usize) {
    // `reqwest`'s own decompression would hide the header we are testing, so
    // this client is built without it and reads the raw body.
    let client = reqwest::Client::builder()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .build()
        .expect("client");

    let mut req = client.get(url);
    if let Some(enc) = encoding {
        req = req.header("accept-encoding", enc);
    }
    if let Some(a) = auth {
        req = req.header("authorization", a);
    }
    let res = req.send().await.expect("request");
    let status = res.status().as_u16();
    let enc = res
        .headers()
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = res.bytes().await.expect("body").len();
    (status, enc, body)
}

#[tokio::test]
async fn a_sizeable_json_response_is_compressed_and_shrinks_substantially() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    // Enough rooms that the listing clears the size floor comfortably. The
    // harness serves an empty static dir, so an API response is the honest way
    // to exercise the same layer the WASM bundle goes through in production.
    for i in 0..8 {
        create_room(&alice.api, &format!("Compression Room {i}")).await;
    }

    let url = server.url("/api/rooms");
    let auth = format!("Bearer {}", alice.api.token());

    let (status, plain_enc, plain_len) = fetch(&url, None, Some(&auth)).await;
    assert_eq!(status, 200);
    assert_eq!(plain_enc, None, "no Accept-Encoding ⇒ no compression");
    assert!(
        plain_len > 512,
        "body must clear the floor, was {plain_len}"
    );

    let (_, gz_enc, gz_len) = fetch(&url, Some("gzip"), Some(&auth)).await;
    assert_eq!(gz_enc.as_deref(), Some("gzip"));
    assert!(
        gz_len < plain_len,
        "gzip should shrink repetitive JSON: {gz_len} vs {plain_len}"
    );

    let (_, br_enc, br_len) = fetch(&url, Some("br"), Some(&auth)).await;
    assert_eq!(br_enc.as_deref(), Some("br"));
    assert!(
        br_len <= gz_len,
        "brotli should not be worse than gzip: {br_len} vs {gz_len}"
    );
}

#[tokio::test]
async fn brotli_is_preferred_when_the_client_offers_both() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    for i in 0..8 {
        create_room(&alice.api, &format!("Preference Room {i}")).await;
    }

    let auth = format!("Bearer {}", alice.api.token());
    let (_, enc, _) = fetch(&server.url("/api/rooms"), Some("gzip, br"), Some(&auth)).await;
    assert_eq!(
        enc.as_deref(),
        Some("br"),
        "brotli compresses better and every modern browser offers it"
    );
}

#[tokio::test]
async fn an_sse_stream_is_never_compressed() {
    // This is the one that matters. A compressor buffers input to fill a block,
    // and a buffered stream is not a stream: events would arrive in clumps, or
    // not until the connection closed. Slower bytes beat broken realtime.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    let body = alice.api.post_empty("/api/events/ticket").await.expect_ok();
    let ticket = body["ticket"].as_str().expect("a ticket").to_owned();

    let client = reqwest::Client::builder()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .build()
        .expect("client");

    let res = client
        .get(server.url(&format!("/api/events?ticket={ticket}")))
        .header("accept-encoding", "br, gzip")
        .send()
        .await
        .expect("sse request");

    assert_eq!(res.status().as_u16(), 200);
    assert_eq!(
        res.headers().get("content-encoding"),
        None,
        "SSE must be delivered unbuffered"
    );
    assert_eq!(
        res.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap_or("").trim().to_owned()),
        Some("text/event-stream".to_owned())
    );
}

#[tokio::test]
async fn a_tiny_json_response_is_left_alone() {
    // Below the size floor the framing overhead exceeds the saving, and every
    // compressed response costs CPU on a server that may be a laptop.
    let server = TestServer::start().await;
    let (status, enc, len) = fetch(&server.url("/api/health"), Some("br, gzip"), None).await;

    assert_eq!(status, 200);
    assert!(len < 512, "health should be tiny, was {len}");
    assert_eq!(
        enc, None,
        "bodies under the floor are not worth compressing"
    );
}
