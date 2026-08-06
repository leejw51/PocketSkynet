//! Per-room message state: the `/sync` fold, cursor arithmetic, block
//! filtering and stream grouping.
//!
//! Everything in this module is pure — no I/O, no clock, no `web_sys` — so all
//! of it is unit-tested on the host. That matters because the fold is the one
//! place where a subtle bug silently loses or resurrects a message.
//!
//! The model to keep in mind (API.md §8.1): `/sync` is an **idempotent
//! state-transfer stream, not an event log**. Edits, deletes and on-chain
//! publishes reuse the row and merely advance its serial, so folding must
//! *upsert*, never "patch if present".

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use pocketskynet_core::{MessageId, WalletAddress};

use crate::api::{Message, MsgKind};

/// Reactions for one message: emoticon code → the set of reactors.
///
/// A `BTreeMap`/`BTreeSet` rather than hashes so the rendered chip order is
/// deterministic; a reaction row that reshuffles on every re-render is a
/// visible bug even though the contents are identical.
pub type ReactionMap = BTreeMap<String, BTreeSet<WalletAddress>>;

/// Everything known about one room's stream.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RoomState {
    /// Live messages, keyed by id. Deleted rows are removed outright rather
    /// than tombstoned: the server scrubs their content and hash, so a
    /// tombstone would carry no information a "message deleted" placeholder
    /// does not, and keeping them complicates every count.
    pub messages: HashMap<MessageId, Message>,
    /// Reactions, keyed by **target** message id. Kept even when the target is
    /// unknown — the target may arrive later via backfill (API.md §9).
    pub reactions: HashMap<MessageId, ReactionMap>,
    /// High-water `msgSerial`. Strictly increasing; `/sync` is `> since`.
    pub cursor: i64,
    /// Whether a full history load has completed at least once. Distinguishes
    /// "empty room" from "not loaded yet", which render very differently.
    pub loaded: bool,
    /// Whether an older page exists behind the oldest message held.
    pub has_more_history: bool,
    /// The read pointer last confirmed by the server.
    pub last_read_serial: i64,
}

/// The outcome of folding one batch, so callers can react without diffing.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FoldOutcome {
    /// Ids added or replaced.
    pub upserted: Vec<MessageId>,
    /// Ids removed.
    pub removed: Vec<MessageId>,
    /// The room's cache was cleared by a `delete_all` marker.
    pub purged: bool,
    /// The highest serial seen in the batch (0 if the batch was empty).
    pub max_serial: i64,
}

/// The sort key for display order.
///
/// `msg_serial` as the tiebreak, **not** `id`. An id is `msg_{millis}_{uuid}`,
/// so it looks like a stable secondary sort and is not one: within a
/// millisecond it orders by a random UUID, which shuffled any burst of
/// messages — most visibly a thread, where three quick replies came back in a
/// different order on every render. The serial is the room's own monotonic
/// counter and is the only column guaranteed to increase with insertion order.
/// The server's queries were fixed the same way.
fn order_key(m: &Message) -> (i64, i64) {
    (m.message_timestamp, m.msg_serial)
}

impl RoomState {
    /// Fold one `/sync` event into the state (API.md §9).
    ///
    /// Unknown `msgType` values are **ignored, not rejected** — a server that
    /// grows a seventh event type must degrade this client to skipping it, not
    /// to dropping the rest of the page.
    pub fn fold_one(&mut self, event: &Message, out: &mut FoldOutcome) {
        // The cursor advances for *every* event, including ones we ignore.
        // Otherwise an unknown or blocked-out event at the head of the stream
        // would pin the cursor and re-deliver the same page forever.
        self.cursor = self.cursor.max(event.msg_serial);
        out.max_serial = out.max_serial.max(event.msg_serial);

        match event.kind() {
            MsgKind::Add | MsgKind::Edit => {
                // Upsert. An `edit` for a message we never saw is normal: it
                // means the message was edited before we first synced, and it
                // arrives once, already edited.
                self.messages.insert(event.id.clone(), event.clone());
                out.upserted.push(event.id.clone());
            }
            MsgKind::Delete => {
                if self.messages.remove(&event.id).is_some() {
                    out.removed.push(event.id.clone());
                }
                // Orphaned reactions would otherwise leak memory and could
                // resurface if an id were ever reused.
                self.reactions.remove(&event.id);
            }
            MsgKind::DeleteAll => {
                // Must clear **at this point in serial order**, not at the end
                // of the batch: later events in the same page are post-purge.
                self.messages.clear();
                self.reactions.clear();
                self.has_more_history = false;
                out.purged = true;
                out.upserted.clear();
                out.removed.clear();
            }
            MsgKind::EmoticonAdd => {
                if let (Some(target), Some(code)) =
                    (event.target_message_id.clone(), event.emoticon_code.clone())
                {
                    self.reactions
                        .entry(target)
                        .or_default()
                        .entry(code)
                        .or_default()
                        .insert(event.sender_address.clone());
                }
            }
            MsgKind::EmoticonRemove => {
                if let (Some(target), Some(code)) = (
                    event.target_message_id.as_ref(),
                    event.emoticon_code.as_ref(),
                ) {
                    if let Some(codes) = self.reactions.get_mut(target) {
                        if let Some(set) = codes.get_mut(code) {
                            set.remove(&event.sender_address);
                            if set.is_empty() {
                                codes.remove(code);
                            }
                        }
                        if codes.is_empty() {
                            self.reactions.remove(target);
                        }
                    }
                }
            }
            MsgKind::Unknown => {}
        }
    }

    /// Fold a whole `/sync` page, in the order received.
    pub fn fold(&mut self, events: &[Message]) -> FoldOutcome {
        let mut out = FoldOutcome::default();
        for e in events {
            self.fold_one(e, &mut out);
        }
        out
    }

    /// Merge a `GET /messages` backfill page.
    ///
    /// Backfill is *not* folded: that endpoint already excludes deleted rows,
    /// reaction events and purge markers, and — critically — its rows must not
    /// move the sync cursor. Their serials are arbitrary relative to the live
    /// stream (an old message can have a very recent serial after an edit), so
    /// letting them advance `cursor` would skip live events.
    pub fn merge_history(&mut self, page: &[Message]) {
        for m in page {
            if m.kind().is_renderable() && !m.is_deleted {
                self.messages
                    .entry(m.id.clone())
                    .or_insert_with(|| m.clone());
            }
        }
    }

    /// Snapshot for the persisted cache (`cache.rs`): the newest rows in wire
    /// form plus the folded reaction table, capped so a busy room cannot eat
    /// the storage quota. Reactions are kept only for rows that survived the
    /// cap — a chip whose message is gone would never render again anyway.
    pub fn to_cached(&self) -> crate::cache::CachedRoom {
        let rows = crate::cache::cap_rows(self.messages.values().cloned().collect());
        let kept: std::collections::HashSet<&MessageId> = rows.iter().map(|m| &m.id).collect();
        let reactions = self
            .reactions
            .iter()
            .filter(|(id, _)| kept.contains(id))
            .map(|(id, codes)| {
                (
                    id.clone(),
                    codes
                        .iter()
                        .map(|(c, who)| (c.clone(), who.iter().cloned().collect()))
                        .collect(),
                )
            })
            .collect();
        crate::cache::CachedRoom {
            rows,
            reactions,
            has_more_history: self.has_more_history,
        }
    }

    /// Rebuild a stream from its cached snapshot. The cursor is the highest
    /// serial in the rows — `/sync` picks up from there, so hydration and a
    /// live session converge on the same state.
    pub fn from_cached(cached: crate::cache::CachedRoom, persisted_cursor: i64) -> Self {
        let mut st = Self {
            has_more_history: cached.has_more_history,
            loaded: true,
            ..Self::default()
        };
        for m in cached.rows {
            st.cursor = st.cursor.max(m.msg_serial);
            if m.kind().is_renderable() && !m.is_deleted {
                st.messages.insert(m.id.clone(), m);
            }
        }
        // The persisted cursor can be ahead of the newest cached row (reaction
        // and delete events advance it without leaving a renderable row).
        st.cursor = st.cursor.max(persisted_cursor);
        for (id, codes) in cached.reactions {
            let map = st.reactions.entry(id).or_default();
            for (code, who) in codes {
                map.entry(code).or_default().extend(who);
            }
        }
        st
    }

    /// Messages in display order: `messageTimestamp` ascending, ties broken on
    /// id for stability.
    ///
    /// Explicitly **not** ordered by `msgSerial` — an edited message keeps its
    /// original timestamp but gains a new serial, and sorting by serial would
    /// make every edit jump to the bottom of the conversation.
    pub fn ordered<'a>(&'a self, blocks: &BlockSet) -> Vec<&'a Message> {
        let mut v: Vec<&Message> = self
            .messages
            .values()
            .filter(|m| !blocks.hides(&m.sender_address))
            .collect();
        v.sort_by_key(|m| order_key(m));
        v
    }

    /// The channel view: top-level messages only.
    ///
    /// Replies are deliberately excluded even though `/sync` delivered them and
    /// they are sitting in `messages` — that is what threads are *for*, and it
    /// is also what the server's `GET /messages` does. Holding them locally is
    /// what lets [`Self::replies_to`] open a thread without a request.
    pub fn ordered_top_level<'a>(&'a self, blocks: &BlockSet) -> Vec<&'a Message> {
        self.ordered(blocks)
            .into_iter()
            .filter(|m| {
                match &m.parent_message_id {
                    None => true,
                    // A reply whose parent is not in the local fold is
                    // *promoted* to the stream rather than hidden. Two ways
                    // that state is reached, both legitimate: the root was
                    // deleted (this fold drops deleted rows outright, so
                    // there is no parent row to hang a thread opener on), or
                    // the root is older than the backfill window and only the
                    // reply arrived over /sync. Filtering such a reply out
                    // would strand it — held in memory, on the server,
                    // renderable, and reachable from nowhere. When the parent
                    // later arrives via backfill, the reply collapses back
                    // under it.
                    Some(parent) => !self.messages.contains_key(parent),
                }
            })
            .collect()
    }

    /// One thread's replies, oldest first.
    ///
    /// Answered from the local fold rather than by fetching, because `/sync`
    /// already delivered every reply — the server hides them from the *channel*
    /// query, not from the stream. Opening a thread is therefore instant and
    /// works offline, and the count below cannot disagree with the list.
    pub fn replies_to<'a>(&'a self, root: &MessageId, blocks: &BlockSet) -> Vec<&'a Message> {
        let mut v: Vec<&Message> = self
            .messages
            .values()
            .filter(|m| m.parent_message_id.as_ref() == Some(root))
            .filter(|m| !m.is_deleted && m.kind().is_renderable())
            .filter(|m| !blocks.hides(&m.sender_address))
            .collect();
        v.sort_by_key(|m| order_key(m));
        v
    }

    /// How many replies to show on the parent's footer.
    ///
    /// The local count wins when there is one, because it is block-filtered and
    /// includes replies that arrived since the page loaded. The server's
    /// `replyCount` is the fallback for the case the local fold cannot cover: a
    /// message loaded by backward pagination, whose replies were never in any
    /// `/sync` window this session.
    pub fn reply_count(&self, message: &Message, blocks: &BlockSet) -> i64 {
        let local = self.replies_to(&message.id, blocks).len() as i64;
        local.max(message.reply_count.unwrap_or(0))
    }

    /// The oldest `messageTimestamp` held — the cursor for backward paging.
    /// Pagination must key on this, never on the returned row count.
    pub fn oldest_timestamp(&self) -> Option<i64> {
        self.messages.values().map(|m| m.message_timestamp).min()
    }

    /// Visible reactions for a message, with blocked reactors removed.
    ///
    /// The server's aggregation endpoint is *not* block-filtered, which is why
    /// folding from `/sync` is the recommended path — the two can disagree, and
    /// this one is the view the user should get.
    pub fn reactions_for(
        &self,
        id: &MessageId,
        blocks: &BlockSet,
    ) -> Vec<(String, Vec<WalletAddress>)> {
        let Some(codes) = self.reactions.get(id) else {
            return Vec::new();
        };
        codes
            .iter()
            .filter_map(|(code, senders)| {
                let visible: Vec<WalletAddress> = senders
                    .iter()
                    .filter(|s| !blocks.hides(s))
                    .cloned()
                    .collect();
                (!visible.is_empty()).then(|| (code.clone(), visible))
            })
            .collect()
    }

    /// Locally computed unread count, used to update the badge before the next
    /// `GET /api/rooms` confirms it.
    ///
    /// Mirrors the server's definition (`msgSerial > lastReadSerial`, `add`
    /// rows only, not your own) **and additionally excludes blocked senders**,
    /// which the server's count does not — API.md §11 flags that as a bug worth
    /// not reproducing, since it badges messages you can never fetch.
    pub fn local_unread(&self, me: &WalletAddress, blocks: &BlockSet) -> u32 {
        self.messages
            .values()
            .filter(|m| {
                m.msg_serial > self.last_read_serial
                    && matches!(m.kind(), MsgKind::Add)
                    && !m.is_deleted
                    && &m.sender_address != me
                    && !blocks.hides(&m.sender_address)
            })
            .count() as u32
    }
}

/// The union of "everyone I blocked" and "everyone who blocked me".
///
/// One set, not two, because the UI must never reveal which direction a block
/// runs (DESIGN.md §9): both cases render identically, so keeping them apart
/// would only create an opportunity to leak the difference.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BlockSet {
    inner: HashSet<WalletAddress>,
}

impl BlockSet {
    pub fn from_pairs(blocked: &[WalletAddress], blocked_by: &[WalletAddress]) -> Self {
        Self {
            inner: blocked.iter().chain(blocked_by).cloned().collect(),
        }
    }

    /// Whether content from this address should be hidden.
    pub fn hides(&self, who: &WalletAddress) -> bool {
        self.inner.contains(who)
    }
}

/// The next `since` value for the drain loop.
///
/// Returns `None` when the loop should stop. The loop must terminate on
/// `hasMore == false` **and** on an empty page, because a page consisting
/// entirely of blocked senders' rows comes back empty with the cursor
/// unchanged — continuing would spin forever.
pub fn next_sync_cursor(
    current: i64,
    batch_max: i64,
    has_more: bool,
    batch_len: usize,
) -> Option<i64> {
    if !has_more || batch_len == 0 {
        None
    } else {
        Some(current.max(batch_max))
    }
}

/// Whether a message row should render its own sender header (DESIGN.md §7.2).
///
/// A header appears when the sender changes, when more than five minutes have
/// passed, or across a day boundary. Otherwise the row is `--grouped`.
pub fn starts_new_group(prev: Option<&Message>, cur: &Message, tz_offset_minutes: i32) -> bool {
    let Some(prev) = prev else { return true };
    if prev.sender_address != cur.sender_address {
        return true;
    }
    if cur.message_timestamp - prev.message_timestamp > 5 * 60_000 {
        return true;
    }
    let a = crate::format::civil_from_ms(prev.message_timestamp, tz_offset_minutes);
    let b = crate::format::civil_from_ms(cur.message_timestamp, tz_offset_minutes);
    a.epoch_day != b.epoch_day
}

/// Whether a day marker should be drawn before this message.
pub fn starts_new_day(prev: Option<&Message>, cur: &Message, tz_offset_minutes: i32) -> bool {
    let Some(prev) = prev else { return true };
    crate::format::civil_from_ms(prev.message_timestamp, tz_offset_minutes).epoch_day
        != crate::format::civil_from_ms(cur.message_timestamp, tz_offset_minutes).epoch_day
}

#[cfg(test)]
mod tests {
    use super::*;
    use pocketskynet_core::RoomId;

    fn room() -> RoomId {
        RoomId::new("room_1749652739650_304e0eaf").unwrap()
    }

    fn addr(n: u8) -> WalletAddress {
        WalletAddress::new(&format!("0x{:040x}", n as u32)).unwrap()
    }

    fn mid(s: &str) -> MessageId {
        MessageId::new(s).unwrap()
    }

    #[test]
    fn threads_fold_under_their_parent_and_orphans_are_promoted() {
        let blocks = BlockSet::default();
        let mut st = RoomState::default();
        let reply = |id: &str, serial: i64, ts: i64, parent: &str| {
            let mut e = ev(id, "add", serial, ts, 2);
            e.parent_message_id = Some(mid(parent));
            e
        };
        st.fold(&[
            ev("msg_aaaa_01", "add", 10, 100, 1),
            reply("msg_aaaa_02", 11, 200, "msg_aaaa_01"),
            reply("msg_aaaa_03", 12, 300, "msg_aaaa_01"),
            ev("msg_aaaa_04", "add", 13, 400, 1),
        ]);

        // The channel shows the parent and the unrelated message; the two
        // replies are under the parent, in order, and counted.
        let top: Vec<_> = st
            .ordered_top_level(&blocks)
            .iter()
            .map(|m| m.id.as_str().to_owned())
            .collect();
        assert_eq!(top, vec!["msg_aaaa_01", "msg_aaaa_04"]);
        let thread: Vec<_> = st
            .replies_to(&mid("msg_aaaa_01"), &blocks)
            .iter()
            .map(|m| m.id.as_str().to_owned())
            .collect();
        assert_eq!(thread, vec!["msg_aaaa_02", "msg_aaaa_03"]);
        let parent = st.messages[&mid("msg_aaaa_01")].clone();
        assert_eq!(st.reply_count(&parent, &blocks), 2);

        // Delete the root. This fold drops deleted rows outright, so there is
        // no parent row left to hang a thread opener on — the replies must be
        // promoted into the stream rather than stranded in memory, reachable
        // from nowhere.
        st.fold(&[ev("msg_aaaa_01", "delete", 14, 100, 1)]);
        let top: Vec<_> = st
            .ordered_top_level(&blocks)
            .iter()
            .map(|m| m.id.as_str().to_owned())
            .collect();
        assert_eq!(top, vec!["msg_aaaa_02", "msg_aaaa_03", "msg_aaaa_04"]);

        // The same promotion covers a reply that arrived over /sync when its
        // root is older than anything backfill has loaded…
        let mut cold = RoomState::default();
        cold.fold(&[reply("msg_aaaa_09", 20, 900, "msg_aaaa_00")]);
        assert_eq!(cold.ordered_top_level(&blocks).len(), 1);

        // …and when the root later arrives via backfill, the reply collapses
        // back under it.
        cold.merge_history(&[ev("msg_aaaa_00", "add", 1, 50, 1)]);
        let top: Vec<_> = cold
            .ordered_top_level(&blocks)
            .iter()
            .map(|m| m.id.as_str().to_owned())
            .collect();
        assert_eq!(top, vec!["msg_aaaa_00"]);
        let parent = cold.messages[&mid("msg_aaaa_00")].clone();
        assert_eq!(cold.reply_count(&parent, &blocks), 1);
    }

    #[test]
    fn a_stream_survives_the_cache_round_trip() {
        // Fold a realistic sequence — messages, a reaction, a delete — then
        // snapshot and rebuild. The rebuilt stream must render identically:
        // same rows, same reactions, same cursor, same pagination flag. This
        // is the property the zero-network open stands on.
        let mut st = RoomState::default();
        st.fold(&[
            ev("msg_aaaa_01", "add", 10, 100, 1),
            ev("msg_aaaa_02", "add", 11, 200, 2),
            {
                let mut e = ev("msg_aaaa_03", "emoticon_add", 12, 300, 2);
                e.target_message_id = Some(mid("msg_aaaa_01"));
                e.emoticon_code = Some("🍇".into());
                e
            },
            ev("msg_aaaa_02", "delete", 13, 200, 2),
        ]);
        st.has_more_history = true;

        let rebuilt = RoomState::from_cached(st.to_cached(), st.cursor);

        let blocks = BlockSet::default();
        let a: Vec<_> = st.ordered(&blocks).into_iter().cloned().collect();
        let b: Vec<_> = rebuilt.ordered(&blocks).into_iter().cloned().collect();
        assert_eq!(a, b);
        assert_eq!(
            rebuilt.cursor, st.cursor,
            "sync must resume from the same place"
        );
        assert_eq!(
            rebuilt.reactions_for(&mid("msg_aaaa_01"), &blocks),
            st.reactions_for(&mid("msg_aaaa_01"), &blocks),
        );
        assert!(rebuilt.has_more_history);
        assert!(rebuilt.loaded, "a hydrated room must not show a spinner");
    }

    #[test]
    fn hydration_respects_a_cursor_ahead_of_the_rows() {
        // Reaction and delete events advance the persisted cursor without
        // leaving a renderable row, so the stored cursor can be ahead of every
        // cached message. Resuming from the rows' max would re-deliver those
        // events on every reload.
        let mut st = RoomState::default();
        st.fold(&[ev("msg_aaaa_01", "add", 10, 100, 1)]);
        let rebuilt = RoomState::from_cached(st.to_cached(), 25);
        assert_eq!(rebuilt.cursor, 25);
    }

    /// A minimal event builder — the fold only ever looks at a handful of fields.
    fn ev(id: &str, kind: &str, serial: i64, ts: i64, sender: u8) -> Message {
        Message {
            id: mid(id),
            room_id: room(),
            sender_address: addr(sender),
            content: format!("content of {id}"),
            msg_hash: "a".repeat(64),
            message_timestamp: ts,
            msg_type: kind.into(),
            msg_serial: serial,
            is_deleted: kind == "delete",
            edited_at: (kind == "edit").then(|| "2025-06-11T14:39:06.000Z".to_owned()),
            created_at: None,
            is_encrypted: false,
            iv: None,
            hmac: None,
            enc_ver: None,
            key_version: None,
            tx_hash: None,
            target_message_id: None,
            emoticon_code: None,
            parent_message_id: None,
            reply_count: None,
            last_reply_at: None,
            sender: None,
        }
    }

    fn react(id: &str, kind: &str, serial: i64, target: &str, code: &str, sender: u8) -> Message {
        let mut m = ev(id, kind, serial, serial, sender);
        m.target_message_id = Some(mid(target));
        m.emoticon_code = Some(code.into());
        m
    }

    // --- the fold ---------------------------------------------------------

    #[test]
    fn add_inserts_and_advances_the_cursor() {
        let mut s = RoomState::default();
        let out = s.fold(&[ev("msg_aaaa_01", "add", 100, 100, 1)]);
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.cursor, 100);
        assert_eq!(out.upserted, vec![mid("msg_aaaa_01")]);
        assert_eq!(out.max_serial, 100);
    }

    #[test]
    fn edit_upserts_even_when_the_original_was_never_seen() {
        // The bug PROTOCOL.md's pseudocode has: "patch if present" would drop
        // a message that was edited before this client's first sync.
        let mut s = RoomState::default();
        s.fold(&[ev("msg_aaaa_01", "edit", 200, 100, 1)]);
        assert_eq!(s.messages.len(), 1);
        assert!(s.messages[&mid("msg_aaaa_01")].is_edited());
    }

    #[test]
    fn edit_replaces_the_whole_row_not_just_the_content() {
        let mut s = RoomState::default();
        s.fold(&[ev("msg_aaaa_01", "add", 100, 100, 1)]);
        let mut edited = ev("msg_aaaa_01", "edit", 200, 100, 1);
        edited.content = "the new text".into();
        edited.is_encrypted = true;
        edited.key_version = Some(3);
        s.fold(&[edited]);

        let m = &s.messages[&mid("msg_aaaa_01")];
        assert_eq!(m.content, "the new text");
        assert!(m.is_encrypted);
        assert_eq!(m.key_version(), 3);
        assert_eq!(s.cursor, 200);
    }

    #[test]
    fn delete_removes_the_message_and_its_orphaned_reactions() {
        let mut s = RoomState::default();
        s.fold(&[
            ev("msg_aaaa_01", "add", 100, 100, 1),
            react("emo_1____13", "emoticon_add", 110, "msg_aaaa_01", "🍎", 2),
        ]);
        assert!(s.reactions.contains_key(&mid("msg_aaaa_01")));

        let out = s.fold(&[ev("msg_aaaa_01", "delete", 120, 100, 1)]);
        assert!(s.messages.is_empty());
        assert!(
            s.reactions.is_empty(),
            "reactions must not outlive their target"
        );
        assert_eq!(out.removed, vec![mid("msg_aaaa_01")]);
    }

    #[test]
    fn delete_all_clears_at_its_position_in_serial_order_not_at_the_end() {
        // The events *after* the marker in the same page are post-purge and
        // must survive; a naive "clear at end of batch" implementation loses
        // them, which looks like message loss to the user.
        let mut s = RoomState::default();
        let out = s.fold(&[
            ev("msg_aaaa_01", "add", 100, 100, 1),
            ev("msg_bbbb_02", "add", 110, 110, 2),
            ev("marker___12", "delete_all", 120, 120, 1),
            ev("msg_cccc_03", "add", 130, 130, 1),
        ]);
        assert_eq!(s.messages.len(), 1);
        assert!(s.messages.contains_key(&mid("msg_cccc_03")));
        assert!(out.purged);
        assert_eq!(s.cursor, 130);
    }

    #[test]
    fn delete_all_also_drops_every_reaction() {
        let mut s = RoomState::default();
        s.fold(&[
            ev("msg_aaaa_01", "add", 100, 100, 1),
            react("emo_1____13", "emoticon_add", 110, "msg_aaaa_01", "🍎", 2),
            ev("marker___12", "delete_all", 120, 120, 1),
        ]);
        assert!(s.reactions.is_empty());
    }

    #[test]
    fn reactions_are_set_based_so_duplicate_adds_are_idempotent() {
        let mut s = RoomState::default();
        s.fold(&[
            ev("msg_aaaa_01", "add", 100, 100, 1),
            react("emo_1____13", "emoticon_add", 110, "msg_aaaa_01", "🍎", 2),
            react("emo_2____14", "emoticon_add", 120, "msg_aaaa_01", "🍎", 2),
            react("emo_3____15", "emoticon_add", 130, "msg_aaaa_01", "🍎", 3),
        ]);
        let r = s.reactions_for(&mid("msg_aaaa_01"), &BlockSet::default());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "🍎");
        assert_eq!(
            r[0].1.len(),
            2,
            "the same reactor twice is still one reactor"
        );
    }

    #[test]
    fn removing_the_last_reactor_drops_the_code_entirely() {
        let mut s = RoomState::default();
        s.fold(&[
            ev("msg_aaaa_01", "add", 100, 100, 1),
            react("emo_1____13", "emoticon_add", 110, "msg_aaaa_01", "🍎", 2),
            react(
                "emo_2____14",
                "emoticon_remove",
                120,
                "msg_aaaa_01",
                "🍎",
                2,
            ),
        ]);
        assert!(s
            .reactions_for(&mid("msg_aaaa_01"), &BlockSet::default())
            .is_empty());
        // …and the whole entry, so the map does not grow without bound.
        assert!(s.reactions.is_empty());
    }

    #[test]
    fn removing_a_reaction_you_never_added_is_a_harmless_no_op() {
        let mut s = RoomState::default();
        s.fold(&[
            ev("msg_aaaa_01", "add", 100, 100, 1),
            react("emo_1____13", "emoticon_add", 110, "msg_aaaa_01", "🍎", 2),
            react(
                "emo_2____14",
                "emoticon_remove",
                120,
                "msg_aaaa_01",
                "🍎",
                9,
            ),
        ]);
        let r = s.reactions_for(&mid("msg_aaaa_01"), &BlockSet::default());
        assert_eq!(r[0].1.len(), 1);
    }

    #[test]
    fn a_reaction_targeting_an_unknown_message_is_kept_for_later_backfill() {
        let mut s = RoomState::default();
        s.fold(&[react(
            "emo_1____13",
            "emoticon_add",
            110,
            "msg_ghost11",
            "🍇",
            2,
        )]);
        assert!(s.messages.is_empty());
        assert_eq!(
            s.reactions.len(),
            1,
            "must survive until the target arrives"
        );
    }

    #[test]
    fn an_unknown_event_type_is_skipped_without_aborting_the_batch() {
        let mut s = RoomState::default();
        let out = s.fold(&[
            ev("msg_aaaa_01", "add", 100, 100, 1),
            ev("msg_xxxx_05", "teleport", 110, 110, 1),
            ev("msg_bbbb_02", "add", 120, 120, 1),
        ]);
        assert_eq!(s.messages.len(), 2);
        // The cursor still passes the unknown event, or we would re-fetch it
        // on every poll forever.
        assert_eq!(s.cursor, 120);
        assert_eq!(out.max_serial, 120);
    }

    #[test]
    fn the_db_default_msg_type_is_treated_as_add() {
        let mut s = RoomState::default();
        s.fold(&[ev("msg_aaaa_01", "message", 100, 100, 1)]);
        assert_eq!(s.messages.len(), 1);
    }

    #[test]
    fn a_republished_row_upserts_its_tx_hash_with_no_special_case() {
        let mut s = RoomState::default();
        s.fold(&[ev("msg_aaaa_01", "add", 100, 100, 1)]);
        let mut anchored = ev("msg_aaaa_01", "add", 300, 100, 1);
        anchored.tx_hash = Some("0xabc".into());
        s.fold(&[anchored]);
        assert_eq!(
            s.messages[&mid("msg_aaaa_01")].tx_hash.as_deref(),
            Some("0xabc")
        );
        assert_eq!(s.cursor, 300);
    }

    #[test]
    fn the_fold_is_idempotent_under_replay() {
        // `/sync` is a state-transfer stream: replaying a page must be a no-op.
        let batch = vec![
            ev("msg_aaaa_01", "add", 100, 100, 1),
            react("emo_1____13", "emoticon_add", 110, "msg_aaaa_01", "🍎", 2),
            ev("msg_bbbb_02", "add", 120, 120, 2),
            ev("msg_aaaa_01", "edit", 130, 100, 1),
        ];
        let mut once = RoomState::default();
        once.fold(&batch);
        let mut twice = RoomState::default();
        twice.fold(&batch);
        twice.fold(&batch);
        assert_eq!(once, twice);
    }

    // --- ordering ---------------------------------------------------------

    #[test]
    fn display_order_is_by_timestamp_so_an_edit_does_not_jump_to_the_bottom() {
        let mut s = RoomState::default();
        s.fold(&[
            ev("msg_aaaa_01", "add", 100, 100, 1),
            ev("msg_bbbb_02", "add", 110, 110, 2),
            ev("msg_cccc_03", "add", 120, 120, 1),
            // msg_a edited: newest serial, oldest timestamp.
            ev("msg_aaaa_01", "edit", 999, 100, 1),
        ]);
        let ids: Vec<&str> = s
            .ordered(&BlockSet::default())
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(ids, vec!["msg_aaaa_01", "msg_bbbb_02", "msg_cccc_03"]);
    }

    #[test]
    fn ties_on_timestamp_break_on_id_so_the_order_is_stable() {
        let mut s = RoomState::default();
        s.fold(&[
            ev("msg_cccc_03", "add", 3, 100, 1),
            ev("msg_aaaa_01", "add", 1, 100, 1),
            ev("msg_bbbb_02", "add", 2, 100, 1),
        ]);
        let first: Vec<String> = s
            .ordered(&BlockSet::default())
            .iter()
            .map(|m| m.id.to_string())
            .collect();
        let second: Vec<String> = s
            .ordered(&BlockSet::default())
            .iter()
            .map(|m| m.id.to_string())
            .collect();
        assert_eq!(first, second);
        assert_eq!(first, vec!["msg_aaaa_01", "msg_bbbb_02", "msg_cccc_03"]);
    }

    // --- blocking ---------------------------------------------------------

    #[test]
    fn block_filtering_is_symmetric_and_hides_messages_and_reactions_alike() {
        let blocks = BlockSet::from_pairs(&[addr(2)], &[addr(3)]);
        assert!(blocks.hides(&addr(2)), "someone I blocked");
        assert!(blocks.hides(&addr(3)), "someone who blocked me");
        assert!(!blocks.hides(&addr(1)));

        let mut s = RoomState::default();
        s.fold(&[
            ev("msg_aaaa_01", "add", 100, 100, 1),
            ev("msg_bbbb_02", "add", 110, 110, 2),
            ev("msg_cccc_03", "add", 120, 120, 3),
            react("emo_1____13", "emoticon_add", 130, "msg_aaaa_01", "🍎", 2),
            react("emo_2____14", "emoticon_add", 140, "msg_aaaa_01", "🍎", 1),
        ]);
        let visible: Vec<&str> = s.ordered(&blocks).iter().map(|m| m.id.as_str()).collect();
        assert_eq!(visible, vec!["msg_aaaa_01"]);

        let r = s.reactions_for(&mid("msg_aaaa_01"), &blocks);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].1, vec![addr(1)], "the blocked reactor is hidden");
    }

    #[test]
    fn a_reaction_code_whose_every_reactor_is_blocked_disappears_entirely() {
        let blocks = BlockSet::from_pairs(&[addr(2)], &[]);
        let mut s = RoomState::default();
        s.fold(&[
            ev("msg_aaaa_01", "add", 100, 100, 1),
            react("emo_1____13", "emoticon_add", 110, "msg_aaaa_01", "🍎", 2),
        ]);
        assert!(s.reactions_for(&mid("msg_aaaa_01"), &blocks).is_empty());
    }

    // --- cursor -----------------------------------------------------------

    #[test]
    fn cursor_never_regresses_even_if_a_page_arrives_out_of_order() {
        let mut s = RoomState::default();
        s.fold(&[ev("msg_bbbb_02", "add", 500, 110, 1)]);
        s.fold(&[ev("msg_aaaa_01", "add", 100, 100, 1)]);
        assert_eq!(s.cursor, 500);
    }

    #[test]
    fn the_drain_loop_terminates_on_both_documented_conditions() {
        // Normal continuation.
        assert_eq!(next_sync_cursor(0, 500, true, 500), Some(500));
        // Server says there is no more.
        assert_eq!(next_sync_cursor(0, 500, false, 500), None);
        // An empty page — every row was filtered out by a block. Continuing
        // would spin, because the cursor cannot advance past rows we cannot see.
        assert_eq!(next_sync_cursor(400, 0, true, 0), None);
        assert_eq!(next_sync_cursor(400, 0, false, 0), None);
    }

    #[test]
    fn history_merge_does_not_move_the_sync_cursor() {
        // A backfilled message can carry a very recent serial (it was edited),
        // and letting that advance the cursor would skip live events.
        let mut s = RoomState {
            cursor: 100,
            ..Default::default()
        };
        s.merge_history(&[ev("msg_old__06", "add", 9_999, 50, 1)]);
        assert_eq!(s.cursor, 100);
        assert_eq!(s.messages.len(), 1);
    }

    #[test]
    fn history_merge_never_overwrites_a_fresher_live_row() {
        let mut s = RoomState::default();
        let mut live = ev("msg_aaaa_01", "add", 200, 100, 1);
        live.content = "edited text".into();
        s.fold(&[live]);
        s.merge_history(&[ev("msg_aaaa_01", "add", 100, 100, 1)]);
        assert_eq!(s.messages[&mid("msg_aaaa_01")].content, "edited text");
    }

    #[test]
    fn history_merge_drops_events_that_are_not_messages() {
        let mut s = RoomState::default();
        s.merge_history(&[
            ev("msg_aaaa_01", "add", 1, 1, 1),
            ev("msg_bbbb_02", "delete", 2, 2, 1),
            react("emo______16", "emoticon_add", 3, "msg_aaaa_01", "🍎", 1),
            ev("marker___12", "delete_all", 4, 4, 1),
        ]);
        assert_eq!(s.messages.len(), 1);
    }

    #[test]
    fn oldest_timestamp_drives_backward_paging_not_the_row_count() {
        let mut s = RoomState::default();
        assert!(s.oldest_timestamp().is_none());
        s.fold(&[
            ev("msg_bbbb_02", "add", 110, 110, 1),
            ev("msg_aaaa_01", "add", 100, 90, 1),
        ]);
        assert_eq!(s.oldest_timestamp(), Some(90));
    }

    // --- unread -----------------------------------------------------------

    #[test]
    fn local_unread_matches_the_servers_definition_plus_block_filtering() {
        let mut s = RoomState {
            last_read_serial: 100,
            ..Default::default()
        };
        s.fold(&[
            ev("msg_old__06", "add", 90, 90, 2),   // below the pointer
            ev("msg_new__07", "add", 110, 110, 2), // counts
            ev("msg_mine_08", "add", 120, 120, 1), // my own never counts
            ev("msg_blk__09", "add", 130, 130, 3), // blocked sender
            ev("msg_edit_10", "edit", 140, 95, 2), // edits never count
        ]);
        let blocks = BlockSet::from_pairs(&[addr(3)], &[]);
        assert_eq!(s.local_unread(&addr(1), &blocks), 1);
        // Without the block, the edited row still does not count.
        assert_eq!(s.local_unread(&addr(1), &BlockSet::default()), 2);
    }

    // --- grouping ---------------------------------------------------------

    #[test]
    fn grouping_breaks_on_sender_change_time_gap_and_day_boundary() {
        let base = 1_749_652_746_000;
        let a = ev("msg_aaaa_01", "add", 1, base, 1);

        // First message always starts a group.
        assert!(starts_new_group(None, &a, 0));
        // Same sender, one minute later: grouped.
        let b = ev("msg_bbbb_02", "add", 2, base + 60_000, 1);
        assert!(!starts_new_group(Some(&a), &b, 0));
        // Same sender, six minutes later: new group.
        let c = ev("msg_cccc_03", "add", 3, base + 6 * 60_000, 1);
        assert!(starts_new_group(Some(&a), &c, 0));
        // Different sender, one second later: new group.
        let d = ev("msg_dddd_04", "add", 4, base + 1_000, 2);
        assert!(starts_new_group(Some(&a), &d, 0));
    }

    #[test]
    fn a_day_boundary_breaks_the_group_even_within_five_minutes() {
        let late = crate::format::parse_iso8601_ms("2025-06-11T23:59:00.000Z").unwrap();
        let early = crate::format::parse_iso8601_ms("2025-06-12T00:01:00.000Z").unwrap();
        let a = ev("msg_aaaa_01", "add", 1, late, 1);
        let b = ev("msg_bbbb_02", "add", 2, early, 1);
        assert!(starts_new_group(Some(&a), &b, 0));
        assert!(starts_new_day(Some(&a), &b, 0));
        assert!(starts_new_day(None, &a, 0));
    }

    #[test]
    fn day_marker_boundaries_follow_the_viewers_timezone() {
        // Both are 2025-06-11 in UTC; in UTC+9 the second one is the 12th.
        let t = |s: &str| crate::format::parse_iso8601_ms(s).unwrap();
        let a = ev("msg_aaaa_01", "add", 1, t("2025-06-11T05:20:00.000Z"), 1);
        let b = ev("msg_bbbb_02", "add", 2, t("2025-06-11T23:20:00.000Z"), 1);
        assert!(!starts_new_day(Some(&a), &b, 0));
        assert!(starts_new_day(Some(&a), &b, 9 * 60));
    }
}
