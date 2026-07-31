//! Async operations: everything that talks to the server and then updates the
//! store.
//!
//! Components stay declarative; the sequencing that the specs care about lives
//! here — the `/sync` drain loop, the key-epoch dance around sending, and the
//! rotation procedure with its mandatory binding checks.

use std::cell::RefCell;
use std::rc::Rc;

use pocketskynet_core::{RoomId, Wallet, WalletAddress};

use crate::api::{Client, RoomKeyWrap};
use crate::components::toast;
use crate::crypto::{self, SessionKeys};
use crate::i18n::{t, Key, Lang};
use crate::realtime::ConnStatus;
use crate::session::{Auth, Session};
use crate::state::{Action, Store};

/// Load the room list, the invitations and both block lists.
///
/// Blocks are fetched **both ways**: the server filters search bidirectionally
/// but filters messages only viewer-side, so a client that ignored
/// `blocked-by` would still render messages from someone who blocked it.
pub async fn refresh_all(store: Store) {
    let client = store.client.clone();

    // Paint the cached list first: the sidebar appears in the same frame as
    // sign-in and the network answer replaces it when it lands. The spinner is
    // shown only when there is nothing to paint — a spinner over content is
    // how a cache makes an app *feel* slower.
    let hydrated = if store.rooms.is_empty() {
        match crate::cache::load_rooms() {
            Some(rooms) if !rooms.is_empty() => {
                store.dispatch(Action::RoomsLoaded(rooms));
                true
            }
            _ => false,
        }
    } else {
        true
    };
    if !hydrated {
        store.dispatch(Action::RoomsLoading);
    }

    match client.rooms().await {
        Ok(rooms) => store.dispatch(Action::RoomsLoaded(rooms)),
        Err(e) => {
            if e.is_unauthorized() {
                sign_out(&store);
                return;
            }
            store.dispatch(Action::RoomsFailed(e.user_message()));
        }
    }

    refresh_invitations(store.clone()).await;
    refresh_shouts(store.clone()).await;
    refresh_blocks(store).await;
}

/// Fetch the active paid broadcasts and hand them to the banner layer.
///
/// Called on sign-in, on every `shout` wake-up event, and from the safety
/// sync so polling-mode clients see shouts too. Failure is silent: a shout
/// is sixty seconds of theatre, not data anyone loses.
pub async fn refresh_shouts(store: Store) {
    if let Ok(shouts) = store.client.active_shouts().await {
        crate::components::shout::sync(shouts);
    }
}

pub async fn refresh_invitations(store: Store) {
    store.dispatch(Action::InvitationsLoading);
    match store.client.invitations().await {
        Ok(v) => store.dispatch(Action::InvitationsLoaded(v)),
        Err(e) => store.dispatch(Action::InvitationsFailed(e.user_message())),
    }
}

pub async fn refresh_blocks(store: Store) {
    let (mine, theirs) = futures::join!(store.client.blocked(), store.client.blocked_by());
    // Degrade open on failure rather than hiding everything: a transient error
    // must not silently blank a conversation.
    store.dispatch(Action::BlocksLoaded(
        mine.unwrap_or_default(),
        theirs.unwrap_or_default(),
    ));
}

pub async fn refresh_rooms(store: Store) {
    match store.client.rooms().await {
        Ok(rooms) => store.dispatch(Action::RoomsLoaded(rooms)),
        Err(e) if e.is_unauthorized() => sign_out(&store),
        Err(e) => store.dispatch(Action::RoomsFailed(e.user_message())),
    }
}

/// Open a room. The contract: **a room you have seen costs zero requests to
/// paint.** The network's only job on open is the `/sync` delta, and it runs
/// after the stream is already on screen.
///
/// Three tiers, cheapest first:
///
/// 1. **In memory** (switching back within a session) — nothing to do.
/// 2. **In the cache** (a reload, or offline) — keys unwrap from the cached
///    wraps and the stream hydrates from the cached rows, both synchronous.
/// 3. **Cold** (first visit) — the old path, except keys and history now fly
///    in one round trip instead of two: `refresh_keys` dispatches its bundle
///    before `join!` returns, so keys still *apply* first and history never
///    flashes sealed. `/sync` genuinely depends on the cursor and stays last.
///
/// After the sync drain, one coverage check: if any held row names an epoch
/// the bundle lacks (a rotation happened while this device was away), fetch
/// keys once. Network key fetches happen when needed, not per open.
pub async fn open_room(store: Store, room_id: RoomId) {
    let cached_cursor = crate::session::load_cursor(room_id.as_str());
    let in_memory = store
        .room_state(&room_id)
        .is_some_and(|s| !s.messages.is_empty());

    if !in_memory {
        // Keys strictly before rows, so a hydrated stream decrypts on its
        // first render instead of flickering out of sealed bubbles.
        hydrate_keys_from_cache(&store, &room_id);
        match crate::cache::load_room(&room_id) {
            Some(cached) => store.dispatch(Action::Hydrate(room_id.clone(), cached)),
            None => {
                store.dispatch(Action::RoomLoading(room_id.clone()));
                let (_, history) = futures::join!(
                    refresh_keys(store.clone(), room_id.clone()),
                    store.client.messages(&room_id, None, 50),
                );
                match history {
                    Ok(page) => {
                        let more = page.len() >= 50;
                        store.dispatch(Action::History(room_id.clone(), page, more));
                    }
                    Err(e) => {
                        store.dispatch(Action::RoomFailed(room_id.clone(), e.user_message()));
                        if !e.is_forbidden() {
                            return;
                        }
                    }
                }
            }
        }
    } else {
        hydrate_keys_from_cache(&store, &room_id);
    }

    drain_sync(store.clone(), room_id.clone(), cached_cursor).await;

    if needs_keys(&store, &room_id) {
        refresh_keys(store, room_id).await;
    }
}

/// The user pressed "Sync now": the one gesture that means *forget what you
/// have and ask the server*. Drops the room's cache and memory, then runs the
/// cold-open path.
pub async fn resync_room(store: Store, room_id: RoomId) {
    crate::cache::forget_room(&room_id);
    store.dispatch(Action::ForgetRoom(room_id.clone()));
    store.dispatch(Action::RoomLoading(room_id.clone()));
    let (_, history) = futures::join!(
        refresh_keys(store.clone(), room_id.clone()),
        store.client.messages(&room_id, None, 50),
    );
    match history {
        Ok(page) => {
            let more = page.len() >= 50;
            store.dispatch(Action::History(room_id.clone(), page, more));
        }
        Err(e) => {
            store.dispatch(Action::RoomFailed(room_id.clone(), e.user_message()));
            if !e.is_forbidden() {
                return;
            }
        }
    }
    let from = store.room_state(&room_id).map(|s| s.cursor).unwrap_or(0);
    drain_sync(store, room_id, from).await;
}

/// Unwrap this room's keys from the cached wraps — no network. A no-op when
/// the bundle is already in memory, the session is locked, or nothing is
/// cached; the caller falls back to `refresh_keys` for those.
fn hydrate_keys_from_cache(store: &Store, room_id: &RoomId) {
    if store.bundle(room_id).is_some() {
        return;
    }
    let Some(session) = store.auth.session().cloned() else {
        return;
    };
    let Some(wraps) = crate::cache::load_wraps(room_id) else {
        return;
    };
    if wraps.is_empty() {
        return;
    }
    let (bundle, _healed) = {
        let mut keys = session.keys.borrow_mut();
        crypto::unwrap_bundle(&mut keys, room_id, &wraps)
    };
    if !bundle.is_empty() {
        store.dispatch(Action::SetBundle(room_id.clone(), Rc::new(bundle)));
    }
}

/// Does any held row name an epoch the bundle cannot open? True also when an
/// encrypted room has no bundle at all — both mean a key fetch would help.
fn needs_keys(store: &Store, room_id: &RoomId) -> bool {
    let Some(st) = store.room_state(room_id) else {
        return false;
    };
    let bundle = store.bundle(room_id);
    st.messages.values().any(|m| {
        m.is_encrypted
            && m.has_crypto_metadata()
            && bundle.is_none_or(|b| b.get(m.key_version()).is_none())
    })
}

/// Load one older page (DESIGN.md §7.3 "Load earlier messages").
pub async fn load_older(store: Store, room_id: RoomId) {
    let before = store
        .room_state(&room_id)
        .and_then(|s| s.oldest_timestamp());
    match store.client.messages(&room_id, before, 50).await {
        Ok(page) => {
            // Paginate on the oldest timestamp, never on the row count: the
            // server applies its LIMIT before filtering reaction rows, so a
            // short page does not mean the history ended.
            let more = !page.is_empty();
            store.dispatch(Action::History(room_id, page, more));
        }
        Err(e) => store.dispatch(Action::RoomFailed(room_id, e.user_message())),
    }
}

/// The `/sync` drain loop (API.md §8.2).
///
/// Runs until the server says there is no more, or a page comes back empty —
/// the second condition matters because a page consisting entirely of blocked
/// senders' rows arrives empty with the cursor unchanged, and looping on that
/// would spin against a 100 requests/minute limiter.
pub async fn drain_sync(store: Store, room_id: RoomId, from: i64) {
    let mut since = store
        .room_state(&room_id)
        .map(|s| s.cursor.max(from))
        .unwrap_or(from);

    // A hard bound as well as the two logical ones: a server that always
    // answers `hasMore: true` must not hang the tab.
    for _ in 0..64 {
        match store.client.sync(&room_id, since).await {
            Ok(page) => {
                let batch_max = page.events.iter().map(|e| e.msg_serial).max().unwrap_or(0);
                let len = page.events.len();
                store.dispatch(Action::Sync(room_id.clone(), page.events));

                match crate::store::next_sync_cursor(since, batch_max, page.has_more, len) {
                    Some(next) => since = next,
                    None => break,
                }
            }
            Err(e) => {
                if e.is_unauthorized() {
                    sign_out(&store);
                }
                store.dispatch(Action::RoomFailed(room_id.clone(), e.user_message()));
                return;
            }
        }
    }
    store.dispatch(Action::RoomLoaded(room_id.clone()));

    // Advance the read pointer to whatever we just rendered. Safe to do
    // unconditionally: the server's pointer is monotonic and `unreadCount`
    // only counts `add` rows, so moving past an edit costs nothing.
    if let Some(cursor) = store.room_state(&room_id).map(|s| s.cursor) {
        if cursor > 0 {
            let client = store.client.clone();
            let store2 = store.clone();
            let room = room_id.clone();
            if let Ok(confirmed) = client.mark_read(&room, cursor).await {
                store2.dispatch(Action::SetReadSerial(room, confirmed));
            }
        }
    }
    store.dispatch(Action::ClearUnread(room_id));
}

/// Fetch and unwrap every epoch key this member holds for a room.
pub async fn refresh_keys(store: Store, room_id: RoomId) {
    let Some(session) = store.auth.session().cloned() else {
        return;
    };
    let wraps = match store.client.room_key_versions(&room_id).await {
        Ok(w) => w,
        Err(_) => return,
    };
    if wraps.is_empty() {
        return;
    }
    // Persist the wraps as received: they are ciphertext wrapped to this
    // account's public key — the same rows the server keeps — and they are
    // what lets the next open of this room unwrap without a request.
    crate::cache::save_wraps(&room_id, &wraps);

    let (bundle, healed) = {
        let mut keys = session.keys.borrow_mut();
        crypto::unwrap_bundle(&mut keys, &room_id, &wraps)
    };

    // Heal: an epoch that only opened with the legacy key is re-wrapped to the
    // current (salted) public key so the legacy derivation is never needed
    // again. Best effort — a failure here costs nothing today.
    for version in healed {
        if let Some(key) = bundle.get(version) {
            let wrap = {
                let keys = session.keys.borrow();
                crypto::wrap_room_key_for_self(key, &room_id, &keys, version)
            };
            if let Ok(w) = wrap {
                let _ = store.client.put_room_key(&room_id, &w).await;
            }
        }
    }

    store.dispatch(Action::SetBundle(room_id, Rc::new(bundle)));
}

/// The whole create-room flow — create, establish the key, refresh the list —
/// shared by the create dialog and the room list's one-click button, so a fast
/// room and a hand-made one are the same code path with different inputs.
///
/// Three outcomes, and the middle one is the important contract:
/// - `Ok((id, None))` — created, and encrypted if that was asked for.
/// - `Ok((id, Some(why)))` — created but **plaintext**, with the reason.
///   The caller MUST put `why` in front of the user: silently downgrading an
///   E2EE room is the worst possible outcome (DESIGN.md §8).
/// - `Err(e)` — nothing was created.
pub async fn create_room_flow(
    store: &Store,
    name: &str,
    description: &str,
    encrypt: bool,
) -> Result<(RoomId, Option<String>), String> {
    let room = store
        .client
        .create_room(name, Some(description))
        .await
        .map_err(|e| e.user_message())?;

    let downgraded = if encrypt {
        match store.auth.session() {
            Some(session) => establish_encryption(&store.client, session, &room.id)
                .await
                .err(),
            None => Some("your encryption keys aren't unlocked on this device".into()),
        }
    } else {
        None
    };

    refresh_rooms(store.clone()).await;
    Ok((room.id, downgraded))
}

/// A name and a description for a room nobody wanted to name.
///
/// Random, not derived from anything: two people creating a room in the same
/// second are creating *different* rooms and must not be handed the same title.
/// The entropy is drawn once and both halves come out of it.
///
/// The description is in the creator's language. A one-click room is the first
/// thing many people make, and handing someone reading the app in Korean an
/// English sentence they did not write is a worse first impression than the
/// duller name the CSPRNG-failure path produces.
pub fn auto_room(lang: Lang) -> (String, String) {
    let mut entropy = [0u8; 16];
    // Failure here is not fatal and must not be: the browser's CSPRNG being
    // unavailable would mean a duller name, not a broken button, and the
    // fallback still produces a valid room. Nothing security-relevant depends
    // on this — the room *key* comes from `crypto::new_room_key`, which does
    // not silently degrade.
    if getrandom::getrandom(&mut entropy).is_err() {
        return (
            t(lang, Key::new_room_fallback).to_owned(),
            auto_description(lang, 0),
        );
    }
    (
        pocketskynet_core::room_name_from_entropy(&entropy),
        auto_description(lang, entropy[0]),
    )
}

/// One of a handful of descriptions, in the creator's language.
///
/// The text itself lives in [`crate::i18n`] alongside the constraint it has to
/// respect — the server rejects ``<>{};"'`\`` in a description, so an
/// apostrophe in any of the six translations would turn the one-click button
/// into a validation error. That is pinned by a test next to the strings,
/// where a translator will see it.
fn auto_description(lang: Lang, pick: u8) -> String {
    crate::i18n::room_description(lang, pick).to_owned()
}

/// The ⚡ path in full: create encrypted, post the greeting, refresh.
///
/// A fast room opens with a hello-world already in it — the first thing the
/// user sees is a *working* room, timestamped, rather than an empty pane whose
/// encryption they have to take on faith.
///
/// The greeting is sealed with the epoch-1 key **returned by the key
/// ceremony**, not read back out of the store. This function runs on a
/// [`Store`] handle captured at some earlier render, and `UseReducerHandle`
/// reads are a snapshot of that render — dispatches from this same task update
/// only *future* renders. Going through [`send_message`] here made the room
/// lookup miss and the greeting quietly went out plaintext, badged
/// NOT ENCRYPTED in an encrypted room. Caught live; sealing from the returned
/// key removes the stale read entirely.
///
/// The `Ok((id, Some(why)))` downgrade contract matches
/// [`create_room_flow`]'s; when the ceremony fails the greeting is posted
/// plaintext — like the room it sits in — because a broken key exchange is no
/// reason to also withhold the message that proves the room itself works.
pub async fn fast_create_room(
    store: &Store,
    name: &str,
    description: &str,
) -> Result<(RoomId, Option<String>), String> {
    let room = store
        .client
        .create_room(name, Some(description))
        .await
        .map_err(|e| e.user_message())?;
    let room_id = room.id;

    let sealed: Result<[u8; 32], String> = match store.auth.session() {
        Some(session) => establish_encryption(&store.client, session, &room_id).await,
        None => Err("your encryption keys aren't unlocked on this device".into()),
    };

    let text = hello_message(store.language, &hello_entropy(), crate::format::now_ms());
    let body = match &sealed {
        // A seal failure with a good key is not a downgrade — the ROOM is
        // encrypted — so a plaintext fallback here would post readable text
        // into a sealed room. Skip the greeting instead; it is decoration.
        Ok(key) => crypto::encrypted_body(key, 1, &room_id, &text).ok(),
        Err(_) => Some(crypto::plaintext_body(&text)),
    };
    if let Some(body) = body {
        // Best effort: the room exists and opens fine without its greeting.
        // Dispatching is safe from a stale handle — it is a message to the
        // reducer, not a read — so the hello is on screen the moment the
        // room opens rather than after the first sync.
        if let Ok(msg) = store.client.send_message(&room_id, &body).await {
            store.dispatch(Action::Sync(room_id.clone(), vec![msg]));
        }
    }

    refresh_rooms(store.clone()).await;
    Ok((room_id, sealed.err()))
}

/// Two random picks for [`hello_message`]: the greeting and its extra flourish.
fn hello_entropy() -> [u8; 2] {
    let mut entropy = [0u8; 2];
    // Same policy as `auto_room`: no CSPRNG means a duller greeting, never a
    // broken button.
    let _ = getrandom::getrandom(&mut entropy);
    entropy
}

/// The first message a fast room greets its creator with.
///
/// One of several hello-worlds (each with its own emoticons) plus a randomly
/// chosen flourish, then the date and the moment it was posted in **both**
/// local time and UTC — the room is meant to be shared across timezones, and
/// stamping both is a tiny demonstration of that.
///
/// Pure — entropy and clock come in as arguments — so the shape is testable on
/// the host without a browser or a wall clock.
fn hello_message(lang: Lang, entropy: &[u8; 2], now_ms: i64) -> String {
    const FLOURISHES: [&str; 8] = ["😎", "🤖", "⚡", "🌟", "🔥", "🛰️", "🦾", "💫"];

    let greeting = crate::i18n::greeting(lang, entropy[0]);
    let flourish = FLOURISHES[entropy[1] as usize % FLOURISHES.len()];

    let local = crate::format::civil_from_ms(now_ms, crate::format::tz_offset_minutes());
    let utc = crate::format::civil_from_ms(now_ms, 0);

    format!(
        "{greeting} {flourish}\n\
         📅 {:04}-{:02}-{:02} · ⏰ {:02}:{:02} local · 🌐 {:02}:{:02} UTC",
        local.year, local.month, local.day, local.hour, local.minute, utc.hour, utc.minute,
    )
}

/// Send a message, handling both key-epoch 409s.
///
/// `STALE_KEY_VERSION` is retried **once**, under the epoch the server named.
/// `KEY_ROTATION_REQUIRED` is never retried: the room needs re-keying first,
/// and retrying would just burn rate limit against a fail-closed gate.
pub async fn send_message(store: Store, room_id: RoomId, local_id: u64, text: String) {
    let room = store.room(&room_id).cloned();
    let encrypted = room.as_ref().is_some_and(|r| r.has_encryption);

    let build = |version: i64| -> Result<crate::api::messages::MessageBody, String> {
        if !encrypted {
            return Ok(crypto::plaintext_body(&text));
        }
        let bundle = store
            .bundle(&room_id)
            .ok_or(t(store.language, Key::no_room_key_yet))?;
        let key = bundle
            .get(version)
            .ok_or(t(store.language, Key::no_current_key))?;
        crypto::encrypted_body(key, version, &room_id, &text)
            .map_err(|e| format!("Couldn't encrypt that message: {e}"))
    };

    let mut version = room
        .as_ref()
        .map(|r| r.room.current_key_version)
        .unwrap_or(1);
    let mut attempt = 0;

    loop {
        let body = match build(version) {
            Ok(b) => b,
            Err(e) => {
                store.dispatch(Action::SendFailed(room_id, local_id, e));
                return;
            }
        };

        match store.client.send_message(&room_id, &body).await {
            Ok(msg) => {
                store.dispatch(Action::Sync(room_id.clone(), vec![msg]));
                store.dispatch(Action::SendSucceeded(room_id, local_id));
                return;
            }
            Err(e) if e.is_stale_key_version() && attempt == 0 => {
                // Silent: refetch the epoch, re-encrypt, retry once. Only a
                // second failure is worth showing anyone (DESIGN.md §7.3).
                attempt += 1;
                refresh_keys(store.clone(), room_id.clone()).await;
                refresh_rooms(store.clone()).await;
                version = e.current_key_version().unwrap_or(version + 1);
            }
            Err(e) => {
                let why = if e.is_key_rotation_required() {
                    t(store.language, Key::someone_left_rotate).to_owned()
                } else {
                    e.user_message()
                };
                store.dispatch(Action::SendFailed(room_id.clone(), local_id, why));
                if e.is_key_rotation_required() {
                    refresh_rooms(store).await;
                }
                return;
            }
        }
    }
}

/// Rotate a room's key to the next epoch (CRYPTO.md §9.3).
///
/// All-or-nothing: every current member must be covered and no non-member may
/// be named. Any recipient whose public-key binding does not verify aborts the
/// whole rotation — wrapping to an unverified key is precisely the attack this
/// check exists to stop, so it fails closed rather than excluding them.
pub async fn rotate_key(store: Store, room_id: RoomId) -> Result<(), String> {
    // Rotating needs the *recipients'* public keys, not ours — but it does need
    // this device to be unlocked, because the new key is generated here.
    if !store.auth.can_decrypt() {
        return Err(t(store.language, Key::unlock_before_rotate).into());
    }

    let members = store
        .client
        .members(&room_id)
        .await
        .map_err(|e| e.user_message())?;
    let roster: Vec<WalletAddress> = members.iter().map(|m| m.user_address.clone()).collect();
    if roster.is_empty() {
        return Err(t(store.language, Key::couldnt_read_members).into());
    }

    let entries = store
        .client
        .public_keys(&roster)
        .await
        .map_err(|e| e.user_message())?;

    let new_key = crypto::new_room_key().map_err(|e| format!("Couldn't generate a key: {e}"))?;
    let (wraps, refusals) = crypto::wrap_room_key_for(&new_key, &room_id, &roster, &entries, None);

    if !refusals.is_empty() {
        let who: Vec<String> = refusals
            .iter()
            .take(3)
            .map(|r| format!("{} {}", r.address.abbreviated(), r.reason.message()))
            .collect();
        return Err(format!(
            "Can't rotate the key yet: {}. Everyone must have a verified encryption key.",
            who.join("; ")
        ));
    }

    let current = store
        .room(&room_id)
        .map(|r| r.room.current_key_version)
        .unwrap_or(1);
    let new_version = current + 1;

    match store.client.rotate_key(&room_id, new_version, &wraps).await {
        Ok(()) => {
            refresh_keys(store.clone(), room_id.clone()).await;
            refresh_rooms(store).await;
            Ok(())
        }
        Err(e) if e.status() == Some(409) => {
            // Someone else rotated first. Their key is as good as ours, so pick
            // it up rather than racing again. The message is read *before* the
            // refresh, which consumes the store.
            let message = t(store.language, Key::someone_rotated_first);
            refresh_keys(store.clone(), room_id.clone()).await;
            refresh_rooms(store).await;
            Err(message.into())
        }
        Err(e) => Err(e.user_message()),
    }
}

/// Establish epoch 1 for a freshly created room by wrapping a new key to
/// yourself.
///
/// Returns the reason on failure so the create dialog can say *why* the room is
/// plaintext. Silently downgrading an E2EE room is the worst possible outcome
/// (DESIGN.md §8), so this never swallows an error.
///
/// On success it hands back the raw epoch-1 key. The fast path needs it to
/// seal the greeting *in the same task*: dispatches update only future
/// renders, so anything this task stored ([`Action::SetBundle`]) is invisible
/// to reads through the `UseReducerHandle` it is still holding.
pub async fn establish_encryption(
    client: &Client,
    session: &Session,
    room_id: &RoomId,
) -> Result<[u8; 32], String> {
    let key = crypto::new_room_key().map_err(|e| format!("key generation failed: {e}"))?;
    let wrap: RoomKeyWrap = {
        let keys = session.keys.borrow();
        crypto::wrap_room_key_for_self(&key, room_id, &keys, 1)
            .map_err(|e| format!("key wrapping failed: {e}"))?
    };
    client
        .put_room_key(room_id, &wrap)
        .await
        .map_err(|e| e.user_message())?;
    Ok(key)
}

/// Wrap the current room key to a newly invited member, so they can read from
/// the moment they accept.
///
/// Best effort by design: the invitation itself has already succeeded, and the
/// invitee will get a key on the next rotation regardless. The caller surfaces
/// the reason rather than failing the invite.
pub async fn prewrap_key_for(
    store: &Store,
    room_id: &RoomId,
    invitee: &WalletAddress,
) -> Result<(), String> {
    let Some(bundle) = store.bundle(room_id) else {
        return Err("you don't have this room's key on this device".into());
    };
    let Some((version, key)) = bundle.latest() else {
        return Err("you don't have this room's key on this device".into());
    };

    let entries = store
        .client
        .public_keys(std::slice::from_ref(invitee))
        .await
        .map_err(|e| e.user_message())?;

    let (wraps, refusals) = crypto::wrap_room_key_for(
        key,
        room_id,
        std::slice::from_ref(invitee),
        &entries,
        Some(version),
    );
    if let Some(r) = refusals.first() {
        return Err(r.reason.message().to_owned());
    }
    let Some(wrap) = wraps.first() else {
        return Err("no key could be prepared".into());
    };

    match store.client.put_room_key(room_id, wrap).await {
        Ok(()) => Ok(()),
        // 409 means they already hold a wrap for this epoch — which is the
        // outcome we wanted, so it is a success, not a failure.
        Err(e) if e.status() == Some(409) => Ok(()),
        Err(e) => Err(e.user_message()),
    }
}

/// The full challenge → sign → login → publish-key round trip.
///
/// Order matters. The encryption salt comes back *in the login response*, so
/// the key derivation happens after authentication — which is correct, because
/// the salt is a per-account secret and there is no way to obtain it
/// unauthenticated.
pub async fn sign_in(client: Client, wallet: Wallet, username: &str) -> Result<Session, String> {
    let address = wallet.address().clone();

    let challenge = client
        .auth_challenge(&address)
        .await
        .map_err(|e| e.user_message())?;

    // Sign the server's bytes verbatim. Reconstructing the message locally
    // would break silently the day the server changes a character.
    let signature = wallet
        .personal_sign(&challenge.message)
        .map_err(|e| format!("Couldn't sign the challenge: {e}"))?;

    let login = client
        .auth_login(
            &address,
            username,
            &challenge.challenge_id,
            &signature,
            None,
            None,
        )
        .await
        .map_err(|e| e.user_message())?;

    let authed = client.with_token(Some(&login.token));

    // The login response usually carries the salt; fall back to the dedicated
    // endpoint for a server that omits it.
    let salt = match login.encryption_salt.clone() {
        Some(s) if s.len() == 64 => s,
        _ => authed
            .encryption_salt()
            .await
            .map_err(|e| e.user_message())?,
    };

    let keys = SessionKeys::derive(wallet, &salt)
        .map_err(|e| format!("Couldn't derive your encryption key: {e}"))?;

    // Re-publish the binding on every sign-in. The reference server clears
    // `public_key_sig` on any login that omits `public_key` (API.md quirk #3),
    // which silently un-binds the key and would make every other member refuse
    // to wrap a room key to us.
    if let Err(e) = authed
        .put_encryption_key(keys.public_key_hex(), keys.binding_sig())
        .await
    {
        // Not fatal: reading still works, and the next sign-in retries. But it
        // does mean nobody can send us a room key until it succeeds.
        web_sys::console::warn_1(&format!("key publish failed: {}", e.user_message()).into());
    }

    Ok(Session {
        token: login.token,
        user: login.user,
        keys: Rc::new(RefCell::new(keys)),
        fruitnation_wallet: login.fruitnation_wallet,
    })
}

/// The same round trip as [`sign_in`], but every signature is a wallet prompt.
///
/// Takes a [`Provider`](crate::eip1193::Provider) rather than reaching for
/// `window.ethereum`, which is what makes MetaMask and Privy the same feature:
/// Privy's embedded wallet hands out an EIP-1193 provider too, so both arrive
/// here and neither gets its own copy of the derivation.
///
/// Three prompts, in this order, and none can be skipped:
///
/// 1. the server's challenge — proves the address is yours;
/// 2. the **salted** derivation message — its signature *is* the E2EE private
///    key, via `keccak256`;
/// 3. the key-binding message — lets other members verify the key before
///    wrapping a room key to it.
///
/// Those messages are byte-identical to the reference client's, so the identity
/// this produces is the same one that client produces for the same wallet: sign
/// in to either and the same rooms decrypt. That interoperability is the reason
/// not to invent a cheaper scheme with fewer prompts.
///
/// The wallet key never reaches this process, so the resulting session cannot
/// sign transactions locally and cannot run the legacy healing path — both are
/// refused explicitly by `SessionKeys`, not discovered at the point of failure.
pub async fn sign_in_with_wallet(
    client: Client,
    provider: crate::eip1193::Provider,
    username: &str,
    want_chain: Option<u64>,
) -> Result<Session, String> {
    use crate::eip1193;

    let lang = Lang::En;
    let fail = |e: eip1193::WalletError| t(lang, e.key()).to_owned();

    // Connect first: the address is needed before a challenge can be asked for,
    // and this is the prompt a person expects to see immediately after clicking.
    let (address, as_given) = provider.connect().await.map_err(fail)?;

    // Nudge the wallet onto the configured chain *before* any signing, so a
    // wrong-network wallet fails at the cheap step rather than after three
    // approvals. A refusal is not fatal — signing is chain-independent, and only
    // publishing a hash later actually needs the right network.
    if let Some(chain) = want_chain {
        if provider.chain_id().await.ok() != Some(chain) {
            let _ = provider.switch_chain(chain).await;
        }
    }

    let challenge = client
        .auth_challenge(&address)
        .await
        .map_err(|e| e.user_message())?;

    // Prompt 1. The server's bytes, verbatim.
    let signature = provider
        .personal_sign(&challenge.message, &as_given)
        .await
        .map_err(fail)?;

    let login = client
        .auth_login(
            &address,
            username,
            &challenge.challenge_id,
            &signature,
            None,
            None,
        )
        .await
        .map_err(|e| e.user_message())?;

    let authed = client.with_token(Some(&login.token));

    let salt = match login.encryption_salt.clone() {
        Some(s) if s.len() == 64 => s,
        _ => authed
            .encryption_salt()
            .await
            .map_err(|e| e.user_message())?,
    };

    // Prompt 2. Built by `core`, so this client and the reference cannot drift.
    let derivation_message =
        pocketskynet_core::keys::build_salted_encryption_message(&address, &salt)
            .map_err(|e| format!("Couldn't build the derivation message: {e}"))?;
    let derivation_sig = provider
        .personal_sign(&derivation_message, &as_given)
        .await
        .map_err(fail)?;

    // Derive first, so the binding message names the key it is actually binding.
    let encryption =
        pocketskynet_core::keys::derive_encryption_keys_from_signature(&derivation_sig)
            .map_err(|e| format!("Couldn't derive your encryption key: {e}"))?;

    // Prompt 3.
    let binding_message =
        pocketskynet_core::keys::build_key_binding_message(&address, encryption.public_key_hex());
    let binding_sig = provider
        .personal_sign(&binding_message, &as_given)
        .await
        .map_err(fail)?;

    let keys = SessionKeys::from_external(address, &derivation_sig, binding_sig)
        .map_err(|e| format!("Couldn't derive your encryption key: {e}"))?;

    if let Err(e) = authed
        .put_encryption_key(keys.public_key_hex(), keys.binding_sig())
        .await
    {
        web_sys::console::warn_1(&format!("key publish failed: {}", e.user_message()).into());
    }

    Ok(Session {
        token: login.token,
        user: login.user,
        keys: Rc::new(RefCell::new(keys)),
        fruitnation_wallet: login.fruitnation_wallet,
    })
}

/// Unlock a stored session using the credential this device was told to keep
/// (`crate::vault`), without the login screen being involved.
///
/// This is what makes remembering worth anything. The login screen has its own
/// copy of this flow — it has a cutscene to play and a form to fill in — but a
/// reload rarely lands on `/login`: it lands on the room the user was reading,
/// where `Login` never mounts and the session would otherwise sit **locked**,
/// showing sealed bubbles beside a perfectly good JWT.
///
/// Silent on every failure. The session stays locked and the unlock screen is
/// one navigation away, which is exactly where the user would have been anyway;
/// a toast here would fire on every flaky reload and explain nothing.
pub async fn unlock_from_vault(store: Store) {
    let Auth::Locked(persisted) = &store.auth else {
        return;
    };
    let Some(stored) = crate::vault::StoredWallet::load_for(&persisted.wallet_address) else {
        return;
    };
    let Some(wallet) = stored.wallet() else {
        return;
    };

    // A fresh challenge, not the stored JWT: the point is to recover the *keys*,
    // and those only come from a login response's salt. The new token replaces
    // the old one, which is fine — it is the same account and the old one was
    // going to expire first.
    if let Ok(session) = sign_in(store.client.clone(), wallet, &stored.username).await {
        session.persist();
        // Parked behind the boot cutscene rather than unlocked directly, so a
        // reload gets the same SKYNET arrival as a sign-in (it is skippable,
        // and reduced-motion collapses it). `FinishBoot` promotes the session.
        store.dispatch(Action::StageBoot(session));
    }
}

/// Clear the session and return to the login screen.
///
/// The device vault goes too. A "sign out" that left the recovery phrase in
/// `localStorage` would put the next visitor to this browser straight back into
/// the account — which is the opposite of what the button says.
pub fn sign_out(store: &Store) {
    // End the Privy session as well, when there is one. Without this, signing
    // out clears everything on our side while Privy stays authenticated, so the
    // next sign-in silently returns the *same* account with no prompt — which
    // looks exactly like the app ignoring the sign-out. Fire-and-forget: the
    // local sign-out below must not wait on, or be blocked by, a third party.
    wasm_bindgen_futures::spawn_local(async move {
        crate::privy::disconnect().await;
    });

    crate::vault::clear();
    crate::session::PersistedSession::clear();
    // The cache holds only ciphertext and wrapped keys, but also room names
    // and membership — none of which the next person at this browser should
    // find. Same reasoning as the vault line above.
    crate::cache::clear_all();
    store.dispatch(Action::SetAuth(crate::session::Auth::SignedOut));
    store.dispatch(Action::SetConn(ConnStatus::Offline));
}

/// The server's cap, mirrored so a 25 MB pick fails on the device instead of
/// after the whole body has crossed the network. Must match
/// `routes/files.rs::MAX_FILE_BYTES`.
pub const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;

/// Attach a file to a room.
///
/// `caption` is whatever was in the composer when the file was picked — that is
/// how an attachment gets its `#hashtags`, and it is why there is no separate
/// tagging dialog. An empty caption is fine: the filename is indexed too, so an
/// untagged attachment is still findable.
pub async fn attach_file(
    store: Store,
    room_id: RoomId,
    filename: String,
    bytes: Vec<u8>,
    caption: String,
) {
    let lang = store.language;
    if bytes.is_empty() {
        toast::error(&store, t(lang, Key::attach_read_failed), None);
        return;
    }
    if bytes.len() > MAX_ATTACHMENT_BYTES {
        // Checked before the request: the server would refuse it anyway, but
        // only after the upload had finished, which is the worst moment to be
        // told a 40 MB file was never going to work.
        toast::error(&store, t(lang, Key::attach_too_large), None);
        return;
    }

    let file = match store
        .client
        .upload_file(room_id.as_str(), &filename, &caption, bytes)
        .await
    {
        Ok(file) => file,
        Err(e) => {
            toast::error(&store, e.user_message(), None);
            return;
        }
    };

    toast::success(
        &store,
        t(lang, Key::attach_uploaded).replace("{name}", &file.filename),
    );

    // Post it into the room, which is the whole point: an attachment that only
    // exists in the Files drawer is invisible from the conversation, and the
    // first thing anyone reported was "I attached a video and nothing
    // happened". The body is the caption (so its #hashtags stay clickable in
    // chat) followed by the attachment's own path, which `message.rs` turns
    // into a card and which is also literally readable — a client that has
    // never heard of attachments still shows something meaningful.
    //
    // Sent through the normal optimistic path, so it queues offline, retries,
    // and encrypts in an encrypted room exactly like any other message.
    let body = if caption.is_empty() {
        file.url.clone()
    } else {
        format!("{caption} {}", file.url)
    };
    let local_id = crate::state::next_local_id();
    let now = crate::format::now_ms();
    store.dispatch(Action::QueueSend(
        room_id.clone(),
        local_id,
        body.clone(),
        now,
    ));
    send_message(store, room_id, local_id, body).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_automatic_name_is_something_the_server_will_accept() {
        // Room names are 1–100 characters with no markup (validate::room_name).
        // A generated name the server then rejects would break the one button
        // that has no field for the user to fix.
        for seed in 0u8..64 {
            let name = pocketskynet_core::room_name_from_entropy(&[seed; 16]);
            let len = name.chars().count();
            assert!((1..=100).contains(&len), "{name} is {len} characters");
            assert!(
                name.chars().all(|c| c.is_ascii_alphanumeric() || c == ' '),
                "{name} contains something that needs escaping"
            );
        }
    }

    // The length and markup limits the server enforces are pinned in
    // `i18n::tests`, next to the strings, so a translator sees the constraint
    // in the file they are editing. What belongs here is the *wiring*: that
    // this module reaches the table, in whatever language the creator is in.

    #[test]
    fn the_description_varies_but_never_runs_off_the_end_of_the_list() {
        for lang in Lang::ALL {
            let all: std::collections::HashSet<String> =
                (0u8..=255).map(|p| auto_description(lang, p)).collect();
            assert!(
                all.len() > 1,
                "every fast room would read identically in {}",
                lang.tag()
            );
            // The modulus is what stops a byte outside the list length panicking.
            assert_eq!(auto_description(lang, 0), auto_description(lang, 4));
        }
    }

    #[test]
    fn the_description_is_written_in_the_creators_language() {
        // Not a translation check — that is i18n's job. This asserts only that
        // the language argument reaches the table at all: a wiring mistake that
        // pinned every room to English would otherwise pass every other test
        // here, because English is a perfectly valid description.
        assert_ne!(auto_description(Lang::Ko, 0), auto_description(Lang::En, 0));
        assert_ne!(auto_description(Lang::Ja, 2), auto_description(Lang::Es, 2));
    }

    /// 2025-06-11T14:39:06Z — an arbitrary but fixed instant.
    const HELLO_T: i64 = 1_749_652_746_000;

    #[test]
    fn the_greeting_stamps_the_date_and_both_clocks() {
        let msg = hello_message(Lang::En, &[0, 0], HELLO_T);

        // On the host the tz offset is pinned to 0 (format::tz_offset_minutes),
        // so local and UTC agree — what matters is that both are present and
        // the date is the civil date of the fixed instant.
        assert!(msg.contains("📅 2025-06-11"), "no date in: {msg}");
        assert!(msg.contains("14:39 local"), "no local time in: {msg}");
        assert!(msg.contains("14:39 UTC"), "no UTC time in: {msg}");

        // Greeting first, timestamps on their own line.
        let mut lines = msg.lines();
        assert!(lines.next().unwrap().contains("Hello, world!"));
        assert!(lines.next().unwrap().starts_with("📅"));
        assert_eq!(lines.next(), None);
    }

    #[test]
    fn the_greeting_varies_with_its_entropy_but_never_panics_on_any_byte() {
        let all: std::collections::HashSet<String> = (0u8..=255)
            .map(|b| hello_message(Lang::En, &[b, b.wrapping_add(1)], HELLO_T))
            .collect();
        assert!(all.len() > 1, "every fast room would open identically");

        // Same picks, same message — the randomness is entirely in the input.
        assert_eq!(
            hello_message(Lang::En, &[3, 7], HELLO_T),
            hello_message(Lang::En, &[3, 7], HELLO_T)
        );
    }

    #[test]
    fn every_greeting_carries_emoticons_and_is_sendable() {
        for (lang, b) in Lang::ALL
            .into_iter()
            .flat_map(|l| (0u8..=255).map(move |b| (l, b)))
        {
            let msg = hello_message(lang, &[b, b], HELLO_T);
            // "with emoticons" is the point: at least one non-ASCII glyph in
            // the greeting line itself, beyond the stamped 📅/⏰/🌐.
            assert!(
                !msg.lines().next().unwrap().is_ascii(),
                "greeting line has no emoticon: {msg}"
            );
            // Sendable: non-empty after trimming, no control characters other
            // than the one deliberate newline.
            assert!(!msg.trim().is_empty());
            assert!(msg.chars().filter(|c| c.is_control()).eq(['\n']));
        }
    }
}
