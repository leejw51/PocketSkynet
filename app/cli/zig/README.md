# pskynet-zig — PocketSkynet client in Zig

A Zig client for the PocketSkynet server: a library module (`pocketskynet`)
plus a CLI (`pskynet-zig`). Implements the blockchain login flow (EIP-191
`personal_sign` over secp256k1), rooms, plaintext messages and health checks,
over HTTP/1.1, HTTPS and HTTP/3. E2EE is out of scope.

**Zig version targeted: 0.16.0** (the `std.Io` era — `std.http.Client`,
`std.process.spawn` and file APIs all take an explicit `Io`). Older Zig
versions will not compile this package.

## Build, test, run

```sh
zig build              # builds zig-out/bin/pskynet-zig
zig build test         # unit tests, no server needed  (48 tests)
zig build itest        # integration tests: boots a real pocketskynet
                       # server per test                (14 tests)
zig build test-all     # both
zig fmt --check build.zig src tests
```

`zig build itest` finds the server binary under
`app/target/{release,debug}/pocketskynet` (walking up from the cwd, so it
also works when the checkout is the absorbed `app/` submodule), or
`POCKETSKYNET_SERVER_BIN`. If absent, it runs `cargo build --release` once.
Each test gets its own server on an ephemeral port with a temp data dir, a
scrubbed environment (`PS_IGNORE_BAKED_ENV=1`, no inherited `PS_*`/`VITE_*`),
`--no-rate-limit`, bind-race detection, and guaranteed teardown.

## CLI usage

```sh
pskynet-zig health --server http://127.0.0.1:9099
pskynet-zig login --server http://127.0.0.1:9099 --key 0x<privkey> [--username alice]
pskynet-zig rooms --key 0x<privkey>
pskynet-zig create-room "My Room" --key 0x<privkey>
pskynet-zig send <roomId> "hello" --key 0x<privkey>
pskynet-zig messages <roomId> --limit 20 --key 0x<privkey>
```

Flags: `--server`, `--key` (or `POCKETSKYNET_KEY`), `--username`, `--token`
(or `POCKETSKYNET_TOKEN`, skips the login round trip), `--http3`,
`--insecure`, `--cacert <pem>`, `--limit`. Exit codes: 0 success, 1
runtime/API failure, 2 usage error.

Prefer `POCKETSKYNET_KEY` (and `POCKETSKYNET_TOKEN`) over the `--key`/`--token`
flags: a value on the command line is visible to other local users via `ps`,
while an environment variable is not.

Login follows the spec exactly: `POST /api/auth/challenge`, sign the returned
message **verbatim**, `POST /api/auth/login` with the required `challengeId`.
A challenge is burned by failure as well as success, so the first-time-login
retry ("Username is required…") fetches a fresh challenge and supplies a
deterministic generated username (`zig` + 12 hex chars of
keccak256(address)) when `--username` was not given.

## Transports (what is actually active)

One request interface (`src/transport.zig`), two backends:

- **`std` backend — HTTP/1.1 and HTTPS.** `std.http.Client`. For HTTPS with
  the server's self-signed certificate, pass `--cacert <data-dir>/tls/ca.crt`:
  the PEM is loaded into the client's CA bundle (`client.ca_bundle` +
  `client.now`, which suppresses the system-bundle rescan). Note std's
  certificate verifier matches **DNS** SANs only, so a CA-pinned connection
  must use `https://localhost:<port>`, not `https://127.0.0.1:<port>`; both
  names are in the generated certificate.
- **`curl` backend — HTTP/3 and `--insecure`.** Zig has no mature HTTP/3
  stack and no HTTP/3-capable libcurl dylib was assumed, so `--http3` shells
  out to an HTTP/3-capable `curl` binary (`--http3-only`), probed from
  `POCKETSKYNET_CURL`, `/opt/homebrew/opt/curl/bin/curl`,
  `/usr/local/opt/curl/bin/curl`, then `curl` in PATH, requiring `HTTP3` in
  its feature list (Homebrew's keg-only `curl` ≥ 8.x provides it; macOS
  system curl does not). Without one, `--http3` fails fast with a clear
  message and the HTTP/3 integration test reports SKIP. `--insecure` HTTPS
  also routes through curl (`-k`), because std's TLS deliberately has no
  skip-verification switch.

  The curl subprocess is spawned **exec-style** (`std.process.run`, argv
  array, no shell anywhere); the JSON body and the bearer header each travel
  via a private `0600` temp file created with `O_EXCL` and an unpredictable
  name (`--data-binary @file`, `--header @file`), and the URL is passed with
  `--url`. So hostile message text can never be word-split, glob-expanded, or
  parsed as an option, and neither the request body (which carries the login
  signature) nor the JWT ever appears in argv (`ps`) or in a world-readable
  file. Unit tests pin the argv construction with `"; rm -rf ~`, `$(…)`,
  backticks and newline payloads, and the secure temp-file creation
  (mode/exclusivity/cleanup).

With an HTTP/3-capable curl (e.g. Homebrew curl ≥ 8.x, built with
ngtcp2/nghttp3), the HTTP/3 integration test runs **end-to-end over QUIC**
(health, login, create-room, send, list) against the server's UDP listener;
otherwise that one test reports SKIP.

The std backend also bounds each response at 64 MB and applies a connect
timeout, so a wedged or hostile server can neither exhaust memory nor hang
the CLI on connect (the curl backend already enforces `--max-time` and the
same 64 MB stdout cap).

## Signing (pure Zig)

`src/secp.zig` implements Ethereum-style recoverable ECDSA on top of
`std.crypto.ecc.Secp256k1` (the standard library's Jacobian group + mod-n
scalar arithmetic): RFC 6979 deterministic nonces (HMAC-SHA256, with the
step-h re-key on rejected candidates), low-S normalization, recovery id with
`v ∈ {27, 28}`, public-key recovery, address derivation
(keccak256(X‖Y)[12..], dropping the 0x04 SEC1 byte) and EIP-55 checksums.

`std.crypto.ecdsa` was evaluated and not used: it hashes the message itself
and exposes no recovery id, so it cannot produce the 65-byte `r‖s‖v` wire
form. `std.crypto.hash.sha3.Keccak256` **is** original Keccak (0x01 padding,
verified against the canonical Ethereum empty-string digest), not NIST
SHA3-256 (0x06) — exactly what EIP-191 needs.

Every signature path is pinned byte-exactly by the `eip191[]` vectors in
`app/core/tests/vectors/protocol-v1.json` (digest, 65-byte signature,
recovered address), plus `wallet.privateKeyImports`, `wallet.eip55`,
`msgHash.*` and the `ecdh` checkpoint.

**Scope note:** `wallet.accounts` derive from a BIP-39 mnemonic via BIP-32;
mnemonic import is not implemented (the client authenticates with a raw
private key), so those vectors are not exercised. `wallet.privateKeyImports`
is covered fully, including the reject-0 / reject-≥n / accept-n−1 edges.

## Layout

```
build.zig, build.zig.zon      module `pocketskynet`, exe `pskynet-zig`
src/hex.zig                   lowercase hex helpers
src/secp.zig                  secp256k1 sign/recover, addresses, EIP-55
src/eip191.zig                EIP-191 personal_sign (byte-length prefix)
src/msghash.zig               plaintext msgHash (Unicode trim + SHA-256)
src/transport.zig             std.http + curl backends, argv builder
src/api.zig                   typed client: login flow, rooms, messages
src/main.zig                  the CLI
tests/vectors.zig             protocol-v1.json vector suite + keccak edges
tests/harness.zig             real-server test harness
tests/integration.zig         14 end-to-end tests (HTTP/1.1, TLS, HTTP/3)
```
