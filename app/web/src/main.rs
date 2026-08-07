//! PocketSkynet web client — Yew → WebAssembly, styled with Topcoat.
//!
//! Layering, top to bottom:
//!
//! * `components/` — one module per screen; pure functions of the store.
//! * `app` — routing, the auth gate, and the realtime lifecycle.
//! * `state` — the single `use_reducer` store every screen reads.
//! * `actions` — everything asynchronous: the `/sync` drain loop, key epochs.
//! * `api` — a typed client mirroring `docs/API.md`, one module per group.
//! * `crypto` — policy over `pocketskynet_core`: key cache, fail-closed wraps.
//! * `session`, `vault` — what survives a reload, and the one thing that only
//!   survives it because the user asked for it to.
//! * `asset` — the one place a `/static/img/…` URL is built, so a skin swap is
//!   a lookup rather than a hunt through the markup for string literals.
//! * `store`, `format`, `identity`, `route`, `realtime` — pure logic, host-tested.
//!
//! The crate is a binary rather than a library because Trunk expects one, but
//! everything below `app` is written as if it were a library: no globals, no
//! `unwrap` on a network boundary, and every pure function unit-tested under a
//! plain `cargo test` on the host.

mod actions;
mod ai;
mod api;
mod app;
mod asset;
mod bank_agent;
mod cache;
mod capture;
mod components;
mod crypto;
mod eip1193;
mod format;
mod i18n;
mod identity;
mod media;
mod mentions;
mod privy;
mod progression;
mod realtime;
mod route;
mod rooms;
mod rpc;
mod secrets;
mod session;
mod state;
mod store;
mod vault;

fn main() {
    // A panic in WASM aborts with no message unless a hook is installed;
    // without this the commonest failure mode is a blank page and an unhelpful
    // "unreachable executed" in the console.
    std::panic::set_hook(Box::new(|info| {
        web_sys::console::error_1(&format!("panic: {info}").into());
    }));

    app::clear_boot_screen();
    yew::Renderer::<app::App>::new().render();
}
