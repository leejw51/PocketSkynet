//! Server-side retrieval for the Knowledge feature (docs/SEARCH.md).
//!
//! Three layers, testable without a server:
//!
//! * [`text`] — tokenisation, hashtag extraction, FTS5 query quoting
//! * [`embed`] — local hashed-feature embeddings; no cloud, no downloads
//! * [`store`] — the index (SQLite FTS5 + embedding blobs) and hybrid
//!   BM25 ⊕ cosine retrieval with reciprocal-rank fusion
//!
//! The server retrieves; it never synthesises. AI answers, when the user
//! wants and approves them, are produced on the client from these results.

pub mod embed;
pub mod store;
pub mod text;
