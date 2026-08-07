# Integration tests: the Python supervisor

`make integrationtest` runs this suite (after the Rust one). The supervisor
boots a throwaway backend on its own ephemeral port with its own temp data
directory, drives every user-facing flow over real HTTP with real wallet
signatures, then stops the backend and deletes the directory — pass or fail.
A second backend then boots with `--tls --http3`: the server mints its
self-signed certificate on the fly, and the suite verifies against exactly
that CA (never an unverified handshake), checks the app over HTTPS, the
Alt-Svc/server-info HTTP/3 advertisement, the HTTP→HTTPS redirect listener,
and probes the QUIC UDP listener with a raw version-negotiation packet.

No third-party packages. Even the EIP-191 signing is stdlib
(`ethwallet.py` implements Keccak-256 and secp256k1 with RFC 6979 nonces),
so the suite runs anywhere `python3` does — CI included — with no pip step.

## Running

```sh
cd app
cargo build -p pocketskynet-server   # run.py uses target/debug/pocketskynet
python3 tests/integration/run.py     # everything
python3 tests/integration/run.py messages sync   # only flows matching a name
POCKETSKYNET_BIN=path/to/pocketskynet python3 tests/integration/run.py
```

On failure the server log tail is printed before the folder is removed.

## What it covers

Every flow the app has, blockchain excluded: health/info, challenge →
EIP-191 → JWT auth (including burned challenges and forged signatures),
profiles and user search, rooms and their admin bounds, invitations,
kick/leave, DMs and group DMs, messages with threads/edits/deletes,
emoticon reactions, mentions and read pointers, presence, blocking,
hidden rooms, `/sync` draining with `X-Has-More`, search and knowledge
notes, whole-body and chunked-resumable uploads, downloads with Range and
capability URLs, hosted images, E2EE key publishing and room-key rotation,
SSE realtime delivery, server administration (suspension included), and
room destruction purging its bytes.

Deliberately absent: shout, sites, and message publishing — they need a
payment wallet and an on-chain transaction. The Rust suite's `paid.rs`
covers what can be covered without a chain.

## Layout

| file | role |
|---|---|
| `run.py` | entry point: supervise, run flows, report, tear down |
| `supervisor.py` | boots/stops the backend, hermetic env, folder cleanup |
| `flows.py` | the flows, each a function over the shared `Context` |
| `client.py` | stdlib HTTP client (proxies disabled) |
| `ethwallet.py` | pure-Python Keccak-256 + secp256k1 wallet |

Flows share logged-in users and one fixed-shape room (alice its only
admin, bob a member); anything that mutates roles or membership builds its
own room, which is what keeps a filtered run equivalent to a full one.
