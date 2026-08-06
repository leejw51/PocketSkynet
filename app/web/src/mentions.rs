//! Turning what somebody typed into the people they meant.
//!
//! # Why the client resolves mentions at all
//!
//! The server parses `@tokens` out of plaintext too, and for a plain room that
//! would be enough. It is not enough for the two cases that matter:
//!
//! * **Encrypted rooms.** The server holds ciphertext and must keep holding
//!   only that, so it has no text to search. Without a client-side list, the
//!   rooms a team cares most about would be the ones where mentions silently
//!   did nothing.
//! * **Names with spaces.** `validate::username` allows spaces and emoji, so
//!   "@Jonghwan Lee" is a perfectly ordinary handle that no `@token` grammar
//!   recovers. The autocomplete inserts the name and records the address, and
//!   the address is what travels.
//!
//! What the client sends is *advisory*: the server checks every address
//! against the room's roster before it becomes a mention, so a stale or
//! hostile list resolves to nothing rather than to a notification.

use pocketskynet_core::WalletAddress;

use crate::api::RoomMember;

/// The longest handle the scanner will consider, in characters.
///
/// `username` allows 100, and a mention is looked up by prefix as somebody
/// types — so this bounds the work per keystroke rather than the name itself.
const MAX_HANDLE: usize = 100;

/// A browser caret position (UTF-16 code units) as a byte offset into `text`.
///
/// `selectionStart` counts UTF-16 code units — the DOM's native string unit —
/// while every scanner in this module works in bytes. For ASCII the two
/// coincide, which is exactly why the confusion shipped: every English test
/// passed while a caret after "안녕 " landed three bytes short, sliced the
/// string mid-character, and the popup silently never opened. In an app whose
/// first language toggle is Korean, "works for ASCII" is not working.
///
/// A position inside a surrogate pair (unreachable for a real caret) rounds
/// forward to the next boundary rather than panicking.
pub fn caret_to_byte(text: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (i, c) in text.char_indices() {
        if units >= utf16 {
            return i;
        }
        units += c.len_utf16();
    }
    text.len()
}

/// The inverse: a byte offset back into UTF-16 units, for `setSelectionRange`.
pub fn byte_to_caret(text: &str, byte: usize) -> usize {
    text[..byte.min(text.len())]
        .chars()
        .map(char::len_utf16)
        .sum()
}

/// Someone the composer can offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub address: WalletAddress,
    pub name: String,
    pub image: Option<String>,
}

/// An in-progress `@` the caret is sitting in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveMention {
    /// Byte offset of the `@` itself.
    pub start: usize,
    /// Byte offset just past what has been typed after it.
    pub end: usize,
    /// What was typed after the `@`, lowercased.
    pub query: String,
}

/// Find the `@handle` the caret is inside, if any.
///
/// Only fires at a token boundary — after whitespace, an opening bracket, or
/// at the start — so typing an email address does not pop a member list open
/// halfway through the domain.
///
/// `caret` is a **byte** offset into `text`.
pub fn active_mention(text: &str, caret: usize) -> Option<ActiveMention> {
    let caret = caret.min(text.len());
    let head = text.get(..caret)?;
    let at = head.rfind('@')?;

    // A boundary check on the character before the `@`.
    let before = head[..at].chars().next_back();
    let boundary = match before {
        None => true,
        Some(c) => c.is_whitespace() || matches!(c, '(' | '[' | '{' | '<' | '"' | '\'' | '“'),
    };
    if !boundary {
        return None;
    }

    let typed = &head[at + 1..];
    // A newline ends it: the `@` is on a previous line and the caret has moved
    // on, so there is nothing being completed any more.
    if typed.contains('\n') || typed.chars().count() > MAX_HANDLE {
        return None;
    }

    Some(ActiveMention {
        start: at,
        end: caret,
        query: typed.to_lowercase(),
    })
}

/// Members whose name or address matches `query`, best first.
///
/// An empty query lists everybody — pressing `@` should show who is here, not
/// wait for a letter. Ranked so a prefix match beats a match in the middle,
/// because that is the order somebody typing expects.
pub fn suggest(members: &[RoomMember], me: &WalletAddress, query: &str) -> Vec<Candidate> {
    let q = query.trim().to_lowercase();
    let mut scored: Vec<(u8, Candidate)> = members
        .iter()
        // Naming yourself is not a mention — the server drops self-mentions
        // from the inbox — so offering it would be offering a no-op.
        .filter(|m| &m.user_address != me)
        .filter_map(|m| {
            let name = m.user.display_name();
            let lower = name.to_lowercase();
            let address = m.user_address.as_str().to_lowercase();
            let rank = if q.is_empty() {
                1
            } else if lower.starts_with(&q) || address.starts_with(&q) {
                0
            } else if lower.contains(&q) {
                1
            } else {
                return None;
            };
            Some((
                rank,
                Candidate {
                    address: m.user_address.clone(),
                    name,
                    image: m.user.profile_image.clone(),
                },
            ))
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
    scored.into_iter().map(|(_, c)| c).collect()
}

/// Replace the in-progress `@handle` with a chosen name.
///
/// Returns the new text and where the caret should land. A trailing space is
/// added because the next thing anybody types after a mention is a word, and
/// without it the name and that word merge into one unresolvable handle.
pub fn apply(text: &str, active: &ActiveMention, name: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len() + name.len());
    out.push_str(&text[..active.start]);
    out.push('@');
    out.push_str(name);
    out.push(' ');
    let caret = out.len();
    out.push_str(&text[active.end..]);
    (out, caret)
}

/// Every member the finished text names.
///
/// Matches **longest name first**, so "@Jon Lee" is not read as "@Jon" when
/// both exist — the shorter name is a prefix of the longer one, and the
/// scanner has no other way to tell which was meant.
///
/// Deliberately generous about what follows a name: a mention at the end of a
/// sentence is followed by a full stop, and refusing that would silently drop
/// the most ordinary mention there is.
pub fn resolve(text: &str, members: &[RoomMember], me: &WalletAddress) -> Vec<WalletAddress> {
    let mut candidates: Vec<(String, &WalletAddress)> = members
        .iter()
        .filter(|m| &m.user_address != me)
        .flat_map(|m| {
            [
                (m.user.display_name().to_lowercase(), &m.user_address),
                (m.user_address.as_str().to_lowercase(), &m.user_address),
            ]
        })
        .filter(|(name, _)| !name.is_empty())
        .collect();
    candidates.sort_by_key(|(name, _)| std::cmp::Reverse(name.len()));

    let lower = text.to_lowercase();
    let mut found: Vec<WalletAddress> = Vec::new();

    for (i, _) in lower.match_indices('@') {
        // Same token-boundary rule as the composer's popup, so an email
        // address does not mention its own domain.
        let before = lower[..i].chars().next_back();
        let boundary = match before {
            None => true,
            Some(c) => c.is_whitespace() || matches!(c, '(' | '[' | '{' | '<' | '"' | '\'' | '“'),
        };
        if !boundary {
            continue;
        }
        let rest = &lower[i + 1..];
        for (name, address) in &candidates {
            if rest.starts_with(name.as_str()) {
                // The name must end at a boundary too, or "@bob" would match
                // inside "@bobby".
                let after = rest[name.len()..].chars().next();
                let ends = after.is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != '-');
                if ends && !found.contains(address) {
                    found.push((*address).clone());
                }
                break;
            }
        }
    }
    found
}

/// The byte ranges in `text` that are mentions of somebody in `names`.
///
/// Separate from [`resolve`] because rendering and notifying want different
/// things: `resolve` answers "who does this reach", deduplicates, and drops
/// self-mentions; this answers "what should be drawn as a chip", keeps every
/// occurrence, and includes the viewer — being named is exactly what a reader
/// most needs to see highlighted.
///
/// Longest name first, for the same reason `resolve` does it: "@bob" must not
/// win inside "@bobby".
pub fn highlight_spans(text: &str, names: &[String]) -> Vec<(usize, usize)> {
    if names.is_empty() {
        return Vec::new();
    }
    let mut sorted: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
    sorted.sort_by_key(|n| std::cmp::Reverse(n.len()));

    let lower = text.to_lowercase();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for (i, _) in lower.match_indices('@') {
        if spans.last().is_some_and(|(_, end)| i < *end) {
            continue;
        }
        let before = lower[..i].chars().next_back();
        let boundary = match before {
            None => true,
            Some(c) => c.is_whitespace() || matches!(c, '(' | '[' | '{' | '<' | '"' | '\'' | '“'),
        };
        if !boundary {
            continue;
        }
        let rest = &lower[i + 1..];
        for name in &sorted {
            if name.is_empty() || !rest.starts_with(name.as_str()) {
                continue;
            }
            let after = rest[name.len()..].chars().next();
            if after.is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != '-') {
                spans.push((i, i + 1 + name.len()));
            }
            break;
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{RoomMember, User};
    use pocketskynet_core::RoomId;

    fn member(address: &str, name: &str) -> RoomMember {
        RoomMember {
            id: 0,
            room_id: RoomId::new("room_1749652739650_aaaa").unwrap(),
            user_address: WalletAddress::new(address).unwrap(),
            joined_at: None,
            user: User {
                wallet_address: WalletAddress::new(address).unwrap(),
                username: name.to_owned(),
                public_key: None,
                public_key_sig: None,
                profile_image: None,
                created_at: None,
                updated_at: None,
            },
        }
    }

    const ALICE: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BOB: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const LONG: &str = "0xcccccccccccccccccccccccccccccccccccccccc";

    fn me() -> WalletAddress {
        WalletAddress::new(ALICE).unwrap()
    }

    #[test]
    fn caret_offsets_survive_korean_text() {
        // 안(1 UTF-16 unit / 3 bytes) 녕(1/3) space(1/1) @(1/1) b(1/1) o(1/1).
        let text = "안녕 @bo";
        // The browser reports the caret after "bo" as 6 UTF-16 units.
        let byte = caret_to_byte(text, 6);
        assert_eq!(byte, text.len());
        assert_eq!(byte_to_caret(text, byte), 6);

        // The bug this pins down: treating the DOM's 6 as a *byte* offset
        // makes the scanner see only "안녕" — everything from the space on,
        // including the @ being typed, is beyond the phantom caret. So the
        // popup silently never opened for anyone typing after CJK text.
        assert!(
            active_mention(text, 6).is_none(),
            "units-as-bytes must reproduce the old failure"
        );
        assert_eq!(
            active_mention(text, byte).map(|a| a.query),
            Some("bo".into())
        );

        // Emoji: a surrogate pair is 2 UTF-16 units and 4 bytes.
        let text = "🚀 @a";
        let total_units: usize = text.chars().map(char::len_utf16).sum();
        assert_eq!(caret_to_byte(text, total_units), text.len());
        assert_eq!(byte_to_caret(text, text.len()), 5);
    }

    #[test]
    fn the_popup_opens_only_at_a_token_boundary() {
        assert!(active_mention("hey @bo", 7).is_some());
        assert_eq!(active_mention("hey @bo", 7).unwrap().query, "bo");
        // Pressing @ with nothing typed lists everybody.
        assert_eq!(active_mention("hey @", 5).unwrap().query, "");
        // Mid-word, so this is an email and not a mention.
        assert!(active_mention("bob@example", 11).is_none());
        // The caret has moved past the token.
        assert!(active_mention("hey @bob\nnext", 13).is_none());
        // No @ at all.
        assert!(active_mention("hello", 5).is_none());
    }

    #[test]
    fn suggestions_prefer_a_prefix_and_never_offer_you() {
        let members = [
            member(ALICE, "alice"),
            member(BOB, "bob"),
            member(LONG, "bobby"),
        ];
        let hits = suggest(&members, &me(), "bob");
        assert_eq!(
            hits.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["bob", "bobby"]
        );
        // Alice is the viewer, so she is not in her own list even though an
        // empty query lists "everybody".
        let all = suggest(&members, &me(), "");
        assert!(all.iter().all(|c| c.address.as_str() != ALICE));
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn choosing_a_name_replaces_what_was_typed_and_leaves_a_space() {
        let active = active_mention("hey @bo", 7).unwrap();
        let (text, caret) = apply("hey @bo", &active, "bobby");
        assert_eq!(text, "hey @bobby ");
        assert_eq!(caret, text.len());

        // Mid-sentence: the tail survives and the caret lands before it.
        let active = active_mention("hey @bo, thanks", 7).unwrap();
        let (text, caret) = apply("hey @bo, thanks", &active, "bobby");
        assert_eq!(text, "hey @bobby , thanks");
        assert_eq!(&text[..caret], "hey @bobby ");
    }

    #[test]
    fn a_name_with_a_space_resolves_where_a_token_scanner_could_not() {
        let members = [member(ALICE, "alice"), member(BOB, "Jonghwan Lee")];
        let found = resolve("morning @Jonghwan Lee, ready?", &members, &me());
        assert_eq!(found, vec![WalletAddress::new(BOB).unwrap()]);
    }

    #[test]
    fn the_longest_matching_name_wins() {
        // "bob" is a prefix of "bobby", so scanning shortest-first would read
        // "@bobby" as a mention of bob plus the stray letters "by".
        let members = [
            member(ALICE, "alice"),
            member(BOB, "bob"),
            member(LONG, "bobby"),
        ];
        assert_eq!(
            resolve("@bobby hello", &members, &me()),
            vec![WalletAddress::new(LONG).unwrap()]
        );
        assert_eq!(
            resolve("@bob hello", &members, &me()),
            vec![WalletAddress::new(BOB).unwrap()]
        );
    }

    #[test]
    fn sentence_punctuation_does_not_break_a_mention() {
        let members = [member(ALICE, "alice"), member(BOB, "bob")];
        for text in ["thanks @bob.", "(@bob)", "@bob, please", "@bob"] {
            assert_eq!(
                resolve(text, &members, &me()),
                vec![WalletAddress::new(BOB).unwrap()],
                "{text}"
            );
        }
        // But an email is still not a mention, and naming yourself is not one.
        assert!(resolve("write to bob@example.com", &members, &me()).is_empty());
        assert!(resolve("@alice talking to myself", &members, &me()).is_empty());
    }

    #[test]
    fn an_address_can_be_written_instead_of_a_name() {
        let members = [member(ALICE, "alice"), member(BOB, "bob")];
        assert_eq!(
            resolve(&format!("ping @{BOB} please"), &members, &me()),
            vec![WalletAddress::new(BOB).unwrap()]
        );
    }

    #[test]
    fn highlight_spans_cover_the_at_and_the_whole_name() {
        let names = vec!["bob".to_owned(), "Jonghwan Lee".to_owned()];
        let text = "hi @bob and @Jonghwan Lee, ready?";
        let spans = highlight_spans(text, &names);
        let got: Vec<&str> = spans.iter().map(|(a, b)| &text[*a..*b]).collect();
        assert_eq!(got, vec!["@bob", "@Jonghwan Lee"]);
    }

    #[test]
    fn highlighting_keeps_every_occurrence_but_never_overlaps() {
        let names = vec!["bob".to_owned(), "bobby".to_owned()];
        let text = "@bob @bobby @bob";
        let spans = highlight_spans(text, &names);
        let got: Vec<&str> = spans.iter().map(|(a, b)| &text[*a..*b]).collect();
        // Unlike `resolve`, repeats are kept — each one is drawn.
        assert_eq!(got, vec!["@bob", "@bobby", "@bob"]);
    }

    #[test]
    fn an_email_is_never_highlighted() {
        let names = vec!["example".to_owned()];
        assert!(highlight_spans("write to bob@example.com", &names).is_empty());
    }

    #[test]
    fn one_mention_per_person_however_many_times_they_are_named() {
        let members = [member(ALICE, "alice"), member(BOB, "bob")];
        assert_eq!(
            resolve("@bob @bob @bob", &members, &me()),
            vec![WalletAddress::new(BOB).unwrap()]
        );
    }
}
