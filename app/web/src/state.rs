//! The single application store: one `use_reducer` shared by every screen.
//!
//! Yew's context is the only sane place for state that outlives a route change
//! — the room list, the decrypted message caches and the WebSocket subscription
//! all have to survive navigating between `/rooms/:id` and `/settings`, which
//! rules out per-component state.
//!
//! [`AppState`] compares by a monotonic `revision` rather than structurally.
//! Structural equality would mean deep-comparing every message in every room on
//! every dispatch, and half of what it holds (key bundles) is deliberately not
//! comparable at all.

use std::collections::HashMap;
use std::rc::Rc;

use pocketskynet_core::chain::Network;
use pocketskynet_core::{MessageId, RoomId, WalletAddress};
use yew::prelude::*;

use crate::api::{BlockchainInfo, BlockedUser, Client, Invitation, Message, RoomWithMembers};
use crate::crypto::RoomKeyBundle;
use crate::i18n::Lang;
use crate::realtime::{ConnStatus, Transport, TypingTracker};
use crate::session::{Auth, ConnectionMode, FontFace, FontScale, ShellLayout, Theme};
use crate::store::{BlockSet, RoomState};

/// A four-state async result. `Idle` and `Loading` render differently — the
/// spec forbids showing a loader before 400 ms, so "not started" must be
/// distinguishable from "in flight" (DESIGN.md §15).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Load {
    #[default]
    Idle,
    Loading,
    Ready,
    Error(String),
}

/// Toast severity (DESIGN.md §15). `Error` never auto-dismisses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Neutral,
    Success,
    Error,
    Warn,
    Info,
}

impl ToastKind {
    pub fn class(self) -> &'static str {
        match self {
            ToastKind::Neutral => "fn-toast",
            ToastKind::Success => "fn-toast fn-toast--success",
            ToastKind::Error => "fn-toast fn-toast--error",
            ToastKind::Warn => "fn-toast fn-toast--warn",
            ToastKind::Info => "fn-toast fn-toast--info",
        }
    }

    /// Errors are assertive; everything else is polite.
    pub fn live_region(self) -> (&'static str, &'static str) {
        match self {
            ToastKind::Error => ("alert", "assertive"),
            _ => ("status", "polite"),
        }
    }

    /// Auto-dismiss delay in milliseconds, or `None` to stay until dismissed.
    pub fn ttl_ms(self, has_description: bool) -> Option<u32> {
        match self {
            // An error the user did not read is an error that did not happen,
            // as far as they are concerned.
            ToastKind::Error => None,
            _ if has_description => Some(6_000),
            _ => Some(4_000),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Toast {
    pub id: u64,
    pub kind: ToastKind,
    pub title: String,
    pub description: Option<String>,
}

/// An outbound message that has not been acknowledged yet.
///
/// Kept out of [`RoomState`] because it is not part of the server's state
/// transfer: it is purely local, it has no `msgSerial`, and folding a `/sync`
/// batch must never touch it.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    /// A client-side id, distinct from any server id.
    pub local_id: u64,
    pub plaintext: String,
    pub failed: Option<String>,
    pub created_ms: i64,
}

/// Which dialog is open, if any. Modelled as one value rather than a bag of
/// booleans so two modals can never be open at once — a state the focus trap
/// has no sensible answer for.
#[derive(Debug, Clone, PartialEq)]
pub enum Modal {
    CreateRoom,
    Invite(RoomId),
    ManageAdmins(RoomId),
    Blocked,
    HiddenRooms,
    RenameRoom(RoomId, String),
    /// A destructive confirmation: title, body, and the action to run.
    Confirm(Confirm),
    /// The message to delete, with its plaintext (when readable) so the
    /// confirm dialog can quote what is about to be destroyed.
    DeleteMessage(MessageId, Option<String>),
    /// The wallet: balances, network switcher, send funds (DESIGN.md §10).
    Wallet,
    /// Compose a paid broadcast (docs/API.md §16.1).
    Shout,
    /// Where this server is and which transport is carrying this session.
    ServerInfo,
    /// Everything the phone's five-slot bottom nav cannot show (dialogs/more.rs).
    More,
    /// The AI assistant, scoped to the room it will draft for.
    Assistant(RoomId),
    /// The Files drawer for one room's attachments (docs/API.md §14).
    Files(RoomId),
}

/// A destructive confirmation. Every one of these names the object and states
/// the consequence — `window.confirm` appears nowhere in this client.
#[derive(Debug, Clone, PartialEq)]
pub struct Confirm {
    pub title: String,
    pub body: String,
    pub confirm_label: String,
    pub action: ConfirmAction,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConfirmAction {
    LeaveRoom(RoomId),
    DeleteRoom(RoomId),
    HideRoom(RoomId),
    DeleteAllMessages(RoomId),
    KickMember(RoomId, WalletAddress),
    BlockUser(WalletAddress),
    UnblockUser(WalletAddress),
    RemoveAdmin(RoomId, WalletAddress),
    EraseLocalData,
    /// Forget the credential this device was told to remember (`crate::vault`),
    /// without signing out. Confirmed, because the next reload will then ask
    /// for a recovery phrase the user may not have to hand.
    ForgetWallet,
    SignOut,
}

/// Everything the app knows.
pub struct AppState {
    /// Bumped by every action; the sole basis for `PartialEq`.
    revision: u64,

    pub auth: Auth,
    pub client: Client,
    pub chain: BlockchainInfo,
    /// The multi-chain registry from `GET /api/networks`. `Rc` because every
    /// dispatch clones the whole state and the registry never changes after
    /// boot.
    pub networks: Rc<Vec<Network>>,

    pub rooms: Vec<RoomWithMembers>,
    pub rooms_load: Load,
    pub invitations: Vec<Invitation>,
    pub invitations_load: Load,
    pub blocked: Vec<BlockedUser>,
    pub blocks: BlockSet,

    pub room_states: HashMap<RoomId, RoomState>,
    pub room_load: HashMap<RoomId, Load>,
    /// Unwrapped epoch keys per room. `Rc` because a bundle is not `Clone` and
    /// re-deriving one costs elliptic-curve work per epoch.
    pub bundles: HashMap<RoomId, Rc<RoomKeyBundle>>,
    pub pending: HashMap<RoomId, Vec<Pending>>,

    pub conn: ConnStatus,
    pub online: bool,
    pub typing: TypingTracker,
    pub mode: ConnectionMode,
    pub theme: Theme,
    /// Two-pane arrangement on wide viewports (`ps-shell-layout`).
    pub shell_layout: ShellLayout,
    pub language: Lang,
    /// Interface typeface (`ps-font`).
    pub font_face: FontFace,
    /// Interface text size (`ps-font-scale`).
    pub font_scale: FontScale,

    pub toasts: Vec<Toast>,
    pub modal: Option<Modal>,
    /// A verified session waiting behind the boot cutscene. The login screen
    /// holds its own copy of this state for the sign-in path; this one exists
    /// for the vault unlock on reload, which runs outside the login screen
    /// (`app.rs`) and would otherwise skip the sequence entirely.
    pub pending_boot: Option<crate::session::Session>,
    /// Messages currently playing their disintegration. Marked the instant
    /// delete is confirmed rather than when the server answers, so the effect
    /// covers the round trip instead of following it — and pruned by the fold
    /// once the row it refers to is actually gone.
    pub dissolving: std::collections::HashSet<MessageId>,
    /// What the Knowledge page should open with — set by a hashtag click or
    /// "Teach from message" *before* navigating there, taken (and cleared) by
    /// the page on mount. A field rather than a route parameter because the
    /// teach seed carries message content, which has no business in a URL.
    pub knowledge_seed: Option<KnowledgeSeed>,
    next_id: u64,
}

/// See [`AppState::knowledge_seed`].
#[derive(Debug, Clone, PartialEq)]
pub enum KnowledgeSeed {
    /// Open the search tab with this query already run ("#tag" from a chip,
    /// or the main page's quick bar). `ask` auto-escalates to an AI answer
    /// once results arrive — the quick bar's "AI SEARCH" promise.
    Search { query: String, ask: bool },
    /// Open the teach tab pre-filled from a message.
    Teach {
        content: String,
        room_id: Option<RoomId>,
        message_id: Option<MessageId>,
    },
}

impl PartialEq for AppState {
    fn eq(&self, other: &Self) -> bool {
        self.revision == other.revision
    }
}

impl AppState {
    pub fn new(auth: Auth, client: Client) -> Self {
        Self {
            revision: 0,
            auth,
            client,
            chain: BlockchainInfo::default(),
            networks: Rc::new(Vec::new()),
            rooms: Vec::new(),
            rooms_load: Load::Idle,
            invitations: Vec::new(),
            invitations_load: Load::Idle,
            blocked: Vec::new(),
            blocks: BlockSet::default(),
            room_states: HashMap::new(),
            room_load: HashMap::new(),
            bundles: HashMap::new(),
            pending: HashMap::new(),
            conn: ConnStatus::Offline,
            online: true,
            typing: TypingTracker::default(),
            mode: ConnectionMode::WebSocket,
            theme: Theme::System,
            shell_layout: ShellLayout::load(),
            language: Lang::load(),
            font_face: FontFace::load(),
            font_scale: FontScale::load(),
            toasts: Vec::new(),
            modal: None,
            pending_boot: None,
            dissolving: std::collections::HashSet::new(),
            knowledge_seed: None,
            next_id: 1,
        }
    }

    pub fn me(&self) -> Option<&WalletAddress> {
        self.auth.address()
    }

    /// The chain this deployment runs on, as reported by the server.
    ///
    /// Not a preference: `GET /api/networks` returns exactly the chain the
    /// server is configured for, so there is nothing to select between and
    /// nothing to persist. `None` only before that call has answered.
    pub fn active_network(&self) -> Option<&Network> {
        self.networks.first()
    }

    pub fn room(&self, id: &RoomId) -> Option<&RoomWithMembers> {
        self.rooms.iter().find(|r| r.id() == id)
    }

    /// Rooms in display order: most recent activity first. The server returns
    /// insertion order, which is not a room list.
    pub fn sorted_rooms(&self) -> Vec<&RoomWithMembers> {
        let mut v: Vec<&RoomWithMembers> = self.rooms.iter().collect();
        v.sort_by_key(|r| std::cmp::Reverse(r.activity_ts()));
        v
    }

    /// Total unread across every room, for the bottom-nav badge.
    pub fn total_unread(&self) -> u32 {
        self.rooms.iter().filter_map(|r| r.unread_count).sum()
    }

    pub fn pending_invitations(&self) -> u32 {
        self.invitations.len() as u32
    }

    pub fn room_state(&self, id: &RoomId) -> Option<&RoomState> {
        self.room_states.get(id)
    }

    pub fn bundle(&self, id: &RoomId) -> Option<&Rc<RoomKeyBundle>> {
        self.bundles.get(id)
    }

    /// Whether this room can accept an encrypted post right now.
    pub fn can_post_encrypted(&self, id: &RoomId) -> bool {
        self.post_block(id).is_none()
    }

    /// *Why* an encrypted room cannot be posted to, when it cannot.
    ///
    /// These three used to collapse into one boolean behind a single message,
    /// "Rotate the room key to post". That is right for exactly one of them:
    /// a user whose keys simply are not on this device was being told to
    /// perform an admin action that would not have helped, instead of to
    /// unlock. The remedies are unrelated, so the reason has to survive.
    pub fn post_block(&self, id: &RoomId) -> Option<PostBlock> {
        let room = self.room(id)?;
        if !room.has_encryption {
            return None;
        }

        // No keys at all for an encrypted room means the session was restored
        // without them — a reload, since keys are deliberately never persisted.
        let Some(bundle) = self.bundle(id) else {
            return Some(PostBlock::Locked);
        };
        if bundle.is_empty() {
            return Some(PostBlock::Locked);
        }

        // Checked before the epoch comparison: while rotation is pending the
        // current epoch is by definition unreachable, and "rotate" is the
        // actionable half of that.
        if room.room.key_rotation_pending {
            return Some(PostBlock::RotationPending);
        }

        match bundle.latest().map(|(v, _)| v) {
            Some(v) if v == room.room.current_key_version => None,
            _ => Some(PostBlock::StaleEpoch),
        }
    }
}

/// Why the composer is inert in an encrypted room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostBlock {
    /// This device holds no key for the room. The user unlocks by re-entering
    /// their recovery phrase or private key.
    Locked,
    /// An admin must rotate the room key before anyone can post.
    RotationPending,
    /// A key is held, but for an older epoch than the room's current one; the
    /// client fetches the newer wrap on its own.
    StaleEpoch,
}

impl PostBlock {
    /// Placeholder text — it must name the action that actually unblocks the
    /// user, because it is the only explanation the composer gives.
    pub fn composer_hint(self) -> &'static str {
        match self {
            Self::Locked => "Unlock encryption to post",
            Self::RotationPending => "Rotate the room key to post",
            Self::StaleEpoch => "Getting the latest room key…",
        }
    }

    /// The banner shown above the stream, explaining the same block at length.
    ///
    /// Deliberately derived from the same value as [`Self::composer_hint`].
    /// These were once computed independently — the banner from the room's
    /// rotation flag, the composer from the key bundle — and the two disagreed:
    /// a user with no keys *and* a pending rotation was shown "Key rotation
    /// needed" while the composer said "Unlock encryption to post", with
    /// nothing on screen offering a way to unlock. One source, one answer.
    pub fn banner_text(self) -> &'static str {
        match self {
            Self::Locked => {
                "Encryption is locked on this device. Unlock with your recovery phrase or \
                 private key to read this room."
            }
            Self::RotationPending => "Key rotation needed before you can post.",
            Self::StaleEpoch => "Fetching this room's latest key…",
        }
    }

    /// Label of the button that resolves it, when the user can resolve it.
    ///
    /// `None` for [`Self::StaleEpoch`]: the client fetches the newer wrap on
    /// its own, so offering a button would invite a click that does nothing.
    pub fn banner_action(self) -> Option<&'static str> {
        match self {
            Self::Locked => Some("Unlock"),
            Self::RotationPending => Some("Rotate now"),
            Self::StaleEpoch => None,
        }
    }
}

/// Every mutation the UI can make.
pub enum Action {
    SetAuth(Auth),
    /// A verified session parked behind the boot cutscene (vault unlock on
    /// reload). `SetAuth` fires when the cutscene ends or is skipped.
    StageBoot(crate::session::Session),
    /// The cutscene finished: promote the parked session, if one is waiting.
    FinishBoot,
    /// Start a message's disintegration (app.css §7).
    Dissolve(MessageId),
    /// Put it back: the delete failed, so the message is still there and must
    /// stop looking like it is not.
    UndoDissolve(MessageId),
    /// Park what the Knowledge page should open with (hashtag chip, "Teach
    /// from message"); the page takes it on mount.
    SeedKnowledge(KnowledgeSeed),
    TakeKnowledgeSeed,
    SetChain(BlockchainInfo),
    /// `PUT /api/auth/profile` succeeded: fold the server's copy of the
    /// caller's profile into the live session (and its persisted shadow).
    ProfileUpdated(crate::api::User),

    RoomsLoading,
    RoomsLoaded(Vec<RoomWithMembers>),
    RoomsFailed(String),
    /// Optimistically zero a room's badge when it is opened.
    ClearUnread(RoomId),

    InvitationsLoading,
    InvitationsLoaded(Vec<Invitation>),
    InvitationsFailed(String),

    BlocksLoaded(Vec<BlockedUser>, Vec<BlockedUser>),

    RoomLoading(RoomId),
    RoomLoaded(RoomId),
    RoomFailed(RoomId, String),
    /// Fold a `/sync` page into a room.
    Sync(RoomId, Vec<Message>),
    /// Merge a `GET /messages` backfill page (older history).
    History(RoomId, Vec<Message>, bool),
    /// Restore a room's stream from the persisted cache (`cache.rs`) — the
    /// zero-network open. Applied only when the room holds nothing in memory,
    /// so a stale snapshot can never clobber a live stream.
    Hydrate(RoomId, crate::cache::CachedRoom),
    /// Drop a room's in-memory stream (Sync now). The caller is expected to
    /// have dropped the persisted copy too, or the next open re-hydrates it.
    ForgetRoom(RoomId),
    SetBundle(RoomId, Rc<RoomKeyBundle>),
    SetReadSerial(RoomId, i64),

    /// The `local_id` is allocated by the caller via [`next_local_id`] rather
    /// than by the reducer: the component needs it *immediately* to key the
    /// follow-up send, and reading it back out of the next state would race
    /// with any other dispatch in the same tick.
    QueueSend(RoomId, u64, String, i64),
    SendSucceeded(RoomId, u64),
    SendFailed(RoomId, u64, String),
    DiscardPending(RoomId, u64),
    RetryPending(RoomId, u64),

    SetConn(ConnStatus),
    SetOnline(bool),
    Typing(RoomId, WalletAddress, i64),
    SweepTyping(i64),
    /// Drop every typing indicator for a room we are navigating away from.
    ClearTyping(RoomId),
    SetMode(ConnectionMode),
    SetTheme(Theme),
    SetShellLayout(ShellLayout),
    SetFontFace(FontFace),
    SetFontScale(FontScale),
    SetLanguage(Lang),
    SetNetworks(Vec<Network>),

    Toast(ToastKind, String, Option<String>),
    DismissToast(u64),
    OpenModal(Modal),
    CloseModal,
}

impl Reducible for AppState {
    type Action = Action;

    fn reduce(self: Rc<Self>, action: Self::Action) -> Rc<Self> {
        let mut s = AppState {
            revision: self.revision + 1,
            auth: self.auth.clone(),
            client: self.client.clone(),
            chain: self.chain.clone(),
            networks: self.networks.clone(),
            rooms: self.rooms.clone(),
            rooms_load: self.rooms_load.clone(),
            invitations: self.invitations.clone(),
            invitations_load: self.invitations_load.clone(),
            blocked: self.blocked.clone(),
            blocks: self.blocks.clone(),
            room_states: self.room_states.clone(),
            room_load: self.room_load.clone(),
            bundles: self.bundles.clone(),
            pending: self.pending.clone(),
            conn: self.conn,
            online: self.online,
            typing: self.typing.clone(),
            mode: self.mode,
            theme: self.theme,
            shell_layout: self.shell_layout,
            language: self.language,
            font_face: self.font_face,
            font_scale: self.font_scale,
            toasts: self.toasts.clone(),
            modal: self.modal.clone(),
            pending_boot: self.pending_boot.clone(),
            dissolving: self.dissolving.clone(),
            knowledge_seed: self.knowledge_seed.clone(),
            next_id: self.next_id,
        };

        match action {
            Action::SetAuth(a) => {
                // Switching identity must not leak the previous account's
                // decrypted content into the new one's screens.
                if a.address() != s.auth.address() {
                    s.rooms.clear();
                    s.room_states.clear();
                    s.bundles.clear();
                    s.pending.clear();
                    s.invitations.clear();
                    s.blocks = BlockSet::default();
                }
                s.client = s.client.with_token(a.token());
                s.auth = a;
            }
            Action::StageBoot(session) => {
                s.pending_boot = Some(session);
            }
            Action::Dissolve(id) => {
                s.dissolving.insert(id);
            }
            Action::UndoDissolve(id) => {
                s.dissolving.remove(&id);
            }
            Action::SeedKnowledge(seed) => s.knowledge_seed = Some(seed),
            Action::TakeKnowledgeSeed => s.knowledge_seed = None,
            Action::FinishBoot => {
                if let Some(session) = s.pending_boot.take() {
                    s.client = s.client.with_token(Some(&session.token));
                    s.auth = Auth::Unlocked(session);
                }
            }
            Action::ProfileUpdated(user) => match &mut s.auth {
                // The common case: fold the server's copy into the live
                // session and its persisted shadow.
                Auth::Unlocked(session) if session.user.wallet_address == user.wallet_address => {
                    session.user = user;
                    session.persist();
                }
                // A locked session can still edit its profile (the JWT works;
                // only decryption is unavailable). Ignoring the update here
                // would leave every avatar on screen unchanged after a 200 —
                // a save that visibly "didn't take".
                Auth::Locked(p) if p.wallet_address == user.wallet_address => {
                    p.username = user.username.clone();
                    p.profile_image = user.profile_image.clone();
                    p.save();
                }
                _ => {}
            },
            Action::SetChain(c) => s.chain = c,
            Action::SetNetworks(list) => s.networks = Rc::new(list),

            Action::RoomsLoading => s.rooms_load = Load::Loading,
            Action::RoomsLoaded(rooms) => {
                // Carry each room's server-confirmed read pointer into the
                // per-room state so local unread math agrees with the badge.
                for r in &rooms {
                    if let Some(serial) = r.last_read_serial {
                        s.room_states
                            .entry(r.id().clone())
                            .or_default()
                            .last_read_serial = serial;
                    }
                }
                crate::cache::save_rooms(&rooms);
                s.rooms = rooms;
                s.rooms_load = Load::Ready;
            }
            Action::RoomsFailed(e) => s.rooms_load = Load::Error(e),
            Action::ClearUnread(id) => {
                if let Some(r) = s.rooms.iter_mut().find(|r| r.id() == &id) {
                    r.unread_count = Some(0);
                }
            }

            Action::InvitationsLoading => s.invitations_load = Load::Loading,
            Action::InvitationsLoaded(v) => {
                s.invitations = v;
                s.invitations_load = Load::Ready;
            }
            Action::InvitationsFailed(e) => s.invitations_load = Load::Error(e),

            Action::BlocksLoaded(blocked, blocked_by) => {
                let mine: Vec<WalletAddress> =
                    blocked.iter().map(|b| b.blocked_address.clone()).collect();
                let theirs: Vec<WalletAddress> = blocked_by
                    .iter()
                    .map(|b| b.blocker_address.clone())
                    .collect();
                s.blocks = BlockSet::from_pairs(&mine, &theirs);
                // Deduplicate: the reference server has no unique index and
                // happily stores the same block twice.
                let mut seen = std::collections::HashSet::new();
                s.blocked = blocked
                    .into_iter()
                    .filter(|b| seen.insert(b.blocked_address.clone()))
                    .collect();
            }

            Action::RoomLoading(id) => {
                s.room_load.insert(id, Load::Loading);
            }
            Action::RoomLoaded(id) => {
                s.room_states.entry(id.clone()).or_default().loaded = true;
                s.room_load.insert(id, Load::Ready);
            }
            Action::RoomFailed(id, e) => {
                s.room_load.insert(id, Load::Error(e));
            }
            Action::Sync(id, events) => {
                let st = s.room_states.entry(id.clone()).or_default();
                let out = st.fold(&events);
                st.loaded = true;
                if out.max_serial > 0 {
                    crate::session::save_cursor(id.as_str(), st.cursor);
                }
                // Write-through, like the cursor above: every path that
                // changes a stream — realtime events, confirmed sends,
                // reactions, purges — funnels through this fold, so
                // persisting here is what keeps the cache always current.
                crate::cache::save_room(&id, &st.to_cached());
                // The effect outlives its subject by design; drop the marker
                // once the message it named has actually left the stream.
                let live: Vec<MessageId> = st.messages.keys().cloned().collect();
                s.dissolving.retain(|m| live.contains(m));
                // A purge invalidates any in-flight optimistic bubbles too.
                if out.purged {
                    s.pending.remove(&id);
                }
                s.room_load.insert(id, Load::Ready);
            }
            Action::History(id, page, has_more) => {
                let st = s.room_states.entry(id.clone()).or_default();
                st.merge_history(&page);
                st.has_more_history = has_more;
                st.loaded = true;
                crate::cache::save_room(&id, &st.to_cached());
                s.room_load.insert(id, Load::Ready);
            }
            Action::Hydrate(id, cached) => {
                // Only into an empty stream: memory is always fresher.
                let entry = s.room_states.entry(id.clone()).or_default();
                if entry.messages.is_empty() {
                    let read = entry.last_read_serial;
                    let cursor = crate::session::load_cursor(id.as_str());
                    let mut st = crate::store::RoomState::from_cached(cached, cursor);
                    st.last_read_serial = read;
                    s.room_states.insert(id.clone(), st);
                    s.room_load.insert(id, Load::Ready);
                }
            }
            Action::ForgetRoom(id) => {
                s.room_states.remove(&id);
            }
            Action::SetBundle(id, b) => {
                s.bundles.insert(id, b);
            }
            Action::SetReadSerial(id, serial) => {
                let st = s.room_states.entry(id.clone()).or_default();
                // Monotonic, mirroring the server: a lower value is ignored.
                st.last_read_serial = st.last_read_serial.max(serial);
            }

            Action::QueueSend(id, local_id, text, now) => {
                s.pending.entry(id).or_default().push(Pending {
                    local_id,
                    plaintext: text,
                    failed: None,
                    created_ms: now,
                });
            }
            Action::SendSucceeded(id, local_id) | Action::DiscardPending(id, local_id) => {
                if let Some(q) = s.pending.get_mut(&id) {
                    q.retain(|p| p.local_id != local_id);
                    if q.is_empty() {
                        s.pending.remove(&id);
                    }
                }
            }
            Action::SendFailed(id, local_id, why) => {
                if let Some(p) = s
                    .pending
                    .get_mut(&id)
                    .and_then(|q| q.iter_mut().find(|p| p.local_id == local_id))
                {
                    p.failed = Some(why);
                }
            }
            Action::RetryPending(id, local_id) => {
                if let Some(p) = s
                    .pending
                    .get_mut(&id)
                    .and_then(|q| q.iter_mut().find(|p| p.local_id == local_id))
                {
                    p.failed = None;
                }
            }

            Action::SetConn(c) => s.conn = c,
            Action::SetOnline(o) => {
                s.online = o;
                if !o {
                    s.conn = ConnStatus::Offline;
                }
            }
            Action::Typing(room, who, now) => s.typing.note(room, who, now),
            Action::SweepTyping(now) => {
                s.typing.sweep(now);
            }
            Action::ClearTyping(room) => s.typing.clear_room(&room),
            Action::SetMode(m) => {
                m.save();
                s.mode = m;
                s.conn = match m {
                    ConnectionMode::Polling => ConnStatus::Live(Transport::Polling),
                    _ => ConnStatus::Syncing,
                };
            }
            Action::SetTheme(t) => {
                t.apply();
                s.theme = t;
            }
            Action::SetFontFace(f) => {
                f.apply();
                s.font_face = f;
            }
            Action::SetFontScale(f) => {
                f.apply();
                s.font_scale = f;
            }
            Action::SetShellLayout(l) => {
                l.save();
                s.shell_layout = l;
            }
            Action::SetLanguage(l) => {
                l.save();
                s.language = l;
            }

            Action::Toast(kind, title, description) => {
                let id = s.next_id;
                s.next_id += 1;
                s.toasts.push(Toast {
                    id,
                    kind,
                    title,
                    description,
                });
                // Max three stacked; a fourth evicts the oldest.
                while s.toasts.len() > 3 {
                    s.toasts.remove(0);
                }
            }
            Action::DismissToast(id) => s.toasts.retain(|t| t.id != id),
            Action::OpenModal(m) => s.modal = Some(m),
            Action::CloseModal => s.modal = None,
        }

        Rc::new(s)
    }
}

/// Allocate a process-unique id for an optimistic send.
///
/// Monotonic within the tab, which is all that is needed: these never leave the
/// client and are discarded the moment the server assigns a real message id.
pub fn next_local_id() -> u64 {
    use std::cell::Cell;
    thread_local! {
        static COUNTER: Cell<u64> = const { Cell::new(1) };
    }
    COUNTER.with(|c| {
        let v = c.get();
        c.set(v + 1);
        v
    })
}

/// The context handle every screen reads.
pub type Store = UseReducerHandle<AppState>;

/// Read the store from context. Panics only if a component is mounted outside
/// the provider, which is a compile-time-shaped mistake caught on first render.
#[hook]
pub fn use_store() -> Store {
    use_context::<Store>().expect("<App/> must provide the store context")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Client;
    use crate::session::PersistedSession;

    fn addr(n: u8) -> WalletAddress {
        WalletAddress::new(&format!("0x{:040x}", n as u32)).unwrap()
    }

    fn state() -> Rc<AppState> {
        Rc::new(AppState::new(Auth::SignedOut, Client::new("")))
    }

    #[test]
    fn a_profile_update_reaches_a_locked_session_too() {
        // A locked session (reload, vault not yet unlocked) can still save a
        // profile image — the JWT works. The reducer must fold the answer in,
        // or the 200 changes nothing on screen and reads as a failed save.
        let p = PersistedSession {
            token: "t1".into(),
            wallet_address: addr(1),
            username: "old".into(),
            profile_image: None,
        };
        let s = state().reduce(Action::SetAuth(Auth::Locked(p)));

        let updated = crate::api::User {
            wallet_address: addr(1),
            username: "old".into(),
            public_key: None,
            public_key_sig: None,
            profile_image: Some("preset:tp-pilot-m".into()),
            created_at: None,
            updated_at: None,
        };
        let s = s.reduce(Action::ProfileUpdated(updated.clone()));
        assert_eq!(s.auth.profile_image(), Some("preset:tp-pilot-m"));

        // Somebody else's profile must never overwrite the session's.
        let mut foreign = updated;
        foreign.wallet_address = addr(2);
        foreign.profile_image = Some("preset:tp-coder-f".into());
        let s = s.reduce(Action::ProfileUpdated(foreign));
        assert_eq!(s.auth.profile_image(), Some("preset:tp-pilot-m"));
    }

    #[test]
    fn every_action_bumps_the_revision_so_yew_re_renders() {
        let s = state();
        let before = s.revision;
        let after = s.reduce(Action::SetOnline(false));
        assert_eq!(after.revision, before + 1);
        assert!(*after != *state(), "a bumped revision must compare unequal");
    }

    #[test]
    fn switching_identity_drops_the_previous_accounts_decrypted_state() {
        // The failure mode this guards against is the worst kind: account A's
        // messages briefly visible under account B's session.
        let s = state();
        let room = RoomId::new("room_00000001").unwrap();
        let s = s.reduce(Action::RoomLoaded(room.clone()));
        assert!(s.room_states.contains_key(&room));

        let s = s.reduce(Action::SetAuth(Auth::Locked(PersistedSession {
            token: "t".into(),
            wallet_address: addr(1),
            username: "a".into(),
            profile_image: None,
        })));
        assert!(s.room_states.is_empty());
        assert!(s.bundles.is_empty());
        assert!(s.rooms.is_empty());
        assert_eq!(s.client.token(), Some("t"));
    }

    /// The store snapshot an effect captures carries the API client, so any
    /// long-lived closure created before sign-in holds a **tokenless** client
    /// unless its effect is re-keyed on the token.
    ///
    /// That is not hypothetical: the safety-sync interval and the `online`
    /// listener were both keyed without it, so their first tick after sign-in
    /// sent `GET /api/rooms` with no `Authorization` header, took the 401, and
    /// ran the sign-out path — logging the user out about a minute after they
    /// logged in. This pins the half of that invariant a host test can see: the
    /// token a caller would key on is exactly the one the client will send.
    #[test]
    fn the_client_token_tracks_auth_so_effects_can_key_on_it() {
        let s = state();
        assert_eq!(s.auth.token(), None);
        assert_eq!(s.client.token(), None, "signed out ⇒ no credential");

        let session = PersistedSession {
            token: "jwt-abc".into(),
            wallet_address: addr(1),
            username: "a".into(),
            profile_image: None,
        };
        let s = s.reduce(Action::SetAuth(Auth::Locked(session)));

        assert_eq!(s.auth.token(), Some("jwt-abc"));
        assert_eq!(
            s.client.token(),
            s.auth.token(),
            "an effect keyed on auth.token() must get a client carrying it"
        );

        // And signing out must clear it, or a stale closure could keep using a
        // credential the user believes they revoked.
        let s = s.reduce(Action::SetAuth(Auth::SignedOut));
        assert_eq!(s.auth.token(), None);
        assert_eq!(s.client.token(), None);
    }

    #[test]
    fn re_authenticating_as_the_same_wallet_keeps_the_caches() {
        let p = PersistedSession {
            token: "t1".into(),
            wallet_address: addr(1),
            username: "a".into(),
            profile_image: None,
        };
        let room = RoomId::new("room_00000001").unwrap();
        let s = state()
            .reduce(Action::SetAuth(Auth::Locked(p.clone())))
            .reduce(Action::RoomLoaded(room.clone()));
        let s = s.reduce(Action::SetAuth(Auth::Locked(PersistedSession {
            token: "t2".into(),
            ..p
        })));
        assert!(
            s.room_states.contains_key(&room),
            "same wallet, same caches"
        );
        assert_eq!(s.client.token(), Some("t2"));
    }

    #[test]
    fn toasts_cap_at_three_and_evict_the_oldest() {
        let mut s = state();
        for i in 0..5 {
            s = s.reduce(Action::Toast(ToastKind::Info, format!("t{i}"), None));
        }
        assert_eq!(s.toasts.len(), 3);
        assert_eq!(s.toasts[0].title, "t2");
        assert_eq!(s.toasts[2].title, "t4");

        let id = s.toasts[1].id;
        let s = s.reduce(Action::DismissToast(id));
        assert_eq!(s.toasts.len(), 2);
    }

    #[test]
    fn errors_never_auto_dismiss_but_everything_else_does() {
        assert_eq!(ToastKind::Error.ttl_ms(false), None);
        assert_eq!(ToastKind::Error.ttl_ms(true), None);
        assert_eq!(ToastKind::Success.ttl_ms(false), Some(4_000));
        assert_eq!(ToastKind::Success.ttl_ms(true), Some(6_000));
        assert_eq!(ToastKind::Error.live_region(), ("alert", "assertive"));
        assert_eq!(ToastKind::Info.live_region(), ("status", "polite"));
    }

    #[test]
    fn the_read_pointer_only_ever_moves_forward() {
        let room = RoomId::new("room_00000001").unwrap();
        let s = state()
            .reduce(Action::SetReadSerial(room.clone(), 500))
            .reduce(Action::SetReadSerial(room.clone(), 100));
        assert_eq!(s.room_states[&room].last_read_serial, 500);
    }

    #[test]
    fn a_purge_clears_the_optimistic_send_queue_too() {
        let room = RoomId::new("room_00000001").unwrap();
        let s = state().reduce(Action::QueueSend(room.clone(), 7, "hi".into(), 0));
        assert_eq!(s.pending[&room].len(), 1);

        let purge = crate::api::Message {
            id: MessageId::new("marker___12").unwrap(),
            room_id: room.clone(),
            sender_address: addr(1),
            content: String::new(),
            msg_hash: String::new(),
            message_timestamp: 1,
            msg_type: "delete_all".into(),
            msg_serial: 10,
            is_deleted: false,
            edited_at: None,
            created_at: None,
            is_encrypted: false,
            iv: None,
            hmac: None,
            enc_ver: None,
            key_version: None,
            tx_hash: None,
            target_message_id: None,
            emoticon_code: None,
            sender: None,
        };
        let s = s.reduce(Action::Sync(room.clone(), vec![purge]));
        assert!(!s.pending.contains_key(&room));
    }

    #[test]
    fn a_failed_send_is_marked_not_dropped_so_it_can_be_retried() {
        let room = RoomId::new("room_00000001").unwrap();
        let s = state().reduce(Action::QueueSend(room.clone(), 7, "hi".into(), 0));
        let local = s.pending[&room][0].local_id;

        let s = s.reduce(Action::SendFailed(room.clone(), local, "boom".into()));
        assert_eq!(s.pending[&room][0].failed.as_deref(), Some("boom"));

        let s = s.reduce(Action::RetryPending(room.clone(), local));
        assert!(s.pending[&room][0].failed.is_none());

        let s = s.reduce(Action::SendSucceeded(room.clone(), local));
        assert!(!s.pending.contains_key(&room), "the queue entry is gone");
    }

    #[test]
    fn going_offline_forces_the_connection_pill_offline() {
        let s = state()
            .reduce(Action::SetConn(ConnStatus::Live(Transport::WebSocket)))
            .reduce(Action::SetOnline(false));
        assert_eq!(s.conn, ConnStatus::Offline);
    }

    #[test]
    fn blocked_lists_are_deduplicated_and_the_filter_is_bidirectional() {
        let b = |blocker: u8, blocked: u8| BlockedUser {
            id: 0,
            blocker_address: addr(blocker),
            blocked_address: addr(blocked),
            created_at: None,
        };
        let s = state().reduce(Action::BlocksLoaded(
            // The reference server can store the same block twice.
            vec![b(1, 2), b(1, 2)],
            vec![b(3, 1)],
        ));
        assert_eq!(s.blocked.len(), 1, "duplicates collapse in the UI list");
        assert!(s.blocks.hides(&addr(2)));
        assert!(s.blocks.hides(&addr(3)));
        assert!(!s.blocks.hides(&addr(1)));
    }

    #[test]
    fn rooms_sort_by_activity_and_unread_totals_add_up() {
        let mk = |id: &str, ts: i64, unread: u32| {
            let mut r: RoomWithMembers = serde_json::from_str(&format!(
                r#"{{"id":"{id}","name":"n","memberCount":1,"unreadCount":{unread}}}"#
            ))
            .unwrap();
            r.last_message = Some(crate::api::Message {
                id: MessageId::new("msg_aaaa_01").unwrap(),
                room_id: RoomId::new(id).unwrap(),
                sender_address: addr(1),
                content: "x".into(),
                msg_hash: String::new(),
                message_timestamp: ts,
                msg_type: "add".into(),
                msg_serial: ts,
                is_deleted: false,
                edited_at: None,
                created_at: None,
                is_encrypted: false,
                iv: None,
                hmac: None,
                enc_ver: None,
                key_version: None,
                tx_hash: None,
                target_message_id: None,
                emoticon_code: None,
                sender: None,
            });
            r
        };
        let s = state().reduce(Action::RoomsLoaded(vec![
            mk("room_00000001", 100, 2),
            mk("room_00000002", 900, 5),
            mk("room_00000003", 500, 0),
        ]));
        let order: Vec<&str> = s.sorted_rooms().iter().map(|r| r.id().as_str()).collect();
        assert_eq!(
            order,
            vec!["room_00000002", "room_00000003", "room_00000001"]
        );
        assert_eq!(s.total_unread(), 7);

        let s = s.reduce(Action::ClearUnread(RoomId::new("room_00000002").unwrap()));
        assert_eq!(s.total_unread(), 2);
    }

    #[test]
    fn an_unencrypted_room_can_always_be_posted_to() {
        let r: RoomWithMembers =
            serde_json::from_str(r#"{"id":"room_00000001","name":"n","hasEncryption":false}"#)
                .unwrap();
        let s = state().reduce(Action::RoomsLoaded(vec![r]));
        assert!(s.can_post_encrypted(&RoomId::new("room_00000001").unwrap()));
    }

    /// Build an encrypted room on `current_key_version`, optionally rotating.
    fn encrypted_room(current: u32, rotating: bool) -> RoomWithMembers {
        serde_json::from_str(&format!(
            r#"{{"id":"room_00000001","name":"n","hasEncryption":true,
                 "currentKeyVersion":{current},"keyRotationPending":{rotating}}}"#
        ))
        .unwrap()
    }

    fn bundle_with(epochs: &[i64]) -> Rc<RoomKeyBundle> {
        let mut b = RoomKeyBundle::default();
        for (i, e) in epochs.iter().enumerate() {
            b.insert(*e, [i as u8; 32]);
        }
        Rc::new(b)
    }

    #[test]
    fn an_encrypted_room_is_blocked_while_rotation_is_pending_or_the_epoch_lags() {
        let id = RoomId::new("room_00000001").unwrap();

        // Rotation pending, but we *do* hold the current key — otherwise the
        // missing key is the more immediate reason and would mask this one.
        let s = state()
            .reduce(Action::RoomsLoaded(vec![encrypted_room(2, true)]))
            .reduce(Action::SetBundle(id.clone(), bundle_with(&[1, 2])));
        assert!(!s.can_post_encrypted(&id), "rotation pending");

        // Rotation settled but we hold only epoch 1 while the room is on 2.
        let s = state()
            .reduce(Action::RoomsLoaded(vec![encrypted_room(2, false)]))
            .reduce(Action::SetBundle(id.clone(), bundle_with(&[1])));
        assert!(!s.can_post_encrypted(&id), "stale epoch");

        let s = s.reduce(Action::SetBundle(id.clone(), bundle_with(&[1, 2])));
        assert!(s.can_post_encrypted(&id));
    }

    /// The reason has to survive, because the three remedies are unrelated:
    /// unlock this device, wait for an admin, or wait for a fetch. Collapsing
    /// them told a locked user to "rotate the room key", which is an admin
    /// action that would not have helped them.
    #[test]
    fn each_reason_a_post_is_blocked_names_its_own_remedy() {
        let id = RoomId::new("room_00000001").unwrap();

        // No keys on this device at all — a reload, since keys are never
        // persisted. This outranks rotation: nothing else is actionable yet.
        let s = state().reduce(Action::RoomsLoaded(vec![encrypted_room(2, true)]));
        assert_eq!(s.post_block(&id), Some(PostBlock::Locked));

        // An empty bundle is the same situation as no bundle.
        let s = s.reduce(Action::SetBundle(id.clone(), bundle_with(&[])));
        assert_eq!(s.post_block(&id), Some(PostBlock::Locked));

        // Keys present and rotation pending.
        let s = s.reduce(Action::SetBundle(id.clone(), bundle_with(&[1, 2])));
        assert_eq!(s.post_block(&id), Some(PostBlock::RotationPending));

        // Keys present, not rotating, but behind the room's epoch.
        let s = state()
            .reduce(Action::RoomsLoaded(vec![encrypted_room(2, false)]))
            .reduce(Action::SetBundle(id.clone(), bundle_with(&[1])));
        assert_eq!(s.post_block(&id), Some(PostBlock::StaleEpoch));

        // Fully caught up.
        let s = s.reduce(Action::SetBundle(id.clone(), bundle_with(&[1, 2])));
        assert_eq!(s.post_block(&id), None);

        // Every reason must offer distinct, non-empty guidance.
        let hints = [
            PostBlock::Locked.composer_hint(),
            PostBlock::RotationPending.composer_hint(),
            PostBlock::StaleEpoch.composer_hint(),
        ];
        assert!(hints.iter().all(|h| !h.is_empty()));
        assert_eq!(
            hints.iter().collect::<std::collections::HashSet<_>>().len(),
            3,
            "two reasons sharing a message is the bug this guards against"
        );
        assert!(
            !PostBlock::Locked
                .composer_hint()
                .to_lowercase()
                .contains("rotate"),
            "a locked device must not be told to rotate"
        );
    }

    /// The regression guard for the contradiction that shipped: the banner was
    /// derived from the room's rotation flag while the composer was derived
    /// from the key bundle, so a user who was both locked *and* rotation-pending
    /// read "Key rotation needed" above a composer saying "Unlock encryption to
    /// post" — two different remedies, and no way to reach the one that would
    /// have worked.
    ///
    /// Both now render from one `PostBlock`. This pins that every variant
    /// answers all three questions, and that the answers agree.
    #[test]
    fn the_banner_and_the_composer_never_name_different_remedies() {
        let all = [
            PostBlock::Locked,
            PostBlock::RotationPending,
            PostBlock::StaleEpoch,
        ];

        for b in all {
            assert!(!b.composer_hint().is_empty(), "{b:?} has no composer hint");
            assert!(!b.banner_text().is_empty(), "{b:?} has no banner text");
        }

        // Each reason must be distinguishable in both surfaces, or two states
        // collapse into one message again.
        let hints: std::collections::HashSet<_> = all.iter().map(|b| b.composer_hint()).collect();
        let texts: std::collections::HashSet<_> = all.iter().map(|b| b.banner_text()).collect();
        assert_eq!(hints.len(), all.len(), "two reasons share a composer hint");
        assert_eq!(texts.len(), all.len(), "two reasons share a banner");

        // The specific mix-up: a locked device must never be pointed at
        // rotation, and vice versa.
        let locked = PostBlock::Locked;
        assert!(locked.composer_hint().to_lowercase().contains("unlock"));
        assert!(locked.banner_text().to_lowercase().contains("unlock"));
        assert_eq!(locked.banner_action(), Some("Unlock"));
        assert!(!locked.banner_text().to_lowercase().contains("rotat"));

        let rotating = PostBlock::RotationPending;
        assert!(rotating.composer_hint().to_lowercase().contains("rotate"));
        assert!(rotating.banner_text().to_lowercase().contains("rotation"));
        assert_eq!(rotating.banner_action(), Some("Rotate now"));

        // Nothing the user can do about a stale epoch, so no button to press.
        assert_eq!(PostBlock::StaleEpoch.banner_action(), None);
    }

    /// The precedence itself, stated once. `post_block` returning `Locked`
    /// while a rotation is also pending is the case that produced the bug.
    #[test]
    fn a_locked_device_reports_locked_even_when_a_rotation_is_also_pending() {
        let id = RoomId::new("room_00000001").unwrap();
        let s = state().reduce(Action::RoomsLoaded(vec![encrypted_room(2, true)]));

        assert_eq!(s.post_block(&id), Some(PostBlock::Locked));
        // And once keys arrive, the *other* reason surfaces — it was never
        // wrong, just not the one the user could act on first.
        let s = s.reduce(Action::SetBundle(id.clone(), bundle_with(&[1, 2])));
        assert_eq!(s.post_block(&id), Some(PostBlock::RotationPending));
    }

    #[test]
    fn an_unencrypted_room_is_never_blocked_whatever_its_key_fields_say() {
        let id = RoomId::new("room_00000001").unwrap();
        let r: RoomWithMembers = serde_json::from_str(
            r#"{"id":"room_00000001","name":"n","hasEncryption":false,
                "currentKeyVersion":9,"keyRotationPending":true}"#,
        )
        .unwrap();
        let s = state().reduce(Action::RoomsLoaded(vec![r]));
        assert_eq!(s.post_block(&id), None);
    }
}
