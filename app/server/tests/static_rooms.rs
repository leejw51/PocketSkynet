//! The three built-in rooms, end to end: My Note, My Jarvis, My Lobby.
//!
//! The unit tests in `db/rooms.rs` drive provisioning directly and prove the
//! rosters reconcile. What only exists with a real server in play — and what
//! this file is for — is the promise as a *user* can check it.
//!
//! Three of those promises are worth the cost of booting a process:
//!
//! * The rooms simply **are there**. Nobody creates them and there is no
//!   endpoint that would; the only way to see whether provisioning fires at the
//!   right moment is to log in and ask for the room list, which is exactly what
//!   a client does.
//! * "My Note" is **unreadable by anyone else**, and the interesting half of
//!   that is not the 403 — it is that a second wallet cannot manufacture a way
//!   in. So the tests walk every door: invite, invite link, admin promotion,
//!   key wrapping, and the note's own derived id guessed from the victim's
//!   address, which is public.
//! * They **cannot be removed** but **can be hidden**. Those are the two halves
//!   of one design decision, and testing either alone would let the other
//!   regress into "the button is gone from the UI".
//!
//! Search is here rather than in `search/store.rs` for the same reason: the
//! scoping rule it relies on is room membership, which is only real once a
//! server has provisioned a room and indexed a message into it.

mod common;

use common::*;
use serde_json::{json, Value};

/// The id the server derives for one person's built-in room.
///
/// Recomputed here rather than read out of the room list on purpose. It is the
/// value an *attacker* can compute — a wallet address is public — so a test
/// that fetched the id from the owner's own listing would be proving the wrong
/// thing about the doors it then tries.
fn static_room_id(kind: &str, owner: &str) -> String {
    format!("room_{kind}_{}", owner.to_lowercase())
}

/// The caller's rooms, keyed by kind.
///
/// Fetching the room list is also what *provisions* the built-in rooms
/// (`routes/rooms.rs::list` explains why that is the hook rather than sign-in),
/// so this doubles as the "make them exist" step below.
async fn rooms_by_kind(user: &User) -> std::collections::HashMap<String, Value> {
    user.api
        .get("/api/rooms")
        .await
        .array()
        .into_iter()
        .map(|room| (s(&room, "kind"), room))
        .collect()
}

/// Do what every client does on start: ask for the room list.
///
/// Spelled out as its own call rather than folded into `new_user` because it
/// *is* the design under test. These rooms are provisioned lazily, by the one
/// request that needs them to exist, and a harness that quietly provisioned
/// them at login would be testing a server nobody ships.
async fn open_the_app(user: &User) {
    user.api.get("/api/rooms").await.expect_status(200);
}

// --- provisioning ---------------------------------------------------------

#[tokio::test]
async fn signing_in_and_asking_for_the_room_list_is_all_it_takes() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;

    // No create call anywhere in this test. That is the feature.
    let rooms = rooms_by_kind(&alice).await;

    assert_eq!(rooms.len(), 3, "exactly the three built-in rooms: {rooms:?}");
    for kind in ["note", "jarvis", "lobby"] {
        let room = rooms
            .get(kind)
            .unwrap_or_else(|| panic!("no room of kind {kind}: {rooms:?}"));
        expect_room_shape(room);
        assert_eq!(s(room, "id"), static_room_id(kind, &alice.address));
        assert!(
            !b(room, "hasEncryption"),
            "{kind} is plaintext so its contents stay searchable"
        );
    }

    // The note is alone; the lobby holds the owner because this deployment
    // configures no admins at all (the harness scrubs VITE_FRUITNATION_ADMIN),
    // which is the honest answer rather than a failure.
    assert_eq!(i(&rooms["note"], "memberCount"), 1);
    assert_eq!(i(&rooms["lobby"], "memberCount"), 1);
    // My Jarvis holds the owner *and* the agent, which is what makes it a
    // conversation rather than a notepad with a button.
    assert_eq!(i(&rooms["jarvis"], "memberCount"), 2);
    let agent = rooms["jarvis"]["members"]
        .as_array()
        .expect("members")
        .iter()
        .find(|m| s(m, "userAddress") != alice.address)
        .expect("the agent is on the roster");
    assert!(
        s(agent, "userAddress").starts_with("0x000000a1"),
        "the agent sits under its own reserved prefix: {agent}"
    );
    assert_eq!(s(&agent["user"], "username"), "Jarvis");
}

#[tokio::test]
async fn two_people_get_two_sets_that_never_touch() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let bob = new_user(&server, "bob").await;

    let hers = rooms_by_kind(&alice).await;
    let his = rooms_by_kind(&bob).await;

    for kind in ["note", "jarvis", "lobby"] {
        assert_ne!(
            s(&hers[kind], "id"),
            s(&his[kind], "id"),
            "{kind} must be one room per person"
        );
    }
    // And the agents are distinct too — one shared Jarvis face on a family
    // server would read as everybody talking to the same machine.
    let agent_of = |rooms: &std::collections::HashMap<String, Value>, me: &str| {
        rooms["jarvis"]["members"]
            .as_array()
            .expect("members")
            .iter()
            .map(|m| s(m, "userAddress"))
            .find(|a| a != me)
            .expect("an agent")
    };
    assert_ne!(
        agent_of(&hers, &alice.address),
        agent_of(&his, &bob.address)
    );
}

#[tokio::test]
async fn the_lobby_seats_the_servers_administrators() {
    // The admin list is configuration, so the wallet has to exist before the
    // server does — hence a signer minted up front and logged in afterwards.
    let boss_signer = crypto::Signer::random();
    let boss_address = boss_signer.address().to_lowercase();
    let server =
        TestServer::start_with_env(&[("VITE_FRUITNATION_ADMIN", boss_address.as_str())]).await;

    let boss = login(&server, boss_signer, "boss").await;
    let alice = new_user(&server, "alice").await;
    open_the_app(&alice).await;
    open_the_app(&boss).await;

    let lobby = static_room_id("lobby", &alice.address);
    let members: Vec<String> = alice
        .api
        .get(&format!("/api/rooms/{lobby}/members"))
        .await
        .array()
        .iter()
        .map(|m| s(m, "userAddress"))
        .collect();

    assert!(members.contains(&alice.address), "{members:?}");
    assert!(
        members.contains(&boss.address),
        "the operator is in every lobby without anybody inviting them: {members:?}"
    );
    // Alice's note is *not* the lobby: an admin is in one and not the other.
    let note = static_room_id("note", &alice.address);
    boss.api
        .get(&format!("/api/rooms/{note}/members"))
        .await
        .expect_status(403);
}

// --- My Note is private ---------------------------------------------------

#[tokio::test]
async fn a_second_wallet_cannot_read_someone_elses_note() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let mallory = new_user(&server, "mallory").await;
    open_the_app(&alice).await;
    open_the_app(&mallory).await;

    let note = static_room_id("note", &alice.address);
    send_message(&alice.api, &note, "the safe combination is 0451").await;

    // Every read path, tried with an id Mallory computed rather than one she
    // was given. All of them answer the same way a non-member is answered
    // anywhere else — no 404/403 split that would confirm the room exists.
    for path in [
        format!("/api/rooms/{note}"),
        format!("/api/rooms/{note}/messages"),
        format!("/api/rooms/{note}/members"),
        format!("/api/rooms/{note}/admins"),
        format!("/api/rooms/{note}/sync?since=0"),
        format!("/api/rooms/{note}/files"),
        format!("/api/rooms/{note}/keys"),
    ] {
        mallory.api.get(&path).await.expect_status(403);
    }
}

#[tokio::test]
async fn nobody_can_be_let_into_a_note_by_any_door() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let mallory = new_user(&server, "mallory").await;
    open_the_app(&alice).await;
    open_the_app(&mallory).await;
    let note = static_room_id("note", &alice.address);

    // Alice is the room's admin, so none of these are refused for *permission*
    // — they are refused because the room's roster is not hers to widen. That
    // distinction is the whole point: a rule enforced only against outsiders
    // would fall to a moment of carelessness by the owner.
    let invited = alice
        .api
        .post(
            &format!("/api/rooms/{note}/invite"),
            json!({ "userAddress": mallory.address }),
        )
        .await;
    invited.expect_status(400);
    assert!(
        invited.message().contains("built-in room"),
        "{}",
        invited.message()
    );

    // An invite link would be a bearer token into the room, so it is refused
    // at creation rather than left to be revoked later.
    alice
        .api
        .post(&format!("/api/rooms/{note}/invites"), json!({}))
        .await
        .expect_status(400);

    // Promotion presupposes membership, and there is none to grant.
    alice
        .api
        .post(
            &format!("/api/rooms/{note}/admins"),
            json!({ "walletAddress": mallory.address }),
        )
        .await
        .expect_status(400);

    // And accepting an invitation that was never issued is still nothing.
    mallory
        .api
        .post_empty(&format!("/api/invitations/{note}/accept"))
        .await
        .expect_status(404);

    // Mallory is still outside, and the roster is still one person.
    assert_eq!(
        alice
            .api
            .get(&format!("/api/rooms/{note}/members"))
            .await
            .array()
            .len(),
        1
    );
    mallory
        .api
        .get(&format!("/api/rooms/{note}"))
        .await
        .expect_status(403);
}

#[tokio::test]
async fn a_built_in_room_refuses_to_be_encrypted() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    open_the_app(&alice).await;

    // The trade is deliberate and documented in `routes/keys.rs`: the server
    // never indexes ciphertext, so an encrypted note is a notebook you cannot
    // search, which is the only reason to keep one on a server at all.
    for kind in ["note", "jarvis", "lobby"] {
        let room = static_room_id(kind, &alice.address);
        let stored = alice
            .api
            .post(
                &format!("/api/rooms/{room}/keys"),
                json!({
                    "userAddress": alice.address,
                    "encryptedSymmetricKey": "wrapped",
                    "ephemeralPublicKey": "04ab",
                    "encryptionIV": "1a2b3c4d5e6f78901234567890abcdef",
                    "hmac": "9".repeat(64),
                    "keyVersion": 1,
                }),
            )
            .await;
        stored.expect_status(409);
        assert!(stored.message().contains("searchable"), "{kind}");

        // And ciphertext is refused at the door too, so a client that skipped
        // the key upload cannot write something nobody can ever read.
        alice
            .api
            .post(
                &format!("/api/rooms/{room}/messages"),
                json!({
                    "content": "ZmFrZS1jaXBoZXJ0ZXh0",
                    "msgHash": "0".repeat(64),
                    "isEncrypted": true,
                    "iv": "1a2b3c4d5e6f78901234567890abcdef",
                    "hmac": "9".repeat(64),
                    "keyVersion": 1,
                }),
            )
            .await
            .expect_status(409);
    }
}

// --- indestructible, but hideable -----------------------------------------

#[tokio::test]
async fn delete_leave_and_destroy_are_all_refused_for_all_three() {
    let boss_signer = crypto::Signer::random();
    let boss_address = boss_signer.address().to_lowercase();
    let server =
        TestServer::start_with_env(&[("VITE_FRUITNATION_ADMIN", boss_address.as_str())]).await;
    let boss = login(&server, boss_signer, "boss").await;
    let alice = new_user(&server, "alice").await;
    open_the_app(&alice).await;
    open_the_app(&boss).await;

    for kind in ["note", "jarvis", "lobby"] {
        let room = static_room_id(kind, &alice.address);

        let deleted = alice.api.delete(&format!("/api/rooms/{room}")).await;
        deleted.expect_status(400);
        assert!(
            deleted.message().contains("Hide it instead"),
            "the refusal has to name the verb that does work: {}",
            deleted.message()
        );

        alice
            .api
            .post_empty(&format!("/api/rooms/{room}/leave"))
            .await
            .expect_status(400);

        // The operator's console reaches past a room's own admins by design.
        // It does not reach past this, or "nobody else can read my note" would
        // quietly mean "except whoever runs the server".
        boss.api
            .delete(&format!("/api/admin/rooms/{room}"))
            .await
            .expect_status(400);

        // Still there, and still the caller's.
        alice
            .api
            .get(&format!("/api/rooms/{room}"))
            .await
            .expect_status(200);
    }

    assert_eq!(rooms_by_kind(&alice).await.len(), 3);
}

#[tokio::test]
async fn hiding_a_built_in_room_works_and_is_reversible() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    open_the_app(&alice).await;
    let note = static_room_id("note", &alice.address);

    alice
        .api
        .post_empty(&format!("/api/rooms/{note}/hide"))
        .await
        .expect_status(200);

    let visible: Vec<String> = alice
        .api
        .get("/api/rooms")
        .await
        .array()
        .iter()
        .map(|r| s(r, "id"))
        .collect();
    assert!(!visible.contains(&note), "{visible:?}");
    assert_eq!(visible.len(), 2);

    // It is in the hidden drawer, with the room folded in so the dialog can
    // name it.
    let hidden = alice.api.get("/api/rooms/hidden").await.array();
    assert_eq!(hidden.len(), 1);
    assert_eq!(s(&hidden[0], "roomId"), note);
    assert_eq!(s(&hidden[0]["room"], "kind"), "note");

    // Hiding is a list preference, not a departure: the room still answers,
    // and — the part that could plausibly have broken — the provisioning that
    // runs on every listing must not quietly unhide it.
    alice
        .api
        .get(&format!("/api/rooms/{note}"))
        .await
        .expect_status(200);
    assert_eq!(alice.api.get("/api/rooms").await.array().len(), 2);

    alice
        .api
        .delete(&format!("/api/rooms/{note}/hide"))
        .await
        .expect_status(200);
    assert_eq!(alice.api.get("/api/rooms").await.array().len(), 3);
}

// --- search ---------------------------------------------------------------

#[tokio::test]
async fn a_note_is_searchable_by_its_owner_and_by_nobody_else() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let mallory = new_user(&server, "mallory").await;
    open_the_app(&alice).await;
    open_the_app(&mallory).await;

    let note = static_room_id("note", &alice.address);
    send_message(&alice.api, &note, "the boiler service code is 2231 #house").await;

    let hits = |body: &Value| -> Vec<String> {
        body["results"]
            .as_array()
            .expect("results")
            .iter()
            .map(|h| s(h, "text"))
            .collect()
    };

    let mine = alice
        .api
        .get("/api/search?q=boiler%20service%20code")
        .await
        .expect_ok();
    assert_eq!(hits(&mine), ["the boiler service code is 2231 #house"]);

    // Mallory shares no room with Alice, so the note's message is outside her
    // scope entirely — the same rule every other room already gets, which is
    // exactly why this needed no second search engine.
    let theirs = mallory
        .api
        .get("/api/search?q=boiler%20service%20code")
        .await
        .expect_ok();
    assert!(hits(&theirs).is_empty(), "{theirs}");

    // The hashtag browse is scoped the same way.
    let tags = alice.api.get("/api/search/tags").await.expect_ok();
    assert!(
        tags["tags"]
            .as_array()
            .expect("tags")
            .iter()
            .any(|t| s(t, "tag") == "house"),
        "{tags}"
    );
    let mallorys_tags = mallory.api.get("/api/search/tags").await.expect_ok();
    assert!(
        mallorys_tags["tags"].as_array().expect("tags").is_empty(),
        "{mallorys_tags}"
    );
}

#[tokio::test]
async fn a_file_uploaded_into_a_note_is_findable_only_by_its_owner() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let mallory = new_user(&server, "mallory").await;
    open_the_app(&alice).await;
    open_the_app(&mallory).await;
    let note = static_room_id("note", &alice.address);

    // The upload path is a raw body with the metadata in the query string, so
    // it goes through reqwest directly rather than through `Api`.
    let url = server.url(&format!(
        "/api/rooms/{note}/files?filename=boiler-warranty.pdf\
         &caption=kept%20for%20the%20plumber%20%23house"
    ));
    let status = alice
        .api
        .http
        .post(url)
        .header("Authorization", format!("Bearer {}", alice.api.token()))
        .body(b"%PDF-1.4 warranty".to_vec())
        .send()
        .await
        .expect("upload request failed")
        .status()
        .as_u16();
    assert_eq!(status, 201, "the upload must have landed");

    let found: Vec<String> = alice
        .api
        .get("/api/search?q=boiler%20warranty&kind=file")
        .await
        .expect_ok()["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|h| s(h, "text"))
        .collect();
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("boiler-warranty.pdf"), "{found:?}");

    let theirs = mallory
        .api
        .get("/api/search?q=boiler%20warranty&kind=file")
        .await
        .expect_ok();
    assert!(
        theirs["results"].as_array().expect("results").is_empty(),
        "an attachment is exactly as private as the room it was posted in: {theirs}"
    );
}

// --- the agent ------------------------------------------------------------

#[tokio::test]
async fn the_agent_speaks_only_in_its_owners_jarvis_room() {
    let server = TestServer::start().await;
    let alice = new_user(&server, "alice").await;
    let mallory = new_user(&server, "mallory").await;
    open_the_app(&alice).await;
    open_the_app(&mallory).await;

    let jarvis = static_room_id("jarvis", &alice.address);
    send_message(&alice.api, &jarvis, "what is on today?").await;

    // The browser holds the key and made the model call; this endpoint exists
    // only to write the answer under an address the browser cannot claim.
    let posted = alice
        .api
        .post(
            &format!("/api/rooms/{jarvis}/agent"),
            json!({ "text": "Nothing until three." }),
        )
        .await
        .expect_ok();
    assert_eq!(s(&posted, "content"), "Nothing until three.");
    assert!(
        s(&posted, "senderAddress").starts_with("0x000000a1"),
        "the reply is the agent's, not the caller's: {posted}"
    );
    assert!(!b(&posted, "isEncrypted"));

    // Every other room refuses it, including Alice's own note — the room kind
    // is part of the check, not just the ownership.
    for room in [
        static_room_id("note", &alice.address),
        static_room_id("lobby", &alice.address),
        static_room_id("jarvis", &mallory.address),
    ] {
        alice
            .api
            .post(
                &format!("/api/rooms/{room}/agent"),
                json!({ "text": "forged" }),
            )
            .await
            .expect_status(403);
    }

    // An ordinary channel Alice administers is refused too.
    let channel = create_room(&alice.api, "Team").await;
    alice
        .api
        .post(
            &format!("/api/rooms/{channel}/agent"),
            json!({ "text": "forged" }),
        )
        .await
        .expect_status(403);
}
