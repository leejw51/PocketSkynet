//! Realtime event types, shared by the WebSocket, SSE, and JSONL encodings.
//!
//! One type, three transports: the serde form here *is* the WebSocket frame
//! body, the SSE `data:` payload, and the `event` field of a JSONL record. If
//! they were defined separately they would drift, and a drift between the log
//! and the wire is exactly the kind of bug that only shows up during an
//! incident.
//!
//! Events are **wake-up signals, not content**. `NewMessage` carries a room id
//! and a serial, never the message body — so the fan-out path never touches
//! ciphertext, never needs a room key, and never has to be block-filtered for
//! content leakage.

use serde::{Deserialize, Serialize};

use crate::ids::{RoomId, WalletAddress};

/// Server → client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    /// Something was appended to a room. Fetch `/api/rooms/:id/sync?since=`.
    NewMessage {
        #[serde(rename = "roomId")]
        room_id: RoomId,
        #[serde(rename = "msgSerial")]
        msg_serial: i64,
    },
    /// The caller's room list changed (joined, renamed, deleted, key rotated).
    RoomsUpdated,
    /// The caller was removed from a room, or lost access to it.
    MemberRemoved {
        #[serde(rename = "roomId")]
        room_id: RoomId,
    },
    /// The caller has a new pending invitation.
    InvitationReceived {
        #[serde(rename = "roomId")]
        room_id: RoomId,
    },
    /// Someone is typing. Never persisted and never replayed.
    Typing {
        #[serde(rename = "roomId")]
        room_id: RoomId,
        from: WalletAddress,
    },
    /// The stream could not be resumed losslessly; do a full sync.
    ResyncRequired {
        reason: ResyncReason,
        #[serde(rename = "fromSeq")]
        from_seq: u64,
        #[serde(rename = "toSeq")]
        to_seq: u64,
    },
    /// The bearer token backing this stream expired. Re-authenticate.
    SessionExpired { reason: String },
    /// A paid broadcast went live (or was refreshed). Like every other event
    /// this is a wake-up, not content: clients fetch `/api/shout/active` and
    /// render what the REST path authorises. Carrying the id lets a client
    /// that already shows this shout skip the fetch.
    Shout {
        #[serde(rename = "shoutId")]
        shout_id: String,
    },
    /// Reply to [`ClientMessage::Ping`].
    Pong,
}

/// Why a resume could not be served exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResyncReason {
    /// The cursor is older than the retained window.
    CursorTooOld,
    /// The connection fell behind the broadcast buffer.
    Lagged,
    /// Membership changed during the gap, so replay could not be filtered
    /// safely against the caller's *current* access.
    MembershipChanged,
}

impl ServerEvent {
    /// The SSE `event:` name — identical to the serde `type` tag, so a client
    /// can dispatch on either without them ever disagreeing.
    pub fn name(&self) -> &'static str {
        match self {
            Self::NewMessage { .. } => "new_message",
            Self::RoomsUpdated => "rooms_updated",
            Self::MemberRemoved { .. } => "member_removed",
            Self::InvitationReceived { .. } => "invitation_received",
            Self::Typing { .. } => "typing",
            Self::ResyncRequired { .. } => "resync_required",
            Self::SessionExpired { .. } => "session_expired",
            Self::Shout { .. } => "shout",
            Self::Pong => "pong",
        }
    }

    /// Whether this event may be replayed to a resuming stream.
    ///
    /// Transient signals must not be: a typing indicator delivered a minute
    /// late is worse than one dropped, and `Pong` belongs to a connection that
    /// no longer exists. A shout lives for at most a minute, and every client
    /// re-fetches the active set on connect anyway, so replaying it would only
    /// provoke fetches for banners that have already burned out.
    pub fn is_replayable(&self) -> bool {
        !matches!(self, Self::Typing { .. } | Self::Shout { .. } | Self::Pong)
    }

    /// The room this event concerns, when it concerns exactly one.
    pub fn room_id(&self) -> Option<&RoomId> {
        match self {
            Self::NewMessage { room_id, .. }
            | Self::MemberRemoved { room_id }
            | Self::InvitationReceived { room_id }
            | Self::Typing { room_id, .. } => Some(room_id),
            Self::RoomsUpdated
            | Self::ResyncRequired { .. }
            | Self::SessionExpired { .. }
            | Self::Shout { .. }
            | Self::Pong => None,
        }
    }
}

/// Client → server. Deliberately tiny: everything else goes over REST, which
/// keeps the socket's attack surface to two shapes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Ping,
    Typing {
        #[serde(rename = "roomId")]
        room_id: RoomId,
    },
}

/// Who an event should reach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Target {
    /// Every connected member of a room.
    Room {
        #[serde(rename = "roomId")]
        room_id: RoomId,
    },
    /// Every connection belonging to one wallet.
    User { wallet: WalletAddress },
    /// Every connected member of a room except one wallet — used for typing,
    /// where echoing to the sender is pure noise.
    RoomExcept {
        #[serde(rename = "roomId")]
        room_id: RoomId,
        except: WalletAddress,
    },
    /// Every connected user, whoever they are — the paid broadcast tier.
    /// Nothing room-scoped may ever use this: it exists only for events whose
    /// authorisation is "logged in at all", like a shout.
    All,
}

impl Target {
    pub fn room(&self) -> Option<&RoomId> {
        match self {
            Self::Room { room_id } | Self::RoomExcept { room_id, .. } => Some(room_id),
            Self::User { .. } | Self::All => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room() -> RoomId {
        RoomId::new("room_1749652739650_304e0eaf").unwrap()
    }

    fn wallet() -> WalletAddress {
        WalletAddress::new("0x742d35Cc6634C0532925a3b8D31cE5bb1C6E6B22").unwrap()
    }

    #[test]
    fn new_message_uses_the_wire_field_names() {
        let ev = ServerEvent::NewMessage {
            room_id: room(),
            msg_serial: 1749652900000,
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();

        assert_eq!(json["type"], "new_message");
        assert_eq!(json["roomId"], "room_1749652739650_304e0eaf");
        assert_eq!(json["msgSerial"], 1749652900000i64);
    }

    #[test]
    fn the_serde_tag_and_the_sse_event_name_never_diverge() {
        let events = [
            ServerEvent::NewMessage {
                room_id: room(),
                msg_serial: 1,
            },
            ServerEvent::RoomsUpdated,
            ServerEvent::MemberRemoved { room_id: room() },
            ServerEvent::InvitationReceived { room_id: room() },
            ServerEvent::Typing {
                room_id: room(),
                from: wallet(),
            },
            ServerEvent::ResyncRequired {
                reason: ResyncReason::Lagged,
                from_seq: 1,
                to_seq: 9,
            },
            ServerEvent::SessionExpired {
                reason: "expired".into(),
            },
            ServerEvent::Shout {
                shout_id: "shout_1".into(),
            },
            ServerEvent::Pong,
        ];

        for ev in events {
            let json: serde_json::Value = serde_json::to_value(&ev).unwrap();
            assert_eq!(
                json["type"].as_str().unwrap(),
                ev.name(),
                "SSE event name must equal the serde tag for {ev:?}"
            );
        }
    }

    #[test]
    fn every_event_round_trips() {
        let events = [
            ServerEvent::NewMessage {
                room_id: room(),
                msg_serial: 42,
            },
            ServerEvent::RoomsUpdated,
            ServerEvent::Typing {
                room_id: room(),
                from: wallet(),
            },
            ServerEvent::Pong,
        ];
        for ev in events {
            let encoded = serde_json::to_string(&ev).unwrap();
            assert_eq!(serde_json::from_str::<ServerEvent>(&encoded).unwrap(), ev);
        }
    }

    #[test]
    fn transient_events_are_not_replayable() {
        assert!(!ServerEvent::Typing {
            room_id: room(),
            from: wallet()
        }
        .is_replayable());
        assert!(!ServerEvent::Pong.is_replayable());
        assert!(
            !ServerEvent::Shout {
                shout_id: "shout_1".into()
            }
            .is_replayable(),
            "a replayed shout would provoke fetches for expired banners"
        );
        assert!(ServerEvent::NewMessage {
            room_id: room(),
            msg_serial: 1
        }
        .is_replayable());
        assert!(ServerEvent::RoomsUpdated.is_replayable());
    }

    #[test]
    fn client_messages_parse_from_their_wire_form() {
        assert_eq!(
            serde_json::from_str::<ClientMessage>(r#"{"type":"ping"}"#).unwrap(),
            ClientMessage::Ping
        );
        assert_eq!(
            serde_json::from_str::<ClientMessage>(
                r#"{"type":"typing","roomId":"room_1749652739650_304e0eaf"}"#
            )
            .unwrap(),
            ClientMessage::Typing { room_id: room() }
        );
        assert!(serde_json::from_str::<ClientMessage>(r#"{"type":"delete_everything"}"#).is_err());
    }

    #[test]
    fn targets_round_trip_through_their_tagged_form() {
        let targets = [
            Target::Room { room_id: room() },
            Target::User { wallet: wallet() },
            Target::RoomExcept {
                room_id: room(),
                except: wallet(),
            },
            Target::All,
        ];
        for t in targets {
            let encoded = serde_json::to_string(&t).unwrap();
            assert_eq!(serde_json::from_str::<Target>(&encoded).unwrap(), t);
        }
    }
}
