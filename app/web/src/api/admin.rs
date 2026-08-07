//! Server administration (API.md §6.14).
//!
//! Every call here except [`Client::admin_session`] answers 403 unless the
//! signed-in wallet is listed in the server's `VITE_FRUITNATION_ADMIN`. That is
//! deployment configuration, not something the client can inspect or change —
//! which is why `admin_session` exists at all: it is the only way for a client
//! holding a restored token to find out whether to offer the console.

use gloo_net::http::Method;
use pocketskynet_core::{RoomId, WalletAddress};
use serde::{Deserialize, Serialize};

use super::{encode_segment, ApiResult, Client};

/// Whether the caller administers this server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminSession {
    #[serde(default)]
    pub is_server_admin: bool,
}

/// Server-wide totals for the console header.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminTotals {
    #[serde(default)]
    pub users: i64,
    #[serde(default)]
    pub suspended: i64,
    #[serde(default)]
    pub channels: i64,
    #[serde(default)]
    pub direct_messages: i64,
    #[serde(default)]
    pub messages: i64,
    #[serde(default)]
    pub files: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminOverview {
    #[serde(default)]
    pub totals: AdminTotals,
    /// The addresses the server parsed out of `VITE_FRUITNATION_ADMIN`,
    /// lowercased. Shown verbatim so an operator can see a typo there — which
    /// is otherwise completely silent, since its only symptom is a colleague
    /// who mysteriously has no powers.
    #[serde(default)]
    pub admins: Vec<String>,
}

/// One account, as an operator sees it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminUser {
    pub wallet_address: WalletAddress,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub profile_image: Option<String>,
    #[serde(default)]
    pub room_count: i64,
    #[serde(default)]
    pub message_count: i64,
    #[serde(default)]
    pub is_suspended: bool,
    #[serde(default)]
    pub suspended_reason: Option<String>,
    #[serde(default)]
    pub is_server_admin: bool,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// One room, as an operator sees it — **metadata only**. There is deliberately
/// no endpoint that hands an admin the contents of a room they are not in.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminRoom {
    pub id: RoomId,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub member_count: i64,
    #[serde(default)]
    pub message_count: i64,
    #[serde(default)]
    pub has_encryption: bool,
    #[serde(default)]
    pub last_activity_at: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

// ------------------------------------------------------------- dashboard ---

/// The storage headline numbers (`GET /api/admin/storage`, `totals`).
///
/// Two byte counts on purpose: storage is content-addressed server-side, so
/// two rows can share one blob. `logical_bytes` is what people uploaded,
/// `disk_bytes` what the disk pays — the gap between them *is* the dedupe.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageTotals {
    #[serde(default)]
    pub files: i64,
    #[serde(default)]
    pub blobs: i64,
    #[serde(default)]
    pub logical_bytes: i64,
    #[serde(default)]
    pub disk_bytes: i64,
    #[serde(default)]
    pub rooms_with_files: i64,
}

/// The categories the server classifies into, in the order every chart,
/// legend and filter shows them. Mirrors the server's `CATEGORIES` — one
/// contract in two places, pinned by the dashboard rendering the server's
/// fixed-order slices against this list.
pub const CATEGORY_ORDER: [&str; 6] = ["image", "video", "audio", "document", "archive", "other"];

/// One slice of the by-kind breakdown. The server sends every category in a
/// fixed order, zeros included, so the chart never reshuffles.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CategorySlice {
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub files: i64,
    #[serde(default)]
    pub bytes: i64,
}

/// One room's share of the disk.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomUsage {
    #[serde(default)]
    pub room_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub files: i64,
    #[serde(default)]
    pub bytes: i64,
}

/// One attachment, as the operator's listing sees it: metadata only. There is
/// deliberately no URL here and no way to build one that works — the bytes
/// stay behind the room-membership check, admin or not.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminFile {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub extension: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub size_bytes: i64,
    #[serde(default)]
    pub room_id: String,
    #[serde(default)]
    pub room_name: String,
    #[serde(default)]
    pub uploader: String,
    #[serde(default)]
    pub uploader_name: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// One day of upload volume, UTC. Days with no uploads are absent; the chart
/// fills the gaps, because a padded wire format is just a bigger wire format.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrowthPoint {
    #[serde(default)]
    pub day: String,
    #[serde(default)]
    pub files: i64,
    #[serde(default)]
    pub bytes: i64,
}

/// One direction of transfer traffic, as integer counters. Rates are computed
/// client-side (`bytes / millis`), which keeps the wire free of floats and
/// the arithmetic host-testable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowStats {
    #[serde(default)]
    pub transfers: i64,
    #[serde(default)]
    pub bytes: i64,
    #[serde(default)]
    pub millis: i64,
    #[serde(default)]
    pub recent_bytes: i64,
    #[serde(default)]
    pub recent_millis: i64,
}

/// The in-process transfer counters — since server start, gone at restart.
/// The dashboard says so out loud rather than pretending they are history.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    #[serde(default)]
    pub since_ms: i64,
    #[serde(default)]
    pub recent_window_ms: i64,
    #[serde(default)]
    pub uploads: FlowStats,
    #[serde(default)]
    pub downloads: FlowStats,
}

/// Everything `GET /api/admin/storage` reports, ready for one screen.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminStorage {
    #[serde(default)]
    pub totals: StorageTotals,
    #[serde(default)]
    pub categories: Vec<CategorySlice>,
    #[serde(default)]
    pub rooms: Vec<RoomUsage>,
    #[serde(default)]
    pub largest: Vec<AdminFile>,
    #[serde(default)]
    pub growth: Vec<GrowthPoint>,
    #[serde(default)]
    pub activity: Activity,
}

/// The rooms, by what they are (`GET /api/admin/stats`, `rooms`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomComposition {
    #[serde(default)]
    pub total: i64,
    #[serde(default)]
    pub channels: i64,
    #[serde(default)]
    pub direct_messages: i64,
    #[serde(default)]
    pub encrypted: i64,
    #[serde(default)]
    pub plaintext: i64,
}

/// The accounts, by standing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeopleStats {
    #[serde(default)]
    pub total: i64,
    #[serde(default)]
    pub suspended: i64,
    #[serde(default)]
    pub in_rooms: i64,
}

/// The conversation volume — counts, never a word of it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageStats {
    #[serde(default)]
    pub total: i64,
    #[serde(default)]
    pub thread_replies: i64,
    #[serde(default)]
    pub reactions: i64,
}

/// One day of message volume, UTC — same contract as [`GrowthPoint`]:
/// silent days are absent and the chart restores them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageDay {
    #[serde(default)]
    pub day: String,
    #[serde(default)]
    pub messages: i64,
}

/// One room's share of the conversation. Names and counts — the same fields
/// the admin room listing shows, and nothing a conversation could leak
/// through.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BusyRoom {
    #[serde(default)]
    pub room_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub members: i64,
    #[serde(default)]
    pub messages: i64,
    #[serde(default)]
    pub has_encryption: bool,
}

/// Heads connected right now — two integers, deliberately not a roster:
/// *who* is online is knowledge scoped to shared rooms, admin or not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PresenceCounts {
    #[serde(default)]
    pub online: i64,
    #[serde(default)]
    pub away: i64,
}

/// Everything `GET /api/admin/stats` reports — the Skynet Dashboard's
/// whole-server half.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminStats {
    #[serde(default)]
    pub uptime_seconds: i64,
    #[serde(default)]
    pub presence: PresenceCounts,
    #[serde(default)]
    pub rooms: RoomComposition,
    #[serde(default)]
    pub people: PeopleStats,
    #[serde(default)]
    pub messages: MessageStats,
    #[serde(default)]
    pub activity: Vec<MessageDay>,
    #[serde(default)]
    pub busiest: Vec<BusyRoom>,
}

#[derive(Serialize)]
struct SuspendReq<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
}

impl Client {
    /// Whether *this* caller is a server admin.
    ///
    /// Never 403s — that is the point. A client restoring a stored token has no
    /// login response to read `isServerAdmin` from, and an endpoint that
    /// refused non-admins would make "you are not one" indistinguishable from
    /// "the server is unreachable".
    pub async fn admin_session(&self) -> ApiResult<bool> {
        let session: AdminSession = self.send(Method::GET, "/api/admin/session").await?;
        Ok(session.is_server_admin)
    }

    pub async fn admin_overview(&self) -> ApiResult<AdminOverview> {
        self.send(Method::GET, "/api/admin/overview").await
    }

    pub async fn admin_users(&self) -> ApiResult<Vec<AdminUser>> {
        self.send(Method::GET, "/api/admin/users").await
    }

    pub async fn admin_rooms(&self) -> ApiResult<Vec<AdminRoom>> {
        self.send(Method::GET, "/api/admin/rooms").await
    }

    /// The files dashboard's aggregates: storage totals, the by-kind
    /// breakdown, per-room usage, the largest attachments, a month of upload
    /// volume, and the in-process transfer counters.
    pub async fn admin_storage(&self) -> ApiResult<AdminStorage> {
        self.send(Method::GET, "/api/admin/storage").await
    }

    /// The deployment in counts: rooms by kind and encryption, accounts,
    /// message volume and its daily series, the loudest rooms, live
    /// presence head-counts, and uptime.
    pub async fn admin_stats(&self) -> ApiResult<AdminStats> {
        self.send(Method::GET, "/api/admin/stats").await
    }

    /// Every attachment's metadata, newest first. Sorting and filtering are
    /// the dashboard's job — the response is capped, not paged, the same
    /// scale decision the other admin listings state.
    pub async fn admin_files(&self) -> ApiResult<Vec<AdminFile>> {
        self.send(Method::GET, "/api/admin/files").await
    }

    /// Suspend an account. Takes effect on the target's *existing* tokens, not
    /// only their next sign-in.
    pub async fn admin_suspend(&self, who: &WalletAddress, reason: Option<&str>) -> ApiResult<()> {
        self.send_ok(
            Method::POST,
            &format!("/api/admin/users/{}/suspend", encode_segment(who.as_str())),
            &SuspendReq {
                reason: reason.filter(|r| !r.trim().is_empty()),
            },
        )
        .await
    }

    pub async fn admin_reinstate(&self, who: &WalletAddress) -> ApiResult<()> {
        self.send_ok_empty(
            Method::DELETE,
            &format!("/api/admin/users/{}/suspend", encode_segment(who.as_str())),
        )
        .await
    }

    /// Remove somebody from every room on the server and suspend them.
    ///
    /// Their messages stay where they are, attributed to them: a room's record
    /// of a conversation is not the operator's to rewrite.
    pub async fn admin_remove_user(&self, who: &WalletAddress) -> ApiResult<()> {
        self.send_ok_empty(
            Method::DELETE,
            &format!("/api/admin/users/{}", encode_segment(who.as_str())),
        )
        .await
    }

    /// Delete any room, including one whose last admin has left — which is the
    /// case the room-level endpoint cannot reach.
    pub async fn admin_delete_room(&self, room: &RoomId) -> ApiResult<()> {
        self.send_ok_empty(
            Method::DELETE,
            &format!("/api/admin/rooms/{}", encode_segment(room.as_str())),
        )
        .await
    }
}
