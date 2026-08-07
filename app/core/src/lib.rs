//! Shared foundation for the PocketSkynet server and web client.
//!
//! Everything here compiles for both the host and `wasm32-unknown-unknown`.
//! That is the point: the encryption that runs in the browser is the same code
//! the server-side test suite validates against the canonical vectors, so a
//! passing test cannot mask a WASM-only divergence.
//!
//! Nothing in this crate performs I/O, reads a clock, or knows what a database
//! is — it is pure protocol.

#![forbid(unsafe_code)]

pub mod abi;
pub mod bank;
pub mod chain;
pub mod crypto;
pub mod eip191;
pub mod events;
pub mod hash;
pub mod ids;
pub mod keys;
pub mod progression;
pub mod username;
pub mod wallet;

pub use chain::{ChainError, ChainKind, LegacyTransaction, Network, SignedTransaction, Token};
pub use crypto::{CryptoError, EncryptedMessage, WrappedRoomKey};
pub use eip191::{personal_sign, recover_address, verify_signature};
pub use events::{ClientMessage, PresenceStatus, ResyncReason, ServerEvent, Target};
pub use hash::{msg_hash_encrypted, msg_hash_plaintext, EmoticonAction};
pub use ids::{IdError, MessageId, RoomId, WalletAddress, WEBHOOK_SENDER_PREFIX};
pub use keys::{verify_key_binding, EncryptionKeypair};
pub use progression::{Award, Directive, Rank, Snapshot, Trophy};
pub use username::{deterministic_username, room_name_from_entropy};
pub use wallet::{MnemonicLength, Wallet};

/// Re-exported so downstream crates can name `SecretKey`/`PublicKey` in their
/// own signatures without having to depend on — and version-match — `k256`
/// themselves. A version skew there would produce two incompatible curve types
/// with identical names, which is a genuinely miserable error to read.
pub use k256;
