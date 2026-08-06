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
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::DashMap;
use pocketskynet_core::{PresenceStatus, ResyncReason, RoomId, ServerEvent, Target, WalletAddress};
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

/// How long a connection may go without a sign of life before its holder is
/// counted as away.
///
/// Five minutes, and it has to be well clear of the WebSocket ping cadence to
/// mean anything: the server pings every 30 s and any inbound frame — the
/// client's own 25 s keepalive included — counts as activity, so a socket that
/// is merely *open* never trips this. What trips it is a browser that has
/// throttled its timers because the tab went to the background, which is
/// exactly the person who is not there.
pub const AWAY_AFTER_MS: i64 = 5 * 60 * 1000;

/// How long a `PUT /api/presence` beacon counts for.
///
/// The SSE and polling tiers have no upstream channel, so their only way to
/// say "still here" is that request, which the client repeats every 60 s. Two
/// and a half times the cadence is the margin: a background tab's timers are
/// throttled rather than stopped, and a window tight enough to be caught out by
/// that throttling would flap somebody between here and gone while they read.
pub const BEACON_TTL_MS: i64 = 150 * 1000;

/// How often the sweeper re-derives everyone's status.
///
/// The only transition it exists to catch is online → away, which has a
/// five-minute threshold, so a slower tick would be indistinguishable to a
/// reader and a faster one would be pure work. Connect, disconnect and an
/// explicit declaration all announce immediately and do not wait for this.
pub const PRESENCE_SWEEP: std::time::Duration = std::time::Duration::from_secs(30);

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
            // One shared room is enough, and it is tested against *this*
            // connection's current subscriptions — so somebody who joined a
            // second ago is authorised for the very next presence event, and
            // somebody kicked a second ago is not.
            Target::Rooms { room_ids } => room_ids.iter().any(|r| self.rooms.contains(r)),
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
    /// Last sign of life on this connection: the handshake itself, then every
    /// inbound frame. Drives the idle half of presence.
    last_active_at: AtomicI64,
    /// The client said its tab is in the background. Per-connection rather
    /// than per-wallet, because that is what it describes: the laptop tab you
    /// switched away from says nothing about the phone in your hand.
    declared_away: AtomicBool,
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
            // Opening a connection is itself the freshest possible sign of
            // life, so a new connection starts online rather than serving out
            // an idle window it never sat through.
            last_active_at: AtomicI64::new(crate::db::now_ms()),
            declared_away: AtomicBool::new(false),
            cancel: CancellationToken::new(),
        }
    }

    pub fn view(&self) -> Arc<ConnView> {
        self.view.load_full()
    }

    /// Note a sign of life. Cheap enough to call on every inbound frame.
    ///
    /// A frame also clears a previous `away` declaration: a client that is
    /// talking to us is a client whose tab came back, and the alternative —
    /// requiring an explicit `online` to undo an `away` — leaves anyone whose
    /// "I'm back" frame was lost stuck as away until they reconnect.
    pub fn mark_active(&self, now: i64) {
        self.last_active_at.store(now, Ordering::Relaxed);
        self.declared_away.store(false, Ordering::Relaxed);
    }

    /// Record what the client says about itself.
    ///
    /// `Online` is treated as activity, not merely as a flag, because that is
    /// what it means: the tab is in the foreground again.
    pub fn declare(&self, status: PresenceStatus, now: i64) {
        match status {
            PresenceStatus::Away => self.declared_away.store(true, Ordering::Relaxed),
            // Offline is refused upstream (`PresenceStatus::is_declarable`);
            // treating it as presence here would be the wrong half of the
            // check to rely on, so it lands with online rather than silently
            // meaning something.
            PresenceStatus::Online | PresenceStatus::Offline => self.mark_active(now),
        }
    }

    /// This one connection's contribution to its holder's status.
    ///
    /// Never `Offline`: the connection exists, which is precisely what offline
    /// is the absence of.
    pub fn presence(&self, now: i64) -> PresenceStatus {
        if self.declared_away.load(Ordering::Relaxed) {
            return PresenceStatus::Away;
        }
        if now - self.last_active_at.load(Ordering::Relaxed) >= AWAY_AFTER_MS {
            return PresenceStatus::Away;
        }
        PresenceStatus::Online
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
    /// The last status announced for each wallet, so a re-derivation that
    /// changes nothing publishes nothing.
    ///
    /// Only non-offline entries are held: offline is the absence of a
    /// connection, so it is also the absence of a row, and the map stays
    /// proportional to who is actually here rather than to who has ever
    /// logged in. It is *not* durable, deliberately — a presence record that
    /// survives the process is a log of when people were at their desks, and
    /// nothing in this feature needs one.
    presence: DashMap<WalletAddress, PresenceStatus>,
    /// The most recent `PUT /api/presence` from a wallet holding **no**
    /// connection, with its arrival time.
    ///
    /// A stand-in for the connection a polling client does not have, and
    /// nothing more: an entry exists only while its owner has no real one.
    /// That invariant is the whole design — a beacon can only be *expired*
    /// (there is no socket whose closing could retract it), so one allowed to
    /// coexist with a connection would outlive it by up to
    /// [`BEACON_TTL_MS`] and keep somebody lit for two and a half minutes
    /// after they shut their laptop. [`Hub::register`] retires it on the way
    /// in and [`Hub::beacon`] declines to write one on the way past.
    beacons: DashMap<WalletAddress, (PresenceStatus, i64)>,
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
            presence: DashMap::new(),
            beacons: DashMap::new(),
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

        // A real connection retires the stand-in. Clients on the WebSocket tier
        // beacon once during start-up, before they know which transport they
        // will get, and an entry left behind by that would keep them present
        // for [`BEACON_TTL_MS`] after the socket it was superseded by closed.
        self.beacons.remove(&handle.wallet);

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
            // Deduplicated across the rooms, or somebody who shares three
            // rooms with the subject would be counted three times and the
            // logged fan-out would be a number that means nothing.
            Target::Rooms { room_ids } => {
                let mut seen = HashSet::new();
                for room_id in room_ids {
                    if let Some(set) = self.by_room.get(room_id) {
                        seen.extend(set.iter().copied());
                    }
                }
                seen.into_iter().collect()
            }
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

    // ------------------------------------------------------------ presence ---

    /// What this wallet's devices currently add up to.
    ///
    /// The maximum across them, so the most present device wins — see
    /// [`PresenceStatus`]. A live connection is one device; a fresh beacon from
    /// a client with no upstream channel is another. Offline is what is left
    /// when a wallet has neither, which is why it is not a value any client can
    /// claim for itself.
    pub fn derive_presence(&self, wallet: &WalletAddress, now: i64) -> PresenceStatus {
        let ids: Vec<ConnId> = self
            .by_user
            .get(wallet)
            .map(|ids| ids.to_vec())
            .unwrap_or_default();

        let from_connections = ids
            .iter()
            .filter_map(|id| self.conns.get(id))
            .map(|handle| handle.presence(now))
            .max()
            .unwrap_or(PresenceStatus::Offline);

        let from_beacon = self
            .beacons
            .get(wallet)
            .filter(|entry| now - entry.1 < BEACON_TTL_MS)
            .map(|entry| entry.0)
            .unwrap_or(PresenceStatus::Offline);

        from_connections.max(from_beacon)
    }

    /// Record a `PUT /api/presence` and republish if it moved the needle.
    ///
    /// The declaration is applied to the caller's live connections *as well as*
    /// the beacon map, which is what makes it work on SSE: an SSE stream is
    /// silent by construction, so without this its handshake timestamp would
    /// age past the idle threshold and a reader five minutes into a thread
    /// would be reported away with the stream still open.
    pub async fn beacon(&self, wallet: &WalletAddress, status: PresenceStatus) {
        let now = now_ms();
        let ids: Vec<ConnId> = self
            .by_user
            .get(wallet)
            .map(|ids| ids.to_vec())
            .unwrap_or_default();

        if ids.is_empty() {
            // Nothing else speaks for this wallet, so the beacon does.
            self.beacons.insert(wallet.clone(), (status, now));
        } else {
            for id in &ids {
                if let Some(handle) = self.conns.get(id) {
                    handle.declare(status, now);
                }
            }
            // No entry written, and any earlier one dropped: a connection can
            // be closed and a beacon can only time out, so letting the two
            // coexist would mean the slower of them decided when somebody left.
            self.beacons.remove(wallet);
        }

        self.announce_presence(wallet).await;
    }

    /// The last status announced for a wallet — what a snapshot request should
    /// answer, and what a client that just received an event already believes.
    pub fn announced_presence(&self, wallet: &WalletAddress) -> PresenceStatus {
        self.presence
            .get(wallet)
            .map(|s| *s)
            .unwrap_or(PresenceStatus::Offline)
    }

    /// Everyone currently non-offline, for the snapshot endpoint to filter.
    ///
    /// Returned whole rather than filtered here because the filter is an
    /// authorisation decision — shared rooms and blocks — and this module has
    /// neither the caller nor the database query in hand. `routes/presence.rs`
    /// owns that, in one place, the way every other authorisation in this
    /// server is done once at the REST boundary.
    pub fn present_wallets(&self) -> Vec<(WalletAddress, PresenceStatus)> {
        self.presence
            .iter()
            .map(|e| (e.key().clone(), *e.value()))
            .collect()
    }

    /// Re-derive one wallet's status and, if it moved, tell everyone who shares
    /// a room with them.
    ///
    /// Publishing only on a change is what keeps this cheap: a client pings
    /// every 25 seconds and each ping marks activity, but activity that does
    /// not cross a threshold produces no event and no log record.
    ///
    /// Two announcements racing can in principle land in the opposite order to
    /// the transitions that caused them. That is tolerable here and nowhere
    /// else in this file, because presence has an authority a client can ask —
    /// `GET /api/presence` — and every client calls it when a stream comes up.
    pub async fn announce_presence(&self, wallet: &WalletAddress) {
        let derived = self.derive_presence(wallet, now_ms());

        let changed = match derived {
            PresenceStatus::Offline => self.presence.remove(wallet).is_some(),
            live => self.presence.insert(wallet.clone(), live) != Some(live),
        };
        if !changed {
            return;
        }

        // Membership comes from the database, not from the connection views:
        // an SSE stream narrowed with `?room=` subscribes to one room, and
        // announcing a departure only to that room would leave every other
        // room showing a ghost until its members happened to refetch.
        let address = wallet.as_str().to_owned();
        let room_ids = match self
            .db
            .call(move |conn| rooms::user_room_ids(conn, &address))
            .await
        {
            Ok(ids) => ids
                .iter()
                .filter_map(|r| RoomId::new(r).ok())
                .collect::<Vec<_>>(),
            Err(e) => {
                tracing::warn!(error = %e, "could not resolve rooms for a presence change");
                return;
            }
        };
        // Somebody in no rooms has nobody to tell. Publishing anyway would be
        // a log record with a fan-out of zero, every time they open a tab.
        if room_ids.is_empty() {
            return;
        }

        self.publish_best_effort(
            Target::Rooms { room_ids },
            Some(wallet.clone()),
            ServerEvent::Presence {
                wallet: wallet.clone(),
                status: derived,
            },
        )
        .await;
    }

    /// Re-derive everyone who is currently held to be present.
    ///
    /// The one transition nothing else can catch: online → away happens
    /// because *no* frame arrived, and an absence raises no event. Only the
    /// announced set is walked, so the cost is proportional to who is online
    /// rather than to how many accounts exist.
    pub async fn sweep_presence(&self) {
        let now = now_ms();
        // Expired beacons go first, so the derivation below sees the state the
        // announcement will be made against rather than one tick behind it.
        self.beacons.retain(|_, (_, at)| now - *at < BEACON_TTL_MS);

        let stale: Vec<WalletAddress> = self
            .presence
            .iter()
            .filter(|e| self.derive_presence(e.key(), now) != *e.value())
            .map(|e| e.key().clone())
            .collect();

        for wallet in stale {
            self.announce_presence(&wallet).await;
        }
    }

    /// Note activity on a connection and announce if it moved the needle.
    ///
    /// The comparison against the announced status is done before any await,
    /// so the overwhelmingly common case — a keepalive from somebody already
    /// online — costs two relaxed atomic stores and a map lookup.
    pub async fn note_activity(&self, handle: &ConnHandle) {
        handle.mark_active(now_ms());
        if self.announced_presence(&handle.wallet) != PresenceStatus::Online {
            self.announce_presence(&handle.wallet).await;
        }
    }

    /// Apply a client's own declaration about one of its connections.
    pub async fn declare_presence(&self, handle: &ConnHandle, status: PresenceStatus) {
        handle.declare(status, now_ms());
        self.announce_presence(&handle.wallet).await;
    }

    /// Drop a connection and republish its holder's presence.
    ///
    /// The pair belongs together: every path that closes a connection has to
    /// re-derive presence, and a separate call would eventually be forgotten on
    /// one of the six exits the SSE and WebSocket tasks have between them.
    pub async fn disconnect(&self, id: ConnId) {
        let wallet = self.conns.get(&id).map(|handle| handle.wallet.clone());
        self.unregister(id);
        if let Some(wallet) = wallet {
            self.announce_presence(&wallet).await;
        }
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
    fn a_presence_target_needs_only_one_room_in_common() {
        let me = wallet("a");
        let v = view(&[room("one")], &[]);

        // One shared room out of three is enough.
        assert!(v.accepts(
            &envelope(
                Target::Rooms {
                    room_ids: vec![room("two"), room("one"), room("three")]
                },
                None
            ),
            &me
        ));
        // None in common: a stranger's presence is not this connection's news.
        assert!(!v.accepts(
            &envelope(
                Target::Rooms {
                    room_ids: vec![room("two"), room("three")]
                },
                None
            ),
            &me
        ));
        // And a block still wins, exactly as it does for typing: presence is
        // the same activity oracle in a slower coat.
        let blocked = wallet("b");
        let blocker = view(&[room("one")], std::slice::from_ref(&blocked));
        assert!(!blocker.accepts(
            &envelope(
                Target::Rooms {
                    room_ids: vec![room("one")]
                },
                Some(blocked)
            ),
            &me
        ));
    }

    #[tokio::test]
    async fn presence_counts_each_recipient_once_however_many_rooms_are_shared() {
        let hub = hub("presence-fanout");
        // Two connections: one sharing three rooms with the subject, one none.
        hub.register(ConnHandle::new(
            hub.next_conn_id(),
            wallet("b"),
            ConnKind::Ws,
            view(&[room("one"), room("two"), room("three")], &[]),
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

        assert_eq!(
            hub.recipients(
                &Target::Rooms {
                    room_ids: vec![room("one"), room("two"), room("three")]
                },
                None
            ),
            1,
            "a colleague in three shared rooms is one recipient, not three"
        );
    }

    #[test]
    fn a_connection_is_online_until_it_goes_quiet() {
        let handle = ConnHandle::new(1, wallet("a"), ConnKind::Ws, ConnView::default(), None);

        handle.mark_active(1_000_000);
        assert_eq!(handle.presence(1_000_000), PresenceStatus::Online);
        assert_eq!(
            handle.presence(1_000_000 + AWAY_AFTER_MS - 1),
            PresenceStatus::Online
        );
        assert_eq!(
            handle.presence(1_000_000 + AWAY_AFTER_MS),
            PresenceStatus::Away
        );
    }

    #[test]
    fn a_declaration_beats_the_idle_timer_in_both_directions() {
        let handle = ConnHandle::new(1, wallet("a"), ConnKind::Ws, ConnView::default(), None);

        // Hidden tab: away immediately, without waiting out five minutes.
        handle.declare(PresenceStatus::Away, 1_000_000);
        assert_eq!(handle.presence(1_000_000), PresenceStatus::Away);

        // And any frame at all undoes it — requiring an explicit `online` to
        // clear it would strand anyone whose "I'm back" was lost.
        handle.mark_active(1_000_100);
        assert_eq!(handle.presence(1_000_100), PresenceStatus::Online);
    }

    #[tokio::test]
    async fn the_most_present_device_decides() {
        let hub = hub("presence-devices");
        let me = wallet("a");
        let now = crate::db::now_ms();

        assert_eq!(
            hub.derive_presence(&me, now),
            PresenceStatus::Offline,
            "no connections and no beacon is the only way to be offline"
        );

        let phone = hub
            .register(ConnHandle::new(
                hub.next_conn_id(),
                me.clone(),
                ConnKind::Ws,
                ConnView::default(),
                None,
            ))
            .unwrap();
        let laptop = hub
            .register(ConnHandle::new(
                hub.next_conn_id(),
                me.clone(),
                ConnKind::Ws,
                ConnView::default(),
                None,
            ))
            .unwrap();

        phone.declare(PresenceStatus::Away, now);
        laptop.mark_active(now);
        assert_eq!(
            hub.derive_presence(&me, now),
            PresenceStatus::Online,
            "a phone idling in a pocket must not drag down the laptop in use"
        );

        laptop.declare(PresenceStatus::Away, now);
        assert_eq!(hub.derive_presence(&me, now), PresenceStatus::Away);

        // Closing every connection is what offline means.
        hub.unregister(phone.id);
        hub.unregister(laptop.id);
        assert_eq!(hub.derive_presence(&me, now), PresenceStatus::Offline);
    }

    #[tokio::test]
    async fn a_beacon_stands_in_for_a_connection_and_then_expires() {
        let hub = hub("presence-beacon-ttl");
        let me = wallet("a");

        hub.beacon(&me, PresenceStatus::Online).await;
        let now = crate::db::now_ms();
        assert_eq!(
            hub.derive_presence(&me, now),
            PresenceStatus::Online,
            "the polling tier holds no connection, so the beacon is all it has"
        );

        // One tick past the window and the stand-in stops counting, which is
        // how a client that simply stopped calling stops being present.
        assert_eq!(
            hub.derive_presence(&me, now + BEACON_TTL_MS),
            PresenceStatus::Offline
        );
    }

    /// A beacon is a stand-in, and a stand-in stands down when the real thing
    /// arrives. Both orderings, because both happen: a WebSocket client
    /// beacons once during start-up *before* it knows its transport, and an
    /// SSE client beacons every minute *while* its stream is open.
    ///
    /// The failure this pins is not subtle once seen. A beacon can only expire
    /// — there is no socket whose closing retracts it — so one left beside a
    /// connection decides when its owner leaves, and the answer it gives is
    /// "two and a half minutes after they shut the lid".
    #[tokio::test]
    async fn a_connection_retires_the_beacon_that_stood_in_for_it() {
        let hub = hub("presence-supersede");
        let me = wallet("a");

        // Start-up order: beacon first, connection second.
        hub.beacon(&me, PresenceStatus::Online).await;
        let conn = hub
            .register(ConnHandle::new(
                hub.next_conn_id(),
                me.clone(),
                ConnKind::Ws,
                ConnView::default(),
                None,
            ))
            .unwrap();
        hub.unregister(conn.id);
        assert_eq!(
            hub.derive_presence(&me, now_ms()),
            PresenceStatus::Offline,
            "closing the socket must end the session, not hand it back to a stale beacon"
        );

        // SSE order: connection first, beacon after — the heartbeat case.
        let conn = hub
            .register(ConnHandle::new(
                hub.next_conn_id(),
                me.clone(),
                ConnKind::Sse,
                ConnView::default(),
                None,
            ))
            .unwrap();
        hub.beacon(&me, PresenceStatus::Online).await;
        hub.unregister(conn.id);
        assert_eq!(
            hub.derive_presence(&me, now_ms()),
            PresenceStatus::Offline,
            "a heartbeat refreshes the stream it was sent over; it does not outlive it"
        );

        // With no connection at all, the beacon is still the only evidence
        // there is — that is what it is for.
        hub.beacon(&me, PresenceStatus::Online).await;
        assert_eq!(hub.derive_presence(&me, now_ms()), PresenceStatus::Online);
    }

    #[tokio::test]
    async fn a_status_that_did_not_change_publishes_nothing() {
        let hub = hub("presence-quiet");
        let me = wallet("a");
        let mut rx = hub.subscribe();

        // Nobody is in a room, so nothing is published either way — the point
        // here is the announced-state bookkeeping underneath.
        hub.beacon(&me, PresenceStatus::Online).await;
        assert_eq!(hub.announced_presence(&me), PresenceStatus::Online);

        hub.beacon(&me, PresenceStatus::Online).await;
        assert_eq!(hub.announced_presence(&me), PresenceStatus::Online);
        assert!(
            rx.try_recv().is_err(),
            "a repeated keepalive must not become an event"
        );
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
