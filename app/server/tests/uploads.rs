//! Resumable chunked uploads and streaming downloads, end to end
//! (`server/src/routes/uploads.rs`, `server/src/routes/files.rs`).
//!
//! The unit tests cover the pieces — the offset guard in `db/uploads.rs`, the
//! digest normaliser, the token scoping in `auth.rs`. What only exists with a
//! real server in play, and what this file is for:
//!
//! * that a file uploaded in pieces comes back byte-identical;
//! * that the offset is the *server's*, so a replayed or reordered chunk is
//!   refused rather than silently corrupting the result;
//! * that a wrong checksum is caught and the bytes destroyed;
//! * that `Range` works, because a 4 GB download that cannot resume is a 4 GB
//!   download that fails;
//! * that a download capability opens one file, for one person, and is not a
//!   login.
//!
//! Sizes here are small on purpose. The properties are all about *boundaries*
//! — offsets, ranges, digests — and none of them get truer at 4 GB; a test
//! suite that moved gigabytes to prove an off-by-one would just be slow.

mod common;

use common::*;

/// Deliberately not a round number and not a multiple of the chunk size below,
/// so the final short chunk is always exercised.
fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

struct Raw {
    status: u16,
    headers: reqwest::header::HeaderMap,
    bytes: Vec<u8>,
}

impl Raw {
    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.bytes).unwrap_or_else(|e| {
            panic!(
                "body is not JSON ({e}); status {}, body: {}",
                self.status,
                String::from_utf8_lossy(&self.bytes)
            )
        })
    }
}

async fn send(
    server: &TestServer,
    user: Option<&User>,
    method: reqwest::Method,
    path: &str,
    body: Option<Vec<u8>>,
    range: Option<&str>,
) -> Raw {
    send_typed(server, user, method, path, body, range, None).await
}

/// As [`send`], with an explicit `Content-Type` — which axum's `Json`
/// extractor requires and answers 415 without.
#[allow(clippy::too_many_arguments)]
async fn send_typed(
    server: &TestServer,
    user: Option<&User>,
    method: reqwest::Method,
    path: &str,
    body: Option<Vec<u8>>,
    range: Option<&str>,
    content_type: Option<&str>,
) -> Raw {
    let http = match user {
        Some(u) => u.api.http.clone(),
        None => server.client(),
    };
    let mut req = http.request(method, server.url(path));
    if let Some(u) = user {
        req = req.header("Authorization", format!("Bearer {}", u.api.token()));
    }
    if let Some(r) = range {
        req = req.header("Range", r);
    }
    if let Some(ct) = content_type {
        req = req.header("Content-Type", ct);
    }
    if let Some(b) = body {
        req = req.body(b);
    }
    let resp = req.send().await.expect("request failed");
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    Raw {
        status,
        headers,
        bytes: resp.bytes().await.unwrap().to_vec(),
    }
}

/// Begin a session for a room attachment.
async fn begin(
    server: &TestServer,
    user: &User,
    room: &str,
    filename: &str,
    data: &[u8],
    declare_digest: bool,
) -> Raw {
    let mut body = serde_json::json!({
        "kind": "file",
        "roomId": room,
        "filename": filename,
        "caption": "",
        "size": data.len(),
    });
    if declare_digest {
        body["sha256"] = serde_json::Value::String(sha256_hex(data));
    }
    send_typed(
        server,
        Some(user),
        reqwest::Method::POST,
        "/api/uploads",
        Some(serde_json::to_vec(&body).unwrap()),
        None,
        Some("application/json"),
    )
    .await
}

async fn append(server: &TestServer, user: &User, id: &str, offset: usize, chunk: &[u8]) -> Raw {
    send(
        server,
        Some(user),
        reqwest::Method::PATCH,
        &format!("/api/uploads/{id}?offset={offset}"),
        Some(chunk.to_vec()),
        None,
    )
    .await
}

async fn finish(server: &TestServer, user: &User, id: &str) -> Raw {
    send(
        server,
        Some(user),
        reqwest::Method::POST,
        &format!("/api/uploads/{id}/finish"),
        Some(Vec::new()),
        None,
    )
    .await
}

/// Drive a whole upload in `chunk` sized pieces.
async fn upload_in_chunks(
    server: &TestServer,
    user: &User,
    room: &str,
    filename: &str,
    data: &[u8],
    chunk: usize,
) -> serde_json::Value {
    let started = begin(server, user, room, filename, data, true).await;
    assert_eq!(
        started.status,
        201,
        "begin failed: {}",
        String::from_utf8_lossy(&started.bytes)
    );
    let id = started.json()["id"].as_str().unwrap().to_owned();

    for (i, piece) in data.chunks(chunk).enumerate() {
        let at = i * chunk;
        let r = append(server, user, &id, at, piece).await;
        assert_eq!(
            r.status,
            200,
            "chunk at {at} failed: {}",
            String::from_utf8_lossy(&r.bytes)
        );
        assert_eq!(r.json()["offset"], (at + piece.len()) as u64);
    }

    let done = finish(server, user, &id).await;
    assert_eq!(
        done.status,
        201,
        "finish failed: {}",
        String::from_utf8_lossy(&done.bytes)
    );
    done.json()
}

// --- the happy path -------------------------------------------------------

#[tokio::test]
async fn a_file_sent_in_pieces_comes_back_whole() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ops").await;

    let data = payload(300_000);
    let meta = upload_in_chunks(&server, &alice, &room, "big.bin", &data, 64_000).await;

    assert_eq!(meta["filename"], "big.bin");
    assert_eq!(meta["sizeBytes"], data.len() as i64);
    assert_eq!(meta["uploader"], alice.address);

    let id = meta["id"].as_str().unwrap();
    let got = send(
        &server,
        Some(&alice),
        reqwest::Method::GET,
        &format!("/api/files/{id}/raw"),
        None,
        None,
    )
    .await;
    assert_eq!(got.status, 200);
    assert_eq!(got.bytes, data, "the reassembled file is not the original");

    // The digest is published, and it is the digest of what was sent.
    assert_eq!(
        got.headers
            .get("x-content-sha256")
            .and_then(|v| v.to_str().ok()),
        Some(sha256_hex(&data).as_str())
    );
    // Resumability has to be advertised or no download manager will try it.
    assert_eq!(
        got.headers
            .get("accept-ranges")
            .and_then(|v| v.to_str().ok()),
        Some("bytes")
    );
}

#[tokio::test]
async fn a_session_reports_where_to_resume_from() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ops").await;

    let data = payload(50_000);
    let started = begin(&server, &alice, &room, "resume.bin", &data, true).await;
    let id = started.json()["id"].as_str().unwrap().to_owned();
    assert_eq!(started.json()["offset"], 0);
    // The server tells the client what chunk size to use rather than leaving
    // it to guess.
    assert!(started.json()["chunkSize"].as_u64().unwrap() > 0);

    append(&server, &alice, &id, 0, &data[..20_000]).await;

    // What a client does after its connection dropped: ask, then carry on.
    let status = send(
        &server,
        Some(&alice),
        reqwest::Method::GET,
        &format!("/api/uploads/{id}"),
        None,
        None,
    )
    .await;
    assert_eq!(status.status, 200);
    assert_eq!(status.json()["offset"], 20_000);
    assert_eq!(status.json()["size"], data.len() as u64);

    append(&server, &alice, &id, 20_000, &data[20_000..]).await;
    let done = finish(&server, &alice, &id).await;
    assert_eq!(done.status, 201);
}

// --- the offset is the server's -------------------------------------------

#[tokio::test]
async fn a_replayed_chunk_is_refused_and_says_where_to_resume() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ops").await;

    let data = payload(30_000);
    let started = begin(&server, &alice, &room, "replay.bin", &data, true).await;
    let id = started.json()["id"].as_str().unwrap().to_owned();

    append(&server, &alice, &id, 0, &data[..10_000]).await;

    // The normal case on a flaky network: the write landed, the response did
    // not, and the client retries. Writing it twice would corrupt the file.
    let again = append(&server, &alice, &id, 0, &data[..10_000]).await;
    assert_eq!(again.status, 409);
    assert!(
        again.message_contains("10000"),
        "a 409 must carry the real offset so the client can seek: {}",
        String::from_utf8_lossy(&again.bytes)
    );

    // Finishing now must fail — the file is short, and nothing should paper
    // over that.
    let premature = finish(&server, &alice, &id).await;
    assert_eq!(premature.status, 400);

    append(&server, &alice, &id, 10_000, &data[10_000..]).await;
    assert_eq!(finish(&server, &alice, &id).await.status, 201);
}

#[tokio::test]
async fn a_chunk_from_the_future_cannot_leave_a_hole() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ops").await;

    let data = payload(30_000);
    let started = begin(&server, &alice, &room, "hole.bin", &data, true).await;
    let id = started.json()["id"].as_str().unwrap().to_owned();

    // Skipping ahead would produce a file of the right length whose middle is
    // whatever the filesystem felt like.
    let ahead = append(&server, &alice, &id, 20_000, &data[20_000..]).await;
    assert_eq!(ahead.status, 409);

    let status = send(
        &server,
        Some(&alice),
        reqwest::Method::GET,
        &format!("/api/uploads/{id}"),
        None,
        None,
    )
    .await;
    assert_eq!(
        status.json()["offset"],
        0,
        "nothing should have been written"
    );
}

#[tokio::test]
async fn a_chunk_past_the_declared_size_is_refused() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ops").await;

    let data = payload(1_000);
    let started = begin(&server, &alice, &room, "over.bin", &data, false).await;
    let id = started.json()["id"].as_str().unwrap().to_owned();

    let too_much = append(&server, &alice, &id, 0, &payload(2_000)).await;
    assert_eq!(too_much.status, 400);
}

// --- integrity ------------------------------------------------------------

#[tokio::test]
async fn data_that_does_not_match_the_declared_checksum_is_destroyed() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ops").await;

    let promised = payload(20_000);
    let started = begin(&server, &alice, &room, "corrupt.bin", &promised, true).await;
    let id = started.json()["id"].as_str().unwrap().to_owned();

    // Same length, different bytes — exactly what a corrupted transfer looks
    // like, and exactly what a length check alone would wave through.
    let mut sent = promised.clone();
    sent[9_999] ^= 0xff;
    append(&server, &alice, &id, 0, &sent).await;

    let done = finish(&server, &alice, &id).await;
    assert_eq!(done.status, 400);
    assert!(
        done.message_contains("checksum"),
        "the failure must name the reason: {}",
        String::from_utf8_lossy(&done.bytes)
    );

    // And the session is gone, not left holding bytes known to be wrong.
    let after = send(
        &server,
        Some(&alice),
        reqwest::Method::GET,
        &format!("/api/uploads/{id}"),
        None,
        None,
    )
    .await;
    assert_eq!(after.status, 404);
}

#[tokio::test]
async fn a_malformed_checksum_is_refused_before_any_bytes_are_sent() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ops").await;

    let body = serde_json::json!({
        "kind": "file",
        "roomId": room.as_str(),
        "filename": "x.bin",
        "size": 10,
        "sha256": "not-a-digest",
    });
    let r = send_typed(
        &server,
        Some(&alice),
        reqwest::Method::POST,
        "/api/uploads",
        Some(serde_json::to_vec(&body).unwrap()),
        None,
        Some("application/json"),
    )
    .await;
    assert_eq!(r.status, 400);
}

// --- ranges ---------------------------------------------------------------

#[tokio::test]
async fn a_download_can_be_resumed_with_a_range_request() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ops").await;

    let data = payload(100_000);
    let meta = upload_in_chunks(&server, &alice, &room, "ranged.bin", &data, 32_768).await;
    let id = meta["id"].as_str().unwrap();
    let path = format!("/api/files/{id}/raw");

    // The middle.
    let part = send(
        &server,
        Some(&alice),
        reqwest::Method::GET,
        &path,
        None,
        Some("bytes=1000-1999"),
    )
    .await;
    assert_eq!(part.status, 206);
    assert_eq!(part.bytes, &data[1000..2000]);
    assert_eq!(
        part.headers
            .get("content-range")
            .and_then(|v| v.to_str().ok()),
        Some(format!("bytes 1000-1999/{}", data.len()).as_str())
    );

    // Open-ended: what a resume after 60 000 bytes actually sends.
    let rest = send(
        &server,
        Some(&alice),
        reqwest::Method::GET,
        &path,
        None,
        Some("bytes=60000-"),
    )
    .await;
    assert_eq!(rest.status, 206);
    assert_eq!(rest.bytes, &data[60_000..]);

    // A suffix range is the *last* N bytes, not the first.
    let tail = send(
        &server,
        Some(&alice),
        reqwest::Method::GET,
        &path,
        None,
        Some("bytes=-500"),
    )
    .await;
    assert_eq!(tail.status, 206);
    assert_eq!(tail.bytes, &data[data.len() - 500..]);

    // Past the end is a 416 with the real length, not a silent whole file —
    // answering 200 here is how a resumed download corrupts itself.
    let bad = send(
        &server,
        Some(&alice),
        reqwest::Method::GET,
        &path,
        None,
        Some("bytes=999999-"),
    )
    .await;
    assert_eq!(bad.status, 416);
    assert_eq!(
        bad.headers
            .get("content-range")
            .and_then(|v| v.to_str().ok()),
        Some(format!("bytes */{}", data.len()).as_str())
    );
}

// --- capabilities ---------------------------------------------------------

#[tokio::test]
async fn a_download_token_opens_its_own_file_without_a_header() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ops").await;

    let data = payload(5_000);
    let meta = upload_in_chunks(&server, &alice, &room, "token.bin", &data, 4_096).await;
    let id = meta["id"].as_str().unwrap();

    let minted = send(
        &server,
        Some(&alice),
        reqwest::Method::POST,
        &format!("/api/files/{id}/download-token"),
        Some(Vec::new()),
        None,
    )
    .await;
    assert_eq!(minted.status, 200);
    let body = minted.json();
    assert_eq!(body["sha256"], sha256_hex(&data));
    let url = body["url"].as_str().unwrap().to_owned();

    // No Authorization header — this is what a browser navigation looks like.
    let anon = send(&server, None, reqwest::Method::GET, &url, None, None).await;
    assert_eq!(anon.status, 200);
    assert_eq!(anon.bytes, data);
}

#[tokio::test]
async fn a_download_token_is_useless_for_anything_else() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ops").await;

    let first = upload_in_chunks(&server, &alice, &room, "a.bin", &payload(2_000), 1_024).await;
    let second = upload_in_chunks(&server, &alice, &room, "b.bin", &payload(2_100), 1_024).await;

    let minted = send(
        &server,
        Some(&alice),
        reqwest::Method::POST,
        &format!(
            "/api/files/{}/download-token",
            first["id"].as_str().unwrap()
        ),
        Some(Vec::new()),
        None,
    )
    .await;
    let url = minted.json()["url"].as_str().unwrap().to_owned();
    let token = url.split("dl=").nth(1).unwrap().to_owned();

    // The same capability pointed at a different file.
    let swapped = send(
        &server,
        None,
        reqwest::Method::GET,
        &format!(
            "/api/files/{}/raw?dl={token}",
            second["id"].as_str().unwrap()
        ),
        None,
        None,
    )
    .await;
    assert_eq!(swapped.status, 401);

    // And it is not a login: it must not authenticate an ordinary API call.
    let as_bearer = server
        .client()
        .get(server.url("/api/rooms"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        as_bearer.status().as_u16(),
        401,
        "a download link must never be a session credential"
    );
}

#[tokio::test]
async fn someone_elses_session_is_invisible() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let mallory = new_user(&server, "mallory").await;
    let room = create_room(&alice.api, "ops").await;

    let data = payload(4_000);
    let started = begin(&server, &alice, &room, "private.bin", &data, true).await;
    let id = started.json()["id"].as_str().unwrap().to_owned();

    // A 404 rather than a 403: confirming the session exists would tell a
    // stranger that this wallet is uploading something.
    for (method, body) in [
        (reqwest::Method::GET, None),
        (reqwest::Method::PATCH, Some(vec![0u8; 16])),
        (reqwest::Method::DELETE, None),
    ] {
        let r = send(
            &server,
            Some(&mallory),
            method.clone(),
            &format!("/api/uploads/{id}?offset=0"),
            body,
            None,
        )
        .await;
        assert_eq!(r.status, 404, "{method} leaked a session");
    }
}

#[tokio::test]
async fn a_non_member_cannot_start_an_upload_into_a_room() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let mallory = new_user(&server, "mallory").await;
    let room = create_room(&alice.api, "ops").await;

    // Refused at `begin`, which is the only point at which refusing costs
    // nobody a transfer.
    let r = begin(
        &server,
        &mallory,
        &room,
        "intrusion.bin",
        &payload(100),
        true,
    )
    .await;
    assert!(
        r.status == 403 || r.status == 404,
        "expected the membership check to bite, got {}",
        r.status
    );
}

#[tokio::test]
async fn aborting_a_session_gives_the_disk_back() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ops").await;

    let data = payload(10_000);
    let started = begin(&server, &alice, &room, "abandoned.bin", &data, true).await;
    let id = started.json()["id"].as_str().unwrap().to_owned();
    append(&server, &alice, &id, 0, &data[..5_000]).await;

    let gone = send(
        &server,
        Some(&alice),
        reqwest::Method::DELETE,
        &format!("/api/uploads/{id}"),
        None,
        None,
    )
    .await;
    assert_eq!(gone.status, 204);

    let after = send(
        &server,
        Some(&alice),
        reqwest::Method::GET,
        &format!("/api/uploads/{id}"),
        None,
        None,
    )
    .await;
    assert_eq!(after.status, 404);
}

impl Raw {
    /// Does the error envelope mention this substring?
    fn message_contains(&self, needle: &str) -> bool {
        String::from_utf8_lossy(&self.bytes).contains(needle)
    }
}

// --- inline media ---------------------------------------------------------

#[tokio::test]
async fn a_video_is_served_as_playable_media_but_only_when_asked() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ops").await;

    // Not a real mp4 — the server never parses content, which is itself the
    // point: the type comes from the stored extension, not from sniffing.
    let data = payload(9_000);
    let meta = upload_in_chunks(&server, &alice, &room, "holiday.mp4", &data, 4_096).await;
    let id = meta["id"].as_str().unwrap();

    let plain = send(
        &server,
        Some(&alice),
        reqwest::Method::GET,
        &format!("/api/files/{id}/raw"),
        None,
        None,
    )
    .await;
    let ct = |r: &Raw| {
        r.headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned()
    };
    let cd = |r: &Raw| {
        r.headers
            .get("content-disposition")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned()
    };

    // The default is unchanged and still the safe one.
    assert_eq!(ct(&plain), "application/octet-stream");
    assert!(cd(&plain).starts_with("attachment"));

    let inline = send(
        &server,
        Some(&alice),
        reqwest::Method::GET,
        &format!("/api/files/{id}/raw?inline=1"),
        None,
        None,
    )
    .await;
    assert_eq!(inline.status, 200);
    assert_eq!(
        ct(&inline),
        "video/mp4",
        "a <video> handed octet-stream plays nothing"
    );
    assert!(cd(&inline).starts_with("inline"));
    // Still nosniff, and still resumable — seeking is Range requests.
    assert_eq!(
        inline
            .headers
            .get("x-content-type-options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );
    assert_eq!(
        inline
            .headers
            .get("accept-ranges")
            .and_then(|v| v.to_str().ok()),
        Some("bytes")
    );
}

#[tokio::test]
async fn inline_cannot_be_used_to_execute_an_upload_on_this_origin() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ops").await;

    // The whole reason attachments are octet-stream. Every one of these asks
    // to be served as something a browser would run or render as markup, and
    // `inline=1` must not grant any of them: the extension is not in the
    // allow-list, so the answer stays octet-stream + attachment.
    for name in [
        "payload.html",
        "payload.htm",
        "payload.svg",
        "payload.xml",
        "payload.js",
        "payload.mjs",
        "payload.pdf",
        "payload.xhtml",
    ] {
        let data = payload(1_200);
        let meta = upload_in_chunks(&server, &alice, &room, name, &data, 1_024).await;
        let id = meta["id"].as_str().unwrap();
        let r = send(
            &server,
            Some(&alice),
            reqwest::Method::GET,
            &format!("/api/files/{id}/raw?inline=1"),
            None,
            None,
        )
        .await;
        assert_eq!(
            r.headers.get("content-type").and_then(|v| v.to_str().ok()),
            Some("application/octet-stream"),
            "{name} was served as something other than octet-stream"
        );
        assert!(
            r.headers
                .get("content-disposition")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .starts_with("attachment"),
            "{name} was served inline"
        );
    }
}

// --- rate limiting --------------------------------------------------------

#[tokio::test]
async fn a_long_upload_is_not_throttled_into_failing() {
    // The general budget is 100 requests a minute, and a chunked upload makes
    // one request per chunk *by design* — so under that budget a 888 MB film
    // 429s partway through and stalls, which is precisely what happened to the
    // first one. Uploads draw on their own budget instead.
    //
    // 150 chunks: comfortably past the general limit, nowhere near the upload
    // one, and fast because each chunk is tiny. What is being tested is the
    // *count* of requests, not their size.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ops").await;

    let chunk = 64usize;
    let chunks = 150usize;
    let data = payload(chunk * chunks);

    let started = begin(&server, &alice, &room, "long.bin", &data, true).await;
    assert_eq!(started.status, 201);
    let id = started.json()["id"].as_str().unwrap().to_owned();

    for (i, piece) in data.chunks(chunk).enumerate() {
        let r = append(&server, &alice, &id, i * chunk, piece).await;
        assert_eq!(
            r.status, 200,
            "chunk {i} of {chunks} was refused with {} — the upload budget is \
             being shared with the general one again",
            r.status
        );
    }
    assert_eq!(finish(&server, &alice, &id).await.status, 201);
}
