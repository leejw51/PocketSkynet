# pocketskynet-client-ts

TypeScript client library and CLI (`pskynet-ts`) for the PocketSkynet server.
Implements the wire protocol from [`PROTOCOL.md`](../../../PROTOCOL.md) and
[`app/docs/API.md`](../../docs/API.md): EIP-191 challenge login, rooms and
plaintext messages, over HTTP/1.1(+TLS) or HTTP/3. E2EE is out of scope.

Requires Node 22+ (developed and tested on Node 24). ESM, strict TypeScript,
built with `tsc`, no framework dependencies — just `@noble/*`, `@scure/*`
(crypto) and `undici` (fetch with per-request TLS control).

## Install / build / test

```sh
cd app/cli/typescript
npm install
npm run build          # tsc -> dist/
npm run typecheck      # tsc --noEmit
npm test               # build + unit + integration tests
npm run test:unit          # vectors-only, no server needed
npm run test:integration   # boots real pocketskynet processes
```

The integration harness (`test/helpers/harness.ts`, modeled on
`app/server/tests/common/harness.rs`) boots one real `pocketskynet` per test
file on an ephemeral port with a temp data dir, scrubs `PS_*`/`VITE_*` from the
child environment, sets `PS_IGNORE_BAKED_ENV=1`, health-polls with a
bind-race check ("is our child alive"), and tears everything down even on
failure. It finds the server binary under `app/target/{release,debug}/` —
resolving through git worktrees and absorbed submodules — builds it once with
cargo when absent, or honors `POCKETSKYNET_BIN`.

## CLI usage

The key comes from `POCKETSKYNET_KEY` — prefer it over `--key`, which is
visible to other local users via `ps`/`/proc` while the process runs:

```sh
export POCKETSKYNET_KEY=0x<privkey>
export POCKETSKYNET_SERVER=http://127.0.0.1:9099

pskynet-ts health
pskynet-ts login --username alice
pskynet-ts rooms
pskynet-ts create-room "Team chat"
pskynet-ts send <roomId> "hello world"
pskynet-ts messages <roomId> --limit 20

# --key / --server also work as flags (the key is then visible in `ps`):
pskynet-ts rooms --server http://127.0.0.1:9099 --key 0x<privkey>

# Reuse a JWT without re-signing:
pskynet-ts rooms --token <jwt>        # or POCKETSKYNET_TOKEN

# HTTPS with the server's self-signed dev cert:
pskynet-ts health --server https://127.0.0.1:9099 --ca /path/to/ca.crt   # preferred: pin the CA
pskynet-ts health --server https://127.0.0.1:9099 --insecure             # dev-only fallback

# HTTP/3 (QUIC — the server's --http3-port listener; TLS is mandatory):
pskynet-ts health --server https://127.0.0.1:9101 --http3 --ca /path/to/ca.crt
```

On the `--http3` path the bearer token and request body are passed to curl via
a `--config -` file on stdin, so they never appear in `ps` — only `--key`
itself (and `--token`, if you use it) is visible there.

Exit codes: `0` success, `1` API/transport failure, `2` usage error.
`--json` prints raw server JSON. First-time logins need a username; when
`--username` is omitted the client retries once with a generated one (a failed
login burns its challenge, so the retry fetches a fresh challenge).

## Library usage

```ts
import { PocketSkynetClient } from "pocketskynet-client-ts";

const client = new PocketSkynetClient({
  baseUrl: "http://127.0.0.1:9099",
  privateKey: process.env.POCKETSKYNET_KEY!,
});
await client.login();                       // challenge -> EIP-191 sign -> JWT
const rooms = await client.rooms();         // includes My Note / My Jarvis / My Lobby
const room = await client.createRoom("Team chat");
await client.sendMessage(room.id, "hello"); // msgHash = sha256(trimmed content)
const msgs = await client.messages(room.id, { limit: 50 });
await client.close();
```

Lower-level pieces are exported too: `eip191Digest` / `personalSign` /
`recoverAddress`, `accountFromMnemonic` (BIP-39/44, `m/44'/60'/0'/0/i`),
`toChecksumAddress` (EIP-55), `msgHashPlaintext`, `createTransport`, and
`ApiError` carrying all three server error-envelope shapes (`message`,
`errors[]`, `code` + `currentKeyVersion`).

## Transports

Both transports serve the same `Transport` interface (`src/transport.ts`).

**HTTP/1.1(+TLS)** — undici `fetch`. TLS options are per-client, not global:
`caPath`/`caPem` pins exactly the given CA (what the integration tests do with
the server-generated CA), `insecure: true` disables verification for
self-signed dev certs. Plain `fetch` cannot do either per-request, which is
why undici's `Agent` (`connect: { rejectUnauthorized, ca }`) is used.

**HTTP/3 — which path shipped:** Node 24 has no HTTP/3 client: `node:quic`
is not exposed (not even under `--experimental-quic`, which only turns on the
flag), and npm has no maintained pure-JS HTTP/3 client. So `--http3` shells
out to an **HTTP/3-capable curl** via `child_process.execFile` — an argv
array, never a shell, so hostile message content is inert (unit tests pin
this, plus header-injection rejection for tokens). Secrets stay off argv: the
bearer token and request body go to curl through a `--config -` file on stdin,
so they are not exposed via `ps`/`/proc` to other local users. The binary is
probed at first use: `PSKYNET_CURL`, then Homebrew curl
(`/opt/homebrew/opt/curl/bin/curl`, `/usr/local/opt/curl/bin/curl`), then
`curl` on PATH — accepting only builds whose `curl --version` features
advertise `HTTP3`. Without one, the transport fails fast with a clear error
and the HTTP/3 integration test group SKIPs with the reason. When an
HTTP/3-capable curl is present (e.g. a Homebrew curl built with
ngtcp2/nghttp3), the HTTP/3 integration tests run end to end — QUIC health,
full login flow, and a cross-check that a room created over HTTP/3 is visible
over TCP.

## Test coverage

Unit (`test/unit`, no server): every `eip191[]` vector byte-exact (digest,
signature, recovered address), UTF-8 byte-length in the EIP-191 prefix, low-S
and `v ∈ {27,28}` enforcement with high-S/malformed rejection,
`wallet.privateKeyImports` + `wallet.bip39Seeds` + `wallet.accounts` (full
mnemonic derivation via `@scure/bip39`/`@scure/bip32`) + `wallet.eip55`,
malformed-key rejection (zero, ≥ n, wrong length, non-hex), `msgHash`
plaintext/encrypted vectors incl. trimming and unicode, camelCase body
building (`username` omitted when `undefined`, never `null`), tolerant
response parsing (nulls, unknown fields, absent enrichments), all three error
envelopes, transport selection, curl argv shape and hostile-input inertness,
CLI flag parsing.

Integration (`test/integration`, real servers): login happy path, first-time
username retry, stored-username reuse, wrong signature → 401, challenge replay
after success *and* failure → 400, minted/tampered/expired/wrong-secret JWTs
(HS256 with the harness `--jwt-secret`), built-in room membership (asserted by
name, never by count), room create/list/enriched-get, validation failures,
foreign **and nonexistent** rooms → 403, malformed room id → 400, message
send/list ordering `(messageTimestamp, msgSerial)`, limit, unicode, msgHash
format validation, 5000-char boundary (5000 ok / 5001 → 400), >100KB body →
413, parallel sends with distinct `msgSerial`s, HTTPS via pinned CA and via
`--insecure` plus refusal without either, HTTP/3 end-to-end, and CLI exit
codes 0/1/2 including hostile-content round-trips.
