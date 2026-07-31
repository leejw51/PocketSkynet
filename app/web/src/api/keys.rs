//! Room keys and epoch rotation (API.md §6.9, §10).

use gloo_net::http::Method;
use pocketskynet_core::RoomId;
use serde::Serialize;

use super::{encode_segment, ApiError, ApiResult, Client, RoomKey, RoomKeyWrap};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RotateReq<'a> {
    new_version: i64,
    keys: &'a [RoomKeyWrap],
}

impl Client {
    /// Store one wrap, for one member, for one epoch.
    ///
    /// Storing for **someone else** 409s if they already hold a wrap for that
    /// epoch: an admin must not be able to clobber a valid wrap and lock a
    /// member out. Overwriting your *own* wrap for an epoch is always allowed.
    pub async fn put_room_key(&self, room: &RoomId, wrap: &RoomKeyWrap) -> ApiResult<()> {
        self.send_ok(
            Method::POST,
            &format!("/api/rooms/{}/keys", encode_segment(room.as_str())),
            wrap,
        )
        .await
    }

    /// Every epoch the caller can read, ascending.
    ///
    /// This — not `GET …/keys` — is what history decryption needs: a member
    /// accumulates one wrap per epoch, and dropping the older ones would black
    /// out everything sent before the last rotation.
    ///
    /// A 404 (old server, or no wrap at all) degrades to an empty list rather
    /// than an error: "this room has no key for me" is a renderable state, not
    /// a failure.
    pub async fn room_key_versions(&self, room: &RoomId) -> ApiResult<Vec<RoomKey>> {
        let path = format!("/api/rooms/{}/keys/versions", encode_segment(room.as_str()));
        match self.send::<Vec<RoomKey>>(Method::GET, &path).await {
            Ok(v) => Ok(v),
            Err(e) if e.is_not_found() => Ok(Vec::new()),
            // Fall back to the single-key endpoint for a server that predates
            // `/versions`, treating a missing keyVersion as epoch 1.
            Err(ApiError::Status(s)) if s.status == 400 => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// The caller's latest wrap only. Kept for the legacy fallback path; prefer
    /// [`Client::room_key_versions`] everywhere else.
    pub async fn room_key(&self, room: &RoomId) -> ApiResult<Option<RoomKey>> {
        let path = format!("/api/rooms/{}/keys", encode_segment(room.as_str()));
        match self.send::<RoomKey>(Method::GET, &path).await {
            Ok(k) => Ok(Some(k)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Advance the room to a new epoch, atomically.
    ///
    /// `keys` must cover **every current member** and name **no** non-members;
    /// the server rejects either with a 400 (the coverage failure lists the
    /// missing addresses). A 409 `Stale key version` means someone else rotated
    /// first — refetch the room and retry only if rotation is still pending.
    ///
    /// Any member may rotate, not just an admin: gating this on admins would
    /// freeze an encrypted room after a departure until an admin appeared.
    pub async fn rotate_key(
        &self,
        room: &RoomId,
        new_version: i64,
        keys: &[RoomKeyWrap],
    ) -> ApiResult<()> {
        self.send_ok(
            Method::POST,
            &format!("/api/rooms/{}/rotate-key", encode_segment(room.as_str())),
            &RotateReq { new_version, keys },
        )
        .await
    }
}
