//! Destroying a room, end to end (`server/src/purge.rs`, `docs/API.md` §6.5.5).
//!
//! The unit tests in `purge.rs` drive the purge directly and the ones in
//! `db/media.rs` cover the reference rules. What only exists with a real server
//! in play — and what this file is for — is the promise as a *user* can check
//! it: after an admin destroys a room, the URLs it showed stop answering.
//!
//! That is the whole point of the feature. A picture under `data/images/` is
//! named by the SHA-256 of its bytes and served to anyone holding the URL, with
//! no room membership involved, so a room deleted only in the database is still
//! readable by precisely the people the deletion was meant to cut off. These
//! tests are written against the URL rather than the disk for that reason: they
//! ask the question an ex-member with a saved link would ask.
//!
//! The counterweight is here too: destroying one room must not reach into
//! another one's pictures, or into somebody's avatar. Content-addressed storage
//! means those are literally the same file.

mod common;

use common::*;
use serde_json::json;

/// A 1×1 transparent PNG — the smallest real image there is.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

/// A second image, so a test can hold two distinct hosted files. One byte of
/// trailing padding is enough: the name is the hash of the content.
fn other_png() -> Vec<u8> {
    let mut bytes = PNG.to_vec();
    bytes.push(0x00);
    bytes
}

/// Host bytes and return the `/api/images/…` URL the server answers with.
async fn host_image(server: &TestServer, user: &User, bytes: &[u8]) -> String {
    let resp = user
        .api
        .http
        .post(server.url("/api/images"))
        .header("Authorization", format!("Bearer {}", user.api.token()))
        .header("Content-Type", "image/png")
        .body(bytes.to_vec())
        .send()
        .await
        .expect("image upload failed");
    assert_eq!(resp.status().as_u16(), 200, "hosting should have worked");
    let body: serde_json::Value = resp.json().await.unwrap();
    body["url"].as_str().expect("a url").to_owned()
}

/// GET a hosted URL with no credentials — the way a saved link is opened.
async fn fetch_anonymously(server: &TestServer, url: &str) -> u16 {
    server
        .client()
        .get(server.url(url))
        .send()
        .await
        .expect("image fetch failed")
        .status()
        .as_u16()
}

/// The `{sha256}.{ext}` tail of a hosted URL — what a client declares.
fn name_of(url: &str) -> &str {
    url.rsplit('/').next().expect("a filename")
}

#[tokio::test]
async fn destroying_a_room_stops_the_pictures_it_showed_from_answering() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "doomed").await;

    let url = host_image(&server, &alice, PNG).await;
    send_message(&alice.api, &room, &format!("here it is: {url}")).await;
    assert_eq!(
        fetch_anonymously(&server, &url).await,
        200,
        "the picture serves before the room is destroyed"
    );

    alice
        .api
        .delete(&format!("/api/rooms/{room}"))
        .await
        .expect_status(200);

    // The URL is the only access control hosted media has, so this 404 is the
    // entire difference between "the room is deleted" and "the room is gone".
    assert_eq!(
        fetch_anonymously(&server, &url).await,
        404,
        "a saved link must stop working when the room it came from is destroyed"
    );
}

#[tokio::test]
async fn the_response_reports_what_it_erased() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "doomed").await;

    let url = host_image(&server, &alice, PNG).await;
    send_message(&alice.api, &room, &url).await;

    let body = alice
        .api
        .delete(&format!("/api/rooms/{room}"))
        .await
        .expect_ok();

    assert_eq!(s(&body, "message"), "Room deleted successfully");
    assert_eq!(body["purged"]["media"], 1);
    assert_eq!(body["purged"]["attachments"], 0);
    assert_eq!(
        body["purged"]["failed"], 0,
        "a file that could not be unlinked is reported, never silent"
    );
}

#[tokio::test]
async fn a_picture_only_an_encrypted_message_named_is_erased_too() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "sealed").await;

    // The server cannot read this message: the content is ciphertext as far as
    // it is concerned. The declaration is the only thing tying the picture to
    // the room — which is exactly the case `media` exists for.
    let url = host_image(&server, &alice, PNG).await;
    alice
        .api
        .post(
            &format!("/api/rooms/{room}/messages"),
            json!({
                "content": "b3BhcXVlIGNpcGhlcnRleHQ=",
                "msgHash": crypto::sha256_hex(b"b3BhcXVlIGNpcGhlcnRleHQ="),
                "isEncrypted": true,
                "iv": "0".repeat(32),
                "hmac": "f".repeat(64),
                "encVer": 2,
                "keyVersion": 1,
                "media": [name_of(&url)],
            }),
        )
        .await
        .expect_ok();

    alice
        .api
        .delete(&format!("/api/rooms/{room}"))
        .await
        .expect_status(200);

    assert_eq!(
        fetch_anonymously(&server, &url).await,
        404,
        "an encrypted room must be able to forget its pictures too"
    );
}

#[tokio::test]
async fn a_picture_another_room_still_shows_survives() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let doomed = create_room(&alice.api, "doomed").await;
    let keeper = create_room(&alice.api, "keeper").await;

    // One file, two rooms — which is what content-addressed storage means, and
    // the case a purge could get catastrophically wrong.
    let shared = host_image(&server, &alice, PNG).await;
    let only_here = host_image(&server, &alice, &other_png()).await;
    send_message(&alice.api, &doomed, &shared).await;
    send_message(&alice.api, &doomed, &only_here).await;
    send_message(&alice.api, &keeper, &shared).await;

    let body = alice
        .api
        .delete(&format!("/api/rooms/{doomed}"))
        .await
        .expect_ok();

    assert_eq!(body["purged"]["media"], 1, "only the unshared one goes");
    assert_eq!(
        fetch_anonymously(&server, &shared).await,
        200,
        "the other room is still showing this"
    );
    assert_eq!(fetch_anonymously(&server, &only_here).await, 404);
}

#[tokio::test]
async fn an_avatar_is_not_collateral() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "doomed").await;

    // Avatars are hosted the same way and in the same directory, so a purge
    // that only counted messages would take somebody's face off the server.
    let avatar = host_image(&server, &alice, PNG).await;
    alice
        .api
        .put(
            "/api/auth/profile",
            json!({ "username": "alice", "profileImage": avatar }),
        )
        .await
        .expect_status(200);
    send_message(&alice.api, &room, &format!("that's me: {avatar}")).await;

    alice
        .api
        .delete(&format!("/api/rooms/{room}"))
        .await
        .expect_status(200);

    assert_eq!(
        fetch_anonymously(&server, &avatar).await,
        200,
        "a profile picture outlives any one room that showed it"
    );
}

#[tokio::test]
async fn a_declared_name_can_only_ever_be_a_hosted_filename() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let room = create_room(&alice.api, "hostile").await;

    // The purge joins these onto a directory, so the grammar check is a
    // traversal guard. Refused, not dropped: a silently ignored declaration is
    // a file that outlives the room that showed it.
    for hostile in [
        "../../jwt.secret.png",
        "/etc/passwd",
        "pocketskynet.db",
        &format!("{}.exe", "a".repeat(64)),
        &format!("{}.png", "a".repeat(63)),
    ] {
        alice
            .api
            .post(
                &format!("/api/rooms/{room}/messages"),
                json!({
                    "content": "hello",
                    "msgHash": crypto::sha256_hex(b"hello"),
                    "media": [hostile],
                }),
            )
            .await
            .expect_status(400);
    }
}

#[tokio::test]
async fn only_an_admin_can_destroy_a_room_and_its_files() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let room = create_room(&alice.api, "shared").await;
    add_member(&alice.api, &bob, &room).await;

    let url = host_image(&server, &alice, PNG).await;
    send_message(&alice.api, &room, &url).await;

    // The purge reaches the filesystem, so the authorization in front of it is
    // load-bearing in a way an ordinary delete's is not.
    bob.api
        .delete(&format!("/api/rooms/{room}"))
        .await
        .expect_status(403);

    assert_eq!(
        fetch_anonymously(&server, &url).await,
        200,
        "a refused destroy must not have erased anything"
    );
}

#[tokio::test]
async fn an_edit_that_removes_a_picture_stops_keeping_it_alive() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let doomed = create_room(&alice.api, "doomed").await;
    let keeper = create_room(&alice.api, "keeper").await;

    // `keeper` showed the picture and then edited it away. If the edit left the
    // reference behind, destroying `doomed` would leave the file on disk
    // forever — kept alive by a message that no longer shows it.
    let url = host_image(&server, &alice, PNG).await;
    send_message(&alice.api, &doomed, &url).await;
    let stale = send_message(&alice.api, &keeper, &url).await;
    let id = s(&stale, "id");
    alice
        .api
        .patch(
            &format!("/api/messages/{id}"),
            json!({ "content": "never mind", "msgHash": crypto::sha256_hex(b"never mind") }),
        )
        .await
        .expect_status(200);

    alice
        .api
        .delete(&format!("/api/rooms/{doomed}"))
        .await
        .expect_status(200);

    assert_eq!(fetch_anonymously(&server, &url).await, 404);
}
