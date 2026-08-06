//! Rooms, membership, admins and hidden rooms (API.md §6.5, §6.6, §6.8).

use gloo_net::http::Method;
use pocketskynet_core::{RoomId, WalletAddress};
use serde::Serialize;

use super::{
    encode_segment, ApiResult, Client, HiddenRoom, Room, RoomMember, RoomWithMembers, User,
};

#[derive(Serialize)]
struct CreateRoomReq<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
}

#[derive(Serialize)]
struct NameReq<'a> {
    name: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DmReq<'a> {
    wallet_addresses: &'a [WalletAddress],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UserAddressReq<'a> {
    user_address: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WalletAddressReq<'a> {
    wallet_address: &'a str,
}

impl Client {
    /// Create a room. The response is a **bare** `Room` with no roster, and no
    /// WebSocket event is emitted — the caller must refetch the room list
    /// itself (API.md §6.5.1).
    pub async fn create_room(&self, name: &str, description: Option<&str>) -> ApiResult<Room> {
        self.send_json(
            Method::POST,
            "/api/rooms",
            &CreateRoomReq {
                name,
                // An empty description is coerced to SQL NULL server-side;
                // sending `""` and sending nothing are equivalent, but omitting
                // it keeps the request honest.
                description: description.filter(|d| !d.trim().is_empty()),
            },
        )
        .await
    }

    /// Open a direct message with `who`, or return the one that already exists.
    ///
    /// **Idempotent by identity, not by convention.** The room is keyed on its
    /// member set server-side, so this is the answer to "the conversation
    /// between these people" rather than a create call that happens to be safe
    /// to retry — two people pressing "message" at the same moment land in one
    /// room. Naming only yourself is allowed and gives the private notebook.
    ///
    /// Returns the *enriched* room, because a DM has no name of its own: the
    /// caller has to title it from the roster, which travels in this response.
    pub async fn open_dm(&self, who: &[WalletAddress]) -> ApiResult<RoomWithMembers> {
        self.send_json(
            Method::POST,
            "/api/rooms/dm",
            &DmReq {
                wallet_addresses: who,
            },
        )
        .await
    }

    /// Every room the caller is in, hidden ones excluded. This is the **only**
    /// endpoint that returns `unreadCount`/`lastReadSerial`.
    ///
    /// The server returns them in insertion order; sorting by activity is the
    /// client's job (`RoomWithMembers::activity_ts`).
    pub async fn rooms(&self) -> ApiResult<Vec<RoomWithMembers>> {
        self.send(Method::GET, "/api/rooms").await
    }

    /// A single room. Note this returns **403, not 404**, for a room that does
    /// not exist: membership is checked before existence so the endpoint cannot
    /// be used as a room-existence oracle.
    pub async fn room(&self, id: &RoomId) -> ApiResult<RoomWithMembers> {
        self.send(
            Method::GET,
            &format!("/api/rooms/{}", encode_segment(id.as_str())),
        )
        .await
    }

    pub async fn rename_room(&self, id: &RoomId, name: &str) -> ApiResult<Room> {
        self.send_json(
            Method::PATCH,
            &format!("/api/rooms/{}", encode_segment(id.as_str())),
            &NameReq { name },
        )
        .await
    }

    pub async fn delete_room(&self, id: &RoomId) -> ApiResult<()> {
        self.send_ok_empty(
            Method::DELETE,
            &format!("/api/rooms/{}", encode_segment(id.as_str())),
        )
        .await
    }

    /// Leaving sets `keyRotationPending` on the room — the leaver may still
    /// hold the current key, so nothing encrypted can be posted until a
    /// remaining member re-keys.
    pub async fn leave_room(&self, id: &RoomId) -> ApiResult<()> {
        self.send_ok(
            Method::POST,
            &format!("/api/rooms/{}/leave", encode_segment(id.as_str())),
            &serde_json::json!({}),
        )
        .await
    }

    pub async fn kick_member(&self, id: &RoomId, who: &WalletAddress) -> ApiResult<()> {
        self.send_ok(
            Method::POST,
            &format!("/api/rooms/{}/kick", encode_segment(id.as_str())),
            &UserAddressReq {
                user_address: who.as_str(),
            },
        )
        .await
    }

    /// The roster. **Not** block-filtered — blocked members still appear, and
    /// the UI marks them rather than hiding them, so a blocker can tell why
    /// they cannot see someone's messages.
    pub async fn members(&self, id: &RoomId) -> ApiResult<Vec<RoomMember>> {
        self.send(
            Method::GET,
            &format!("/api/rooms/{}/members", encode_segment(id.as_str())),
        )
        .await
    }

    pub async fn admins(&self, id: &RoomId) -> ApiResult<Vec<User>> {
        self.send(
            Method::GET,
            &format!("/api/rooms/{}/admins", encode_segment(id.as_str())),
        )
        .await
    }

    /// Promote a member. Capped at 9 admins per room, server-enforced.
    pub async fn add_admin(&self, id: &RoomId, who: &WalletAddress) -> ApiResult<()> {
        self.send_ok(
            Method::POST,
            &format!("/api/rooms/{}/admins", encode_segment(id.as_str())),
            &WalletAddressReq {
                wallet_address: who.as_str(),
            },
        )
        .await
    }

    /// Demote an admin. The server refuses to remove the last one — a room
    /// with no admin can never be managed again.
    pub async fn remove_admin(&self, id: &RoomId, who: &WalletAddress) -> ApiResult<()> {
        self.send_ok_empty(
            Method::DELETE,
            &format!(
                "/api/rooms/{}/admins/{}",
                encode_segment(id.as_str()),
                encode_segment(who.as_str())
            ),
        )
        .await
    }

    /// Hidden rooms, with the room detail nested. Membership is re-checked
    /// server-side, so a former member who hid a room sees nothing here.
    pub async fn hidden_rooms(&self) -> ApiResult<Vec<HiddenRoom>> {
        self.send(Method::GET, "/api/rooms/hidden").await
    }

    /// Hiding removes a room from the list without touching membership: you
    /// still receive its messages, you just do not see the row.
    pub async fn hide_room(&self, id: &RoomId) -> ApiResult<()> {
        self.send_ok(
            Method::POST,
            &format!("/api/rooms/{}/hide", encode_segment(id.as_str())),
            &serde_json::json!({}),
        )
        .await
    }

    pub async fn unhide_room(&self, id: &RoomId) -> ApiResult<()> {
        self.send_ok_empty(
            Method::DELETE,
            &format!("/api/rooms/{}/hide", encode_segment(id.as_str())),
        )
        .await
    }
}
