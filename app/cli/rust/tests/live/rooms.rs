//! Rooms: create, list, and the refusals API.md pins.

use pocketskynet_client::ClientError;

use crate::common::{self, TestServer};

#[tokio::test]
async fn a_created_room_appears_in_the_list_with_its_creator_as_member() {
    let server = TestServer::start().await;
    let (client, _wallet) = common::login_new_user(&server, "roomer").await;

    let created = client
        .create_room("integration lounge", Some("made by the test"))
        .await
        .unwrap();
    assert_eq!(created.name.as_deref(), Some("integration lounge"));
    assert_eq!(created.description.as_deref(), Some("made by the test"));
    assert_eq!(created.current_key_version, Some(1));
    assert_eq!(created.key_rotation_pending, Some(false));

    // Fresh accounts also get built-in rooms (My Note/My Jarvis/My Lobby),
    // so membership is asserted, never the total count.
    let rooms = client.rooms().await.unwrap();
    let mine = rooms
        .iter()
        .find(|r| r.id == created.id)
        .expect("the created room must be listed");
    assert_eq!(mine.member_count, Some(1));
    assert_eq!(
        mine.unread_count,
        Some(0),
        "own messages never count unread"
    );
    assert_eq!(mine.name.as_deref(), Some("integration lounge"));
}

#[tokio::test]
async fn two_rooms_may_share_a_name() {
    // Names are labels, not identities — the id is the identity.
    let server = TestServer::start().await;
    let (client, _wallet) = common::login_new_user(&server, "twin").await;

    let first = client.create_room("twins", None).await.unwrap();
    let second = client.create_room("twins", None).await.unwrap();
    assert_ne!(first.id, second.id);

    let rooms = client.rooms().await.unwrap();
    let twins: Vec<_> = rooms
        .iter()
        .filter(|r| r.name.as_deref() == Some("twins"))
        .collect();
    assert_eq!(twins.len(), 2, "both identically-named rooms are listed");
}

#[tokio::test]
async fn invalid_room_names_are_refused_with_400() {
    let server = TestServer::start().await;
    let (client, _wallet) = common::login_new_user(&server, "namer").await;

    // Forbidden markup characters (API.md §3.1 roomName).
    match client.create_room("<script>alert(1)</script>", None).await {
        Err(ClientError::Api {
            status: 400,
            message,
            ..
        }) => assert!(
            message.contains("Validation failed") || message.contains("invalid"),
            "unexpected message: {message}"
        ),
        other => panic!("markup in a room name must be a 400, got {other:?}"),
    }

    // Empty (after trim) is refused too.
    match client.create_room("   ", None).await {
        Err(ClientError::Api { status: 400, .. }) => {}
        other => panic!("a blank room name must be a 400, got {other:?}"),
    }
}

#[tokio::test]
async fn someone_elses_room_and_a_missing_room_both_read_as_403() {
    // Membership is checked before existence (deliberately, per API.md
    // §6.5.3): a non-member and a nonexistent room are indistinguishable, so
    // there is no room-existence oracle.
    let server = TestServer::start().await;
    let (owner, _w1) = common::login_new_user(&server, "owner").await;
    let (outsider, _w2) = common::login_new_user(&server, "outsider").await;

    let room = owner.create_room("private club", None).await.unwrap();

    match outsider.messages(&room.id, 10).await {
        Err(ClientError::Api {
            status: 403,
            message,
            ..
        }) => assert_eq!(message, "Access denied"),
        other => panic!("a non-member read must be a 403, got {other:?}"),
    }

    match outsider
        .messages("room_0000000000_does-not-exist", 10)
        .await
    {
        Err(ClientError::Api {
            status: 403,
            message,
            ..
        }) => assert_eq!(message, "Access denied", "no existence oracle"),
        other => panic!("a missing room must read as 403, got {other:?}"),
    }
}
