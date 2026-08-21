# Changelog

Notable changes to Pocket Skynet, newest first. Versions are the workspace
version in [`app/Cargo.toml`](app/Cargo.toml); a `v*` tag matching it builds and
publishes the macOS installer.

## 1.0.2 — 2026-08-21

### m-of-n TSS (MPC) wallets

A wallet whose private key never exists in one piece. Key generation runs a
CGGMP21 distributed ceremony and hands you **n passphrase-sealed share files**;
any **m** of them sign, and losing up to n−m of them loses nothing. The server
persists no share: a finished ceremony waits in memory for exactly one `collect`
call gated by a random capability, and every later signature presents a quorum of
files in the request body.

Each share also carries the account's sealed E2EE identity, so any quorum
recovers messaging as well as the wallet — the encryption keypair is generated
from the CSPRNG and bound once by a ceremony signature, never derived from a
signature, because threshold ECDSA's jointly-random nonce would yield a different
key on every login.

Built on the independently audited
[`cggmp21`](https://github.com/LFDT-Lockness/cggmp21) implementation (Linux
Foundation Decentralized Trust) of the peer-reviewed
[CGGMP21](https://eprint.iacr.org/2021/060) protocol. `PROTOCOL.md` §20 and
`core/tests/vectors/tss-v1.json` record a real 2-of-3 ceremony signed by a
strict subset of parties, verified byte-exactly by MPC-blind code. The share
seal is PBKDF2-600k → AES-256-CBC → HMAC, encrypt-then-MAC, one KDF salt per
wallet and a fresh IV per file.

Ceremonies run natively in the server: `cggmp21` depends on C GMP and does not
target `wasm32-unknown-unknown`, so the browser drives `/api/tss/*` and never
holds a raw share. Mnemonic wallets are untouched.

### Web local mode, and a custom server address

The login screen gains a three-way connection picker: **this server** (the origin
that served the bundle, unchanged default), **custom server** (any PocketSkynet
server by IP, port or URL — admitted through the existing `PS_CORS_ORIGIN`
allowlist, with no server-side change), and **local**, where the client answers
its own API calls and nothing leaves the browser.

Local mode makes `web/dist` a complete deployment on its own: host it as static
files and every visitor gets a working app — My Note, My Jarvis, Knowledge,
Skynet Password, the wallet — with their data in their own IndexedDB, encrypted
at rest. Messages ride the existing E2EE room-key path; knowledge and generated
media are sealed under a wallet-derived subkey. Sign-out keeps the database;
Settings → Erase local data deletes it. On a static host the picker auto-suggests
local.

Two connectivity bugs fell out of the end-to-end work: a static host answering
`/api/health` with the SPA fallback page no longer reads as a live server (a 2xx
must be JSON), and the probe's transition detector now seeds from the store, so a
pre-sign-in "offline" verdict is corrected after sign-in.

### The packaged desktop app serves https

The embedded server in the packaged app runs with its self-signed certificate, so
the shareable URL in the title bar is `https://…` — encrypted, and a secure
context for the client's crypto on phones. Because a webview hard-fails on a
self-signed certificate, the server gained an optional loopback HTTP port: the
same router on a 127.0.0.1-only ephemeral port, which is what the window loads,
so realtime state stays shared between the window and https clients.

`make package` now copies its artifacts to `./dist` at the repository root and
prints the absolute path when it finishes. The `.app` is copied with `ditto`, so
the code signature survives.

### Uploads in 500 KB pieces, and `.mov` as a video

The suggested chunk size drops from 8 MB to 500 KiB: on a lossy link a dropped
connection costs one chunk, so what matters is how little a failure throws away,
not how few requests a transfer makes. The 16 MB `MAX_CHUNK_BYTES` ceiling stays
— it is the hard limit against a client that ignores what it was told.

Avatar upload, AI-generated image and video hosting, and site publishing each
sent a whole 5–25 MB file as one body; all three now chunk through the same path
as everything else, and the dead single-shot functions are gone.

`.mov` was missing from all four allow-lists that decide whether something is a
video, so an iPhone screen recording or Camera clip rendered as a download-only
card with no thumbnail and no inline playback. It is a video now.

### Deployment

The web client deploys to Cloudflare Pages on a `v*` tag.

## 1.0.1 — 2026-08-09

Chunked uploads stop hanging: every request is bounded by a timeout, an append
probes the server so a lost response costs seconds rather than minutes, the chunk
size slow-starts at 2 MB and doubles per landed chunk, and a commit whose answer
never arrived is adopted instead of retried. The composer accepts up to 10 photos
or videos per message.

Also in this release: the three built-in rooms (My Note, My Jarvis, My Lobby)
with real E2E encryption and client-side search; Jarvis's twenty-one tools and a
vault it can use but never read; Skynet Password, a sealed key/value store only
its owner can open; the Skynet Dashboard; invite links, incoming webhooks, the
room photo gallery, presence, mentions, threads and DMs; 4 GB file transfers;
Bonjour advertisement and an HTTP/3 listener; and the installable PWA.

## 1.0.0 — 2026-07-31

First release: the messenger, wallet and AI assistant, as an axum + SQLite
server, a Yew/WebAssembly web client and a Tauri desktop app, over one shared
`core` crate. `PROTOCOL.md` documents the wire protocol with byte-exact test
vectors.
