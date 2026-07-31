# Parity audit — PocketSkynet (Rust) vs FruitNation (React + Express)

Scope: the **React client** at `server/client/src/` vs the **Rust/WASM client** at
`PocketSkynet/web/src/`, and the **Express server** at `server/server/` vs the
**Rust server** at `PocketSkynet/server/src/`.

Method: full reads of both clients' core modules, a route-by-route diff of the two
servers, and cross-checks against `docs/API.md` §14–15 and
`server/tests/FINDINGS.md`. Line references are to the state of the tree at the time
of writing. Anything I could not confirm is marked **unverified** rather than guessed.

---

## Verdict

**Full-stack Rust works — decisively on the server, conditionally on the client.**

The Rust server is not a partial port; it is a **complete and in several respects
better** replacement. All 52 Express HTTP endpoints exist at identical method and
path, plus two the Express server never had (`GET /api/events`, `POST
/api/events/ticket`). It fixes ~22 documented reference bugs, has 702 tests against
the Express server's far thinner suite, and its realtime hub is O(recipients) rather
than O(all sockets). The only capability Express has that it lacks is on-chain
`txHash` verification via `FN_RPC_URL`, deliberately omitted and documented. If the
question were only "can axum replace Express here", the answer is an unqualified yes.

The Rust/WASM client is where the honest answer gets complicated. As a **messenger**
it is at or above parity: rooms, membership, admins, invitations, messaging, editing,
deletion, reactions, E2EE with epoch rotation, blocking, hidden rooms, unread counts,
three realtime transports, and an optimistic offline send queue the React client does
not have. Its architecture is visibly better — a real router with shareable URLs, no
`window.location.reload()`, no key material in `localStorage`, 141 client-side unit
tests, and accessibility that is actually implemented rather than aspirational.

But it is **not a drop-in replacement for the React client**, because the React client
is not only a messenger. Four substantial product surfaces are simply absent: **all
internationalisation** (React ships 7 languages × 441 strings; the Rust client is
hard-coded English with a language *preference* that translates nothing), **the entire
on-chain publish flow** (React signs and broadcasts the anchoring transaction; the
Rust client has the API method but zero call sites and no transaction-signing code at
all), **all wallet operations** (balance, send, gas controls — absent, though
`DESIGN.md` §10 specifies a "Send funds" modal), and **all AI features**. MetaMask and
Privy sign-in are also gone, deliberately and with a documented reason.

So: **go for the server, unreservedly. For the client, go as a prototype — but budget
for i18n and the blockchain/wallet surface as real work, not polish.** The bones are
better than the React client's. The product surface is roughly 70% of it. The gap is
concentrated, not diffuse, which is the good kind of gap.

Two things that would be easy to overstate and should not be: the WASM bundle is
**6.7 MB uncompressed / 1.6 MB gzipped** (`web/dist/*_bg.wasm`), and there is **zero
browser-level test coverage** — `wasm-bindgen-test` is declared in
`web/Cargo.toml` but not used anywhere in `web/src/`. Every one of the 141 client
tests is a host-target unit test of pure logic. No rendering, no DOM, no integration.

---

## Feature matrix

| Feature | React | Rust | Notes |
|---|---|---|---|
| **Auth — mnemonic (BIP-39/BIP-44)** | ✅ | ✅ | Both derive at `m/44'/60'/0'/0/{index}` with a wallet-index stepper. Rust pins derivation against ethers.js vectors (`web/src/components/login.rs:832`). |
| **Auth — wallet generation** | ✅ | ✅ | Rust is stricter: submit is **blocked** until the phrase is copied or downloaded (`login.rs:397`). React's "Fastest Login" auto-downloads and proceeds. |
| **Auth — private key import** | ❌ | ✅ | **Rust-only.** `login.rs:98-122`, with shape-specific error messages and out-of-range scalar rejection. React has no raw-key path. |
| **Auth — MetaMask / injected** | ✅ | ❌ | React: `LoginForm.tsx:94-164` + chain-switch. Rust: deliberately absent, reason documented at `login.rs:5-8` (needs a JS provider bridge the bundle doesn't ship). |
| **Auth — Privy (email/embedded)** | ⚠️ | ❌ | React has it behind `VITE_PRIVY_APPID` (`App.tsx:61-85`); off when unset. Rust: absent, same documented reason. |
| **Auth — wallet-backup file load** | ✅ | ⚠️ | React can *load* a backup JSON to sign in (`LoginForm.tsx:342-415`). Rust writes the backup (`login.rs:269-293`) but has no file-load path — you paste the phrase. |
| **Locked/unlock state** | ❌ | ✅ | **Rust-only and a genuine security improvement.** Keys are never persisted, so a reload lands in `Auth::Locked` — readable, but encrypted content stays sealed until you re-enter the credential (`web/src/session.rs:13-30, 167-230`). React persists the E2EE private key in `localStorage` (`auth.ts:396`). |
| **Room create / rename / delete** | ✅ | ✅ | Parity. |
| **Room types (public/private/DM)** | ❌ | ❌ | Neither. The schema has no room type; every room is invite-only. Not a gap — a shared limitation. |
| **Room description** | ⚠️ | ⚠️ | Both capture it at creation; React never displays it. Rust display **unverified**. |
| **Membership — leave / kick / list** | ✅ | ✅ | Parity. Rust replaces `window.confirm` + full page reload (`ChatWindow.tsx:210-246`) with a real dialog and a router navigation (`app.rs:606-614`). |
| **Presence ("online" dot)** | ❌ | ❌ | React renders a **hard-coded green dot for everyone** (`MembersList.tsx:412`) — there is no presence system. Rust correctly omits it. |
| **Admins — add / remove / max 9** | ✅ | ✅ | Parity. |
| **Invitations — create/accept/decline/list** | ✅ | ✅ | Parity. Both pre-wrap the room key to the invitee (React `ChatWindow.tsx:291-297`; Rust `actions.rs:369-408`). Rust treats a 409 as success, which is correct. |
| **Invite links / codes** | ❌ | ❌ | Neither. Address-only invitations. |
| **Messaging — send / list / paginate** | ✅ | ✅ | Both page backward on timestamp at 50/page. Rust paginates on emptiness rather than row count (`actions.rs:100-114`), which is correct given the server filters reaction rows after `LIMIT`. |
| **Message edit** | ✅ | ✅ | Both re-encrypt under the current epoch. |
| **Message delete / delete-all** | ✅ | ✅ | Both expose delete on every message (server enforces); both have delete-all. |
| **Replies / threads** | ❌ | ❌ | Neither. Not in the protocol. |
| **File attachments** | ❌ | ❌ | Neither. |
| **Optimistic send + offline queue** | ❌ | ✅ | **Rust-only.** Pending bubbles, per-message failure state, retry and discard (`state.rs:92, 289-293, 442-471`), flushed oldest-first. React awaits the server with no pending state and no outbox. |
| **Emoticon reactions** | ✅ | ✅ | Same event-sourced model (`emoticon_add`/`emoticon_remove` as message rows). Rust additionally **block-filters reactors** (`store.rs:186-196`). |
| **Emoji picker breadth** | ✅ | ⚠️ | React: 12 categories, ~48 emoji each. Rust: **3 categories, 48 total** (`composer.rs:154-176`), deliberately curated to keep the bundle down. Functional, visibly narrower. |
| **E2EE — AES-256-CBC + ECDH + HMAC** | ✅ | ✅ | Same v2 primitives, same labels, same MAC inputs, both validated against `server/test/vectors/crypto-v2.json`. |
| **E2EE — key binding / anti-MITM** | ✅ | ✅ | Both verify `publicKeySig` before wrapping and refuse rather than proceed. |
| **E2EE — epochs & rotation** | ✅ | ✅ | Both hold every epoch, decrypt with the message's epoch, retry once on `STALE_KEY_VERSION`. Rust fails **closed** on any unverified recipient (`actions.rs:305-315`). |
| **E2EE — legacy (unsalted) heal path** | ✅ | ✅ | Both re-wrap a legacy-only epoch to the current key. |
| **Silent plaintext downgrade** | ⚠️ | ✅ | React sends `isEncrypted: false` with **no warning** when the room key is missing (`MessageInput.tsx:48-67`), and creates unencrypted rooms on key failure with only a toast. Rust locks the composer, states the reason, surfaces *why* a room was created plaintext (`dialogs/create_room.rs:149`), and badges plaintext messages inside encrypted rooms (`message.rs:127-133`). |
| **Create-room encryption toggle** | ❌ | ✅ | React's `UI.md` claims one; the code has none. Rust has a real toggle defaulting on (`create_room.rs:203-224`). |
| **Blocking (bidirectional filtering)** | ✅ | ✅ | Both fetch `blocked` and `blocked-by` and filter both directions. Both offer manual block-by-address with the same regex. |
| **Hidden rooms** | ✅ | ✅ | Parity. |
| **Unread counts** | ✅ | ✅ | Both server-computed with a monotonic read pointer, `99+` cap. Rust adds a local fallback count (`room_list.rs:288-292`). |
| **Search — users** | ✅ | ✅ | Both debounced against `/api/users/search`. |
| **Search — rooms** | ✅ | ✅ | Both client-side over the loaded list. Rust adds ⌘K focus (`room_list.rs:40-53`). |
| **Search — messages** | ❌ | ❌ | Neither. No endpoint exists. |
| **Realtime — WebSocket** | ✅ | ✅ | Both authenticate via the `fnauth` subprotocol. Rust reconnects with jittered backoff **indefinitely**; React caps at 20 attempts then goes permanently silent (`websocket.ts:28-30`). |
| **Realtime — SSE** | ❌ | ✅ | **Rust-only**, with ticket auth and `Last-Event-ID` resume. |
| **Realtime — polling** | ✅ | ✅ | Both 10 s. Rust also runs a 60 s safety-net sync in *every* mode (`realtime.rs:146`) — React has no backstop. |
| **Automatic transport degradation** | ❌ | ⚠️ | Neither actually degrades. React's mode is a manual toggle with no fallback. Rust *implements and unit-tests* a WS→SSE→polling ladder (`realtime.rs:123-139`) but the **only call site passes a hard-coded `0` failure count** (`app.rs:171`), so it never engages. Dead-but-tested code. |
| **Offline — connectivity awareness** | ❌ | ✅ | **Rust-only.** `navigator.onLine` + online/offline listeners, offline banners, disabled-with-reason controls (`app.rs:87-115`). React has no `navigator.onLine` handling anywhere. |
| **Offline — persisted message cache** | ✅ | ✅ | Both, differently — and Rust's is the better design. React caches full rows *including decrypted plaintext* in IndexedDB via Dexie (`lib/database.ts:28`). Rust caches rows **in wire form** (ciphertext, IVs, HMACs) plus the wrapped epoch keys (`web/src/cache.rs`), so a reopened room paints in one frame with zero requests — only the `/sync` delta touches the network — without plaintext ever resting on disk. Write-through happens in the reducer's fold, so realtime events keep the cache current for rooms not on screen. The Settings "Removes cached messages" claim is now true. |
| **i18n** | ✅ | ❌ | **The single largest client gap.** React: i18next, 7 locales (`en, ko, ja, es, zh, yue, de`), 441 leaf strings each, at full parity. Rust: every string is a hard-coded English literal; `store.language` is stored and written to `<html lang>` (`session.rs:321-328`) but **translates nothing**, and there is no language switcher in Settings. |
| **On-chain publish (anchoring)** | ✅ | ❌ | **React-only.** Full flow: `BlockchainService.publishHashToBlockchain` (`services/blockchain.ts:86-185`) with chain verification, balance check, hash-as-calldata, fixed gas limit, error mapping, plus `PublishHashDialog` with a ≥10× confirmation and a Privy raw-provider branch. Rust has `Client::publish_message` (`api/messages.rs:122`) with **no call site anywhere**, and contains **no transaction-signing code at all** — grep for `eth_sendTransaction`/`getBalance`/`Transaction` in `web/src/` returns nothing. |
| **On-chain — display of anchors** | ✅ | ✅ | Both render a verified badge and link to the explorer. Rust's "ledger gutter" hash slug (`message.rs:217-260`) is the nicer treatment. |
| **Wallet — balance** | ✅ | ❌ | React: header dialog + Blockchain tab. Rust: absent. |
| **Wallet — send native / gas controls / calldata** | ✅ | ❌ | React: full send flow with editable gwei + gas limit, estimation, arbitrary calldata, and tiered confirmations (1/2/3 steps by amount) in both `BlockchainTab.tsx` and `MembersList.tsx`. Rust: absent, though `DESIGN.md` §10 specifies a three-step "Send funds" modal. |
| **Wallet — swap / DEX / ERC-20** | ❌ | ❌ | Neither. (That lives in the CLIs.) |
| **Media — YouTube embeds** | ✅ | ❌ | React: `youtube-nocookie.com` iframes, 4 URL forms (`utils/mediaDetection.ts:50-100`). |
| **Media — inline images / GIFs** | ✅ | ❌ | React: extension list + host allowlist, lazy loading, failure fallback. Rust: **autolink only** (`message.rs:195-215`), with an explicit rationale — no markdown, no HTML, no sanitiser to get wrong. Defensible, but it is a visible feature loss. |
| **Link previews (OpenGraph cards)** | ❌ | ❌ | Neither. |
| **AI features** | ✅ | ❌ | **React-only.** `services/ai.ts` + `AIAssistant.tsx`: 4 providers (Grok/OpenAI/Anthropic/Gemini), BYO-key in `localStorage`, 5 tabs, persona-driven auto-post/auto-meme/auto-react, key-test button. Worth noting for a security review: it **decrypts room history client-side and sends it to a third-party LLM** (`AIAssistant.tsx:131-158`). Rust: nothing. |
| **Theming (light/dark)** | ✅ | ✅ | React: 2 options, defaults light, no `prefers-color-scheme`. Rust: **3 options including System** (`settings.rs`), `data-theme` on `<html>`. Rust is better. |
| **Mobile responsiveness** | ✅ | ✅ | Both two-pane desktop / single-column mobile with a bottom nav. React adds safe-area insets and `ResizeObserver`-driven overlay measurement; Rust's handling is **unverified** beyond `DESIGN.md` §16. |
| **Animation / particle effects** | ✅ | ❌ | React: framer-motion springs, fruit-burst particles, WAAPI disintegration on delete, a lazy-loaded three.js backdrop. Rust: none. Cosmetic, but it is most of the product's personality. |
| **Fruit avatars (deterministic)** | ✅ | ✅ | Byte-identical 40-emoji table and djb2 index (`web/src/fruit.rs:15-21`). Rust adds a decorrelated hue from the high hash bits. |
| **Deterministic username generation** | ✅ | ✅ | Byte-identical: the same 152 adjectives × 156 nouns in the same order, the same `keccak256(lowercase(address))` slicing (`core/src/username.rs`), pinned against PROTOCOL.md §10's two vectors. The login screen shows the derived name as the username field's placeholder and uses it when the field is left blank. |
| **Accessibility** | ⚠️ | ✅ | React: reduced-motion respected and Radix primitives help, but **zero `aria-label`s in app code** — icon-only buttons rely on `title` only, and there is no focus-visible styling or skip link. Rust: `aria-label` on essentially every control, `role="radiogroup"`/`role="tab"`/`role="menu"`, `aria-pressed`, a modal focus trap, `aria-live` typing region, and unread counts announced as sentences (`format.rs:188-193`). **Rust is clearly ahead.** |
| **Destructive-action confirmations** | ⚠️ | ✅ | React mixes native `window.confirm` (leave/delete/hide/remove-admin) with Radix dialogs. Rust: one modal system, every confirmation names the object and states the consequence (`state.rs:116-124`); `window.confirm` appears nowhere. |
| **Routing / deep links** | ⚠️ | ✅ | React has **one real route** (`/messenger`); views are internal state, so no room is linkable and back/forward do nothing. Rust has real paths (`/rooms/:id`, `/members/:id`, …), `popstate` handling, and a 404 screen. |
| **Client test coverage** | ⚠️ | ⚠️ | React: 14 Vitest files (~3.6k lines), crypto-heavy, **no component/UI tests**. Rust: 141 `#[test]`s across 13 modules, all pure host-target logic, **no DOM/browser tests**. Different shapes, same hole. |

Dead or vestigial code exists on both sides. React: `GlobalSearchBox.tsx` and
`UserSearch.tsx` are never imported; `getRoomAdmins`, `isUserBlocked`, `getUser`,
`updateProfile` are never called; `MainLayout`'s testnet badge is unreachable on the
only route. Rust: `publish_message` has no call site, and `select_transport`'s
degradation ladder is never fed a real failure count.

---

## Server matrix

| Group | Express | Rust | Notes |
|---|---|---|---|
| misc (`/health`, `/blockchain/info`) | 2 | 2 | Identical shapes; both exempt `/health` from rate limiting. |
| auth | 7 | 7 | Rust re-parses the wallet claim through `WalletAddress` so a mixed-case claim can't become a shadow identity (`server/src/auth.rs:82-92`). |
| users / blocking | 8 | 8 | Rust caps `/users/search` at 50 and orders by username; returns 400 (not Express's 500) on validation failure. |
| rooms / membership / admins | 14 | 14 | Rust adds a **membership check on `/leave`** (`routes/rooms.rs:265-270`) — Express had none, so any authenticated user could set `keyRotationPending` on any room. That is a fixed DoS. |
| invitations | 4 | 4 | Rust filters on room-missing rather than the literal string `"(deleted room)"`. |
| keys (E2EE) | 4 | 4 | Rotation coverage failures return a structured `missing[]` array instead of prose inside `errors[]`. |
| messages | 6 | 6 | Rust applies the same epoch gates to `PATCH` as to `POST`, refusing the silent encrypted→plaintext downgrade Express allowed. |
| emoticons | 3 | 3 | Rust percent-decodes once, not twice; `GET` is block-filtered. |
| sync / read state | 4 | 4 | Identical, incl. the `X-Has-More` header (CORS-exposed in Rust). |
| realtime | `/ws` | `/ws` + `/api/events` + `/api/events/ticket` | **Rust superset.** SSE with 30-min lifetime, 15 s heartbeat, `Last-Event-ID` resume from the JSONL log (≤1000 events) then `resync_required`. |
| **Totals** | **52 + 1 WS** | **54 + 1 WS** | |

**Express has that Rust does not:** on-chain `txHash` verification inside `POST
/api/messages/:id/publish` via `FN_RPC_URL` — deliberately omitted and documented
(`server/tests/FINDINGS.md:85-87`); `express.urlencoded` body parsing, which no
endpoint depends on; and `X-Forwarded-For` / `trust proxy` support.

**Rust has that Express does not:** the two SSE endpoints; a `room_serials` table that
moves serial allocation into the writing transaction (Express's in-process `lastGen`
map silently dropped messages across replicas); `origin`-tagged realtime envelopes so
a blocked user produces **no wake-up at all** rather than a wake-up followed by an
empty sync (Express leaked activity through timing); a `by_room` index making fan-out
O(recipients) instead of O(all sockets); a JSON `{message}` envelope on *every*
non-2xx including axum's own 405/415/413; and a `new_message` emission on `/publish`
that Express omitted.

**Persistence.** Express: PostgreSQL + Drizzle + drizzle-kit migrations. Rust: SQLite
in WAL mode with a hand-rolled 4-connection pool and an idempotent replayed schema —
**plus an append-only JSONL event log** that is the ordering authority for SSE resume.
Tables map one-for-one plus `room_serials`. Ordering is commit → log → fan-out, so a
crash in the gap loses a wake-up, never a message.

**Testing.** 702 `#[test]`/`#[tokio::test]` in `server/` and `core/`, across nine
integration suites. `FINDINGS.md` reports **no open findings**; its two recorded
findings were both resolved (one was against a stale build; one was a docs bug where
the code was right and the error message was unhelpful). Its one live suggestion is a
`, msg_serial ASC` tiebreak on `GET /messages` for deterministic same-millisecond
ordering.

---

## What the Rust stack does better

Specific and evidence-based, not a courtesy list.

1. **Key material never touches disk.** `session.rs:13-30` — the mnemonic, wallet key,
   derived E2EE key, salt and every unwrapped room key live in memory only. A test
   *enforces* the persisted shape (`session.rs:362-384`). The React client keeps the
   E2EE private key and decrypted room keys in `localStorage`, where one XSS
   exfiltrates the key that decrypts every epoch of every room forever — because it
   is deterministically derived and never rotates. This is the audit's most
   consequential difference.
2. **No silent plaintext downgrade.** React sends unencrypted with no warning when the
   key is missing. Rust locks the composer with a stated reason, explains why a room
   was created plaintext, and badges plaintext messages inside encrypted rooms.
3. **Optimistic sends with a real outbox.** Pending bubbles, per-message failure text,
   retry/discard, flush on reconnect, and invalidation on purge. React has none of it.
4. **Genuine offline awareness.** `navigator.onLine` listeners, banners, controls
   disabled with a reason, auto-refresh on reconnect. React has zero.
5. **Realtime hub scales.** O(recipients) fan-out, block-aware envelopes that suppress
   the wake-up entirely, SSE resume from a durable log, and indefinite jittered
   reconnect. React gives up permanently after 20 attempts with no polling fallback.
6. **The server fixes ~22 real bugs**, several security-relevant: the unauthenticated
   `/leave` DoS, the `PATCH` encryption downgrade, 500s that should have been 400s,
   duplicate rows in four list endpoints, and cross-replica serial collisions.
7. **Accessibility is implemented, not aspirational** — see the matrix row.
8. **Routing is real.** Shareable `/rooms/:id` URLs, working back/forward, a 404
   screen, and no `window.location.reload()` anywhere.
9. **One shared crypto crate.** `core/` compiles to both native and wasm32, so the
   exact bytes that run in the browser are what the vector tests exercise. React's
   browser crypto and its Node test path are separate code.
10. **Test volume.** 702 server/core tests plus 141 client-logic tests, with tests
    written as executable specifications of the reasoning.

---

## What is genuinely missing

Ordered by size of the hole. Effort estimates assume a developer already fluent in the
codebase.

| # | Gap | Effort | Detail |
|---|---|---|---|
| 1 | **Internationalisation — all of it** | **Large** | 441 strings × 7 locales, currently hard-coded English literals across ~20 components. Needs a lookup layer, extraction of every literal, a Settings switcher, and plural/interpolation handling. There is no partial version of this: today it is 0%. |
| 2 | **On-chain publish flow** | **Medium** | ~~No transaction-signing code exists~~ **Mostly closed:** `core/src/chain.rs` now has EIP-155 legacy signing, RLP, and the Cronos intrinsic-gas rule (validated against the EIP-155 spec vector and all seven PROTOCOL.md gas vectors), and `web/src/rpc.rs` is the eth-JSON-RPC client with chain-ID verification. What remains is only the publish *dialog* wiring `Client::publish_message` to a real send — the machinery below it all exists. |
| 3 | **Wallet operations** | ~~Large~~ **Done** | The top-bar wallet modal (`web/src/components/dialogs/wallet.rs`): balances (native + ERC-20), send with editable gas price/limit and calldata, tiered confirmations (>1 warn, >10 retype), receipt with explorer link and before/after balances, and an **active-network switcher** over `GET /api/networks` — Cronos testnet 338 (default) / mainnet 25 with USDC, plus Solana/Cardano registry entries that switch but do not yet sign (`ChainKind` gates send support). |
| 4 | **AI assistant** | ~~Medium~~ **Done** | `web/src/ai.rs` + the composer's assistant dialog: 4 providers (Grok/OpenAI/Anthropic/Gemini), Write/Reply/Image/Keys tabs, BYO keys in localStorage with a live Test button, and image hosting via the now-implemented `POST /api/images` (content-addressed, fixing the reference's missing endpoint). Reply-context decryption still ships room plaintext to the chosen provider — kept, but it is opt-in per press and the dialog says so. |
| 5 | **Persisted message cache / offline read** | ~~Medium~~ **Done** | `web/src/cache.rs` + the reducer's write-through: rows persist in wire form (ciphertext) with the wrapped keys beside them, so the "nothing sensitive persists" stance holds and reopening a room costs zero requests. `localStorage` rather than IndexedDB, deliberately: synchronous reads are what let a cached room paint in the same frame as the click, and the 200-row/room cap keeps quota far away. The Sync button is the explicit full-refetch path. |
| 6 | **Media rendering** | **Small–Medium** | YouTube embeds and inline images with a host allowlist. The Rust client's refusal to render HTML is correct; this can be done with typed URL parsing and no sanitiser. |
| 7 | **MetaMask / Privy sign-in** | **Medium** | Needs a JS interop bridge for `window.ethereum` and the Privy SDK. The reason for omission is sound, but it does exclude every user whose keys live in a browser extension — which for a wallet-auth product is not a small population. |
| 8 | **Transport degradation is unwired** | **Small** | `select_transport` is written and tested; `app.rs:171` passes a hard-coded `0`. Thread a failure counter through and the ladder works. Cheapest high-value fix on this list. |
| 9 | **Emoji picker breadth** | **Small** | 48 glyphs vs ~576. A deliberate bundle-size trade; revisit only if users complain. |
| 10 | **Wallet-backup file load** | **Small** | Rust writes the backup JSON but cannot read it back. |
| 11 | **Deterministic username generation** | ~~Small~~ **Done** | `core/src/username.rs`: the reference word lists transcribed in order, the PROTOCOL.md §10 algorithm, and its two published vectors as tests. Word-list *order* is protocol — inserting a word anywhere but the end renames every account past it. |
| 12 | **Server: on-chain anchor verification** | **Small** | The `FN_RPC_URL` branch of `POST /messages/:id/publish`. Now worth doing: the client-side signing machinery (#2) exists. |

---

## Risks and unknowns for productionising

**WASM bundle size — the top operational risk.** 6.7 MB uncompressed, **1.6 MB
gzipped** (`web/dist/pocketskynet-web-*_bg.wasm`), already built with `opt-level="s"`,
`lto = true`, `codegen-units = 1`, `panic = "abort"`. That is a hard floor before
`wasm-opt`, Brotli, or streaming instantiation. On a slow mobile connection this is a
multi-second blank screen. Every item on the "missing" list makes it worse, and it is
the structural reason the emoji picker was cut. **Measure `wasm-opt -Oz` and Brotli
before committing to this stack for a consumer-facing deploy.**

**No browser-level test coverage at all.** `wasm-bindgen-test` is declared in
`web/Cargo.toml` but appears nowhere in `web/src/`. All 141 client tests are host-target
unit tests of pure functions. Nothing exercises rendering, the focus trap, the modal
layer, `EventSource` handling, `localStorage` interaction, or any user flow end to end.
The `select_transport` bug is exactly the class of defect this misses: perfectly
unit-tested logic wired to a constant. This is the highest-value gap to close, and it
is cheap relative to the features above.

**Single-node only.** SQLite, an in-process `Hub`, an in-process `TicketStore` and a
`DashMap` rate limiter are all process-local. Horizontal scaling needs a shared bus.
Express is *also* effectively single-node, but its Postgres backend at least let
multiple app replicas share state — which is precisely what was broken (duplicate
serials silently dropping messages). Rust's design makes the boundary explicit and the
JSONL log durable, so a future bus replaces the broadcast channel and little else. But
today: **one process, one disk.**

**Rate limiting behind a proxy.** The Rust limiter keys on the peer socket address and
never consults `X-Forwarded-For` (`server/src/ratelimit.rs:15-17`). Behind any reverse
proxy the entire deployment shares one bucket — 100 requests/minute total. This will
take the service down on first contact with a load balancer.

**`--no-rate-limit` has no production guard.** Express hard-refuses the equivalent flag
under `NODE_ENV=production` (`routes.ts:29-37`). The Rust flag
(`config.rs:48-49`) does not. One stray environment variable disables all abuse
protection silently.

**Crypto hardening gaps carried over from the reference.** Both clients derive the E2EE
keypair as `keccak256(walletSignature)` — a deterministic key that **never rotates**,
so compromising it once compromises all history forever. Both use AES-256-CBC rather
than an AEAD; the encrypt-then-MAC construction with per-label subkeys is done
correctly, but CBC+HMAC is a construction you have to get exactly right rather than one
that is hard to get wrong. Neither client offers key rotation for the *identity* key.
Rust's non-persistence materially limits the blast radius; it does not change the
underlying design.

**The `msg_serial` tiebreak.** `FINDINGS.md` notes `GET /messages` has no tiebreak on
`messageTimestamp`, so same-millisecond ordering is non-deterministic and pushed to the
client. Within spec, and a one-line fix.

**Unverified areas.** I did not run either client, so I cannot speak to actual
responsive behaviour at real breakpoints, render performance with large rooms, the
focus trap's correctness in practice, or whether the Rust client's room-description
display exists. Bundle-size figures are from the committed `dist/`, which I confirmed
is a release build but did not rebuild myself.

**~~One documentation inaccuracy worth fixing.~~ Resolved.** Settings' claim that
"Erase local data" removes "cached messages" was written before any cache existed;
the persisted room cache (`web/src/cache.rs`) has since made it true.
