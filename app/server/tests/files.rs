//! Attachments, end to end (`server/src/routes/files.rs`, `docs/API.md` §14).
//!
//! The unit tests in `db/files.rs` cover the storage contract and the ones in
//! `routes/files.rs` cover the extension guard as a property. What is left —
//! and what this file is for — is everything that only exists once a real
//! server, the auth extractor, the membership check and the filesystem are all
//! in play:
//!
//! * the round trip, including that the bytes come back byte-identical;
//! * that an attachment is exactly as private as its room, from four angles;
//! * that a hostile upload cannot become executable content on this origin;
//! * that hashtags in a caption reach search, scoped to members.
//!
//! Binary bodies go through `reqwest` directly rather than through `Api`: the
//! harness's `Resp` keeps the body as a `String`, which is lossy for bytes and
//! would quietly turn a round-trip assertion into a comparison of two
//! replacement-character soups.

mod common;

use common::*;

/// Not a real PDF. The server never parses content, which is itself worth
/// pinning: a store that sniffs is a store that can be lied to.
const BYTES: &[u8] = b"%PDF-1.7\nnot really a pdf, and that is the point\n";

/// Percent-encode a query value. Hand-rolled and total, so the harness is never
/// the thing deciding what the server gets to see.
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

/// Upload bytes as `user`, or anonymously when `user` is `None`.
async fn upload(
    server: &TestServer,
    user: Option<&User>,
    room: &str,
    filename: &str,
    caption: &str,
    bytes: &[u8],
) -> Raw {
    let url = server.url(&format!(
        "/api/rooms/{room}/files?filename={}&caption={}",
        q(filename),
        q(caption)
    ));
    let http = match user {
        Some(u) => u.api.http.clone(),
        None => server.client(),
    };
    let mut req = http.post(url).body(bytes.to_vec());
    if let Some(u) = user {
        req = req.header("Authorization", format!("Bearer {}", u.api.token()));
    }
    let resp = req.send().await.expect("upload request failed");
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    Raw {
        status,
        headers,
        bytes: resp.bytes().await.unwrap().to_vec(),
    }
}

/// GET a path, preserving the bytes exactly.
async fn get_raw(server: &TestServer, user: Option<&User>, path: &str) -> Raw {
    let http = match user {
        Some(u) => u.api.http.clone(),
        None => server.client(),
    };
    let mut req = http.get(server.url(path));
    if let Some(u) = user {
        req = req.header("Authorization", format!("Bearer {}", u.api.token()));
    }
    let resp = req.send().await.expect("download request failed");
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    Raw {
        status,
        headers,
        bytes: resp.bytes().await.unwrap().to_vec(),
    }
}

// --- the happy path -------------------------------------------------------

#[tokio::test]
async fn the_round_trip_returns_the_same_bytes_and_the_metadata_it_was_given() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ops").await;

    let up = upload(
        &server,
        Some(&alice),
        &room,
        "Q3 report.pdf",
        "the numbers #finance #urgent",
        BYTES,
    )
    .await;
    assert_eq!(up.status, 201, "{}", String::from_utf8_lossy(&up.bytes));
    let body = up.json();

    assert_eq!(body["filename"], "Q3 report.pdf");
    assert_eq!(body["caption"], "the numbers #finance #urgent");
    assert_eq!(body["sizeBytes"], BYTES.len() as i64);
    assert_eq!(body["uploader"], alice.address);
    assert_eq!(body["roomId"], room.as_str());
    // Tags are derived server-side so no client re-implements the rule.
    assert_eq!(body["tags"], serde_json::json!(["finance", "urgent"]));
    assert!(body["id"].as_str().is_some_and(|i| i.starts_with("file_")));

    // The filesystem name must never reach the wire.
    let raw = body.to_string();
    assert!(!raw.contains("storedName"), "{raw}");
    assert!(!raw.contains("stored_name"), "{raw}");

    let url = body["url"].as_str().expect("a url");
    let got = get_raw(&server, Some(&alice), url).await;
    assert_eq!(got.status, 200);
    assert_eq!(got.bytes, BYTES, "bytes must survive the round trip");
}

#[tokio::test]
async fn the_api_wide_body_limit_is_lifted_for_attachments() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ops").await;

    // 400 KB: over the 100 KB API-wide cap, far under the 25 MB attachment cap.
    // Without the route-local override this is a 413.
    let big = vec![7u8; 400 * 1024];
    let up = upload(&server, Some(&alice), &room, "big.bin", "", &big).await;
    assert_eq!(up.status, 201, "{}", String::from_utf8_lossy(&up.bytes));
    assert_eq!(up.json()["sizeBytes"], big.len() as i64);

    let url = up.json()["url"].as_str().unwrap().to_owned();
    let got = get_raw(&server, Some(&alice), &url).await;
    assert_eq!(got.status, 200);
    assert_eq!(got.bytes.len(), big.len());
    assert_eq!(got.bytes, big);
}

#[tokio::test]
async fn a_unicode_filename_survives_the_content_disposition_header() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ops").await;

    let up = upload(&server, Some(&alice), &room, "보고서.pdf", "#report", BYTES).await;
    assert_eq!(up.status, 201, "{}", String::from_utf8_lossy(&up.bytes));
    assert_eq!(up.json()["filename"], "보고서.pdf");

    let url = up.json()["url"].as_str().unwrap().to_owned();
    let got = get_raw(&server, Some(&alice), &url).await;
    assert_eq!(got.status, 200);

    // A raw non-ASCII byte is not a legal header value, so the header must stay
    // ASCII while still carrying the real name via RFC 5987.
    let disposition = got.headers["content-disposition"].to_str().unwrap();
    assert!(disposition.is_ascii(), "{disposition:?}");
    assert!(
        disposition.contains("filename*=UTF-8''%EB%B3%B4%EA%B3%A0%EC%84%9C.pdf"),
        "{disposition:?}"
    );
}

// --- the download must never be executable --------------------------------

#[tokio::test]
async fn a_download_is_never_executable_content_on_this_origin() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ops").await;

    // Something a browser would happily run if we let it choose the type.
    let html = b"<script>alert(document.domain)</script>";
    let up = upload(&server, Some(&alice), &room, "evil.html", "", html).await;
    assert_eq!(up.status, 201, "{}", String::from_utf8_lossy(&up.bytes));

    let url = up.json()["url"].as_str().unwrap().to_owned();
    let got = get_raw(&server, Some(&alice), &url).await;
    assert_eq!(got.status, 200);
    assert_eq!(got.bytes, html, "bytes are stored verbatim");

    // ...but served opaquely, with sniffing off, so it cannot execute here.
    assert_eq!(got.headers["content-type"], "application/octet-stream");
    assert_eq!(got.headers["x-content-type-options"], "nosniff");
    let disposition = got.headers["content-disposition"].to_str().unwrap();
    assert!(
        disposition.starts_with("attachment;"),
        "must be an attachment: {disposition:?}"
    );
    // Authorised per request, so a shared cache must not pass it on.
    assert!(
        got.headers["cache-control"]
            .to_str()
            .unwrap()
            .contains("private"),
        "{:?}",
        got.headers["cache-control"]
    );
}

// --- privacy --------------------------------------------------------------

#[tokio::test]
async fn an_attachment_is_exactly_as_private_as_its_room() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "alice only").await;

    let up = upload(
        &server,
        Some(&alice),
        &room,
        "secret.pdf",
        "#private",
        BYTES,
    )
    .await;
    assert_eq!(up.status, 201);
    let body = up.json();
    let url = body["url"].as_str().unwrap().to_owned();
    let id = body["id"].as_str().unwrap().to_owned();

    // 1. No token. Unlike /api/images, the hash is not a capability here.
    let got = get_raw(&server, None, &url).await;
    assert_eq!(got.status, 401);

    // 2. A valid token, but not a member. A 404 rather than a 403: a 403 would
    //    confirm the attachment exists to someone outside its room.
    let got = get_raw(&server, Some(&bob), &url).await;
    assert_eq!(got.status, 404);

    // 3. Metadata leaks nothing either...
    bob.api
        .get(&format!("/api/files/{id}"))
        .await
        .expect_status(404);

    // 4. ...and an unknown id is indistinguishable from a forbidden one.
    bob.api
        .get("/api/files/file_1_does-not-exist")
        .await
        .expect_status(404);
}

#[tokio::test]
async fn uploading_requires_membership_not_merely_a_token() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "alice only").await;

    let up = upload(&server, Some(&bob), &room, "x.pdf", "", BYTES).await;
    assert_eq!(up.status, 403, "{}", String::from_utf8_lossy(&up.bytes));

    let up = upload(&server, None, &room, "x.pdf", "", BYTES).await;
    assert_eq!(up.status, 401);
}

// --- input validation -----------------------------------------------------

#[tokio::test]
async fn a_hostile_filename_is_refused_rather_than_quietly_rewritten() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ops").await;

    for bad in [
        "../../etc/passwd",
        "sub/dir.pdf",
        "back\\slash.pdf",
        "quote\".pdf",
        "sneaky..pdf",
        "",
        "   ",
        // A CR would split the Content-Disposition header at download time, so
        // it has to die at the door rather than being escaped later.
        "a\r\nX-Injected: 1.pdf",
    ] {
        let up = upload(&server, Some(&alice), &room, bad, "", BYTES).await;
        assert_eq!(
            up.status,
            400,
            "accepted filename {bad:?}: {}",
            String::from_utf8_lossy(&up.bytes)
        );
        assert!(up.headers.get("x-injected").is_none());
    }
}

#[tokio::test]
async fn an_empty_body_is_refused() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ops").await;

    let up = upload(&server, Some(&alice), &room, "nothing.pdf", "", b"").await;
    assert_eq!(up.status, 400, "{}", String::from_utf8_lossy(&up.bytes));
}

// --- listing --------------------------------------------------------------

#[tokio::test]
async fn listing_is_scoped_newest_first_and_filterable_by_tag() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "ops").await;
    let other = create_room(&alice.api, "other").await;

    upload(&server, Some(&alice), &room, "a.pdf", "#finance", BYTES).await;
    upload(
        &server,
        Some(&alice),
        &room,
        "b.pdf",
        "#legal",
        b"other bytes",
    )
    .await;
    upload(
        &server,
        Some(&alice),
        &other,
        "elsewhere.pdf",
        "#finance",
        BYTES,
    )
    .await;

    let listed = alice
        .api
        .get(&format!("/api/rooms/{room}/files"))
        .await
        .expect_status(200)
        .json();
    let files = listed["files"].as_array().unwrap();
    assert_eq!(files.len(), 2, "only this room's files: {listed}");
    // Newest first.
    assert_eq!(files[0]["filename"], "b.pdf");

    // The tag filter, with and without the `#` a person actually types.
    for query in ["finance", "%23finance"] {
        let listed = alice
            .api
            .get(&format!("/api/rooms/{room}/files?tag={query}"))
            .await
            .expect_status(200)
            .json();
        let files = listed["files"].as_array().unwrap();
        assert_eq!(files.len(), 1, "tag={query}: {listed}");
        assert_eq!(files[0]["filename"], "a.pdf");
    }

    // A non-member cannot list at all.
    bob.api
        .get(&format!("/api/rooms/{room}/files"))
        .await
        .expect_status(403);
}

// --- search ---------------------------------------------------------------

#[tokio::test]
async fn a_caption_hashtag_makes_a_file_findable_but_only_to_members() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "ops").await;

    upload(
        &server,
        Some(&alice),
        &room,
        "invoice-q3.pdf",
        "quarterly #finance",
        BYTES,
    )
    .await;

    // The uploader finds it by tag...
    let hits = alice
        .api
        .get("/api/search?q=%23finance")
        .await
        .expect_status(200)
        .json();
    let results = hits["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "{hits}");
    assert_eq!(results[0]["kind"], "file");

    // ...and by filename, which is indexed alongside the caption so that an
    // untagged upload is still findable.
    let hits = alice
        .api
        .get("/api/search?q=invoice")
        .await
        .expect_status(200)
        .json();
    assert_eq!(hits["results"].as_array().unwrap().len(), 1, "{hits}");

    // A non-member finds nothing. This is the assertion that catches a new
    // kind being indexed without being added to the VISIBLE scope clause.
    for query in ["%23finance", "invoice"] {
        let hits = bob
            .api
            .get(&format!("/api/search?q={query}"))
            .await
            .expect_status(200)
            .json();
        assert!(
            hits["results"].as_array().unwrap().is_empty(),
            "a non-member saw a file for q={query}: {hits}"
        );
    }

    // `kind=file` is accepted now; nonsense still is not.
    let hits = alice
        .api
        .get("/api/search?q=invoice&kind=file")
        .await
        .expect_status(200)
        .json();
    assert_eq!(hits["results"].as_array().unwrap().len(), 1);
    alice
        .api
        .get("/api/search?q=x&kind=nonsense")
        .await
        .expect_status(400);
}

// --- deletion -------------------------------------------------------------

#[tokio::test]
async fn deletion_is_the_uploader_or_an_admin_and_nobody_else() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "ops").await;
    add_member(&alice.api, &bob, &room).await;

    let up = upload(&server, Some(&alice), &room, "alice.pdf", "", BYTES).await;
    let id = up.json()["id"].as_str().unwrap().to_owned();

    // A member who is neither uploader nor admin is refused — and gets a 403,
    // not a 404, because he can legitimately see that the file exists.
    bob.api
        .delete(&format!("/api/files/{id}"))
        .await
        .expect_status(403);

    // The uploader may.
    alice
        .api
        .delete(&format!("/api/files/{id}"))
        .await
        .expect_status(200);

    // Gone from metadata, download and a second delete alike.
    alice
        .api
        .get(&format!("/api/files/{id}"))
        .await
        .expect_status(404);
    let got = get_raw(&server, Some(&alice), &format!("/api/files/{id}/raw")).await;
    assert_eq!(got.status, 404);
    alice
        .api
        .delete(&format!("/api/files/{id}"))
        .await
        .expect_status(404);

    // And gone from search, so a deleted attachment cannot be found by its tag.
    let hits = alice
        .api
        .get("/api/search?q=alice")
        .await
        .expect_status(200)
        .json();
    assert!(hits["results"].as_array().unwrap().is_empty(), "{hits}");
}

#[tokio::test]
async fn an_admin_can_remove_someone_elses_attachment() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "ops").await;
    add_member(&alice.api, &bob, &room).await;

    // Bob uploads; Alice, the creator and therefore an admin, removes it.
    let up = upload(&server, Some(&bob), &room, "bob.pdf", "", BYTES).await;
    assert_eq!(up.status, 201, "{}", String::from_utf8_lossy(&up.bytes));
    let id = up.json()["id"].as_str().unwrap().to_owned();

    alice
        .api
        .delete(&format!("/api/files/{id}"))
        .await
        .expect_status(200);
}

#[tokio::test]
async fn identical_bytes_are_stored_once_but_are_two_independent_attachments() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "ops").await;

    let first = upload(&server, Some(&alice), &room, "a.pdf", "#one", BYTES)
        .await
        .json();
    let second = upload(&server, Some(&alice), &room, "b.pdf", "#two", BYTES)
        .await
        .json();

    // Two rows, two ids, two urls — the sharing on disk is invisible outside.
    assert_ne!(first["id"], second["id"]);
    assert_ne!(first["url"], second["url"]);

    // Deleting one must not break the other's download. This is the test that
    // fails if the delete path ever unlinks the shared bytes.
    let id = first["id"].as_str().unwrap();
    alice
        .api
        .delete(&format!("/api/files/{id}"))
        .await
        .expect_status(200);

    let url = second["url"].as_str().unwrap();
    let got = get_raw(&server, Some(&alice), url).await;
    assert_eq!(got.status, 200, "the survivor must still download");
    assert_eq!(got.bytes, BYTES);
}

#[tokio::test]
async fn deleting_a_room_takes_its_attachments_with_it() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "doomed").await;

    let up = upload(&server, Some(&alice), &room, "doomed.pdf", "#gone", BYTES).await;
    let id = up.json()["id"].as_str().unwrap().to_owned();

    alice
        .api
        .delete(&format!("/api/rooms/{room}"))
        .await
        .expect_status(200);

    alice
        .api
        .get(&format!("/api/files/{id}"))
        .await
        .expect_status(404);
}
