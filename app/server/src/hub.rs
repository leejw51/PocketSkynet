//! The realtime fan-out hub (`docs/REALTIME.md` §9).
//!
//! One event model, three encodings: a [`ServerEvent`] is broadcast to
//! WebSocket connections, streamed to SSE connections, and appended to the
//! JSONL log with byte-identical JSON in all three places.
//!
//! Three properties are load-bearing.
//!
//! **The log is written before the fan-out.** Every published event gets its
//! `seq` from [`JsonlLog`] and is durable before any connection sees it, so
//! the log is a superset of what was delivered — never the reverse. A crash in
//! the gap costs a wake-up signal, and clients recover by syncing.
//!
//! **There is a room index.** The reference server iterated every open socket
//! for every event, which is O(total connections) per message. `by_room` makes
//! it O(recipients).
//!
//! **Envelopes carry their origin.** A connection drops any envelope whose
//! origin is in its block set, so a blocked user's activity produces no wake-up
//! at all. The reference delivered the wake-up and then served an empty sync,
//! which leaked "the person you blocked is active in this room" through timing.

use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::DashMap;
use pocketskynet_core::{ResyncReason, RoomId, ServerEvent, Target, WalletAddress};
use smallvec::SmallVec;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::db::{now_ms, rooms, users, Db};
use crate::jsonl::{JsonlLog, Kind};

/// Broadcast backlog. A connection that falls this far behind is told to
/// resync rather than being fed a partial history — correct, and bounded.
const BROADCAST_CAPACITY: usize = 1024;

/// Total connections the process will hold. Checked before authentication so
/// an unauthenticated flood cannot make the server do signature work.
pub const MAX_CONNECTIONS: usize = 5000;

/// Connections one wallet may hold. Checked after authentication, because it
/// needs the identity.
pub const MAX_PER_WALLET: usize = 8;

/// How many events a resuming SSE stream will replay before giving up and
/// asking for a full resync.
pub const MAX_REPLAY: usize = 1000;

pub type ConnId = u64;

#[derive(Debug, thiserror::Error)]
pub enum HubError {
    #[error("server at capacity")]
    AtCapacity,
    #[error("too many connections for this wallet")]
    TooManyConnections,
    #[error("event log: {0}")]
    Log(#[from] crate::jsonl::LogError),
    #[error("loading connection view: {0}")]
    View(#[source] Box<crate::error::ApiError>),
}

/// Why a resume could not be served exactly.
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    /// The cursor predates what the log still retains.
    #[error("cursor is older than the retained window")]
    CursorTooOld,
    #[error("event log: {0}")]
    Log(#[from] crate::jsonl::LogError),
}

/// A published event plus everything a connection needs to decide whether it
/// is a recipient. `event` is an `Arc` so a broadcast to N connections is one
/// allocation, not N.
#[derive(Debug, Clone)]
pub struct Envelope {
    pub seq: u64,
    pub at_ms: i64,
    pub target: Target,
    pub origin: Option<WalletAddress>,
    pub event: Arc<ServerEvent>,
}

/// A connection's authorisation snapshot: what it is subscribed to and who it
/// must not hear from. Replaced wholesale on refresh, never mutated in place,
/// so a delivery in flight always sees a consistent view.
#[derive(Debug, Clone, Default)]
pub struct ConnView {
    pub rooms: HashSet<RoomId>,
    /// Union of both directions: everyone the user blocked and everyone who
    /// blocked them. Presence must not cross a block either way.
    pub blocks: HashSet<WalletAddress>,
}

impl ConnView {
    /// Whether this connection should receive `env`.
    pub fn accepts(&self, env: &Envelope, me: &WalletAddress) -> bool {
        // Block filtering first: it applies regardless of how the event was
        // targeted, and it is the check that must never be skipped.
        if let Some(origin) = &env.origin {
            if self.blocks.contains(origin) {
                return false;
            }
        }

        match &env.target {
            Target::Room { room_id } => self.rooms.contains(room_id),
            Target::User { wallet } => wallet == me,
            Target::RoomExcept { room_id, except } => self.rooms.contains(room_id) && except != me,
            // Everyone — but the block check above already ran, so a shout
            // from someone this user blocked still produces no wake-up.
            Target::All => true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnKind {
    Ws,
    Sse,
}

/// Per-connection state the hub owns. The connection task owns the socket;
/// this is the part other tasks need to reach.
#[derive(Debug)]
pub struct ConnHandle {
    pub id: ConnId,
    pub wallet: WalletAddress,
    pub kind: ConnKind,
    /// Swapped, not locked: refreshing every socket of a user must not
    /// contend with the delivery path.
    pub view: Arc<ArcSwap<ConnView>>,
    /// `exp` from the bearer token, so a long-lived stream can be closed when
    /// its credential expires rather than outliving it.
    pub token_exp: Option<i64>,
    /// Last accepted typing relay, for the 1/s per-connection throttle.
    pub last_typing_at: AtomicI64,
    pub cancel: CancellationToken,
}

impl ConnHandle {
    pub fn new(
        id: ConnId,
        wallet: WalletAddress,
        kind: ConnKind,
        view: ConnView,
        token_exp: Option<i64>,
    ) -> Self {
        Self {
            id,
            wallet,
            kind,
            view: Arc::new(ArcSwap::from_pointee(view)),
            token_exp,
            last_typing_at: AtomicI64::new(0),
            cancel: CancellationToken::new(),
        }
    }

    pub fn view(&self) -> Arc<ConnView> {
        self.view.load_full()
    }

    /// Consume the typing budget. Returns `false` when the caller is inside
    /// the throttle window, in which case the relay is dropped silently — an
    /// error frame would be noisier than the signal it refuses.
    pub fn allow_typing(&self, now: i64, min_interval_ms: i64) -> bool {
        let last = self.last_typing_at.load(Ordering::Relaxed);
        if now - last < min_interval_ms {
            return false;
        }
        self.last_typing_at.store(now, Ordering::Relaxed);
        true
    }

    /// Whether the token backing this connection has expired.
    pub fn token_expired(&self, now_secs: i64) -> bool {
        self.token_exp.is_some_and(|exp| now_secs > exp)
    }
}

pub struct Hub {
    tx: broadcast::Sender<Envelope>,
    conns: DashMap<ConnId, Arc<ConnHandle>>,
    by_user: DashMap<WalletAddress, SmallVec<[ConnId; 4]>>,
    /// The index the reference server lacked.
    by_room: DashMap<RoomId, HashSet<ConnId>>,
    total: AtomicUsize,
    next_id: AtomicU64,
    log: Arc<JsonlLog>,
    db: Db,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hub")
            .field("connections", &self.total.load(Ordering::Relaxed))
            .field("rooms_indexed", &self.by_room.len())
            .finish_non_exhaustive()
    }
}

impl Hub {
    pub fn new(log: Arc<JsonlLog>, db: Db) -> Arc<Self> {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Arc::new(Self {
            tx,
            conns: DashMap::new(),
            by_user: DashMap::new(),
            by_room: DashMap::new(),
            total: AtomicUsize::new(0),
            next_id: AtomicU64::new(1),
            log,
            db,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Envelope> {
        self.tx.subscribe()
    }

    pub fn connection_count(&self) -> usize {
        self.total.load(Ordering::Relaxed)
    }

    /// Reject before doing signature work. Checked at the very start of a
    /// handshake, before the token is verified.
    pub fn has_capacity(&self) -> bool {
        self.connection_count() < MAX_CONNECTIONS
    }

    pub fn next_conn_id(&self) -> ConnId {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Build the connect-time snapshot for a wallet.
    pub async fn load_view(&self, wallet: &WalletAddress) -> Result<ConnView, HubError> {
        let address = wallet.as_str().to_owned();
        self.db
            .call(move |conn| {
                let room_ids = rooms::user_room_ids(conn, &address)?;
                let blocks = users::mutual_block_set(conn, &address)?;
                Ok(ConnView {
                    rooms: room_ids
                        .iter()
                        .filter_map(|r| RoomId::new(r).ok())
                        .collect(),
                    blocks: blocks
                        .iter()
                        .filter_map(|b| WalletAddress::new(b).ok())
                        .collect(),
                })
            })
            .await
            .map_err(|e| HubError::View(Box::new(e)))
    }

    /// Admit a connection, enforcing both caps.
    pub fn register(&self, conn: ConnHandle) -> Result<Arc<ConnHandle>, HubError> {
        if !self.has_capacity() {
            return Err(HubError::AtCapacity);
        }
        {
            let existing = self.by_user.get(&conn.wallet).map(|e| e.len()).unwrap_or(0);
            if existing >= MAX_PER_WALLET {
                return Err(HubError::TooManyConnections);
            }
        }

        let handle = Arc::new(conn);
        let id = handle.id;

        for room in &handle.view().rooms {
            self.by_room.entry(room.clone()).or_default().insert(id);
        }
        self.by_user
            .entry(handle.wallet.clone())
            .or_default()
            .push(id);
        self.conns.insert(id, handle.clone());
        self.total.fetch_add(1, Ordering::Relaxed);

        Ok(handle)
    }

    /// Remove a connection from every index. Idempotent: a connection task
    /// that hits several exit paths may call this more than once.
    pub fn unregister(&self, id: ConnId) {
        let Some((_, handle)) = self.conns.remove(&id) else {
            return;
        };
        self.total.fetch_sub(1, Ordering::Relaxed);

        for room in &handle.view().rooms {
            let now_empty = if let Some(mut set) = self.by_room.get_mut(room) {
                set.remove(&id);
                set.is_empty()
            } else {
                false
            };
            if now_empty {
                self.by_room.remove(room);
            }
        }

        let now_empty = if let Some(mut ids) = self.by_user.get_mut(&handle.wallet) {
            ids.retain(|existing| *existing != id);
            ids.is_empty()
        } else {
            false
        };
        if now_empty {
            self.by_user.remove(&handle.wallet);
        }

        handle.cancel.cancel();
    }

    /// Assign a `seq`, append to the log, then fan out.
    ///
    /// The append happens first and unconditionally: a delivered event that is
    /// not in the log would break `Last-Event-ID` resume, while a logged event
    /// that reached nobody is merely an ops signal (`fanout: 0`).
    pub async fn publish(
        &self,
        target: Target,
        origin: Option<WalletAddress>,
        event: ServerEvent,
    ) -> Result<u64, HubError> {
        let recipients = self.recipients(&target, origin.as_ref());
        let seq = self
            .log
            .append_event(&target, origin.as_ref(), &event, recipients as u32)?;

        let envelope = Envelope {
            seq,
            at_ms: now_ms(),
            target,
            origin,
            event: Arc::new(event),
        };
        // A send with no receivers is not an error: nobody is connected.
        let _ = self.tx.send(envelope);
        Ok(seq)
    }

    /// Publish, logging rather than propagating a failure.
    ///
    /// Used at the end of a request that has already committed its database
    /// work. Failing the response at that point would tell the client the
    /// write did not happen when it did; a lost wake-up costs one sync.
    pub async fn publish_best_effort(
        &self,
        target: Target,
        origin: Option<WalletAddress>,
        event: ServerEvent,
    ) {
        if let Err(e) = self.publish(target, origin, event).await {
            tracing::warn!(error = %e, "realtime publish failed; clients will recover by syncing");
        }
    }

    /// How many live connections would accept this envelope.
    fn recipients(&self, target: &Target, origin: Option<&WalletAddress>) -> usize {
        let probe = Envelope {
            seq: 0,
            at_ms: 0,
            target: target.clone(),
            origin: origin.cloned(),
            event: Arc::new(ServerEvent::Pong),
        };

        let ids: Vec<ConnId> = match target {
            Target::Room { room_id } | Target::RoomExcept { room_id, .. } => self
                .by_room
                .get(room_id)
                .map(|set| set.iter().copied().collect())
                .unwrap_or_default(),
            Target::User { wallet } => self
                .by_user
                .get(wallet)
                .map(|ids| ids.to_vec())
                .unwrap_or_default(),
            // O(connections), and deliberately so: this target exists for the
            // paid broadcast, which is rare by construction (each one costs a
            // real on-chain payment).
            Target::All => self.conns.iter().map(|e| *e.key()).collect(),
        };

        ids.iter()
            .filter_map(|id| self.conns.get(id))
            .filter(|handle| handle.view().accepts(&probe, &handle.wallet))
            .count()
    }

    /// Replace the view of every connection a wallet holds.
    ///
    /// Membership and blocks are re-read from the database rather than patched
    /// incrementally: the cost is one query, and a patched set that drifts
    /// from the database is a silent authorisation bug.
    pub async fn refresh_user(&self, wallet: &WalletAddress) -> Result<(), HubError> {
        let view = self.load_view(wallet).await?;
        self.set_user_view(wallet, view);
        Ok(())
    }

    /// Same as [`Hub::refresh_user`]; kept under the name the design document
    /// uses for the membership-change call site.
    pub async fn refresh_user_rooms(&self, wallet: &WalletAddress) -> Result<(), HubError> {
        self.refresh_user(wallet).await
    }

    /// Same, for the block-change call site. Blocks and rooms are loaded
    /// together because both are one cheap query and splitting them would let
    /// the two halves of a view disagree.
    pub async fn refresh_user_blocks(&self, wallet: &WalletAddress) -> Result<(), HubError> {
        self.refresh_user(wallet).await
    }

    /// Swap a fresh view into every connection of one wallet and reindex.
    pub fn set_user_view(&self, wallet: &WalletAddress, view: ConnView) {
        let ids: Vec<ConnId> = self
            .by_user
            .get(wallet)
            .map(|ids| ids.to_vec())
            .unwrap_or_default();

        let shared = Arc::new(view);
        for id in ids {
            let Some(handle) = self.conns.get(&id).map(|h| h.clone()) else {
                continue;
            };
            let previous = handle.view();
            handle.view.store(shared.clone());

            for room in previous.rooms.difference(&shared.rooms) {
                let now_empty = if let Some(mut set) = self.by_room.get_mut(room) {
                    set.remove(&id);
                    set.is_empty()
                } else {
                    false
                };
                if now_empty {
                    self.by_room.remove(room);
                }
            }
            for room in shared.rooms.difference(&previous.rooms) {
                self.by_room.entry(room.clone()).or_default().insert(id);
            }
        }
    }

    /// Replay retained events after `cursor` that this connection may see.
    ///
    /// Authorisation is applied at replay time against the caller's *current*
    /// view, never trusted from the log: membership may have changed during
    /// the gap, and the log records what was true then.
    pub fn replay_since(
        &self,
        cursor: u64,
        view: &ConnView,
        wallet: &WalletAddress,
        max: usize,
    ) -> Result<Vec<Envelope>, ReplayError> {
        let Some(records) = self.log.replay_since(cursor, max)? else {
            return Err(ReplayError::CursorTooOld);
        };

        let mut out = Vec::new();
        for record in records {
            if record.kind != Kind::Realtime {
                continue;
            }
            let (Some(event), Some(target)) = (record.server_event(), record.target.clone()) else {
                continue;
            };
            // Typing and pong are deliberately not replayable: a typing
            // indicator delivered a minute late is worse than one dropped.
            if !event.is_replayable() {
                continue;
            }

            let envelope = Envelope {
                seq: record.seq,
                at_ms: record.at_ms,
                target,
                origin: record.origin.clone(),
                event: Arc::new(event),
            };
            if view.accepts(&envelope, wallet) {
                out.push(envelope);
            }
        }
        Ok(out)
    }

    /// The newest sequence number the log has issued.
    ///
    /// Used as the `id:` of synthesised frames (resync, session expiry) so a
    /// resuming client's cursor lands on real ground rather than on a number
    /// no record ever carried.
    pub fn lagged_seq(&self) -> u64 {
        self.log.next_seq().saturating_sub(1)
    }

    /// The event a connection is sent when it falls behind the broadcast
    /// buffer. Degrading to "do a full sync" is always correct, because the
    /// events are wake-ups and the REST path is the authority.
    pub fn lagged_event(&self, from_seq: u64) -> ServerEvent {
        ServerEvent::ResyncRequired {
            reason: ResyncReason::Lagged,
            from_seq,
            to_seq: self.lagged_seq(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    fn tempdir(tag: &str) -> std::path::PathBuf {
        let mut buf = [0u8; 8];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut buf);
        let dir = std::env::temp_dir().join(format!("ps-hub-{tag}-{}", hex::encode(buf)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn hub(tag: &str) -> Arc<Hub> {
        let log = Arc::new(JsonlLog::open(tempdir(tag)).unwrap());
        Hub::new(log, test_db())
    }

    fn wallet(byte: &str) -> WalletAddress {
        WalletAddress::new(&format!("0x{}", byte.repeat(40))).unwrap()
    }

    fn room(tag: &str) -> RoomId {
        RoomId::new(&format!("room_1749652739650_{tag}")).unwrap()
    }

    fn view(rooms: &[RoomId], blocks: &[WalletAddress]) -> ConnView {
        ConnView {
            rooms: rooms.iter().cloned().collect(),
            blocks: blocks.iter().cloned().collect(),
        }
    }

    fn envelope(target: Target, origin: Option<WalletAddress>) -> Envelope {
        Envelope {
            seq: 1,
            at_ms: 0,
            target,
            origin,
            event: Arc::new(ServerEvent::RoomsUpdated),
        }
    }

    #[test]
    fn a_view_accepts_only_its_own_rooms() {
        let me = wallet("a");
        let v = view(&[room("one")], &[]);

        assert!(v.accepts(
            &envelope(
                Target::Room {
                    room_id: room("one")
                },
                None
            ),
            &me
        ));
        assert!(!v.accepts(
            &envelope(
                Target::Room {
                    room_id: room("two")
                },
                None
            ),
            &me
        ));
    }

    #[test]
    fn user_targeted_events_reach_only_that_wallet() {
        let me = wallet("a");
        let other = wallet("b");
        let v = view(&[], &[]);

        assert!(v.accepts(&envelope(Target::User { wallet: me.clone() }, None), &me));
        assert!(!v.accepts(&envelope(Target::User { wallet: other }, None), &me));
    }

    #[test]
    fn room_except_never_echoes_to_its_originator() {
        let me = wallet("a");
        let v = view(&[room("one")], &[]);

        let to_others = Target::RoomExcept {
            room_id: room("one"),
            except: me.clone(),
        };
        assert!(!v.accepts(&envelope(to_others, None), &me));

        let from_someone_else = Target::RoomExcept {
            room_id: room("one"),
            except: wallet("b"),
        };
        assert!(v.accepts(&envelope(from_someone_else, None), &me));
    }

    #[test]
    fn a_blocked_origin_produces_no_wake_up_at_all() {
        let me = wallet("a");
        let blocked = wallet("b");
        let v = view(&[room("one")], std::slice::from_ref(&blocked));

        let env = envelope(
            Target::Room {
                room_id: room("one"),
            },
            Some(blocked),
        );
        assert!(
            !v.accepts(&env, &me),
            "the reference woke the blocker and then served an empty sync"
        );

        let visible = envelope(
            Target::Room {
                room_id: room("one"),
            },
            Some(wallet("c")),
        );
        assert!(v.accepts(&visible, &me));
    }

    #[tokio::test]
    async fn publish_writes_the_log_before_fanning_out() {
        let hub = hub("log-first");

        let mut rx = hub.subscribe();
        let seq = hub
            .publish(
                Target::Room { room_id: room("x") },
                Some(wallet("a")),
                ServerEvent::NewMessage {
                    room_id: room("x"),
                    msg_serial: 42,
                },
            )
            .await
            .unwrap();

        let env = rx.try_recv().expect("the event should have been broadcast");
        assert_eq!(env.seq, seq);
        assert_eq!(env.origin, Some(wallet("a")));

        // The log already holds it, with the same seq the wire carried.
        let replayed = hub
            .replay_since(
                seq.saturating_sub(1),
                &view(&[room("x")], &[]),
                &wallet("b"),
                100,
            )
            .unwrap();
        assert!(replayed.iter().any(|e| e.seq == seq));
    }

    #[test]
    fn a_broadcast_reaches_everyone_except_across_a_block() {
        let me = wallet("a");

        // No room membership required: being connected is the authorisation.
        let roomless = view(&[], &[]);
        assert!(roomless.accepts(&envelope(Target::All, None), &me));
        assert!(roomless.accepts(&envelope(Target::All, Some(wallet("c"))), &me));

        // But a blocked origin still produces no wake-up — a paid megaphone
        // does not override a block.
        let blocked = wallet("b");
        let blocker = view(&[], std::slice::from_ref(&blocked));
        assert!(!blocker.accepts(&envelope(Target::All, Some(blocked)), &me));
    }

    #[tokio::test]
    async fn a_shout_fans_out_to_every_connection_the_sender_may_reach() {
        let hub = hub("shout-fanout");
        let sender = wallet("a");

        // Three listeners: one plain, one in some unrelated room, one who
        // has blocked the sender. No shared room anywhere.
        hub.register(ConnHandle::new(
            hub.next_conn_id(),
            wallet("b"),
            ConnKind::Ws,
            view(&[], &[]),
            None,
        ))
        .unwrap();
        hub.register(ConnHandle::new(
            hub.next_conn_id(),
            wallet("c"),
            ConnKind::Sse,
            view(&[room("elsewhere")], &[]),
            None,
        ))
        .unwrap();
        hub.register(ConnHandle::new(
            hub.next_conn_id(),
            wallet("d"),
            ConnKind::Ws,
            view(&[], std::slice::from_ref(&sender)),
            None,
        ))
        .unwrap();

        assert_eq!(
            hub.recipients(&Target::All, Some(&sender)),
            2,
            "everyone connected hears a shout, except across a block"
        );

        let mut rx = hub.subscribe();
        hub.publish(
            Target::All,
            Some(sender),
            ServerEvent::Shout {
                shout_id: "shout_1".into(),
            },
        )
        .await
        .unwrap();
        let env = rx.try_recv().expect("the broadcast must go out");
        assert_eq!(env.target, Target::All);

        // Not replayable: a resuming stream must not be fed month-old ash.
        let replayed = hub
            .replay_since(0, &view(&[], &[]), &wallet("b"), 100)
            .unwrap();
        assert!(replayed.is_empty());
    }

    #[tokio::test]
    async fn registration_enforces_the_per_wallet_cap() {
        let hub = hub("cap");
        let me = wallet("a");

        for _ in 0..MAX_PER_WALLET {
            let handle = ConnHandle::new(
                hub.next_conn_id(),
                me.clone(),
                ConnKind::Ws,
                view(&[room("one")], &[]),
                None,
            );
            hub.register(handle).unwrap();
        }

        let extra = ConnHandle::new(
            hub.next_conn_id(),
            me.clone(),
            ConnKind::Ws,
            ConnView::default(),
            None,
        );
        assert!(matches!(
            hub.register(extra),
            Err(HubError::TooManyConnections)
        ));

        // A different wallet is unaffected.
        let other = ConnHandle::new(
            hub.next_conn_id(),
            wallet("b"),
            ConnKind::Ws,
            ConnView::default(),
            None,
        );
        assert!(hub.register(other).is_ok());
    }

    #[tokio::test]
    async fn fanout_counts_only_connections_that_would_accept() {
        let hub = hub("fanout");
        let listener = wallet("a");
        let blocker = wallet("b");
        let sender = wallet("c");

        hub.register(ConnHandle::new(
            hub.next_conn_id(),
            listener,
            ConnKind::Ws,
            view(&[room("one")], &[]),
            None,
        ))
        .unwrap();
        hub.register(ConnHandle::new(
            hub.next_conn_id(),
            blocker,
            ConnKind::Sse,
            view(&[room("one")], std::slice::from_ref(&sender)),
            None,
        ))
        .unwrap();
        // Subscribed to a different room entirely.
        hub.register(ConnHandle::new(
            hub.next_conn_id(),
            wallet("d"),
            ConnKind::Ws,
            view(&[room("two")], &[]),
            None,
        ))
        .unwrap();

        let counted = hub.recipients(
            &Target::Room {
                room_id: room("one"),
            },
            Some(&sender),
        );
        assert_eq!(
            counted, 1,
            "the blocker and the outsider are not recipients"
        );
    }

    #[tokio::test]
    async fn unregistering_clears_every_index() {
        let hub = hub("unregister");
        let handle = hub
            .register(ConnHandle::new(
                hub.next_conn_id(),
                wallet("a"),
                ConnKind::Ws,
                view(&[room("one")], &[]),
                None,
            ))
            .unwrap();

        assert_eq!(hub.connection_count(), 1);
        hub.unregister(handle.id);
        assert_eq!(hub.connection_count(), 0);
        assert_eq!(
            hub.recipients(
                &Target::Room {
                    room_id: room("one")
                },
                None
            ),
            0
        );
        assert!(
            handle.cancel.is_cancelled(),
            "the task must be told to stop"
        );

        // Idempotent.
        hub.unregister(handle.id);
        assert_eq!(hub.connection_count(), 0);
    }

    #[tokio::test]
    async fn swapping_a_view_reindexes_the_room_membership() {
        let hub = hub("reindex");
        let me = wallet("a");
        hub.register(ConnHandle::new(
            hub.next_conn_id(),
            me.clone(),
            ConnKind::Ws,
            view(&[room("one")], &[]),
            None,
        ))
        .unwrap();

        assert_eq!(
            hub.recipients(
                &Target::Room {
                    room_id: room("one")
                },
                None
            ),
            1
        );

        hub.set_user_view(&me, view(&[room("two")], &[]));

        assert_eq!(
            hub.recipients(
                &Target::Room {
                    room_id: room("one")
                },
                None
            ),
            0,
            "a kicked member must stop receiving the room's events"
        );
        assert_eq!(
            hub.recipients(
                &Target::Room {
                    room_id: room("two")
                },
                None
            ),
            1
        );
    }

    #[tokio::test]
    async fn replay_refilters_against_the_current_view() {
        let hub = hub("replay-filter");
        let sender = wallet("c");

        // Sequence numbers start at FIRST_SEQ (1), so the first event published
        // is reachable from a zero cursor and no priming write is needed.
        hub.publish(
            Target::Room {
                room_id: room("one"),
            },
            Some(sender.clone()),
            ServerEvent::NewMessage {
                room_id: room("one"),
                msg_serial: 1,
            },
        )
        .await
        .unwrap();

        // A viewer who has since blocked the sender gets nothing, even though
        // the event was authorised for them when it was logged.
        let filtered = hub
            .replay_since(0, &view(&[room("one")], &[sender]), &wallet("a"), 100)
            .unwrap();
        assert!(filtered.is_empty());

        let unfiltered = hub
            .replay_since(0, &view(&[room("one")], &[]), &wallet("a"), 100)
            .unwrap();
        assert_eq!(unfiltered.len(), 1);
    }

    #[tokio::test]
    async fn transient_events_are_never_replayed() {
        let hub = hub("replay-transient");
        hub.publish(
            Target::Room {
                room_id: room("one"),
            },
            None,
            ServerEvent::Typing {
                room_id: room("one"),
                from: wallet("a"),
            },
        )
        .await
        .unwrap();

        let replayed = hub
            .replay_since(0, &view(&[room("one")], &[]), &wallet("b"), 100)
            .unwrap();
        assert!(
            replayed.is_empty(),
            "a stale typing indicator is worse than none"
        );
    }

    #[tokio::test]
    async fn a_cursor_beyond_the_replay_bound_is_refused_not_truncated() {
        let hub = hub("replay-bound");
        for i in 0..20 {
            hub.publish(
                Target::Room {
                    room_id: room("one"),
                },
                None,
                ServerEvent::NewMessage {
                    room_id: room("one"),
                    msg_serial: i,
                },
            )
            .await
            .unwrap();
        }

        let err = hub
            .replay_since(0, &view(&[room("one")], &[]), &wallet("a"), 5)
            .unwrap_err();
        assert!(matches!(err, ReplayError::CursorTooOld));
    }

    #[test]
    fn typing_is_throttled_to_one_per_interval() {
        let handle = ConnHandle::new(1, wallet("a"), ConnKind::Ws, ConnView::default(), None);

        assert!(handle.allow_typing(10_000, 1000));
        assert!(!handle.allow_typing(10_500, 1000), "inside the window");
        assert!(handle.allow_typing(11_100, 1000));
    }

    #[test]
    fn token_expiry_is_only_enforced_when_the_token_has_one() {
        let with_exp =
            ConnHandle::new(1, wallet("a"), ConnKind::Ws, ConnView::default(), Some(100));
        assert!(with_exp.token_expired(101));
        assert!(!with_exp.token_expired(99));

        let without = ConnHandle::new(2, wallet("a"), ConnKind::Ws, ConnView::default(), None);
        assert!(!without.token_expired(i64::MAX));
    }
}
