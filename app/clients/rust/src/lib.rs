//! Rust client for the PocketSkynet server.
//!
//! One API layer ([`Client`]) over two interchangeable transports
//! ([`transport::Transport`]): HTTP/1.1(+TLS) via `reqwest`, and HTTP/3 over
//! QUIC via `quinn` + `h3` — the same stack the server's own listener uses
//! (`app/server/src/http3.rs`), with ALPN `h3`.
//!
//! All signing goes through `pocketskynet-core`: the challenge string returned
//! by `POST /api/auth/challenge` is signed **verbatim** with EIP-191
//! `personal_sign` (RFC 6979 deterministic nonces, low-S, `v ∈ {27, 28}`) and
//! never reconstructed locally.

pub mod client;
pub mod error;
pub mod transport;
pub mod types;

pub use client::Client;
pub use error::ClientError;
pub use transport::{Transport, TransportOptions};

// Re-export the wallet so a caller of this library never has to depend on —
// and version-match — `pocketskynet-core` themselves just to build a signer.
pub use pocketskynet_core::wallet::Wallet;
