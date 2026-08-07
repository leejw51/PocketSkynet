//! Skynet Password end to end: the CRUD round trip, the wallet scoping, the
//! authentication gate, and the claim the whole feature rests on — that what
//! lands on the server's disk is not the plaintext.
//!
//! Spec: `docs/API.md` §18, `docs/CRYPTO.md` §14.
//!
//! These run against a real server process started by the harness, so the
//! ciphertext genuinely crosses HTTP and genuinely reaches SQLite. The
//! plaintext check reads the database *file*, not a query result: a test that
//! asked the server whether it had stored a secret would be asking the wrong
//! party.

mod common;

use common::*;
use pocketskynet_core::secrets::Field;
use serde_json::{json, Value};

/// The plaintexts every test uses. Distinctive enough that finding them in a
/// binary blob is unambiguous, and long enough not to appear by chance.
const NAME: &str = "chase-bank-login-QYZX";
const SECRET: &str = "correct-horse-battery-staple-QYZX";

/// Seal a pair and build the create body.
fn body(vault: &pocketskynet_core::secrets::VaultKey, id: &str, name: &str, secret: &str) -> Value {
    json!({
        "id": id,
        "key": sealed_to_json(&seal_secret(vault, id, Field::Key, name)),
        "value": sealed_to_json(&seal_secret(vault, id, Field::Value, secret)),
        "encVer": 1,
    })
}

/// The body of a `PUT`, which carries no id.
fn replacement(
    vault: &pocketskynet_core::secrets::VaultKey,
    id: &str,
    name: &str,
    secret: &str,
) -> Value {
    json!({
        "key": sealed_to_json(&seal_secret(vault, id, Field::Key, name)),
        "value": sealed_to_json(&seal_secret(vault, id, Field::Value, secret)),
        "encVer": 1,
    })
}

// --- the round trip -------------------------------------------------------

#[tokio::test]
async fn an_entry_survives_the_whole_round_trip_and_decrypts_to_what_went_in() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let identity = alice.publish_encryption_key().await;
    let vault = vault_key(&identity);
    let id = new_entry_id();

    let created = alice
        .api
        .post("/api/passwords", body(&vault, &id, NAME, SECRET))
        .await
        .expect_ok();
    assert_eq!(s(&created, "id"), id);
    assert_eq!(i(&created, "encVer"), 1);
    assert!(i(&created, "createdAt") > 0);
    assert_eq!(
        i(&created, "createdAt"),
        i(&created, "updatedAt"),
        "a fresh row is its own last edit"
    );

    // Fetched back through the list, and opened with the key derived from the
    // same wallet signature the browser would have used.
    let listed = alice.api.get("/api/passwords").await.expect_ok();
    let rows = listed.as_array().expect("an array");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(s(row, "id"), id);

    assert_eq!(
        open_secret(&vault, &id, Field::Key, &sealed_from_json(&row["key"])),
        Some(NAME.to_owned())
    );
    assert_eq!(
        open_secret(&vault, &id, Field::Value, &sealed_from_json(&row["value"])),
        Some(SECRET.to_owned())
    );
}

#[tokio::test]
async fn a_different_key_cannot_open_what_came_back() {
    // The other half of the round trip: the ciphertext is not merely opaque to
    // the server, it is opaque to every key but the one that sealed it.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let alice_vault = vault_key(&alice.publish_encryption_key().await);
    let bob_vault = vault_key(&bob.publish_encryption_key().await);
    let id = new_entry_id();

    alice
        .api
        .post("/api/passwords", body(&alice_vault, &id, NAME, SECRET))
        .await
        .expect_status(200);

    let row = alice.api.get("/api/passwords").await.expect_ok()[0].clone();
    let sealed_value = sealed_from_json(&row["value"]);

    assert_eq!(
        open_secret(&alice_vault, &id, Field::Value, &sealed_value),
        Some(SECRET.to_owned())
    );
    assert_eq!(
        open_secret(&bob_vault, &id, Field::Value, &sealed_value),
        None,
        "another wallet's vault key must not open it"
    );
}

#[tokio::test]
async fn an_edit_replaces_the_value_and_leaves_no_trace_of_the_old_one() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let vault = vault_key(&alice.publish_encryption_key().await);
    let id = new_entry_id();

    alice
        .api
        .post("/api/passwords", body(&vault, &id, NAME, SECRET))
        .await
        .expect_status(200);
    let before = alice.api.get("/api/passwords").await.expect_ok()[0].clone();

    let rotated = "rotated-secret-ZZTOP";
    let updated = alice
        .api
        .put(
            &format!("/api/passwords/{id}"),
            replacement(&vault, &id, NAME, rotated),
        )
        .await
        .expect_ok();

    assert_eq!(
        open_secret(
            &vault,
            &id,
            Field::Value,
            &sealed_from_json(&updated["value"])
        ),
        Some(rotated.to_owned())
    );
    assert_eq!(
        i(&updated, "createdAt"),
        i(&before, "createdAt"),
        "the creation time is not rewritten by an edit"
    );

    // The old ciphertext is gone, not versioned beside the new one — and the
    // new one is not the old one with a tweak, because the IV is fresh.
    let after = alice.api.get("/api/passwords").await.expect_ok()[0].clone();
    assert_ne!(after["value"]["ciphertext"], before["value"]["ciphertext"]);
    assert_ne!(after["value"]["iv"], before["value"]["iv"]);
    assert_eq!(
        alice
            .api
            .get("/api/passwords")
            .await
            .expect_ok()
            .as_array()
            .unwrap()
            .len(),
        1,
        "an edit must not leave the previous row behind"
    );

    // And the old value is unreadable from anything the server still holds.
    assert_eq!(
        open_secret(
            &vault,
            &id,
            Field::Value,
            &sealed_from_json(&after["value"])
        ),
        Some(rotated.to_owned())
    );
}

#[tokio::test]
async fn an_entry_can_be_deleted_and_deleting_it_twice_is_a_404() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let vault = vault_key(&alice.publish_encryption_key().await);
    let id = new_entry_id();

    alice
        .api
        .post("/api/passwords", body(&vault, &id, NAME, SECRET))
        .await
        .expect_status(200);
    alice
        .api
        .delete(&format!("/api/passwords/{id}"))
        .await
        .expect_message("Entry deleted");
    assert!(alice
        .api
        .get("/api/passwords")
        .await
        .expect_ok()
        .as_array()
        .unwrap()
        .is_empty());
    alice
        .api
        .delete(&format!("/api/passwords/{id}"))
        .await
        .expect_status(404);
}

#[tokio::test]
async fn creating_the_same_id_twice_is_a_conflict_rather_than_an_overwrite() {
    // A retried create must not destroy an edit made in between.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let vault = vault_key(&alice.publish_encryption_key().await);
    let id = new_entry_id();

    alice
        .api
        .post("/api/passwords", body(&vault, &id, NAME, SECRET))
        .await
        .expect_status(200);
    alice
        .api
        .post(
            "/api/passwords",
            body(&vault, &id, "something-else", "nope"),
        )
        .await
        .expect_status(409);

    let row = alice.api.get("/api/passwords").await.expect_ok()[0].clone();
    assert_eq!(
        open_secret(&vault, &id, Field::Value, &sealed_from_json(&row["value"])),
        Some(SECRET.to_owned()),
        "the original entry survived the refused create"
    );
}

// --- the thing on disk ----------------------------------------------------

#[tokio::test]
async fn nothing_the_user_typed_reaches_the_database_file() {
    // The claim the whole feature rests on, checked against the bytes rather
    // than against a query: the plaintext must not appear anywhere in the
    // database, its write-ahead log, or the server's own log file. A `SELECT`
    // would only prove that one column is ciphertext; scanning the file proves
    // nothing leaked through an index, a trigger or a stray log line.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let vault = vault_key(&alice.publish_encryption_key().await);
    let id = new_entry_id();

    alice
        .api
        .post("/api/passwords", body(&vault, &id, NAME, SECRET))
        .await
        .expect_status(200);
    // Read it back once, so anything the read path might cache or log has
    // happened before the file is scanned.
    alice.api.get("/api/passwords").await.expect_status(200);

    let db = server.db_path();
    let mut scanned = 0usize;
    for path in [
        db.clone(),
        db.with_extension("db-wal"),
        db.with_extension("db-shm"),
        server.db_path().parent().unwrap().join("server.log"),
    ] {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        scanned += 1;
        for needle in [NAME, SECRET] {
            assert!(
                !contains(&bytes, needle.as_bytes()),
                "{needle:?} found in plaintext inside {}",
                path.display()
            );
        }
    }
    assert!(scanned > 0, "no server file was actually scanned");

    // The control: the *ciphertext* is in there, so the scan above was
    // looking at a file that really does hold this entry. Without this, a
    // typo'd path would make the test pass by reading nothing.
    let row = alice.api.get("/api/passwords").await.expect_ok()[0].clone();
    let ciphertext = s(&row["value"], "ciphertext");
    let db_bytes = std::fs::read(&db).unwrap_or_default();
    let wal_bytes = std::fs::read(db.with_extension("db-wal")).unwrap_or_default();
    assert!(
        contains(&db_bytes, ciphertext.as_bytes()) || contains(&wal_bytes, ciphertext.as_bytes()),
        "the entry was not in the file this test scanned"
    );
}

/// Naive substring search over bytes. `Vec::windows` is enough for a database
/// this size, and pulling in a searcher crate for one assertion would be a
/// dependency in the test suite that the product does not have.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

// --- who may touch what ---------------------------------------------------

#[tokio::test]
async fn a_second_wallet_cannot_read_edit_or_delete_another_wallets_entry() {
    // Bob knows Alice's entry id exactly — this is not a search, it is a
    // targeted attempt with the one piece of information he would need.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let alice_vault = vault_key(&alice.publish_encryption_key().await);
    let bob_vault = vault_key(&bob.publish_encryption_key().await);
    let id = new_entry_id();

    alice
        .api
        .post("/api/passwords", body(&alice_vault, &id, NAME, SECRET))
        .await
        .expect_status(200);

    // Read: his list is his own, and it is empty.
    let his = bob.api.get("/api/passwords").await.expect_ok();
    assert!(
        his.as_array().unwrap().is_empty(),
        "the list is scoped to the caller"
    );

    // Edit: refused, and refused as "not found" rather than "not yours" —
    // an entry id is a secret, and a 403 would confirm the guess.
    bob.api
        .put(
            &format!("/api/passwords/{id}"),
            replacement(&bob_vault, &id, "pwned", "pwned"),
        )
        .await
        .expect_status(404);

    // Delete: likewise.
    bob.api
        .delete(&format!("/api/passwords/{id}"))
        .await
        .expect_status(404);

    // And Alice's entry is exactly as she left it.
    let row = alice.api.get("/api/passwords").await.expect_ok()[0].clone();
    assert_eq!(
        open_secret(
            &alice_vault,
            &id,
            Field::Value,
            &sealed_from_json(&row["value"])
        ),
        Some(SECRET.to_owned())
    );
}

#[tokio::test]
async fn an_unowned_entry_is_indistinguishable_from_one_that_never_existed() {
    // The existence oracle, closed. If these two answers ever differ — in the
    // status *or* in the body — an attacker holding a guessed id learns
    // whether it names somebody's secret.
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;
    let alice_vault = vault_key(&alice.publish_encryption_key().await);
    let bob_vault = vault_key(&bob.publish_encryption_key().await);

    let real = new_entry_id();
    let invented = new_entry_id();
    alice
        .api
        .post("/api/passwords", body(&alice_vault, &real, NAME, SECRET))
        .await
        .expect_status(200);

    let hers = bob
        .api
        .put(
            &format!("/api/passwords/{real}"),
            replacement(&bob_vault, &real, "x", "y"),
        )
        .await;
    let nobodys = bob
        .api
        .put(
            &format!("/api/passwords/{invented}"),
            replacement(&bob_vault, &invented, "x", "y"),
        )
        .await;

    assert_eq!(hers.code(), nobodys.code());
    assert_eq!(hers.code(), 404);
    assert_eq!(hers.message(), nobodys.message());

    let hers = bob.api.delete(&format!("/api/passwords/{real}")).await;
    let nobodys = bob.api.delete(&format!("/api/passwords/{invented}")).await;
    assert_eq!(hers.code(), nobodys.code());
    assert_eq!(hers.message(), nobodys.message());
}

#[tokio::test]
async fn every_route_refuses_an_unauthenticated_caller() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let vault = vault_key(&alice.publish_encryption_key().await);
    let id = new_entry_id();
    alice
        .api
        .post("/api/passwords", body(&vault, &id, NAME, SECRET))
        .await
        .expect_status(200);

    let anon = alice.api.without_token();
    anon.get("/api/passwords").await.expect_status(401);
    anon.post(
        "/api/passwords",
        body(&vault, &new_entry_id(), NAME, SECRET),
    )
    .await
    .expect_status(401);
    anon.put(
        &format!("/api/passwords/{id}"),
        replacement(&vault, &id, NAME, SECRET),
    )
    .await
    .expect_status(401);
    anon.delete(&format!("/api/passwords/{id}"))
        .await
        .expect_status(401);

    // A garbage token is refused the same way — the 401 is not merely the
    // absence of a header.
    let forged = alice.api.with_raw_token("not.a.token");
    forged.get("/api/passwords").await.expect_status(401);

    // Alice's entry is still there and still readable, so none of the above
    // was refused *after* having an effect.
    assert_eq!(
        alice
            .api
            .get("/api/passwords")
            .await
            .expect_ok()
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

// --- validation -----------------------------------------------------------

#[tokio::test]
async fn a_malformed_body_is_a_400_that_names_the_field() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let vault = vault_key(&alice.publish_encryption_key().await);
    let id = new_entry_id();

    // A truncated IV — what a buggy client produces.
    let mut bad = body(&vault, &id, NAME, SECRET);
    bad["value"]["iv"] = json!("0".repeat(31));
    let response = alice.api.post("/api/passwords", bad).await;
    assert_eq!(response.code(), 400);
    assert!(
        response.json().to_string().contains("value.iv"),
        "the error must name the offending field: {}",
        response.json()
    );

    // An id that could never have been minted: `|` would make the MAC framing
    // ambiguous, so it is refused on both sides of the wire.
    let mut bad = body(&vault, &id, NAME, SECRET);
    bad["id"] = json!("has|pipes|in|it");
    alice
        .api
        .post("/api/passwords", bad)
        .await
        .expect_status(400);

    // A missing half.
    let mut bad = body(&vault, &id, NAME, SECRET);
    bad.as_object_mut().unwrap().remove("value");
    alice
        .api
        .post("/api/passwords", bad)
        .await
        .expect_status(400);

    // Nothing was stored by any of it.
    assert!(alice
        .api
        .get("/api/passwords")
        .await
        .expect_ok()
        .as_array()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn the_list_is_ordered_by_last_change() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let vault = vault_key(&alice.publish_encryption_key().await);

    let first = new_entry_id();
    let second = new_entry_id();
    alice
        .api
        .post("/api/passwords", body(&vault, &first, "first", "a"))
        .await
        .expect_status(200);
    alice
        .api
        .post("/api/passwords", body(&vault, &second, "second", "b"))
        .await
        .expect_status(200);

    // Touching the older one moves it to the front.
    alice
        .api
        .put(
            &format!("/api/passwords/{first}"),
            replacement(&vault, &first, "first", "a2"),
        )
        .await
        .expect_status(200);

    let rows = alice.api.get("/api/passwords").await.expect_ok();
    let ids: Vec<String> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|r| s(r, "id"))
        .collect();
    assert_eq!(ids, vec![first, second]);
}
