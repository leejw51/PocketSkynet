# pocketskynet-client

Rust client (library + CLI) for the PocketSkynet server, speaking the same API
over either of the server's two listeners:

- **HTTP/1.1(+TLS)** via `reqwest`
- **HTTP/3 over QUIC** via `quinn` 0.11 + `h3` + `h3-quinn` — the exact stack
  the server's own listener uses (`app/server/src/http3.rs`), ALPN `h3`

All signing goes through `pocketskynet-core`: the login challenge returned by
`POST /api/auth/challenge` is signed **verbatim** with EIP-191 `personal_sign`
(RFC 6979 deterministic nonces, low-S, `v ∈ {27, 28}`), and the resulting JWT
is sent as `Authorization: Bearer <jwt>` on every authenticated call.

## Build

The crate is a member of the `app/` workspace:

```sh
cd app
cargo build -p pocketskynet-client
cargo test  -p pocketskynet-client   # EIP-191 vector + serialization tests
```

The binary lands at `target/debug/pocketskynet-client`.

## Usage

A secp256k1 private key is required for everything except `health` — pass it
with `--key 0x…` or the `POCKETSKYNET_KEY` environment variable. **Prefer
`POCKETSKYNET_KEY`**: a `--key` argument is visible to anyone who can run `ps`
and lingers in your shell history, while the environment variable is not passed
around on the command line.

On a wallet's first login the server requires a username; pass `--username`, or
let the client fall back to the protocol's deterministic username for the
address.

For scripted use, log in once and reuse the JWT with `--token <jwt>` or
`POCKETSKYNET_TOKEN` instead of re-signing on every command — the production
server caps logins at 5 requests/minute/IP, so a script that logs in per
command trips a `429`.

Global flags: `--server <url>`, `--http3`, `--http3-port <port>`, `--insecure`,
`--key <hex>`, `--token <jwt>`, `--username <name>`. Every request carries a
30-second timeout (10 seconds to connect) so a server that accepts the
connection but never answers fails rather than hanging.

### HTTP/1.1

Against a plain-HTTP dev server (`make http`, default port 9099):

```sh
export POCKETSKYNET_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80

pocketskynet-client --server http://127.0.0.1:9099 health
pocketskynet-client --server http://127.0.0.1:9099 login
pocketskynet-client --server http://127.0.0.1:9099 create-room lounge
pocketskynet-client --server http://127.0.0.1:9099 rooms
pocketskynet-client --server http://127.0.0.1:9099 send <roomId> "hello over TCP"
pocketskynet-client --server http://127.0.0.1:9099 messages <roomId> --limit 20
```

Scripting many commands? Log in once, capture the JWT, and reuse it so you
sign in only that one time:

```sh
export POCKETSKYNET_TOKEN=$(pocketskynet-client --server http://127.0.0.1:9099 login | awk '/^jwt:/{print $2}')
pocketskynet-client --server http://127.0.0.1:9099 rooms          # no re-login
pocketskynet-client --server http://127.0.0.1:9099 send <roomId> "hi"
```

Against HTTPS (`make start` / `make https`) the certificate is self-signed
into the server's data dir, so add `--insecure`:

```sh
pocketskynet-client --server https://127.0.0.1:9099 --insecure rooms
```

### HTTP/3

Add `--http3`. QUIC mandates TLS, so the scheme is treated as `https`
regardless, and against the self-signed dev certificate you need `--insecure`.
`make start` serves QUIC on the **same port number** as HTTPS (UDP instead of
TCP), so the URL's port is used as the QUIC port by default; a deployment that
runs QUIC elsewhere (the bare server binary defaults to TCP port + 2) is
reached with `--http3-port`:

```sh
pocketskynet-client --server https://127.0.0.1:9099 --http3 --insecure health
pocketskynet-client --server https://127.0.0.1:9099 --http3 --insecure login
pocketskynet-client --server https://127.0.0.1:9099 --http3 --insecure send <roomId> "hello over QUIC"

# QUIC on a different UDP port than the URL's TCP port:
pocketskynet-client --server https://127.0.0.1:9099 --http3 --http3-port 9101 --insecure rooms
```

`--insecure` skips certificate verification only; use it exclusively against
your own development server.

## Library

```rust
use pocketskynet_client::{Client, Transport, TransportOptions, Wallet};

# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
// Timeouts default to 30s per request / 10s connect; override as needed.
let options = TransportOptions {
    insecure: true,
    ..TransportOptions::default()
};

// Pick a transport once; the API layer is identical for both.
let transport = Transport::http1("https://127.0.0.1:9099", &options)?;
// or: Transport::http3("https://127.0.0.1:9099", &options, None).await?;

let wallet = Wallet::from_private_key_hex("0xac09…ff80")?;
let mut client = Client::new(transport);
let session = client.login(&wallet, None).await?; // challenge → sign → JWT

let room = client.create_room("lounge", None).await?;
client.send_message(&room.id, "hello").await?;
for m in client.messages(&room.id, 50).await? {
    println!("{}: {}", m.sender_address, m.content);
}
# Ok(()) }
```

Plaintext messages carry the required `msgHash` (lowercase-hex SHA-256 of the
trimmed content, computed via `pocketskynet_core::msg_hash_plaintext`). E2EE
is out of scope for this client — encrypted rooms' messages come back as
ciphertext and are shown as `(encrypted)` by the CLI.

## Tests

`cargo test -p pocketskynet-client` runs three suites:

- **In-crate unit tests** — wire-type deserialization (nulls, omitted fields,
  unknown-field tolerance), error-envelope shapes, `h3_target` port
  resolution, `TransportOptions` defaults, and room-id path-injection
  rejection.
- **`tests/vectors.rs`** — replays every `eip191[]` and `wallet` entry of the
  canonical vector file `app/core/tests/vectors/protocol-v1.json` through the
  same signing path `Client::login` uses (key import → `personal_sign` →
  recovery), plus `msgHash` and camelCase serialization pinning.
- **`tests/live/`** — a ~25-test integration suite. **It builds the server**
  (`cargo build -p pocketskynet-server`, unless a binary is already present or
  `POCKETSKYNET_SERVER_BIN` points at one) **and boots roughly 25 real
  `pocketskynet` processes**, one per test, each on an ephemeral port with a
  temporary data directory torn down in `Drop`. Set
  `CARGO_TARGET_DIR=…/app/target` to reuse the workspace's dependency cache.
