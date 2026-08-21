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

Byte-exact validation against the canonical vectors in
`app/core/tests/vectors/protocol-v1.json` — Keccak-256, EIP-191 digests
(UTF-8 byte lengths, not char counts), RFC 6979 deterministic low-S
signatures with `v = 27/28`, key → address derivation, and EIP-55 checksums
— plus FIPS/RFC self-tests for SHA-256 and HMAC-SHA256:

```sh
lua app/clients/lua/test.lua
```

Expected output: `55 passed, 0 failed`.
