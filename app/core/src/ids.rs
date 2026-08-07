//! Identifier newtypes.
//!
//! [`WalletAddress`] normalises to lowercase everywhere it is constructed —
//! parsing, deserialisation, and `TryFrom`. Address casing is the single most
//! common source of "why didn't this match" bugs in a protocol that uses
//! addresses as primary keys, and making the invariant a property of the type
//! means no call site can forget it.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdError {
    #[error("wallet address must be 0x followed by 40 hex characters")]
    WalletFormat,
    #[error("identifier is empty")]
    Empty,
    #[error("identifier is shorter than {min} characters")]
    TooShort { min: usize },
    #[error("identifier is longer than {max} characters")]
    TooLong { max: usize },
    #[error("identifier contains invalid characters")]
    Charset,
}

/// A lowercase, `0x`-prefixed, 20-byte Ethereum address.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WalletAddress(String);

impl WalletAddress {
    /// Parse and normalise. Accepts any casing, stores lowercase.
    pub fn new(s: &str) -> Result<Self, IdError> {
        let s = s.trim();
        if s.len() != 42 || !s.starts_with("0x") {
            return Err(IdError::WalletFormat);
        }
        if !s[2..].bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(IdError::WalletFormat);
        }
        Ok(Self(s.to_ascii_lowercase()))
    }

    /// Build from raw address bytes.
    pub fn from_bytes(bytes: &[u8; 20]) -> Self {
        Self(format!("0x{}", hex::encode(bytes)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn to_bytes(&self) -> [u8; 20] {
        let mut out = [0u8; 20];
        // Infallible: the constructors guarantee 40 hex digits after `0x`.
        hex::decode_to_slice(&self.0[2..], &mut out).expect("validated on construction");
        out
    }

    /// EIP-55 checksummed form, for display only. Never use this as a key.
    pub fn to_checksummed(&self) -> String {
        use sha3::{Digest, Keccak256};

        let lower = &self.0[2..];
        let hash = Keccak256::digest(lower.as_bytes());

        let mut out = String::with_capacity(42);
        out.push_str("0x");
        for (i, c) in lower.chars().enumerate() {
            // Nibble i of the hash decides the case of character i.
            let nibble = if i % 2 == 0 {
                hash[i / 2] >> 4
            } else {
                hash[i / 2] & 0x0f
            };
            if nibble >= 8 {
                out.extend(c.to_uppercase());
            } else {
                out.push(c);
            }
        }
        out
    }

    /// Short display form used in the UI: `0x742d…6b22`.
    pub fn abbreviated(&self) -> String {
        format!("{}…{}", &self.0[..6], &self.0[38..])
    }

    /// The same short form in EIP-55 casing: `0x742d…6B22`.
    ///
    /// Sliced out of [`Self::to_checksummed`] rather than out of the stored
    /// lowercase — which is the whole point. The abbreviation exists so
    /// someone can check it against an address they have elsewhere, and a
    /// truncation whose casing disagrees with the full form is not a shorter
    /// version of it: on a screen showing both, the same four characters
    /// appear twice in two different cases, which reads as one of them being
    /// wrong. Checksum casing is also the only integrity signal an address
    /// carries, so dropping it in the short form drops it exactly where
    /// people are most likely to eyeball rather than paste.
    ///
    /// Display only, like `to_checksummed`. Never compare on this.
    pub fn abbreviated_checksummed(&self) -> String {
        let full = self.to_checksummed();
        format!("{}…{}", &full[..6], &full[38..])
    }

    /// The sender address for an incoming webhook's posts:
    /// `0x00000000` ‖ first 32 hex chars of `SHA-256(hook id)`.
    ///
    /// A webhook post needs a sender, and every consumer of `senderAddress` —
    /// this type's own deserializer, avatar seeds, block lists, the mention
    /// resolver — expects a wallet-shaped string. Rather than teach all of
    /// them a second grammar, a webhook *gets* a wallet address: derived from
    /// its id, so it is stable across posts, distinct per webhook, and held
    /// by nobody — recovering a private key whose account is a chosen address
    /// is a preimage attack on Keccak. It can therefore never sign a login
    /// challenge, which is what "not impersonating a member" means here.
    ///
    /// The four zero bytes are the machine-readable attribution: clients
    /// render anything under this prefix as a webhook, not a person (see
    /// [`Self::is_webhook_sender`]). The derivation lives here in core so the
    /// server that writes the address and the client that badges it cannot
    /// drift apart.
    pub fn webhook_sender(hook_id: &str) -> Self {
        use sha2::{Digest, Sha256};
        let digest = hex::encode(Sha256::digest(hook_id.as_bytes()));
        Self(format!("{WEBHOOK_SENDER_PREFIX}{}", &digest[..32]))
    }

    /// Whether this address was minted by [`Self::webhook_sender`].
    ///
    /// A person *could* grind ~2³² keys for a vanity address under this
    /// prefix, and would thereby earn themselves a robot badge on their own
    /// messages — that is the entire prize. The direction that would matter,
    /// a webhook landing on an address somebody actually holds, is not up for
    /// grinding: a webhook's address is fixed by its id, not chosen.
    pub fn is_webhook_sender(&self) -> bool {
        self.0.starts_with(WEBHOOK_SENDER_PREFIX)
    }

    /// The sender address of one person's AI agent — the other half of the
    /// "My Jarvis" room: `0x000000a1` ‖ first 32 hex chars of
    /// `SHA-256("jarvis:" ‖ owner)`.
    ///
    /// Exactly the same trick as [`Self::webhook_sender`], and for exactly the
    /// same reason: an agent's reply is an ordinary message, so it needs an
    /// ordinary `senderAddress` that avatars, block lists, mention resolution
    /// and the roster can all read without learning a second grammar. Deriving
    /// it from the owner makes it stable across replies and distinct per
    /// person, so two people's agents never share an identity — which matters
    /// because the address is also the avatar seed, and one shared Jarvis face
    /// on a family server would read as everybody talking to the same machine.
    ///
    /// Held by nobody, like a webhook's: finding a private key for a chosen
    /// address is a preimage attack on Keccak, so this address can never sign
    /// a login challenge. The agent therefore cannot *act* — it can only be
    /// spoken as, by the server, on behalf of the one wallet whose room it is.
    ///
    /// The `a1` byte is the machine-readable attribution and the only thing
    /// separating this from the webhook block; clients badge anything under it
    /// as an agent (see [`Self::is_agent_sender`]). The derivation lives in
    /// core so the server that writes the address and the client that badges
    /// it cannot drift apart.
    pub fn agent_of(owner: &WalletAddress) -> Self {
        use sha2::{Digest, Sha256};
        let digest = hex::encode(Sha256::digest(
            format!("jarvis:{}", owner.as_str()).as_bytes(),
        ));
        Self(format!("{AGENT_SENDER_PREFIX}{}", &digest[..32]))
    }

    /// Whether this address was minted by [`Self::agent_of`].
    pub fn is_agent_sender(&self) -> bool {
        self.0.starts_with(AGENT_SENDER_PREFIX)
    }
}

/// The reserved leading bytes of every webhook sender address.
pub const WEBHOOK_SENDER_PREFIX: &str = "0x00000000";

/// The reserved leading bytes of every AI-agent sender address.
///
/// One byte apart from [`WEBHOOK_SENDER_PREFIX`] on purpose: both name a
/// speaker that is not a person, and keeping them in the same all-but-empty
/// corner of the address space means a future third kind has an obvious place
/// to go rather than a fresh argument about where to put it.
pub const AGENT_SENDER_PREFIX: &str = "0x000000a1";

impl fmt::Display for WalletAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for WalletAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WalletAddress({})", self.0)
    }
}

impl FromStr for WalletAddress {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl Serialize for WalletAddress {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for WalletAddress {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::new(&raw).map_err(serde::de::Error::custom)
    }
}

/// An opaque, server-assigned room identifier.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct RoomId(String);

/// An opaque, server-assigned message identifier.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct MessageId(String);

/// Generates the shared body of the two opaque-id newtypes: same conversions,
/// different type so they cannot be swapped at a call site, and a per-type
/// charset predicate so each matches the protocol's own rule.
macro_rules! opaque_id {
    ($ty:ident, $min:expr, $max:expr, $allowed:expr) => {
        impl $ty {
            /// Minimum accepted length, per `docs/API.md` §3.1.
            pub const MIN_LEN: usize = $min;
            /// Maximum accepted length, per `docs/API.md` §3.1.
            pub const MAX_LEN: usize = $max;

            pub fn new(s: &str) -> Result<Self, IdError> {
                let s = s.trim();
                if s.is_empty() {
                    return Err(IdError::Empty);
                }
                if s.len() < $min {
                    return Err(IdError::TooShort { min: $min });
                }
                if s.len() > $max {
                    return Err(IdError::TooLong { max: $max });
                }
                // Restricting the charset keeps ids safe to interpolate into
                // log lines, file names, and URL paths without escaping.
                let allowed: fn(u8) -> bool = $allowed;
                if !s.bytes().all(allowed) {
                    return Err(IdError::Charset);
                }
                Ok(Self(s.to_owned()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($ty), self.0)
            }
        }

        impl FromStr for $ty {
            type Err = IdError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::new(s)
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(d)?;
                Self::new(&raw).map_err(serde::de::Error::custom)
            }
        }
    };
}

// Room ids permit a dot; message ids do not. The asymmetry is not an accident
// in the protocol — it is `docs/API.md` §3.1 — and reproducing it here means a
// dotted room id survives the round trip instead of being rejected by a type
// that was stricter than the wire format.
opaque_id!(RoomId, 10, 100, |b| b.is_ascii_alphanumeric()
    || b == b'_'
    || b == b'-'
    || b == b'.');
opaque_id!(MessageId, 10, 100, |b| b.is_ascii_alphanumeric()
    || b == b'_'
    || b == b'-');

#[cfg(test)]
mod tests {
    use super::*;

    const MIXED: &str = "0x742d35Cc6634C0532925a3b8D31cE5bb1C6E6B22";
    const LOWER: &str = "0x742d35cc6634c0532925a3b8d31ce5bb1c6e6b22";

    #[test]
    fn wallet_normalises_case_on_every_construction_path() {
        assert_eq!(WalletAddress::new(MIXED).unwrap().as_str(), LOWER);
        assert_eq!(MIXED.parse::<WalletAddress>().unwrap().as_str(), LOWER);

        let via_serde: WalletAddress = serde_json::from_str(&format!("\"{MIXED}\"")).unwrap();
        assert_eq!(via_serde.as_str(), LOWER);
    }

    #[test]
    fn differently_cased_addresses_compare_and_hash_equal() {
        let a = WalletAddress::new(MIXED).unwrap();
        let b = WalletAddress::new(LOWER).unwrap();
        assert_eq!(a, b);

        let mut set = std::collections::HashSet::new();
        set.insert(a);
        assert!(set.contains(&b), "casing must not split hash buckets");
    }

    #[test]
    fn wallet_rejects_malformed_input() {
        for bad in [
            "",
            "0x",
            LOWER.trim_start_matches("0x"),                // no prefix
            "0x742d35cc6634c0532925a3b8d31ce5bb1c6e6b2",   // 39 digits
            "0x742d35cc6634c0532925a3b8d31ce5bb1c6e6b222", // 41 digits
            "0x742d35cc6634c0532925a3b8d31ce5bb1c6e6bzz",  // non-hex
        ] {
            assert_eq!(
                WalletAddress::new(bad).err(),
                Some(IdError::WalletFormat),
                "should have rejected {bad:?}"
            );
        }
    }

    #[test]
    fn wallet_round_trips_through_bytes() {
        let a = WalletAddress::new(MIXED).unwrap();
        assert_eq!(WalletAddress::from_bytes(&a.to_bytes()), a);
    }

    #[test]
    fn eip55_checksum_matches_the_reference_vectors() {
        // From EIP-55 itself.
        for expected in [
            "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed",
            "0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359",
            "0xdbF03B407c01E7cD3CBea99509d93f8DDDC8C6FB",
            "0xD1220A0cf47c7B9Be7A2E6BA89F429762e7b9aDb",
        ] {
            let addr = WalletAddress::new(expected).unwrap();
            assert_eq!(addr.to_checksummed(), expected);
        }
    }

    #[test]
    fn abbreviated_keeps_both_ends() {
        let a = WalletAddress::new(MIXED).unwrap();
        assert_eq!(a.abbreviated(), "0x742d…6b22");
    }

    #[test]
    fn the_checksummed_abbreviation_is_a_slice_of_the_checksummed_form() {
        // Not merely "is uppercase somewhere": the point is that the visible
        // characters are the *same characters* the full form shows, so the two
        // can be compared by eye when a screen offers both.
        let a = WalletAddress::new(MIXED).unwrap();
        let full = a.to_checksummed();
        let short = a.abbreviated_checksummed();

        // Note the casing is *not* MIXED's own: that constant is a mixed-case
        // input for the normalisation test, not this address's real checksum.
        // Its actual EIP-55 form is 0x742D35cC…, so the head gains a capital
        // the plain abbreviation does not have and the tail keeps its
        // lowercase — which is exactly the kind of difference that makes
        // slicing the right string matter.
        assert_eq!(short, "0x742D…6b22");
        let (head, tail) = short.split_once('…').expect("one ellipsis");
        assert!(full.starts_with(head), "{full} does not start with {head}");
        assert!(full.ends_with(tail), "{full} does not end with {tail}");
    }

    #[test]
    fn the_two_abbreviations_differ_only_in_case() {
        // The guard against someone "simplifying" one into the other: they
        // must stay the same address, and must not be the same string.
        let a = WalletAddress::new(MIXED).unwrap();
        let plain = a.abbreviated();
        let checked = a.abbreviated_checksummed();
        assert_ne!(plain, checked);
        assert_eq!(plain, checked.to_lowercase());
    }

    #[test]
    fn opaque_ids_reject_traversal_and_separators() {
        for bad in [
            "../../etc/passwd",
            "room/1234567890",
            "room 1234567890",
            "room\n1234567890",
            "room;DROP TABLE",
            "room%2e%2e1234",
            "room'or'1'='1",
        ] {
            assert!(RoomId::new(bad).is_err(), "should have rejected {bad:?}");
            assert!(MessageId::new(bad).is_err(), "should have rejected {bad:?}");
        }
        assert!(RoomId::new("room_1749652739650_304e0eaf").is_ok());
        assert!(MessageId::new("msg_1749652900000_ab12cd34").is_ok());
    }

    #[test]
    fn room_ids_accept_a_dot_and_message_ids_do_not() {
        // Not a stylistic choice — API.md §3.1 gives roomId the charset
        // [a-zA-Z0-9_.-] and messageId [a-zA-Z0-9_-]. A type stricter than the
        // wire format would reject ids the server legitimately issues.
        assert!(RoomId::new("room.with.dots.1").is_ok());
        assert_eq!(
            MessageId::new("msg.with.dots.1").err(),
            Some(IdError::Charset)
        );
    }

    #[test]
    fn opaque_ids_enforce_both_length_bounds() {
        assert_eq!(
            RoomId::new(&"a".repeat(101)).err(),
            Some(IdError::TooLong { max: 100 })
        );
        assert_eq!(
            RoomId::new("short").err(),
            Some(IdError::TooShort { min: 10 })
        );
        assert_eq!(RoomId::new("").err(), Some(IdError::Empty));

        // The bounds themselves are inclusive on both ends.
        assert!(RoomId::new(&"a".repeat(10)).is_ok());
        assert!(RoomId::new(&"a".repeat(100)).is_ok());
    }

    #[test]
    fn webhook_senders_are_valid_addresses_under_the_reserved_prefix() {
        let hook = WalletAddress::webhook_sender("hook_1749652746620_ab12cd34");

        // Wallet-shaped: everything that parses addresses must accept it, or
        // one webhook post would fail deserialisation of a whole message list.
        assert!(WalletAddress::new(hook.as_str()).is_ok());
        assert!(hook.is_webhook_sender());
        assert!(hook.as_str().starts_with(WEBHOOK_SENDER_PREFIX));

        // Stable per id, distinct across ids.
        assert_eq!(
            hook,
            WalletAddress::webhook_sender("hook_1749652746620_ab12cd34")
        );
        assert_ne!(
            hook,
            WalletAddress::webhook_sender("hook_1749652746620_other")
        );

        // An ordinary wallet is never mistaken for one.
        let person = WalletAddress::new("0x742d35Cc6634C0532925a3b844Bc9e7595f06b22").unwrap();
        assert!(!person.is_webhook_sender());
    }

    #[test]
    fn agent_senders_are_derived_from_their_owner_and_nobody_else() {
        let alice = WalletAddress::new("0x742d35Cc6634C0532925a3b844Bc9e7595f06b22").unwrap();
        let bob = WalletAddress::new("0x1111111111111111111111111111111111111111").unwrap();
        let agent = WalletAddress::agent_of(&alice);

        assert!(WalletAddress::new(agent.as_str()).is_ok());
        assert!(agent.is_agent_sender());
        assert!(agent.as_str().starts_with(AGENT_SENDER_PREFIX));

        // Stable for one owner — the agent keeps the same face across every
        // reply it ever posts — and never shared between two people.
        assert_eq!(agent, WalletAddress::agent_of(&alice));
        assert_ne!(agent, WalletAddress::agent_of(&bob));

        // The two reserved kinds do not overlap, in either direction.
        assert!(!agent.is_webhook_sender());
        assert!(!WalletAddress::webhook_sender("hook_1749652746620_ab12cd34").is_agent_sender());
        assert!(!alice.is_agent_sender());
    }
}
