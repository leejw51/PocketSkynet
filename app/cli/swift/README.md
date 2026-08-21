# PocketSkynet Swift client

A SwiftPM package for the PocketSkynet server: the `PocketSkynetClient`
library plus the `pskynet-swift` CLI. macOS 13+, Swift 5.9+.

## What's inside

- **`PocketSkynetClient`** — wallet login (EIP-191 `personal_sign` over the
  server's challenge, secp256k1 with RFC 6979 deterministic nonces, low-S,
  `v = recid + 27`), rooms, plaintext messages with `msgHash` (SHA-256 of the
  trimmed content), and typed decoding of the API's camelCase wire shapes and
  all three error-envelope shapes. E2EE is out of scope.
- **`pskynet-swift`** — `login`, `rooms`, `create-room`, `send`, `messages`,
  `health`, with `--server`, `--http3`, `--insecure`, `--key` /
  `POCKETSKYNET_KEY`, `--username`.

Signing stack: [GigaBitcoin `secp256k1.swift`](https://github.com/GigaBitcoin/secp256k1.swift)
(libsecp256k1 bindings — recoverable ECDSA, RFC 6979, low-S by construction)
plus a small bundled Keccak-256. Signatures reproduce the repo's
`protocol-v1.json` `eip191[]` vectors byte-exactly.

## Build

```sh
cd app/cli/swift
swift build            # library + CLI → .build/debug/pskynet-swift
swift build -c release # optimized
```

If `swift test` complains `no such module 'XCTest'`, your active developer
directory is the Command Line Tools (which do not ship XCTest). Use Xcode's:

```sh
DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer swift test
```

## Usage

Plain HTTP / HTTPS (TCP listener):

```sh
pskynet-swift health   --server http://127.0.0.1:9099
pskynet-swift login    --server http://127.0.0.1:9099 --key 0x<hex32> [--username alice]
pskynet-swift rooms    --server http://127.0.0.1:9099 --key 0x<hex32>
pskynet-swift create-room "Team chat" --server … --key …
pskynet-swift send <roomId> "hello"   --server … --key …
pskynet-swift messages <roomId> --limit 20 --server … --key …
```

The key can also come from the environment: `export POCKETSKYNET_KEY=0x…`.
Prefer the environment variable over `--key`, which is visible to anyone who
can run `ps` while the command is in flight. Against a server with a
self-signed certificate (`--tls` dev servers), add
`--insecure`; without it the certificate is refused, deliberately.

First-time logins need a username. Pass `--username`, or the client retries
once with a generated `swift_<address-prefix>` name (the server burns a
challenge on **every** failed attempt, so the retry fetches a fresh one).

### HTTP/3

```sh
pskynet-swift health --server https://127.0.0.1:9101 --http3 --insecure
```

Point `--server` at the server's QUIC endpoint (an `https` URL naming the
`--http3-port`; the server prints it at startup and reports it in
`GET /api/server/info` as `http3Port`). `--insecure` is needed for the
self-signed certificate dev servers generate.

**Which h3 path is active: native URLSession.** On macOS, URLSession is
backed by Network.framework, which negotiates QUIC itself when a request is
marked `assumesHTTP3Capable` — no curl fallback, no external processes. This
is verified honestly, not assumed: `GET /api/server/info` reports the
protocol that carried the request, and the integration suite asserts it says
`"h3"` on the QUIC port (and *not* `"h3"` on the TCP port, as the control).
Verified on macOS 26; on an older macOS whose Network.framework cannot reach
the QUIC listener, the HTTP/3 integration tests skip with a clear message
rather than passing vacuously.

Both transports go through the same `Transport` abstraction
(`URLSessionTransport(insecure:http3:)`).

## Tests

```sh
DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer swift test
```

- **Unit** (`PocketSkynetClientTests`, 39 tests, no server): every `eip191[]`
  vector (digest, byte-exact signature, address recovery), key→address for
  `wallet.privateKeyImports` and `wallet.accounts`, `msgHash` vectors
  (trimming, unicode), EIP-191 UTF-8 byte-length semantics, low-S and
  v ∈ {27, 28} enforcement, malformed-key rejection, camelCase request bodies
  (optionals omitted, never null), tolerant response decoding, all three
  error-envelope shapes, hex-decode sign/space rejection, base-URL
  trailing-slash normalization, and URL path-segment encoding (a caller
  cannot retarget a request through a crafted roomId).
- **Integration** (`IntegrationTests`, 36 tests): each suite boots a real
  `pocketskynet` process (modeled on `app/server/tests/common/harness.rs` —
  ephemeral TCP/UDP ports, serialized boots with bind-race detection, temp
  data dirs, guaranteed teardown). Covers the login flow incl. the
  first-time-username retry, wrong signature → 401, challenge replay and
  burn-on-failure → 400, tampered/absent JWT → 401, built-in room
  provisioning (asserted by membership, never by count), room create/list/
  invalid-name, foreign **and** nonexistent rooms → 403 (no existence
  oracle), message send/list/limit/trim/unicode/ordering, `msgHash`
  format validation, 100KB → 413, HTTPS with and without `--insecure`,
  the full flow over HTTP/3 with server-side protocol confirmation, CLI
  exit codes (0 / 1 / 64), and parallel sends receiving distinct
  `msgSerial`s.

The harness finds the server binary by walking up the tree for
`app/target/{release,debug}/pocketskynet` (newest build wins; worktree and
absorbed-submodule layouts both resolve), or honors `POCKETSKYNET_BIN`. If
none exists it attempts one `cargo build --release`; build the server first
for a faster start:

```sh
cd app && cargo build --release --bin pocketskynet
```
