# PocketSkynet Lua client

A dependency-free Lua library + CLI for the PocketSkynet server: wallet
login (EIP-191 challenge/response), rooms, and plaintext messages, over
HTTP/1.1 or HTTP/3.

## Requirements

- **Lua 5.4 or newer** (developed and tested on Lua 5.5.0 from Homebrew).
  The crypto relies on Lua's native 64-bit integers and bitwise operators,
  so **LuaJIT is not supported** (it lacks 5.3+ integer semantics).
- A **`curl` binary** on `PATH` — the only external dependency.
- **No luarocks packages.** JSON, SHA-256/HMAC, Keccak-256, and secp256k1
  ECDSA (RFC 6979) are bundled as pure Lua under `pocketskynet/`.

## Design choices (what is actually active)

**Transport — the `curl` binary, one code path for both protocols.**
The preferred routes from the porting notes (lua-curl bindings, or a native
Lua HTTP library for HTTP/1.1) both require luarocks, which is not present
on the target machine, and the system libcurl on macOS (SecureTransport
build, curl 8.7.1) has no HTTP/3 anyway. So `pocketskynet/transport.lua`
shells out to `curl`: the same invocation serves HTTP/1.1 and HTTPS, and
`--http3` simply appends curl's `--http3` flag. Request bodies travel
through a temp file (`--data-binary @file`), never through shell
interpolation.

For HTTP/3 you need an HTTP/3-capable curl (check `curl --version` for
`HTTP3` in Features). On macOS:

```sh
brew install curl        # Homebrew curl ships with HTTP3
./pocketskynet.lua --curl /opt/homebrew/opt/curl/bin/curl --http3 ...
# or: export POCKETSKYNET_CURL=/opt/homebrew/opt/curl/bin/curl
```

With a non-HTTP/3 curl, `--http3` fails fast with curl's own message
("the installed libcurl version doesn't support this").

**Signing — bundled pure-Lua secp256k1 + RFC 6979** (option (b) from the
porting notes, again because luarocks/luaossl are unavailable). 256-bit
arithmetic on 16-bit limbs, Jacobian point math, Fermat inverses, HMAC-SHA256
DRBG nonces, low-S normalization, `v = 27/28`. Correctness over speed — a
signature takes ~0.1 s — and validated **byte-exactly** against the canonical
protocol vectors (see Tests). Keccak-256 (original padding `0x01`, not
SHA3-256) is likewise pure Lua.

**JSON — bundled minimal encoder/decoder** (`pocketskynet/json.lua`),
including `\uXXXX` surrogate-pair decoding to UTF-8 (the vectors and chat
messages contain emoji). Use `dkjson`/`lua-cjson` instead if you prefer;
the module boundary is one `require`.

## Usage

```sh
export POCKETSKYNET_KEY=0xac09...ff80      # wallet private key (or --key)

# HTTP/1.1 against a dev server
./pocketskynet.lua --server http://127.0.0.1:9099 health
./pocketskynet.lua --server http://127.0.0.1:9099 --username alice login
./pocketskynet.lua --server http://127.0.0.1:9099 rooms
./pocketskynet.lua --server http://127.0.0.1:9099 create-room "My room"
./pocketskynet.lua --server http://127.0.0.1:9099 send <roomId> "hello"
./pocketskynet.lua --server http://127.0.0.1:9099 messages <roomId>

# HTTPS with the server's self-signed dev certificate
./pocketskynet.lua --server https://127.0.0.1:9099 --insecure health

# HTTP/3 (QUIC) — server started with --http3; HTTP/3-capable curl required.
# The QUIC listener defaults to UDP port = main port + 2
# (GET /api/server/info reports the exact endpoints).
./pocketskynet.lua --server https://127.0.0.1:9101 --http3 --insecure health
```

Flags: `--server <url>`, `--http3`, `--insecure`, `--key <hex>`,
`--token <jwt>`, `--username <name>`, `--curl <path>`. Environment
equivalents: `POCKETSKYNET_SERVER`, `POCKETSKYNET_KEY`, `POCKETSKYNET_TOKEN`,
`POCKETSKYNET_CURL`.

`login` prints the JWT; export it as `POCKETSKYNET_TOKEN` to skip the
challenge/login round-trip on subsequent commands (each command otherwise
logs in transparently with the key). First-time logins need `--username`.

Messages are sent **plaintext** (`isEncrypted: false`, `msgHash` =
SHA-256 of the trimmed content); E2EE is out of scope for this client, and
encrypted messages render as a placeholder when listing.

## Library

```lua
local Client = require("pocketskynet.client")
local c = Client.new{ server = "http://127.0.0.1:9099", key = "0x...", username = "alice" }
c:login()                      -- challenge → EIP-191 sign (verbatim) → JWT
c:rooms()                      -- GET  /api/rooms
c:create_room("My room")       -- POST /api/rooms
c:send_message(roomId, "hi")   -- POST /api/rooms/:id/messages
c:messages(roomId)             -- GET  /api/rooms/:id/messages
```

Lower-level modules: `pocketskynet.eip191` (digest/sign/address/EIP-55),
`pocketskynet.secp256k1`, `pocketskynet.keccak`, `pocketskynet.sha2`,
`pocketskynet.json`, `pocketskynet.transport`.

## Tests

**Unit suites** (no server needed):

```sh
lua app/clients/lua/test.lua          # runs everything in test/unit_*.lua
lua app/clients/lua/test/unit.lua crypto json   # or a subset by name
```

Expected output: `unit: 123 passed, 0 failed`. Coverage:

- `test/unit_crypto.lua` — byte-exact validation against the canonical
  vectors in `app/core/tests/vectors/protocol-v1.json`: every `eip191[]`
  digest/signature/address, `wallet.privateKeyImports` / `wallet.accounts` /
  `wallet.eip55`, and every `msgHash` vector (plaintext trim rule, encrypted
  base64 rule, emoticon event strings). Plus FIPS/RFC 4231 SHA-256 and
  HMAC-SHA256 vectors (padding boundaries, >block-size keys), Keccak-256
  edges (empty, rate-boundary and single-byte-padding blocks, ≠ SHA3-256),
  and secp256k1 edges (key 0 / ≥ n rejected, n−1 accepted, RFC 6979
  determinism, low-S with v ∈ {27, 28} enforced across many digests).
- `test/unit_json.lua` — codec round-trips, surrogate-pair decoding, null
  sentinel vs omitted key, escapes, malformed-input rejection.
- `test/unit_client.lua` — wire shapes against a stub transport: camelCase
  bodies, username omitted vs present, verbatim challenge signing, Bearer
  headers, msgHash of the trimmed content, error-envelope surfacing.
- `test/unit_transport.lua` — the curl transport against a fake curl
  binary: flag mapping (`--http3` → curl `--http3`, `--insecure` → `-k`),
  and the shell-quoting security property — hostile message text, headers,
  and URLs (`"; rm -rf ~"`, `$(…)`, backticks, newlines) stay inert.
- `test/unit_cli.lua` — CLI exit codes and usage errors.

**Integration suite** (boots real servers on ephemeral ports; needs a
built binary at `app/target/{release,debug}/pocketskynet`, in the main
checkout when running from a git worktree, or via `$POCKETSKYNET_BIN`):

```sh
lua app/clients/lua/test/integration.lua
```

Expected output: `integration: 31 passed, 0 failed` (+ 1 skip on machines
without an HTTP/3-capable curl). Covers the login protocol (happy path,
first-login username, wrong-wallet signature → 401, challenge reuse → 400,
bad/missing JWT → 401), rooms (create/list/validation/auth), messages
(unicode round-trips, ordering, limit, non-member → 403, malformed
msgHash → 400), the CLI end to end with exit codes, HTTPS with
`--insecure`, and — when an HTTP/3 curl exists — the same flow over QUIC,
verified via `/api/server/info` reporting `protocol: "h3"`. The harness
(`test/harness.lua`) kills every server it spawned even when tests fail,
and the suite's last test asserts none is still alive.
