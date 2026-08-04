# PocketSkynet

A standalone, blockchain-authenticated messenger — **Rust server, Rust web client**.

No passwords and no accounts: you log in by signing a challenge with an Ethereum
wallet. Rooms can be end-to-end encrypted, and the server never sees plaintext
messages or symmetric keys.

Three things in one shell — a small-team Slack alternative you run yourself:

- **Messenger** — rooms, invitations, reactions, E2EE, realtime over
  WebSocket/SSE, all on one SQLite file.
- **Wallet** — the top-bar wallet sends native CRO/TCRO and USDC on Cronos
  (mainnet 25 / testnet 338), with an active-network switcher designed to grow
  to other chain families (Solana and Cardano already sit in the registry,
  switchable, send pending). Transactions are signed in the browser
  (EIP-155 + RLP in `core/`); the server never sees a key.
- **AI assistant** — Grok, OpenAI, Anthropic and Gemini behind one dialog in
  the composer: draft, reply-in-context, and image generation. Bring your own
  keys; they live in your browser's localStorage, never on the server.

Two **paid features** fund the deployment — each requires an on-chain payment
to the operator's wallet (`VITE_FRUITNATION_WALLET`), verified by the server
over the configured chain's RPC, one transaction hash per action (which also
makes them useless as spam or DoS levers — flooding costs real money):

- **Shout** (📣 in the top bar, ≥ 10 CRO, `PS_SHOUT_PRICE_CRO`) — your line
  lands on every connected screen as an animated banner for up to a minute;
  each viewer can dismiss it.
- **Web publishing** (🌐 `/publish`, 1 CRO, `PS_PUBLISH_PRICE_CRO`) — the
  server hosts your page at `/sites/{id}/`: pasted HTML, an `.html` file, or
  a zip with `index.html` + assets. Recorded in SQLite, searchable like
  knowledge, and removable by *any* signed-in user. Served under a strict
  `CSP: sandbox` so a published page can never touch the app's origin.

PocketSkynet is a self-contained reimplementation of the FruitNation messenger
protocol (see `../server/PROTOCOL.md`). It shares the wire protocol and the
canonical crypto vectors, but none of the code: the backend is axum + SQLite
instead of Express + PostgreSQL, and the frontend is Yew compiled to WebAssembly
instead of React.

## Demo

[![Watch the PocketSkynet demo on YouTube](https://img.youtube.com/vi/jraQ8KFgQ74/sddefault.jpg)](https://youtu.be/jraQ8KFgQ74)

▶ [Watch on YouTube](https://youtu.be/jraQ8KFgQ74)

## Quick start

```bash
make          # show every target
make build    # release build: server + WASM client
make start    # run the server from the terminal
make https    # the same, over TLS — for joining from a phone or tablet
make gui      # run the desktop app
```

Either way it prints the addresses it is reachable on:

```
  PocketSkynet is running.

    local     http://127.0.0.1:9099
    network   http://192.168.1.24:9099

  Anyone who can reach a network address above can open it and sign in
  with their own wallet. Bind to 127.0.0.1 to keep it to this machine.
```

Anyone on the same network can open a network URL and log in with their own
wallet. Nothing is installed on their side — the client is a web page.

**Both the CLI and the desktop app bind `0.0.0.0` by default**, so the app on
your laptop is a server other people can join. Set `PS_HOST=127.0.0.1` (or
`make start HOST=127.0.0.1`) to keep it to one machine.

## Joining from a phone or a tablet

Use `make https`. A browser gives a plain-HTTP page on a LAN address the
reduced platform — no `crypto.subtle`, no clipboard, no notifications — and a
permanent "Not Secure" badge beside a page that asks you to sign with a wallet
key. Over TLS all of it comes back.

The certificate is generated on first run into `~/.pocketskynet/tls/` and reused after
that. The banner then prints the address to hand over, as text to copy and as a
QR code to point a camera at:

```
  PocketSkynet is running.

    local     https://127.0.0.1:9099
    network   https://192.168.1.24:9099
    vpn       https://100.120.4.113:9099

  Open on a phone or tablet — copy this, or scan below:

      https://100.120.4.113:9099

      ▄▄▄▄▄▄▄  ▄ ▄▄  ▄▄▄▄▄▄▄
      █ ▄▄▄ █ ▀▄█▀▄▀ █ ▄▄▄ █    (a scannable QR code)
      █▄▄▄▄▄█ ▀▄▀ ▄▀ █▄▄▄▄▄█
```

`vpn` marks the `100.64.0.0/10` range a mesh VPN like Tailscale hands out — the
address that still works when the tablet is not on your network. That is the
one the QR code carries when there is one.

Because the certificate is self-signed, the browser objects the first time:
tap **Show Details → visit this website**. To be rid of the warning
permanently, install the CA on the device — the banner prints the URL:

1. open `http://<address>:9100/ca.crt` on the device,
2. iOS: **Settings → General → VPN & Device Management** → install the profile,
3. iOS: **Settings → General → About → Certificate Trust Settings** → switch it on.

Port 9100 (the HTTPS port plus one) is a plain-HTTP listener that serves that
one file and redirects everything else to HTTPS, so a typed `host:9100` still
lands on the app. `--http-redirect-port 0` turns it off.

Moving to a different network reissues the certificate, naming the addresses
the machine then holds — but **not** the CA, so a device that has trusted it
once stays trusted. To use a real certificate instead, pass `--tls-cert` and
`--tls-key`.

`make gui` stays on plain HTTP: its window loads the server over loopback, and
a webview meets a self-signed certificate with a hard failure and no way to
accept it.

## Installing it as an app

The client is a progressive web app, so a phone can keep it on the home screen
and open it in its own window with no browser chrome — and open it *fast*,
because the shell is already on the device:

- **iOS/iPadOS** — Safari, **Share → Add to Home Screen**. It has to be Safari;
  no other iOS browser can install.
- **Android** — Chrome offers **Install app** in the menu, or a prompt.
- **Desktop** — Chrome and Edge show an install button in the address bar.

This needs a **secure context**, which means `make https` (or a real
certificate). On a plain-HTTP LAN address the browser does not define
`navigator.serviceWorker` at all, so there is nothing to install — the same
reason the section above exists. Loopback counts as secure, so a desktop
install from `http://127.0.0.1:9099` works.

What is cached is the shell and only the shell: `index.html`, the WASM bundle,
the stylesheet, the fonts and the imagery it has used. **No message, no room
key and no API response is ever written to Cache Storage** — the same rule the
client already follows in memory (see *Storage*). Launched with the server
unreachable, the app paints and routes; it just has nothing to show in it.

Redeploys are handled without a cache to bust:

- the document is fetched **network-first**, so a rebuilt client is picked up on
  the next launch rather than the one after it;
- content-hashed bundles are served from the cache with no request at all,
  because a changed file is a changed URL;
- when the shell does change, the hashed files it no longer names are deleted —
  otherwise every redeploy would leave another 2.5 MB WASM bundle behind
  forever.

The installed tile is the **native iOS app's icon** — the same 1024px artwork in
`ios/App/Resources/Assets.xcassets/AppIcon.appiconset/appicon.png` — so an
installed web app and the App Store one are the same thing on the home screen.
It is checked in at the sizes each platform asks for, regenerated with:

```bash
SRC=../../ios/App/Resources/Assets.xcassets/AppIcon.appiconset/appicon.png
cd web/static/img
# iOS home screen, and the manifest's "any" icons — the art untouched, because
# it is drawn for a rounded-rect crop and already carries its own margin.
for s in 180 192 512; do
  magick "$SRC" -resize ${s}x${s} -strip -define png:compression-level=9 icon-$s.png
done
# The manifest's "maskable" icons. Android crops to a circle inside the middle
# 80%, which cuts the orbital rings clean off the full-bleed art, so these sit
# the icon at 86% on its own background gradient. The added margin is invisible
# — the gradient is sampled from the artwork's own corners.
for s in 192 512; do
  magick -size ${s}x${s} gradient:'srgb(5,60,72)-srgb(2,15,18)' \
    \( "$SRC" -resize $((s * 86 / 100))x$((s * 86 / 100)) \) \
    -gravity center -composite -depth 8 -strip \
    -define png:compression-level=9 icon-maskable-$s.png
done
```

The worker is `web/pwa/sw.js`, copied to `/sw.js` because a worker can only
control paths below its own URL. The desktop app deliberately does not register
one: its window is a secure context on loopback and would happily install a
worker, and a shell cached inside the app would then outlive an app update.

## HTTP/3

`--http3` adds a QUIC listener beside the TCP one. Both run at once and serve
the same API, so a client can pick either:

```sh
pocketskynet --tls --http3              # https on 9099/tcp, http/3 on 9101/udp
pocketskynet --http3 --http3-port 4433  # plain http on 9099/tcp, http/3 on 4433/udp
```

The default is the main port plus two, leaving plus-one to the HTTPS redirect.
Setting it *equal* to the HTTPS port is valid and conventional — TCP and UDP
port numbers are separate namespaces, and a browser expects `Alt-Svc` to point
at the number it already knows.

Clients are told about it through an `Alt-Svc: h3=":<port>"` header on the TCP
listener. That is the only discovery mechanism there is: QUIC on a closed UDP
port is silence, not a refusal, so nothing can find HTTP/3 by probing for it.

**What it buys.** No transport head-of-line blocking (one lost packet stalls
one request instead of every request sharing the TCP connection), one round
trip to first byte instead of three, and connection migration — a phone moving
Wi-Fi→cellular keeps the same connection. All three matter on a bad or mobile
network.

**What it does not.** On loopback or a clean LAN, expect HTTP/2 to match or
beat it: TCP is offloaded to the kernel and often the NIC, while QUIC does
congestion control and packet assembly in userspace. Both listeners run
together precisely so this can be measured rather than assumed.

**Two limits worth knowing.** QUIC has no plaintext mode, so `--http3` without
`--tls` still generates a certificate for the UDP listener alone — download it
from `/ca.crt`. And there is no WebSocket over HTTP/3: RFC 9220 exists but
nothing implements it, so the QUIC listener answers an upgrade attempt with a
plain 501 rather than hanging, and realtime stays on the TCP listener. SSE is
an ordinary streaming body and works over both.

0-RTT is deliberately off. It is the headline latency number in every HTTP/3
benchmark, and its data is replayable by design — a bad trade on a server whose
POSTs move CRO.

## Two ways to run it

|  | `make start` | `make gui` |
|---|---|---|
| What it is | the server, in a terminal | a desktop window |
| The server | this process | embedded in the app |
| Data | `~/.pocketskynet` (`POCKETSKYNET_PATH` overrides) | the same directory — one shared database |
| Port | `PORT=9099` | `PS_PORT=9099`, falls back if taken |
| Client | any browser at the printed URL | the app's own window |

The desktop app is not a thin wrapper around a remote site: it runs the same
axum server inside the process and points its webview at it. That is why it
needs no configuration — and why other machines can connect to it too. The
window title shows the address to hand out.

`make package` bundles it as a native application.

## Layout

```
PocketSkynet/
├── Makefile              # make / build / start / gui / package / test
├── core/                 # pocketskynet-core — shared, compiles native AND wasm32
│   └── src/              #   wallet, EIP-191, E2EE crypto, wire types
├── server/               # pocketskynet-server — axum, SQLite, WebSocket, SSE
│   └── src/
├── gui/                  # pocketskynet-gui — Tauri desktop app, embeds the server
├── web/                  # pocketskynet-web — Yew + WebAssembly
│   ├── index.html
│   ├── pwa/              #   manifest + service worker, copied to the site root
│   └── static/           #   app.css, vendored Topcoat reset, generated imagery
├── docs/                 # API.md, CRYPTO.md, REALTIME.md, DESIGN.md
├── scripts/
└── tools/genart.py       # regenerates the imagery via Grok
```

Runtime data — the SQLite database, the JSONL event log, uploads and TLS
material — lives outside the repository, in `~/.pocketskynet` by default
(`POCKETSKYNET_PATH` or `--data-dir` relocates it). The startup banner prints
the resolved path.

`core/` is deliberately the only crate both sides depend on. The exact same
encryption code runs in the browser and in the test suite, so a vector that
passes natively cannot silently differ in WASM.

## Storage

Two layers, on purpose:

- **SQLite** (`~/.pocketskynet/pocketskynet.db`) — the queryable state: users, rooms,
  members, admins, wrapped room keys, messages. WAL mode.
- **JSONL** (`~/.pocketskynet/events/events-YYYY-MM-DD.jsonl`) — an append-only, ordered,
  grep-able log of every realtime event. It backs SSE resume and gives you a
  plain-text audit trail that survives a database rebuild.

Events are committed to SQLite first, then logged, then fanned out — so the log
is always a superset of what was delivered, never the reverse.

On the **client** side the rule is that nothing sensitive persists: the derived
E2EE key, the per-account salt and every unwrapped room key live in the tab's
memory and are gone on reload. The one exception is opt-in and on screen —
"Stay signed in on this device" keeps your username and recovery phrase in
`localStorage` so a reload signs you back in instead of asking again. It is a
real trade, the sign-in screen says what it costs, and Settings → *Recovery
phrase on this device* shows it, copies it and forgets it. Signing out clears it.

## Realtime

Three transports, same JSON event shapes:

| Transport | Endpoint | Use |
|---|---|---|
| WebSocket | `GET /ws` | Default. Bidirectional; carries typing indicators. |
| SSE | `GET /api/events` | One-way fallback; resumes via `Last-Event-ID`. |
| Polling | `GET /api/rooms/:id/sync?since=` | Always available; the floor. |

Realtime events are wake-up signals only — they carry no message content. The
client reacts by syncing the affected room, which keeps the fan-out path free of
anything that would need block-filtering or decryption.

See `docs/REALTIME.md`.

## Look

Dark by default — a cold blue-black command-center shell with a single optic-cyan
accent, an ambient circuit-grid backdrop, and a cinematic half-human/half-machine
guardian on the login screen (all imagery generated by `tools/genart.py` via
`GROK_API_KEY`). Motion rides two easing curves — exponential attack, cosine
settle — so panels power on rather than fade in. Light is a deliberate
alternative rather than the starting point. Identity is
a monogram tile coloured deterministically from the wallet address, so the same
account is recognisable everywhere without anyone having to read 42 characters of
hex. Motion is short and compositor-only; see `docs/DESIGN.md` §1 and
`docs/MOTION.md`.

Topcoat is vendored as a control reset, not as the look — most of its appearance
is overridden.

## Performance

First load is **~475 KB** over the wire, from a 1.2 MB WASM bundle:

| Asset | Built | On the wire (brotli) |
|---|---:|---:|
| `*_bg.wasm` | 1222 KB | **452 KB** |
| `*.js` | 54 KB | 9 KB |
| `app.css` | 57 KB | 13 KB |
| `index.html` | 1.2 KB | 1.1 KB |

Four things get it there, and each is load-bearing:

- **Brotli/gzip** on every response above 512 bytes — the bundle compresses about
  4.5:1, so this is most of the first-load time. Deliberately **off** for
  `text/event-stream`: a compressor buffers to fill a block, and a buffered
  stream is not a stream. Off for images too, which are already compressed.
- **`wasm-opt -Oz`** on release builds, worth roughly 26% of the bundle.
- **`Cache-Control: immutable`** for content-hashed bundles, `no-cache` for
  stable URLs like `index.html` and `app.css` — so a redeploy is picked up
  immediately while the bundle is never re-fetched.
- **A service worker** for every load after the first: the bundle is already on
  the device, so a home-screen launch paints without waiting for 452 KB — and
  paints at all with no server reachable. See *Installing it as an app*.
- **A `Link:` preload hint** on the HTML document, so the WASM fetch starts in
  parallel with the JS instead of waiting a round trip for it. The server emits
  this rather than the bundler, because the filename is content-hashed and only
  something reading the built directory knows it.

## Commands

| Command | Description |
|---|---|
| `make` | Show every target |
| `make build` | Release build: server + WASM client |
| `make start` | Run the server from the terminal |
| `make https` | The same, over TLS with a self-signed certificate |
| `make gui` | Run the desktop app |
| `make package` | Bundle the desktop app |
| `make test` | Unit, web, and integration tests |
| `make verify` | fmt, clippy, tests, and a wasm build check |
| `make dev` | Hot-reloading development mode |
| `make assets` | Regenerate imagery via Grok (needs `GROK_API_KEY`) |
| `make db-reset` | Delete the database and event log |

Override the defaults with variables: `make start PORT=8080 DATA_DIR=/var/ps`.
The data root defaults to `~/.pocketskynet`; exporting `POCKETSKYNET_PATH`
(in the shell or in `.env`) moves it, and `DATA_DIR=…` overrides even that.

## Deploying behind a proxy

Rate limiting keys on the socket's peer address. Behind a reverse proxy every
request arrives from the proxy, so the whole deployment would share one budget —
but trusting `X-Forwarded-For` by default would be worse, because it is
client-controlled and anyone could rotate it to become un-limitable.

So it is opt-in, and you state how many proxies you actually control:

```bash
pocketskynet --trust-proxy 1        # one proxy in front (nginx, Caddy, a CDN)
```

The value counts from the **right** of the header, because that is the end your
own proxies wrote; the leftmost entry is whatever the client claimed. If the
chain is shorter than you declared, the peer address is used instead.

`--no-rate-limit` exists for the test suite, which drives many wallets from one
address. The server **refuses to start** if it is combined with
`PS_ENV=production`.

Leave `--tls` off when the proxy terminates TLS: the certificate a client sees
is the proxy's, and a second TLS layer on the same hop buys nothing. `--tls` is
for the case with no proxy at all — a laptop other devices connect to directly.

## Requirements

- Rust 1.82+ with the `wasm32-unknown-unknown` target
- [`trunk`](https://trunkrs.dev) — `cargo install trunk`
- For `make gui` / `make package`: `cargo install tauri-cli --version '^2'`

`make` installs the wasm target itself if it is missing, and tells you what to
run if `trunk` is absent. SQLite is compiled from source (`rusqlite/bundled`),
so there is no system library to install.

## Packaging

```bash
make package
```

produces both, in `target/release/bundle/`:

```
    macos/PocketSkynet.app                    the application
    dmg/PocketSkynet_1.0.0_arm64.dmg          the installer
```

The `.app` carries the web bundle inside it as a resource, so it runs on a
machine with no Rust toolchain and no source tree.

The `.dmg` is built by `scripts/make-dmg.sh` rather than by Tauri. Tauri's own
DMG step drives Finder over AppleScript to position icons and set a background
picture, and Finder is a GUI process: over ssh or in CI it never answers and the
build dies with `AppleEvent timed out (-1712)` *after* the application has
already built successfully. The replacement uses `hdiutil` alone. What is lost
is cosmetic — a background image and hand-placed icon coordinates. What is kept
is everything that makes a disk image work: the app, a drag-to-Applications
alias, compression, and a signature.

## Credits

Imagery in `web/static/img/` is generated with Grok (xAI Imagine) from the
prompts in `tools/genart.py`. Topcoat is vendored under
`web/static/vendor/topcoat/` (Apache-2.0, see its `LICENSE`).
