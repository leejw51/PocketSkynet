//! The TSS ceremony worker binary (docs/CRYPTO.md §15.3).
//!
//! Trunk builds this as a second wasm module (`data-type="worker"` in
//! `index.html`); the main app spawns it per request through
//! `src/tss.rs`. Everything CPU-heavy about a TSS wallet — safe-prime
//! generation, the DKG, every signing ceremony, the PBKDF2 that opens a
//! quorum — runs here, off the main thread, inside the user's browser.

// The one protocol source, compiled into both binaries — see its module
// docs for why it must stay self-contained.
#[path = "../tss_proto.rs"]
mod tss_proto;

use gloo_worker::Registrable;

fn main() {
    tss_proto::TssWorker::registrar().register();
}
