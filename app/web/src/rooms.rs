//! The built-in rooms, and how the sidebar files everything into categories.
//!
//! # Why this is its own module
//!
//! Three facts about each built-in room are needed in four different places —
//! the room list, the chat header, the composer's agent hook and the hidden
//! rooms dialog — and every one of them is a *lookup on `kind`*: what to call
//! it, which picture it wears, and one line saying what it is for. Left inline
//! they would be three `match` arms repeated four times, and the failure mode
//! of that is not a compile error. It is a room list that says "My Note" over a
//! chat header that says the raw server name, because somebody added a kind to
//! one match and not the others.
//!
//! So the vocabulary lives here, once, as a type — and the categorisation that
//! reads it lives here too, beside it, where it can be tested without a browser.
//! Both halves are pure: no Yew, no `web_sys`, no store. That is what lets the
//! interesting decisions (which rooms are pinned, what happens when a category
//! is empty) be checked by `cargo test` on the host rather than by looking at a
//! screenshot.
//!
//! # Why the titles are translated rather than taken from the server
//!
//! `rooms.name` holds "My Note" because the column is `NOT NULL` and the admin
//! console has to print something. But these three rooms are part of the
//! *interface*, not content somebody typed — a Korean user did not name their
//! notebook in English — so they are translated here exactly as a DM is titled
//! after its members rather than after the placeholder the column holds.

use crate::api::{RoomKind, RoomWithMembers};
use crate::i18n::Key;

/// One of the three rooms every account has.
///
/// An enum rather than bare `&str` comparisons so that adding a fourth is a
/// compile error at every site that decides something per kind, which is the
/// property the four call sites were missing when this was inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticRoom {
    /// A place to talk to yourself. One member, forever; nobody else can read
    /// it, join it or be invited to it, and the server enforces that rather
    /// than the button being hidden.
    Note,
    /// A conversation with the user's own AI agent. The key stays in the
    /// browser and the model call is made from there; the server only writes
    /// the answer down.
    Jarvis,
    /// The standing line to whoever runs this server — the owner plus the
    /// wallets `VITE_FRUITNATION_ADMIN` names, with nobody having to invite
    /// anybody.
    Lobby,
}

impl StaticRoom {
    /// Pinned order, and the order the room list renders them in. Note first
    /// because it is the one people open every day; lobby last because it is
    /// the one they open when something is wrong.
    pub const ALL: [StaticRoom; 3] = [StaticRoom::Note, StaticRoom::Jarvis, StaticRoom::Lobby];

    /// Which built-in room this wire `kind` names, if any.
    pub fn from_kind(kind: &str) -> Option<Self> {
        match kind {
            RoomKind::NOTE => Some(StaticRoom::Note),
            RoomKind::JARVIS => Some(StaticRoom::Jarvis),
            RoomKind::LOBBY => Some(StaticRoom::Lobby),
            _ => None,
        }
    }

    pub fn kind(self) -> &'static str {
        match self {
            StaticRoom::Note => RoomKind::NOTE,
            StaticRoom::Jarvis => RoomKind::JARVIS,
            StaticRoom::Lobby => RoomKind::LOBBY,
        }
    }

    /// The translated name shown wherever the room is named.
    pub fn title(self) -> Key {
        match self {
            StaticRoom::Note => Key::room_my_note,
            StaticRoom::Jarvis => Key::room_my_jarvis,
            StaticRoom::Lobby => Key::room_my_lobby,
        }
    }

    /// One line saying what the room is for, shown where there is space for it.
    ///
    /// Worth carrying because these rooms arrive unannounced: a user who never
    /// created them is owed an answer to "what is this and who can see it",
    /// and the answer differs sharply between the three.
    pub fn blurb(self) -> Key {
        match self {
            StaticRoom::Note => Key::room_my_note_blurb,
            StaticRoom::Jarvis => Key::room_my_jarvis_blurb,
            StaticRoom::Lobby => Key::room_my_lobby_blurb,
        }
    }

    /// The art stem for this room — see `asset::img`.
    ///
    /// A specific picture rather than the hashed room sigil every other room
    /// wears. The sigils exist to make eight rooms distinguishable from one
    /// another; these three sit pinned at the top of the list and have to say
    /// *what they are*, which a hash cannot.
    pub fn art(self) -> &'static str {
        match self {
            StaticRoom::Note => "my-note",
            StaticRoom::Jarvis => "my-jarvis",
            StaticRoom::Lobby => "my-lobby",
        }
    }
}

/// Which built-in room this is, if it is one.
pub fn static_room(room: &RoomWithMembers) -> Option<StaticRoom> {
    StaticRoom::from_kind(&room.room.kind)
}

/// The headings the sidebar groups rooms under.
///
/// Deliberately not "one heading per kind". A DM and a group DM are the same
/// thing to somebody scanning a list — a conversation with people rather than a
/// place — and splitting them would put a heading over a list of one for most
/// users. The built-in rooms are the opposite case and are the reason this enum
/// exists: they are three *different* things that nevertheless belong together,
/// because what they have in common is the thing worth saying about them, which
/// is that you did not make them and cannot remove them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// The three built-in rooms, pinned above everything else.
    Mine,
    /// Rooms somebody created and invited people to.
    Channels,
    /// Direct and group messages.
    Directs,
}

impl Category {
    /// Render order. `Mine` first is the whole point of the category: these
    /// are the rooms that are always there, so they are always in the same
    /// place, and a list that reordered them by recent activity would move the
    /// one fixed landmark in the sidebar.
    pub const ALL: [Category; 3] = [Category::Mine, Category::Channels, Category::Directs];

    pub fn of(room: &RoomWithMembers) -> Self {
        if static_room(room).is_some() {
            Category::Mine
        } else if room.is_direct() {
            Category::Directs
        } else {
            Category::Channels
        }
    }

    pub fn heading(self) -> Key {
        match self {
            Category::Mine => Key::section_my_rooms,
            Category::Channels => Key::section_channels,
            Category::Directs => Key::section_direct_messages,
        }
    }
}

/// One sidebar section: its heading and the rooms under it, each carrying the
/// index it had in the flat list.
pub type Section<T> = (Option<Key>, Vec<(usize, T)>);

/// Group a room list into its categories, keeping the index each row had in
/// the flat list.
///
/// The index is what drives the CSS entrance stagger, so it has to count across
/// the whole list rather than restart per section — otherwise the sections
/// animate on top of each other.
///
/// Two rules about headings, both inherited from the DM/channel split this
/// replaces. Empty categories produce no heading, obviously. Less obviously, a
/// list that is *entirely one category* gets no heading either: a heading over
/// the only list on screen labels something nobody could have confused, and on
/// a phone it costs one of the few rows that fit.
///
/// Ordering *within* a category is the caller's, untouched — `sorted_rooms`
/// has already put the most recently active first, and the built-in three
/// inherit that among themselves.
pub fn sectioned<T: Clone>(rooms: &[T], category: impl Fn(&T) -> Category) -> Vec<Section<T>> {
    let mut buckets: Vec<(Category, Vec<(usize, T)>)> =
        Category::ALL.iter().map(|c| (*c, Vec::new())).collect();
    for (i, room) in rooms.iter().enumerate() {
        let want = category(room);
        if let Some(bucket) = buckets.iter_mut().find(|(c, _)| *c == want) {
            bucket.1.push((i, room.clone()));
        }
    }

    let filled: Vec<(Category, Vec<(usize, T)>)> = buckets
        .into_iter()
        .filter(|(_, rooms)| !rooms.is_empty())
        .collect();
    let label = filled.len() > 1;
    filled
        .into_iter()
        .map(|(c, rooms)| (label.then(|| c.heading()), rooms))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three built-in kinds and the three ordinary ones, as `Category::of`
    /// sees them — a bare `&str` stands in for a room so the classification
    /// can be tested without building a whole `RoomWithMembers`.
    fn category_of_kind(kind: &str) -> Category {
        if StaticRoom::from_kind(kind).is_some() {
            Category::Mine
        } else if kind == RoomKind::DM || kind == RoomKind::GROUP_DM {
            Category::Directs
        } else {
            Category::Channels
        }
    }

    #[test]
    fn every_wire_kind_classifies_into_exactly_one_category() {
        assert_eq!(category_of_kind(RoomKind::NOTE), Category::Mine);
        assert_eq!(category_of_kind(RoomKind::JARVIS), Category::Mine);
        assert_eq!(category_of_kind(RoomKind::LOBBY), Category::Mine);
        assert_eq!(category_of_kind(RoomKind::DM), Category::Directs);
        assert_eq!(category_of_kind(RoomKind::GROUP_DM), Category::Directs);
        assert_eq!(category_of_kind(RoomKind::CHANNEL), Category::Channels);
    }

    /// A kind this build has never heard of has to land somewhere sensible.
    /// A newer server could add one, and the wrong answer is not "crash" — it
    /// is a room that vanishes from the sidebar because no bucket claimed it.
    #[test]
    fn an_unknown_kind_is_filed_as_an_ordinary_channel() {
        assert_eq!(category_of_kind("holodeck"), Category::Channels);
        assert_eq!(StaticRoom::from_kind("holodeck"), None);
    }

    #[test]
    fn the_static_vocabulary_round_trips_and_is_distinct() {
        for room in StaticRoom::ALL {
            assert_eq!(StaticRoom::from_kind(room.kind()), Some(room));
        }
        // Three kinds, three titles, three pictures — no accidental sharing,
        // which is exactly the copy-paste error this table invites.
        let kinds: Vec<&str> = StaticRoom::ALL.iter().map(|r| r.kind()).collect();
        let art: Vec<&str> = StaticRoom::ALL.iter().map(|r| r.art()).collect();
        for set in [kinds, art] {
            let mut sorted = set.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), set.len(), "{set:?} repeats a value");
        }
        let titles: Vec<Key> = StaticRoom::ALL.iter().map(|r| r.title()).collect();
        assert_ne!(titles[0], titles[1]);
        assert_ne!(titles[1], titles[2]);
        assert_ne!(titles[0], titles[2]);
    }

    #[test]
    fn categories_render_with_the_built_in_rooms_pinned_first() {
        // Deliberately shuffled: the input is ordered by recent activity, so
        // a channel really can arrive before a built-in room.
        let rooms = vec!["channel", "note", "dm", "lobby", "group_dm", "jarvis"];
        let sections = sectioned(&rooms, |k| category_of_kind(k));

        let headings: Vec<Option<Key>> = sections.iter().map(|(h, _)| *h).collect();
        assert_eq!(
            headings,
            vec![
                Some(Key::section_my_rooms),
                Some(Key::section_channels),
                Some(Key::section_direct_messages),
            ]
        );
        assert_eq!(
            sections[0].1.iter().map(|(_, k)| *k).collect::<Vec<_>>(),
            vec!["note", "lobby", "jarvis"],
            "pinned above the rest, in the order the caller gave them"
        );
        assert_eq!(
            sections[2].1.iter().map(|(_, k)| *k).collect::<Vec<_>>(),
            vec!["dm", "group_dm"]
        );
    }

    #[test]
    fn the_stagger_index_counts_across_the_whole_list() {
        // If the index restarted per section, two rows would share an index
        // and animate on top of each other.
        let rooms = vec!["channel", "note", "dm"];
        let sections = sectioned(&rooms, |k| category_of_kind(k));
        let mut seen: Vec<usize> = sections
            .iter()
            .flat_map(|(_, rows)| rows.iter().map(|(i, _)| *i))
            .collect();
        seen.sort_unstable();
        assert_eq!(seen, vec![0, 1, 2]);
    }

    #[test]
    fn a_list_of_one_category_gets_no_heading_at_all() {
        // The commonest state of a brand-new account: three built-in rooms and
        // nothing else. A heading over the only list on screen labels
        // something nobody could have confused.
        let rooms = vec!["note", "jarvis", "lobby"];
        let sections = sectioned(&rooms, |k| category_of_kind(k));
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].0, None);
        assert_eq!(sections[0].1.len(), 3);
    }

    #[test]
    fn an_empty_list_produces_no_sections() {
        let rooms: Vec<&str> = vec![];
        assert!(sectioned(&rooms, |k| category_of_kind(k)).is_empty());
    }

    #[test]
    fn a_hidden_built_in_room_simply_leaves_its_category() {
        // Hiding is a list preference, so it reaches the sidebar as a room
        // that is not in the list at all — and the category must then vanish
        // rather than render an empty heading.
        let rooms = vec!["channel", "dm"];
        let sections = sectioned(&rooms, |k| category_of_kind(k));
        assert_eq!(
            sections.iter().map(|(h, _)| *h).collect::<Vec<_>>(),
            vec![
                Some(Key::section_channels),
                Some(Key::section_direct_messages)
            ]
        );
    }
}
